// crates/velocut-ui/src/context.rs
//
// AppContext owns all runtime handles that are NOT part of the serializable
// project state.  VeloCutApp holds one of these plus a ProjectState and the
// module list — nothing else.
//
// Sub-struct layout:
//   AppContext
//     ├── media_worker        — the FFmpeg worker + all channel handles
//     ├── cache: CacheContext — all GPU texture caches with a memory ceiling
//     ├── playback: PlaybackContext — scrub/playback decode tracking
//     ├── audio_stream        — rodio OutputStream (must outlive all sinks)
//     └── audio_sinks         — per-clip Sink map (managed by audio_module only)

use velocut_media::{MediaWorker, MediaResult};
use velocut_core::media_types::PlaybackFrame;
use velocut_core::state::{ProjectState, AspectRatio};
use crate::modules::ThumbnailCache;
use eframe::egui;
use rodio::{OutputStream, Sink};
use std::collections::HashMap;
use uuid::Uuid;

// ── Memory ceiling ────────────────────────────────────────────────────────────
// Approximate byte budget for GPU-resident frame textures.
// At 640×360 RGBA each frame is ~900 KB; 128 frames ≈ 115 MB.
// This constant is checked on every frame_bucket_cache insert.
// Change it here to tune the scrub cache size without touching eviction logic.
const MAX_FRAME_CACHE_BYTES: usize = 192 * 1024 * 1024; // 192 MB

// ── CacheContext ──────────────────────────────────────────────────────────────
// Owns all GPU-resident texture caches and the memory ceiling that governs them.
// Nothing outside AppContext should own or evict textures — route all cache
// writes through CacheContext methods so the budget stays accurate.
pub struct CacheContext {
    /// GPU-resident clip thumbnails (library card images).
    /// Never evicted today — see Optimization Opportunities §thumbnail eviction.
    pub thumbnail_cache: ThumbnailCache,

    /// Latest live-playback or scrub frame per media_id.
    /// Written by ingest_media_results (scrub) and poll_playback (playback).
    pub frame_cache: HashMap<Uuid, egui::TextureHandle>,

    /// Next-to-display playback frame, held until its PTS is due.
    /// Prevents the drain-all pattern from racing ahead of wall-clock time.
    pub pending_pb_frame: Option<PlaybackFrame>,

    /// Decoded frames keyed by (media_id, fine_bucket) — the scrub look-ahead store.
    /// Eviction: when byte estimate exceeds MAX_FRAME_CACHE_BYTES, evict the 32
    /// entries furthest from the current playhead (not random, not LRU).
    pub frame_bucket_cache: HashMap<(Uuid, u32), egui::TextureHandle>,

    /// Approximate bytes currently held in frame_bucket_cache.
    /// Updated on insert and eviction.  Treated as an estimate (we don't track
    /// exact compressed GPU size) — uses raw RGBA bytes as a conservative ceiling.
    frame_cache_bytes: usize,
}

impl CacheContext {
    fn new() -> Self {
        Self {
            thumbnail_cache:    HashMap::new(),
            frame_cache:        HashMap::new(),
            pending_pb_frame:   None,
            frame_bucket_cache: HashMap::new(),
            frame_cache_bytes:  0,
        }
    }

    /// Insert a decoded frame into the bucket cache, evicting if over budget.
    /// `width` and `height` are the frame dimensions (for byte accounting).
    /// Returns a clone of the inserted TextureHandle for the caller's use.
    pub fn insert_bucket_frame(
        &mut self,
        key: (Uuid, u32),
        tex: egui::TextureHandle,
        width: usize,
        height: usize,
        current_time: f64,
    ) -> egui::TextureHandle {
        let frame_bytes = width * height * 4;

        // Evict if this insert would exceed the budget.
        if self.frame_cache_bytes + frame_bytes > MAX_FRAME_CACHE_BYTES {
            let current_bucket = (current_time * 4.0) as u32;
            let mut keys: Vec<_> = self.frame_bucket_cache.keys().copied().collect();
            // Sort furthest-first so we truncate the tail.
            keys.sort_by_key(|(_, b)| std::cmp::Reverse(b.abs_diff(current_bucket)));
            keys.truncate(32);
            for k in &keys {
                // Subtract evicted bytes. Approximate: assume all frames same size.
                self.frame_cache_bytes =
                    self.frame_cache_bytes.saturating_sub(frame_bytes);
                self.frame_bucket_cache.remove(k);
            }
        }

        self.frame_bucket_cache.insert(key, tex.clone());
        self.frame_cache_bytes += frame_bytes;
        tex
    }
}

