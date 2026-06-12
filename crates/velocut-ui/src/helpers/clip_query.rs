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

/// Return the timeline clip that contains `time` (i.e. the clip currently
/// under the playhead), or `None` when the playhead is in a gap.
///
/// Replaces the inline predicate used in video_module.rs, audio_module.rs,
/// and app.rs for "what is playing right now" queries.
#[inline]
pub fn clip_at_time(state: &ProjectState, time: f64) -> Option<&TimelineClip> {
    state
        .timeline
        .iter()
        .find(|c| time >= c.start_time && time < c.start_time + c.duration)
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

/// Returns blend info if `state.current_time` is inside a transition zone, else `None`.
///
/// A transition of duration D between clip_a and clip_b is centered on the cut
/// point — the zone is [clip_a_end − D/2, clip_a_end + D/2).
/// Only considers V-row clips (even track_row, non-extracted-audio).
///
/// `alpha` = 0.0 at zone start (pure clip_a), 1.0 at zone end (pure clip_b).
pub fn active_transition_at(state: &ProjectState) -> Option<TransitionZone<'_>> {
    let t = state.current_time;

    // Collect V-row video clips, sorted by timeline position.
    let mut v_clips: Vec<&TimelineClip> = state
        .timeline
        .iter()
        .filter(|c| c.track_row % 2 == 0 && !is_extracted_audio_clip(c))
        .collect();
    if v_clips.len() < 2 {
        return None;
    }
    v_clips.sort_unstable_by(|a, b| a.start_time.total_cmp(&b.start_time));

    for pair in v_clips.windows(2) {
        let clip_a = pair[0];
        let clip_b = pair[1];

        // Look for a non-Cut transition recorded after clip_a.
        // Use if-let + continue instead of ? so a missing transition on one pair
        // doesn't abort the entire search — other pairs may still have one.
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

        let d = tr.kind.duration_secs as f64;
        if d <= 0.0 {
            continue;
        }

        // Zone is centered on the cut point: [clip_a_end − D/2, clip_a_end + D/2).
        let clip_a_end = clip_a.start_time + clip_a.duration;
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

        return Some(TransitionZone {
            clip_a,
            clip_b,
            transition: tr.kind.clone(), // TransitionType is Clone; tr is a shared ref
            alpha,
            clip_a_source_ts,
            clip_b_source_ts,
        });
    }
    None
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
    // Extracted A-row first (linked_clip_id present = V↔A pair, not standalone)
    state
        .timeline
        .iter()
        .find(|c| {
            matches!(c.track_row, 1 | 3)
                && c.linked_clip_id.is_some()
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
    fn clip_at_time_exact_match() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        let clip_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, clip_id, lib_id, 5.0, 3.0, 0);
        // time inside clip
        assert!(clip_at_time(&state, 6.0).is_some());
        assert_eq!(clip_at_time(&state, 6.0).unwrap().id, clip_id);
    }

    #[test]
    fn clip_at_time_boundary_start_inclusive() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        let clip_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, clip_id, lib_id, 2.0, 4.0, 0);
        // start is inclusive
        assert!(clip_at_time(&state, 2.0).is_some());
    }

    #[test]
    fn clip_at_time_boundary_end_exclusive() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        let clip_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, clip_id, lib_id, 1.0, 3.0, 0);
        // end is exclusive
        assert!(clip_at_time(&state, 4.0).is_none());
    }

    #[test]
    fn clip_at_time_gap() {
        let mut state = make_state();
        let lib_id = Uuid::new_v4();
        add_lib_clip(&mut state, lib_id, "test", 10.0, ClipType::Video);
        add_timeline_clip(&mut state, Uuid::new_v4(), lib_id, 0.0, 2.0, 0);
        add_timeline_clip(&mut state, Uuid::new_v4(), lib_id, 4.0, 2.0, 0);
        // gap between 2.0 and 4.0
        assert!(clip_at_time(&state, 3.0).is_none());
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
}
