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
            loop {
                if let Some((id, ref mut d)) = decoder {
                    match pb_cmd_rx.try_recv() {
                        Ok(PlaybackCmd::Start { id: new_id, path, ts, aspect }) => {
                            blend = None; // clear any pending transition
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
                            // For invert_ab=true (clip_b incoming half), pre-open decoder_b
                            // synchronously here with burn_to_pts so the frame loop never
                            // enters the still_burning path. The still_burning path sends raw
                            // unblended clip_b frames (no iris/wipe/etc.) for every burn chunk,
                            // causing a visible 1-3 frame glitch at the start of the clip_b half.
                            //
                            // decoder_b here is clip_a tail — burn distance is bounded by the
                            // GOP size near clip_a's end (~1s at most for NLE-friendly exports).
                            // That's ~30 frames × 1ms = ~30ms synchronous block, which is
                            // indistinguishable from the primary burn that already happens here.
                            //
                            // For invert_ab=false (clip_a outgoing half), decoder_b is clip_b
                            // at source_offset — keep lazy open there since that path works.
                            let invert = spec.invert_ab;
                            let clip_b_path  = spec.clip_b_path.clone();
                            let clip_b_start = spec.clip_b_source_start;
                            blend = Some(ActiveBlend { spec, aspect, decoder_b: None });
                            let t0 = std::time::Instant::now();
                            eprintln!("[pb] StartBlend received (active), ts={ts:.3}");
                            match LiveDecoder::open(&path, ts, aspect, None) {
                                Ok(mut nd) => {
                                    let tpts = nd.ts_to_pts(ts);
                                    nd.burn_to_pts(tpts);
                                    eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                    decoder = Some((new_id, nd));
                                    // Pre-open decoder_b for the incoming blend half only.
                                    if invert {
                                        match LiveDecoder::open(&clip_b_path, clip_b_start, aspect, None) {
                                            Ok(mut db) => {
                                                let tpts_b = db.ts_to_pts(clip_b_start);
                                                db.burn_to_pts(tpts_b);
                                                eprintln!("[pb] decoder_b (clip_a tail) pre-burned in {}ms", t0.elapsed().as_millis());
                                                if let Some(b) = blend.as_mut() {
                                                    b.decoder_b = Some(db);
                                                }
                                            }
                                            Err(e) => eprintln!("[pb] decoder_b pre-open (active): {e}"),
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[pb] open (blend): {e}");
                                    decoder = None;
                                    blend   = None; // clear orphaned blend on primary failure
                                }
                            }
                            continue;
                        }
                        Ok(PlaybackCmd::Stop) => { decoder = None; blend = None; continue; }
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
                                        Some((b.spec.clip_b_path.clone(), b.spec.clip_b_source_start, alpha, b.spec.kind, b.aspect))
                                    } else {
                                        None
                                    }
                                })
                            };

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
                                            match LiveDecoder::open(&clip_b_path, clip_b_start, decoder_b_aspect, None) {
                                                Ok(mut db) => {
                                                    // Seek decoder_b to the exact target position.
                                                    // open() already seeked to the nearest keyframe; without
                                                    // skip_until_pts decoder_b would start from that keyframe
                                                    // — possibly seconds before clip_a_tail — showing the
                                                    // wrong content (e.g. wide-shot when the end of clip_a
                                                    // is a close-up) for the entire clip_b blend half.
                                                    //
                                                    // next_frame() now uses chunked skip (MAX_SKIP_PACKETS=60
                                                    // per call, ~30 ms each) and returns None with
                                                    // skip_until_pts still set while burning. The else branch
                                                    // below distinguishes "still burning" (send raw primary)
                                                    // from EOF (clear blend), so the pb thread stays live.
                                                    let tpts = db.ts_to_pts(clip_b_start);
                                                    db.skip_until_pts = tpts;
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
                                                // Distinguish the two: if still skipping, send a raw
                                                // primary frame this tick and let the burn continue
                                                // next iteration. If skip is done (pts cleared to 0)
                                                // and we got None, decoder_b has hit EOF.
                                                let still_burning = b.decoder_b.as_ref()
                                                    .map(|db| db.skip_until_pts > 0)
                                                    .unwrap_or(false);
                                                if still_burning {
                                                    return None; // raw primary this tick; burn continues
                                                }
                                                decoder_b_exhausted = true;
                                            }
                                        }
                                    }
                                    None
                                })();
                                // Must run after closure releases its borrow on blend.
                                if decoder_b_exhausted { blend = None; }
                                blended.unwrap_or(data)
                            } else {
                                data
                            };

                            let f = PlaybackFrame { id, timestamp: ts_secs, width: w, height: h, data: send_data };
                            frame_count += 1;
                            if frame_count % 60 == 0 {
                                eprintln!("[pb] frame #{frame_count} sent, ts={ts_secs:.3}");
                            }
                            if pb_frame_tx.send(f).is_err() { return; }
                        }
                        None => {
                            eprintln!("[pb] primary decoder EOF, clearing decoder + blend");
                            decoder = None;
                            blend   = None;
                        }
                    }
                } else {
                    match pb_cmd_rx.recv() {
                        Ok(PlaybackCmd::Start { id, path, ts, aspect }) => {
                            blend = None;
                            match LiveDecoder::open(&path, ts, aspect, None) {
                                Ok(mut d) => {
                                    let tpts = d.ts_to_pts(ts);
                                    d.burn_to_pts(tpts);
                                    decoder = Some((id, d));
                                }
                                Err(e) => eprintln!("[pb] open: {e}"),
                            }
                        }
                        Ok(PlaybackCmd::StartBlend { id, path, ts, aspect, blend: spec }) => {
                            // decoder_b is always opened lazily in the frame loop
                            // (see ActiveBlend / NOTE block for why eager/async was abandoned).
                            let invert = spec.invert_ab;
                            let clip_b_path  = spec.clip_b_path.clone();
                            let clip_b_start = spec.clip_b_source_start;
                            blend = Some(ActiveBlend { spec, aspect, decoder_b: None });
                            let t0 = std::time::Instant::now();
                            eprintln!("[pb] StartBlend received (idle), ts={ts:.3}");
                            match LiveDecoder::open(&path, ts, aspect, None) {
                                Ok(mut d) => {
                                    let tpts = d.ts_to_pts(ts);
                                    d.burn_to_pts(tpts);
                                    eprintln!("[pb] primary burn done in {}ms", t0.elapsed().as_millis());
                                    decoder = Some((id, d));
                                    // Pre-open decoder_b for the incoming blend half only.
                                    if invert {
                                        match LiveDecoder::open(&clip_b_path, clip_b_start, aspect, None) {
                                            Ok(mut db) => {
                                                let tpts_b = db.ts_to_pts(clip_b_start);
                                                db.burn_to_pts(tpts_b);
                                                eprintln!("[pb] decoder_b (clip_a tail) pre-burned in {}ms", t0.elapsed().as_millis());
                                                if let Some(b) = blend.as_mut() {
                                                    b.decoder_b = Some(db);
                                                }
                                            }
                                            Err(e) => eprintln!("[pb] decoder_b pre-open (idle): {e}"),
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[pb] open (blend, idle): {e}");
                                    // Bug 3 fix: blend was set just above; clear it here so we
                                    // do not hold a dangling blend with no primary decoder.
                                    blend = None;
                                }
                            }
                        }
                        Ok(PlaybackCmd::Stop) => { blend = None; }
                        Err(_) => return,
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
// Used by both the scrub (request_transition_frame) and playback (pb thread
// blend loop) paths to produce a blended RGBA frame from two source frames.
//
// Implements the same visual effects as the YUV VideoTransition::apply() impls
// in velocut-core but operates directly in RGBA — no format conversion needed
// since LiveDecoder / decode_one_frame_rgba already output RGBA.
//
// Easing and blend helpers are imported from velocut_core::transitions::helpers
// so the math is the same code as the encoder uses.

fn blend_rgba_transition(
    a:     &[u8],
    b:     &[u8],
    w:     u32,
    h:     u32,
    alpha: f32,
    kind:  velocut_core::transitions::TransitionKind,
) -> Vec<u8> {
    use velocut_core::transitions::TransitionKind;
    use velocut_core::transitions::helpers::{
        blend_byte, ease_in_out, ease_in_out_cubic, wipe_alpha,
    };

    let len = (w * h) as usize * 4;
    let mut out = vec![0u8; len];

    match kind {
        TransitionKind::Cut => {
            out.copy_from_slice(a);
        }

        TransitionKind::Crossfade => {
            // Matches crossfade.rs: blend_byte(a, b, ease_in_out(alpha)) per channel.
            let t = ease_in_out(alpha);
            for i in 0..len {
                out[i] = blend_byte(a[i], b[i], t);
            }
        }

        TransitionKind::DipToBlack => {
            // Matches dip_to_black.rs: first half fades a→black, second half black→b.
            // RGB channels lerp; alpha channel copied unchanged.
            if alpha < 0.5 {
                let t = ease_in_out(alpha * 2.0);
                for i in (0..len).step_by(4) {
                    out[i]   = (a[i]   as f32 * (1.0 - t)) as u8;
                    out[i+1] = (a[i+1] as f32 * (1.0 - t)) as u8;
                    out[i+2] = (a[i+2] as f32 * (1.0 - t)) as u8;
                    out[i+3] = a[i+3];
                }
            } else {
                let t = ease_in_out((alpha - 0.5) * 2.0);
                for i in (0..len).step_by(4) {
                    out[i]   = (b[i]   as f32 * t) as u8;
                    out[i+1] = (b[i+1] as f32 * t) as u8;
                    out[i+2] = (b[i+2] as f32 * t) as u8;
                    out[i+3] = b[i+3];
                }
            }
        }

        TransitionKind::Wipe => {
            // Matches wipe.rs: left-to-right bar with 2% feather.
            // blend_byte(b, a, wa): wa=0 (left of bar) → b; wa=1 (right) → a.
            const FEATHER: f32 = 0.02;
            let edge = ease_in_out(alpha);
            for py in 0..h {
                for px in 0..w {
                    let nx = px as f32 / w as f32;
                    let wa = wipe_alpha(nx, edge, FEATHER);
                    let i  = (py * w + px) as usize * 4;
                    out[i]   = blend_byte(b[i],   a[i],   wa);
                    out[i+1] = blend_byte(b[i+1], a[i+1], wa);
                    out[i+2] = blend_byte(b[i+2], a[i+2], wa);
                    out[i+3] = 255;
                }
            }
        }

        TransitionKind::Push => {
            // Matches push.rs: b slides in from right, displacing a to the left.
            // Zero blending — hard pixel copy, no ghosting.
            let p        = ease_in_out_cubic(alpha);
            let boundary = ((1.0 - p) * w as f32) as i32;
            let shift_a  = (p * w as f32) as i32;
            for py in 0..h as i32 {
                for px in 0..w as i32 {
                    let i = (py * w as i32 + px) as usize * 4;
                    if px < boundary {
                        // Clip-a pixel, shifted left by shift_a.
                        let src_x = (px + shift_a).clamp(0, w as i32 - 1);
                        let s = (py * w as i32 + src_x) as usize * 4;
                        out[i..i+4].copy_from_slice(&a[s..s+4]);
                    } else {
                        // Clip-b pixel measured from the right edge sweeping in.
                        let src_x = (px - boundary).clamp(0, w as i32 - 1);
                        let s = (py * w as i32 + src_x) as usize * 4;
                        out[i..i+4].copy_from_slice(&b[s..s+4]);
                    }
                }
            }
        }

        TransitionKind::Iris => {
            // Matches iris.rs: circular aperture expands from center.
            // Inside aperture → b; outside → a.
            // blend_byte(b, a, wa): wa=0 (inside) → b; wa=1 (outside) → a.
            const FEATHER: f32 = 0.05;
            let p      = ease_in_out(alpha);
            // Max radius from center (0.5, 0.5) to corner in normalised [0..1]² space.
            let max_r  = 0.5f32.hypot(0.5);
            let radius = p * max_r;
            for py in 0..h {
                for px in 0..w {
                    let nx   = px as f32 / w as f32 - 0.5;
                    let ny   = py as f32 / h as f32 - 0.5;
                    let dist = (nx * nx + ny * ny).sqrt();
                    let wa   = wipe_alpha(dist, radius, FEATHER);
                    let i    = (py * w + px) as usize * 4;
                    out[i]   = blend_byte(b[i],   a[i],   wa);
                    out[i+1] = blend_byte(b[i+1], a[i+1], wa);
                    out[i+2] = blend_byte(b[i+2], a[i+2], wa);
                    out[i+3] = 255;
                }
            }
        }
    }
    out
}