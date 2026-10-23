// crates/velocut-media/src/worker/pb_thread.rs
//
// Playback decode thread: runs a state machine that decodes primary frames,
// handles transitions (blend), coast mode, and prebuffer.
// Extracted from MediaWorker::new() in worker.rs.

use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use uuid::Uuid;

use velocut_core::media_types::{FrameBuf, PlaybackFrame, PlaybackTransitionSpec};

use crate::decode::LiveDecoder;

use super::blend::{blend_rgba_transition_into, rotate_blended_frame, ActiveBlend};
use super::types::PlaybackCmd;

/// Alpha the incoming half of a centred transition starts from.
///
/// A transition is rendered in two halves: the outgoing clip blends out while
/// its primary decoder is live, then the primary switches to the incoming clip
/// and the blend continues from the cut to the end of the zone. The second half
/// is built with a nominal `alpha_start` of 0.5, which is only exactly right when
/// the switch lands on the cut frame. Real handoffs land a frame or two either
/// side — decode latency, a held frame during decoder_b's burn — and restarting
/// at the nominal value then snaps the effect forwards or backwards on that one
/// frame: the iris radius pops, the wipe bar jumps, the push slides back.
///
/// Continuing from the alpha of the frame that is actually on screen keeps the
/// motion monotonic. `last_alpha <= 0.0` means no outgoing blend frame was ever
/// produced, so the nominal midpoint is the right start.
pub(super) fn incoming_alpha_start(nominal: f32, last_alpha: f32) -> f32 {
    if !last_alpha.is_finite() || last_alpha <= 0.0 {
        return nominal;
    }
    // Never hand over a finished blend: 1.0 would make the incoming half a
    // one-frame no-op and drop the effect early.
    last_alpha.clamp(0.0, 0.999)
}

/// Whether an incoming-half StartBlend continues the *same transition zone*
/// whose outgoing half (or coast over it) is currently recorded in `prev`.
///
/// The handoff carry (`last_blend_alpha`) and the coast bridge run on state
/// that is loop-local, not keyed to a zone — `last_blend_alpha`,
/// `blend_frame_count`, `held_blend`, `coast_last_alpha/primary`. Those survive
/// sessions, so an incoming StartBlend from a DIFFERENT zone (possible when a
/// Stop command was dropped, or when a session ended without a Stop) must not
/// carry them: a foreign zone's alpha makes the incoming half start
/// near-complete and visibly collapses the effect. Callers reset the
/// continuity state in that case instead.
///
/// Same-zone identity: the recorded blend must be an outgoing half
/// (`invert_ab == false`), its clip_b must be this command's primary clip (both
/// halves of a zone name the same incoming clip), and the transition must
/// agree on duration and kind — both halves compute those from the same
/// `clamped_secs` call, so equality means the same zone.
pub(super) fn continues_same_zone(
    prev: Option<&ActiveBlend>,
    cmd_id: Uuid,
    spec: &PlaybackTransitionSpec,
) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    !prev.spec.invert_ab
        && prev.spec.clip_b_id == cmd_id
        && prev.spec.duration == spec.duration
        && prev.spec.kind == spec.kind
}

/// Seconds of transition time one coast frame covers, from the real interval
/// between coast frames. The lower bound keeps an ultra-high-refresh drain from
/// stalling the blend; the upper bound means a long UI stall costs at most a
/// tenth of a second of blend instead of jumping the effect to its end.
pub(super) fn coast_frame_dt(dt_secs: f64) -> f64 {
    if !dt_secs.is_finite() {
        return 1.0 / 30.0;
    }
    dt_secs.clamp(1.0 / 240.0, 0.1)
}

/// Collect the buffers the UI has handed back, up to the pool cap (anything
/// beyond it is dropped rather than kept forever after a resolution change).
fn refill_pool(pool: &mut Vec<Vec<u8>>, recycle_rx: &Receiver<Vec<u8>>) {
    while pool.len() < FRAME_POOL_MAX {
        match recycle_rx.try_recv() {
            Ok(buf) => pool.push(buf),
            Err(_) => return,
        }
    }
    while recycle_rx.try_recv().is_ok() {}
}

/// How many returned frame buffers the playback thread keeps. The channel holds
/// six frames and the UI holds one pending, so this covers everything in flight.
const FRAME_POOL_MAX: usize = 6;