// ── PlaybackContext ───────────────────────────────────────────────────────────
// Owns all decode-tracking state for the 3-layer scrub system and the
// playback pipeline.  Isolated here so video_module.rs has a clear home for
// the state it mutates, and it never accidentally touches cache or audio.
pub struct PlaybackContext {
    /// Exact (media_id, timestamp_secs) of the last scrub decode request.
    /// Stored as exact f64 (not a ¼s bucket) so scrub fires on every drag pixel.
    pub last_frame_req: Option<(Uuid, f64)>,

    /// Coarse-bucket key (media_id, 2 s bucket) of the last background prefetch.
    pub scrub_coarse_req: Option<(Uuid, u32)>,

    /// Wall-clock instant the scrub head last moved (for 150 ms L3 debounce).
    pub scrub_last_moved: Option<std::time::Instant>,

    /// Which clip the live-playback thread is currently decoding.
    pub playback_media_id: Option<Uuid>,

    /// Was is_playing true on the previous frame?  Used to detect play/stop edges.
    pub prev_playing: bool,

    /// Was audio running before the last scrub or seek?
    /// Lets audio_module restart the sink after a scrub without double-starting.
    pub audio_was_playing: bool,
}

impl PlaybackContext {
    fn new() -> Self {
        Self {
            last_frame_req:    None,
            scrub_coarse_req:  None,
            scrub_last_moved:  None,
            playback_media_id: None,
            prev_playing:      false,
            audio_was_playing: false,
        }
    }
}

// ── AppContext ────────────────────────────────────────────────────────────────

pub struct AppContext {
    // ── Media worker ─────────────────────────────────────────────────────────
    pub media_worker: MediaWorker,

    // ── Texture caches (with memory budget) ──────────────────────────────────
    pub cache: CacheContext,

    // ── Scrub / playback decode tracking ─────────────────────────────────────
    pub playback: PlaybackContext,

    // ── Audio (rodio 0.21) ───────────────────────────────────────────────────
    // OutputStream MUST stay alive for the entire app lifetime — dropping it
    // stops all audio.  audio_module borrows it each tick via .mixer().
    pub audio_stream: Option<OutputStream>,
    pub audio_sinks:  HashMap<Uuid, Sink>,
}

impl AppContext {
    pub fn new(media_worker: MediaWorker) -> Self {
        let audio_stream = rodio::OutputStreamBuilder::open_default_stream()
            .map_err(|e| eprintln!("[audio] stream init failed: {e}"))
            .ok();
        eprintln!("[audio] stream ready: {}", audio_stream.is_some());
        Self {
            media_worker,
            cache:        CacheContext::new(),
            playback:     PlaybackContext::new(),
            audio_stream,
            audio_sinks:  HashMap::new(),
        }
    }

