// crates/velocut-media/src/worker.rs
//
// MediaWorker: owns the frame-request slot and playback decode thread.
// All public API that velocut-ui calls lives here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Condvar, atomic::{AtomicBool, Ordering}};
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use uuid::Uuid;

use velocut_core::media_types::{MediaResult, PlaybackFrame, PlaybackTransitionSpec, TransitionScrubRequest};

use crate::decode::{LiveDecoder, decode_frame, decode_one_frame_rgba};
use crate::encode::{EncodeSpec, encode_timeline};
use crate::probe::{probe_duration, probe_video_size_and_thumbnail};
use crate::waveform::extract_waveform;
use crate::audio::extract_audio;

// ── Internal types ────────────────────────────────────────────────────────────

struct FrameRequest {
    id:        Uuid,
    path:      PathBuf,
    timestamp: f64,
    aspect:    f32,
}

enum PlaybackCmd {
    Start { id: Uuid, path: PathBuf, ts: f64, aspect: f32 },
    /// Like Start but also carries blend info so the pb thread can open a second
    /// decoder for clip_b and blend frames during the transition zone.
    StartBlend { id: Uuid, path: PathBuf, ts: f64, aspect: f32, blend: PlaybackTransitionSpec },
    Stop,
}

// ── MediaWorker ───────────────────────────────────────────────────────────────

pub struct MediaWorker {
    /// Shared result channel: probes, waveforms, audio, encode progress, HQ frames.
    pub rx:    Receiver<MediaResult>,
    tx:        Sender<MediaResult>,

    /// [Opt 3] Dedicated channel for on-demand scrub VideoFrame results.
    ///
    /// Previously scrub frames traveled through the same `rx` channel as probe
    /// results (Duration, Thumbnail, Waveform) and encode progress.  During a busy
    /// import with 4 probe threads running that channel fills quickly, adding latency
    /// between the scrub decode thread sending a frame and the UI consuming it.
    ///
    /// Separating it means scrub responsiveness is independent of import load.
    /// The channel is drained by `AppContext::ingest_media_results` before the
    /// shared channel so the UI sees scrub frames with minimal delay.
    ///
    /// Capacity = 8: the scrub slot is latest-wins, so at most one in-flight
    /// request exists at a time; 8 gives headroom for back-to-back requests
    /// during rapid scrub without dropping frames.
    pub scrub_rx: Receiver<MediaResult>,
    /// Sender half kept alive so the channel stays open and cloned into
    /// `request_transition_frame` threads that need to send blended frames.
    scrub_tx:    Sender<MediaResult>,

    /// Latest-wins slot for on-demand scrub frames.
    frame_req: Arc<(Mutex<Option<FrameRequest>>, Condvar)>,
    /// Dedicated playback pipeline.
    pb_tx:     Sender<PlaybackCmd>,
    pub pb_rx: Receiver<PlaybackFrame>,
    shutdown:  Arc<AtomicBool>,
    /// Limits concurrent probe threads: (active_count, Condvar). Max = PROBE_CONCURRENCY.
    probe_sem: Arc<(Mutex<u32>, Condvar)>,
    /// Per-job cancel flags. Keyed by job_id so cancellation is targeted.
    /// Entries are inserted by start_encode and removed by cancel_encode or on
    /// the next start_encode call (old jobs are implicitly superseded).
    encode_cancels: Arc<Mutex<HashMap<Uuid, Arc<AtomicBool>>>>,
}

impl MediaWorker {
    pub fn new() -> Self {
        let (tx, rx)           = bounded(512);
        let (scrub_tx, scrub_rx) = bounded(8); // [Opt 3] dedicated scrub channel

        let frame_req: Arc<(Mutex<Option<FrameRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));

        // ── Scrub frame decode thread ─────────────────────────────────────────
        // Blocks on the latest-wins slot; reuses the LiveDecoder when possible.
        // [Opt 3] Sends VideoFrame on scrub_tx (not tx) so scrub results bypass
        // the shared channel and are consumed with lower latency under probe load.
        let scrub_result_tx = scrub_tx.clone();
        let slot             = Arc::clone(&frame_req);
        thread::spawn(move || {
            let mut live: Option<LiveDecoder> = None;
            loop {
                let req = {
                    let (lock, cvar) = &*slot;
                    let mut guard = lock.lock().unwrap();
                    while guard.is_none() {
                        guard = cvar.wait(guard).unwrap();
                    }
                    guard.take().unwrap()
                };

                // Poison-pill: a request with a nil id signals shutdown.
                if req.id == Uuid::nil() { return; }

                // Reset (re-open + seek to keyframe) when:
                //   a) different file
                //   b) any backward movement — advance_to() can only go forward
                //   c) forward jump > 2 s — advance_to() would decode 60+ frames
                //      (~300-800 ms), blocking the thread. Re-open is instant and
                //      Layer 3 (150 ms debounce) fires the exact frame once idle.
                let needs_reset = live.as_ref().map(|d| {
                    let tpts     = d.ts_to_pts(req.timestamp);
                    let two_secs = d.ts_to_pts(2.0);
                    d.path != req.path
                        || tpts <= d.last_pts               // any backward seek
                        || tpts > d.last_pts + two_secs     // large forward jump
                }).unwrap_or(true);

                if needs_reset {
                    // [Opt #1] Move the old decoder's SwsContext out before dropping it.
                    // If the new clip has the same source format/dimensions the context
                    // is reused instead of calling SwsContext::get (which re-runs
                    // internal lookup-table init — measurable cost on the scrub path).
                    let cached_sws = live.take().map(|d| {
                        (d.scaler, d.decoder_fmt, d.decoder_w, d.decoder_h)
                    });
                    match LiveDecoder::open(&req.path, req.timestamp, req.aspect, cached_sws) {
                        Ok(mut d) => {
                            // Set skip_until_pts so next_frame() burns through the GOP
                            // (decode-only, no scale/alloc) and returns the frame at
                            // exactly req.timestamp rather than the keyframe.
                            // This replaces the old "show keyframe immediately" approach
                            // which showed a frame that could be seconds off-position.
                            // The skip loop is ~4x faster than advance_to() since it
                            // avoids scaling every intermediate frame.
                            d.skip_until_pts = d.ts_to_pts(req.timestamp);
                            if let Some((data, w, h, _)) = d.next_frame() {
                                let _ = scrub_result_tx.send(MediaResult::VideoFrame {
                                    id: req.id, width: w, height: h, data,
                                });
                            }
                            live = Some(d);
                        }
                        Err(e) => eprintln!("[media] LiveDecoder::open: {e}"),
                    }
                } else if let Some(d) = &mut live {
                    let tpts = d.ts_to_pts(req.timestamp);
                    if let Some((data, w, h)) = d.advance_to(tpts) {
                        let _ = scrub_result_tx.send(MediaResult::VideoFrame {
                            id: req.id, width: w, height: h, data,
                        });
                    }
                }
            }
        });

