// crates/velocut-ui/src/helpers/memory_manager.rs
//
// Proactive memory manager for VeloCut.
//
// The core problem: CacheContext.frame_bucket_cache uses a 192 MB ceiling that
// only evicts on *insert*. Once scrubbing fills it, the cache sits fully loaded
// at idle — no playback, no scrubbing, just the UI ticking — holding hundreds of
// decoded frames that serve no purpose until the user scrubs again.
//
// This module watches for idleness and trims caches in two stages:
//
//   Stage 1 — Scrub idle (2 s after last playhead movement):
//     • Evict every frame_bucket_cache entry except a small window (±5 s)
//       around the current playhead.  Keeps the current view snappy but
//       frees the bulk of decoded frames immediately.
//
//   Stage 2 — Deep idle (30 s after last any interaction):
//     • Evict ALL frame_bucket_cache and frame_cache entries.
//     • Flush egui's internal caches by replacing Memory with its Default.
//     • Call ctx.forget_all_images() to flush the egui_extras URI loader.
//
// Thumbnail cache is intentionally NOT evicted here — thumbnails are small
// (≈ 20 KB each), actively displayed in the library panel, and expensive to
// re-probe. A separate thumbnail cap (MAX_THUMBNAILS) evicts the oldest entry
// when the library grows very large.
//
// ── Integration ───────────────────────────────────────────────────────────────
//
//   1. Add `memory_manager: MemoryManager` to VeloCutApp.
//   2. Call `self.memory_manager.tick(ctx, &self.state, &mut self.context)`
//      from `tick_modules()` — after audio/video ticks, before repaint.
//   3. The tick() call detects activity automatically via state comparison —
//      no manual notify calls needed.
//
// ── Required change in context.rs ────────────────────────────────────────────
//
//   Change `frame_cache_bytes: usize` to `pub(crate) frame_cache_bytes: usize`
//   so evict_outside_window (an impl on CacheContext defined here) can update
//   the byte budget. The field stays private to external crates.

use std::time::Instant;
use eframe::egui;
use velocut_core::state::ProjectState;
use crate::context::{AppContext, CacheContext};
use crate::velocut_log;
use std::collections::HashMap;

// ── Tuning constants ──────────────────────────────────────────────────────────

/// Seconds of playhead stillness before Stage 1 (scrub-idle trim) fires.
const SCRUB_IDLE_SECS:  f64   = 2.0;

/// Seconds of total inactivity before Stage 2 (deep idle flush) fires.
const DEEP_IDLE_SECS:   f64   = 30.0;

/// How many seconds around the current playhead to *keep* during Stage 1.
/// ±KEEP_WINDOW_SECS worth of bucket entries survive the trim so seeking
/// a short distance after idle still hits the cache.
const KEEP_WINDOW_SECS: f64   = 5.0;

/// Maximum thumbnails to retain. Evicts oldest-inserted beyond this count.
/// At ≈ 20 KB per thumbnail, 100 entries ≈ 2 MB — negligible but bounded.
const MAX_THUMBNAILS:   usize = 100;

// ── MemoryManager ─────────────────────────────────────────────────────────────

pub struct MemoryManager {
    /// Wall-clock time the playhead last changed position.
    last_scrub:      Instant,

    /// Wall-clock time of last any user activity (play/pause/import/edit).
    last_activity:   Instant,

    /// Playhead position observed on the previous tick — used to detect movement.
    prev_time:       f64,

    /// Whether playback was active on the previous tick.
    prev_playing:    bool,

    /// Number of library clips seen on the previous tick — detects imports.
    prev_clip_count: usize,

    /// True after Stage 1 has fired for the current idle period.
    /// Prevents re-firing every frame once the trim is done.
    scrub_trim_done: bool,

    /// True after Stage 2 has fired for the current idle period.
    deep_flush_done: bool,
}

