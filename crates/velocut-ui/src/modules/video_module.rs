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
use velocut_core::media_types::{PlaybackFrame, PlaybackTransitionSpec, TransitionScrubRequest};
use velocut_core::transitions::TransitionKind;
use crate::context::AppContext;
use crate::helpers::clip_query;
use crate::modules::ThumbnailCache;
use super::EditorModule;
use eframe::egui;
use uuid::Uuid;

pub struct VideoModule;

impl VideoModule {
    // ── Public helpers ────────────────────────────────────────────────────────

    /// Returns the media_id of the timeline clip currently under the playhead.
    /// Used by app.rs to do the thumbnail→frame swap for the preview panel.
    pub fn active_media_id(state: &ProjectState) -> Option<Uuid> {
        clip_query::clip_at_time(state, state.current_time).map(|c| c.media_id)
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
        // Find clip under playhead — we need both pb_local_t and media_id.
        let current_clip = clip_query::clip_at_time(state, state.current_time);
        let current_media_id = current_clip.map(|c| c.media_id);
        let pb_local_t: Option<f64> = current_clip
            .map(|c| (state.current_time - c.start_time + c.source_offset).max(0.0));

        // ── Clip-transition eviction (must run before the UI renders) ────────────
        // tick() also calls frame_cache.remove on clip change, but tick() runs
        // AFTER app.rs has already read frame_cache into preview.current_frame.
        // That one-frame window is enough to flash a stale scrub frame from the
        // incoming clip. Clearing it here — inside poll_playback, which is called
        // from poll_media() before update() — closes the window entirely.
        //
        // We detect the transition by comparing current_media_id to
        // ctx.playback.playback_media_id (the id that was active last tick).  We only read
        // playback_media_id here; tick() remains responsible for writing it.
        if state.is_playing {
            let clip_changed = match (current_media_id, ctx.playback.playback_media_id) {
                (Some(cur), Some(prev)) => cur != prev,
                (Some(_), None)         => true,  // playback just started
                _                       => false,
            };
            if clip_changed {
                if let Some(id) = current_media_id {
                    ctx.cache.frame_cache.remove(&id);
                }
                // Only clear pending if it belongs to the wrong clip.
                // Correct new-clip frames may already be in flight.
                if ctx.cache.pending_pb_frame.as_ref()
                    .map(|f| current_media_id.map_or(true, |id| f.id != id))
                    .unwrap_or(false)
                {
                    ctx.cache.pending_pb_frame = None;
                }
            }
        }

        // ── Discard stale pending frame ───────────────────────────────────────
        // Two cases that permanently block the slot if not handled:
        //
        // a) Wrong clip: tick() detects clip_changed AFTER poll_playback runs, so
        //    the transition frame always picks up a clip-1 frame with a large
        //    timestamp (e.g. 28s) while local_t is 0s. Upper-bound check fails.
        //    The slot is stuck until tick() clears it next frame — fine. But if
        //    the same media_id appears on both clips (reused asset), tick() never
        //    clears it. This guard covers both cases.
        //
        // b) Too old: burn_to_pts runs synchronously so current_time advances
        //    during the burn. The first correct frame has timestamp T but local_t
        //    is T + burn_time. If burn_time > lower_bound the frame is rejected
        //    but never discarded — permanent freeze. The too_old guard here
        //    discards it immediately so the slot opens for the next frame.
        if let Some(pending) = &ctx.cache.pending_pb_frame {
            let wrong_clip = current_media_id.map(|id| id != pending.id).unwrap_or(true);
            let too_old    = pb_local_t.map(|lt| pending.timestamp < lt - 3.0).unwrap_or(false);
            if wrong_clip || too_old {
                ctx.cache.pending_pb_frame = None;
            }
        }

        // Step 1: fill pending slot if empty.
        //
        // Drain wrong-id (coast) frames in a single loop rather than one-per-tick.
        // Coast frames carry the OLD clip's media_id. Without this drain, each
        // stale frame takes a full tick to evict via the wrong_clip guard above —
        // up to 4 frames × 16ms = ~64ms of showing nothing at the transition.
        if ctx.cache.pending_pb_frame.is_none() {
            while let Ok(f) = ctx.media_worker.pb_rx.try_recv() {
                let stale = current_media_id.map_or(false, |id| id != f.id);
                if stale { continue; }
                ctx.cache.pending_pb_frame = Some(f);
                break;
            }
        }

        // Step 2: fast-forward past overdue frames.
        // With the 32-frame channel and burn_to_pts completing before first send,
        // this drains the early keyframe frames in a single tick after seek.
        if let Some(local_t) = pb_local_t {
            while ctx.cache.pending_pb_frame
                .as_ref()
                .map(|f: &PlaybackFrame| f.timestamp < local_t - (1.0 / 30.0))
                .unwrap_or(false)
            {
                match ctx.media_worker.pb_rx.try_recv() {
                    Ok(newer) => { ctx.cache.pending_pb_frame = Some(newer); }
                    Err(_)    => break,
                }
            }
        }

        // Step 3: promote pending frame when its PTS is due.
        //
        // Upper bound: 2 frames (2/30s ≈ 67ms) of early-show tolerance for steady
        // state, plus a startup exception for the first 150ms of any new clip.
        //
        // Why the startup exception: after a clip transition the primary decoder
        // burns to `elapsed` (e.g. 10ms into clip_b), but the first decodable
        // keyframe lands ~73ms later (e.g. ts=0.083s). With pb_local_t=0.010 that
        // blows past even the 2/30s upper bound (0.083 > 0.010+0.067), so the
        // frame sits in pending while held_frame (frozen coast, alpha=0.437) keeps
        // displaying — visible as a stall right at the blend handoff.
        // The exception bypasses the upper bound for the first 150ms of any new
        // clip (lt<0.15), where any near-future frame (ts<0.30) is preferable to
        // the stale held_frame. The lower bound (-3s) still guards against genuinely
        // stale frames, and step 2 fast-forward handles runaway lookahead in steady
        // playback.
        //
        // Lower bound: 3.0 s — must cover the worst-case burn_to_pts duration.
        // At 60 fps H.264 with a 5 s GOP, burn is ~300 frames × ~2 ms = ~600 ms.
        // 3 s is ample headroom. Genuine staleness (different clip, scrub bleed)
        // is caught by the wrong_clip / too_old guards above.
        let frame_due = ctx.cache.pending_pb_frame.as_ref().map(|f: &PlaybackFrame| {
            pb_local_t.map(|lt| {
                let above_lower   = f.timestamp >= lt - 3.0;
                let normal_window = f.timestamp <= lt + (2.0 / 30.0);
                // Startup exception: at clip startup (lt < 150ms) show any
                // near-future frame immediately rather than stalling on held_frame.
                let startup_early = lt < 0.15 && f.timestamp < 0.30;
                above_lower && (normal_window || startup_early)
            }).unwrap_or(true)
        }).unwrap_or(false);


        if frame_due {
            if let Some(mut f) = ctx.cache.pending_pb_frame.take() {
                // Look up the filter for the clip currently under the playhead (V-row only).
                let active_filter = state.timeline.iter()
                    .find(|c| {
                        c.track_row % 2 == 0
                            && state.current_time >= c.start_time
                            && state.current_time < c.start_time + c.duration
                    })
                    .map(|c| c.filter.clone())
                    .unwrap_or_default();

                if !active_filter.is_identity() {
                    use velocut_core::filters::helpers::apply_filter_rgba;
                    apply_filter_rgba(&mut f.data, &active_filter);
                }

                let tex = egui_ctx.load_texture(
                    format!("pb-{}", f.id),
                    egui::ColorImage::from_rgba_unmultiplied(
                        [f.width as usize, f.height as usize], &f.data,
                    ),
                    egui::TextureOptions::LINEAR,
                );
                ctx.cache.frame_cache.insert(f.id, tex);
                egui_ctx.request_repaint();
                if let Ok(next) = ctx.media_worker.pb_rx.try_recv() {
                    ctx.cache.pending_pb_frame = Some(next);
                }
            }
        }
    }

