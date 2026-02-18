// crates/velocut-core/src/media_types.rs
//
// Types that flow across the channel between velocut-media and velocut-ui.
// No egui, no ffmpeg — just plain data.

use std::path::PathBuf;
use uuid::Uuid;

/// Results sent from the MediaWorker background threads to the UI.
pub enum MediaResult {
    Duration   { id: Uuid, seconds: f64 },
    Thumbnail  { id: Uuid, width: u32, height: u32, data: Vec<u8> },
    Waveform   { id: Uuid, peaks: Vec<f32> },
    VideoFrame { id: Uuid, width: u32, height: u32, data: Vec<u8> },
    VideoSize  { id: Uuid, width: u32, height: u32 },
    FrameSaved { path: PathBuf },
    AudioPath  { id: Uuid, path: PathBuf },
    Error      { id: Uuid, msg: String },
}

/// A decoded frame from the dedicated playback pipeline.
pub struct PlaybackFrame {
    pub id:        Uuid,
    pub timestamp: f64,
    pub width:     u32,
    pub height:    u32,
    pub data:      Vec<u8>, // RGBA
}