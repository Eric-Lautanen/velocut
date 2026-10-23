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

use uuid::Uuid;
use velocut_core::state::{LibraryClip, ProjectState, TimelineClip};

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
    state
        .selected_timeline_clip
        .and_then(|id| timeline_clip(state, id))
}

/// Return the VIDEO clip (even track row, not an extracted audio clip) that
/// contains `time`, or `None` when no video clip covers the playhead.
///
/// A row-agnostic "clip at time" lookup is an audio-preview bug waiting to
/// happen: an A-row extracted-audio clip whose span starts before or outlives
/// its linked V-row partner would be matched first, making the preview decode
/// an audio source as video and compute a wrong `local_t`. Every consumer
/// that drives what the preview panel decodes or displays must use this
/// video-row lookup; audio/code paths need an A-row equivalent should add a
/// sibling helper here rather than reverting to matching any row.
#[inline]
pub fn video_clip_at_time(state: &ProjectState, time: f64) -> Option<&TimelineClip> {
    // Deterministic pick when two covers overlap (e.g. an overlap left by an
    // undo/redo or a project made before the ripple policy): the LATER-STARTING
    // clip wins, i.e. the one the user placed "on top". Before this the
    // winner depended on document order — which clip was dragged onto the
    // timeline first.
    state
        .timeline
        .iter()
        .filter(|c| {
            c.track_row % 2 == 0
                && !is_extracted_audio_clip(c)
                && time >= c.start_time
                && time < c.start_time + c.duration
        })
        .max_by(|a, b| a.start_time.total_cmp(&b.start_time))
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
    clip: &TimelineClip,
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
    state: &'s ProjectState,
    video_clip: &TimelineClip,
) -> Option<&'s TimelineClip> {
    video_clip
        .linked_clip_id
        .and_then(|aid| timeline_clip(state, aid))
}

/// Timeline time → seek position (seconds) inside a clip's extracted WAV.
///
/// A WAV extracted from a trimmed range starts at the clip's `source_offset`,
/// which `LibraryClip::audio_trimmed_offset` records. Subtracting it here keeps
/// both extraction paths (full file, trimmed range) on one formula and stops
/// `source_offset` from being counted twice.
#[inline]
pub fn wav_seek_t(clip: &TimelineClip, lib: &LibraryClip, timeline_t: f64) -> f64 {
    (timeline_t - clip.start_time + clip.source_offset - lib.audio_trimmed_offset).max(0.0)
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

    let tc = selected_timeline_clip(state)?;
    let lib = library_entry_for(state, tc)?;

    let offset =
        (state.current_time - tc.start_time).clamp(0.0, (tc.duration - ONE_FRAME).max(0.0));

    Some((tc.source_offset + offset, lib))
}

// ── Transition zone detection ─────────────────────────────────────────────────

/// Returned when `state.current_time` is inside a transition blend zone.
/// Callers use this to request a blended preview frame instead of a single-clip frame.
pub struct TransitionZone<'a> {
    /// The outgoing clip (frame_a in the transition — the one ending).
    pub clip_a: &'a TimelineClip,
    /// The incoming clip (frame_b — the one starting).
    pub clip_b: &'a TimelineClip,
    /// The transition parameters (kind + duration_secs).
    pub transition: velocut_core::transitions::TransitionType,
    /// Blend factor: 0.0 = fully clip_a, 1.0 = fully clip_b.
    pub alpha: f32,
    /// Source-file timestamp to decode from clip_a at this alpha.
    pub clip_a_source_ts: f64,
    /// Source-file timestamp to decode from clip_b at this alpha.
    pub clip_b_source_ts: f64,
}

/// Tolerance, in seconds, for treating two clips as sharing a cut. Covers
/// floating-point drift in `start_time + duration` chains; anything larger is a
/// real gap or overlap in the timeline.
///
/// Public because the playback blend builders (`video_module.rs`) must agree
/// with the zone math on where a transition exists — before this, playback
/// blended across a 0.05s gap that the preview rendered as a hard cut.
pub const ADJACENCY_EPS: f64 = 1.0e-6;

