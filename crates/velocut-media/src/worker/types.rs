// crates/velocut-media/src/worker/types.rs
//
// Internal types for frame requests and playback commands.
// Extracted from worker.rs.

use std::path::PathBuf;

use uuid::Uuid;

use velocut_core::media_types::PlaybackTransitionSpec;

// ── Internal types ────────────────────────────────────────────────────────────

/// One scrub-frame request: what to decode and how the result is stamped.
///
/// Public because `MediaWorker::request_frame` takes the whole request instead
/// of seven positional arguments — UI callers build it directly, and so do the
/// poison-pill writes in `MediaWorker::shutdown`.
pub struct FrameRequest {
    pub id: Uuid,
    pub path: PathBuf,
    pub timestamp: f64,
    pub aspect: f32,
    /// When Some, decode at these pixel dimensions instead of the default
    /// scrub resolution (320px). Set to the current preview canvas size so
    /// L2 scrub frames fill the panel without blurry upscaling.
    pub preview_size: Option<(u32, u32)>,
    /// Monotone UI request sequence, echoed back on the VideoFrame result so
    /// ingest can reject out-of-order stale completions.
    pub req_seq: u64,
}
// Tier routing: exact (L2) and coarse (L2b) prefetch requests go through
// separate latest-wins slots (`frame_req` / `frame_req_coarse`), so the tier
// is decided at `MediaWorker::request_frame` and needs no field here. The old
// single-slot design let a coarse request fired in the same tick as an exact
// one replace it in the slot and return a frame up to 2 s from the playhead.

pub(super) enum PlaybackCmd {
    Start {
        id: Uuid,
        path: PathBuf,
        ts: f64,
        aspect: f32,
        preview_size: Option<(u32, u32)>,
        /// Playback-session generation (see `PlaybackFrame::generation`). The
        /// pb thread stamps it on every frame produced for this session so the
        /// UI can reject frames still in flight from an older session.
        gen: u64,
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
        /// Playback-session generation (see `Start::gen`).
        gen: u64,
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
