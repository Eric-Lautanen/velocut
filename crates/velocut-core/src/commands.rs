// crates/velocut-core/src/commands.rs
//
// Every user action in VeloCut is expressed as an EditorCommand.
// Modules emit these; app.rs processes them after the UI pass.
// Adding a new feature = add a variant here + one match arm in app.rs.

use std::path::PathBuf;
use uuid::Uuid;
use crate::state::AspectRatio;
use crate::transitions::TransitionType;

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
    AddToTimeline { media_id: Uuid, at_time: f64, track_row: usize },
    DeleteTimelineClip(Uuid),
    SelectTimelineClip(Option<Uuid>),
    MoveTimelineClip { id: Uuid, new_start: f64 },
    TrimClipStart  { id: Uuid, new_source_offset: f64, new_duration: f64 },
    TrimClipEnd    { id: Uuid, new_duration: f64 },
    SplitClipAt(f64),
    /// Extract the audio from a video timeline clip onto the A track below it.
    /// Mutes audio on the source video clip and creates a linked audio clip.
    ExtractAudioTrack(Uuid),
    /// Set per-clip gain (0.0–2.0). Applied multiplicatively with global volume.
    SetClipVolume { id: Uuid, volume: f32 },

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
    RenderMP4 { filename: String, width: u32, height: u32, fps: u32 },
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
    SetTransition { after_clip_id: Uuid, kind: TransitionType },
    /// Remove the transition after a specific clip, reverting it to a hard cut.
    RemoveTransition(Uuid),

    // ── View / UI ────────────────────────────────────────────────────────────
    SetAspectRatio(AspectRatio),
    SetTimelineZoom(f32),
    ClearSaveStatus,
    SaveFrameToDisk { path: PathBuf, timestamp: f64 },
    RequestSaveFramePicker { path: PathBuf, timestamp: f64 },
}