/// Returns blend info if `state.current_time` is inside a transition zone, else `None`.
///
/// A transition of duration D between clip_a and clip_b is centered on the cut
/// point — the zone is [clip_a_end − D/2, clip_a_end + D/2).
/// Only considers V-row clips (even track_row, non-extracted-audio).
///
/// `alpha` = 0.0 at zone start (pure clip_a), 1.0 at zone end (pure clip_b).
pub fn active_transition_at(state: &ProjectState) -> Option<TransitionZone<'_>> {
    let t = state.current_time;

    // Only clips on the SAME lane can blend. V1 and V2 are independent layers,
    // so pairing across them used to invent transitions between clips that are
    // not adjacent on screen — the phantom blends behind "random" transitions
    // appearing where none was set.
    //
    // Two blends can also overlap when the clip between them is short (D_a/2 +
    // D_b/2 > clip_b). Previously the first pair in document order won, which is
    // arbitrary; now the blend whose cut is nearest the playhead wins.
    let mut best: Option<(f64, TransitionZone<'_>)> = None;

    for row in [0_usize, 2] {
        let mut v_clips: Vec<&TimelineClip> = state
            .timeline
            .iter()
            .filter(|c| c.track_row == row && !is_extracted_audio_clip(c))
            .collect();
        if v_clips.len() < 2 {
            continue;
        }
        v_clips.sort_unstable_by(|a, b| a.start_time.total_cmp(&b.start_time));

        for pair in v_clips.windows(2) {
            let clip_a = pair[0];
            let clip_b = pair[1];

            // Look for a non-Cut transition recorded after clip_a.
            // Use if-let + continue instead of ? so a missing transition on one
            // pair doesn't abort the entire search — other pairs may still have one.
            let tr = match state
                .transitions
                .iter()
                .find(|tr| tr.after_clip_id == clip_a.id)
            {
                Some(tr) => tr,
                None => continue,
            };
            if tr.kind.kind == velocut_core::transitions::TransitionKind::Cut {
                continue;
            }

            // Zone is centered on the cut: [clip_a_end − D/2, clip_a_end + D/2).
            // D is clamped so the zone stays inside both clips' spans; an
            // unclamped 3s transition on a 0.4s clip put the playhead on a
            // neighbouring clip while a zone was still reported, showing a
            // blend of clips that were not on screen.
            let clip_a_end = clip_a.start_time + clip_a.duration;

            // A transition only exists where the two clips actually meet. A gap
            // or an overlap means there is no shared boundary to blend across,
            // and blending anyway showed clip_b early (or late) out of nowhere.
            if (clip_b.start_time - clip_a_end).abs() > ADJACENCY_EPS {
                continue;
            }

            let d = tr.kind.clamped_secs(clip_a.duration, clip_b.duration);
            if d <= 0.0 {
                continue;
            }

            let half_d = d / 2.0;
            let zone_start = clip_a_end - half_d;
            let zone_end = clip_a_end + half_d;

            if t < zone_start || t >= zone_end {
                continue;
            }

            let local_blend = t - zone_start; // 0.0 .. D
            let alpha = (local_blend / d).clamp(0.0, 1.0) as f32;

            const ONE_FRAME: f64 = 1.0 / 30.0;

            // Source timestamps (centered on cut):
            //   clip_a: last D/2 of its source playing out over the full zone.
            //     In the second half (local_blend > half_d), clip_a freezes at its
            //     last valid frame — we never request a timestamp past source end.
            //   clip_b: first D/2 of its source; starts at source_offset, freezes
            //     before the cut, then advances from the cut to zone_end.
            let clip_a_source_ts = (clip_a.source_offset + clip_a.duration - half_d + local_blend)
                .clamp(
                    clip_a.source_offset,
                    (clip_a.source_offset + clip_a.duration - ONE_FRAME).max(clip_a.source_offset),
                )
                .max(0.0);
            let clip_b_source_ts = (clip_b.source_offset + (local_blend - half_d).max(0.0))
                .clamp(
                    clip_b.source_offset,
                    (clip_b.source_offset + clip_b.duration - ONE_FRAME).max(clip_b.source_offset),
                )
                .max(0.0);

            let dist = (t - clip_a_end).abs();
            if best.as_ref().is_none_or(|(best_dist, _)| dist < *best_dist) {
                best = Some((
                    dist,
                    TransitionZone {
                        clip_a,
                        clip_b,
                        // TransitionType is Clone; tr is a shared ref.
                        transition: tr.kind.clone(),
                        alpha,
                        clip_a_source_ts,
                        clip_b_source_ts,
                    },
                ));
            }
        }
    }

    best.map(|(_, zone)| zone)
}

