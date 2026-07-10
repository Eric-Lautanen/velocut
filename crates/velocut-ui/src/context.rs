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

use crate::modules::ThumbnailCache;
use crate::velocut_log;
use eframe::egui;
use rodio::{MixerDeviceSink, Player};
use std::collections::HashMap;
use uuid::Uuid;
use velocut_core::media_types::PlaybackFrame;
use velocut_core::state::ProjectState;
use velocut_media::{MediaResult, MediaWorker};

// ── Memory ceiling ────────────────────────────────────────────────────────────
// Approximate byte budget for GPU-resident frame textures.
// At 640×360 RGBA each frame is ~900 KB; 192 MB ≈ 213 frames.
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

    /// Insertion-order tracking for thumbnail_cache.
    /// Used by the memory manager to evict oldest-first (not arbitrary HashMap order).
    thumbnail_order: Vec<Uuid>,

    /// Latest live-playback or scrub frame per media_id.
    /// Written by ingest_media_results (scrub) and poll_playback (playback).
    pub frame_cache: HashMap<Uuid, egui::TextureHandle>,

    /// Next-to-display playback frame, held until its PTS is due.
    /// Prevents the drain-all pattern from racing ahead of wall-clock time.
    pub pending_pb_frame: Option<PlaybackFrame>,

    /// Decoded frames keyed by (media_id, fine_bucket, is_coarse) - the scrub
    /// look-ahead store.  The `is_coarse` flag prevents L1 fine-bucket entries from
    /// colliding with L2 coarse-bucket entries when the numeric bucket values happen
    /// to overlap (e.g. fine_bucket == coarse_bucket * 8 at 2-second boundaries).
    /// Value is (texture, byte_size).
    pub frame_bucket_cache: HashMap<(Uuid, u32, bool), (egui::TextureHandle, usize)>,

    /// Approximate bytes currently held in frame_bucket_cache.
    /// Updated on insert and eviction.  Treated as an estimate (we don't track
    /// exact compressed GPU size) — uses raw RGBA bytes as a conservative ceiling.
    pub(crate) frame_cache_bytes: usize,

    /// Persistent GPU texture handles for the scrub decode path, keyed by media_id.
    ///
    /// On every scrub frame ingest we call `TextureHandle::set()` on the existing
    /// handle rather than `ctx.load_texture()`.  `set()` does an in-place GPU pixel
    /// upload with no reallocation when dimensions are unchanged - one GPU transfer
    /// per frame instead of alloc + upload + dealloc.  A new handle is allocated only
    /// on first use or when the clip's source resolution changes (e.g. switching to a
    /// clip recorded at a different camera resolution).
    ///
    /// Stores `(handle, width, height)` so a dimension change is detectable in O(1).
    pub scrub_textures: HashMap<Uuid, (egui::TextureHandle, u32, u32)>,

    /// Last time each media_id was accessed (for time-based eviction).
    pub scrub_texture_access: std::collections::HashMap<Uuid, std::time::Instant>,
}

impl CacheContext {
    pub fn new() -> Self {
        Self {
            thumbnail_cache: HashMap::new(),
            thumbnail_order: Vec::new(),
            frame_cache: HashMap::new(),
            pending_pb_frame: None,
            frame_bucket_cache: HashMap::new(),
            frame_cache_bytes: 0,
            scrub_textures: HashMap::new(),
            scrub_texture_access: std::collections::HashMap::new(),
        }
    }

    /// Insert a decoded frame into the bucket cache, evicting if over budget.
    /// `width` and `height` are the frame dimensions (for byte accounting).
    /// Returns a clone of the inserted TextureHandle for the caller's use.
    ///
    /// [Opt 4] Eviction now uses `select_nth_unstable_by_key` (O(N) partial select)
    /// instead of a full sort (O(N log N)).  Both collect the key Vec — that part
    /// stays O(N) — but the partial select avoids the full ordering pass over all
    /// 128 entries.  The difference is modest at the current cap of 128 but grows
    /// linearly if the cap is raised.
    pub fn insert_bucket_frame(
        &mut self,
        key: (Uuid, u32, bool),
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

            // [Opt 4] O(N) partial select: puts the 32 furthest entries at keys[..32]
            // without fully sorting the remaining N-32 entries.
            if keys.len() > 32 {
                keys.select_nth_unstable_by_key(32, |(_, b, _)| {
                    std::cmp::Reverse(b.abs_diff(current_bucket))
                });
            }
            keys.truncate(32);

            for k in &keys {
                // Subtract this entry's own byte count - not the incoming frame size.
                // Mixed-resolution projects (e.g. 4K + 720p) would cause the budget
                // estimate to drift if we assumed all entries are the same size.
                if let Some((_, entry_bytes)) = self.frame_bucket_cache.remove(k) {
                    self.frame_cache_bytes = self.frame_cache_bytes.saturating_sub(entry_bytes);
                }
            }
        }

