// crates/velocut-ui/src/helpers/clip_query.rs
//
// Clip-lookup helpers that replace the repeated
//
//   state.timeline.iter().find(|c| c.id == id)
//   state.library.iter().find(|l| l.id == tc.media_id)
//
// chains that appear across app.rs, timeline.rs, and library.rs.
//
// All functions borrow ProjectState immutably and return an Option reference
// with a lifetime tied to the state, so callers can continue to read other
// fields on state in the same expression.

use velocut_core::state::{LibraryClip, ProjectState, TimelineClip};
use uuid::Uuid;

// ── Timeline lookups ──────────────────────────────────────────────────────────

/// Return the timeline clip whose `id` field matches `id`, or `None`.
///
/// Replaces the `state.timeline.iter().find(|c| c.id == id)` pattern.
#[inline]
pub fn timeline_clip(state: &ProjectState, id: Uuid) -> Option<&TimelineClip> {
    state.timeline.iter().find(|c| c.id == id)
}

/// Return the currently selected timeline clip, or `None` when nothing is
/// selected.
///
/// Replaces the `state.selected_timeline_clip.and_then(|id| state.timeline
/// .iter().find(|c| c.id == id))` chain that recurs in the timeline toolbar.
#[inline]
pub fn selected_timeline_clip(state: &ProjectState) -> Option<&TimelineClip> {
    state.selected_timeline_clip.and_then(|id| timeline_clip(state, id))
}

/// Return the timeline clip that contains `time` (i.e. the clip currently
/// under the playhead), or `None` when the playhead is in a gap.
///
/// Replaces the inline predicate used in video_module.rs, audio_module.rs,
/// and app.rs for "what is playing right now" queries.
#[inline]
pub fn clip_at_time(state: &ProjectState, time: f64) -> Option<&TimelineClip> {
    state.timeline.iter().find(|c| {
        time >= c.start_time && time < c.start_time + c.duration
    })
}

// ── Library lookups ───────────────────────────────────────────────────────────

/// Return the library entry whose `id` matches `id`, or `None`.
///
/// Replaces `state.library.iter().find(|l| l.id == id)`.
#[inline]
pub fn library_clip(state: &ProjectState, id: Uuid) -> Option<&LibraryClip> {
    state.library.iter().find(|l| l.id == id)
}

/// Return the library entry whose `id` matches `media_id` on `clip`.
///
/// The most common two-step pattern — look up a timeline clip then its
/// library entry — collapsed into one call.
///
/// ```ignore
/// // Before
/// if let Some(lib) = state.library.iter().find(|l| l.id == clip.media_id) { … }
///
/// // After
/// if let Some(lib) = library_entry_for(state, clip) { … }
/// ```
#[inline]
pub fn library_entry_for<'s>(
    state: &'s ProjectState,
    clip:  &TimelineClip,
) -> Option<&'s LibraryClip> {
    library_clip(state, clip.media_id)
}

/// Return the library entry for the currently-selected timeline clip.
///
/// Combines `selected_timeline_clip` + `library_entry_for` for the pattern
/// that appears in the timeline toolbar's "Extract frame" enabled-check.
///
/// Returns `None` if there is no selected clip or its media_id has no library
/// entry (e.g. the source was deleted from the library while still on the
/// timeline).
#[inline]
pub fn selected_clip_library_entry(state: &ProjectState) -> Option<&LibraryClip> {
    selected_timeline_clip(state).and_then(|tc| library_entry_for(state, tc))
}

// ── Extracted audio helpers ───────────────────────────────────────────────────

/// Returns `true` when `clip` is an extracted-audio clip — a video-source clip
/// that has been placed on an audio track row via `ExtractAudioTrack`.
///
/// Canonical definition: odd track row AND has a linked partner clip.
/// Use this everywhere instead of inlining the predicate — both
/// `timeline.rs` and `begin_render()` in `app.rs` previously held their own
/// copies, which risks silent drift if one is updated and the other is not.
#[inline]
pub fn is_extracted_audio_clip(clip: &TimelineClip) -> bool {
    clip.track_row % 2 == 1 && clip.linked_clip_id.is_some()
}

/// Return the A-row extracted-audio clip linked to `video_clip`, if any.
///
/// The V↔A link is stored as `video_clip.linked_clip_id → audio_clip.id`.
/// Returns `None` if the video clip has no linked audio partner or if the
/// linked id is not found in `state.timeline` (e.g. removed by undo).
///
/// Used in `begin_render()` to look up the A-row volume when the V-row clip
/// has `audio_muted = true`, and anywhere else the V↔A relationship is traversed.
#[inline]
pub fn linked_audio_clip<'s>(
    state:      &'s ProjectState,
    video_clip: &TimelineClip,
) -> Option<&'s TimelineClip> {
    video_clip.linked_clip_id.and_then(|aid| timeline_clip(state, aid))
}

// ── Playhead helpers ──────────────────────────────────────────────────────────

/// Resolve the source-file timestamp that corresponds to `state.current_time`
/// within the selected timeline clip, together with that clip's library entry.
///
/// Returns `None` when:
/// - no timeline clip is selected, or
/// - the selected clip's library entry is missing (source deleted), or
/// - the playhead is outside the clip's timeline range (gap).
///
/// The timestamp is clamped so it always lands on a valid frame within the
/// clip's trimmed range:
///
/// ```text
/// ts = tc.source_offset
///    + clamp(state.current_time − tc.start_time,  0.0,  tc.duration − one_frame)
/// ```
///
/// This is the source-truth for "Export this frame" and anything else that
/// needs to know which frame of the source file is currently visible.
///
/// # Example
/// ```ignore
/// if let Some((ts, lib)) = clip_query::playhead_source_timestamp(state) {
///     cmd.push(EditorCommand::RequestSaveFramePicker {
///         path: lib.path.clone(),
///         timestamp: ts,
///     });
/// }
/// ```
pub fn playhead_source_timestamp(state: &ProjectState) -> Option<(f64, &LibraryClip)> {
    const ONE_FRAME: f64 = 1.0 / 30.0;

    let tc  = selected_timeline_clip(state)?;
    let lib = library_entry_for(state, tc)?;

    let offset = (state.current_time - tc.start_time)
        .clamp(0.0, (tc.duration - ONE_FRAME).max(0.0));

    Some((tc.source_offset + offset, lib))
}

// ── Audio playback helper ─────────────────────────────────────────────────────

/// Return the clip that should be providing audio at `time`.
///
/// Priority: A-row clips (extracted audio, rows 1/3) over V-row clips (rows 0/2).
/// This ensures that after `ExtractAudioTrack`, the A-row WAV plays instead of
/// the muted V-row source. V-row clips with `audio_muted = true` are skipped.
///
/// Centralised here so `audio_module.rs` and any future consumer share a single
/// definition of "what clip plays audio at time t" — no silent drift between callsites.
#[inline]
pub fn active_audio_clip(state: &ProjectState, time: f64) -> Option<&TimelineClip> {
    // A-row first (extracted audio takes priority)
    state.timeline.iter()
        .find(|c| {
            matches!(c.track_row, 1 | 3)
                && c.start_time <= time
                && time < c.start_time + c.duration
        })
        .or_else(|| {
            // V-row fallback — skip muted clips
            state.timeline.iter().find(|c| {
                matches!(c.track_row, 0 | 2)
                    && !c.audio_muted
                    && c.start_time <= time
                    && time < c.start_time + c.duration
            })
        })
}