// ── Transition boundary lookup ──────────────────────────────────────

/// The pair of clips a transition keyed on `after_clip_id` blends across:
/// the outgoing clip and the same-row video clip immediately following it.
///
/// `None` when the clip no longer exists, is an extracted audio clip, or its
/// successor sits on another lane / across a gap — the transition then has no
/// boundary to live on and its duration must not affect the timeline math.
pub fn transition_boundary(
    state: &ProjectState,
    after_clip_id: Uuid,
) -> Option<(&TimelineClip, &TimelineClip)> {
    let clip_a = state.timeline.iter().find(|c| c.id == after_clip_id)?;
    if is_extracted_audio_clip(clip_a) {
        return None;
    }
    let clip_a_end = clip_a.start_time + clip_a.duration;
    state
        .timeline
        .iter()
        .filter(|c| {
            c.track_row == clip_a.track_row && c.id != clip_a.id && !is_extracted_audio_clip(c)
        })
        .find(|c| (c.start_time - clip_a_end).abs() < ADJACENCY_EPS)
        .map(|clip_b| (clip_a, clip_b))
}

// ── Audio playback helper ─────────────────────────────────────────────────────

/// Return the clip that should be providing the *primary* audio at `time`.
///
/// Priority: extracted A-row clips (rows 1/3 with `linked_clip_id`) over V-row
/// clips (rows 0/2).  This ensures that after `ExtractAudioTrack`, the A-row WAV
/// plays instead of the muted V-row source.
///
/// Standalone A-row clips (no `linked_clip_id`) are intentionally excluded here —
/// they play as independent overlays via `active_overlay_clips` so they mix
/// additively with V-row audio rather than silencing it.
///
/// V-row clips with `audio_muted = true` are skipped.
#[inline]
pub fn active_audio_clip(state: &ProjectState, time: f64) -> Option<&TimelineClip> {
    // Covers use the same deterministic tie-break as `video_clip_at_time`:
    // later start wins, never document order.
    // Extracted A-row first (linked_clip_id present = V↔A pair, not standalone)
    state
        .timeline
        .iter()
        .filter(|c| {
            matches!(c.track_row, 1 | 3)
                && c.linked_clip_id.is_some()
                && c.start_time <= time
                && time < c.start_time + c.duration
        })
        .max_by(|a, b| a.start_time.total_cmp(&b.start_time))
        .or_else(|| {
            // V-row fallback — skip muted clips
            state
                .timeline
                .iter()
                .filter(|c| {
                    matches!(c.track_row, 0 | 2)
                        && !c.audio_muted
                        && c.start_time <= time
                        && time < c.start_time + c.duration
                })
                .max_by(|a, b| a.start_time.total_cmp(&b.start_time))
        })
}

