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

use crate::helpers::clip_query;
use crate::modules::ThumbnailCache;
use crate::velocut_log;
use eframe::egui;
use rodio::{MixerDeviceSink, Player};
use std::collections::{HashMap, HashSet};
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

    /// Persistent GPU texture handles for the playback path, keyed by media_id.
    ///
    /// Same contract as `scrub_textures`, and deliberately a separate map: the
    /// `frame_cache` entry for a clip is usually a clone of a scrub or thumbnail
    /// texture, and `TextureHandle::set()` writes through the shared Arc — mutating
    /// one of those would overwrite the pixels `frame_bucket_cache` and
    /// `thumbnail_cache` still point at. Owning the handle here means every `set`
    /// only ever touches pixels the playback path produced itself.
    ///
    /// Stores `(handle, width, height)` so a dimension change is detectable in O(1)
    /// and falls back to a fresh allocation, exactly like the scrub path.
    pub pb_textures: HashMap<Uuid, (egui::TextureHandle, u32, u32)>,
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
            pb_textures: HashMap::new(),
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
        self.pb_textures.clear();
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

    /// Counter handed out as `req_seq` on every scrub-path request (L2 exact,
    /// L2b coarse, L2/HQ transition, L3 HQ). Monotone within the session.
    pub scrub_req_seq: u64,

    /// Highest `req_seq` accepted into the display path so far. A scrub or
    /// transition result whose sequence is older arrived out of order from one
    /// of the concurrent decode executors — e.g. a slow L2 blend finishing
    /// after the idle-triggered L3 HQ blend — and must never overwrite the
    /// newer frame already shown.
    pub scrub_seq_watermark: u64,

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
            scrub_req_seq: 0,
            scrub_seq_watermark: 0,
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
        // The sequence counter must NOT reset here: in-flight results from the
        // old project still carry seq numbers below the running counter, and a
        // reset could let them through the watermark gate.
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

    /// Library ids whose WAV temp file was (re-)written by an accepted
    /// `MediaResult::AudioPath`. Written by `ingest_media_results`, consumed
    /// and cleared by the AudioModule each tick: every rodio sink belongs to a
    /// session of that WAV file — the old extraction — and the re-extraction
    /// rewrote the file under the open decoder, so live sinks must be drained
    /// and their tracking state cleared so they rebuild from the new content.
    /// Keyed by library id; AudioModule resolves the timeline clips through
    /// `TimelineClip::media_id`.
    pub pending_audio_wav_rebuild: HashSet<Uuid>,
}