    // ── tick ──────────────────────────────────────────────────────────────────
    /// 3-layer scrub + playback start/stop. Call every frame from app::update().
    pub fn tick(state: &ProjectState, ctx: &mut AppContext, egui_ctx: &egui::Context, preview_size: Option<(u32, u32)>) {
        let just_started = state.is_playing && !ctx.playback.prev_playing;
        let just_stopped = !state.is_playing && ctx.playback.prev_playing;
        ctx.playback.prev_playing = state.is_playing;

        // clip_at_time is the shared helper — same predicate as poll_playback above,
        // single source of truth, no risk of the two copies drifting.
        let current_clip = clip_query::clip_at_time(state, state.current_time).cloned();

        // ── Playback mode ─────────────────────────────────────────────────────
        if state.is_playing {
            if let Some(clip) = &current_clip {
                let clip_changed = Some(clip.media_id) != ctx.playback.playback_media_id;
                if just_started || clip_changed {
                    eprintln!(
                        "[tick] clip_changed: just_started={just_started} current_time={:.3} \
                         clip.start_time={:.3} clip.media_id={} prev_media_id={:?}",
                        state.current_time, clip.start_time, clip.media_id,
                        ctx.playback.playback_media_id
                    );
                    ctx.playback.playback_media_id = Some(clip.media_id);
                    // Drop stale scrub frame so preview doesn't freeze on wrong pos.
                    ctx.cache.frame_cache.remove(&clip.media_id);
                    // Only clear pending if it belongs to a different clip.
                    // The pb thread may already have sent a correct frame for the
                    // new clip — clearing it would cause a one-frame blank.
                    if ctx.cache.pending_pb_frame.as_ref().map(|f| f.id != clip.media_id).unwrap_or(false) {
                        ctx.cache.pending_pb_frame = None;
                    }
                    if let Some(lib) = clip_query::library_entry_for(state, clip) {
                        let local_ts = (state.current_time - clip.start_time + clip.source_offset).max(0.0);
                        // Pass aspect=0.0 → LiveDecoder opens at native source resolution.
                        // The pb thread is the preview player; it must be full quality.
                        // crop_uv_rect in preview_module handles any AR mismatch on the GPU.
                        if let Some(spec) = build_incoming_blend_spec(state, clip)
                            .or_else(|| build_blend_spec(state, clip))
                        {
                            eprintln!("[tick] → start_blend_playback alpha_start={:.3}", spec.alpha_start);
                            ctx.media_worker.start_blend_playback(lib.id, lib.path.clone(), local_ts, 0.0, spec, preview_size);
                        } else {
                            eprintln!("[tick] → start_playback (no blend spec — hard cut)");
                            ctx.media_worker.start_playback(lib.id, lib.path.clone(), local_ts, 0.0, preview_size);
                        }
                    }
                    ctx.playback.prebuffer_sent_for = None; // reset on clip change
                }

                // [P0-3] Look-ahead: pre-buffer the next clip's decoder when the
                // playhead is within 500ms of the current clip's end.  The pb thread
                // opens the decoder and incrementally burns its GOP so it is ready
                // (or nearly so) by the time Start/StartBlend arrives at clip_changed.
                let time_remaining = (clip.start_time + clip.duration) - state.current_time;
                if time_remaining > 0.0 && time_remaining < 0.5
                    && ctx.playback.prebuffer_sent_for != Some(clip.id)
                {
                    let clip_end = clip.start_time + clip.duration;
                    let next_clip = state.timeline.iter()
                        .filter(|c| c.track_row % 2 == 0 && !clip_query::is_extracted_audio_clip(c))
                        .find(|c| (c.start_time - clip_end).abs() < 0.05);
                    if let Some(nc) = next_clip {
                        if let Some(lib) = clip_query::library_entry_for(state, nc) {
                            ctx.playback.prebuffer_sent_for = Some(clip.id);
                            ctx.media_worker.prebuffer(lib.id, lib.path.clone(), nc.source_offset, 0.0, preview_size);
                            eprintln!("[tick] prebuffer sent for next clip id={} remaining={time_remaining:.3}s", lib.id);
                        }
                    }
                }
            }
            return;
        }

        // ── Transition: playing → stopped ─────────────────────────────────────
        if just_stopped {
            ctx.media_worker.stop_playback();
            ctx.playback.playback_media_id  = None;
            ctx.playback.last_frame_req     = None;
            ctx.playback.scrub_last_moved   = None;
            ctx.playback.scrub_coarse_req   = None;
            ctx.playback.prebuffer_sent_for = None;
            ctx.cache.pending_pb_frame      = None;
            // Bucket cache is no longer needed after playback stops — clear it
            // so TextureHandles are released and GPU memory returns to baseline.
            ctx.cache.frame_bucket_cache.clear();
        }

        let Some(clip) = current_clip else {
            if let Some((prev_id, _)) = ctx.playback.last_frame_req {
                // Playhead moved into empty space — evict that clip's bucket cache.
                ctx.cache.frame_bucket_cache.retain(|(id, _), _| *id != prev_id);
            }
            ctx.playback.last_frame_req   = None;
            ctx.playback.scrub_last_moved = None;
            ctx.playback.scrub_coarse_req = None;
            return;
        };

        let local_t       = (state.current_time - clip.start_time + clip.source_offset).max(0.0);
        let fine_bucket   = (local_t * 4.0) as u32;   // ¼s grid — cache key only
        let coarse_bucket = (local_t / 2.0) as u32;   // 2s grid — prefetch key


        // scrub_moved: any position change > ~10ms fires a new decode request.
        // Compare exact f64 ts so every ruler pixel triggers a request, not just
        // every ¼s bucket crossing. The latest-wins condvar slot is the rate limiter.
        let scrub_moved = ctx.playback.last_frame_req
            .map(|(rid, last_ts)| rid != clip.media_id || (last_ts - local_t).abs() > 0.010)
            .unwrap_or(true);

        if scrub_moved {
            ctx.playback.scrub_last_moved = Some(std::time::Instant::now());

            // Compute zone first — needed both for clip_changed eviction and L1/L2 below.
            let zone = clip_query::active_transition_at(state);

            if let Some((prev_id, _)) = ctx.playback.last_frame_req {
                if prev_id != clip.media_id {
                    ctx.cache.frame_cache.remove(&prev_id);
                    ctx.playback.scrub_coarse_req = None;
                    // Evict all bucket cache entries for the previous clip.
                    // Buckets accumulate one entry per ¼s scrubbed — without this
                    // they are never freed, causing unbounded TextureHandle growth.
                    ctx.cache.frame_bucket_cache.retain(|(id, _), _| *id != prev_id);
                    // If the new clip is inside a transition zone, also evict any
                    // stale raw frame that may be sitting in frame_cache for it.
                    //
                    // Root cause of both the "flash frame" and "transition appears
                    // half as long" bugs: the user may have scrubbed clip_b outside
                    // the zone previously, leaving a raw single-clip frame in
                    // frame_cache[clip_b.media_id]. When the playhead crosses the
                    // clip_a/clip_b boundary, frame_cache[clip_a] is cleared above
                    // but frame_cache[clip_b] is not. The stale raw frame shows
                    // immediately (alpha≈1.0 visually) while the async blend decode
                    // is in flight — covering 50–150 ms of the clip_b blend half and
                    // making the visible transition appear half as long.
                    if zone.is_some() {
                        ctx.cache.frame_cache.remove(&clip.media_id);
                    }
                }
            }
            ctx.playback.last_frame_req = Some((clip.media_id, local_t));

            // Layer 1 (0ms): show nearest cached frame immediately.
            // In transition zones, L2 stores blended frames in the bucket cache
            // (L2b prefetch is disabled there, so no unblended frames sneak in).
            // Use a narrower search window (2 buckets ≈ 500ms) in zones so the
            // displayed alpha is close to correct; outside zones use the full
            // 8-bucket (2s) window.  Showing a slightly-wrong-alpha cached frame
            // for the 1–3 ticks until L2's exact decode arrives is far better than
            // showing nothing (which caused the visible flicker at zone entry).
            {
                let search_range = if zone.is_some() { 2u32 } else { 8u32 };
                let found_nearby = (0..=search_range).find_map(|delta| {
                    let b = fine_bucket.saturating_sub(delta);
                    ctx.cache.frame_bucket_cache.get(&(clip.media_id, b))
                        .map(|(tex, _)| tex.clone())
                });
                if let Some(cached) = found_nearby {
                    ctx.cache.frame_cache.insert(clip.media_id, cached);
                }
            }

            // Thumbnail fallback: if L1 missed (no bucket-cached frame for the
            // new clip — typical right after a clip boundary crossing), insert the
            // library thumbnail as an immediate low-res placeholder.  The async L2
            // decode will overwrite it within a few ms, but this prevents the blank
            // flash that occurred when frame_cache was empty between L1 miss and L2
            // arrival.
            if !ctx.cache.frame_cache.contains_key(&clip.media_id) {
                if let Some(thumb) = ctx.cache.thumbnail_cache.get(&clip.media_id) {
                    ctx.cache.frame_cache.insert(clip.media_id, thumb.clone());
                }
            }

            // Layer 2 (every scrub move): fire exact-timestamp decode request.
            // If the playhead is inside a transition zone, decode both clips and
            // send a blended frame; otherwise request a normal single-clip frame.
            if let Some(zone) = zone {
                let path_a = clip_query::library_entry_for(state, zone.clip_a).map(|l| l.path.clone());
                let path_b = clip_query::library_entry_for(state, zone.clip_b).map(|l| l.path.clone());
                if let (Some(pa), Some(pb)) = (path_a, path_b) {
                    ctx.media_worker.request_transition_frame(TransitionScrubRequest {
                        // Tag with the CURRENT clip's media_id, not always zone.clip_a.media_id.
                        //
                        // ingest_media_results stores the VideoFrame result under this id.
                        // app.rs looks it up via frame_cache[active_media_id] where
                        // active_media_id = clip_at_time(current_time).media_id = clip.media_id.
                        //
                        // In the first half of the zone clip == clip_a so the value is the
                        // same as before. In the second half clip == clip_b, so tagging with
                        // clip_a.media_id caused a permanent cache miss — ingest stored the
                        // blend under clip_a's key while preview looked it up under clip_b's.
                        clip_a_id:   clip.media_id,
                        clip_a_path: pa,
                        clip_a_ts:   zone.clip_a_source_ts,
                        clip_b_id:   zone.clip_b.media_id,
                        clip_b_path: pb,
                        clip_b_ts:   zone.clip_b_source_ts,
                        alpha:       zone.alpha,
                        kind:        zone.transition.kind,
                    });
                }
            } else if let Some(lib) = clip_query::library_entry_for(state, &clip) {
                let aspect = state.active_video_ratio();
                ctx.media_worker.request_frame(lib.id, lib.path.clone(), local_t, aspect);
            }

            // Layer 2b (per 2s): coarse warm-up prefetch ahead of scrub head.
            // Skipped in transition zones: request_frame would store a raw single-clip
            // frame in frame_bucket_cache which L1 would then flash on the next
            // scrub-in to the zone before the blend decode completes.
            if clip_query::active_transition_at(state).is_none() {
                let coarse_key = (clip.media_id, coarse_bucket);
                if ctx.playback.scrub_coarse_req != Some(coarse_key) {
                    ctx.playback.scrub_coarse_req = Some(coarse_key);
                    if let Some(lib) = clip_query::library_entry_for(state, &clip) {
                        let aspect = state.active_video_ratio();
                        ctx.media_worker.request_frame(lib.id, lib.path.clone(), coarse_bucket as f64 * 2.0, aspect);
                    }
                }
            }
        } else {
            // Layer 3 (150ms idle): full native-resolution decode after scrub stops.
            //
            // We reschedule a repaint from here (not from scrub_moved) so that the
            // timer is self-sustaining: each tick in the idle-wait state schedules the
            // next wakeup for exactly the remaining time. This guarantees L3 fires even
            // if the user never touches the mouse again after releasing the playhead.
            let idle = match ctx.playback.scrub_last_moved {
                None => false,
                Some(moved) => {
                    let elapsed = moved.elapsed();
                    if elapsed < std::time::Duration::from_millis(150) {
                        // Not yet idle — schedule a wakeup for when we will be.
                        let remaining = std::time::Duration::from_millis(150) - elapsed
                            + std::time::Duration::from_millis(5); // small buffer
                        egui_ctx.request_repaint_after(remaining);
                        false
                    } else {
                        true
                    }
                }
            };
            if !idle { return; }
            // L3: check for transition zone exactly as L2 does.
            // Inside a zone: request_transition_frame_hq decodes both clips at
            // native resolution and blends them, replacing the 320-px scrub thumb
            // with a full-quality blended frame once the user stops scrubbing.
            // Outside a zone: request_frame_hq as before.
            let zone_hq = clip_query::active_transition_at(state);
            if let Some(zone) = zone_hq {
                let path_a = clip_query::library_entry_for(state, zone.clip_a).map(|l| l.path.clone());
                let path_b = clip_query::library_entry_for(state, zone.clip_b).map(|l| l.path.clone());
                if let (Some(pa), Some(pb)) = (path_a, path_b) {
                    ctx.media_worker.request_transition_frame_hq(TransitionScrubRequest {
                        clip_a_id:   clip.media_id,
                        clip_a_path: pa,
                        clip_a_ts:   zone.clip_a_source_ts,
                        clip_b_id:   zone.clip_b.media_id,
                        clip_b_path: pb,
                        clip_b_ts:   zone.clip_b_source_ts,
                        alpha:       zone.alpha,
                        kind:        zone.transition.kind,
                    });
                    ctx.playback.scrub_last_moved = None;
                }
            } else if let Some(lib) = clip_query::library_entry_for(state, &clip) {
                // Use local_t (exact playhead position), not the quantised fine_bucket,
                // so the HQ decode lands on the precise frame the user is looking at.
                ctx.media_worker.request_frame_hq(lib.id, lib.path.clone(), local_t, preview_size);
                ctx.playback.scrub_last_moved = None;
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

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a `PlaybackTransitionSpec` for the outgoing transition on `clip`, if any.
///
/// Returns `None` when `clip` has no non-Cut transition, when the next V-row clip
/// cannot be found, or when the next clip's library entry is missing.
/// Used by `tick()` to pass blend info to `start_blend_playback` so the pb thread
/// can open clip_b's decoder lazily and blend frames at the boundary.
fn build_blend_spec(
    state: &velocut_core::state::ProjectState,
    clip:  &velocut_core::state::TimelineClip,
) -> Option<PlaybackTransitionSpec> {
    // Find a non-Cut transition recorded after this clip.
    let tr = state.transitions.iter().find(|tr| {
        tr.after_clip_id == clip.id
            && tr.kind.kind != TransitionKind::Cut
    })?;

    // Find the next V-row video clip (butted up against this one's end via snap-to-end).
    let clip_end = clip.start_time + clip.duration;
    let next_clip = state.timeline.iter()
        .filter(|c| c.track_row % 2 == 0 && !clip_query::is_extracted_audio_clip(c))
        // Snapped clips are exactly adjacent; allow a small epsilon for float drift.
        .find(|c| (c.start_time - clip_end).abs() < 0.05)?;

    let next_lib = clip_query::library_entry_for(state, next_clip)?;

    // Source timestamp in clip_a at which blending begins.
    // Transition is centered on the cut: blend starts D/2 before clip_a ends.
    let blend_start_ts = (clip.source_offset + clip.duration
        - tr.kind.duration_secs as f64 / 2.0).max(0.0);

    Some(PlaybackTransitionSpec {
        clip_b_id:           next_clip.media_id,
        clip_b_path:         next_lib.path.clone(),
        clip_b_source_start: next_clip.source_offset,
        blend_start_ts,
        duration:            tr.kind.duration_secs,
        kind:                tr.kind.kind,
        alpha_start:         0.0,  // clip_a side: ramps 0.0 → 0.5 at cut
        invert_ab:           false,
    })
}

/// Build a `PlaybackTransitionSpec` for the *incoming* (clip_b) side of a
/// centered transition — called when clip_b becomes the active timeline clip
/// and the playhead is still in the first half-D of clip_b (i.e. the blend
/// zone has not yet ended).
///
/// Uses a direct time-range guard instead of `active_transition_at` to avoid
/// the early-exit bug in that function when 3+ clips are on the timeline.
fn build_incoming_blend_spec(
    state:  &velocut_core::state::ProjectState,
    clip_b: &velocut_core::state::TimelineClip,
) -> Option<PlaybackTransitionSpec> {
    let clip_b_start = clip_b.start_time;

    // Find the preceding V-row clip (clip_a).
    let clip_a = state.timeline.iter()
        .filter(|c| c.track_row % 2 == 0 && !clip_query::is_extracted_audio_clip(c))
        .find(|c| (c.start_time + c.duration - clip_b_start).abs() < 0.05);

    let clip_a = match clip_a {
        Some(c) => c,
        None => {
            eprintln!(
                "[blend_in] clip_a not found for clip_b.start_time={:.3} \
                 (no V-row clip ends within 0.05s of that point — gap or first clip)",
                clip_b_start
            );
            return None;
        }
    };

    // Find the non-Cut transition recorded after clip_a.
    let tr = state.transitions.iter().find(|tr| {
        tr.after_clip_id == clip_a.id
            && tr.kind.kind != TransitionKind::Cut
    });

    let tr = match tr {
        Some(t) => t,
        None => {
            eprintln!(
                "[blend_in] no non-Cut transition found after clip_a id={} — no blend",
                clip_a.id
            );
            return None;
        }
    };

    let half_d  = tr.kind.duration_secs as f64 / 2.0;
    let elapsed = (state.current_time - clip_b_start).max(0.0);

    eprintln!(
        "[blend_in] guard: current_time={:.3} clip_b_start={:.3} elapsed={:.3} \
         half_d={:.3} duration={:.3} kind={:?}",
        state.current_time, clip_b_start, elapsed, half_d,
        tr.kind.duration_secs, tr.kind.kind
    );

    // In-zone guard: only activate when we are still inside the incoming blend
    // half, i.e. [clip_b_start, clip_b_start + D/2).
    //
    // A generous 2-frame budget (≈ 67 ms at 30 fps) is added beyond half_d to
    // tolerate stable_dt overshoots and minor frame-rate dips. Without this
    // buffer, a single late tick on a short transition (< 200 ms total duration)
    // is enough to push elapsed past the bare half_d and silently abort the blend.
    //
    // Seeking mid-clip far from the zone (elapsed >> half_d) still returns None
    // and avoids the cost of opening a spurious secondary decoder.
    const TWO_FRAMES: f64 = 2.0 / 30.0;
    if elapsed >= half_d + TWO_FRAMES {
        eprintln!(
            "[blend_in] GUARD TRIGGERED — elapsed {:.3} >= half_d+2f {:.3}; \
             returning None (hard cut will follow). \
             This is the clip_b blend bug if you see it during normal playback.",
            elapsed, half_d + TWO_FRAMES
        );
        return None;
    }

    let clip_a_lib = match clip_query::library_entry_for(state, clip_a) {
        Some(l) => l,
        None => {
            eprintln!("[blend_in] clip_a library entry missing — no blend");
            return None;
        }
    };

    // alpha_start is always 0.5 — do NOT add elapsed/duration here.
    //
    // In the pb thread the formula is:
    //   alpha = alpha_start + local_t / duration
    // where local_t = ts_secs − blend_start_ts.
    //
    // blend_start_ts = clip_b.source_offset, and the primary decoder was opened
    // and burned to (clip_b.source_offset + elapsed). So the first ts_secs out of
    // next_frame() is already ≈ source_offset + elapsed, making local_t ≈ elapsed
    // on frame 1. The elapsed offset is therefore already baked into local_t —
    // adding it again into alpha_start double-counts it and produces a visible
    // jump at the start of the clip_b blend half (circle size pop for iris, etc.).
    let alpha_start = 0.5_f32;

    // Secondary decoder: clip_a's tail, starting at the last D/2 of clip_a's source.
    let clip_a_tail = (clip_a.source_offset + clip_a.duration - half_d).max(0.0);

    eprintln!(
        "[blend_in] returning Some: alpha_start={:.3} clip_a_tail={:.3} \
         clip_b.source_offset={:.3}",
        alpha_start, clip_a_tail, clip_b.source_offset
    );

    Some(PlaybackTransitionSpec {
        clip_b_id:           clip_a.media_id,       // secondary = clip_a tail
        clip_b_path:         clip_a_lib.path.clone(),
        clip_b_source_start: clip_a_tail,
        blend_start_ts:      clip_b.source_offset,  // blend from clip_b's first frame
        duration:            tr.kind.duration_secs, // full D; alpha_start offsets into it
        kind:                tr.kind.kind,
        alpha_start,          // dynamic: 0.5 + elapsed/duration, clamped to [0.5, 1.0]
        invert_ab:           true,  // primary=clip_b is "b"; secondary=clip_a is "a"
    })
}