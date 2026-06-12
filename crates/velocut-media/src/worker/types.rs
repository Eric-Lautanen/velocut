// crates/velocut-media/src/worker/types.rs
//
// Internal types for frame requests and playback commands.
// Extracted from worker.rs.

use std::path::PathBuf;

use uuid::Uuid;

use velocut_core::media_types::PlaybackTransitionSpec;

// ── Internal types ────────────────────────────────────────────────────────────

pub(super) struct FrameRequest {
    pub(super) id: Uuid,
    pub(super) path: PathBuf,
    pub(super) timestamp: f64,
    pub(super) aspect: f32,
    /// When Some, decode at these pixel dimensions instead of the default
    /// scrub resolution (320px). Set to the current preview canvas size so
    /// L2 scrub frames fill the panel without blurry upscaling.
    pub(super) preview_size: Option<(u32, u32)>,
}

pub(super) enum PlaybackCmd {
    Start {
        id: Uuid,
        path: PathBuf,
        ts: f64,
        aspect: f32,
        preview_size: Option<(u32, u32)>,
    },
    /// Like Start but also carries blend info so the pb thread can open a second
    /// decoder for clip_b and blend frames during the transition zone.
    StartBlend {
        id: Uuid,
        path: PathBuf,
        ts: f64,
        aspect: f32,
        blend: PlaybackTransitionSpec,
        preview_size: Option<(u32, u32)>,
    },
    Stop,
    /// Pre-open and start burning a decoder for the next clip so the transition
    /// at the clip boundary is instant.  Sent ~500 ms before the current clip
    /// ends.  The pb thread opens the decoder, sets skip_until_pts, and advances
    /// the burn incrementally (10 packets per primary frame) so the decoder is
    /// ready by the time Start / StartBlend arrives.
    PreBuffer {
        id: Uuid,
        path: PathBuf,
        ts: f64,
        aspect: f32,
        preview_size: Option<(u32, u32)>,
    },
}