        // ── Dedicated playback decode thread ──────────────────────────────────
        // Runs continuously ahead of the UI filling a bounded channel (backpressure).
        // Channel capacity = 6 frames (~240ms lookahead at 25fps).
        let (pb_tx, pb_cmd_rx) = bounded::<PlaybackCmd>(4);
        let (pb_frame_tx, pb_rx) = bounded::<PlaybackFrame>(32); // 32 frames = ~1s lookahead headroom for seek burn

        thread::spawn(move || {
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
            let mut coasting:        bool = false;
            let mut coast_id:        Uuid = Uuid::nil();
            let mut coast_w:         u32  = 0;
            let mut coast_h:         u32  = 0;
            let mut coast_ts:        f64  = 0.0;
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
            let mut coast_last_primary: Option<Vec<u8>> = None;
            loop {
                if let Some((id, ref mut d)) = decoder {
                    match pb_cmd_rx.try_recv() {
                        Ok(PlaybackCmd::Start { id: new_id, path, ts, aspect }) => {
                            blend            = None; // clear any pending transition
                            held_blend       = None;
                            last_blend_alpha = 0.0;
                            coast_last_alpha = 0.5;
                            let t0 = std::time::Instant::now();
                            eprintln!("[pb] Start received (active), ts={ts:.3}");
                            match LiveDecoder::open(&path, ts, aspect, None) {
                                Ok(mut nd) => {
                                    // burn_to_pts runs synchronously (decode-only, no scale)
                                    // before we enter the send loop. The channel is empty at
                                    // this point so we're not blocking anything useful.
                                    // The first frame we send will be at the correct position.
                                    //
                                    // Using skip_until_pts (lazy, inside next_frame) was wrong:
                                    // current_time advances during the lazy burn, so the first
                                    // correct frame fails poll_playback's lower-bound check and
                                    // gets stuck in pending_pb_frame forever → hard freeze.
                                    let tpts = nd.ts_to_pts(ts);
                                    nd.burn_to_pts(tpts);
                                    eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                    decoder = Some((new_id, nd));
                                }
                                Err(e) => { eprintln!("[pb] open: {e}"); decoder = None; }
                            }
                            continue;
                        }
                        Ok(PlaybackCmd::StartBlend { id: new_id, path, ts, aspect, blend: spec }) => {
                            let invert = spec.invert_ab;

                            // For invert_ab=true (clip_b incoming): recycle the old clip_a
                            // primary decoder directly as decoder_b. At this moment it is
                            // already positioned at clip_a_tail — the exact position we need
                            // — because it was running there live just before clip_changed.
                            // No re-open, no skip_until_pts, no GOP burn, no freeze.
                            //
                            // For invert_ab=false (clip_a outgoing): decoder_b is clip_b at
                            // source_offset — a new file position, lazy open still needed.
                            let recycled_decoder_b = if invert {
                                let d = decoder.take().map(|(_, d)| d);
                                if let Some(ref db) = d {
                                    eprintln!("[pb] recycling old primary as decoder_b: last_pts={} (ts≈{:.3}s)",
                                        db.last_pts, db.pts_to_secs(db.last_pts));
                                } else {
                                    eprintln!("[pb] invert=true but no active decoder to recycle — will lazy-open");
                                }
                                d
                            } else {
                                None
                            };

                            blend = Some(ActiveBlend { spec, aspect, decoder_b: recycled_decoder_b });
                            if !invert { held_blend = None; }
                            let t0 = std::time::Instant::now();
                            eprintln!("[pb] StartBlend received (active), ts={ts:.3}, recycled_decoder_b={invert}");
                            eprintln!("[pb] StartBlend spec: blend_start_ts={:.3} duration={:.3} alpha_start={:.3} invert={invert}",
                                blend.as_ref().map(|b| b.spec.blend_start_ts).unwrap_or(0.0),
                                blend.as_ref().map(|b| b.spec.duration as f64).unwrap_or(0.0),
                                blend.as_ref().map(|b| b.spec.alpha_start as f64).unwrap_or(0.0),
                            );
                            held_streak = 0; blend_frame_count = 0;
                            match LiveDecoder::open(&path, ts, aspect, None) {
                                Ok(mut nd) => {
                                    let tpts = nd.ts_to_pts(ts);
                                    nd.burn_to_pts(tpts);
                                    eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                    decoder = Some((new_id, nd));
                                }
                                Err(e) => {
                                    eprintln!("[pb] open (blend): {e}");
                                    decoder = None;
                                    blend   = None;
                                }
                            }
                            continue;
                        }
                        Ok(PlaybackCmd::Stop) => { decoder = None; blend = None; held_blend = None; last_blend_alpha = 0.0; coast_last_alpha = 0.5; coast_last_primary = None; continue; }
                        Err(TryRecvError::Disconnected) => return,
                        Err(TryRecvError::Empty) => {}
                    }
                    // Decode next frame. send() blocks when channel is full —
                    // that IS the rate-limiter, no sleep needed.
                    match d.next_frame() {
                        Some((data, w, h, ts_secs)) => {
                            // Check whether this frame falls inside a transition blend zone.
                            //
                            // decoder_b is opened lazily on the first frame where blend_params
                            // is Some. Previously a defer guard sent raw primary frames until
                            // pb_frame_tx had ≥6 entries (~200ms buffer) before opening
                            // decoder_b, to absorb the skip_until_pts burn latency (~150ms).
                            // skip_until_pts was removed from the decoder_b open path — open is
                            // now just file-open + codec-init + keyframe seek (~10-30ms, well
                            // under one frame). The defer guard now only causes harm: on the
                            // clip_b incoming side the channel starts empty (just drained by
                            // start_blend_playback), so all 6 deferred frames are raw unblended
                            // clip_b — a 200ms flash of full clip_b at the start of the blend.

                            // Phase 1: read-only borrow to extract blend parameters.
                            let blend_params = {
                                blend.as_ref().and_then(|b| {
                                    if ts_secs >= b.spec.blend_start_ts {
                                        let local_t = ts_secs - b.spec.blend_start_ts;
                                        let alpha   = (b.spec.alpha_start as f64 + local_t / b.spec.duration as f64)
                                            .clamp(0.0, 1.0) as f32;
                                        // Log every blend frame: ts, local_t, alpha, decoder_b state.
                                        let db_state = match b.decoder_b.as_ref() {
                                            None     => "none",
                                            Some(db) if db.skip_until_pts > 0 => "burning",
                                            Some(_)  => "ready",
                                        };
                                        eprintln!("[blend] frame={blend_frame_count} ts={ts_secs:.3} local_t={local_t:.3} alpha={alpha:.3} db={db_state}");
                                        Some((b.spec.clip_b_path.clone(), b.spec.clip_b_source_start, alpha, b.spec.kind, b.aspect))
                                    } else {
                                        None
                                    }
                                })
                            };

                            // Track the most-recent outgoing blend alpha so coast mode can
                            // hand it off to the incoming StartBlend as a corrected alpha_start.
                            if let Some((_, _, alpha, _, _)) = &blend_params {
                                last_blend_alpha = *alpha;
                            }

                            // Save raw clip_a primary frame for coast animation.
                            // When primary EOF fires during outgoing blend, the coast idle loop
                            // blends this with decoder_b (clip_b) to keep the transition
                            // animating instead of freezing at the last rendered alpha.
                            if blend.as_ref().map(|b| !b.spec.invert_ab).unwrap_or(false)
                                && blend_params.is_some()
                            {
                                coast_last_primary = Some(data.clone());
                            }

                            // Phase 2: mutable access to open decoder_b lazily and blend.
                            // decoder_b_exhausted is set inside the closure (which
                            // holds a mutable borrow on blend) and acted on after it
                            // returns, clearing blend so we never call next_frame() on
                            // a dead decoder again.
                            let mut decoder_b_exhausted = false;
                            let send_data = if let Some((clip_b_path, clip_b_start, alpha, kind, decoder_b_aspect)) = blend_params {
                                let blended = (|| -> Option<Vec<u8>> {
                                    if let Some(b) = blend.as_mut() {
                                        let invert = b.spec.invert_ab;
                                        if b.decoder_b.is_none() {
                                            eprintln!("[blend] decoder_b is None — opening lazily, clip_b_start={clip_b_start:.3}");
                                            let t_open = std::time::Instant::now();
                                            match LiveDecoder::open(&clip_b_path, clip_b_start, decoder_b_aspect, None) {
                                                Ok(mut db) => {
                                                    let tpts = db.ts_to_pts(clip_b_start);
                                                    db.skip_until_pts = tpts;
                                                    eprintln!("[blend] decoder_b opened in {}ms, skip_until_pts={tpts}", t_open.elapsed().as_millis());
                                                    b.decoder_b = Some(db);
                                                }
                                                Err(e) => eprintln!("[pb] blend decoder_b open: {e}"),
                                            }
                                        }
                                        if let Some(db) = b.decoder_b.as_mut() {
                                            if let Some((data_b, wb, hb, _)) = db.next_frame() {
                                                // Bug 2 fix: guard against mismatched dimensions
                                                // (different-AR clips).  A size mismatch would cause
                                                // an out-of-bounds read inside blend_rgba_transition
                                                // which indexes both buffers up to w*h*4 derived from
                                                // the primary decoder — causing a panic that kills the
                                                // pb thread and makes the project unplayable.
                                                if data_b.len() != data.len() || wb != w || hb != h {
                                                    eprintln!(
                                                        "[pb] blend size mismatch — primary {}×{} ({} B)                                                          vs decoder_b {}×{} ({} B); skipping blend",
                                                        w, h, data.len(), wb, hb, data_b.len()
                                                    );
                                                    return None; // fall through to unblended primary frame
                                                }
                                                let blended = if invert {
                                                    blend_rgba_transition(&data_b, &data, w, h, alpha, kind)
                                                } else {
                                                    blend_rgba_transition(&data, &data_b, w, h, alpha, kind)
                                                };
                                                return Some(blended);
                                            } else {
                                                // next_frame() returned None — either still burning
                                                // through the GOP (skip_until_pts > 0) or true EOF.
                                                // Distinguish the two: if still skipping, return None
                                                // so the caller sends held_blend (last good blended
                                                // frame) instead of raw primary. Repeating the last
                                                // blended frame for ~60ms is invisible; flashing raw
                                                // primary (no iris/wipe) is not.
                                                // If skip is done (pts cleared to 0) and we got None,
                                                // decoder_b has hit EOF.
                                                let still_burning = b.decoder_b.as_ref()
                                                    .map(|db| db.skip_until_pts > 0)
                                                    .unwrap_or(false);
                                                if still_burning {
                                                    let db = b.decoder_b.as_ref().unwrap();
                                                    eprintln!("[blend] still_burning: skip_until_pts={} last_pts={} gap_pts={}",
                                                        db.skip_until_pts, db.last_pts, db.skip_until_pts - db.last_pts);
                                                    // For invert_ab=true (clip_b incoming), primary=clip_b
                                                    // is in `data` and held_blend approximates clip_a at the
                                                    // last known position. Compute a real blend so the
                                                    // burn-window frames animate instead of freezing at the
                                                    // coast alpha — eliminating the visible alpha jump when
                                                    // decoder_b finishes. blend_rgba_transition(a, b, ...):
                                                    //   a = held_blend (clip_a stand-in, slightly stale)
                                                    //   b = data        (clip_b primary, exact)
                                                    // Not pixel-perfect since held_blend already mixes some
                                                    // clip_b, but imperceptible for ≤2 frames vs a freeze.
                                                    // For invert_ab=false (outgoing), decoder_b IS clip_b
                                                    // and still burning — no clean clip_b pixels available,
                                                    // so fall through to frozen held_blend as before.
                                                    if invert {
                                                        if let Some(hb) = held_blend.as_ref() {
                                                            if hb.len() == data.len() {
                                                                eprintln!("[blend] still_burning animated: alpha={alpha:.3}");
                                                                return Some(blend_rgba_transition(hb, &data, w, h, alpha, kind));
                                                            }
                                                        }
                                                    }
                                                    return None; // outgoing or no held_blend: use frozen held_blend
                                                }
                                                decoder_b_exhausted = true;
                                            }
                                        }
                                    }
                                    None
                                })();
                                // Must run after closure releases its borrow on blend.
                                if decoder_b_exhausted { blend = None; }
                                // If blended is Some: update held_blend and use it.
                                // If None (still burning): use held_blend (frozen last frame)
                                //   or fall back to raw primary only on the very first frame
                                //   before any blend has been produced yet.
                                match blended {
                                    Some(b) => {
                                        if held_streak > 0 {
                                            eprintln!("[blend] held_blend streak ended after {held_streak} frames");
                                            held_streak = 0;
                                        }
                                        blend_frame_count += 1;
                                        held_blend = Some(b.clone()); b
                                    }
                                    None => {
                                        held_streak += 1;
                                        if held_streak == 1 {
                                            eprintln!("[blend] held_blend streak START (ts={ts_secs:.3} alpha from blend_params pending)");
                                        }
                                        held_blend.clone().unwrap_or(data)
                                    }
                                }
                            } else {
                                data
                            };

                            // Track metadata for coast mode: if primary EOF fires during an
                            // outgoing blend these values are used to keep sending held_blend
                            // frames (same id/dims, incrementing ts) until clip_changed fires.
                            coast_id = id;
                            coast_w  = w;
                            coast_h  = h;
                            coast_ts = ts_secs;
                            let f = PlaybackFrame { id, timestamp: ts_secs, width: w, height: h, data: send_data };
                            frame_count += 1;
                            if frame_count % 60 == 0 {
                                eprintln!("[pb] frame #{frame_count} sent, ts={ts_secs:.3}");
                            }
                            if pb_frame_tx.send(f).is_err() { return; }
                        }
                        None => {
                            // Primary decoder reached EOF.
                            // If we were in an outgoing blend (invert_ab=false) and have a
                            // held_blend frame, enter coast mode instead of going idle.
                            // This covers the case where the video file runs out a few frames
                            // before the nominal clip end (gap between last real frame and the
                            // probed duration) — without coast mode the pb thread blocks on
                            // recv() until clip_changed fires, draining pb_rx and freezing
                            // the UI for ~100-200ms.
                            let outgoing_blend = blend.as_ref().map(|b| !b.spec.invert_ab).unwrap_or(false);
                            if outgoing_blend && held_blend.is_some() {
                                coast_last_alpha = last_blend_alpha;
                                eprintln!("[pb] primary EOF during outgoing blend — entering coast mode \
                                           (ts={coast_ts:.3}, alpha={coast_last_alpha:.3}, \
                                           decoder_b preserved for animated coast)");
                                coasting = true;
                                // blend intentionally NOT cleared: decoder_b (clip_b) stays alive
                                // so the coast idle loop generates real animated blend frames.
                                // held_blend preserved to cover the incoming StartBlend burn window.
                            } else {
                                eprintln!("[pb] primary decoder EOF, clearing decoder + blend");
                                held_blend        = None;
                                coast_last_primary = None;
                                blend             = None;
                            }
                            decoder = None;
                            // When coasting: blend stays live (cleared in else branch above only).
                        }
                    }
                } else {
                    // Idle branch: no primary decoder.
                    //
                    // Coast mode (entered when primary EOF fires during outgoing blend):
                    //   try_recv so we can keep sending held_blend frames at ~30 fps to
                    //   feed pb_rx while we wait for clip_changed / StartBlend to arrive.
                    //   pb_frame_tx.send() provides the rate-limit (blocks when full).
                    //
                    // Normal idle: block on recv() until the next command.
                    let cmd_opt = if coasting {
                        match pb_cmd_rx.try_recv() {
                            Ok(cmd)                         => Some(cmd),
                            Err(TryRecvError::Empty)        => None,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    } else {
                        match pb_cmd_rx.recv() {
                            Ok(cmd) => Some(cmd),
                            Err(_)  => return,
                        }
                    };

                    if let Some(cmd) = cmd_opt {
                        // Save coasting BEFORE clearing it — the StartBlend arm needs
                        // was_coasting to apply the alpha-continuity correction.
                        // Previously `coasting = false` ran first, making
                        // `if invert && coasting` always false → alpha_start was never
                        // overridden → visible jump at handoff (wipe/iris snaps forward).
                        let was_coasting = coasting;
                        coasting = false;
                        match cmd {
                            PlaybackCmd::Start { id, path, ts, aspect } => {
                                blend            = None;
                                held_blend       = None;
                                last_blend_alpha = 0.0;
                                coast_last_alpha = 0.5;
                                let t0 = std::time::Instant::now();
                                eprintln!("[pb] Start received (idle), ts={ts:.3}");
                                match LiveDecoder::open(&path, ts, aspect, None) {
                                    Ok(mut d) => {
                                        let tpts = d.ts_to_pts(ts);
                                        d.burn_to_pts(tpts);
                                        eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                        decoder = Some((id, d));
                                    }
                                    Err(e) => eprintln!("[pb] open: {e}"),
                                }
                            }
                            PlaybackCmd::StartBlend { id, path, ts, aspect, blend: mut spec } => {
                                let invert = spec.invert_ab;

                                // ── Alpha-continuity correction ───────────────────────────────
                                // coast_last_alpha records where the outgoing wipe/iris actually
                                // stopped.  Override alpha_start so the incoming blend resumes
                                // from that exact position instead of the hardcoded 0.5 midpoint.
                                if invert && was_coasting {
                                    eprintln!("[pb] incoming StartBlend while coasting: overriding \
                                               alpha_start {:.3} → {:.3} (coast_last_alpha)",
                                               spec.alpha_start, coast_last_alpha);
                                    spec.alpha_start = coast_last_alpha;
                                }

                                // ── Bridge frames: fill the keyframe gap ─────────────────────
                                // After burn_to_pts(ts≈0.002), the primary decoder's first
                                // decodable keyframe lands at ts≈0.083 (~83ms, 2–3 frames).
                                // Without bridging, held_frame freezes at the last coast alpha
                                // for that entire window before the real blend resumes — a
                                // visible snap.
                                //
                                // Fix: extract decoder_b from the coast blend (it's still
                                // decoding clip_b), produce bridge frames using it +
                                // coast_last_primary (last clip_a raw frame), and send them
                                // with the NEW clip's id and clip_b-relative timestamps.
                                // Those timestamps fall inside poll_playback's startup-exception
                                // window (lt<0.15, f.ts<0.30) so they are shown immediately
                                // after clip_changed, filling the gap smoothly.
                                // The loop now fills pb_frame_tx to near-capacity (see below)
                                // so the channel stays fed through the subsequent burn_to_pts.
                                //
                                // Only applies on the incoming (invert_ab=true) side — that's
                                // when coast mode is active and decoder_b holds live clip_b data.
                                let coast_blend_db = if was_coasting && invert {
                                    // Extract decoder_b before we replace blend below.
                                    blend.take().and_then(|b| b.decoder_b)
                                } else {
                                    drop(blend.take());
                                    None
                                };

                                // ── Pre-open decoder_b (clip_a tail) ─────────────────────────
                                // decoder_b for the incoming blend is clip_a's tail.  We
                                // open and burn it here — during the bridge window — so that
                                // frame 1 from the new primary already has a ready secondary.
                                // Without this, decoder_b is opened lazily on the first primary
                                // frame and needs skip_until_pts to burn to clip_a_tail (a late
                                // position in the file), causing several frames of held_blend
                                // approximation and a visible quality jump when it becomes ready.
                                let prebuilt_db = if was_coasting && invert {
                                    let db_path  = spec.clip_b_path.clone();
                                    let db_start = spec.clip_b_source_start;
                                    let t_db     = std::time::Instant::now();
                                    eprintln!("[pb] pre-opening decoder_b for clip_a tail at {db_start:.3}");
                                    match LiveDecoder::open(&db_path, db_start, aspect, None) {
                                        Ok(mut db) => {
                                            let tpts = db.ts_to_pts(db_start);
                                            db.burn_to_pts(tpts);
                                            eprintln!("[pb] decoder_b pre-burn done in {}ms", t_db.elapsed().as_millis());
                                            Some(db)
                                        }
                                        Err(e) => { eprintln!("[pb] decoder_b pre-open: {e}"); None }
                                    }
                                } else {
                                    None
                                };

                                // burn_ts starts at ts (clip_b start) and is updated to
                                // bridge_ts at the end of the bridge loop so the primary
                                // decoder is opened at the position where the bridge ended.
                                let mut burn_ts = ts;

                                if let Some(mut bridge_db) = coast_blend_db {
                                    coast_id = id; // bridge frames carry the NEW clip's id
                                    let bridge_duration = spec.duration;
                                    let bridge_kind     = spec.kind;
                                    let mut bridge_ts   = ts;
                                    // Fill pb_frame_tx to near-capacity rather than capping at 3.
                                    //
                                    // The 3-frame cap was the root cause of the freeze at the B-side
                                    // handoff: poll_playback's startup-exception shows all 3 bridge
                                    // frames in one tick (lt<0.15, ts<0.30 all pass), draining the
                                    // channel before burn_to_pts finishes below.  Filling to 28/32
                                    // ensures pb_rx stays fed through even a 200ms burn window
                                    // (~6 frames at 30fps) with plenty of margin.
                                    //
                                    // bridge_db may run out before the channel is full — that's fine,
                                    // the loop exits and held_blend covers the remainder.
                                    const BRIDGE_TARGET: usize = 28; // leave 4 slots for real blend frames
                                    while pb_frame_tx.len() < BRIDGE_TARGET {
                                        let fa = match coast_last_primary.as_ref() {
                                            Some(f) => f,
                                            None    => break,
                                        };
                                        let (data_b, _, _, _) = match bridge_db.next_frame() {
                                            Some(f) => f,
                                            None    => break, // bridge_db exhausted
                                        };
                                        if data_b.len() != (coast_w * coast_h * 4) as usize { break; }
                                        let step = (1.0_f32 / 30.0) / bridge_duration;
                                        coast_last_alpha = (coast_last_alpha + step).min(1.0);
                                        bridge_ts += 1.0 / 30.0;
                                        let blended = blend_rgba_transition(
                                            fa, &data_b, coast_w, coast_h,
                                            coast_last_alpha, bridge_kind,
                                        );
                                        eprintln!("[pb] bridge: ts={bridge_ts:.3} alpha={coast_last_alpha:.3} chan={}", pb_frame_tx.len());
                                        held_blend = Some(blended.clone());
                                        let f = PlaybackFrame {
                                            id, timestamp: bridge_ts,
                                            width: coast_w, height: coast_h, data: blended,
                                        };
                                        if pb_frame_tx.send(f).is_err() { return; }
                                    }
                                    // Update alpha_start so the real blend continues from the
                                    // bridge's final alpha rather than restarting from 0.5.
                                    spec.alpha_start = coast_last_alpha;
                                    // Record where the bridge ended so the primary is burned
                                    // to this position instead of the original ts (see below).
                                    burn_ts = bridge_ts;
                                    eprintln!("[pb] bridge done: alpha_start updated to {:.3}, burn_ts={burn_ts:.3}, chan_filled={}", spec.alpha_start, pb_frame_tx.len());
                                }

                                blend = Some(ActiveBlend { spec, aspect, decoder_b: prebuilt_db });
                                if !invert { held_blend = None; }
                                let t0 = std::time::Instant::now();
                                // Burn primary to burn_ts (end of bridge) not original ts.
                                //
                                // After the bridge loop, current_time has advanced by
                                // bridge_frame_count/30 seconds.  Burning the primary to the
                                // original ts means its first decodable frame is at ts, but
                                // poll_playback's step-2 fast-forward immediately skips every
                                // frame where f.timestamp < local_t − 1/30 — draining the
                                // channel faster than the primary can refill it → brief stall,
                                // then a burst of frames as the decoder catches up.
                                // Burning to burn_ts (= bridge_ts after the loop) places the
                                // primary's first frame close to where current_time actually
                                // is, so step-2 discards at most one frame and playback
                                // continues smoothly from the bridge endpoint.
                                eprintln!("[pb] StartBlend received (idle), ts={ts:.3} burn_ts={burn_ts:.3}");
                                match LiveDecoder::open(&path, burn_ts, aspect, None) {
                                    Ok(mut d) => {
                                        let tpts = d.ts_to_pts(burn_ts);
                                        d.burn_to_pts(tpts);
                                        eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                        decoder = Some((id, d));
                                    }
                                    Err(e) => {
                                        eprintln!("[pb] open (blend, idle): {e}");
                                        blend = None;
                                    }
                                }
                            }
                            PlaybackCmd::Stop => { blend = None; held_blend = None; last_blend_alpha = 0.0; coast_last_alpha = 0.5; coast_last_primary = None; }
                        }
                    } else {
                        // Coast mode, no command yet — hold last frame until StartBlend arrives.
                        //
                        // We intentionally do NOT increment coast_ts: the UI should see the same
                        // frozen frame (same timestamp, same alpha) rather than a flood of
                        // synthetic frames with ever-advancing timestamps that pile up in the
                        // queue ahead of the real blend frames. Flooding was the root cause of
                        // the "frozen preview" during transition: 30+ coast frames queued up,
                        // the UI drained them (taking ~1 s) before reaching the first real blend
                        // frame from StartBlend.
                        //
                        // We only push a single coast frame if the channel is nearly empty
                        // (< 5 items), so pb_rx stays fed enough that the UI doesn't stall
                        // while still leaving headroom for the incoming blend burst.
                        // Animated coast: pull the next clip_b frame from decoder_b and
                        // blend it with the saved last clip_a frame (coast_last_primary).
                        // This keeps the wipe/iris/etc. visually moving through the ~125ms
                        // window between primary EOF and the incoming StartBlend, instead of
                        // freezing and then jumping when real blend frames resume.
                        if pb_frame_tx.len() >= 5 {
                            // Channel is full enough; yield to avoid a hot-spin.
                            std::thread::sleep(std::time::Duration::from_millis(4));
                        } else {
                            let animated = (|| -> Option<Vec<u8>> {
                                let b  = blend.as_mut()?;
                                let fa = coast_last_primary.as_ref()?;
                                let db = b.decoder_b.as_mut()?;
                                let (data_b, _, _, _) = db.next_frame()?;
                                if data_b.len() != fa.len() { return None; }
                                // Advance alpha one frame step (invert_ab=false: a=clip_a, b=clip_b).
                                let step = (1.0_f32 / 30.0) / b.spec.duration;
                                coast_last_alpha = (coast_last_alpha + step).min(1.0);
                                coast_ts += 1.0 / 30.0;
                                Some(blend_rgba_transition(fa, &data_b, coast_w, coast_h, coast_last_alpha, b.spec.kind))
                            })();

                            let send_data = if let Some(blended) = animated {
                                eprintln!("[pb] coast animated: ts={coast_ts:.3} alpha={coast_last_alpha:.3}");
                                held_blend = Some(blended.clone());
                                Some(blended)
                            } else {
                                // decoder_b exhausted or unavailable — frozen held_blend fallback.
                                held_blend.clone()
                            };

                            if let Some(data) = send_data {
                                let f = PlaybackFrame {
                                    id:        coast_id,
                                    timestamp: coast_ts,
                                    width:     coast_w,
                                    height:    coast_h,
                                    data,
                                };
                                if pb_frame_tx.send(f).is_err() { return; }
                            } else {
                                eprintln!("[pb] coast: both animated and held_blend None — exiting coast");
                                coasting = false;
                            }
                        }
                    }
                }
            }
        });

        Self {
            rx, tx, scrub_rx, scrub_tx, frame_req, pb_tx, pb_rx,
            shutdown:       Arc::new(AtomicBool::new(false)),
            probe_sem:      Arc::new((Mutex::new(0), Condvar::new())),
            encode_cancels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Cancel any active encode jobs.
        let cancels = self.encode_cancels.lock().unwrap();
        for flag in cancels.values() {
            flag.store(true, Ordering::Relaxed);
        }
        // Wake the scrub decode thread with a poison-pill so it exits cleanly
        // instead of blocking forever on the condvar.
        let (lock, cvar) = &*self.frame_req;
        *lock.lock().unwrap() = Some(FrameRequest {
            id:        Uuid::nil(),
            path:      std::path::PathBuf::new(),
            timestamp: 0.0,
            aspect:    0.0,
        });
        cvar.notify_one();
    }

    pub fn probe_clip(&self, id: Uuid, path: PathBuf) {
        let tx  = self.tx.clone();
        let sd  = self.shutdown.clone();
        let sem = self.probe_sem.clone();

        // Spawn a single gatekeeper thread that acquires the semaphore *before*
        // spawning the real work. This means at most PROBE_CONCURRENCY + 1 threads
        // exist at any time (one gatekeeper waiting + N workers running), instead of
        // one parked thread per queued clip.
        std::thread::spawn(move || {
            const PROBE_CONCURRENCY: u32 = 4;
            {
                let (lock, cvar) = &*sem;
                let mut count = lock.lock().unwrap();
                while *count >= PROBE_CONCURRENCY {
                    count = cvar.wait(count).unwrap();
                }
                *count += 1;
            }
            // RAII release guard — decrements count and wakes next waiter on drop
            struct SemGuard(Arc<(Mutex<u32>, Condvar)>);
            impl Drop for SemGuard {
                fn drop(&mut self) {
                    let (lock, cvar) = &*self.0;
                    *lock.lock().unwrap() -= 1;
                    cvar.notify_one();
                }
            }
            let _guard = SemGuard(sem);

            if sd.load(Ordering::Relaxed) { return; }
            let dur = probe_duration(&path, id, &tx);
            if sd.load(Ordering::Relaxed) { return; }
            probe_video_size_and_thumbnail(&path, id, dur, &tx);

            // Release the semaphore here — the in-process FFmpeg work (duration +
            // thumbnail) is done. Waveform and audio decoding are also in-process
            // but can run for seconds on long files. Holding the semaphore through
            // them starves thumbnail/duration results for clips imported afterward.
            drop(_guard);

            if sd.load(Ordering::Relaxed) { return; }
            extract_waveform(&path, id, &tx);
            if sd.load(Ordering::Relaxed) { return; }
            if dur > 0.0 {
                extract_audio(&path, id, &tx);
            }
        });
    }

    /// No-op for compat — thumbnails now come back via probe_clip as RGBA data.
    /// Called on restore from saved state where thumbnail_path was persisted.
    pub fn reload_thumbnail(&self, id: Uuid, path: PathBuf) {
        self.probe_clip(id, path);
    }

    pub fn request_frame(&self, id: Uuid, path: PathBuf, timestamp: f64, aspect: f32) {
        // Overwrite any pending request — the decode thread always gets the freshest one.
        let (lock, cvar) = &*self.frame_req;
        *lock.lock().unwrap() = Some(FrameRequest { id, path, timestamp, aspect });
        cvar.notify_one();
    }

    /// Decode frames from two clips, blend them with the given transition, and
    /// send the result via the scrub channel (keyed by `req.clip_a_id`).
    ///
    /// Spawns a one-shot background thread — does not block the caller.
    /// The blended frame arrives in `scrub_rx` with `id = clip_a_id` so
    /// `ingest_video_frame` stores it under the outgoing clip's slot, which is
    /// what `poll_playback` and the frame_cache expect during the blend zone.
    pub fn request_transition_frame(&self, req: TransitionScrubRequest) {
        let scrub_tx = self.scrub_tx.clone();
        let sd       = self.shutdown.clone();
        thread::spawn(move || {
            if sd.load(Ordering::Relaxed) { return; }
            let (data_a, w, h) = match decode_one_frame_rgba(&req.clip_a_path, req.clip_a_ts) {
                Ok(f)  => f,
                Err(e) => { eprintln!("[transition] clip_a decode: {e}"); return; }
            };
            if sd.load(Ordering::Relaxed) { return; }
            let (data_b_raw, wb, hb) = match decode_one_frame_rgba(&req.clip_b_path, req.clip_b_ts) {
                Ok(f)  => f,
                Err(e) => { eprintln!("[transition] clip_b decode: {e}"); return; }
            };
            // Guard against mixed-AR clip pairs: if clip_b decoded at a different
            // size than clip_a, blend_rgba_transition would index data_b out of bounds
            // (w*h*4 derived from clip_a) → panic → thread dies, nothing sent.
            // Center-crop data_b to w×h so both buffers are the same size.
            let data_b = if wb != w || hb != h {
                eprintln!(
                    "[transition] clip_b size {}×{} differs from clip_a {}×{}; cropping",
                    wb, hb, w, h
                );
                crop_rgba(&data_b_raw, wb, hb, w, h)
            } else {
                data_b_raw
            };
            let blended = blend_rgba_transition(&data_a, &data_b, w, h, req.alpha, req.kind);
            let _ = scrub_tx.send(MediaResult::VideoFrame {
                id: req.clip_a_id, width: w, height: h, data: blended,
            });
        });
    }

    /// Start the dedicated playback pipeline at `ts` seconds into `path`.
    pub fn start_playback(&self, id: Uuid, path: PathBuf, ts: f64, aspect: f32) {
        // Send Start BEFORE draining pb_rx.  The pb thread processes the Start
        // command and resets its decoder before it can push any new frames, so
        // everything remaining in the channel after the send is guaranteed to be
        // from the previous session.  The old order (drain then send) had a window
        // where the pb thread pushed a stale frame after the drain but before the
        // Start was processed.
        if self.pb_tx.try_send(PlaybackCmd::Start { id, path, ts, aspect }).is_err() {
            eprintln!("[pb] start_playback: command channel full — Start dropped. This is a bug.");
        }
        while self.pb_rx.try_recv().is_ok() {}
    }

    /// Like `start_playback` but also informs the playback thread of an upcoming
    /// transition so it can blend frames at the clip boundary without an extra
    /// start/stop cycle.
    ///
    /// The pb thread opens clip_b's decoder lazily when `ts_secs` in clip_a reaches
    /// `spec.blend_start_ts`, blends frames until clip_a ends, then continues with
    /// whatever `Start` command `tick()` sends for clip_b.
    pub fn start_blend_playback(
        &self,
        id:     Uuid,
        path:   PathBuf,
        ts:     f64,
        aspect: f32,
        blend:  PlaybackTransitionSpec,
    ) {
        if self.pb_tx.try_send(PlaybackCmd::StartBlend { id, path, ts, aspect, blend }).is_err() {
            eprintln!("[pb] start_blend_playback: command channel full — StartBlend dropped. This is a bug.");
        }
        while self.pb_rx.try_recv().is_ok() {}
    }

    /// Stop the dedicated playback pipeline.
    pub fn stop_playback(&self) {
        // try_send (not send) — must never block the UI thread.
        // Log loudly if the channel is full so the failure surfaces in testing
        // rather than silently leaving the pb thread running.
        if self.pb_tx.try_send(PlaybackCmd::Stop).is_err() {
            eprintln!("[pb] stop_playback: command channel full — Stop dropped. This is a bug.");
        }
        // Drain any buffered frames so their RGBA allocations (~30 MB at 640×360
        // for a full 32-frame channel) are freed immediately rather than held until
        // the next start_playback call.
        // Previously this mattered only on explicit stop. With the SetPlayhead fix,
        // stop_playback is called on every intra-clip seek during playback, making
        // prompt drainage more important.
        while self.pb_rx.try_recv().is_ok() {}
    }

    pub fn extract_frame_hq(&self, id: Uuid, path: PathBuf, timestamp: f64, dest: PathBuf) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        thread::spawn(move || {
            if sd.load(Ordering::Relaxed) { return; }
            if let Err(e) = decode_frame(
                &path, id, timestamp, 0.0, true, Some(dest), &tx,
            ) {
                eprintln!("[media] extract_frame_hq: {e}");
            }
        });
    }

