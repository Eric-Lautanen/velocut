// crates/velocut-ui/src/modules/video_module.rs
//
// VideoModule owns all video scrub/playback frame logic.
// Non-rendering module — tick() and poll_playback() are called every frame
// from app.rs. No egui panel is shown.
//
// Extracted from app.rs so playback and scrub changes never require touching
// the main app loop. To add a feature: edit here, not app.rs.

use velocut_core::state::ProjectState;
use velocut_core::commands::EditorCommand;
use velocut_core::media_types::PlaybackFrame;
use crate::context::AppContext;
use crate::modules::ThumbnailCache;
use super::EditorModule;
use eframe::egui;
use uuid::Uuid;

pub struct VideoModule;

impl VideoModule {
    pub fn new() -> Self { Self }

    // ── Public helpers ────────────────────────────────────────────────────────

    /// Returns the media_id of the timeline clip currently under the playhead.
    /// Used by app.rs to do the thumbnail→frame swap for the preview panel.
    pub fn active_media_id(state: &ProjectState) -> Option<Uuid> {
        state.timeline.iter().find(|c| {
            state.current_time >= c.start_time
                && state.current_time < c.start_time + c.duration
        }).map(|c| c.media_id)
    }

    // ── poll_playback ─────────────────────────────────────────────────────────
    /// PTS-gated playback frame consumption. Call from app::poll_media().
    ///
    /// The decode thread pre-fills a 6-frame channel as fast as FFmpeg can go.
    /// Draining all frames and showing the last races ahead at decode speed
    /// (the classic 3×-speed bug). Instead we use a one-slot pending buffer
    /// and only promote a frame to frame_cache once wall-clock current_time
    /// has caught up to that frame's PTS.
    pub fn poll_playback(
        state: &ProjectState,
        ctx:   &mut AppContext,
        egui_ctx: &egui::Context,
    ) {
        // Clip-local expected timestamp right now (source-offset adjusted).
        let pb_local_t: Option<f64> = state.timeline.iter()
            .find(|c| {
                state.current_time >= c.start_time
                    && state.current_time < c.start_time + c.duration
            })
            .map(|c| (state.current_time - c.start_time + c.source_offset).max(0.0));

        // Step 1: fill pending slot if empty.
        if ctx.pending_pb_frame.is_none() {
            if let Ok(f) = ctx.media_worker.pb_rx.try_recv() {
                ctx.pending_pb_frame = Some(f);
            }
        }

        // Step 2: fast-forward past overdue frames (UI lagged behind decode).
        if let Some(local_t) = pb_local_t {
            while ctx.pending_pb_frame
                .as_ref()
                .map(|f: &PlaybackFrame| f.timestamp < local_t - (1.0 / 30.0))
                .unwrap_or(false)
            {
                match ctx.media_worker.pb_rx.try_recv() {
                    Ok(newer) => { ctx.pending_pb_frame = Some(newer); }
                    Err(_)    => break,
                }
            }
        }

        // Step 3: display pending frame when its PTS is due.
        // Upper bound: frame timestamp <= current_time + 1 frame (not too early).
        // Lower bound: frame timestamp >= current_time - 150ms (not too old).
        // Without the lower bound, a keyframe-aligned frame that's 300ms behind
        // current_time passes the due check on play start and causes a visible
        // jump backward while the decoder catches up.
        // Upper bound: don't show a frame more than 1 tick early.
        // Lower bound: 0.5 s. skip_until_pts in next_frame() means the first frame
        // from the playback thread after a seek should arrive at the correct position,
        // so we don't need the 2 s window we used when advance_to() was blocking.
        // 0.5 s is generous enough to absorb jitter while still rejecting a
        // genuinely stale frame from a previous clip or scrub session leaking through.
        let frame_due = ctx.pending_pb_frame.as_ref().map(|f: &PlaybackFrame| {
            pb_local_t.map(|lt| {
                f.timestamp <= lt + (1.0 / 60.0)
                    && f.timestamp >= lt - 0.5
            }).unwrap_or(true)
        }).unwrap_or(false);

        if frame_due {
            if let Some(f) = ctx.pending_pb_frame.take() {
                let tex = egui_ctx.load_texture(
                    format!("pb-{}", f.id),
                    egui::ColorImage::from_rgba_unmultiplied(
                        [f.width as usize, f.height as usize], &f.data,
                    ),
                    egui::TextureOptions::LINEAR,
                );
                ctx.frame_cache.insert(f.id, tex);
                egui_ctx.request_repaint();
                // Pre-pull next frame so it's ready for the next tick.
                if let Ok(next) = ctx.media_worker.pb_rx.try_recv() {
                    ctx.pending_pb_frame = Some(next);
                }
            }
        }
    }

