// crates/velocut-ui/src/context.rs
//
// AppContext owns all runtime handles that are NOT part of the serializable
// project state.  VeloCutApp holds one of these plus a ProjectState and the
// module list — nothing else.

use velocut_media::{MediaWorker, MediaResult};
use velocut_core::media_types::PlaybackFrame;
use velocut_core::state::{ProjectState, AspectRatio};
use crate::modules::ThumbnailCache;
use eframe::egui;
use rodio::{OutputStream, Sink};
use std::collections::HashMap;
use uuid::Uuid;

pub struct AppContext {
    // ── Media worker ─────────────────────────────────────────────────────────
    pub media_worker: MediaWorker,

    // ── Per-frame decode tracking (3-layer scrub system) ────────────────────
    /// Exact (media_id, timestamp_secs) of the last scrub decode request.
    /// Storing exact secs (not a ¼s bucket) so scrub_moved fires on every pixel of drag.
    pub last_frame_req: Option<(Uuid, f64)>,
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
    /// Next-to-display playback frame, held until its PTS is due.
    /// Prevents the drain-all pattern from racing ahead of wall-clock time.
    pub pending_pb_frame: Option<PlaybackFrame>,
    /// Decoded frames keyed by (media_id, fine_bucket) — the scrub look-ahead store
    /// Cap: 128 entries; evict 32 at a time to avoid O(n) clear.
    pub frame_bucket_cache: HashMap<(Uuid, u32), egui::TextureHandle>,

    // ── Audio (rodio 0.21) ───────────────────────────────────────────────────
    // OutputStream MUST stay alive for the entire app lifetime — dropping it
    // stops all audio. audio_module borrows it each tick via .mixer().
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
            last_frame_req:     None,
            scrub_coarse_req:   None,
            scrub_last_moved:   None,
            playback_media_id:  None,
            prev_playing:       false,
            audio_was_playing:  false,
            thumbnail_cache:    HashMap::new(),
            frame_cache:        HashMap::new(),
            pending_pb_frame:   None,
            frame_bucket_cache: HashMap::new(),
            audio_stream,
            audio_sinks:        HashMap::new(),
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
                    self.thumbnail_cache.insert(id, tex);
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
                    // The count includes the clip that just updated, so == 1 fires exactly once.
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
                    // last_frame_req stores exact f64 ts (not a bucket) — convert here.
                    let bucket: u32 = self.last_frame_req
                        .filter(|(rid, _)| *rid == id)
                        .map(|(_, ts)| (ts * 4.0) as u32)
                        .or_else(|| self.scrub_coarse_req
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

                    // Evict the 32 entries furthest from the current playhead when
                    // the cache hits its 128-entry cap. Random HashMap iteration order
                    // (the previous approach) could evict the frames nearest the scrub
                    // head, which are exactly the ones about to be needed.
                    if self.frame_bucket_cache.len() >= 128 {
                        let current_bucket = (state.current_time * 4.0) as u32;
                        let mut keys: Vec<_> = self.frame_bucket_cache.keys().copied().collect();
                        keys.sort_by_key(|(_, b)| std::cmp::Reverse(b.abs_diff(current_bucket)));
                        keys.truncate(32);
                        for k in keys { self.frame_bucket_cache.remove(&k); }
                    }
                    self.frame_bucket_cache.insert((id, bucket), tex.clone());

                    // During playback the pb channel owns frame_cache; a late-arriving
                    // scrub result would overwrite the correct playback frame with a
                    // wrong-position one.  Skip the frame_cache write while playing.
                    if !state.is_playing {
                        self.frame_cache.insert(id, tex);
                        ctx.request_repaint();
                    }
                }

                MediaResult::Error { id, msg } => {
                    eprintln!("[media] {id}: {msg}");
                }
            }
        }
    }
}