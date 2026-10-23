// crates/velocut-ui/src/modules/video_module.rs
//
// VideoModule owns all video scrub/playback frame logic.
// Non-rendering module — tick() and poll_playback() are called every frame
// from app.rs. No egui panel is shown.
//
// Extracted from app.rs so playback and scrub changes never require touching
// the main app loop. To add a feature: edit here, not app.rs.

use super::EditorModule;
use crate::context::AppContext;
use crate::helpers::clip_query;
use crate::modules::ThumbnailCache;
use eframe::egui;
use uuid::Uuid;
use velocut_core::commands::EditorCommand;
use velocut_core::media_types::{PlaybackFrame, PlaybackTransitionSpec, TransitionScrubRequest};
use velocut_core::state::ProjectState;
use velocut_core::transitions::TransitionKind;
use velocut_media::{FrameRequest, MediaWorker};

pub struct VideoModule;

/// Drain the playback channel, returning the next frame that belongs to the
/// current playback session.
///
/// Every Start/StartBlend/Stop bumps the worker's session generation; frames
/// produced under an older session are discarded here instead of being shown.
/// This closes the cross-session wrong-alpha window: the `pb_rx` flush inside
/// `start_playback`/`start_blend_playback` is not atomic with the pb thread's
/// blocking sends, and a dropped `Stop` lets the dying session keep producing —
/// in both cases the in-flight frames can carry a matching clip id and a
/// timestamp inside the ±0.5 s windows but content blended under the OLD spec
/// (old alpha), and the id/timestamp-only guards would accept them.
/// Returns `None` once the channel holds nothing but stale frames (or nothing).
fn recv_session_frame(worker: &MediaWorker) -> Option<PlaybackFrame> {
    recv_gen(&worker.pb_rx, worker.playback_generation())
}

/// Body of the session-generation gate, split out so tests can feed a channel
/// directly and pin each cross-session rejection case from the audit.
fn recv_gen(
    pb_rx: &crossbeam_channel::Receiver<PlaybackFrame>,
    expected: u64,
) -> Option<PlaybackFrame> {
    loop {
        match pb_rx.try_recv() {
            Ok(f) if f.generation == expected => return Some(f),
            Ok(_) => continue, // stale generation — cross-session frame
            Err(_) => return None,
        }
    }
}

/// How many playback textures may stay allocated. Playback touches one clip at a
/// time (transitions included — the frame carries the primary clip's id), so this
/// only has to cover the clips a session bounced between recently.
const MAX_PB_TEXTURES: usize = 4;

/// Upload one playback frame into `id`'s persistent texture.
///
/// `load_texture` with a fresh name allocates a GPU texture, registers it, and —
/// since egui frees textures whose last handle dropped — destroys the previous
/// frame's: one alloc/free pair, plus a discarded preview-sized `ColorImage`, per
/// displayed frame. Reusing a `TextureHandle` and calling `set()` on it is a plain
/// in-place upload, which is what the scrub path already does.
///
/// The handle is taken from `pb_textures`, not from `frame_cache`: the frame_cache
/// entry for a clip is often a clone of a scrub or thumbnail texture, and `set()`
/// writes through the Arc — mutating one of those would corrupt the pixels that
/// `frame_bucket_cache` and `thumbnail_cache` still reference.
fn upload_playback_texture(
    ctx: &mut AppContext,
    egui_ctx: &egui::Context,
    id: Uuid,
    width: u32,
    height: u32,
    rgba: &[u8],
) {
    let image =
        || egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba);
    let reusable = matches!(
        ctx.cache.pb_textures.get(&id),
        Some((_, w, h)) if *w == width && *h == height
    );
    if reusable {
        let (handle, _, _) = ctx.cache.pb_textures.get_mut(&id).unwrap();
        handle.set(image(), egui::TextureOptions::LINEAR);
        let tex = handle.clone();
        ctx.cache.frame_cache.insert(id, tex);
        return;
    }
    // Cold path: first frame for this clip, or its preview size changed.
    let tex = egui_ctx.load_texture(format!("pb-{id}"), image(), egui::TextureOptions::LINEAR);
    ctx.cache.frame_cache.insert(id, tex.clone());
    ctx.cache.pb_textures.insert(id, (tex, width, height));
    if ctx.cache.pb_textures.len() > MAX_PB_TEXTURES {
        if let Some(old) = ctx.cache.pb_textures.keys().copied().find(|k| *k != id) {
            ctx.cache.pb_textures.remove(&old);
        }
    }
}

impl VideoModule {
    // ── Public helpers ────────────────────────────────────────────────────────

