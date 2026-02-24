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
    Start { id: Uuid, path: PathBuf, ts: f64, aspect: f32, preview_size: Option<(u32, u32)> },
    /// Like Start but also carries blend info so the pb thread can open a second
    /// decoder for clip_b and blend frames during the transition zone.
    StartBlend { id: Uuid, path: PathBuf, ts: f64, aspect: f32, blend: PlaybackTransitionSpec, preview_size: Option<(u32, u32)> },
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
    /// Limits concurrent HQ / transition-scrub decode threads.
    /// Each request_frame_hq / request_transition_frame* call opens one or two
    /// full native-res FFmpeg decoder contexts; without a cap, rapid L3-idle
    /// updates pile up threads and inflate RSS by ~16 MB per in-flight decode.
    /// Limit = 2: one in-flight HQ frame plus one transition blend at most.
    hq_sem: Arc<(Mutex<u32>, Condvar)>,
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
                    match LiveDecoder::open(&req.path, req.timestamp, req.aspect, cached_sws, None) {
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
        //
        // [Fix 1] Channel reduced from 32 → 6 frames.
        // At 480p RGBA each frame is ~1.2 MB — 32 frames = 38 MB sitting in the
        // channel at all times during playback. burn_to_pts runs synchronously
        // before the send loop starts so the extra headroom was never consumed;
        // 6 frames (~200ms at 30fps) is sufficient for smooth playback.
        let (pb_tx, pb_cmd_rx) = bounded::<PlaybackCmd>(4);
        let (pb_frame_tx, pb_rx) = bounded::<PlaybackFrame>(3); // was 6; preview-res frames ~0.5MB each so 3 = ~1.5MB vs old 48MB

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
            // [Fix 3] Only cloned when we're in an active outgoing blend that has
            // reached blend_start_ts — previously cloned every frame in the blend zone
            // even before the visual transition started, wasting ~1.2 MB/frame.
            let mut coast_last_primary: Option<Vec<u8>> = None;
            loop {
                if let Some((id, ref mut d)) = decoder {
                    match pb_cmd_rx.try_recv() {
                        Ok(PlaybackCmd::Start { id: new_id, path, ts, aspect, preview_size }) => {
                            blend            = None; // clear any pending transition
                            held_blend       = None;
                            last_blend_alpha = 0.0;
                            coast_last_alpha = 0.5;
                            let t0 = std::time::Instant::now();
                            eprintln!("[pb] Start received (active), ts={ts:.3}");
                            match LiveDecoder::open(&path, ts, aspect, None, preview_size) {
                                Ok(mut nd) => {
                                    let tpts = nd.ts_to_pts(ts);
                                    nd.burn_to_pts(tpts);
                                    eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                    decoder = Some((new_id, nd));
                                }
                                Err(e) => { eprintln!("[pb] open: {e}"); decoder = None; }
                            }
                            continue;
                        }
                        Ok(PlaybackCmd::StartBlend { id: new_id, path, ts, aspect, blend: spec, preview_size }) => {
                            let invert = spec.invert_ab;

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

                            blend = Some(ActiveBlend { spec, aspect, decoder_b: recycled_decoder_b, preview_size });
                            if !invert { held_blend = None; }
                            let t0 = std::time::Instant::now();
                            eprintln!("[pb] StartBlend received (active), ts={ts:.3}, recycled_decoder_b={invert}");
                            eprintln!("[pb] StartBlend spec: blend_start_ts={:.3} duration={:.3} alpha_start={:.3} invert={invert}",
                                blend.as_ref().map(|b| b.spec.blend_start_ts).unwrap_or(0.0),
                                blend.as_ref().map(|b| b.spec.duration as f64).unwrap_or(0.0),
                                blend.as_ref().map(|b| b.spec.alpha_start as f64).unwrap_or(0.0),
                            );
                            held_streak = 0; blend_frame_count = 0;
                            match LiveDecoder::open(&path, ts, aspect, None, preview_size) {
                                Ok(mut nd) => {
                                    let tpts = nd.ts_to_pts(ts);
                                    nd.burn_to_pts(tpts);
                                    eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                    decoder = Some((new_id, nd));

                                    if !invert {
                                        let primary_size = decoder.as_ref().map(|(_, d)| (d.out_w, d.out_h));
                                        if let Some(ref mut b) = blend {
                                            let db_path   = b.spec.clip_b_path.clone();
                                            let db_start  = b.spec.clip_b_source_start;
                                            let db_aspect = b.aspect;
                                            let t_db = std::time::Instant::now();
                                            eprintln!("[pb] pre-opening decoder_b for outgoing blend: clip_b_start={db_start:.3}");
                                            match LiveDecoder::open(&db_path, db_start, db_aspect, None, primary_size) {
                                                Ok(mut db) => {
                                                    db.skip_until_pts = db.ts_to_pts(db_start);
                                                    eprintln!("[pb] decoder_b pre-opened in {}ms, lazy burn started (skip_until_pts={})",
                                                        t_db.elapsed().as_millis(), db.skip_until_pts);
                                                    b.decoder_b = Some(db);
                                                }
                                                Err(e) => eprintln!("[pb] decoder_b pre-open (outgoing): {e}"),
                                            }
                                        }
                                    }
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
                                        eprintln!("[blend] frame={blend_frame_count} ts={ts_secs:.3} local_t={local_t:.3} alpha={alpha:.3} db={db_state}");
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

                            let blend_params = if blend_params.as_ref().map(|(_, _, a, _, _)| *a >= 1.0).unwrap_or(false) {
                                blend      = None;
                                held_blend = None;
                                eprintln!("[blend] alpha=1.0 — transition complete, dropping blend");
                                None
                            } else {
                                blend_params
                            };

                            let mut decoder_b_exhausted = false;
                            let send_data = if let Some((clip_b_path, clip_b_start, alpha, kind, decoder_b_aspect)) = blend_params {
                                let blended = (|| -> Option<Vec<u8>> {
                                    if let Some(b) = blend.as_mut() {
                                        let invert = b.spec.invert_ab;
                                        if b.decoder_b.is_none() {
                                            eprintln!("[blend] decoder_b is None — opening lazily, clip_b_start={clip_b_start:.3}");
                                            let t_open = std::time::Instant::now();
                                            let primary_size = decoder.as_ref().map(|(_, d)| (d.out_w, d.out_h));
                                            match LiveDecoder::open(&clip_b_path, clip_b_start, decoder_b_aspect, None, primary_size) {
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
                                                if data_b.len() != data.len() || wb != w || hb != h {
                                                    eprintln!(
                                                        "[pb] blend size mismatch — primary {}×{} ({} B) \
                                                         vs decoder_b {}×{} ({} B); skipping blend",
                                                        w, h, data.len(), wb, hb, data_b.len()
                                                    );
                                                    return None;
                                                }
                                                let blended = if invert {
                                                    blend_rgba_transition(&data_b, &data, w, h, alpha, kind)
                                                } else {
                                                    blend_rgba_transition(&data, &data_b, w, h, alpha, kind)
                                                };
                                                return Some(blended);
                                            } else {
                                                let still_burning = b.decoder_b.as_ref()
                                                    .map(|db| db.skip_until_pts > 0)
                                                    .unwrap_or(false);
                                                if still_burning {
                                                    let db = b.decoder_b.as_ref().unwrap();
                                                    eprintln!("[blend] still_burning: skip_until_pts={} last_pts={} gap_pts={}",
                                                        db.skip_until_pts, db.last_pts, db.skip_until_pts - db.last_pts);
                                                    if invert {
                                                        if let Some(hb) = held_blend.as_ref() {
                                                            if hb.len() == data.len() {
                                                                eprintln!("[blend] still_burning animated: alpha={alpha:.3}");
                                                                return Some(blend_rgba_transition(hb, &data, w, h, alpha, kind));
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
                                if decoder_b_exhausted { blend = None; }
                                // [Fix 2] Avoid double-allocation in the blend hot path.
                                // Previously: held_blend = Some(b.clone()); b
                                // — two Vec<u8> of equal size lived simultaneously.
                                // Now: move b into held_blend, clone once for the return.
                                // One allocation instead of two per blended frame.
                                match blended {
                                    Some(b) => {
                                        if held_streak > 0 {
                                            eprintln!("[blend] held_blend streak ended after {held_streak} frames");
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
                                            eprintln!("[blend] held_blend streak START (ts={ts_secs:.3} alpha from blend_params pending)");
                                        }
                                        held_blend.clone().unwrap_or(data)
                                    }
                                }
                            } else {
                                data
                            };

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
                            let outgoing_blend = blend.as_ref().map(|b| !b.spec.invert_ab).unwrap_or(false);
                            if outgoing_blend && held_blend.is_some() {
                                coast_last_alpha = last_blend_alpha;
                                eprintln!("[pb] primary EOF during outgoing blend — entering coast mode \
                                           (ts={coast_ts:.3}, alpha={coast_last_alpha:.3}, \
                                           decoder_b preserved for animated coast)");
                                coasting = true;
                            } else {
                                eprintln!("[pb] primary decoder EOF, clearing decoder + blend");
                                held_blend        = None;
                                coast_last_primary = None;
                                blend             = None;
                            }
                            decoder = None;
                        }
                    }
                } else {
                    // Idle branch: no primary decoder.
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
                        let was_coasting = coasting;
                        coasting = false;
                        match cmd {
                            PlaybackCmd::Start { id, path, ts, aspect, preview_size } => {
                                blend            = None;
                                held_blend       = None;
                                last_blend_alpha = 0.0;
                                coast_last_alpha = 0.5;
                                let t0 = std::time::Instant::now();
                                eprintln!("[pb] Start received (idle), ts={ts:.3}");
                                match LiveDecoder::open(&path, ts, aspect, None, preview_size) {
                                    Ok(mut d) => {
                                        let tpts = d.ts_to_pts(ts);
                                        d.burn_to_pts(tpts);
                                        eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                        decoder = Some((id, d));
                                    }
                                    Err(e) => eprintln!("[pb] open: {e}"),
                                }
                            }
                            PlaybackCmd::StartBlend { id, path, ts, aspect, blend: mut spec, preview_size } => {
                                let invert = spec.invert_ab;

                                if invert && was_coasting {
                                    eprintln!("[pb] incoming StartBlend while coasting: overriding \
                                               alpha_start {:.3} → {:.3} (coast_last_alpha)",
                                               spec.alpha_start, coast_last_alpha);
                                    spec.alpha_start = coast_last_alpha;
                                }

                                let coast_blend_db = if was_coasting && invert {
                                    blend.take().and_then(|b| b.decoder_b)
                                } else {
                                    drop(blend.take());
                                    None
                                };

                                let prebuilt_db = if was_coasting && invert {
                                    let db_path  = spec.clip_b_path.clone();
                                    let db_start = spec.clip_b_source_start;
                                    let t_db     = std::time::Instant::now();
                                    eprintln!("[pb] pre-opening decoder_b for clip_a tail at {db_start:.3}");
                                    match LiveDecoder::open(&db_path, db_start, aspect, None, preview_size) {
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

                                let mut burn_ts = ts;

                                if let Some(mut bridge_db) = coast_blend_db {
                                    coast_id = id;
                                    let bridge_duration = spec.duration;
                                    let bridge_kind     = spec.kind;
                                    let mut bridge_ts   = ts;
                                    // [Fix 1] BRIDGE_TARGET reduced from 28 → 4 to match
                                    // the new channel size of 6. The old 28/32 fill ratio
                                    // is preserved (4/6 ≈ 67%), ensuring the channel stays
                                    // fed through the primary burn without flooding it.
                                    const BRIDGE_TARGET: usize = 2; // 2/3 fill ratio matches old 4/6
                                    while pb_frame_tx.len() < BRIDGE_TARGET {
                                        let fa = match coast_last_primary.as_ref() {
                                            Some(f) => f,
                                            None    => break,
                                        };
                                        let (data_b, _, _, _) = match bridge_db.next_frame() {
                                            Some(f) => f,
                                            None    => break,
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
                                        // [Fix 2] Move blended into held_blend, clone for send.
                                        // Previously: held_blend = Some(blended.clone()); ... data: blended
                                        // — two equal-sized allocations per bridge frame.
                                        let send = blended.clone();
                                        held_blend = Some(blended);
                                        let f = PlaybackFrame {
                                            id, timestamp: bridge_ts,
                                            width: coast_w, height: coast_h, data: send,
                                        };
                                        if pb_frame_tx.send(f).is_err() { return; }
                                    }
                                    spec.alpha_start = coast_last_alpha;
                                    burn_ts = bridge_ts;
                                    eprintln!("[pb] bridge done: alpha_start updated to {:.3}, burn_ts={burn_ts:.3}, chan_filled={}", spec.alpha_start, pb_frame_tx.len());
                                }

                                blend = Some(ActiveBlend { spec, aspect, decoder_b: prebuilt_db, preview_size });
                                if !invert { held_blend = None; }
                                let t0 = std::time::Instant::now();
                                eprintln!("[pb] StartBlend received (idle), ts={ts:.3} burn_ts={burn_ts:.3}");
                                match LiveDecoder::open(&path, burn_ts, aspect, None, preview_size) {
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
                        // Coast mode, no command yet.
                        if pb_frame_tx.len() >= 5 {
                            std::thread::sleep(std::time::Duration::from_millis(4));
                        } else {
                            let animated = (|| -> Option<Vec<u8>> {
                                let b  = blend.as_mut()?;
                                let fa = coast_last_primary.as_ref()?;
                                let db = b.decoder_b.as_mut()?;
                                let (data_b, _, _, _) = db.next_frame()?;
                                if data_b.len() != fa.len() { return None; }
                                let step = (1.0_f32 / 30.0) / b.spec.duration;
                                coast_last_alpha = (coast_last_alpha + step).min(1.0);
                                coast_ts += 1.0 / 30.0;
                                Some(blend_rgba_transition(fa, &data_b, coast_w, coast_h, coast_last_alpha, b.spec.kind))
                            })();

                            let send_data = if let Some(blended) = animated {
                                eprintln!("[pb] coast animated: ts={coast_ts:.3} alpha={coast_last_alpha:.3}");
                                // [Fix 2] Move into held_blend, clone for send.
                                let out = blended.clone();
                                held_blend = Some(blended);
                                Some(out)
                            } else {
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
            hq_sem:         Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let cancels = self.encode_cancels.lock().unwrap();
        for flag in cancels.values() {
            flag.store(true, Ordering::Relaxed);
        }
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

        std::thread::spawn(move || {
            // Limit = 2: each full-pipeline probe peak-allocates ~100+ MB for
            // extract_audio (full Vec<f32> of all PCM before WAV write).
            // With 3 clips and limit=4 all three ran simultaneously (~318 MB peak).
            // Limit=2 caps that at ~212 MB. The semaphore stays active through
            // the ENTIRE probe (including waveform + audio) — previously it was
            // released after thumbnail, which defeated the limit entirely.
            const PROBE_CONCURRENCY: u32 = 2;
            {
                let (lock, cvar) = &*sem;
                let mut count = lock.lock().unwrap();
                while *count >= PROBE_CONCURRENCY {
                    count = cvar.wait(count).unwrap();
                }
                *count += 1;
            }
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

            // NOTE: do NOT drop(_guard) here. extract_waveform and extract_audio
            // must run under the semaphore — they are the expensive operations.
            if sd.load(Ordering::Relaxed) { return; }
            extract_waveform(&path, id, &tx);
            if sd.load(Ordering::Relaxed) { return; }
            if dur > 0.0 {
                extract_audio(&path, id, 0.0, f64::MAX, &tx);
            }
        });
    }

    /// No-op for compat — thumbnails now come back via probe_clip as RGBA data.
    pub fn reload_thumbnail(&self, id: Uuid, path: PathBuf) {
        self.probe_clip(id, path);
    }

    /// Re-extract the WAV temp file for an audio overlay, restricted to
    /// `[source_offset, source_offset + duration)`.
    ///
    /// The UI should call this whenever an audio overlay's trim changes so that
    /// the rodio playback buffer only holds the audible portion of the source
    /// file rather than the full duration.  For a 157-second MP3 trimmed to
    /// 28 seconds this reduces the temp WAV from ~55 MB to ~10 MB.
    pub fn extract_audio_trimmed(
        &self,
        id:            Uuid,
        path:          PathBuf,
        source_offset: f64,
        duration:      f64,
    ) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        thread::spawn(move || {
            if sd.load(Ordering::Relaxed) { return; }
            extract_audio(&path, id, source_offset, duration, &tx);
        });
    }

    pub fn request_frame(&self, id: Uuid, path: PathBuf, timestamp: f64, aspect: f32) {
        let (lock, cvar) = &*self.frame_req;
        *lock.lock().unwrap() = Some(FrameRequest { id, path, timestamp, aspect });
        cvar.notify_one();
    }

    pub fn request_frame_hq(&self, id: Uuid, path: PathBuf, timestamp: f64) {
        let tx  = self.scrub_tx.clone();
        let sd  = self.shutdown.clone();
        let sem = self.hq_sem.clone();
        thread::spawn(move || {
            // Acquire the HQ semaphore before opening any FFmpeg context.
            // Without this, rapid L3-idle updates spawn N threads simultaneously,
            // each holding a native-res decoder + scaler + frame buffer (~16 MB).
            const HQ_CONCURRENCY: u32 = 2;
            {
                let (lock, cvar) = &*sem;
                let mut c = lock.lock().unwrap();
                while *c >= HQ_CONCURRENCY { c = cvar.wait(c).unwrap(); }
                *c += 1;
            }
            let _guard = {
                struct G(Arc<(Mutex<u32>, Condvar)>);
                impl Drop for G {
                    fn drop(&mut self) {
                        let (lock, cvar) = &*self.0;
                        *lock.lock().unwrap() -= 1;
                        cvar.notify_one();
                    }
                }
                G(sem)
            };
            if sd.load(Ordering::Relaxed) { return; }
            if let Err(e) = decode_frame(&path, id, timestamp, 0.0, false, None, &tx) {
                eprintln!("[media] request_frame_hq: {e}");
            }
        });
    }

    pub fn request_transition_frame(&self, req: TransitionScrubRequest) {
        let scrub_tx = self.scrub_tx.clone();
        let sd       = self.shutdown.clone();
        let sem      = self.hq_sem.clone();
        thread::spawn(move || {
            // Two decoders opened per call (clip_a + clip_b) — gate on hq_sem.
            const HQ_CONCURRENCY: u32 = 2;
            {
                let (lock, cvar) = &*sem;
                let mut c = lock.lock().unwrap();
                while *c >= HQ_CONCURRENCY { c = cvar.wait(c).unwrap(); }
                *c += 1;
            }
            let _guard = {
                struct G(Arc<(Mutex<u32>, Condvar)>);
                impl Drop for G {
                    fn drop(&mut self) {
                        let (lock, cvar) = &*self.0;
                        *lock.lock().unwrap() -= 1;
                        cvar.notify_one();
                    }
                }
                G(sem)
            };
            if sd.load(Ordering::Relaxed) { return; }
            let (data_a, w, h) = match decode_one_frame_rgba(&req.clip_a_path, req.clip_a_ts, 1.0) {
                Ok(f)  => f,
                Err(e) => { eprintln!("[transition] clip_a decode: {e}"); return; }
            };
            if sd.load(Ordering::Relaxed) { return; }
            let (data_b_raw, wb, hb) = match decode_one_frame_rgba(&req.clip_b_path, req.clip_b_ts, 1.0) {
                Ok(f)  => f,
                Err(e) => { eprintln!("[transition] clip_b decode: {e}"); return; }
            };
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

    pub fn request_transition_frame_hq(&self, req: TransitionScrubRequest) {
        let scrub_tx = self.scrub_tx.clone();
        let sd       = self.shutdown.clone();
        let sem      = self.hq_sem.clone();
        thread::spawn(move || {
            // Two native-res decoders opened per call — gate on hq_sem.
            const HQ_CONCURRENCY: u32 = 2;
            {
                let (lock, cvar) = &*sem;
                let mut c = lock.lock().unwrap();
                while *c >= HQ_CONCURRENCY { c = cvar.wait(c).unwrap(); }
                *c += 1;
            }
            let _guard = {
                struct G(Arc<(Mutex<u32>, Condvar)>);
                impl Drop for G {
                    fn drop(&mut self) {
                        let (lock, cvar) = &*self.0;
                        *lock.lock().unwrap() -= 1;
                        cvar.notify_one();
                    }
                }
                G(sem)
            };
            if sd.load(Ordering::Relaxed) { return; }
            let (data_a, w, h) = match decode_one_frame_rgba(&req.clip_a_path, req.clip_a_ts, 0.0) {
                Ok(f)  => f,
                Err(e) => { eprintln!("[transition_hq] clip_a decode: {e}"); return; }
            };
            if sd.load(Ordering::Relaxed) { return; }
            let (data_b_raw, wb, hb) = match decode_one_frame_rgba(&req.clip_b_path, req.clip_b_ts, 0.0) {
                Ok(f)  => f,
                Err(e) => { eprintln!("[transition_hq] clip_b decode: {e}"); return; }
            };
            let data_b = if wb != w || hb != h {
                eprintln!(
                    "[transition_hq] clip_b size {}x{} differs from clip_a {}x{}; cropping",
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

    /// `preview_size` — the actual pixel dimensions of the preview panel (e.g. 960×540).
    /// The playback decoder will scale output to this size instead of native resolution,
    /// dramatically reducing swscale CPU and channel memory:
    ///   • 1080p native: 8 MB/frame × 6 frames = 48 MB in channel, ~8% CPU (swscale)
    ///   • 960×540:      2 MB/frame × 6 frames = 12 MB in channel, ~0.5% CPU
    /// Pass None to decode at native resolution (not recommended for preview).
    pub fn start_playback(&self, id: Uuid, path: PathBuf, ts: f64, aspect: f32, preview_size: Option<(u32, u32)>) {
        if self.pb_tx.try_send(PlaybackCmd::Start { id, path, ts, aspect, preview_size }).is_err() {
            eprintln!("[pb] start_playback: command channel full — Start dropped. This is a bug.");
        }
        while self.pb_rx.try_recv().is_ok() {}
    }

    pub fn start_blend_playback(
        &self,
        id:           Uuid,
        path:         PathBuf,
        ts:           f64,
        aspect:       f32,
        blend:        PlaybackTransitionSpec,
        preview_size: Option<(u32, u32)>,
    ) {
        if self.pb_tx.try_send(PlaybackCmd::StartBlend { id, path, ts, aspect, blend, preview_size }).is_err() {
            eprintln!("[pb] start_blend_playback: command channel full — StartBlend dropped. This is a bug.");
        }
        while self.pb_rx.try_recv().is_ok() {}
    }

    pub fn stop_playback(&self) {
        if self.pb_tx.try_send(PlaybackCmd::Stop).is_err() {
            eprintln!("[pb] stop_playback: command channel full — Stop dropped. This is a bug.");
        }
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

    pub fn start_encode(&self, spec: EncodeSpec) {
        let job_id = spec.job_id;
        let cancel = Arc::new(AtomicBool::new(false));
        let tx     = self.tx.clone();
        let sd     = self.shutdown.clone();

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

            cancels_ref.lock().unwrap().remove(&job_id);
        });
    }

    pub fn cancel_encode(&self, job_id: Uuid) {
        if let Some(flag) = self.encode_cancels.lock().unwrap().get(&job_id) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

// ── Blend decoder helpers ─────────────────────────────────────────────────────

struct ActiveBlend {
    spec:         velocut_core::media_types::PlaybackTransitionSpec,
    aspect:       f32,
    decoder_b:    Option<LiveDecoder>,
    preview_size: Option<(u32, u32)>,
}

// ── RGBA crop helper ──────────────────────────────────────────────────────────

fn crop_rgba(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let src_ar = src_w as f32 / src_h.max(1) as f32;
    let dst_ar = dst_w as f32 / dst_h.max(1) as f32;

    let (off_x, off_y, used_w, used_h) = if src_ar > dst_ar {
        let used_w = (src_h as f32 * dst_ar) as u32;
        let off_x  = (src_w - used_w) / 2;
        (off_x, 0u32, used_w, src_h)
    } else {
        let used_h = (src_w as f32 / dst_ar) as u32;
        let off_y  = (src_h - used_h) / 2;
        (0u32, off_y, src_w, used_h)
    };

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