    // ── tick ──────────────────────────────────────────────────────────────────
    /// 3-layer scrub + playback start/stop. Call every frame from app::update().
    pub fn tick(state: &ProjectState, ctx: &mut AppContext) {
        let just_started = state.is_playing && !ctx.prev_playing;
        let just_stopped = !state.is_playing && ctx.prev_playing;
        ctx.prev_playing = state.is_playing;

        let current_clip = state.timeline.iter().find(|c| {
            state.current_time >= c.start_time
                && state.current_time < c.start_time + c.duration
        }).cloned();

        // ── Playback mode ─────────────────────────────────────────────────────
        if state.is_playing {
            if let Some(clip) = &current_clip {
                let clip_changed = Some(clip.media_id) != ctx.playback_media_id;
                if just_started || clip_changed {
                    ctx.playback_media_id = Some(clip.media_id);
                    // Drop stale scrub frame so preview doesn't freeze on wrong pos.
                    ctx.frame_cache.remove(&clip.media_id);
                    ctx.pending_pb_frame = None;
                    if let Some(lib) = state.library.iter().find(|l| l.id == clip.media_id) {
                        let local_ts = (state.current_time - clip.start_time + clip.source_offset).max(0.0);
                        let aspect   = state.active_video_ratio();
                        ctx.media_worker.start_playback(lib.id, lib.path.clone(), local_ts, aspect);
                    }
                }
            }
            return;
        }

        // ── Transition: playing → stopped ─────────────────────────────────────
        if just_stopped {
            ctx.media_worker.stop_playback();
            ctx.playback_media_id = None;
            ctx.last_frame_req    = None;
            ctx.scrub_last_moved  = None;
            ctx.scrub_coarse_req  = None;
            ctx.pending_pb_frame  = None;
        }

        let Some(clip) = current_clip else {
            ctx.last_frame_req   = None;
            ctx.scrub_last_moved = None;
            ctx.scrub_coarse_req = None;
            return;
        };

        let local_t       = (state.current_time - clip.start_time + clip.source_offset).max(0.0);
        let fine_bucket   = (local_t * 4.0) as u32;   // ¼s grid — cache key only
        let coarse_bucket = (local_t / 2.0) as u32;   // 2s grid — prefetch key
        let fine_key      = (clip.media_id, fine_bucket);

        // scrub_moved: any position change > ~10ms fires a new decode request.
        // Compare exact f64 ts so every ruler pixel triggers a request, not just
        // every ¼s bucket crossing. The latest-wins condvar slot is the rate limiter.
        let scrub_moved = ctx.last_frame_req
            .map(|(rid, last_ts)| rid != clip.media_id || (last_ts - local_t).abs() > 0.010)
            .unwrap_or(true);

        if scrub_moved {
            ctx.scrub_last_moved = Some(std::time::Instant::now());

            if let Some((prev_id, _)) = ctx.last_frame_req {
                if prev_id != clip.media_id {
                    ctx.frame_cache.remove(&prev_id);
                    ctx.scrub_coarse_req = None;
                }
            }
            ctx.last_frame_req = Some((clip.media_id, local_t));

            // Layer 1 (0ms): show nearest cached frame immediately.
            let found_nearby = (0..=8u32).find_map(|delta| {
                let b = fine_bucket.saturating_sub(delta);
                ctx.frame_bucket_cache.get(&(clip.media_id, b)).cloned()
            });
            if let Some(cached) = found_nearby {
                ctx.frame_cache.insert(clip.media_id, cached);
            }

            // Layer 2 (every scrub move): fire exact-timestamp decode request.
            if let Some(lib) = state.library.iter().find(|m| m.id == clip.media_id) {
                let aspect = state.active_video_ratio();
                ctx.media_worker.request_frame(lib.id, lib.path.clone(), local_t, aspect);
            }

            // Layer 2b (per 2s): coarse warm-up prefetch ahead of scrub head.
            let coarse_key = (clip.media_id, coarse_bucket);
            if ctx.scrub_coarse_req != Some(coarse_key) {
                ctx.scrub_coarse_req = Some(coarse_key);
                if let Some(lib) = state.library.iter().find(|m| m.id == clip.media_id) {
                    let aspect = state.active_video_ratio();
                    ctx.media_worker.request_frame(lib.id, lib.path.clone(), coarse_bucket as f64 * 2.0, aspect);
                }
            }
        } else {
            // Layer 3 (150ms idle): precise frame after scrub stops moving.
            if ctx.frame_cache.contains_key(&clip.media_id) {
                let idle = ctx.scrub_last_moved
                    .map(|t| t.elapsed() >= std::time::Duration::from_millis(150))
                    .unwrap_or(false);
                if !idle { return; }
                if ctx.frame_bucket_cache.contains_key(&fine_key) { return; }
                if let Some(lib) = state.library.iter().find(|m| m.id == clip.media_id) {
                    let aspect = state.active_video_ratio();
                    ctx.media_worker.request_frame(lib.id, lib.path.clone(), fine_bucket as f64 / 4.0, aspect);
                }
            }
        }
    }
}

// ── EditorModule (no panel) ───────────────────────────────────────────────────

impl EditorModule for VideoModule {
    fn name(&self) -> &str { "Video" }

    fn ui(
        &mut self,
        _ui:          &mut egui::Ui,
        _state:       &ProjectState,
        _thumb_cache: &mut ThumbnailCache,
        _cmd:         &mut Vec<EditorCommand>,
    ) {
        // No panel — driven entirely by tick() and poll_playback().
    }
}