    /// Returns the media_id of the timeline clip currently under the playhead.
    /// Used by app.rs to do the thumbnail→frame swap for the preview panel.
    /// Video rows only — the preview must never treat an extracted A-row audio
    /// clip as the clip whose frame is displayed.
    pub fn active_media_id(state: &ProjectState) -> Option<Uuid> {
        clip_query::video_clip_at_time(state, state.current_time).map(|c| c.media_id)
    }

    // ── poll_playback ─────────────────────────────────────────────────────────
    /// PTS-gated playback frame consumption. Call from app::poll_media().
    ///
    /// The decode thread pre-fills a 6-frame channel as fast as FFmpeg can go.
    /// Draining all frames and showing the last races ahead at decode speed
    /// (the classic 3×-speed bug). Instead we use a one-slot pending buffer
    /// and only promote a frame to frame_cache once wall-clock current_time
    /// has caught up to that frame's PTS.
    pub fn poll_playback(state: &ProjectState, ctx: &mut AppContext, egui_ctx: &egui::Context) {
        // Find VIDEO clip under playhead — we need both pb_local_t and media_id.
        // clip_at_time alone matches A-row audio clips whose span starts before
        // or outlives their V-row partner, which made the preview decode an
        // audio source as video and compute a wrong local_t.
        let current_clip = clip_query::video_clip_at_time(state, state.current_time);
        let current_media_id = current_clip.map(|c| c.media_id);
        let pb_local_t: Option<f64> =
            current_clip.map(|c| (state.current_time - c.start_time + c.source_offset).max(0.0));

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
                (Some(_), None) => true, // playback just started
                _ => false,
            };
            if clip_changed {
                if let Some(id) = current_media_id {
                    ctx.cache.frame_cache.remove(&id);
                }
                // Only clear pending if it belongs to the wrong clip.
                // Correct new-clip frames may already be in flight.
                if ctx
                    .cache
                    .pending_pb_frame
                    .as_ref()
                    .map(|f| current_media_id != Some(f.id))
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
            // Session generation must match too: a frame queued before a
            // start/stop can never be shown under the new session, even when
            // its id and timestamp still look right.
            let wrong_gen = pending.generation != ctx.media_worker.playback_generation();
            let wrong_clip = current_media_id.map(|id| id != pending.id).unwrap_or(true);
            // Same 0.5 s staleness threshold the drain/deduct loop and the
            // promote gate below use. The previous 3.0 s window let a frame
            // that could never promote (0.5–3 s behind) sit in the slot,
            // blocking fresher frames.
            let too_old = pb_local_t
                .map(|lt| pending.timestamp < lt - 0.5)
                .unwrap_or(false);
            if wrong_gen || wrong_clip || too_old {
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
            while let Some(f) = recv_session_frame(&ctx.media_worker) {
                let stale = current_media_id.is_some_and(|id| id != f.id);
                if stale {
                    continue;
                }
                ctx.cache.pending_pb_frame = Some(f);
                break;
            }
        }

        // Step 2: drain stale/overdue frames, keep the best available frame.
        //
        // With blocking send in the pb thread, the channel is small (6 frames)
        // and the decoder is rate-limited to UI consumption speed, so this loop
        // typically executes 0-1 iterations per tick.
        if let Some(local_t) = pb_local_t {
            // Discard frames that are more than 0.5s behind local_t.
            let too_old_threshold = local_t - 0.5;
            let too_far_ahead = local_t + 0.5;

            // Drain any frames that are too old.
            while ctx
                .cache
                .pending_pb_frame
                .as_ref()
                .map(|f| f.timestamp < too_old_threshold)
                .unwrap_or(false)
            {
                if let Some(newer) = recv_session_frame(&ctx.media_worker) {
                    ctx.cache.pending_pb_frame = Some(newer);
                } else {
                    ctx.cache.pending_pb_frame = None;
                    break;
                }
            }

            // If we have a frame that's too far ahead, try to find a closer one
            // in the channel (but keep the too-far-ahead one if nothing better).
            if ctx
                .cache
                .pending_pb_frame
                .as_ref()
                .map(|f| f.timestamp > too_far_ahead)
                .unwrap_or(false)
            {
                let mut best = ctx.cache.pending_pb_frame.take();
                loop {
                    match recv_session_frame(&ctx.media_worker) {
                        Some(f) => {
                            if f.timestamp >= too_old_threshold && f.timestamp <= too_far_ahead {
                                // Found a good frame within window.
                                ctx.cache.pending_pb_frame = Some(f);
                                break;
                            }
                            if f.timestamp < too_old_threshold {
                                continue; // still too old, discard
                            }
                            // Too far ahead, but maybe closer than current best.
                            if best.as_ref().is_none_or(|b| f.timestamp < b.timestamp) {
                                best = Some(f);
                            }
                        }
                        None => {
                            ctx.cache.pending_pb_frame = best;
                            break;
                        }
                    }
                }
            }
        }

        // Step 3: promote pending frame when its PTS is within the display window.
        let frame_due = ctx
            .cache
            .pending_pb_frame
            .as_ref()
            .map(|f: &PlaybackFrame| {
                pb_local_t
                    .map(|lt| {
                        // Both bounds use the same 0.5 s window as the drain
                        // loop above, so a frame outside it can never promote
                        // and will have been discarded there already.
                        f.timestamp >= lt - 0.5 && f.timestamp <= lt + 0.5
                    })
                    .unwrap_or(true)
            })
            .unwrap_or(false);

        if frame_due {
            if let Some(mut f) = ctx.cache.pending_pb_frame.take() {
                // Look up the filter for the clip currently under the playhead (V-row only).
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
                    apply_filter_rgba(&mut f.data, &active_filter);
                }

                upload_playback_texture(ctx, egui_ctx, f.id, f.width, f.height, &f.data);
                egui_ctx.request_repaint();
                // Pre-fill pending for next tick.
                if ctx.cache.pending_pb_frame.is_none() {
                    while let Some(f) = recv_session_frame(&ctx.media_worker) {
                        let stale = current_media_id.is_some_and(|id| id != f.id);
                        if stale {
                            continue;
                        }
                        ctx.cache.pending_pb_frame = Some(f);
                        break;
                    }
                }
            }
        }
    }

    // ── tick ──────────────────────────────────────────────────────────────────
    /// The next monotone scrub-request sequence number.
    ///
    /// Every scrub-path request (L2 exact, L2b coarse, L2/HQ transition, L3 HQ)
    /// gets one at send time; the media worker echoes it back on the result so
    /// ingest can reject out-of-order completions from the concurrent decode
    /// executors (see the watermark gate in `AppContext::ingest_video_frame`).
    fn next_scrub_seq(ctx: &mut AppContext) -> u64 {
        ctx.playback.scrub_req_seq += 1;
        ctx.playback.scrub_req_seq
    }

    /// 3-layer scrub + playback start/stop. Call every frame from app::update().
    pub fn tick(
        state: &ProjectState,
        ctx: &mut AppContext,
        egui_ctx: &egui::Context,
        preview_size: Option<(u32, u32)>,
    ) {
        let just_started = state.is_playing && !ctx.playback.prev_playing;
        let just_stopped = !state.is_playing && ctx.playback.prev_playing;
        ctx.playback.prev_playing = state.is_playing;

        // clip_query::video_clip_at_time is the shared helper — video rows only,
        // single source of truth, no risk of the copies drifting.
        let current_clip = clip_query::video_clip_at_time(state, state.current_time).cloned();

        // ── Playback mode ─────────────────────────────────────────────────────
        if state.is_playing {
            if let Some(clip) = &current_clip {
                let clip_changed = Some(clip.media_id) != ctx.playback.playback_media_id;
                if just_started || clip_changed {
                    eprintln!(
                        "[tick] clip_changed: just_started={just_started} current_time={:.3} \
                         clip.start_time={:.3} clip.media_id={} prev_media_id={:?}",
                        state.current_time,
                        clip.start_time,
                        clip.media_id,
                        ctx.playback.playback_media_id
                    );
                    ctx.playback.playback_media_id = Some(clip.media_id);
                    // Drop stale scrub frame so preview doesn't freeze on wrong pos.
                    ctx.cache.frame_cache.remove(&clip.media_id);
                    // Clear ALL pending playback frames when starting new playback.
                    // This prevents the "scrub then play freeze" where a stale
                    // pending frame from scrubbing blocks new playback frames.
                    ctx.cache.pending_pb_frame = None;
                    // Also clear the frame bucket cache for this clip to prevent
                    // stale low-res scrub frames from showing during playback.
                    ctx.cache
                        .frame_bucket_cache
                        .retain(|(id, _, _), _| *id != clip.media_id);
                    if let Some(lib) = clip_query::library_entry_for(state, clip) {
                        let local_ts =
                            (state.current_time - clip.start_time + clip.source_offset).max(0.0);
                        // Pass aspect=0.0 → without preview_size the pb decoder opens at
                        // native source resolution and crop_uv_rect corrects AR on the GPU.
                        // With preview_size known, the decoder forces panel dimensions and
                        // center-crops the source to the canvas AR itself
                        // (center_crop_and_scale_cached) — both converge on the same visual.
                        if let Some(spec) = build_incoming_blend_spec(state, clip)
                            .or_else(|| build_blend_spec(state, clip))
                        {
                            eprintln!(
                                "[tick] → start_blend_playback alpha_start={:.3}",
                                spec.alpha_start
                            );
                            ctx.media_worker.start_blend_playback(
                                lib.id,
                                lib.path.clone(),
                                local_ts,
                                0.0,
                                spec,
                                preview_size,
                            );
                        } else {
                            eprintln!("[tick] → start_playback (no blend spec — hard cut)");
                            ctx.media_worker.start_playback(
                                lib.id,
                                lib.path.clone(),
                                local_ts,
                                0.0,
                                preview_size,
                            );
                        }
                    }
                    ctx.playback.prebuffer_sent_for = None; // reset on clip change
                }

                // [P0-3] Look-ahead: pre-buffer the next clip's decoder when the
                // playhead is within 500ms of the current clip's end.  The pb thread
                // opens the decoder and incrementally burns its GOP so it is ready
                // (or nearly so) by the time Start/StartBlend arrives at clip_changed.
                let time_remaining = (clip.start_time + clip.duration) - state.current_time;
                if time_remaining > 0.0
                    && time_remaining < 0.5
                    && ctx.playback.prebuffer_sent_for != Some(clip.id)
                {
                    let clip_end = clip.start_time + clip.duration;
                    let next_clip = state
                        .timeline
                        .iter()
                        .filter(|c| c.track_row % 2 == 0 && !clip_query::is_extracted_audio_clip(c))
                        .find(|c| (c.start_time - clip_end).abs() < 0.05);
                    if let Some(nc) = next_clip {
                        if let Some(lib) = clip_query::library_entry_for(state, nc) {
                            ctx.playback.prebuffer_sent_for = Some(clip.id);
                            ctx.media_worker.prebuffer(
                                lib.id,
                                lib.path.clone(),
                                nc.source_offset,
                                0.0,
                                preview_size,
                            );
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
            ctx.playback.playback_media_id = None;
            ctx.playback.last_frame_req = None;
            ctx.playback.scrub_last_moved = None;
            ctx.playback.scrub_coarse_req = None;
            ctx.playback.prebuffer_sent_for = None;
            ctx.cache.pending_pb_frame = None;
            // Bucket cache is no longer needed after playback stops — clear it
            // so TextureHandles are released and GPU memory returns to baseline.
            ctx.cache.frame_bucket_cache.clear();
        }

        let Some(clip) = current_clip else {
            if let Some((prev_id, _)) = ctx.playback.last_frame_req {
                // Playhead moved into empty space - evict that clip's bucket cache.
                ctx.cache
                    .frame_bucket_cache
                    .retain(|(id, _, _), _| *id != prev_id);
            }
            ctx.playback.last_frame_req = None;
            ctx.playback.scrub_last_moved = None;
            ctx.playback.scrub_coarse_req = None;
            return;
        };

        let local_t = (state.current_time - clip.start_time + clip.source_offset).max(0.0);
        let fine_bucket = (local_t * 4.0) as u32; // ¼s grid — cache key only
        let coarse_bucket = (local_t / 2.0) as u32; // 2s grid — prefetch key

        // scrub_moved: position change > ~30ms (≈1 frame) fires a new decode request.
        // The debounce prevents redundant decodes when the UI runs at 60 Hz and the
        // user scrubs quickly - without it the decode thread gets swamped with stale
        // requests that the latest-wins slot can only partially suppress.
        const SCRUB_DEBOUNCE_SECS: f64 = 0.030;
        let scrub_moved = ctx
            .playback
            .last_frame_req
            .map(|(rid, last_ts)| {
                rid != clip.media_id || (last_ts - local_t).abs() > SCRUB_DEBOUNCE_SECS
            })
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
                    ctx.cache
                        .frame_bucket_cache
                        .retain(|(id, _, _), _| *id != prev_id);
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
            // for the 1-3 ticks until L2's exact decode arrives is far better than
            // showing nothing (which caused the visible flicker at zone entry).
            {
                let search_range = if zone.is_some() { 2u32 } else { 8u32 };
                // Search both directions, nearest-first: scrubbing FORWARD into a
                // region whose only cached frames lie ahead previously found
                // nothing because the search subtracted buckets only.
                let found_nearby = (0..=search_range).find_map(|delta| {
                    // Only look at fine-bucket entries (is_coarse=false) for L1.
                    // Coarse prefetch entries are stored with is_coarse=true so
                    // they never collide with fine-bucket lookups.
                    let lookup = |b: u32| {
                        ctx.cache
                            .frame_bucket_cache
                            .get(&(clip.media_id, b, false))
                            .map(|(tex, _)| tex.clone())
                    };
                    let minus = lookup(fine_bucket.saturating_sub(delta));
                    let plus = lookup(fine_bucket + delta);
                    minus.or(plus)
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
                let path_a =
                    clip_query::library_entry_for(state, zone.clip_a).map(|l| l.path.clone());
                let path_b =
                    clip_query::library_entry_for(state, zone.clip_b).map(|l| l.path.clone());
                if let (Some(pa), Some(pb)) = (path_a, path_b) {
                    let seq = Self::next_scrub_seq(ctx);
                    ctx.media_worker
                        .request_transition_frame(TransitionScrubRequest {
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
                            clip_a_id: clip.media_id,
                            clip_a_path: pa,
                            clip_a_ts: zone.clip_a_source_ts,
                            clip_b_id: zone.clip_b.media_id,
                            clip_b_path: pb,
                            clip_b_ts: zone.clip_b_source_ts,
                            alpha: zone.alpha,
                            kind: zone.transition.kind,
                            req_seq: seq,
                        });
                }
            } else if let Some(lib) = clip_query::library_entry_for(state, &clip) {
                let aspect = state.active_video_ratio();
                let seq = Self::next_scrub_seq(ctx);
                ctx.media_worker.request_frame(
                    FrameRequest {
                        id: lib.id,
                        path: lib.path.clone(),
                        timestamp: local_t,
                        aspect,
                        preview_size,
                        req_seq: seq,
                    },
                    false,
                );
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
                        let seq = Self::next_scrub_seq(ctx);
                        // Coarse prefetch: routed to its own slot and tier-tagged so
                        // the result is stored with is_coarse=true and can never
                        // displace an exact scrub request or land in frame_cache.
                        ctx.media_worker.request_frame(
                            FrameRequest {
                                id: lib.id,
                                path: lib.path.clone(),
                                timestamp: coarse_bucket as f64 * 2.0,
                                aspect,
                                preview_size,
                                req_seq: seq,
                            },
                            true,
                        );
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
            if !idle {
                return;
            }
            // L3: check for transition zone exactly as L2 does.
            // Inside a zone: request_transition_frame_hq decodes both clips at
            // native resolution and blends them, replacing the 320-px scrub thumb
            // with a full-quality blended frame once the user stops scrubbing.
            // Outside a zone: request_frame_hq as before.
            let zone_hq = clip_query::active_transition_at(state);
            if let Some(zone) = zone_hq {
                let path_a =
                    clip_query::library_entry_for(state, zone.clip_a).map(|l| l.path.clone());
                let path_b =
                    clip_query::library_entry_for(state, zone.clip_b).map(|l| l.path.clone());
                if let (Some(pa), Some(pb)) = (path_a, path_b) {
                    let seq = Self::next_scrub_seq(ctx);
                    ctx.media_worker
                        .request_transition_frame_hq(TransitionScrubRequest {
                            clip_a_id: clip.media_id,
                            clip_a_path: pa,
                            clip_a_ts: zone.clip_a_source_ts,
                            clip_b_id: zone.clip_b.media_id,
                            clip_b_path: pb,
                            clip_b_ts: zone.clip_b_source_ts,
                            alpha: zone.alpha,
                            kind: zone.transition.kind,
                            req_seq: seq,
                        });
                    ctx.playback.scrub_last_moved = None;
                }
            } else if let Some(lib) = clip_query::library_entry_for(state, &clip) {
                // Use local_t (exact playhead position), not the quantised fine_bucket,
                // so the HQ decode lands on the precise frame the user is looking at.
                let seq = Self::next_scrub_seq(ctx);
                ctx.media_worker.request_frame_hq(
                    lib.id,
                    lib.path.clone(),
                    local_t,
                    preview_size,
                    seq,
                );
                ctx.playback.scrub_last_moved = None;
            }
        }
    }
}

// ── EditorModule (no panel) ───────────────────────────────────────────────────

impl EditorModule for VideoModule {
    fn ui(
        &mut self,
        _ui: &mut egui::Ui,
        _state: &ProjectState,
        _thumb_cache: &mut ThumbnailCache,
        _cmd: &mut Vec<EditorCommand>,
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
    clip: &velocut_core::state::TimelineClip,
) -> Option<PlaybackTransitionSpec> {
    // Find a non-Cut transition recorded after this clip.
    let tr = state
        .transitions
        .iter()
        .find(|tr| tr.after_clip_id == clip.id && tr.kind.kind != TransitionKind::Cut)?;

    // Find the next video clip on the SAME row, butted up against this one's
    // end. Before this was constrained by `track_row % 2 == 0` — V1 and V2
    // matched each other, so a clip ending where an unrelated V2 clip began
    // invented a playback blend across the two layers (the same phantom-blend
    // bug `active_transition_at` fixed for the preview).
    let clip_end = clip.start_time + clip.duration;
    let next_clip = state
        .timeline
        .iter()
        .filter(|c| c.track_row == clip.track_row && !clip_query::is_extracted_audio_clip(c))
        // Snapped clips are exactly adjacent; use the same epsilon as the zone
        // math so scrub preview and playback agree on where a transition exists.
        .find(|c| (c.start_time - clip_end).abs() < clip_query::ADJACENCY_EPS)?;

    let next_lib = clip_query::library_entry_for(state, next_clip)?;

    // Effective duration clamped to what both clips can host. A slider value
    // larger than the clips must behave the same in playback as in the preview
    // (which clamps via `active_transition_at`), otherwise the exported video
    // blends over spans the timeline view does not show.
    let d_eff = tr.kind.clamped_secs(clip.duration, next_clip.duration);
    if d_eff <= 0.0 {
        return None;
    }

    // Source timestamp in clip_a at which blending begins.
    // Transition is centered on the cut: blend starts d_eff/2 before clip_a ends.
    let blend_start_ts = (clip.source_offset + clip.duration - d_eff / 2.0).max(0.0);

    Some(PlaybackTransitionSpec {
        clip_b_id: next_clip.media_id,
        clip_b_path: next_lib.path.clone(),
        clip_b_source_start: next_clip.source_offset,
        blend_start_ts,
        duration: d_eff as f32,
        kind: tr.kind.kind,
        alpha_start: 0.0, // clip_a side: ramps 0.0 → 0.5 at cut
        invert_ab: false,
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
    state: &velocut_core::state::ProjectState,
    clip_b: &velocut_core::state::TimelineClip,
) -> Option<PlaybackTransitionSpec> {
    let clip_b_start = clip_b.start_time;

    // Find the preceding video clip on the SAME row as clip_b (see the
    // cross-layer note in `build_blend_spec`).
    let clip_a = state
        .timeline
        .iter()
        .filter(|c| c.track_row == clip_b.track_row && !clip_query::is_extracted_audio_clip(c))
        .find(|c| (c.start_time + c.duration - clip_b_start).abs() < clip_query::ADJACENCY_EPS);

    let clip_a = match clip_a {
        Some(c) => c,
        None => {
            return None;
        }
    };

    // Find the non-Cut transition recorded after clip_a.
    let tr = state
        .transitions
        .iter()
        .find(|tr| tr.after_clip_id == clip_a.id && tr.kind.kind != TransitionKind::Cut);

    let tr = match tr {
        Some(t) => t,
        None => {
            return None;
        }
    };

    // Same clamp as the outgoing spec and the preview zone math: playback must
    // not blend across more timeline than `active_transition_at` reports.
    let d_eff = tr.kind.clamped_secs(clip_a.duration, clip_b.duration);
    let half_d = d_eff / 2.0;
    let elapsed = (state.current_time - clip_b_start).max(0.0);

    // In-zone guard: only activate when we are still inside the incoming blend
    // half, i.e. [clip_b_start, clip_b_start + half_d).
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
        return None;
    }

    let clip_a_lib = match clip_query::library_entry_for(state, clip_a) {
        Some(l) => l,
        None => {
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

    // Secondary decoder: clip_a's tail, starting at the last half_d of clip_a's source.
    let clip_a_tail = (clip_a.source_offset + clip_a.duration - half_d).max(0.0);

    Some(PlaybackTransitionSpec {
        clip_b_id: clip_a.media_id, // secondary = clip_a tail
        clip_b_path: clip_a_lib.path.clone(),
        clip_b_source_start: clip_a_tail,
        blend_start_ts: clip_b.source_offset, // blend from clip_b's first frame
        duration: d_eff as f32,               // full D; alpha_start offsets into it
        kind: tr.kind.kind,
        alpha_start,     // dynamic: 0.5 + elapsed/duration, clamped to [0.5, 1.0]
        invert_ab: true, // primary=clip_b is "b"; secondary=clip_a is "a"
    })
}

#[cfg(test)]
mod session_gate_tests {
    //! Pins the playback session-generation gate — the pb-path analog of the
    //! scrub-path seq watermark. See the audit: old-session frames can carry a
    //! matching clip id and a timestamp inside the ±0.5 s windows at a session
    //! boundary, but their pixels were blended under the OLD spec (old alpha).

    use super::recv_gen;
    use uuid::Uuid;
    use velocut_core::media_types::{FrameBuf, PlaybackFrame};

    fn frame(gen: u64, id: Uuid, ts: f64) -> PlaybackFrame {
        PlaybackFrame {
            id,
            timestamp: ts,
            width: 1,
            height: 1,
            data: FrameBuf::detached(vec![0u8; 4]),
            generation: gen,
        }
    }

    #[test]
    fn frames_of_a_superseded_session_are_skipped_even_with_matching_id_and_window() {
        // Audit case 1 — pause/resume inside the same clip's transition half:
        // the fresh StartBlend targets the same clip id at a nearby timeline
        // position, so wrong_clip/too_old would accept these old-session
        // frames. Only the generation separates them.
        let id = Uuid::new_v4();
        let (tx, rx) = crossbeam_channel::bounded(8);
        tx.send(frame(1, id, 3.00)).unwrap(); // old incoming half, alpha≈0.52
        tx.send(frame(1, id, 3.05)).unwrap(); // old incoming half, alpha≈0.68
        tx.send(frame(2, id, 3.02)).unwrap(); // fresh session, correct alpha

        let got = recv_gen(&rx, 2).unwrap();
        assert_eq!(got.generation, 2);
        assert_eq!(got.timestamp, 3.02);
        // The stale ones never surfaced.
        assert!(recv_gen(&rx, 2).is_none());
    }

    #[test]
    fn the_same_asset_reused_across_zones_is_still_rejected_by_generation() {
        // Audit case 2 — the asset is reused back-to-back on the timeline, so
        // the old and new clips share a media_id. id alone cannot discriminate
        // the sessions; the generation gate must.
        let id = Uuid::new_v4();
        let (tx, rx) = crossbeam_channel::bounded(8);
        tx.send(frame(4, id, 7.40)).unwrap(); // zone 1's blend output
        tx.send(frame(5, id, 7.42)).unwrap(); // zone 2's fresh frame

        let got = recv_gen(&rx, 5).unwrap();
        assert_eq!(got.generation, 5);
    }

    #[test]
    fn a_dropped_stop_leaves_only_rejectable_frames() {
        // Audit case 3 — stop_playback bumps the generation even when its Stop
        // command is dropped, so a session the pb thread is still finishing
        // (or will never finish until the channel fills) cannot surface.
        let id = Uuid::new_v4();
        let (tx, rx) = crossbeam_channel::bounded(8);
        tx.send(frame(3, id, 1.10)).unwrap();
        tx.send(frame(3, id, 1.15)).unwrap();
        // Session bumped to 4 by the (lost) stop.
        assert!(recv_gen(&rx, 4).is_none());
        // Channel fully drained, nothing left behind.
        assert!(recv_gen(&rx, 4).is_none());
    }

    #[test]
    fn the_current_session_passes_through_untouched() {
        let id = Uuid::new_v4();
        let (tx, rx) = crossbeam_channel::bounded(8);
        tx.send(frame(7, id, 2.00)).unwrap();
        tx.send(frame(7, Uuid::new_v4(), 2.05)).unwrap();

        let first = recv_gen(&rx, 7).unwrap();
        assert_eq!((first.id, first.timestamp, first.generation), (id, 2.00, 7));
        let second = recv_gen(&rx, 7).unwrap();
        assert_eq!(second.generation, 7);
        assert!(recv_gen(&rx, 7).is_none());
    }
}

#[cfg(test)]
mod texture_reuse_tests {
    //! Pins the playback texture policy: one persistent `TextureHandle` per clip,
    //! updated in place, instead of an alloc+free pair for every displayed frame.
    //!
    //! Assertions go through the `TextureManager` rather than through pixels: a
    //! queued `TexturesDelta` names the texture it writes to, so "did this upload
    //! touch the thumbnail's texture?" and "how many textures did six frames
    //! allocate?" are both answerable headlessly. A full `set()` also coalesces any
    //! pending delta for the same texture id, which is why six uploads leave one
    //! entry.

    use super::{upload_playback_texture, MAX_PB_TEXTURES};
    use crate::context::AppContext;
    use std::collections::HashSet;
    use uuid::Uuid;
    use velocut_media::MediaWorker;

    fn clip(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn rgba(w: usize, h: usize, fill: u8) -> Vec<u8> {
        vec![fill; w * h * 4]
    }

    fn ctx() -> AppContext {
        AppContext::new(MediaWorker::new())
    }

    /// Textures egui itself allocates (the font atlas), so counts can be asserted
    /// as "how many did playback add" rather than against a magic constant.
    fn baseline(egui_ctx: &egui::Context) -> usize {
        let allocated = egui_ctx.tex_manager().read().num_allocated();
        // Drop egui's own queued uploads so later delta checks see only playback's.
        drained_writes(egui_ctx);
        allocated
    }

    /// Ids written to by the deltas queued since the last drain.
    fn drained_writes(egui_ctx: &egui::Context) -> Vec<egui::TextureId> {
        egui_ctx
            .tex_manager()
            .write()
            .take_delta()
            .set
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    #[test]
    fn six_playback_frames_allocate_one_texture() {
        let egui_ctx = egui::Context::default();
        let mut ctx = ctx();
        let c = clip(1);
        let pixels = rgba(16, 8, 40);
        let base = baseline(&egui_ctx);

        for _ in 0..6 {
            upload_playback_texture(&mut ctx, &egui_ctx, c, 16, 8, &pixels);
        }

        // One GPU texture for the clip, and the displayed handle is that texture.
        assert_eq!(egui_ctx.tex_manager().read().num_allocated(), base + 1);
        let (tex, w, h) = &ctx.cache.pb_textures[&c];
        assert_eq!((*w, *h), (16, 8));
        assert_eq!(ctx.cache.frame_cache[&c].id(), tex.id());
        assert_eq!(tex.size(), [16, 8]);

        // Six in-place uploads, coalesced into the latest one per texture id.
        // Before the change this was six distinct textures, each with its own name.
        let writes = drained_writes(&egui_ctx);
        assert_eq!(writes, vec![tex.id()]);
    }

    #[test]
    fn playback_never_writes_through_a_shared_texture() {
        // When the playhead enters a clip, frame_cache is seeded with a clone of the
        // thumbnail or a scrub-bucket texture. Writing through that handle would
        // rewrite the pixels the timeline thumbnail (and every bucket entry sharing
        // it) still points at, so playback must allocate its own texture even though
        // the size matches.
        let egui_ctx = egui::Context::default();
        let mut ctx = ctx();
        let c = clip(2);
        let pixels = rgba(16, 8, 40);

        let shared = egui_ctx.load_texture(
            "thumb",
            egui::ColorImage::from_rgba_unmultiplied([16, 8], &rgba(16, 8, 200)),
            egui::TextureOptions::LINEAR,
        );
        let base = baseline(&egui_ctx);
        ctx.cache.thumbnail_cache.insert(c, shared.clone());
        ctx.cache.frame_cache.insert(c, shared.clone());
        drained_writes(&egui_ctx);

        upload_playback_texture(&mut ctx, &egui_ctx, c, 16, 8, &pixels);

        let pb = ctx.cache.pb_textures[&c].0.id();
        assert_ne!(pb, shared.id());
        assert_eq!(ctx.cache.frame_cache[&c].id(), pb);
        // The uploaded texture is the only one written to: the shared handle's
        // texture still holds the thumbnail's own pixels.
        assert_eq!(drained_writes(&egui_ctx), vec![pb]);
        assert_eq!(egui_ctx.tex_manager().read().num_allocated(), base + 1);
    }

    #[test]
    fn a_preview_size_change_reallocates_once() {
        let egui_ctx = egui::Context::default();
        let mut ctx = ctx();
        let c = clip(3);
        let base = baseline(&egui_ctx);

        upload_playback_texture(&mut ctx, &egui_ctx, c, 16, 8, &rgba(16, 8, 1));
        let first = ctx.cache.pb_textures[&c].0.id();

        upload_playback_texture(&mut ctx, &egui_ctx, c, 16, 16, &rgba(16, 16, 2));
        let (tex, w, h) = &ctx.cache.pb_textures[&c];
        assert_eq!((*w, *h), (16, 16));
        assert_ne!(tex.id(), first);
        assert_eq!(tex.size(), [16, 16]);
        assert_eq!(ctx.cache.frame_cache[&c].id(), tex.id());
        // Only the current size is held; the old texture is released with its handle.
        assert_eq!(ctx.cache.pb_textures.len(), 1);
        assert_eq!(egui_ctx.tex_manager().read().num_allocated(), base + 1);
    }

    #[test]
    fn playback_textures_stay_capped() {
        let egui_ctx = egui::Context::default();
        let mut ctx = ctx();
        let pixels = rgba(16, 8, 7);

        for n in 0..(MAX_PB_TEXTURES as u8 + 3) {
            upload_playback_texture(&mut ctx, &egui_ctx, clip(n), 16, 8, &pixels);
        }

        assert_eq!(ctx.cache.pb_textures.len(), MAX_PB_TEXTURES);
        // The clip uploaded last always keeps its texture, cap or no cap.
        let last = clip(MAX_PB_TEXTURES as u8 + 2);
        assert_eq!(
            ctx.cache.frame_cache[&last].id(),
            ctx.cache.pb_textures[&last].0.id()
        );
        let live: HashSet<_> = ctx
            .cache
            .pb_textures
            .values()
            .map(|(t, ..)| t.id())
            .collect();
        assert_eq!(live.len(), MAX_PB_TEXTURES);
    }
}
