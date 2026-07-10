// crates/velocut-media/src/worker.rs
//
// MediaWorker: owns the frame-request slot and playback decode thread.
// All public API that velocut-ui calls lives here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use uuid::Uuid;

use velocut_core::media_types::{
    MediaResult, PlaybackFrame, PlaybackTransitionSpec, TransitionScrubRequest,
};

use crate::audio::extract_audio;
use crate::decode::{decode_frame, decode_one_frame_rgba, LiveDecoder};
use crate::encode::{encode_timeline, EncodeSpec};
use crate::probe::{probe_duration, probe_video_size_and_thumbnail};
use crate::waveform::extract_waveform;

mod blend;
use blend::{blend_rgba_transition, crop_rgba, decode_transition_scrub_frame};

mod types;
use types::{FrameRequest, PlaybackCmd};

mod semaphore;
use semaphore::SemaphoreGuard;

mod pb_thread;
use pb_thread::PbThread;

// ── MediaWorker ───────────────────────────────────────────────────────────────

pub struct MediaWorker {
    /// Shared result channel: probes, waveforms, audio, encode progress, HQ frames.
    pub rx: Receiver<MediaResult>,
    tx: Sender<MediaResult>,

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
    scrub_tx: Sender<MediaResult>,

    /// Latest-wins slot for on-demand scrub frames.
    frame_req: Arc<(Mutex<Option<FrameRequest>>, Condvar)>,
    /// Dedicated playback pipeline.
    pb_tx: Sender<PlaybackCmd>,
    pub pb_rx: Receiver<PlaybackFrame>,
    shutdown: Arc<AtomicBool>,
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
    /// Latest-wins slot for transition scrub frames (L2 scrub in a transition zone).
    /// Mirrors `frame_req` but carries a full `TransitionScrubRequest` and is
    /// consumed by a dedicated thread that keeps two `LiveDecoder`s alive across
    /// requests — avoiding the per-frame decoder open/seek/close overhead that
    /// made transition scrubbing laggy on CPU-only systems.
    transition_scrub_req: Arc<(Mutex<Option<TransitionScrubRequest>>, Condvar)>,

    // ── Thread handles (for graceful shutdown) ─────────────────────────────────
    /// Handle for the dedicated scrub-frame decode thread.
    scrub_thread: Option<thread::JoinHandle<()>>,
    /// Handle for the dedicated transition-scrub decode thread.
    transition_scrub_thread: Option<thread::JoinHandle<()>>,
    /// Handle for the dedicated playback decode thread.
    pb_thread: Option<thread::JoinHandle<()>>,
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        // Drop pb_tx first to disconnect the channel and wake the PB thread
        // from recv().  We need to do this before joining because Rust drops
        // fields in declaration order AFTER Drop::drop() returns.
        //
        // pb_tx is a crossbeam Sender; dropping it causes the Receiver in
        // the PB thread to return Disconnected, which exits its main loop.
        // We replace it with a dummy channel that's immediately dropped.
        let (dummy_tx, _) = bounded::<PlaybackCmd>(1);
        let old_tx = std::mem::replace(&mut self.pb_tx, dummy_tx);
        drop(old_tx);

        // Now join the PB thread — it should exit within a few ms.
        if let Some(h) = self.pb_thread.take() {
            let _ = h.join();
        }
    }
}

impl Default for MediaWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaWorker {
    pub fn new() -> Self {
        let (tx, rx) = bounded(512);
        let (scrub_tx, scrub_rx) = bounded(8); // [Opt 3] dedicated scrub channel

        let frame_req: Arc<(Mutex<Option<FrameRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));