impl MemoryManager {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            last_scrub:      now,
            last_activity:   now,
            prev_time:       -1.0,
            prev_playing:    false,
            prev_clip_count: 0,
            scrub_trim_done: false,
            deep_flush_done: false,
        }
    }

    /// Call once per frame from `tick_modules()`.
    pub fn tick(
        &mut self,
        ctx:     &egui::Context,
        state:   &ProjectState,
        context: &mut AppContext,
    ) {
        self.detect_activity(state);

        let scrub_idle = self.last_scrub.elapsed().as_secs_f64();
        let deep_idle  = self.last_activity.elapsed().as_secs_f64();

        // ── Stage 2: deep idle flush ──────────────────────────────────────────
        if deep_idle >= DEEP_IDLE_SECS && !self.deep_flush_done {
            self.deep_flush(&mut context.cache, ctx);
            self.deep_flush_done = true;
            self.scrub_trim_done = true; // Stage 1 is moot after Stage 2
            return;
        }

        // ── Stage 1: scrub-idle trim ──────────────────────────────────────────
        if scrub_idle >= SCRUB_IDLE_SECS && !self.scrub_trim_done {
            self.scrub_trim(&mut context.cache, state.current_time);
            self.scrub_trim_done = true;
        }

        // ── Thumbnail cap: evict oldest beyond MAX_THUMBNAILS ─────────────────
        // Runs every tick but is O(1) when under the cap so it's cheap.
        let cache = &mut context.cache;
        if cache.thumbnail_cache.len() > MAX_THUMBNAILS {
            let over = cache.thumbnail_cache.len() - MAX_THUMBNAILS;
            let to_remove: Vec<_> = cache.thumbnail_cache.keys()
                .take(over)
                .copied()
                .collect();
            for k in to_remove {
                cache.thumbnail_cache.remove(&k);
            }
            velocut_log!("[memory] thumbnail cap: evicted {over} entries");
        }
    }

    // ── Activity detection ────────────────────────────────────────────────────

    fn detect_activity(&mut self, state: &ProjectState) {
        let time_moved    = (state.current_time - self.prev_time).abs() > 1e-6;
        let play_changed  = state.is_playing != self.prev_playing;
        let clips_changed = state.library.len() != self.prev_clip_count;

        if time_moved {
            // Playhead moved — reset scrub idle timer and re-arm both stages
            // so the next idle period fires fresh.
            self.last_scrub      = Instant::now();
            self.scrub_trim_done = false;
            self.deep_flush_done = false;
        }

        if time_moved || play_changed || clips_changed {
            self.last_activity   = Instant::now();
            self.deep_flush_done = false;
        }

        self.prev_time       = state.current_time;
        self.prev_playing    = state.is_playing;
        self.prev_clip_count = state.library.len();
    }

    // ── Stage 1: scrub-idle trim ──────────────────────────────────────────────

    /// Evict frame_bucket_cache entries outside ±KEEP_WINDOW_SECS of the
    /// current playhead. Keeps the immediate neighbourhood hot so short seeks
    /// after idle still hit the cache; frees everything else.
    fn scrub_trim(&self, cache: &mut CacheContext, current_time: f64) {
        let keep_min = ((current_time - KEEP_WINDOW_SECS).max(0.0) * 4.0) as u32;
        let keep_max = ((current_time + KEEP_WINDOW_SECS) * 4.0) as u32;

        let before = cache.frame_bucket_cache.len();
        cache.evict_outside_window(keep_min, keep_max);
        let after  = cache.frame_bucket_cache.len();

        if before != after {
            velocut_log!(
                "[memory] scrub-idle trim: evicted {} frame buckets ({} remain)",
                before - after, after
            );
        }
    }

    // ── Stage 2: deep idle flush ──────────────────────────────────────────────

    /// Full cache flush: all decoded frames plus egui internal state.
    ///
    /// Note on egui API availability in 0.33:
    ///   • CacheStorage::clear() does not exist — we replace the entire Memory
    ///     with its Default instead, preserving only `options` (theme, zoom,
    ///     accessibility) so the visual config survives the flush.
    ///   • TextureManager::free_unused() is not public — egui reclaims dropped
    ///     textures automatically on the next render pass so this is fine.
    fn deep_flush(&self, cache: &mut CacheContext, ctx: &egui::Context) {
        let buckets_before = cache.frame_bucket_cache.len();

        cache.frame_cache        = HashMap::new();
        cache.frame_bucket_cache = HashMap::new();
        cache.frame_cache_bytes  = 0;
        cache.pending_pb_frame   = None;

        ctx.forget_all_images();

        ctx.memory_mut(|mem| {
            let options = mem.options.clone();
            *mem = egui::Memory::default();
            mem.options = options;
        });

        velocut_log!(
            "[memory] deep idle flush: evicted {} frame buckets + egui memory",
            buckets_before
        );
    }
}

// ── CacheContext extension ────────────────────────────────────────────────────
// evict_outside_window lives here (same crate, different module) rather than
// in context.rs to keep the memory management logic co-located.
//
// REQUIRED: change `frame_cache_bytes: usize` in CacheContext (context.rs) to
// `pub(crate) frame_cache_bytes: usize` — one word — so this impl block can
// update the byte budget. It stays private to external crates.

impl CacheContext {
    /// Evict all frame_bucket_cache entries whose bucket index falls outside
    /// [keep_min, keep_max]. Pass (u32::MAX, u32::MAX) to evict everything.
    /// Updates frame_cache_bytes to match.
    pub fn evict_outside_window(&mut self, keep_min: u32, keep_max: u32) {
        self.frame_bucket_cache.retain(|(_, bucket), (_, bytes)| {
            let keep = *bucket >= keep_min && *bucket <= keep_max;
            if !keep {
                self.frame_cache_bytes = self.frame_cache_bytes.saturating_sub(*bytes);
            }
            keep
        });
    }
}