/// A decoded video frame routed to `AppContext::ingest_video_frame`.
///
/// `id` is the media id the decoder was asked for, `req_ts` the source-file
/// timestamp of that request, and `coarse`/`transition` classify the request
/// tier so ingest can store the result in the right lane (see the fn docs).
struct VideoFrameResult {
    id: Uuid,
    req_ts: f64,
    /// Echoed UI request sequence (`MediaResult` field `req_seq`). 0 means
    /// untracked and bypasses the watermark gate.
    seq: u64,
    /// Echoed blend factor (transition results only; 0.0 otherwise). Compared
    /// against the active zone's alpha in the transition freshness gate.
    alpha: f32,
    coarse: bool,
    transition: bool,
    width: u32,
    height: u32,
    data: Vec<u8>,
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
            pending_audio_wav_rebuild: HashSet::new(),
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
                // Single-clip scrub frame (exact or coarse scrub thread)
                MediaResult::VideoFrame {
                    id,
                    req_ts,
                    req_seq,
                    width,
                    height,
                    data,
                } => {
                    self.ingest_video_frame(
                        VideoFrameResult {
                            id,
                            req_ts,
                            seq: req_seq,
                            alpha: 0.0,
                            coarse: false,
                            transition: false,
                            width,
                            height,
                            data,
                        },
                        state,
                        &mut needs_repaint,
                        ctx,
                    );
                }
                // Blended transition frame (transition scrub thread)
                MediaResult::TransitionVideoFrame {
                    id,
                    req_ts,
                    req_seq,
                    alpha,
                    width,
                    height,
                    data,
                } => {
                    self.ingest_video_frame(
                        VideoFrameResult {
                            id,
                            req_ts,
                            seq: req_seq,
                            alpha,
                            coarse: false,
                            transition: true,
                            width,
                            height,
                            data,
                        },
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
                    generation,
                } => {
                    // Session-generation gate: see `audio_path_is_current`.
                    if !audio_path_is_current(&self.media_worker, id, generation) {
                        let expected = self.media_worker.audio_generation_of(id);
                        velocut_log!(
                            "[audio] dropping stale AudioPath id={id} gen={generation} expected={expected} path={}",
                            path.display()
                        );
                    } else {
                        velocut_log!("[audio] AudioPath arrived id={id} path={} trimmed_offset={trimmed_offset:.3}", path.display());
                        state.set_audio_path(id, path, trimmed_offset);
                        // The WAV file was rewritten on disk. Every live sink
                        // is a session of the OLD extraction — drain it and
                        // clear its tracking so AudioModule rebuilds from the
                        // new content on the next tick.
                        self.pending_audio_wav_rebuild.insert(id);
                        needs_repaint = true;
                    }
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
                // request_frame_hq / extract_frame_hq.  Scrub frames travel on
                // scrub_rx and are consumed above.
                MediaResult::VideoFrame {
                    id,
                    req_ts,
                    req_seq,
                    width,
                    height,
                    data,
                } => {
                    self.ingest_video_frame(
                        VideoFrameResult {
                            id,
                            req_ts,
                            seq: req_seq,
                            alpha: 0.0,
                            coarse: false,
                            transition: false,
                            width,
                            height,
                            data,
                        },
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

    /// Shared logic for handling a video-frame result from either channel.
    ///
    /// Factored out so the scrub_rx fast path and the shared rx path (HQ
    /// one-shot frames) go through identical bucket-cache and frame_cache
    /// logic without duplication.
    ///
    /// `req_ts` is the source timestamp the decoder was requested for, `coarse`
    /// marks an L2b look-ahead prefetch, `transition` marks a blended zone
    /// frame. All three attributes let the ingest reject results that belong
    /// to a position the playhead has already left — without this gate, a
    /// slow or out-of-order decode overwrites the live frame with a stale one
    /// and the frame then persists in the caches.
    fn ingest_video_frame(
        &mut self,
        frame: VideoFrameResult,
        state: &mut ProjectState,
        needs_repaint: &mut bool,
        ctx: &egui::Context,
    ) {
        let VideoFrameResult {
            id,
            req_ts,
            seq,
            alpha,
            coarse,
            transition,
            width,
            height,
            data,
        } = frame;
        let mut data = data;
        // ── Freshness gate ───────────────────────────────────────────────────
        // Animate only results whose request matches the current playhead.
        // Blend frames must align with the zone actually under the playhead;
        // single-clip frames must align with the video clip covering it.
        if transition {
            let fresh = clip_query::active_transition_at(state).is_some_and(|z| {
                (z.clip_a_source_ts - req_ts).abs() <= 0.5
                    // Two results from different moments of the same zone (or
                    // a neighbouring zone sharing the same clip pair) cannot be
                    // told apart by timestamp alone — clip_a_source_ts is a
                    // per-zone-pair constant. A wrong-alpha blend replacing a
                    // correct one is exactly the preview-flush class the seq
                    // watermark fixed for executor races, so the blend factor
                    // must also still match the playhead's zone position.
                    && transition_alpha_fresh(z.alpha, alpha)
            });
            if !fresh {
                return;
            }
        } else if coarse {
            // Coarse prefetches are only meaningful while the coarse request
            // they belong to is still the latest one for this media id.
            let fresh = self
                .playback
                .scrub_coarse_req
                .is_some_and(|(rid, cb)| rid == id && (cb as f64 * 2.0 - req_ts).abs() < 0.01);
            if !fresh {
                return;
            }
        } else {
            let fresh =
                clip_query::video_clip_at_time(state, state.current_time).is_some_and(|c| {
                    c.media_id == id
                        && (state.current_time - c.start_time + c.source_offset - req_ts).abs()
                            <= 0.5
                });
            if !fresh {
                return;
            }
        }
        // ── Out-of-order completion gate (sequence watermark) ─────────────────
        // The scrub executors run CONCURRENTLY (persistent L2/L2b slot threads,
        // one-shot HQ threads, the persistent transition thread all feed the
        // same result channel), so completions can arrive out of request order.
        // A result older than what has already been accepted must never replace
        // a newer frame: a slow L2 transition blend finishing after the
        // idle-triggered L3 HQ blend would otherwise downgrade the displayed
        // frame from native-res to panel-res. `seq == 0` marks an untracked
        // (legacy/PNG-path) sender and bypasses the gate. Coarse prefetches are
        // excluded — they are never displayed and only warm the coarse bucket.
        if !coarse && seq != 0 {
            if !seq_accepts(self.playback.scrub_seq_watermark, seq) {
                return;
            }
            self.playback.scrub_seq_watermark = seq;
        }
        {
            let active_filter = clip_query::video_clip_at_time(state, state.current_time)
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

        // Bucket key comes from the REQUEST's own timestamp — the request is
        // the single source of truth, so a result can never be mis-bucketed
        // under a later request's position (the old last_frame_req-derived key
        // stored coarse frames in fine buckets when both tiers were in flight).
        let bucket = (req_ts * 4.0) as u32;

        let tex = self.cache.insert_bucket_frame(
            (id, bucket, coarse),
            tex,
            width as usize,
            height as usize,
            state.current_time,
        );

        // Coarse prefetch frames are warm-up only — they must never become the
        // displayed frame. During playback the pb channel owns frame_cache —
        // a late-arriving scrub result would overwrite the correct playback
        // frame with a wrong-position one. Skip the write while playing.
        if !coarse && !state.is_playing {
            self.cache.frame_cache.insert(id, tex);
            *needs_repaint = true;
        }
    }
}

/// Body of the `MediaResult::AudioPath` session-generation gate, split out so
/// tests can pin the superseded-extraction cases without a full ingest
/// round-trip — the ingest-side analog of the video path's `recv_session_frame`.
///
/// WAV-extraction requests for the same library id supersede each other and
/// the two WAVs can carry different content windows, while the accepted
/// result's `trimmed_offset` drives the `wav_seek_t` seek math. Accepting a
/// result whose request has since been superseded would make the seek math
/// describe a window the current WAV does not hold.
fn audio_path_is_current(worker: &MediaWorker, id: Uuid, generation: u64) -> bool {
    worker.audio_generation_of(id) == generation
}

/// Monotone accept rule for out-of-order decode completions.
///
/// `watermark` is the highest accepted sequence so far. `seq == 0` marks an
/// untracked (legacy, PNG-path) sender and is always accepted.
fn seq_accepts(watermark: u64, seq: u64) -> bool {
    seq == 0 || seq >= watermark
}

/// Blend-factor fitness of a transition result against the active zone.
///
/// The seq watermark already rejects out-of-order completions, so this rule
/// only has to catch the remaining staleness class: a result whose alpha no
/// longer matches the playhead's position in the zone (a fast scrub that got
/// a far-away-alpha blend as the last accepted one). Fine-grained mismatches
/// (a few percent) are kept — no better frame is in flight and rejecting
/// them would just freeze the preview on the older blend.
const TRANSITION_ALPHA_GAP: f32 = 0.05;

fn transition_alpha_fresh(zone_alpha: f32, result_alpha: f32) -> bool {
    (zone_alpha - result_alpha).abs() <= TRANSITION_ALPHA_GAP
}

#[cfg(test)]
mod watermark_tests {
    use super::seq_accepts;

    #[test]
    fn slow_l2_blend_cannot_downgrade_a_accepted_l3_hq_result() {
        // The exact scenario from the Area A audit: L3 HQ blend (seq 8) is
        // accepted, then a slow L2 blend (seq 7) finishes and must be rejected.
        let mut watermark = 0u64;
        for seq in [3u64, 5, 8] {
            assert!(seq_accepts(watermark, seq), "seq {seq} must be accepted");
            watermark = watermark.max(seq);
        }
        assert!(
            !seq_accepts(watermark, 7),
            "seq 7 arrives after seq 8 → reject"
        );
        // A fresh request (seq 9) is still accepted afterwards.
        assert!(seq_accepts(watermark, 9));
    }

    #[test]
    fn in_order_results_are_all_accepted() {
        for seq in [1u64, 2, 3, 4] {
            assert!(seq_accepts(seq - 1, seq), "watermark {seq}-1, seq {seq}");
        }
    }

    #[test]
    fn untracked_senders_always_pass() {
        assert!(seq_accepts(1000, 0));
    }

    #[test]
    fn wrong_alpha_blends_are_rejected_close_ones_kept() {
        use super::{transition_alpha_fresh, TRANSITION_ALPHA_GAP};
        // Cross-zone / far-position blends must not replace the displayed one.
        assert!(!transition_alpha_fresh(0.9, 0.2));
        assert!(!transition_alpha_fresh(0.0, 0.5));
        // A blend within the tolerance is kept even when it is slightly off.
        assert!(transition_alpha_fresh(0.30, 0.302));
        assert!(transition_alpha_fresh(0.0, TRANSITION_ALPHA_GAP));
        assert!(!transition_alpha_fresh(0.0, TRANSITION_ALPHA_GAP * 2.0));
    }
}

#[cfg(test)]
mod audio_path_gate_tests {
    //! The WAV-extraction session gate (see `audio_path_is_current`): extraction
    //! requests for the same library id supersede each other and the ingest may
    //! only accept a result stamped with the id's latest generation.

    use super::audio_path_is_current;
    use velocut_media::MediaWorker;

    #[test]
    fn stale_audio_path_rejected_and_current_accepted() {
        let worker = MediaWorker::new();
        let id = uuid::Uuid::new_v4();

        // Missing paths are fine: the request's epoch is bumped synchronously
        // on the caller thread, which is what we pin; extraction soft-fails.
        worker.extract_audio_trimmed(id, std::path::PathBuf::from("m1.wav"), 0.0, f64::MAX);
        let gen1 = worker.audio_generation_of(id);
        assert_ne!(gen1, 0);
        assert!(audio_path_is_current(&worker, id, gen1));

        // A second request for the same id supersedes the first.
        worker.extract_audio_trimmed(id, std::path::PathBuf::from("m2.wav"), 1.5, 3.0);
        let gen2 = worker.audio_generation_of(id);
        assert_ne!(gen1, gen2);
        assert!(
            !audio_path_is_current(&worker, id, gen1),
            "a superseded extraction's AudioPath must be rejected"
        );
        assert!(audio_path_is_current(&worker, id, gen2));

        // A result for an id that has never had a request is always stale:
        // the counter starts at 1 and the accessor defaults to 0.
        let other = uuid::Uuid::new_v4();
        assert!(!audio_path_is_current(&worker, other, gen2));
    }
}