/// Return all *standalone* A-row clips active at `time`.
///
/// These are independent audio files dragged to an A-row track (odd `track_row`,
/// no `linked_clip_id`).  They play simultaneously with — not instead of —
/// the primary audio from `active_audio_clip`, mirroring the `AudioOverlay`
/// behaviour in the encode pipeline.
#[inline]
pub fn active_overlay_clips(state: &ProjectState, time: f64) -> Vec<&TimelineClip> {
    state
        .timeline
        .iter()
        .filter(|c| {
            matches!(c.track_row, 1 | 3)
                && c.linked_clip_id.is_none()
                && c.start_time <= time
                && time < c.start_time + c.duration
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use velocut_core::state::{ClipType, LibraryClip, TimelineClip};
    use velocut_core::transitions::{TimelineTransition, TransitionKind, TransitionType};

    fn make_state() -> ProjectState {
        ProjectState::default()
    }

    fn add_lib_clip(
        state: &mut ProjectState,
        id: Uuid,
        name: &str,
        duration: f64,
        clip_type: ClipType,
    ) {
        state.library.push(LibraryClip {
            id,
            path: format!("C:\\test\\{name}.mp4").into(),
            name: name.to_string(),
            duration,
            clip_type,
            thumbnail_path: None,
            duration_probed: true,
            waveform_peaks: vec![0.5; 100],
            video_size: Some((1920, 1080)),
            audio_path: None,
            audio_trimmed_offset: 0.0,
        });
    }

    fn add_timeline_clip(
        state: &mut ProjectState,
        id: Uuid,
        media_id: Uuid,
        start_time: f64,
        duration: f64,
        track_row: usize,
    ) {
        state.timeline.push(TimelineClip {
            id,
            media_id,
            start_time,
            duration,
            track_row,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        });
    }

    // ── timeline_clip ──────────────────────────────────────────────────────────

    #[test]
    fn timeline_clip_found() {
        let mut state = make_state();
        let id = Uuid::new_v4();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, id, lib_id, 0.0, 5.0, 0);
        assert!(timeline_clip(&state, id).is_some());
    }

    #[test]
    fn timeline_clip_not_found() {
        let state = make_state();
        assert!(timeline_clip(&state, Uuid::new_v4()).is_none());
    }

    // ── clip_at_time ───────────────────────────────────────────────────────────

    #[test]
    fn video_clip_at_time_exact_match() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        let clip_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, clip_id, lib_id, 5.0, 3.0, 0);
        // time inside clip
        assert!(video_clip_at_time(&state, 6.0).is_some());
        assert_eq!(video_clip_at_time(&state, 6.0).unwrap().id, clip_id);
    }

    #[test]
    fn video_clip_at_time_boundary_start_inclusive() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        let clip_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, clip_id, lib_id, 2.0, 4.0, 0);
        // start is inclusive
        assert!(video_clip_at_time(&state, 2.0).is_some());
    }

    #[test]
    fn video_clip_at_time_boundary_end_exclusive() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        let clip_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, clip_id, lib_id, 1.0, 3.0, 0);
        // end is exclusive
        assert!(video_clip_at_time(&state, 4.0).is_none());
    }

    #[test]
    fn video_clip_at_time_gap() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, Uuid::new_v4(), lib_id, 0.0, 2.0, 0);
        add_timeline_clip(&mut state, Uuid::new_v4(), lib_id, 4.0, 2.0, 0);
        // gap between 2.0 and 4.0
        assert!(video_clip_at_time(&state, 3.0).is_none());
    }

    #[test]
    fn video_clip_at_time_skips_overlapping_audio_clip() {
        let mut state = make_state();
        let video_lib = Uuid::new_v4();
        let audio_lib = Uuid::new_v4();
        let video_id = Uuid::new_v4();
        let audio_id = Uuid::new_v4();
        add_lib_clip(&mut state, video_lib, "video", 10.0, ClipType::Video);
        add_lib_clip(&mut state, audio_lib, "audio", 10.0, ClipType::Audio);
        // Video span [0, 5); extracted audio span [2, 8) — the audio clip comes
        // first in the timeline vec and covers t=6 where the video does not.
        add_timeline_clip(&mut state, video_id, video_lib, 0.0, 5.0, 0);
        add_timeline_clip(&mut state, audio_id, audio_lib, 2.0, 6.0, 1);
        state.timeline.last_mut().unwrap().linked_clip_id = Some(video_id);
        // Where both cover (t=3), the video clip must win.
        assert_eq!(video_clip_at_time(&state, 3.0).unwrap().id, video_id);
        // Where only the audio clip covers (t=6), the answer is None — the
        // preview has no video to show there.
        assert!(video_clip_at_time(&state, 6.0).is_none());
    }

    // ── is_extracted_audio_clip ────────────────────────────────────────────────

    #[test]
    fn is_extracted_audio_clip_true() {
        let clip = TimelineClip {
            id: Uuid::new_v4(),
            media_id: Uuid::new_v4(),
            start_time: 0.0,
            duration: 5.0,
            track_row: 1, // odd = audio row
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: Some(Uuid::new_v4()),
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        };
        assert!(is_extracted_audio_clip(&clip));
    }

    #[test]
    fn is_extracted_audio_clip_false_video_row() {
        let clip = TimelineClip {
            id: Uuid::new_v4(),
            media_id: Uuid::new_v4(),
            start_time: 0.0,
            duration: 5.0,
            track_row: 0, // even = video row
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: Some(Uuid::new_v4()),
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        };
        assert!(!is_extracted_audio_clip(&clip));
    }

    #[test]
    fn is_extracted_audio_clip_false_no_link() {
        let clip = TimelineClip {
            id: Uuid::new_v4(),
            media_id: Uuid::new_v4(),
            start_time: 0.0,
            duration: 5.0,
            track_row: 1,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        };
        assert!(!is_extracted_audio_clip(&clip));
    }

    // ── active_transition_at ───────────────────────────────────────────────────

    #[test]
    fn active_transition_at_returns_none_for_one_clip() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, Uuid::new_v4(), lib_id, 0.0, 5.0, 0);
        state.current_time = 2.5;
        assert!(active_transition_at(&state).is_none());
    }

    #[test]
    fn active_transition_at_detects_transition_zone() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 20.0, ClipType::Video);
        let clip_a = Uuid::new_v4();
        let clip_b = Uuid::new_v4();
        add_timeline_clip(&mut state, clip_a, lib_id, 0.0, 5.0, 0);
        add_timeline_clip(&mut state, clip_b, lib_id, 5.0, 5.0, 0);

        // Add a 2-second crossfade centered on the cut at 5.0
        state.transitions.push(TimelineTransition {
            after_clip_id: clip_a,
            kind: TransitionType::new(TransitionKind::Crossfade, 2.0),
        });

        // At time=4.5 (halfway through the blend zone from clip_a side),
        // zone = [5.0 - 1.0, 5.0 + 1.0) = [4.0, 6.0)
        state.current_time = 4.5;
        let zone = active_transition_at(&state);
        assert!(zone.is_some());
        let z = zone.unwrap();
        assert_eq!(z.clip_a.id, clip_a);
        assert_eq!(z.clip_b.id, clip_b);
        assert!((z.alpha - 0.25).abs() < 0.01); // 0.5/2.0 = 0.25
    }

    #[test]
    fn active_transition_at_ignores_pairs_from_different_video_rows() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 20.0, ClipType::Video);
        let v1 = Uuid::new_v4();
        let v2 = Uuid::new_v4();
        // Same timeline position, different lanes: V1 ends where V2 starts, but
        // they never meet on screen, so there is no shared boundary to blend.
        add_timeline_clip(&mut state, v1, lib_id, 0.0, 5.0, 0);
        add_timeline_clip(&mut state, v2, lib_id, 5.0, 5.0, 2);
        state.transitions.push(TimelineTransition {
            after_clip_id: v1,
            kind: TransitionType::new(TransitionKind::Crossfade, 2.0),
        });

        state.current_time = 5.0;
        assert!(
            active_transition_at(&state).is_none(),
            "clips on separate lanes must never be paired into a transition"
        );
    }

    #[test]
    fn active_transition_at_requires_adjacent_clips() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 20.0, ClipType::Video);
        let clip_a = Uuid::new_v4();
        let clip_b = Uuid::new_v4();
        add_timeline_clip(&mut state, clip_a, lib_id, 0.0, 5.0, 0);
        // 1s gap between the clips — nothing to blend across.
        add_timeline_clip(&mut state, clip_b, lib_id, 6.0, 5.0, 0);
        state.transitions.push(TimelineTransition {
            after_clip_id: clip_a,
            kind: TransitionType::new(TransitionKind::Crossfade, 2.0),
        });

        state.current_time = 5.0;
        assert!(active_transition_at(&state).is_none());
    }

    #[test]
    fn active_transition_at_picks_the_nearest_cut_when_zones_overlap() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 40.0, ClipType::Video);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        add_timeline_clip(&mut state, a, lib_id, 0.0, 5.0, 0);
        add_timeline_clip(&mut state, b, lib_id, 5.0, 1.0, 0);
        add_timeline_clip(&mut state, c, lib_id, 6.0, 5.0, 0);
        // 4s blends on a 1s middle clip: both zones cover 4.0..8.0 / 3.0..7.0.
        state.transitions.push(TimelineTransition {
            after_clip_id: a,
            kind: TransitionType::new(TransitionKind::Crossfade, 4.0),
        });
        state.transitions.push(TimelineTransition {
            after_clip_id: b,
            kind: TransitionType::new(TransitionKind::Crossfade, 4.0),
        });

        // 6.2 is inside both zones; the cut at 6.0 is nearer than the one at 5.0.
        state.current_time = 6.2;
        let zone = active_transition_at(&state).expect("a blend is active");
        assert_eq!(
            zone.clip_a.id, b,
            "nearest cut must win over document order"
        );
        assert_eq!(zone.clip_b.id, c);
    }

    #[test]
    fn active_transition_at_clamps_zone_to_the_adjacent_clips() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 20.0, ClipType::Video);
        let prev = Uuid::new_v4();
        let clip_a = Uuid::new_v4();
        let clip_b = Uuid::new_v4();
        // clip_a is 0.3s long — too short to host the requested 3.0s crossfade.
        add_timeline_clip(&mut state, prev, lib_id, 0.0, 2.0, 0);
        add_timeline_clip(&mut state, clip_a, lib_id, 2.0, 0.3, 0);
        add_timeline_clip(&mut state, clip_b, lib_id, 2.3, 5.0, 0);
        state.transitions.push(TimelineTransition {
            after_clip_id: clip_a,
            kind: TransitionType::new(TransitionKind::Crossfade, 3.0),
        });

        // Clamped zone: [2.3 − 0.3, 2.3 + 0.3) = [2.0, 2.6), centered on the cut.
        // Before the clamp, the zone was [0.8, 3.8) and a playhead on the
        // PREDECESSOR clip reported an active transition.
        state.current_time = 1.9;
        assert!(
            active_transition_at(&state).is_none(),
            "zone must not spill outside clip_a's span"
        );

        // Just inside clip_a, near the zone start — alpha must account for the
        // clamped length, not the slider value.
        state.current_time = 2.05;
        let zone = active_transition_at(&state).expect("zone inside clip_a");
        // alpha = (2.05 − 2.0) / 0.6 ≈ 0.083; well below the 0.25 an unclamped
        // 3.0s zone would give.
        assert!(zone.alpha < 0.15, "alpha used the unclamped duration");

        // Alpha is 0.5 at the cut itself, as in any centered zone.
        state.current_time = 2.3;
        let zone = active_transition_at(&state).expect("zone at the cut");
        assert!((zone.alpha - 0.5).abs() < 0.01);

        // Second half — inside clip_b.
        state.current_time = 2.5;
        let zone = active_transition_at(&state).expect("zone inside clip_b");
        assert_eq!(zone.clip_a.id, clip_a);
        assert_eq!(zone.clip_b.id, clip_b);
    }

    #[test]
    fn transition_boundary_resolves_no_pair_for_gaps_or_cross_rows() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 20.0, ClipType::Video);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        add_timeline_clip(&mut state, a, lib_id, 0.0, 5.0, 0);
        add_timeline_clip(&mut state, b, lib_id, 5.0, 5.0, 0); // adjacent, same row
        add_timeline_clip(&mut state, c, lib_id, 12.0, 5.0, 0); // 2s gap after b

        let (az, bz) = transition_boundary(&state, a).expect("b is adjacent to a");
        assert_eq!(az.id, a);
        assert_eq!(bz.id, b);

        // b's successor is 2s away — no blend boundary exists.
        assert!(transition_boundary(&state, b).is_none());
        // Unknown id.
        assert!(transition_boundary(&state, Uuid::new_v4()).is_none());
    }

    #[test]
    fn active_transition_at_outside_zone_returns_none() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 20.0, ClipType::Video);
        let clip_a = Uuid::new_v4();
        let clip_b = Uuid::new_v4();
        add_timeline_clip(&mut state, clip_a, lib_id, 0.0, 5.0, 0);
        add_timeline_clip(&mut state, clip_b, lib_id, 5.0, 5.0, 0);

        state.transitions.push(TimelineTransition {
            after_clip_id: clip_a,
            kind: TransitionType::new(TransitionKind::Crossfade, 2.0),
        });

        // Before zone: time=3.5 (< 4.0)
        state.current_time = 3.5;
        assert!(active_transition_at(&state).is_none());

        // After zone: time=6.5 (>= 6.0)
        state.current_time = 6.5;
        assert!(active_transition_at(&state).is_none());
    }

    // ── active_audio_clip ──────────────────────────────────────────────────────

    #[test]
    fn active_audio_clip_returns_v_row_clip() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        let clip_id = Uuid::new_v4();
        add_timeline_clip(&mut state, clip_id, lib_id, 0.0, 5.0, 0);
        state.current_time = 2.0;
        let result = active_audio_clip(&state, 2.0);
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, clip_id);
    }

    #[test]
    fn active_audio_clip_skips_muted_v_row() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        let clip_id = Uuid::new_v4();
        let clip = TimelineClip {
            id: clip_id,
            media_id: lib_id,
            start_time: 0.0,
            duration: 5.0,
            track_row: 0,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None,
            audio_muted: true, // muted!
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        };
        state.timeline.push(clip);
        assert!(active_audio_clip(&state, 2.0).is_none());
    }

    // ── active_overlay_clips ──────────────────────────────────────────────────

    #[test]
    fn active_overlay_clips_returns_standalone_audio() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "music", 30.0, ClipType::Audio);
        let overlay_id = Uuid::new_v4();
        // Standalone A-row clip: odd row, no linked_clip_id
        state.timeline.push(TimelineClip {
            id: overlay_id,
            media_id: lib_id,
            start_time: 0.0,
            duration: 10.0,
            track_row: 1,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: None, // standalone
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        });
        let overlays = active_overlay_clips(&state, 5.0);
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].id, overlay_id);
    }

    #[test]
    fn active_overlay_clips_excludes_linked_audio() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        // Extracted audio clip: odd row, HAS linked_clip_id
        state.timeline.push(TimelineClip {
            id: Uuid::new_v4(),
            media_id: lib_id,
            start_time: 0.0,
            duration: 5.0,
            track_row: 1,
            source_offset: 0.0,
            volume: 1.0,
            linked_clip_id: Some(Uuid::new_v4()), // linked → extracted audio
            audio_muted: false,
            fade_in_secs: 0.0,
            fade_in_start_secs: 0.0,
            fade_out_secs: 0.0,
            fade_out_end_secs: 0.0,
            filter: Default::default(),
        });
        let overlays = active_overlay_clips(&state, 2.0);
        assert!(overlays.is_empty());
    }

    // ── wav_seek_t ─────────────────────────────────────────────────────────────

    fn lib_with_trim(trimmed_offset: f64) -> LibraryClip {
        LibraryClip {
            id: Uuid::new_v4(),
            path: "C:\\test\\wav.wav".into(),
            name: "wav".to_string(),
            duration: 20.0,
            clip_type: ClipType::Audio,
            thumbnail_path: None,
            duration_probed: true,
            waveform_peaks: Vec::new(),
            video_size: None,
            audio_path: None,
            audio_trimmed_offset: trimmed_offset,
        }
    }

    #[test]
    fn wav_seek_t_full_file_extraction() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        let clip_id = Uuid::new_v4();
        add_timeline_clip(&mut state, clip_id, lib_id, 3.0, 5.0, 0);
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == clip_id)
            .unwrap()
            .source_offset = 2.0;

        let clip = timeline_clip(&state, clip_id).unwrap();
        // WAV starts at source 0: 1.5s into the clip → 2.0 + 1.5.
        assert!((wav_seek_t(clip, &lib_with_trim(0.0), 4.5) - 3.5).abs() < 1e-9);
    }

    #[test]
    fn wav_seek_t_trimmed_extraction_does_not_double_count_offset() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        let clip_id = Uuid::new_v4();
        add_timeline_clip(&mut state, clip_id, lib_id, 3.0, 5.0, 0);
        state
            .timeline
            .iter_mut()
            .find(|c| c.id == clip_id)
            .unwrap()
            .source_offset = 2.0;

        let clip = timeline_clip(&state, clip_id).unwrap();
        // WAV already starts at source 2.0: 1.5s in → seek to 1.5, not 3.5.
        assert!((wav_seek_t(clip, &lib_with_trim(2.0), 4.5) - 1.5).abs() < 1e-9);
    }

    #[test]
    fn wav_seek_t_clamps_before_clip_start() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        let clip_id = Uuid::new_v4();
        add_timeline_clip(&mut state, clip_id, lib_id, 3.0, 5.0, 0);

        let clip = timeline_clip(&state, clip_id).unwrap();
        assert_eq!(wav_seek_t(clip, &lib_with_trim(0.0), 1.0), 0.0);
    }
}