    /// Spawn a background thread to encode `spec` to disk.
    ///
    /// Only one encode job runs at a time from the UI's perspective (ExportModule
    /// tracks `encode_job_id`), but the architecture supports multiple concurrent
    /// jobs if needed in the future — each has its own cancel flag keyed by job_id.
    pub fn start_encode(&self, spec: EncodeSpec) {
        let job_id = spec.job_id;
        let cancel = Arc::new(AtomicBool::new(false));
        let tx     = self.tx.clone();
        let sd     = self.shutdown.clone();

        // Register cancel flag before spawning — avoids a window where
        // cancel_encode is called before the thread has inserted the flag.
        self.encode_cancels.lock().unwrap().insert(job_id, Arc::clone(&cancel));

        let cancels_ref = Arc::clone(&self.encode_cancels);
        thread::spawn(move || {
            if sd.load(Ordering::Relaxed) {
                let _ = tx.send(MediaResult::EncodeError {
                    job_id,
                    msg: "worker shutting down".into(),
                });
                return;
            }

            encode_timeline(spec, cancel, tx);

            // Remove cancel flag once the job is done (avoids unbounded growth
            // of the HashMap if many short encodes are started over a session).
            cancels_ref.lock().unwrap().remove(&job_id);
        });
    }

    /// Signal the encode job identified by `job_id` to stop.
    /// The thread will finish its current frame and then exit, sending
    /// `EncodeError { msg: "cancelled" }` over the result channel.
    pub fn cancel_encode(&self, job_id: Uuid) {
        if let Some(flag) = self.encode_cancels.lock().unwrap().get(&job_id) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

// ── Blend decoder helpers ─────────────────────────────────────────────────────

/// All state for an active blend zone inside the pb thread.
///
/// Replaces the raw `(PlaybackTransitionSpec, Option<LiveDecoder>)` tuple.
struct ActiveBlend {
    spec:      velocut_core::media_types::PlaybackTransitionSpec,
    /// Project aspect ratio inherited from the StartBlend command.
    /// Used when opening decoder_b so both decoders scale to the same output
    /// dimensions. Without this, decoder_b uses the clip's native AR and its
    /// output height differs from the primary decoder → permanent size mismatch
    /// → every blend frame skipped.
    aspect:    f32,
    /// The secondary decoder, opened lazily when the blend zone is first reached.
    decoder_b: Option<LiveDecoder>,
}

// NOTE — why there is no eager/async decoder_b pre-open:
//
// A previous attempt tried to open decoder_b on a background thread concurrently
// with the primary burn, passing it back via a crossbeam channel.  This does not
// compile: `LiveDecoder` contains `ffmpeg::software::scaling::Context` which wraps
// `*mut SwsContext` and is not `Send`.  FFmpeg context objects must stay on the
// thread that created them.
//
// The lazy-open path in the frame loop minimises the stall by using
// `skip_until_pts` (decode-only, ~4x faster than `burn_to_pts`) for decoder_b.
// decoder_b frames are used only for blending and are never checked against
// `poll_playback`'s lower-bound timestamp gate, so the lazy-skip is safe here
// even though it cannot be used for the primary decoder.

// ── RGBA crop helper ──────────────────────────────────────────────────────────
//
// Center-crops a src_w×src_h RGBA buffer down to dst_w×dst_h.
// Used by request_transition_frame when clip_b has a different native AR than
// clip_a — both decode_one_frame_rgba outputs must be the same size before
// blend_rgba_transition can safely index them.
//
// Matches the crop semantics in preview_module.rs::crop_uv_rect and encode.rs
// CropScaler: the wider dimension is cropped symmetrically from both sides.
// Allocates only when src and dst dims differ; fast path is the equality check
// in the caller.
fn crop_rgba(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let src_ar = src_w as f32 / src_h.max(1) as f32;
    let dst_ar = dst_w as f32 / dst_h.max(1) as f32;

    // Compute the region of src to sample.
    let (off_x, off_y, used_w, used_h) = if src_ar > dst_ar {
        // Source wider — crop left/right, keep full height.
        let used_w = (src_h as f32 * dst_ar) as u32;
        let off_x  = (src_w - used_w) / 2;
        (off_x, 0u32, used_w, src_h)
    } else {
        // Source taller — crop top/bottom, keep full width.
        let used_h = (src_w as f32 / dst_ar) as u32;
        let off_y  = (src_h - used_h) / 2;
        (0u32, off_y, src_w, used_h)
    };

    // Scale factors from the used region to dst dims.
    let sx = used_w  as f32 / dst_w.max(1) as f32;
    let sy = used_h as f32 / dst_h.max(1) as f32;

    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
    for dy in 0..dst_h {
        for dx in 0..dst_w {
            let src_x = (off_x as f32 + dx as f32 * sx) as u32;
            let src_y = (off_y as f32 + dy as f32 * sy) as u32;
            let src_x = src_x.clamp(0, src_w.saturating_sub(1));
            let src_y = src_y.clamp(0, src_h.saturating_sub(1));
            let si = (src_y * src_w + src_x) as usize * 4;
            let di = (dy   * dst_w + dx)     as usize * 4;
            out[di..di+4].copy_from_slice(&src[si..si+4]);
        }
    }
    out
}


// ── RGBA transition blending ──────────────────────────────────────────────────
//
// Delegates to VideoTransition::apply_rgba() via the registry — no transition
// logic lives here.  To add a new transition, create its .rs file and add one
// line to declare_transitions! in mod.rs.  Nothing in this file needs to change.

fn blend_rgba_transition(
    a:     &[u8],
    b:     &[u8],
    w:     u32,
    h:     u32,
    alpha: f32,
    kind:  velocut_core::transitions::TransitionKind,
) -> Vec<u8> {
    use velocut_core::transitions::{TransitionKind, registry};

    if kind == TransitionKind::Cut {
        return a.to_vec();
    }

    registry()
        .remove(&kind)
        .expect("blend_rgba_transition: unregistered TransitionKind — add it to declare_transitions!")
        .apply_rgba(a, b, w, h, alpha)
}