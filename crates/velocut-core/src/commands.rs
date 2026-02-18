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
    RenderMP4 { filename: String, width: u32, height: u32, fps: u32 },

    // ── View / UI ────────────────────────────────────────────────────────────
    SetAspectRatio(AspectRatio),
    SetTimelineZoom(f32),
    ClearSaveStatus,
    SaveFrameToDisk { path: PathBuf, timestamp: f64 },
    RequestSaveFramePicker { path: PathBuf, timestamp: f64 },
}