        self.frame_bucket_cache
            .insert(key, (tex.clone(), frame_bytes));
        self.frame_cache_bytes += frame_bytes;
        tex
    }

    /// Evict every cached texture and reset the byte budget to zero.
    ///
    /// Called by the `ClearProject` handler before wiping `ProjectState`.
    /// Dropping TextureHandles releases the GPU allocations; clearing the
    /// byte counter prevents a stale over-budget signal on the next insert.
    pub fn clear_all(&mut self) {
        // Use .clear() rather than = HashMap::new() to retain the HashMap's heap
        // allocation - avoids a pointless dealloc+realloc on the next project open.
        // TextureHandle drops in-place, releasing GPU memory for each entry.
        self.thumbnail_cache.clear();
        self.thumbnail_order.clear();
        self.frame_cache.clear();
        self.frame_bucket_cache.clear();
        self.scrub_textures.clear();
        self.scrub_texture_access.clear();
        self.pending_pb_frame = None;
        self.frame_cache_bytes = 0;
    }

    /// Remove a thumbnail from the cache, updating insertion-order tracking.
    /// Call this instead of `thumbnail_cache.remove()` directly.
    pub fn remove_thumbnail(&mut self, id: &Uuid) {
        self.thumbnail_cache.remove(id);
        self.thumbnail_order.retain(|x| x != id);
    }

    /// Evict the oldest thumbnails so the total count does not exceed `max`.
    /// Returns the number evicted.
    pub fn evict_oldest_thumbnails(&mut self, max: usize) -> usize {
        let over = self.thumbnail_cache.len().saturating_sub(max);
        if over == 0 {
            return 0;
        }
        // Drain the oldest `over` entries from the front of the insertion-order vec.
        let to_remove: Vec<Uuid> = self.thumbnail_order.drain(..over).collect();
        for id in &to_remove {
            self.thumbnail_cache.remove(id);
        }
        to_remove.len()
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

    /// Which clip_id we last sent a PreBuffer command for.
    /// Prevents spamming PreBuffer on every tick during the look-ahead window.
    /// Reset on clip change, playback stop, or when the prebuffered clip starts.
    pub prebuffer_sent_for: Option<Uuid>,
}

impl PlaybackContext {
    fn new() -> Self {
        Self {
            last_frame_req: None,
            scrub_coarse_req: None,
            scrub_last_moved: None,
            playback_media_id: None,
            prev_playing: false,
            audio_was_playing: false,
            prebuffer_sent_for: None,
        }
    }

    /// Reset all tracking state to its initial values.
    ///
    /// Called by the `ClearProject` handler so the scrub / playback pipeline
    /// starts clean after a wipe without needing to reconstruct the struct.
    pub fn reset(&mut self) {
        self.last_frame_req = None;
        self.scrub_coarse_req = None;
        self.scrub_last_moved = None;
        self.playback_media_id = None;
        self.prev_playing = false;
        self.audio_was_playing = false;
        self.prebuffer_sent_for = None;
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

    // ── Audio (rodio 0.22) ───────────────────────────────────────────────────
    // MixerDeviceSink MUST stay alive for the entire app lifetime — dropping it
    // stops all audio.  audio_module borrows it each tick via .mixer().
    pub audio_stream: Option<MixerDeviceSink>,
    pub audio_sinks: HashMap<Uuid, Player>,
    pub audio_overlay_sinks: HashMap<Uuid, rodio::Player>,
}

impl AppContext {
    pub fn new(media_worker: MediaWorker) -> Self {
        // audio_stream is initialized lazily on the first tick() call.
        // Initializing here races with eframe/winit Win32 setup in GUI-subsystem
        // (double-click) mode — WASAPI init fails silently, leaving audio broken
        // for the entire session. By the time tick() runs, the message loop is live.
        Self {
            media_worker,
            cache: CacheContext::new(),
            playback: PlaybackContext::new(),
            audio_stream: None,
            audio_sinks: HashMap::new(),
            audio_overlay_sinks: HashMap::new(),
        }
    }

