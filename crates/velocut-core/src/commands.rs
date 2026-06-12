// crates/velocut-core/src/commands.rs
//
// Every user action in VeloCut is expressed as an EditorCommand.
// Modules emit these; app.rs processes them after the UI pass.
// Adding a new feature = add a variant here + one match arm in app.rs.

use crate::filters::FilterParams;
use crate::state::{AspectRatio, ProjectState};
use crate::transitions::TransitionType;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum EditorCommand {
    // ── Playback ─────────────────────────────────────────────────────────────
    SetPlayhead(f64),
    Play,
    Pause,
    Stop,
    SetVolume(f32),
    ToggleMute,

    // ── Library ──────────────────────────────────────────────────────────────
    ImportFile(PathBuf),
    DeleteLibraryClip(Uuid),
    SelectLibraryClip(Option<Uuid>),

    // ── Timeline ─────────────────────────────────────────────────────────────
    AddToTimeline {
        media_id: Uuid,
        at_time: f64,
        track_row: usize,
    },
    DeleteTimelineClip(Uuid),
    SelectTimelineClip(Option<Uuid>),
    MoveTimelineClip {
        id: Uuid,
        new_start: f64,
        new_row: usize,
    },
    TrimClipStart {
        id: Uuid,
        new_source_offset: f64,
        new_duration: f64,
    },
    TrimClipEnd {
        id: Uuid,
        new_duration: f64,
    },
    SplitClipAt(f64),
    /// Extract the audio from a video timeline clip onto the A track below it.
    /// Mutes audio on the source video clip and creates a linked audio clip.
    ExtractAudioTrack(Uuid),
    /// Set per-clip gain (0.0–2.0). Applied multiplicatively with global volume.
    SetClipVolume {
        id: Uuid,
        volume: f32,
    },
    /// Set per-clip audio fade-in duration (seconds, 0.0 = none).
    SetClipFadeIn {
        id: Uuid,
        secs: f32,
    },
    /// Set silence before the fade-in ramp (0 = ramp at clip start).
    SetClipFadeInStart {
        id: Uuid,
        secs: f32,
    },
    /// Set per-clip audio fade-out ramp duration (seconds, 0.0 = none).
    SetClipFadeOut {
        id: Uuid,
        secs: f32,
    },
    /// Set silence after fade-out ramp ends, before clip end (0 = ramp ends at clip boundary).
    SetClipFadeOutEnd {
        id: Uuid,
        secs: f32,
    },

    // ── Undo / Redo ───────────────────────────────────────────────────────────
    /// Snapshot the current ProjectState onto the undo stack and clear redo.
    /// Emitted by timeline.rs immediately before any user-visible mutation
    /// (button click, drag_started). Never emitted during per-frame drag updates.
    PushUndoSnapshot,
    /// Restore the most recent undo snapshot.
    Undo,
    /// Re-apply the most recently undone snapshot.
    Redo,

    // ── Export ───────────────────────────────────────────────────────────────
    /// Emitted by ExportModule when the user clicks Render. `filename` is the
    /// bare stem (no extension, no directory); app.rs opens the save dialog and
    /// calls MediaWorker::start_encode with the resolved PathBuf.
    RenderMP4 {
        filename: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    /// Request the active encode job (if any) to stop. The encode thread
    /// observes its cancel AtomicBool and exits after finishing the current frame.
    CancelEncode(Uuid),
    /// Clear encode_job / encode_progress / encode_done / encode_error in
    /// ProjectState. Emitted when the user dismisses a done/error banner.
    ClearEncodeStatus,
    /// Set the crossfade duration (in seconds) for ALL clip boundaries at once.
    /// Convenience for the global slider; sets a Crossfade transition on every
    /// adjacent touching pair. 0.0 clears all transitions (all become Cut).
    SetCrossfadeDuration(f32),
    /// Set or update the transition at a specific clip boundary.
    /// `after_clip_id` is the TimelineClip UUID that comes before the transition.
    SetTransition {
        after_clip_id: Uuid,
        kind: TransitionType,
    },
    /// Remove the transition after a specific clip, reverting it to a hard cut.
    RemoveTransition(Uuid),

    SetClipFilter {
        id: Uuid,
        filter: FilterParams,
    },

    // ── View / UI ────────────────────────────────────────────────────────────
    SetAspectRatio(AspectRatio),
    SetTimelineZoom(f32),
    ClearSaveStatus,
    SaveFrameToDisk {
        path: PathBuf,
        timestamp: f64,
    },
    RequestSaveFramePicker {
        path: PathBuf,
        timestamp: f64,
    },

    // ── Project reset ─────────────────────────────────────────────────────────
    /// Full app reset: wipe library, timeline, transitions, all temp WAV files,
    /// all GPU texture caches, playback state, and undo/redo history.
    /// Equivalent to "start a new project" without restarting the process.
    /// app.rs::process_command handles the ordered teardown sequence.
    ClearProject,
}

impl EditorCommand {
    /// Validate that this command can be applied to the given state.
    /// Returns `Ok(())` if valid, or an error message describing the problem.
    pub fn validate(&self, state: &ProjectState) -> Result<(), String> {
        match self {
            EditorCommand::SetPlayhead(t) => {
                if *t < 0.0 {
                    return Err("Playhead cannot be negative".to_string());
                }
                let total = state.total_duration();
                if total > 0.0 && *t > total {
                    return Err(format!(
                        "Playhead {:.2}s exceeds timeline duration {:.2}s",
                        t, total
                    ));
                }
            }
            EditorCommand::SetVolume(v) => {
                if *v < 0.0 || *v > 2.0 {
                    return Err("Volume must be between 0.0 and 2.0".to_string());
                }
            }
            EditorCommand::SetClipVolume { id, volume } => {
                if state.timeline.iter().all(|c| c.id != *id) {
                    return Err("Clip not found in timeline".to_string());
                }
                if *volume < 0.0 || *volume > 2.0 {
                    return Err("Volume must be between 0.0 and 2.0".to_string());
                }
            }
            EditorCommand::SetClipFadeIn { id, secs } => {
                let clip = state.timeline.iter().find(|c| c.id == *id);
                match clip {
                    None => return Err("Clip not found in timeline".to_string()),
                    Some(c) => {
                        if *secs < 0.0 {
                            return Err("Fade-in duration cannot be negative".to_string());
                        }
                        if c.fade_in_start_secs + *secs > c.duration as f32 {
                            return Err(format!(
                                "Fade-in start ({:.2}s) + duration ({:.2}s) = {:.2}s exceeds clip duration ({:.2}s)",
                                c.fade_in_start_secs, secs, c.fade_in_start_secs + secs, c.duration
                            ));
                        }
                    }
                }
            }
            EditorCommand::SetClipFadeInStart { id, secs } => {
                let clip = state.timeline.iter().find(|c| c.id == *id);
                match clip {
                    None => return Err("Clip not found in timeline".to_string()),
                    Some(c) => {
                        if *secs < 0.0 {
                            return Err("Fade-in start silence cannot be negative".to_string());
                        }
                        if *secs + c.fade_in_secs > c.duration as f32 {
                            return Err(format!(
                                "Fade-in start ({:.2}s) + fade-in duration ({:.2}s) = {:.2}s exceeds clip duration ({:.2}s)",
                                secs, c.fade_in_secs, secs + c.fade_in_secs, c.duration
                            ));
                        }
                    }
                }
            }
            EditorCommand::SetClipFadeOut { id, secs } => {
                let clip = state.timeline.iter().find(|c| c.id == *id);
                match clip {
                    None => return Err("Clip not found in timeline".to_string()),
                    Some(c) => {
                        if *secs < 0.0 {
                            return Err("Fade-out duration cannot be negative".to_string());
                        }
                        if c.fade_out_end_secs + *secs > c.duration as f32 {
                            return Err(format!(
                                "Fade-out end silence ({:.2}s) + duration ({:.2}s) = {:.2}s exceeds clip duration ({:.2}s)",
                                c.fade_out_end_secs, secs, c.fade_out_end_secs + secs, c.duration
                            ));
                        }
                    }
                }
            }
            EditorCommand::SetClipFadeOutEnd { id, secs } => {
                let clip = state.timeline.iter().find(|c| c.id == *id);
                match clip {
                    None => return Err("Clip not found in timeline".to_string()),
                    Some(c) => {
                        if *secs < 0.0 {
                            return Err("Fade-out end silence cannot be negative".to_string());
                        }
                        if *secs + c.fade_out_secs > c.duration as f32 {
                            return Err(format!(
                                "Fade-out end silence ({:.2}s) + fade-out duration ({:.2}s) = {:.2}s exceeds clip duration ({:.2}s)",
                                secs, c.fade_out_secs, secs + c.fade_out_secs, c.duration
                            ));
                        }
                    }
                }
            }
            EditorCommand::MoveTimelineClip {
                id,
                new_start,
                new_row,
            } => {
                let Some(clip) = state.timeline.iter().find(|c| c.id == *id) else {
                    return Err("Clip not found in timeline".to_string());
                };
                if *new_start < 0.0 {
                    return Err("Clip start time cannot be negative".to_string());
                }
                if clip.linked_clip_id.is_some() && *new_row % 2 == 0 {
                    return Err("Cannot move audio-extracted clip to video track".to_string());
                }
                // Enforce row range: 0-3 for the 4-track layout
                if *new_row > 3 {
                    return Err("Track row must be 0-3".to_string());
                }
            }
            EditorCommand::TrimClipStart {
                id,
                new_source_offset,
                new_duration,
            } => {
                if state.timeline.iter().all(|c| c.id != *id) {
                    return Err("Clip not found in timeline".to_string());
                }
                if *new_source_offset < 0.0 {
                    return Err("Source offset cannot be negative".to_string());
                }
                if *new_duration <= 0.0 {
                    return Err("Duration must be positive".to_string());
                }
            }
            EditorCommand::TrimClipEnd { id, new_duration } => {
                if state.timeline.iter().all(|c| c.id != *id) {
                    return Err("Clip not found in timeline".to_string());
                }
                if *new_duration <= 0.0 {
                    return Err("Duration must be positive".to_string());
                }
            }
            EditorCommand::SplitClipAt(t) => {
                if *t < 0.0 {
                    return Err("Split time cannot be negative".to_string());
                }
                let total = state.total_duration();
                if total > 0.0 && *t > total {
                    return Err(format!(
                        "Split time {:.2}s exceeds timeline duration {:.2}s",
                        t, total
                    ));
                }
                // Check there's a clip at this position with room to split
                let min_dur = 2.0 / 30.0;
                let splittable = state.timeline.iter().any(|c| {
                    *t > c.start_time + min_dur && *t < c.start_time + c.duration - min_dur
                });
                if !splittable {
                    return Err(format!(
                        "No clip at {:.2}s with enough room to split (need > {:.3}s on each side)",
                        t, min_dur
                    ));
                }
            }
            EditorCommand::SetTimelineZoom(z) => {
                if *z < 0.01 || *z > 1000.0 {
                    return Err("Zoom must be between 0.01 and 1000.0".to_string());
                }
            }
            EditorCommand::SetCrossfadeDuration(d) => {
                if *d < 0.0 {
                    return Err("Crossfade duration cannot be negative".to_string());
                }
                if *d > 30.0 {
                    return Err("Crossfade duration cannot exceed 30 seconds".to_string());
                }
            }
            EditorCommand::RenderMP4 {
                width, height, fps, ..
            } => {
                if *width == 0 || *height == 0 {
                    return Err("Render dimensions must be non-zero".to_string());
                }
                if *width % 2 != 0 || *height % 2 != 0 {
                    return Err("Render dimensions must be even (YUV420P requirement)".to_string());
                }
                if ![24u32, 30, 60].contains(fps) {
                    return Err("Frame rate must be 24, 30, or 60 fps".to_string());
                }
                if state.timeline.is_empty() {
                    return Err("Cannot render: timeline is empty".to_string());
                }
            }
            EditorCommand::CancelEncode(job_id) => {
                if state.encode_job != Some(*job_id) {
                    return Err("No active encode job with this ID".to_string());
                }
            }
            EditorCommand::SetClipFilter { id, .. } => {
                if state.timeline.iter().all(|c| c.id != *id) {
                    return Err("Clip not found in timeline".to_string());
                }
            }
            EditorCommand::DeleteTimelineClip(id)
            | EditorCommand::SelectTimelineClip(Some(id))
            | EditorCommand::ExtractAudioTrack(id) => {
                if state.timeline.iter().all(|c| c.id != *id) {
                    return Err("Clip not found in timeline".to_string());
                }
                // ExtractAudioTrack: check it's a video clip that hasn't been extracted
                if let EditorCommand::ExtractAudioTrack(cid) = self {
                    if let Some(clip) = state.timeline.iter().find(|c| c.id == *cid) {
                        if clip.audio_muted {
                            return Err("Audio already extracted from this clip".to_string());
                        }
                        if clip.track_row % 2 != 0 {
                            return Err("Cannot extract audio from an audio-track clip".to_string());
                        }
                    }
                }
            }
            EditorCommand::DeleteLibraryClip(id) | EditorCommand::SelectLibraryClip(Some(id)) => {
                if state.library.iter().all(|c| c.id != *id) {
                    return Err("Clip not found in library".to_string());
                }
            }
            EditorCommand::SetTransition {
                after_clip_id,
                kind,
            } => {
                if state.timeline.iter().all(|c| c.id != *after_clip_id) {
                    return Err("Clip not found in timeline".to_string());
                }
                if kind.kind != crate::transitions::TransitionKind::Cut && kind.duration_secs <= 0.0
                {
                    return Err("Transition duration must be positive".to_string());
                }
                if kind.duration_secs > 10.0 {
                    return Err("Transition duration cannot exceed 10 seconds".to_string());
                }
                // Check there's a next clip to transition to
                let clip = state.timeline.iter().find(|c| c.id == *after_clip_id);
                if let Some(c) = clip {
                    let clip_end = c.start_time + c.duration;
                    let has_next = state.timeline.iter().any(|nc| {
                        nc.id != *after_clip_id
                            && nc.track_row % 2 == 0
                            && (nc.start_time - clip_end).abs() < 0.05
                    });
                    if !has_next {
                        return Err("No adjacent clip after this one for a transition".to_string());
                    }
                }
            }
            EditorCommand::RemoveTransition(after_clip_id) => {
                if state.timeline.iter().all(|c| c.id != *after_clip_id) {
                    return Err("Clip not found in timeline".to_string());
                }
                // Check there's actually a transition to remove
                if !state
                    .transitions
                    .iter()
                    .any(|t| t.after_clip_id == *after_clip_id)
                {
                    return Err("No transition found at this clip boundary".to_string());
                }
            }
            EditorCommand::SaveFrameToDisk { timestamp, .. }
            | EditorCommand::RequestSaveFramePicker { timestamp, .. } => {
                if *timestamp < 0.0 {
                    return Err("Frame timestamp cannot be negative".to_string());
                }
            }
            EditorCommand::ImportFile(path) => {
                if path.as_os_str().is_empty() {
                    return Err("Import path cannot be empty".to_string());
                }
            }
            // Commands with no validation requirements
            EditorCommand::Play
            | EditorCommand::Pause
            | EditorCommand::Stop
            | EditorCommand::ToggleMute
            | EditorCommand::AddToTimeline { .. }
            | EditorCommand::SelectTimelineClip(None)
            | EditorCommand::SelectLibraryClip(None)
            | EditorCommand::SetAspectRatio(_)
            | EditorCommand::ClearSaveStatus
            | EditorCommand::ClearEncodeStatus
            | EditorCommand::ClearProject
            | EditorCommand::PushUndoSnapshot
            | EditorCommand::Undo
            | EditorCommand::Redo => {}
        }
        Ok(())
    }
}
