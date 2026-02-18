// crates/velocut-media/src/worker.rs
//
// MediaWorker: owns the frame-request slot and playback decode thread.
// All public API that velocut-ui calls lives here.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Condvar, atomic::{AtomicBool, Ordering}};
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use uuid::Uuid;

use velocut_core::media_types::{MediaResult, PlaybackFrame};

use crate::decode::{LiveDecoder, decode_frame};
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
    Stop,
}

// ── MediaWorker ───────────────────────────────────────────────────────────────

pub struct MediaWorker {
    pub rx:    Receiver<MediaResult>,
    tx:        Sender<MediaResult>,
    /// Latest-wins slot for on-demand scrub frames.
    frame_req: Arc<(Mutex<Option<FrameRequest>>, Condvar)>,
    /// Dedicated playback pipeline.
    pb_tx:     Sender<PlaybackCmd>,
    pub pb_rx: Receiver<PlaybackFrame>,
    shutdown:  Arc<AtomicBool>,
    /// Limits concurrent probe threads: (active_count, Condvar). Max = PROBE_CONCURRENCY.
    probe_sem: Arc<(Mutex<u32>, Condvar)>,
}

impl MediaWorker {
    pub fn new() -> Self {
        let (tx, rx) = bounded(512);
        let frame_req: Arc<(Mutex<Option<FrameRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));

        // ── Scrub frame decode thread ─────────────────────────────────────────
        // Blocks on the latest-wins slot; reuses the LiveDecoder when possible.
        let result_tx = tx.clone();
        let slot      = Arc::clone(&frame_req);
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
                    match LiveDecoder::open(&req.path, req.timestamp, req.aspect) {
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
                                let _ = result_tx.send(MediaResult::VideoFrame {
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
                        let _ = result_tx.send(MediaResult::VideoFrame {
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
            loop {
                if let Some((id, ref mut d)) = decoder {
                    match pb_cmd_rx.try_recv() {
                        Ok(PlaybackCmd::Start { id: new_id, path, ts, aspect }) => {
                            match LiveDecoder::open(&path, ts, aspect) {
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
                                    decoder = Some((new_id, nd));
                                }
                                Err(e) => { eprintln!("[pb] open: {e}"); decoder = None; }
                            }
                            continue;
                        }
                        Ok(PlaybackCmd::Stop) => { decoder = None; continue; }
                        Err(TryRecvError::Disconnected) => return,
                        Err(TryRecvError::Empty) => {}
                    }
                    // Decode next frame. send() blocks when channel is full —
                    // that IS the rate-limiter, no sleep needed.
                    match d.next_frame() {
                        Some((data, w, h, ts_secs)) => {
                            let f = PlaybackFrame { id, timestamp: ts_secs, width: w, height: h, data };
                            if pb_frame_tx.send(f).is_err() { return; }
                        }
                        None => { decoder = None; } // EOF
                    }
                } else {
                    match pb_cmd_rx.recv() {
                        Ok(PlaybackCmd::Start { id, path, ts, aspect }) => {
                            match LiveDecoder::open(&path, ts, aspect) {
                                Ok(mut d) => {
                                    let tpts = d.ts_to_pts(ts);
                                    d.burn_to_pts(tpts);
                                    decoder = Some((id, d));
                                }
                                Err(e) => eprintln!("[pb] open: {e}"),
                            }
                        }
                        Ok(PlaybackCmd::Stop) => {}
                        Err(_) => return,
                    }
                }
            }
        });

        Self {
            rx, tx, frame_req, pb_tx, pb_rx,
            shutdown:  Arc::new(AtomicBool::new(false)),
            probe_sem: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
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
            // thumbnail) is done. Waveform and audio use blocking CLI subprocesses
            // that can run for seconds on long files. Holding the semaphore through
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

    /// Start the dedicated playback pipeline at `ts` seconds into `path`.
    pub fn start_playback(&self, id: Uuid, path: PathBuf, ts: f64, aspect: f32) {
        // Flush stale frames from previous playback session.
        while self.pb_rx.try_recv().is_ok() {}
        let _ = self.pb_tx.try_send(PlaybackCmd::Start { id, path, ts, aspect });
    }

    /// Stop the dedicated playback pipeline.
    pub fn stop_playback(&self) {
        let _ = self.pb_tx.try_send(PlaybackCmd::Stop);
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
}