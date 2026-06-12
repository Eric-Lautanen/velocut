// crates/velocut-media/src/worker/pb_thread.rs
//
// Playback decode thread: runs a state machine that decodes primary frames,
// handles transitions (blend), coast mode, and prebuffer.
// Extracted from MediaWorker::new() in worker.rs.

use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use uuid::Uuid;

use velocut_core::media_types::PlaybackFrame;

use crate::decode::LiveDecoder;

use super::blend::{ActiveBlend, blend_rgba_transition};
use super::types::PlaybackCmd;

pub(super) struct PbThread {
    pub cmd_rx: Receiver<PlaybackCmd>,
    pub frame_tx: Sender<PlaybackFrame>,
}

impl PbThread {
    pub fn run(self) {
        let Self { cmd_rx: pb_cmd_rx, frame_tx: pb_frame_tx } = self;

        let mut decoder: Option<(Uuid, LiveDecoder)> = None;
        // Active transition blend state.
        // Set by StartBlend, cleared by Start / Stop / primary-EOF / decoder_b-EOF.
        let mut blend: Option<ActiveBlend> = None;
        let mut frame_count: u64 = 0;
        // Last successfully blended output frame.
        // During decoder_b's skip_until_pts burn window, next_frame() returns None
        // (chunked, ~60 packets per call) and we need something to send. Repeating
        // the last blended frame ("frozen frame") is visually invisible at ~60ms.
        // The alternative — sending raw unblended primary — flashes the effect away.
        // Cleared on Start, Stop, primary-EOF, and StartBlend(invert_ab=false).
        // PRESERVED on StartBlend(invert_ab=true) — the clip_a outgoing phase left
        // alpha≈0.5 here, which is the correct frozen frame for the decoder_b burn window.
        let mut held_blend: Option<Vec<u8>> = None;
        // Diagnostics: count consecutive held_blend frames, log alpha of every blend frame.
        let mut held_streak: u32 = 0;
        let mut blend_frame_count: u64 = 0;
        // Coast mode: entered when primary EOF fires during an outgoing blend (invert_ab=false).
        // Instead of going idle (blocking on recv()), we keep sending held_blend frames at ~30fps
        // so pb_rx stays fed and the UI doesn't freeze while clip_changed fires.
        // The last-frame metadata fields provide the id/dims/ts for the synthetic coast frames.
        let mut coasting: bool = false;
        let mut coast_id: Uuid = Uuid::nil();
        let mut coast_w: u32 = 0;
        let mut coast_h: u32 = 0;
        let mut coast_ts: f64 = 0.0;
        // Last alpha value from the outgoing blend phase. Set when entering coast mode.
        // Used to correct alpha_start on the incoming StartBlend so the wipe/iris/etc.
        // resumes from where clip_a's decoder actually stopped, not from the hardcoded 0.5
        // midpoint — which causes a visible jump when clip_a's video runs a few frames short.
        let mut coast_last_alpha: f32 = 0.5;
        // Tracks the alpha of the most-recently produced blend frame so coast mode can
        // capture it when primary EOF fires.
        let mut last_blend_alpha: f32 = 0.0;
        // Last raw clip_a primary frame, saved during outgoing blend mode.
        // Used by the coast idle loop to generate real animated blend frames
        // instead of repeating held_blend frozen at the last alpha.
        // [Fix 3] Only cloned when we're in an active outgoing blend that has
        // reached blend_start_ts — previously cloned every frame in the blend zone
        // even before the visual transition started, wasting ~1.2 MB/frame.
        let mut coast_last_primary: Option<Vec<u8>> = None;
        // [P0-3] Pre-buffered decoder for the next clip.
        // Opened by PreBuffer, consumed by Start/StartBlend.
        // Burn is advanced incrementally between primary frames.
        let mut prebuffered: Option<(Uuid, LiveDecoder)> = None;
        loop {
            if let Some((id, ref mut d)) = decoder {
                match pb_cmd_rx.try_recv() {
                    Ok(PlaybackCmd::Start {
                        id: new_id,
                        path,
                        ts,
                        aspect,
                        preview_size,
                    }) => {
                        blend = None;
                        held_blend = None;
                        last_blend_alpha = 0.0;
                        coast_last_alpha = 0.5;
                        // [P0-3] Try prebuffered decoder first.
                        if let Some((pb_id, mut pb_dec)) = prebuffered.take() {
                            if pb_id == new_id {
                                if pb_dec.skip_until_pts > 0 {
                                    crate::media_log!("[pb] Start: prebuffer burn incomplete (skip_until_pts={}, last_pts={}), opening fresh", pb_dec.skip_until_pts, pb_dec.last_pts);
                                } else {
                                    let t0 = Instant::now();
                                    let tpts = pb_dec.ts_to_pts(ts);
                                    if tpts > pb_dec.last_pts {
                                        pb_dec.burn_to_pts(tpts);
                                    }
                                    crate::media_log!("[pb] Start (active): using prebuffered decoder, residual burn {}ms", t0.elapsed().as_millis());
                                    decoder = Some((new_id, pb_dec));
                                    continue;
                                }
                            }
                            // Wrong id — drop it and open fresh.
                        }
                        let t0 = Instant::now();
                        crate::media_log!("[pb] Start received (active), ts={ts:.3}");
                        match LiveDecoder::open(&path, ts, aspect, None, preview_size) {
                            Ok(mut nd) => {
                                let tpts = nd.ts_to_pts(ts);
                                nd.burn_to_pts(tpts);
                                crate::media_log!(
                                    "[pb] primary burn done in {}ms",
                                    t0.elapsed().as_millis()
                                );
                                decoder = Some((new_id, nd));
                            }
                            Err(e) => {
                                crate::media_log!("[pb] open: {e}");
                                decoder = None;
                            }
                        }
                        continue;
                    }
                    Ok(PlaybackCmd::StartBlend {
                        id: new_id,
                        path,
                        ts,
                        aspect,
                        blend: spec,
                        preview_size,
                    }) => {
                        let invert = spec.invert_ab;

                        let recycled_decoder_b = if invert {
                            let d = decoder.take().map(|(_, d)| d);
                            if let Some(ref db) = d {
                                crate::media_log!("[pb] recycling old primary as decoder_b: last_pts={} (ts≈{:.3}s)",
                                    db.last_pts, db.pts_to_secs(db.last_pts));
                            } else {
                                crate::media_log!("[pb] invert=true but no active decoder to recycle — will lazy-open");
                            }
                            d
                        } else {
                            None
                        };

                        blend = Some(ActiveBlend {
                            spec,
                            aspect,
                            decoder_b: recycled_decoder_b,
                        });
                        if !invert {
                            held_blend = None;
                        }
                        let t0 = Instant::now();
                        crate::media_log!("[pb] StartBlend received (active), ts={ts:.3}, recycled_decoder_b={invert}");
                        crate::media_log!("[pb] StartBlend spec: blend_start_ts={:.3} duration={:.3} alpha_start={:.3} invert={invert}",
                            blend.as_ref().map(|b| b.spec.blend_start_ts).unwrap_or(0.0),
                            blend.as_ref().map(|b| b.spec.duration as f64).unwrap_or(0.0),
                            blend.as_ref().map(|b| b.spec.alpha_start as f64).unwrap_or(0.0),
                        );
                        held_streak = 0;
                        blend_frame_count = 0;
                        // [P0-3] Try prebuffered decoder for the primary.
                        if let Some((pb_id, mut pb_dec)) = prebuffered.take() {
                            if pb_id == new_id {
                                if pb_dec.skip_until_pts > 0 {
                                    // Burn never reached the target — prebuffer was
                                    // advanced past EOF by interleaved try_advance_burn
                                    // calls during clip_a playback.  Using it as primary
                                    // would make next_frame() return None immediately,
                                    // causing the None handler to clear blend + decoder
                                    // and freeze the preview for clip_b's entire duration.
                                    crate::media_log!("[pb] StartBlend: prebuffer burn incomplete (skip_until_pts={}, last_pts={}), opening fresh", pb_dec.skip_until_pts, pb_dec.last_pts);
                                } else {
                                    let tpts = pb_dec.ts_to_pts(ts);
                                    if tpts > pb_dec.last_pts {
                                        pb_dec.burn_to_pts(tpts);
                                    }
                                    crate::media_log!("[pb] StartBlend (active): using prebuffered decoder, residual burn {}ms", t0.elapsed().as_millis());
                                    decoder = Some((new_id, pb_dec));
                                }
                            }
                            // Wrong id — drop and fall through to open fresh.
                        }
                        if decoder.is_none() {
                            match LiveDecoder::open(&path, ts, aspect, None, preview_size) {
                                Ok(mut nd) => {
                                    let tpts = nd.ts_to_pts(ts);
                                    nd.burn_to_pts(tpts);
                                    crate::media_log!(
                                        "[pb] primary burn done in {}ms",
                                        t0.elapsed().as_millis()
                                    );
                                    decoder = Some((new_id, nd));
                                }
                                Err(e) => {
                                    crate::media_log!("[pb] open (blend): {e}");
                                    decoder = None;
                                    blend = None;
                                }
                            }
                        }
                        if !invert {
                            let primary_size =
                                decoder.as_ref().map(|(_, d)| (d.out_w, d.out_h));
                            if let Some(ref mut b) = blend {
                                let db_path = b.spec.clip_b_path.clone();
                                let db_start = b.spec.clip_b_source_start;
                                let db_aspect = b.aspect;
                                let t_db = Instant::now();
                                crate::media_log!("[pb] pre-opening decoder_b for outgoing blend: clip_b_start={db_start:.3}");
                                match LiveDecoder::open(
                                    &db_path,
                                    db_start,
                                    db_aspect,
                                    None,
                                    primary_size,
                                ) {
                                    Ok(mut db) => {
                                        db.skip_until_pts = db.ts_to_pts(db_start);
                                        crate::media_log!("[pb] decoder_b pre-opened in {}ms, lazy burn started (skip_until_pts={})",
                                            t_db.elapsed().as_millis(), db.skip_until_pts);
                                        b.decoder_b = Some(db);
                                    }
                                    Err(e) => {
                                        crate::media_log!("[pb] decoder_b pre-open (outgoing): {e}")
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    Ok(PlaybackCmd::Stop) => {
                        decoder = None;
                        blend = None;
                        held_blend = None;
                        last_blend_alpha = 0.0;
                        coast_last_alpha = 0.5;
                        coast_last_primary = None;
                        prebuffered = None;
                        continue;
                    }
                    Ok(PlaybackCmd::PreBuffer {
                        id: pb_id,
                        path: pb_path,
                        ts: pb_ts,
                        aspect: pb_aspect,
                        preview_size: pb_ps,
                    }) => {
                        // [P0-3] Open decoder for the next clip and start lazy burn.
                        let t0 = Instant::now();
                        match LiveDecoder::open(&pb_path, pb_ts, pb_aspect, None, pb_ps) {
                            Ok(mut d) => {
                                d.skip_until_pts = d.ts_to_pts(pb_ts);
                                crate::media_log!("[pb] prebuffer opened for id={pb_id} ts={pb_ts:.3} in {}ms", t0.elapsed().as_millis());
                                prebuffered = Some((pb_id, d));
                            }
                            Err(e) => crate::media_log!("[pb] prebuffer open: {e}"),
                        }
                        // Fall through to decode next primary frame — don't continue.
                    }
                    Err(TryRecvError::Disconnected) => return,
                    Err(TryRecvError::Empty) => {}
                }
                // Decode next frame. send() blocks when channel is full —
                // that IS the rate-limiter, no sleep needed.
                match d.next_frame() {
                    Some((data, w, h, ts_secs)) => {
                        // Phase 1: read-only borrow to extract blend parameters.
                        let blend_params = {
                            blend.as_ref().and_then(|b| {
                                if ts_secs >= b.spec.blend_start_ts {
                                    let local_t = ts_secs - b.spec.blend_start_ts;
                                    let alpha   = (b.spec.alpha_start as f64 + local_t / b.spec.duration as f64)
                                        .clamp(0.0, 1.0) as f32;
                                    let db_state = match b.decoder_b.as_ref() {
                                        None     => "none",
                                        Some(db) if db.skip_until_pts > 0 => "burning",
                                        Some(_)  => "ready",
                                    };
                                    crate::media_log!("[blend] frame={blend_frame_count} ts={ts_secs:.3} local_t={local_t:.3} alpha={alpha:.3} db={db_state}");
                                    Some((b.spec.clip_b_path.clone(), b.spec.clip_b_source_start, alpha, b.spec.kind, b.aspect))
                                } else {
                                    None
                                }
                            })
                        };

                        if let Some((_, _, alpha, _, _)) = &blend_params {
                            last_blend_alpha = *alpha;
                        }

                        // [Fix 3] Only save coast_last_primary when blend_params is
                        // Some (we've reached blend_start_ts) and we're in an outgoing
                        // blend. Previously this cloned a full frame (~1.2 MB) every
                        // frame of the clip even before the visual transition began.
                        if blend_params.is_some()
                            && blend.as_ref().map(|b| !b.spec.invert_ab).unwrap_or(false)
                        {
                            coast_last_primary = Some(data.clone());
                        }

                        let blend_params = if blend_params
                            .as_ref()
                            .map(|(_, _, a, _, _)| *a >= 1.0)
                            .unwrap_or(false)
                        {
                            blend = None;
                            held_blend = None;
                            crate::media_log!(
                                "[blend] alpha=1.0 — transition complete, dropping blend"
                            );
                            None
                        } else {
                            blend_params
                        };

                        let mut decoder_b_exhausted = false;
                        let send_data = if let Some((
                            clip_b_path,
                            clip_b_start,
                            alpha,
                            kind,
                            decoder_b_aspect,
                        )) = blend_params
                        {
                            let blended = (|| -> Option<Vec<u8>> {
                                if let Some(b) = blend.as_mut() {
                                    let invert = b.spec.invert_ab;
                                    if b.decoder_b.is_none() {
                                        crate::media_log!("[blend] decoder_b is None — opening lazily, clip_b_start={clip_b_start:.3}");
                                        let t_open = Instant::now();
                                        let primary_size =
                                            decoder.as_ref().map(|(_, d)| (d.out_w, d.out_h));
                                        match LiveDecoder::open(
                                            &clip_b_path,
                                            clip_b_start,
                                            decoder_b_aspect,
                                            None,
                                            primary_size,
                                        ) {
                                            Ok(mut db) => {
                                                let tpts = db.ts_to_pts(clip_b_start);
                                                db.skip_until_pts = tpts;
                                                crate::media_log!("[blend] decoder_b opened in {}ms, skip_until_pts={tpts}", t_open.elapsed().as_millis());
                                                b.decoder_b = Some(db);
                                            }
                                            Err(e) => {
                                                crate::media_log!("[pb] blend decoder_b open: {e}")
                                            }
                                        }
                                    }
                                    if let Some(db) = b.decoder_b.as_mut() {
                                        // [P0-2] Non-blocking burn: advance decoder_b
                                        // incrementally (~10 packets ≈ 5ms) instead of
                                        // letting next_frame() block for 60 packets (~30ms).
                                        // This keeps primary frame production smooth during
                                        // decoder_b's GOP burn after lazy/pre-open.
                                        if db.skip_until_pts > 0 {
                                            let done = db.try_advance_burn(10);
                                            if !done {
                                                // Still burning — produce a held frame instead.
                                                let invert = b.spec.invert_ab;
                                                if invert {
                                                    if let Some(hb) = held_blend.as_ref() {
                                                        if hb.len() == data.len() {
                                                            return Some(
                                                                blend_rgba_transition(
                                                                    hb, &data, w, h, alpha,
                                                                    kind,
                                                                ),
                                                            );
                                                        }
                                                    }
                                                }
                                                return None;
                                            }
                                        }
                                        if let Some((data_b, wb, hb, _)) = db.next_frame() {
                                            if data_b.len() != data.len() || wb != w || hb != h
                                            {
                                                crate::media_log!(
                                                    "[pb] blend size mismatch — primary {}×{} ({} B) \
                                                     vs decoder_b {}×{} ({} B); skipping blend",
                                                    w, h, data.len(), wb, hb, data_b.len()
                                                );
                                                return None;
                                            }
                                            let blended = if invert {
                                                blend_rgba_transition(
                                                    &data_b, &data, w, h, alpha, kind,
                                                )
                                            } else {
                                                blend_rgba_transition(
                                                    &data, &data_b, w, h, alpha, kind,
                                                )
                                            };
                                            return Some(blended);
                                        } else {
                                            let still_burning = b
                                                .decoder_b
                                                .as_ref()
                                                .map(|db| db.skip_until_pts > 0)
                                                .unwrap_or(false);
                                            if still_burning {
                                                let db = b.decoder_b.as_ref().unwrap();
                                                crate::media_log!("[blend] still_burning: skip_until_pts={} last_pts={} gap_pts={}",
                                                    db.skip_until_pts, db.last_pts, db.skip_until_pts - db.last_pts);
                                                if invert {
                                                    if let Some(hb) = held_blend.as_ref() {
                                                        if hb.len() == data.len() {
                                                            crate::media_log!("[blend] still_burning animated: alpha={alpha:.3}");
                                                            return Some(
                                                                blend_rgba_transition(
                                                                    hb, &data, w, h, alpha,
                                                                    kind,
                                                                ),
                                                            );
                                                        }
                                                    }
                                                }
                                                return None;
                                            }
                                            decoder_b_exhausted = true;
                                        }
                                    }
                                }
                                None
                            })();
                            if decoder_b_exhausted {
                                blend = None;
                            }
                            // [Fix 2] Avoid double-allocation in the blend hot path.
                            // Previously: held_blend = Some(b.clone()); b
                            // — two Vec<u8> of equal size lived simultaneously.
                            // Now: move b into held_blend, clone once for the return.
                            // One allocation instead of two per blended frame.
                            match blended {
                                Some(b) => {
                                    if held_streak > 0 {
                                        crate::media_log!("[blend] held_blend streak ended after {held_streak} frames");
                                        held_streak = 0;
                                    }
                                    blend_frame_count += 1;
                                    let out = b.clone();
                                    held_blend = Some(b);
                                    out
                                }
                                None => {
                                    held_streak += 1;
                                    if held_streak == 1 {
                                        crate::media_log!("[blend] held_blend streak START (ts={ts_secs:.3} alpha from blend_params pending)");
                                    }
                                    held_blend.clone().unwrap_or(data)
                                }
                            }
                        } else {
                            data
                        };

                        coast_id = id;
                        coast_w = w;
                        coast_h = h;
                        coast_ts = ts_secs;
                        // Save last frame for hard-cut coast when prebuffered is ready.
                        // At EOF, this lets coast mode re-send the last frame of clip_a
                        // to keep the channel fed while waiting for Start.
                        if prebuffered.is_some() && blend.is_none() {
                            held_blend = Some(send_data.clone());
                        }
                        let f = PlaybackFrame {
                            id,
                            timestamp: ts_secs,
                            width: w,
                            height: h,
                            data: send_data,
                        };
                        frame_count += 1;
                        if frame_count.is_multiple_of(60) {
                            crate::media_log!("[pb] frame #{frame_count} sent, ts={ts_secs:.3}");
                        }
                        // Blocking send — rate-limits the decoder to the UI's
                        // consumption speed.  When the channel is full the pb thread
                        // sleeps here until poll_playback drains a frame, which also
                        // gives the UI thread a chance to send Stop/Start commands.
                        // The UI always drains before/after sending Stop, so this
                        // cannot deadlock.
                        if pb_frame_tx.send(f).is_err() {
                            return;
                        }
                        // [P0-3] Interleave prebuffer burn between primary frames.
                        // Advance by 10 packets (~5ms) per frame so the prebuffered
                        // decoder is ready by the time Start arrives (~15 frames later).
                        if let Some((_, ref mut pb_dec)) = prebuffered {
                            if pb_dec.skip_until_pts > 0 {
                                pb_dec.try_advance_burn(10);
                            }
                        }
                    }
                    None => {
                        let outgoing_blend =
                            blend.as_ref().map(|b| !b.spec.invert_ab).unwrap_or(false);
                        if outgoing_blend && held_blend.is_some() {
                            coast_last_alpha = last_blend_alpha;
                            crate::media_log!(
                                "[pb] primary EOF during outgoing blend — entering coast mode \
                                       (ts={coast_ts:.3}, alpha={coast_last_alpha:.3}, \
                                       decoder_b preserved for animated coast)"
                            );
                            coasting = true;
                            decoder = None;
                        } else if prebuffered.is_some() && held_blend.is_some() {
                            // Hard-cut coast: prebuffered decoder is ready for the next
                            // clip.  Enter coast mode to keep the channel fed with the
                            // last frame of clip_a (via held_blend) while waiting for
                            // the Start command from tick().  Coast uses try_recv() so
                            // the Start is picked up immediately — no blocking recv()
                            // delay.  The prebuffered decoder is consumed instantly by
                            // the Start handler.
                            blend = None;
                            coast_last_primary = None;
                            coasting = true;
                            decoder = None;
                            crate::media_log!(
                                "[pb] primary EOF → hard-cut coast (prebuffered ready, \
                                       held last frame for bridge)"
                            );
                        } else {
                            crate::media_log!("[pb] primary decoder EOF, clearing decoder + blend");
                            held_blend = None;
                            coast_last_primary = None;
                            blend = None;
                            decoder = None;
                        }
                    }
                }
            } else {
                // Idle branch: no primary decoder.
                let cmd_opt = if coasting {
                    match pb_cmd_rx.try_recv() {
                        Ok(cmd) => Some(cmd),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => return,
                    }
                } else {
                    match pb_cmd_rx.recv() {
                        Ok(cmd) => Some(cmd),
                        Err(_) => return,
                    }
                };

                if let Some(cmd) = cmd_opt {
                    let was_coasting = coasting;
                    coasting = false;
                    match cmd {
                        PlaybackCmd::Start {
                            id,
                            path,
                            ts,
                            aspect,
                            preview_size,
                        } => {
                            blend = None;
                            held_blend = None;
                            last_blend_alpha = 0.0;
                            coast_last_alpha = 0.5;
                            // [P0-3] Try prebuffered decoder first.
                            if let Some((pb_id, mut pb_dec)) = prebuffered.take() {
                                if pb_id == id {
                                    if pb_dec.skip_until_pts > 0 {
                                        crate::media_log!("[pb] Start (idle): prebuffer burn incomplete (skip_until_pts={}, last_pts={}), opening fresh", pb_dec.skip_until_pts, pb_dec.last_pts);
                                    } else {
                                        let t0 = Instant::now();
                                        let tpts = pb_dec.ts_to_pts(ts);
                                        if tpts > pb_dec.last_pts {
                                            pb_dec.burn_to_pts(tpts);
                                        }
                                        crate::media_log!("[pb] Start (idle): using prebuffered decoder, residual burn {}ms", t0.elapsed().as_millis());
                                        decoder = Some((id, pb_dec));
                                        continue;
                                    }
                                }
                            }
                            let t0 = Instant::now();
                            crate::media_log!("[pb] Start received (idle), ts={ts:.3}");
                            match LiveDecoder::open(&path, ts, aspect, None, preview_size) {
                                Ok(mut d) => {
                                    let tpts = d.ts_to_pts(ts);
                                    d.burn_to_pts(tpts);
                                    crate::media_log!(
                                        "[pb] primary burn done in {}ms",
                                        t0.elapsed().as_millis()
                                    );
                                    decoder = Some((id, d));
                                }
                                Err(e) => crate::media_log!("[pb] open: {e}"),
                            }
                        }
                        PlaybackCmd::StartBlend {
                            id,
                            path,
                            ts,
                            aspect,
                            blend: mut spec,
                            preview_size,
                        } => {
                            let invert = spec.invert_ab;

                            if invert && was_coasting {
                                crate::media_log!(
                                    "[pb] incoming StartBlend while coasting: overriding \
                                           alpha_start {:.3} → {:.3} (coast_last_alpha)",
                                    spec.alpha_start, coast_last_alpha
                                );
                                spec.alpha_start = coast_last_alpha;
                            }

                            let coast_blend_db = if was_coasting && invert {
                                blend.take().and_then(|b| b.decoder_b)
                            } else {
                                drop(blend.take());
                                None
                            };

                            let prebuilt_db = if was_coasting && invert {
                                let db_path = spec.clip_b_path.clone();
                                let db_start = spec.clip_b_source_start;
                                let t_db = Instant::now();
                                crate::media_log!("[pb] pre-opening decoder_b for clip_a tail at {db_start:.3}");
                                match LiveDecoder::open(
                                    &db_path,
                                    db_start,
                                    aspect,
                                    None,
                                    preview_size,
                                ) {
                                    Ok(mut db) => {
                                        let tpts = db.ts_to_pts(db_start);
                                        db.burn_to_pts(tpts);
                                        crate::media_log!(
                                            "[pb] decoder_b pre-burn done in {}ms",
                                            t_db.elapsed().as_millis()
                                        );
                                        Some(db)
                                    }
                                    Err(e) => {
                                        crate::media_log!("[pb] decoder_b pre-open: {e}");
                                        None
                                    }
                                }
                            } else {
                                None
                            };

                            let mut burn_ts = ts;

                            if let Some(mut bridge_db) = coast_blend_db {
                                coast_id = id;
                                let bridge_duration = spec.duration;
                                let bridge_kind = spec.kind;
                                let mut bridge_ts = ts;
                                // [Fix 1] BRIDGE_TARGET reduced from 28 → 4 to match
                                // the new channel size of 6. The old 28/32 fill ratio
                                // is preserved (4/6 ≈ 67%), ensuring the channel stays
                                // fed through the primary burn without flooding it.
                                const BRIDGE_TARGET: usize = 2; // 2/3 fill ratio matches old 4/6
                                while pb_frame_tx.len() < BRIDGE_TARGET {
                                    let fa = match coast_last_primary.as_ref() {
                                        Some(f) => f,
                                        None => break,
                                    };
                                    let (data_b, _, _, _) = match bridge_db.next_frame() {
                                        Some(f) => f,
                                        None => break,
                                    };
                                    if data_b.len() != (coast_w * coast_h * 4) as usize {
                                        break;
                                    }
                                    let step = (1.0_f32 / 30.0) / bridge_duration;
                                    coast_last_alpha = (coast_last_alpha + step).min(1.0);
                                    bridge_ts += 1.0 / 30.0;
                                    let blended = blend_rgba_transition(
                                        fa,
                                        &data_b,
                                        coast_w,
                                        coast_h,
                                        coast_last_alpha,
                                        bridge_kind,
                                    );
                                    crate::media_log!("[pb] bridge: ts={bridge_ts:.3} alpha={coast_last_alpha:.3} chan={}", pb_frame_tx.len());
                                    // [Fix 2] Move blended into held_blend, clone for send.
                                    // Previously: held_blend = Some(blended.clone()); ... data: blended
                                    // — two equal-sized allocations per bridge frame.
                                    let send = blended.clone();
                                    held_blend = Some(blended);
                                    let f = PlaybackFrame {
                                        id,
                                        timestamp: bridge_ts,
                                        width: coast_w,
                                        height: coast_h,
                                        data: send,
                                    };
                                    if pb_frame_tx.send(f).is_err() {
                                        return;
                                    }
                                }
                                spec.alpha_start = coast_last_alpha;
                                burn_ts = bridge_ts;
                                crate::media_log!("[pb] bridge done: alpha_start updated to {:.3}, burn_ts={burn_ts:.3}, chan_filled={}", spec.alpha_start, pb_frame_tx.len());
                            }

                            blend = Some(ActiveBlend {
                                spec,
                                aspect,
                                decoder_b: prebuilt_db,
                            });
                            if !invert {
                                held_blend = None;
                            }
                            // [P0-3] Try prebuffered decoder for the primary.
                            if let Some((pb_id, mut pb_dec)) = prebuffered.take() {
                                if pb_id == id {
                                    if pb_dec.skip_until_pts > 0 {
                                        crate::media_log!("[pb] StartBlend (idle): prebuffer burn incomplete (skip_until_pts={}, last_pts={}), opening fresh", pb_dec.skip_until_pts, pb_dec.last_pts);
                                    } else {
                                        let t0 = Instant::now();
                                        let tpts = pb_dec.ts_to_pts(burn_ts);
                                        if tpts > pb_dec.last_pts {
                                            pb_dec.burn_to_pts(tpts);
                                        }
                                        crate::media_log!("[pb] StartBlend (idle): using prebuffered decoder, residual burn {}ms", t0.elapsed().as_millis());
                                        decoder = Some((id, pb_dec));
                                    }
                                } else {
                                    // Wrong id — drop and open fresh below.
                                    prebuffered = None;
                                }
                            }
                            if decoder.is_none() {
                                let t0 = Instant::now();
                                crate::media_log!("[pb] StartBlend received (idle), ts={ts:.3} burn_ts={burn_ts:.3}");
                                match LiveDecoder::open(
                                    &path,
                                    burn_ts,
                                    aspect,
                                    None,
                                    preview_size,
                                ) {
                                    Ok(mut d) => {
                                        let tpts = d.ts_to_pts(burn_ts);
                                        d.burn_to_pts(tpts);
                                        crate::media_log!(
                                            "[pb] primary burn done in {}ms",
                                            t0.elapsed().as_millis()
                                        );
                                        decoder = Some((id, d));
                                    }
                                    Err(e) => {
                                        crate::media_log!("[pb] open (blend, idle): {e}");
                                        blend = None;
                                    }
                                }
                            }
                        }
                        PlaybackCmd::Stop => {
                            blend = None;
                            held_blend = None;
                            last_blend_alpha = 0.0;
                            coast_last_alpha = 0.5;
                            coast_last_primary = None;
                            prebuffered = None;
                        }
                        PlaybackCmd::PreBuffer {
                            id: pb_id,
                            path: pb_path,
                            ts: pb_ts,
                            aspect: pb_aspect,
                            preview_size: pb_ps,
                        } => {
                            // [P0-3] Open decoder for the next clip and start lazy burn.
                            let t0 = Instant::now();
                            match LiveDecoder::open(&pb_path, pb_ts, pb_aspect, None, pb_ps) {
                                Ok(mut d) => {
                                    d.skip_until_pts = d.ts_to_pts(pb_ts);
                                    crate::media_log!("[pb] prebuffer opened (idle) for id={pb_id} ts={pb_ts:.3} in {}ms", t0.elapsed().as_millis());
                                    prebuffered = Some((pb_id, d));
                                }
                                Err(e) => crate::media_log!("[pb] prebuffer open (idle): {e}"),
                            }
                            // Restore coast state — PreBuffer is a side-effect-only
                            // command that should not interrupt coast frame production.
                            coasting = was_coasting;
                        }
                    }
                } else {
                    // Coast mode, no command yet.
                    // Produce an animated blend frame or repeat held_blend.
                    let animated = (|| -> Option<Vec<u8>> {
                        let b = blend.as_mut()?;
                        let fa = coast_last_primary.as_ref()?;
                        let db = b.decoder_b.as_mut()?;
                        let (data_b, _, _, _) = db.next_frame()?;
                        if data_b.len() != fa.len() {
                            return None;
                        }
                        let step = (1.0_f32 / 30.0) / b.spec.duration;
                        coast_last_alpha = (coast_last_alpha + step).min(1.0);
                        coast_ts += 1.0 / 30.0;
                        Some(blend_rgba_transition(
                            fa,
                            &data_b,
                            coast_w,
                            coast_h,
                            coast_last_alpha,
                            b.spec.kind,
                        ))
                    })();

                    let send_data = if let Some(blended) = animated {
                        crate::media_log!("[pb] coast animated: ts={coast_ts:.3} alpha={coast_last_alpha:.3}");
                        // [Fix 2] Move into held_blend, clone for send.
                        let out = blended.clone();
                        held_blend = Some(blended);
                        Some(out)
                    } else {
                        held_blend.clone()
                    };

                    if let Some(data) = send_data {
                        let f = PlaybackFrame {
                            id: coast_id,
                            timestamp: coast_ts,
                            width: coast_w,
                            height: coast_h,
                            data,
                        };
                        // Blocking send — rate-limits coast to UI consumption speed.
                        if pb_frame_tx.send(f).is_err() {
                            return;
                        }
                    } else {
                        crate::media_log!(
                            "[pb] coast: both animated and held_blend None — exiting coast"
                        );
                        coasting = false;
                    }
                }
            }
        }
    }
}
