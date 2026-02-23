// crates/velocut-core/src/media_types.rs
//
// Types that flow across the channel between velocut-media and velocut-ui.
// No egui, no ffmpeg — just plain data.

use std::path::PathBuf;
use uuid::Uuid;

/// Results sent from the MediaWorker background threads to the UI.
pub enum MediaResult {
    // ── Probe results ─────────────────────────────────────────────────────────
    Duration   { id: Uuid, seconds: f64 },
    Thumbnail  { id: Uuid, width: u32, height: u32, data: Vec<u8> },
    Waveform   { id: Uuid, peaks: Vec<f32> },
    VideoFrame { id: Uuid, width: u32, height: u32, data: Vec<u8> },
    VideoSize  { id: Uuid, width: u32, height: u32 },
    FrameSaved { path: PathBuf },
    AudioPath  { id: Uuid, path: PathBuf },
    Error      { id: Uuid, msg: String },

    // ── Encode results ────────────────────────────────────────────────────────
    /// Periodic progress report from the encode thread.
    /// `frame` is the number of output frames written so far;
    /// `total_frames` is the sum of all clip frame counts at the target fps.
    EncodeProgress {
        job_id:       Uuid,
        frame:        u64,
        total_frames: u64,
    },
    /// Encode completed successfully.
    EncodeDone {
        job_id: Uuid,
        path:   PathBuf,
    },
    /// Encode failed or was cancelled.
    EncodeError {
        job_id: Uuid,
        msg:    String,
    },
}

/// A decoded frame from the dedicated playback pipeline.
pub struct PlaybackFrame {
    pub id:        Uuid,
    pub timestamp: f64,
    pub width:     u32,
    pub height:    u32,
    pub data:      Vec<u8>, // RGBA
}

/// Parameters for a one-shot blended scrub-frame request.
/// Sent from `video_module` to `MediaWorker::request_transition_frame`.
/// The worker decodes a frame from each clip and returns a blended RGBA result
/// via the scrub channel, keyed by `clip_a_id`.
pub struct TransitionScrubRequest {
    pub clip_a_id:   Uuid,
    pub clip_a_path: PathBuf,
    /// Source-file timestamp for clip_a (seconds).
    pub clip_a_ts:   f64,
    pub clip_b_id:   Uuid,
    pub clip_b_path: PathBuf,
    /// Source-file timestamp for clip_b (seconds).
    pub clip_b_ts:   f64,
    /// Blend factor: 0.0 = fully clip_a, 1.0 = fully clip_b.
    pub alpha:       f32,
    pub kind:        crate::transitions::TransitionKind,
}

/// Passed to `MediaWorker::start_blend_playback` so the playback thread can
/// open a second decoder for clip_b and blend frames at the clip boundary.
pub struct PlaybackTransitionSpec {
    pub clip_b_id:           Uuid,
    pub clip_b_path:         PathBuf,
    /// clip_b.source_offset — where to seek clip_b's decoder on open.
    pub clip_b_source_start: f64,
    /// Source timestamp in clip_a's decoded stream at which blending begins.
    /// For the clip_a side of a centered transition:
    ///   = clip_a.source_offset + clip_a.duration − transition.duration_secs / 2
    /// For the clip_b side (when clip_b takes over as primary decoder):
    ///   = clip_b.source_offset  (blend starts immediately from clip_b's first frame)
    pub blend_start_ts:      f64,
    pub duration:            f32,
    pub kind:                crate::transitions::TransitionKind,
    /// Alpha value at the start of blending for this decoder's half of the transition.
    /// Clip_a side: 0.0  (fades out from pure clip_a → 0.5 at cut point).
    /// Clip_b side: 0.5  (continues from the cut point → 1.0 pure clip_b).
    pub alpha_start:         f32,
    /// When true, swap a and b in the blend call.
    /// Set for the clip_b (incoming) side of the transition: the main decoder is
    /// clip_b but blend_rgba_transition expects clip_a as the first argument.
    pub invert_ab:           bool,
}