pub(super) struct PbThread {
    pub cmd_rx: Receiver<PlaybackCmd>,
    pub frame_tx: Sender<PlaybackFrame>,
    /// Buffers of frames the UI has finished with.
    pub recycle_rx: Receiver<Vec<u8>>,
    /// Stamped onto every sent frame so its buffer finds its way back here.
    pub recycle: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
}

impl PbThread {
    pub fn run(self) {
        let Self {
            cmd_rx: pb_cmd_rx,
            frame_tx: pb_frame_tx,
            recycle_rx: pb_recycle_rx,
            recycle: pb_recycle,
        } = self;

        let mut decoder: Option<(Uuid, LiveDecoder)> = None;
        // Playback-session generation. Bumped on every Start / StartBlend by the
        // worker and carried in on the command; stamped onto every frame this
        // thread sends so the UI can reject frames still in flight from an older
        // session (they would otherwise slip past the wrong_clip / too_old
        // windows at a session boundary and flash a wrong-spec blend).
        let mut generation: u64 = 0;
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
        // Scratch destination for decoder_b's frames. decoder_b is read once per
        // blended frame; without this it allocates a 5 MB frame per frame, faults
        // and all.
        let mut decoder_b_buf: Vec<u8> = Vec::new();
        // Scratch destination for the next blend. Kept alive across frames so the
        // blend writes into already-faulted pages instead of a fresh 5 MB buffer
        // every time — see `blend::rotate_blended_frame`.
        let mut blend_out: Vec<u8> = Vec::new();
        // Buffers the UI has handed back, ready to be written into again.
        let mut frame_pool: Vec<Vec<u8>> = Vec::new();
        // Diagnostics: count consecutive held_blend frames, log alpha of every blend frame.
        let mut held_streak: u32 = 0;
        let mut blend_frame_count: u64 = 0;
        // Coast mode: entered when primary EOF fires during an outgoing blend (invert_ab=false).
        // Instead of going idle (blocking on recv()), we keep sending held_blend frames at ~30fps
        // so pb_rx stays fed and the UI doesn't freeze while clip_changed fires.
        // The last-frame metadata fields provide the id/dims/ts for the synthetic coast frames.
        let mut coasting: bool = false;
        // Wall clock of the previous coast frame. Coast frames are produced at the
        // rate the UI drains them (display refresh), not at the 30 fps timeline
        // rate, so the blend must advance by real elapsed time.
        let mut coast_clock: Option<Instant> = None;
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
                        gen,
                    }) => {
                        generation = gen;
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
                        blend: mut spec,
                        preview_size,
                        gen,
                    }) => {
                        generation = gen;
                        let invert = spec.invert_ab;

                        // Phase handoff of a centred transition: the outgoing half is
                        // still live and has already put a blended frame on screen.
                        // Continue from that frame's alpha instead of snapping back to
                        // the nominal midpoint (see `incoming_alpha_start`) — but only
                        // for the SAME transition zone (see `continues_same_zone`).
                        // The continuity state survives sessions, so a StartBlend
                        // from a different zone resets it instead of carrying it.
                        let same_zone = continues_same_zone(blend.as_ref(), new_id, &spec);
                        if invert && same_zone {
                            let outgoing_frames = blend
                                .as_ref()
                                .filter(|b| !b.spec.invert_ab)
                                .map(|_| blend_frame_count)
                                .unwrap_or(0);
                            if outgoing_frames > 0 {
                                let carried =
                                    incoming_alpha_start(spec.alpha_start, last_blend_alpha);
                                crate::media_log!(
                                    "[pb] handoff: alpha_start {:.3} → {:.3} carried from last outgoing frame ({} frames)",
                                    spec.alpha_start,
                                    carried,
                                    outgoing_frames
                                );
                                spec.alpha_start = carried;
                            }
                        } else if !same_zone {
                            crate::media_log!(
                                "[pb] StartBlend continues a different zone — resetting continuity \
                                 (last_blend_alpha={last_blend_alpha:.3}, blend_frame_count={blend_frame_count}, invert={invert})"
                            );
                            last_blend_alpha = 0.0;
                            coast_last_alpha = 0.5;
                            // blend_frame_count is reset unconditionally later in
                            // this arm; no separate write here.
                            held_blend = None;
                            coast_last_primary = None;
                            coast_clock = None;
                        }

                        // The outgoing half's live primary decoder is only worth
                        // recycling as clip_a's tail decoder for the same zone;
                        // for a different zone it is simply discarded (and a new
                        // decoder_b is lazy-opened against the new spec).
                        let recycled_decoder_b = if invert && same_zone {
                            let d = decoder.take().map(|(_, d)| d);
                            if let Some(ref db) = d {
                                crate::media_log!("[pb] recycling old primary as decoder_b: last_pts={} (ts≈{:.3}s)",
                                    db.last_pts, db.pts_to_secs(db.last_pts));
                            } else {
                                crate::media_log!("[pb] invert=true but no active decoder to recycle — will lazy-open");
                            }
                            d
                        } else {
                            if invert {
                                crate::media_log!(
                                    "[pb] different zone — dropping the old primary instead of recycling it as decoder_b"
                                );
                            }
                            if invert {
                                decoder.take();
                            }
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
                            let primary_size = decoder.as_ref().map(|(_, d)| (d.out_w, d.out_h));
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
                                crate::media_log!(
                                    "[pb] prebuffer opened for id={pb_id} ts={pb_ts:.3} in {}ms",
                                    t0.elapsed().as_millis()
                                );
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
                // Decode into a buffer the UI has finished with, when one is
                // available: a fresh 5 MB frame costs ~1230 page faults to write,
                // which is real time on every single frame. `next_frame` would
                // allocate one per frame.
                refill_pool(&mut frame_pool, &pb_recycle_rx);
                let mut decoded_into = frame_pool.pop().unwrap_or_default();
                let decoded = d.next_frame_into(&mut decoded_into);
                match decoded {
                    Some((w, h, ts_secs)) => {
                        let data = decoded_into;
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
                            // Copy into the existing buffer rather than allocating one
                            // per frame: this runs on every frame of an outgoing blend.
                            match coast_last_primary.as_mut() {
                                Some(buf) if buf.len() == data.len() => buf.copy_from_slice(&data),
                                _ => coast_last_primary = Some(data.clone()),
                            }
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
                            let blended = (|| -> Option<()> {
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
                                                            blend_rgba_transition_into(
                                                                hb,
                                                                &data,
                                                                w,
                                                                h,
                                                                alpha,
                                                                kind,
                                                                &mut blend_out,
                                                            );
                                                            return Some(());
                                                        }
                                                    }
                                                }
                                                return None;
                                            }
                                        }
                                        let decoded_b = db.next_frame_into(&mut decoder_b_buf);
                                        if let Some((wb, hb, _)) = decoded_b {
                                            let data_b = decoder_b_buf.as_slice();
                                            if data_b.len() != data.len() || wb != w || hb != h {
                                                crate::media_log!(
                                                    "[pb] blend size mismatch — primary {}×{} ({} B) \
                                                     vs decoder_b {}×{} ({} B); skipping blend",
                                                    w, h, data.len(), wb, hb, data_b.len()
                                                );
                                                return None;
                                            }
                                            if invert {
                                                blend_rgba_transition_into(
                                                    data_b,
                                                    &data,
                                                    w,
                                                    h,
                                                    alpha,
                                                    kind,
                                                    &mut blend_out,
                                                );
                                            } else {
                                                blend_rgba_transition_into(
                                                    &data,
                                                    data_b,
                                                    w,
                                                    h,
                                                    alpha,
                                                    kind,
                                                    &mut blend_out,
                                                );
                                            }
                                            return Some(());
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
                                                            blend_rgba_transition_into(
                                                                hb,
                                                                &data,
                                                                w,
                                                                h,
                                                                alpha,
                                                                kind,
                                                                &mut blend_out,
                                                            );
                                                            return Some(());
                                                        }
                                                    }
                                                }
                                                return None;
                                            }
                                            // decoder_b returned None but isn't burning.
                                            // Check if it's truly exhausted or just needs more time.
                                            // If the primary decoder is still producing frames,
                                            // keep the blend alive and retry next frame.
                                            let db = b.decoder_b.as_ref().unwrap();
                                            crate::media_log!("[blend] decoder_b next_frame returned None, skip_until_pts=0, last_pts={}", db.last_pts);
                                            decoder_b_exhausted = true;
                                        }
                                    }
                                }
                                None
                            })();
                            if decoder_b_exhausted {
                                blend = None;
                            }
                            // The frame was blended into `blend_out`; rotate it into
                            // `held_blend` and clone once for the channel. The clone is
                            // the only allocation per blended frame, and the buffer it
                            // replaces goes back to `blend_out` — so both long-lived
                            // buffers stay warm and the blend never faults pages in.
                            // Collect whatever the UI has finished with before making
                            // the outgoing copy, so the copy writes into a warm buffer.
                            refill_pool(&mut frame_pool, &pb_recycle_rx);
                            match blended {
                                Some(()) => {
                                    if held_streak > 0 {
                                        crate::media_log!("[blend] held_blend streak ended after {held_streak} frames");
                                        held_streak = 0;
                                    }
                                    blend_frame_count += 1;
                                    rotate_blended_frame(
                                        &mut blend_out,
                                        &mut held_blend,
                                        frame_pool.pop().unwrap_or_default(),
                                    )
                                }
                                None => {
                                    held_streak += 1;
                                    if held_streak == 1 {
                                        crate::media_log!("[blend] held_blend streak START (ts={ts_secs:.3} alpha from blend_params pending)");
                                    }
                                    // Repeat the last blended frame from a recycled
                                    // buffer instead of allocating a copy of it.
                                    match held_blend.as_ref() {
                                        Some(hb) => {
                                            refill_pool(&mut frame_pool, &pb_recycle_rx);
                                            let mut out = frame_pool.pop().unwrap_or_default();
                                            out.clear();
                                            out.extend_from_slice(hb);
                                            out
                                        }
                                        None => data,
                                    }
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
                            // Keep the last frame for the hard-cut coast, but copy into
                            // the buffer already held: this runs on every frame while a
                            // prebuffer is open, so an allocation here is per-frame.
                            match held_blend.as_mut() {
                                Some(buf) if buf.len() == send_data.len() => {
                                    buf.copy_from_slice(&send_data)
                                }
                                _ => held_blend = Some(send_data.clone()),
                            }
                        }
                        let f = PlaybackFrame {
                            id,
                            timestamp: ts_secs,
                            width: w,
                            height: h,
                            data: FrameBuf::recycled(send_data, pb_recycle.clone()),
                            generation,
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
                        // EOF or a burn in progress: keep the decode buffer for the
                        // next frame instead of dropping it back to the allocator.
                        if decoded_into.capacity() > 0 {
                            frame_pool.push(decoded_into);
                        }
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
                            coast_clock = None;
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
                            gen,
                        } => {
                            generation = gen;
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
                            gen,
                        } => {
                            generation = gen;
                            let invert = spec.invert_ab;

                            // Same continuity rule as the active branch: an outgoing
                            // blend that EOF'd without entering coast still left a frame
                            // on screen at `last_blend_alpha` — but only for the SAME
                            // transition zone (see `continues_same_zone`). A StartBlend
                            // from a different zone resets the continuity state instead
                            // of carrying it, and must not ride the old zone's coast.
                            let same_zone = continues_same_zone(blend.as_ref(), id, &spec);
                            if invert && same_zone && !was_coasting {
                                let outgoing_frames = blend
                                    .as_ref()
                                    .filter(|b| !b.spec.invert_ab)
                                    .map(|_| blend_frame_count)
                                    .unwrap_or(0);
                                if outgoing_frames > 0 {
                                    let carried =
                                        incoming_alpha_start(spec.alpha_start, last_blend_alpha);
                                    crate::media_log!(
                                        "[pb] handoff (idle): alpha_start {:.3} → {:.3} carried from last outgoing frame ({} frames)",
                                        spec.alpha_start,
                                        carried,
                                        outgoing_frames
                                    );
                                    spec.alpha_start = carried;
                                }
                            }

                            if invert && same_zone && was_coasting {
                                crate::media_log!(
                                    "[pb] incoming StartBlend while coasting: overriding \
                                           alpha_start {:.3} → {:.3} (coast_last_alpha)",
                                    spec.alpha_start,
                                    coast_last_alpha
                                );
                                spec.alpha_start = coast_last_alpha;
                            }

                            if !same_zone {
                                crate::media_log!(
                                    "[pb] StartBlend continues a different zone — resetting continuity \
                                     (last_blend_alpha={last_blend_alpha:.3}, blend_frame_count={blend_frame_count}, was_coasting={was_coasting}, invert={invert})"
                                );
                                last_blend_alpha = 0.0;
                                coast_last_alpha = 0.5;
                                blend_frame_count = 0;
                                held_blend = None;
                                coast_last_primary = None;
                                coast_clock = None;
                            }

                            let coast_blend_db = if was_coasting && invert && same_zone {
                                blend.take().and_then(|b| b.decoder_b)
                            } else {
                                drop(blend.take());
                                None
                            };

                            let prebuilt_db = if was_coasting && invert && same_zone {
                                let db_path = spec.clip_b_path.clone();
                                let db_start = spec.clip_b_source_start;
                                let t_db = Instant::now();
                                crate::media_log!(
                                    "[pb] pre-opening decoder_b for clip_a tail at {db_start:.3}"
                                );
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
                                                                // decoder_b's frame for the bridge, reused across the
                                                                // handful of frames the bridge produces.
                                let mut bridge_buf: Vec<u8> = Vec::new();
                                while pb_frame_tx.len() < BRIDGE_TARGET {
                                    let fa = match coast_last_primary.as_ref() {
                                        Some(f) => f,
                                        None => break,
                                    };
                                    if bridge_db.next_frame_into(&mut bridge_buf).is_none() {
                                        break;
                                    }
                                    if bridge_buf.len() != (coast_w * coast_h * 4) as usize {
                                        break;
                                    }
                                    let step = (1.0_f32 / 30.0) / bridge_duration;
                                    coast_last_alpha = (coast_last_alpha + step).min(1.0);
                                    bridge_ts += 1.0 / 30.0;
                                    blend_rgba_transition_into(
                                        fa,
                                        &bridge_buf,
                                        coast_w,
                                        coast_h,
                                        coast_last_alpha,
                                        bridge_kind,
                                        &mut blend_out,
                                    );
                                    crate::media_log!("[pb] bridge: ts={bridge_ts:.3} alpha={coast_last_alpha:.3} chan={}", pb_frame_tx.len());
                                    // Same rotation as the main blend path: the bridge
                                    // frame goes into held_blend, its predecessor's buffer
                                    // becomes the next blend destination, and the copy handed
                                    // to the channel reuses a buffer the UI returned.
                                    refill_pool(&mut frame_pool, &pb_recycle_rx);
                                    let send = rotate_blended_frame(
                                        &mut blend_out,
                                        &mut held_blend,
                                        frame_pool.pop().unwrap_or_default(),
                                    );
                                    let f = PlaybackFrame {
                                        id,
                                        timestamp: bridge_ts,
                                        width: coast_w,
                                        height: coast_h,
                                        data: FrameBuf::recycled(send, pb_recycle.clone()),
                                        generation,
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
                                match LiveDecoder::open(&path, burn_ts, aspect, None, preview_size)
                                {
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
                    let animated = (|| -> Option<()> {
                        let b = blend.as_mut()?;
                        let fa = coast_last_primary.as_ref()?;
                        let db = b.decoder_b.as_mut()?;
                        // Same buffers as the live blend path: decoder_b's frame in,
                        // the blended frame out. Coast frames arrive at display rate,
                        // so allocating two 5 MB frames each was pure churn.
                        db.next_frame_into(&mut decoder_b_buf)?;
                        if decoder_b_buf.len() != fa.len() {
                            return None;
                        }
                        // Advance by wall-clock time: a fixed 1/30 per frame made
                        // the effect crawl on high-refresh displays (twice as many
                        // frames drained per second → alpha advanced at half the
                        // intended rate) and let coast_ts drift behind the playhead.
                        let now = Instant::now();
                        let dt = coast_frame_dt(
                            coast_clock
                                .map(|t| (now - t).as_secs_f64())
                                .unwrap_or(1.0 / 30.0),
                        );
                        coast_clock = Some(now);
                        let step = (dt / b.spec.duration as f64) as f32;
                        coast_last_alpha = (coast_last_alpha + step).min(1.0);
                        coast_ts += dt;
                        blend_rgba_transition_into(
                            fa,
                            &decoder_b_buf,
                            coast_w,
                            coast_h,
                            coast_last_alpha,
                            b.spec.kind,
                            &mut blend_out,
                        );
                        Some(())
                    })();

                    let send_data = if animated.is_some() {
                        crate::media_log!(
                            "[pb] coast animated: ts={coast_ts:.3} alpha={coast_last_alpha:.3}"
                        );
                        refill_pool(&mut frame_pool, &pb_recycle_rx);
                        Some(rotate_blended_frame(
                            &mut blend_out,
                            &mut held_blend,
                            frame_pool.pop().unwrap_or_default(),
                        ))
                    } else {
                        // No animated frame: re-send the last one from a recycled
                        // buffer rather than allocating a copy per coast frame.
                        match held_blend.as_ref() {
                            Some(hb) => {
                                refill_pool(&mut frame_pool, &pb_recycle_rx);
                                let mut out = frame_pool.pop().unwrap_or_default();
                                out.clear();
                                out.extend_from_slice(hb);
                                Some(out)
                            }
                            None => None,
                        }
                    };

                    if let Some(data) = send_data {
                        let f = PlaybackFrame {
                            id: coast_id,
                            timestamp: coast_ts,
                            width: coast_w,
                            height: coast_h,
                            data: FrameBuf::recycled(data, pb_recycle.clone()),
                            generation,
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

#[cfg(test)]
mod handoff_tests {
    use super::incoming_alpha_start;

    #[test]
    fn no_outgoing_frames_keeps_the_nominal_midpoint() {
        assert_eq!(incoming_alpha_start(0.5, 0.0), 0.5);
    }

    #[test]
    fn continues_from_the_frame_that_is_already_on_screen() {
        // Frames ran a little ahead of the cut: keep going, don't rewind.
        assert!((incoming_alpha_start(0.5, 0.58) - 0.58).abs() < 1e-6);
        // Frames lagged behind the cut: same rule, no forward snap either.
        assert!((incoming_alpha_start(0.5, 0.42) - 0.42).abs() < 1e-6);
    }

    #[test]
    fn never_hands_over_a_finished_blend() {
        assert!((incoming_alpha_start(0.5, 1.0) - 0.999).abs() < 1e-6);
    }

    #[test]
    fn rejects_non_finite_alpha() {
        assert_eq!(incoming_alpha_start(0.5, f32::NAN), 0.5);
    }
}

#[cfg(test)]
mod same_zone_tests {
    //! Pins the handoff-carry identity gate for the idle/active StartBlend
    //! branches (see the audit's sessionless-carry finding): carry only when
    //! the incoming half continues the same transition zone, otherwise reset.

    use super::*;
    use std::path::PathBuf;
    use velocut_core::transitions::TransitionKind;

    fn outgoing_blend(zone_b: u128, duration: f64, kind: TransitionKind) -> ActiveBlend {
        ActiveBlend {
            spec: PlaybackTransitionSpec {
                clip_b_id: Uuid::from_u128(zone_b),
                clip_b_path: PathBuf::new(),
                clip_b_source_start: 0.0,
                blend_start_ts: 10.0,
                duration: duration as f32,
                kind,
                alpha_start: 0.0,
                invert_ab: false,
            },
            aspect: 1.0,
            decoder_b: None, // tests never touch the decoder
        }
    }

    fn incoming_cmd_id(zone_b: u128) -> Uuid {
        Uuid::from_u128(zone_b)
    }

    fn incoming_spec(duration: f64, kind: TransitionKind) -> PlaybackTransitionSpec {
        PlaybackTransitionSpec {
            clip_b_id: Uuid::nil(), // the outgoing clip — irrelevant to the gate
            clip_b_path: PathBuf::new(),
            clip_b_source_start: 0.0,
            blend_start_ts: 20.0,
            duration: duration as f32,
            kind,
            alpha_start: 0.5,
            invert_ab: true,
        }
    }

    #[test]
    fn the_zone_incoming_half_continues_the_recorded_outgoing_state() {
        // Normal handoff: the outgoing half is live, both halves name the same
        // incoming clip and agree on the clamped duration + kind.
        let prev = outgoing_blend(0xA001, 1.0, TransitionKind::Iris);
        let spec = incoming_spec(1.0, TransitionKind::Iris);
        assert!(continues_same_zone(
            Some(&prev),
            incoming_cmd_id(0xA001),
            &spec
        ));
    }

    #[test]
    fn a_startblend_for_another_clip_is_a_different_zone() {
        // The dropped-Stop scenario: a fresh incoming StartBlend whose primary
        // clip is not the old zone's incoming clip must not carry alpha.
        let prev = outgoing_blend(0xA001, 1.0, TransitionKind::Iris);
        let spec = incoming_spec(1.0, TransitionKind::Iris);
        assert!(!continues_same_zone(
            Some(&prev),
            incoming_cmd_id(0xB002),
            &spec
        ));
    }

    #[test]
    fn a_startblend_of_different_duration_or_kind_is_a_different_zone() {
        // Compensations changed the effective transition — identity requires
        // an exact match on both, they come from the same clamped computation.
        let a = outgoing_blend(0xA001, 1.0, TransitionKind::Iris);
        assert!(!continues_same_zone(
            Some(&a),
            incoming_cmd_id(0xA001),
            &incoming_spec(0.5, TransitionKind::Iris)
        ));
        assert!(!continues_same_zone(
            Some(&a),
            incoming_cmd_id(0xA001),
            &incoming_spec(1.0, TransitionKind::Wipe)
        ));
    }

    #[test]
    fn only_an_outgoing_half_plays_the_handoff_role() {
        // A recorded incoming blend can never hand alpha to anyone — and an
        // incoming StartBlend is not a continuation target in either branch.
        let mut prev = outgoing_blend(0xA001, 1.0, TransitionKind::Iris);
        prev.spec.invert_ab = true;
        assert!(!continues_same_zone(
            Some(&prev),
            incoming_cmd_id(0xA001),
            &incoming_spec(1.0, TransitionKind::Iris)
        ));
    }

    #[test]
    fn no_recorded_blend_cannot_continue_anything() {
        assert!(!continues_same_zone(
            None,
            incoming_cmd_id(0xA001),
            &incoming_spec(1.0, TransitionKind::Iris)
        ));
    }
}

#[cfg(test)]
mod coast_dt_tests {
    use super::coast_frame_dt;

    #[test]
    fn sixty_hertz_drain_advances_half_a_frame_per_frame() {
        // 1/60 s of transition time per coast frame, not 1/30 — this is the
        // double-speed bug the wall clock fixes.
        assert!((coast_frame_dt(1.0 / 60.0) - 1.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn a_stall_costs_at_most_a_tenth_of_a_second() {
        assert!((coast_frame_dt(5.0) - 0.1).abs() < 1e-9);
    }

    #[test]
    fn sub_frame_intervals_are_floored() {
        assert!((coast_frame_dt(0.0) - 1.0 / 240.0).abs() < 1e-9);
    }

    #[test]
    fn non_finite_interval_falls_back_to_a_thirtieth() {
        assert!((coast_frame_dt(f64::NAN) - 1.0 / 30.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod frame_pool_tests {
    use super::{refill_pool, FRAME_POOL_MAX};
    use crossbeam_channel::bounded;

    #[test]
    fn returned_buffers_fill_the_pool_up_to_its_cap() {
        let (tx, rx) = bounded::<Vec<u8>>(16);
        let mut pool: Vec<Vec<u8>> = Vec::new();
        for i in 0..(FRAME_POOL_MAX + 3) {
            tx.send(vec![i as u8; 8]).unwrap();
        }

        refill_pool(&mut pool, &rx);
        assert_eq!(pool.len(), FRAME_POOL_MAX, "pool must stay bounded");
        // Surplus returns are dropped, not queued for later.
        assert!(rx.is_empty(), "channel must be drained");

        // Draining again is a no-op rather than an error.
        refill_pool(&mut pool, &rx);
        assert_eq!(pool.len(), FRAME_POOL_MAX);
    }

    #[test]
    fn an_empty_channel_leaves_the_pool_alone() {
        let (_tx, rx) = bounded::<Vec<u8>>(4);
        let mut pool: Vec<Vec<u8>> = vec![vec![1, 2, 3]];
        refill_pool(&mut pool, &rx);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool[0], vec![1, 2, 3]);
    }

    #[test]
    fn a_disconnected_pool_does_not_spin() {
        let (tx, rx) = bounded::<Vec<u8>>(4);
        tx.send(vec![9; 4]).unwrap();
        drop(tx);
        let mut pool: Vec<Vec<u8>> = Vec::new();
        refill_pool(&mut pool, &rx);
        assert_eq!(pool.len(), 1);
        refill_pool(&mut pool, &rx);
        assert_eq!(pool.len(), 1);
    }
}
