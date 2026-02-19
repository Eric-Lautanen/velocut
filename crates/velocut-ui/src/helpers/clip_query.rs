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