        // ── Scrub frame decode thread ─────────────────────────────────────────
        // Blocks on the latest-wins slot; reuses the LiveDecoder when possible.
        // [Opt 3] Sends VideoFrame on scrub_tx (not tx) so scrub results bypass
        // the shared channel and are consumed with lower latency under probe load.
        let scrub_result_tx = scrub_tx.clone();
        let slot = Arc::clone(&frame_req);
        let scrub_thread = thread::spawn(move || {
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
                if req.id == Uuid::nil() {
                    return;
                }

                let needs_open = live.as_ref().map(|d| d.path != req.path).unwrap_or(true);

                if needs_open {
                    // Different file or first request — open a fresh decoder.
                    let cached_sws = live
                        .take()
                        .map(|d| (d.scaler, d.decoder_fmt, d.decoder_w, d.decoder_h));
                    // When preview_size is known, pass it as forced_size so the
                    // decoder output matches the panel dimensions exactly.
                    // Otherwise fall back to the aspect-based 320px scrub size.
                    let forced = if req.preview_size.is_some() {
                        req.preview_size
                    } else {
                        // No preview size and no aspect: let LiveDecoder use its
                        // 320px default scrub size.
                        None
                    };
                    match LiveDecoder::open(
                        &req.path,
                        req.timestamp,
                        req.aspect,
                        cached_sws,
                        forced,
                    ) {
                        Ok(mut d) => {
                            let target_pts = d.ts_to_pts(req.timestamp);
                            if let Some((data, w, h)) = d.advance_to(target_pts) {
                                let _ = scrub_result_tx.send(MediaResult::VideoFrame {
                                    id: req.id,
                                    width: w,
                                    height: h,
                                    data,
                                });
                            }
                            live = Some(d);
                        }
                        Err(e) => crate::media_log!("[media] LiveDecoder::open: {e}"),
                    }
                } else if let Some(d) = &mut live {
                    let tpts = d.ts_to_pts(req.timestamp);
                    // Seek within the existing decoder instead of reopening when:
                    //   a) backward movement - advance_to can only go forward
                    //   b) large forward jump > 2 s - avoid decoding hundreds of frames
                    let needs_seek = tpts < d.last_pts || tpts > d.last_pts + d.ts_to_pts(2.0);
                    if needs_seek {
                        if let Err(e) = d.seek_to(req.timestamp) {
                            crate::media_log!("[media] seek_to failed: {e}");
                            continue;
                        }
                        // After a seek, burn_to_pts positions the decoder at the target.
                        // Use next_frame to get the frame at or just after the target.
                        if let Some((data, w, h, _ts)) = d.next_frame() {
                            let _ = scrub_result_tx.send(MediaResult::VideoFrame {
                                id: req.id,
                                width: w,
                                height: h,
                                data,
                            });
                        }
                    } else if let Some((data, w, h)) = d.advance_to(tpts) {
                        let _ = scrub_result_tx.send(MediaResult::VideoFrame {
                            id: req.id,
                            width: w,
                            height: h,
                            data,
                        });
                    }
                }
            }
        });

        // ── Dedicated transition scrub thread ────────────────────────────────
        // Mirrors the single-clip scrub thread above but keeps TWO LiveDecoders
        // alive (clip_a + clip_b) so consecutive transition-zone scrub frames
        // reuse the open decoders instead of opening fresh ones each frame.
        // On CPU-only systems this cuts transition scrub latency from ~80-200ms
        // (two full decoder opens per frame) to ~5-15ms (advance_to only).
        let transition_scrub_req: Arc<(Mutex<Option<TransitionScrubRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let transition_slot = Arc::clone(&transition_scrub_req);
        let transition_scrub_result_tx = scrub_tx.clone();
        let transition_scrub_thread = thread::spawn(move || {
            let mut live_a: Option<(PathBuf, LiveDecoder)> = None;
            let mut live_b: Option<(PathBuf, LiveDecoder)> = None;
            // Cache the last successfully decoded frame for each slot so we can
            // freeze on the last frame when the decoder hits EOF (e.g. clip_a
            // timestamp clamped to its source end during the second half of a
            // centered transition).
            let mut last_a: Option<(Vec<u8>, u32, u32)> = None;
            let mut last_b: Option<(Vec<u8>, u32, u32)> = None;
            loop {
                let req: TransitionScrubRequest = {
                    let (lock, cvar) = &*transition_slot;
                    let mut guard = lock.lock().unwrap();
                    while guard.is_none() {
                        guard = cvar.wait(guard).unwrap();
                    }
                    guard.take().unwrap()
                };

                // Poison-pill: nil clip_a_id signals shutdown.
                if req.clip_a_id == Uuid::nil() {
                    return;
                }

                // ── Decode clip_a frame ────────────────────────────────────────
                let frame_a =
                    decode_transition_scrub_frame(&mut live_a, &req.clip_a_path, req.clip_a_ts);
                let (data_a, w, h) = match frame_a {
                    Some(f) => {
                        last_a = Some(f.clone());
                        f
                    }
                    None => match last_a.as_ref() {
                        Some(cached) => cached.clone(),
                        None => continue,
                    },
                };

                // ── Decode clip_b frame ────────────────────────────────────────
                let frame_b =
                    decode_transition_scrub_frame(&mut live_b, &req.clip_b_path, req.clip_b_ts);
                let data_b = match frame_b {
                    Some((data_b_raw, wb, hb)) => {
                        let sized = if wb == w && hb == h {
                            data_b_raw
                        } else {
                            crop_rgba(&data_b_raw, wb, hb, w, h)
                        };
                        last_b = Some((sized.clone(), w, h));
                        sized
                    }
                    None => match last_b.as_ref() {
                        Some((cached, cw, ch)) if *cw == w && *ch == h => cached.clone(),
                        Some((cached, cw, ch)) => crop_rgba(cached, *cw, *ch, w, h),
                        None => continue,
                    },
                };

                let blended = blend_rgba_transition(&data_a, &data_b, w, h, req.alpha, req.kind);
                let _ = transition_scrub_result_tx.send(MediaResult::TransitionVideoFrame {
                    id: req.clip_a_id,
                    width: w,
                    height: h,
                    data: blended,
                });
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
        let (pb_tx, pb_cmd_rx) = bounded::<PlaybackCmd>(8);
        let (pb_frame_tx, pb_rx) = bounded::<PlaybackFrame>(6);

        let pb = PbThread {
            cmd_rx: pb_cmd_rx,
            frame_tx: pb_frame_tx,
        };
        let pb_thread = thread::spawn(move || {
            pb.run();
        });

        Self {
            rx,
            tx,
            scrub_rx,
            scrub_tx,
            frame_req,
            pb_tx,
            pb_rx,
            shutdown: Arc::new(AtomicBool::new(false)),
            probe_sem: Arc::new((Mutex::new(0), Condvar::new())),
            encode_cancels: Arc::new(Mutex::new(HashMap::new())),
            hq_sem: Arc::new((Mutex::new(0), Condvar::new())),
            transition_scrub_req,
            scrub_thread: Some(scrub_thread),
            transition_scrub_thread: Some(transition_scrub_thread),
            pb_thread: Some(pb_thread),
        }
    }

    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let cancels = self.encode_cancels.lock().unwrap();
        for flag in cancels.values() {
            flag.store(true, Ordering::Release);
        }
        // Poison-pill the regular scrub thread.
        let (lock, cvar) = &*self.frame_req;
        *lock.lock().unwrap() = Some(FrameRequest {
            id: Uuid::nil(),
            path: std::path::PathBuf::new(),
            timestamp: 0.0,
            aspect: 0.0,
            preview_size: None,
        });
        cvar.notify_one();
        // Poison-pill the transition scrub thread.
        {
            let (lock, cvar) = &*self.transition_scrub_req;
            *lock.lock().unwrap() = Some(TransitionScrubRequest {
                clip_a_id: Uuid::nil(),
                clip_a_path: std::path::PathBuf::new(),
                clip_a_ts: 0.0,
                clip_b_id: Uuid::nil(),
                clip_b_path: std::path::PathBuf::new(),
                clip_b_ts: 0.0,
                alpha: 0.0,
                kind: velocut_core::transitions::TransitionKind::Cut,
            });
            cvar.notify_one();
        }
        // ── Join long-lived threads ─────────────────────────────────────────
        // The poison pills above cause each thread to exit its main loop
        // immediately.  We join them so all resources (LiveDecoders, SwsContexts,
        // scalers) are dropped cleanly before the process exits.
        //
        // For the PB thread: the active loop uses try_recv() and will see
        // Stop/Disconnected; the idle loop uses recv() which will wake when
        // pb_tx is dropped. We send a Stop then replace pb_tx with a dummy
        // (same pattern as Drop) so the idle recv() wakes up.
        let _ = self.pb_tx.try_send(PlaybackCmd::Stop);
        let (dummy_tx, _) = bounded::<PlaybackCmd>(1);
        let old_tx = std::mem::replace(&mut self.pb_tx, dummy_tx);
        drop(old_tx);

        // Joining is best-effort — if a thread is stuck in an FFmpeg call,
        // we don't want to hang the process.  In practice the poison pills
        // cause near-instant exit from all three thread loops.
        if let Some(h) = self.scrub_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.transition_scrub_thread.take() {
            let _ = h.join();
        }
        if let Some(h) = self.pb_thread.take() {
            let _ = h.join();
        }
    }

    pub fn probe_clip(&self, id: Uuid, path: PathBuf) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        let sem = self.probe_sem.clone();

        std::thread::spawn(move || {
            // Limit = 2: each full-pipeline probe peak-allocates ~100+ MB for
            // extract_audio (full Vec<f32> of all PCM before WAV write).
            // With 3 clips and limit=4 all three ran simultaneously (~318 MB peak).
            // Limit=2 caps that at ~212 MB. The semaphore stays active through
            // the ENTIRE probe (including waveform + audio) — previously it was
            // released after thumbnail, which defeated the limit entirely.
            const PROBE_CONCURRENCY: u32 = 2;
            let _guard = SemaphoreGuard::acquire(sem, PROBE_CONCURRENCY);

            if sd.load(Ordering::Acquire) {
                return;
            }
            let dur = probe_duration(&path, id, &tx);
            if sd.load(Ordering::Acquire) {
                return;
            }
            probe_video_size_and_thumbnail(&path, id, dur, &tx);

            // NOTE: do NOT drop(_guard) here. extract_waveform and extract_audio
            // must run under the semaphore — they are the expensive operations.
            if sd.load(Ordering::Acquire) {
                return;
            }
            extract_waveform(&path, id, &tx);
            if sd.load(Ordering::Acquire) {
                return;
            }
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
        id: Uuid,
        path: PathBuf,
        source_offset: f64,
        duration: f64,
    ) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        thread::spawn(move || {
            if sd.load(Ordering::Acquire) {
                return;
            }
            extract_audio(&path, id, source_offset, duration, &tx);
        });
    }

    pub fn request_frame(
        &self,
        id: Uuid,
        path: PathBuf,
        timestamp: f64,
        aspect: f32,
        preview_size: Option<(u32, u32)>,
    ) {
        let (lock, cvar) = &*self.frame_req;
        *lock.lock().unwrap() = Some(FrameRequest {
            id,
            path,
            timestamp,
            aspect,
            preview_size,
        });
        cvar.notify_one();
    }

    pub fn request_frame_hq(
        &self,
        id: Uuid,
        path: PathBuf,
        timestamp: f64,
        preview_size: Option<(u32, u32)>,
    ) {
        let tx = self.scrub_tx.clone();
        let sd = self.shutdown.clone();
        let sem = self.hq_sem.clone();
        thread::spawn(move || {
            // Acquire the HQ semaphore before opening any FFmpeg context.
            // Without this, rapid L3-idle updates spawn N threads simultaneously,
            // each holding a native-res decoder + scaler + frame buffer (~16 MB).
            const HQ_CONCURRENCY: u32 = 2;
            let _guard = SemaphoreGuard::acquire(sem, HQ_CONCURRENCY);
            if sd.load(Ordering::Acquire) {
                return;
            }
            if let Err(e) = decode_frame(&path, id, timestamp, 0.0, false, None, &tx, preview_size)
            {
                crate::media_log!("[media] request_frame_hq: {e}");
            }
        });
    }

    pub fn request_transition_frame(&self, req: TransitionScrubRequest) {
        let (lock, cvar) = &*self.transition_scrub_req;
        *lock.lock().unwrap() = Some(req);
        cvar.notify_one();
    }

    pub fn request_transition_frame_hq(&self, req: TransitionScrubRequest) {
        let scrub_tx = self.scrub_tx.clone();
        let sd = self.shutdown.clone();
        let sem = self.hq_sem.clone();
        thread::spawn(move || {
            // Two native-res decoders opened per call — gate on hq_sem.
            const HQ_CONCURRENCY: u32 = 2;
            let _guard = SemaphoreGuard::acquire(sem, HQ_CONCURRENCY);
            if sd.load(Ordering::Acquire) {
                return;
            }
            let (data_a, w, h) = match decode_one_frame_rgba(&req.clip_a_path, req.clip_a_ts, 0.0) {
                Ok(f) => f,
                Err(e) => {
                    crate::media_log!("[transition_hq] clip_a decode: {e}");
                    return;
                }
            };
            if sd.load(Ordering::Acquire) {
                return;
            }
            let (data_b_raw, wb, hb) =
                match decode_one_frame_rgba(&req.clip_b_path, req.clip_b_ts, 0.0) {
                    Ok(f) => f,
                    Err(e) => {
                        crate::media_log!("[transition_hq] clip_b decode: {e}");
                        return;
                    }
                };
            let data_b = if wb != w || hb != h {
                crate::media_log!(
                    "[transition_hq] clip_b size {}x{} differs from clip_a {}x{}; cropping",
                    wb,
                    hb,
                    w,
                    h
                );
                crop_rgba(&data_b_raw, wb, hb, w, h)
            } else {
                data_b_raw
            };
            let blended = blend_rgba_transition(&data_a, &data_b, w, h, req.alpha, req.kind);
            let _ = scrub_tx.send(MediaResult::VideoFrame {
                id: req.clip_a_id,
                width: w,
                height: h,
                data: blended,
            });
        });
    }

    /// `preview_size` — the actual pixel dimensions of the preview panel (e.g. 960×540).
    /// The playback decoder will scale output to this size instead of native resolution,
    /// dramatically reducing swscale CPU and channel memory:
    ///   • 1080p native: 8 MB/frame × 6 frames = 48 MB in channel, ~8% CPU (swscale)
    ///   • 960×540:      2 MB/frame × 6 frames = 12 MB in channel, ~0.5% CPU
    /// Pass None to decode at native resolution (not recommended for preview).
    pub fn start_playback(
        &self,
        id: Uuid,
        path: PathBuf,
        ts: f64,
        aspect: f32,
        preview_size: Option<(u32, u32)>,
    ) {
        // Clear any stale playback frames before starting new playback.
        // This prevents the "scrub then play freeze" where a stale pending
        // frame blocks the new playback from displaying.
        while self.pb_rx.try_recv().is_ok() {}
        if self
            .pb_tx
            .try_send(PlaybackCmd::Start {
                id,
                path,
                ts,
                aspect,
                preview_size,
            })
            .is_err()
        {
            crate::media_log!(
                "[pb] start_playback: command channel full - Start dropped. This is a bug."
            );
        }
    }

    pub fn start_blend_playback(
        &self,
        id: Uuid,
        path: PathBuf,
        ts: f64,
        aspect: f32,
        blend: PlaybackTransitionSpec,
        preview_size: Option<(u32, u32)>,
    ) {
        // Clear any stale playback frames before starting new blend playback.
        while self.pb_rx.try_recv().is_ok() {}
        if self
            .pb_tx
            .try_send(PlaybackCmd::StartBlend {
                id,
                path,
                ts,
                aspect,
                blend,
                preview_size,
            })
            .is_err()
        {
            crate::media_log!("[pb] start_blend_playback: command channel full - StartBlend dropped. This is a bug.");
        }
    }

    /// [P0-3] Pre-open a decoder for the next clip so Start/StartBlend can
    /// reuse it instead of opening fresh.  Best-effort: if the command channel
    /// is full the request is silently dropped — the Start handler falls back
    /// to its normal open+burn path.
    pub fn prebuffer(
        &self,
        id: Uuid,
        path: PathBuf,
        ts: f64,
        aspect: f32,
        preview_size: Option<(u32, u32)>,
    ) {
        let _ = self.pb_tx.try_send(PlaybackCmd::PreBuffer {
            id,
            path,
            ts,
            aspect,
            preview_size,
        });
    }

    pub fn stop_playback(&self) {
        if self.pb_tx.try_send(PlaybackCmd::Stop).is_err() {
            crate::media_log!(
                "[pb] stop_playback: command channel full — Stop dropped. This is a bug."
            );
        }
        while self.pb_rx.try_recv().is_ok() {}
    }

    pub fn extract_frame_hq(&self, id: Uuid, path: PathBuf, timestamp: f64, dest: PathBuf) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        thread::spawn(move || {
            if sd.load(Ordering::Acquire) {
                return;
            }
            if let Err(e) = decode_frame(&path, id, timestamp, 0.0, true, Some(dest), &tx, None) {
                crate::media_log!("[media] extract_frame_hq: {e}");
            }
        });
    }

    pub fn start_encode(&self, spec: EncodeSpec) {
        let job_id = spec.job_id;
        let cancel = Arc::new(AtomicBool::new(false));
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();

        self.encode_cancels
            .lock()
            .unwrap()
            .insert(job_id, Arc::clone(&cancel));

        let cancels_ref = Arc::clone(&self.encode_cancels);
        thread::spawn(move || {
            if sd.load(Ordering::Acquire) {
                let _ = tx.send(MediaResult::EncodeError {
                    job_id,
                    msg: "worker shutting down".into(),
                });
                return;
            }

            // Lower the encode thread's OS scheduling priority so the UI,
            // audio, and scrub-decode threads are never starved.  The encoder
            // still runs as fast as the CPU allows when nothing else competes,
            // but yields the moment any higher-priority thread needs a core.
            // Combined with the libx264 thread cap in open_software_encoder,
            // this prevents system lockups during 2K/4K CPU encodes.
            velocut_core::windows::lower_thread_priority();

            encode_timeline(spec, cancel, tx);

            cancels_ref.lock().unwrap().remove(&job_id);
        });
    }

    pub fn cancel_encode(&self, job_id: Uuid) {
        if let Some(flag) = self.encode_cancels.lock().unwrap().get(&job_id) {
            flag.store(true, Ordering::Release);
        }
    }
}

// ── Blend decoder helpers ─────────────────────────────────────────────────────

// ── Blend helpers (extracted to worker/blend.rs) ────────────────────────────
// ActiveBlend, decode_transition_scrub_frame, crop_rgba, and
// blend_rgba_transition are defined in the `blend` submodule and
// re-imported at the top of this file via `use blend::*`.