    /// Drain the MediaWorker result channel and load everything into the
    /// appropriate cache or state field.  Called once per frame from
    /// `app::poll_media`, after PTS-gated playback frame consumption.
    ///
    /// This is the single translation layer between raw `MediaWorker` output
    /// and UI-visible state — textures, waveform peaks, clip metadata, and
    /// save confirmations all land here, next to the caches they fill.
    pub fn ingest_media_results(
        &mut self,
        state: &mut ProjectState,
        ctx:   &egui::Context,
    ) {
        while let Ok(result) = self.media_worker.rx.try_recv() {
            match result {
                MediaResult::AudioPath { id, path } => {
                    eprintln!("[audio] AudioPath arrived id={id} path={}", path.display());
                    if let Some(clip) = state.library.iter_mut().find(|c| c.id == id) {
                        clip.audio_path = Some(path);
                    }
                }

                MediaResult::Duration { id, seconds } => {
                    state.update_clip_duration(id, seconds);
                    ctx.request_repaint();
                }

                MediaResult::Thumbnail { id, width, height, data } => {
                    let tex = ctx.load_texture(
                        format!("thumb-{id}"),
                        egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize], &data,
                        ),
                        egui::TextureOptions::LINEAR,
                    );
                    self.cache.thumbnail_cache.insert(id, tex);
                    ctx.request_repaint();
                }

                MediaResult::Waveform { id, peaks } => {
                    state.update_waveform(id, peaks);
                    ctx.request_repaint();
                }

                MediaResult::VideoSize { id, width, height } => {
                    if let Some(clip) = state.library.iter_mut().find(|c| c.id == id) {
                        clip.video_size = Some((width, height));
                    }
                    // Auto-set aspect ratio from the first clip that reports a size.
                    let is_first = state.library.iter()
                        .filter(|c| c.video_size.is_some()).count() == 1;
                    if is_first && width > 0 && height > 0 {
                        let r = width as f32 / height as f32;
                        state.aspect_ratio =
                            if      (r - 16.0/9.0 ).abs() < 0.05 { AspectRatio::SixteenNine   }
                            else if (r - 9.0/16.0 ).abs() < 0.05 { AspectRatio::NineSixteen   }
                            else if (r - 2.0/3.0  ).abs() < 0.05 { AspectRatio::TwoThree      }
                            else if (r - 3.0/2.0  ).abs() < 0.05 { AspectRatio::ThreeTwo      }
                            else if (r - 4.0/3.0  ).abs() < 0.05 { AspectRatio::FourThree     }
                            else if (r - 1.0      ).abs() < 0.05 { AspectRatio::OneOne        }
                            else if (r - 4.0/5.0  ).abs() < 0.05 { AspectRatio::FourFive      }
                            else if (r - 21.0/9.0 ).abs() < 0.10 { AspectRatio::TwentyOneNine }
                            else if (r - 2.39     ).abs() < 0.05 { AspectRatio::Anamorphic    }
                            else if r > 1.0 { AspectRatio::SixteenNine }
                            else            { AspectRatio::NineSixteen };
                        eprintln!("[app] aspect ratio auto-set from {width}x{height}");
                        ctx.request_repaint();
                    }
                }

                MediaResult::FrameSaved { path } => {
                    eprintln!("[app] frame PNG saved → {:?}", path);
                    let name = path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "frame".into());
                    state.save_status = Some(format!("✓ Saved: {}", name));
                    ctx.request_repaint();
                }

                MediaResult::VideoFrame { id, width, height, data } => {
                    let tex = ctx.load_texture(
                        format!("frame-{id}"),
                        egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize], &data,
                        ),
                        egui::TextureOptions::LINEAR,
                    );

                    // Derive the ¼s bucket key for frame_bucket_cache.
                    // playback.last_frame_req stores exact f64 ts — convert here.
                    let bucket: u32 = self.playback.last_frame_req
                        .filter(|(rid, _)| *rid == id)
                        .map(|(_, ts)| (ts * 4.0) as u32)
                        .or_else(|| self.playback.scrub_coarse_req
                            .filter(|(rid, _)| *rid == id)
                            .map(|(_, cb)| cb * 8))
                        .unwrap_or_else(|| {
                            state.timeline.iter()
                                .find(|c| c.media_id == id)
                                .map(|c| {
                                    let lt = (state.current_time - c.start_time).max(0.0);
                                    (lt * 4.0) as u32
                                })
                                .unwrap_or(0)
                        });

                    let tex = self.cache.insert_bucket_frame(
                        (id, bucket),
                        tex,
                        width as usize,
                        height as usize,
                        state.current_time,
                    );

                    // During playback the pb channel owns frame_cache — a late-arriving
                    // scrub result would overwrite the correct playback frame with a
                    // wrong-position one.  Skip the frame_cache write while playing.
                    if !state.is_playing {
                        self.cache.frame_cache.insert(id, tex);
                        ctx.request_repaint();
                    }
                }

                MediaResult::Error { id, msg } => {
                    eprintln!("[media] {id}: {msg}");
                }

                // ── Encode results ────────────────────────────────────────────
                // All three arms guard on `state.encode_job == Some(job_id)` so a
                // stale result from a previously cancelled job never clobbers a
                // freshly started one.

                MediaResult::EncodeProgress { job_id, frame, total_frames } => {
                    if state.encode_job == Some(job_id) {
                        state.encode_progress = Some((frame, total_frames));
                        ctx.request_repaint();
                    }
                }

                MediaResult::EncodeDone { job_id, path } => {
                    if state.encode_job == Some(job_id) {
                        if let Some((_, total)) = state.encode_progress {
                            state.encode_progress = Some((total, total));
                        }
                        state.encode_done = Some(path);
                        ctx.request_repaint();
                    }
                }

                MediaResult::EncodeError { job_id, msg } => {
                    if state.encode_job == Some(job_id) {
                        state.encode_error = Some(msg);
                        ctx.request_repaint();
                    }
                }
            }
        }
    }
}