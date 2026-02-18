// crates/velocut-ui/src/context.rs
//
// AppContext owns all runtime handles that are NOT part of the serializable
// project state.  VeloCutApp holds one of these plus a ProjectState and the
// module list — nothing else.
//
// Separating context from state makes the god-object go away and lets future
// crates depend on just AppContext without pulling in egui/rodio.

use velocut_media::MediaWorker;
use crate::modules::ThumbnailCache;
use eframe::egui;
use rodio::{OutputStream, Sink};
use std::collections::HashMap;
use uuid::Uuid;

pub struct AppContext {
    // ── Media worker ─────────────────────────────────────────────────────────
    pub media_worker: MediaWorker,

    // ── Per-frame decode tracking (3-layer scrub system) ────────────────────
    /// Fine-bucket key (media_id, ¼ s bucket) of the last decode request
    pub last_frame_req: Option<(Uuid, u32)>,
    /// Coarse-bucket key (media_id, 2 s bucket) of the last background prefetch
    pub scrub_coarse_req: Option<(Uuid, u32)>,
    /// Wall-clock instant the scrub head last moved (for 150 ms debounce)
    pub scrub_last_moved: Option<std::time::Instant>,
    /// Tracks which clip the live-playback thread is currently decoding
    pub playback_media_id: Option<Uuid>,
    pub prev_playing: bool,
    pub audio_was_playing: bool,

    // ── Texture caches ───────────────────────────────────────────────────────
    /// GPU-resident clip thumbnails (library card images)
    pub thumbnail_cache: ThumbnailCache,
    /// Latest live-playback frame per media_id
    pub frame_cache: HashMap<Uuid, egui::TextureHandle>,
    /// Decoded frames keyed by (media_id, fine_bucket) — the scrub look-ahead store
    /// Cap: 128 entries; evict 32 at a time to avoid O(n) clear.
    pub frame_bucket_cache: HashMap<(Uuid, u32), egui::TextureHandle>,

    // ── Audio (rodio 0.21) ───────────────────────────────────────────────────
    pub audio_stream: Option<OutputStream>,
    pub audio_sinks: HashMap<Uuid, Sink>,
}

impl AppContext {
    pub fn new(media_worker: MediaWorker) -> Self {
        let audio_stream = rodio::OutputStreamBuilder::open_default_stream().ok();
        Self {
            media_worker,
            last_frame_req:     None,
            scrub_coarse_req:   None,
            scrub_last_moved:   None,
            playback_media_id:  None,
            prev_playing:       false,
            audio_was_playing:  false,
            thumbnail_cache:    HashMap::new(),
            frame_cache:        HashMap::new(),
            frame_bucket_cache: HashMap::new(),
            audio_stream,
            audio_sinks:        HashMap::new(),
        }
    }
}