    /// Drain the MediaWorker result channel and load everything into the
    /// appropriate cache or state field.  Called once per frame from
    /// `app::poll_media`, after PTS-gated playback frame consumption.
    ///
    /// This is the single translation layer between raw `MediaWorker` output
    /// and UI-visible state — textures, waveform peaks, clip metadata, and
    /// save confirmations all land here, next to the caches they fill.
    ///
    /// [Opt 3] scrub_rx is drained first (before the shared rx) so scrub
    /// VideoFrame results are never delayed behind probe or encode traffic.
    pub fn ingest_media_results(&mut self, state: &mut ProjectState, ctx: &egui::Context) {
        // Single repaint flag — set by any result that changes visible state.
        // Calling ctx.request_repaint() once at the end is identical in effect
        // to calling it N times mid-loop (egui deduplicates), but avoids the
        // repeated Arc-lock overhead of N individual calls when draining a busy
        // channel (e.g. bulk import of 20 clips on startup).
        let mut needs_repaint = false;

        // ── [Opt 3] Scrub frames — high-priority path ─────────────────────────
        // Drain the dedicated scrub channel before the shared channel.
        // The scrub thread sends at most one result per condvar wake, so this
        // loop typically executes 0 or 1 iterations per frame.
        while let Ok(result) = self.media_worker.scrub_rx.try_recv() {
            match result {
                // Single-clip scrub frame (regular scrub thread)
                MediaResult::VideoFrame {
                    id,
                    width,
                    height,
                    data,
                } => {
                    self.ingest_video_frame(
                        id,
                        width,
                        height,
                        data,
                        state,
                        &mut needs_repaint,
                        ctx,
                    );
                }
                // Blended transition frame (transition scrub thread)
                MediaResult::TransitionVideoFrame {
                    id,
                    width,
                    height,
                    data,
                } => {
                    self.ingest_video_frame(
                        id,
                        width,
                        height,
                        data,
                        state,
                        &mut needs_repaint,
                        ctx,
                    );
                }
                // Other variants never arrive on scrub_rx - ignore them.
                _ => {}
            }
        }

        // ── Shared channel: probes, waveforms, audio, encode, HQ frames ───────
        while let Ok(result) = self.media_worker.rx.try_recv() {
            match result {
                MediaResult::AudioPath {
                    id,
                    path,
                    trimmed_offset,
                } => {
                    velocut_log!("[audio] AudioPath arrived id={id} path={} trimmed_offset={trimmed_offset:.3}", path.display());
                    state.set_audio_path(id, path, trimmed_offset);
                }

                MediaResult::Duration { id, seconds } => {
                    state.update_clip_duration(id, seconds);
                    needs_repaint = true;
                }

                MediaResult::Thumbnail {
                    id,
                    width,
                    height,
                    data,
                } => {
                    let tex = ctx.load_texture(
                        format!("thumb-{id}"),
                        egui::ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &data,
                        ),
                        egui::TextureOptions::LINEAR,
                    );
                    self.cache.thumbnail_cache.insert(id, tex.clone());
                    // Track insertion order for oldest-first eviction.
                    // Remove any previous entry for this id (re-insertion moves to end).
                    self.cache.thumbnail_order.retain(|x| *x != id);
                    self.cache.thumbnail_order.push(id);
                    needs_repaint = true;
                }

                MediaResult::Waveform { id, peaks } => {
                    state.update_waveform(id, peaks);
                    needs_repaint = true;
                }

                MediaResult::VideoSize { id, width, height } => {
                    if let Some(clip) = state.library.iter_mut().find(|c| c.id == id) {
                        clip.video_size = Some((width, height));
                    }
                    needs_repaint = true;
                }

                MediaResult::FrameSaved { path } => {
                    velocut_log!("[app] frame PNG saved → {:?}", path);
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "frame".into());
                    state.save_status = Some(format!("✓ Saved: {}", name));
                    needs_repaint = true;
                }

                // VideoFrame on the shared channel = HQ one-shot frame from
                // extract_frame_hq.  Scrub frames no longer arrive here —
                // they travel on scrub_rx and are consumed above.
                MediaResult::VideoFrame {
                    id,
                    width,
                    height,
                    data,
                } => {
                    self.ingest_video_frame(
                        id,
                        width,
                        height,
                        data,
                        state,
                        &mut needs_repaint,
                        ctx,
                    );
                }

                MediaResult::Error { id, msg } => {
                    velocut_log!("[media] {id}: {msg}");
                }

                // ── Encode results ────────────────────────────────────────────
                // All three arms guard on `state.encode_job == Some(job_id)` so a
                // stale result from a previously cancelled job never clobbers a
                // freshly started one.
                MediaResult::EncodeProgress {
                    job_id,
                    frame,
                    total_frames,
                } => {
                    if state.encode_job == Some(job_id) {
                        state.encode_progress = Some((frame, total_frames));
                        needs_repaint = true;
                    }
                }

                MediaResult::EncodeDone { job_id, path } => {
                    if state.encode_job == Some(job_id) {
                        if let Some((_, total)) = state.encode_progress {
                            state.encode_progress = Some((total, total));
                        }
                        state.encode_done = Some(path);
                        needs_repaint = true;
                    }
                }

                MediaResult::EncodeError { job_id, msg } => {
                    if state.encode_job == Some(job_id) {
                        state.encode_error = Some(msg);
                        needs_repaint = true;
                    }
                }
                // TransitionVideoFrame only arrives on scrub_rx, not the shared channel.
                MediaResult::TransitionVideoFrame { .. } => {}
            }
        }

