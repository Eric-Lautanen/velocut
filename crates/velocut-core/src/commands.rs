// crates/velocut-core/src/commands.rs
//
// Every user action in VeloCut is expressed as an EditorCommand.
// Modules emit these; app.rs processes them after the UI pass.
// Adding a new feature = add a variant here + one match arm in app.rs.

use std::path::PathBuf;
use uuid::Uuid;
use crate::state::AspectRatio;

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
    AddToTimeline { media_id: Uuid, at_time: f64 },
    DeleteTimelineClip(Uuid),
    SelectTimelineClip(Option<Uuid>),
    MoveTimelineClip { id: Uuid, new_start: f64 },
    TrimClipStart  { id: Uuid, new_source_offset: f64, new_duration: f64 },
    TrimClipEnd    { id: Uuid, new_duration: f64 },
    SplitClipAt(f64),

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
    /// Set the crossfade duration (in seconds) applied between all adjacent clips.
    /// 0.0 = hard cut (no transition). Stored in ProjectState and serialized.
    SetCrossfadeDuration(f32),

    // ── View / UI ────────────────────────────────────────────────────────────
    SetAspectRatio(AspectRatio),
    SetTimelineZoom(f32),
    ClearSaveStatus,
    SaveFrameToDisk { path: PathBuf, timestamp: f64 },
    RequestSaveFramePicker { path: PathBuf, timestamp: f64 },
}