        if needs_repaint {
            ctx.request_repaint();
        }
    }

    /// Shared logic for handling a VideoFrame result from either channel.
    ///
    /// Factored out so both the scrub_rx fast path and the shared rx path
    /// (HQ frames from extract_frame_hq) go through identical bucket-cache
    /// and frame_cache logic without duplication.
    #[allow(clippy::too_many_arguments)]
    fn ingest_video_frame(
        &mut self,
        id: Uuid,
        width: u32,
        height: u32,
        mut data: Vec<u8>,
        state: &mut ProjectState,
        needs_repaint: &mut bool,
        ctx: &egui::Context,
    ) {
        {
            let active_filter = state
                .timeline
                .iter()
                .find(|c| {
                    c.track_row % 2 == 0
                        && state.current_time >= c.start_time
                        && state.current_time < c.start_time + c.duration
                })
                .map(|c| c.filter.clone())
                .unwrap_or_default();

            if !active_filter.is_identity() {
                use velocut_core::filters::helpers::apply_filter_rgba;
                apply_filter_rgba(&mut data, &active_filter);
            }
        }
        // ── Build ColorImage ─────────────────────────────────────────────────────
        // `data` is tight RGBA bytes from the decode pipeline.  We use the safe
        // `ColorImage::from_rgba_unmultiplied` which copies the data internally.
        // The previous zero-copy reinterpret (Vec<u8> → Vec<Color32> via unsafe)
        // was correct at the time (Color32 is repr(C) with alignment 1), but is
        // fragile against future egui layout changes.  The extra copy is negligible
        // on the GPU-upload path which already allocates and copies for the texture.
        let image =
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &data);

        // ── Persistent texture reuse ──────────────────────────────────────────
        // Reuse the existing GPU texture handle for this clip when dimensions
        // are unchanged.  TextureHandle::set() performs an in-place GPU pixel
        // upload (no realloc, no driver-level dealloc/alloc round-trip).
        //
        // A new handle is allocated only on:
        //   a) First frame for this clip (cold path, O(1) HashMap miss).
        //   b) Source resolution change - e.g. switching from a 4K clip to a
        //      720p clip that happens to share the same media_id (pathological but
        //      handled correctly: the old handle is replaced and the GPU memory
        //      freed when the Arc refcount in frame_bucket_cache reaches zero).
        let tex = match self.cache.scrub_textures.get_mut(&id) {
            Some((handle, w, h)) if *w == width && *h == height => {
                // Hot path: in-place GPU upload, zero CPU allocations.
                handle.set(image, egui::TextureOptions::LINEAR);
                handle.clone()
            }
            _ => {
                // Cold path: first frame or resolution change - full GPU alloc.
                let handle =
                    ctx.load_texture(format!("scrub-{id}"), image, egui::TextureOptions::LINEAR);
                self.cache
                    .scrub_textures
                    .insert(id, (handle.clone(), width, height));
                handle
            }
        };

        // Update access time for time-based eviction.
        self.cache
            .scrub_texture_access
            .insert(id, std::time::Instant::now());

        // Evict old scrub textures that haven't been accessed in 30 seconds.
        // This prevents unbounded memory growth when scrubbing many clips.
        let now = std::time::Instant::now();
        let old_ids: Vec<Uuid> = self
            .cache
            .scrub_texture_access
            .iter()
            .filter(|(_, t)| now.duration_since(**t) > std::time::Duration::from_secs(30))
            .map(|(id, _)| *id)
            .collect();
        for old_id in old_ids {
            self.cache.scrub_textures.remove(&old_id);
            self.cache.scrub_texture_access.remove(&old_id);
        }

        // Derive the ¼s bucket key for frame_bucket_cache.
        // playback.last_frame_req stores exact f64 ts - convert here.
        let (bucket, is_coarse) = self
            .playback
            .last_frame_req
            .filter(|(rid, _)| *rid == id)
            .map(|(_, ts)| ((ts * 4.0) as u32, false))
            .or_else(|| {
                self.playback
                    .scrub_coarse_req
                    .filter(|(rid, _)| *rid == id)
                    .map(|(_, cb)| (cb * 8, true))
            })
            .unwrap_or_else(|| {
                state
                    .timeline
                    .iter()
                    .find(|c| c.media_id == id)
                    .map(|c| {
                        // Match the local_t formula in video_module::tick():
                        // source-relative time = timeline time + source_offset.
                        let lt = (state.current_time - c.start_time + c.source_offset).max(0.0);
                        ((lt * 4.0) as u32, false)
                    })
                    .unwrap_or((0, false))
            });

        let tex = self.cache.insert_bucket_frame(
            (id, bucket, is_coarse),
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
            *needs_repaint = true;
        }
    }
}
