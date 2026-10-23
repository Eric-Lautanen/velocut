// crates/velocut-media/src/worker.rs
//
// MediaWorker: owns the frame-request slot and playback decode thread.
// All public API that velocut-ui calls lives here.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use uuid::Uuid;

use velocut_core::media_types::{
    MediaResult, PlaybackFrame, PlaybackTransitionSpec, TransitionScrubRequest,
};

use crate::audio::extract_audio;
use crate::decode::{decode_frame, decode_one_frame_rgba, DecodeFrameRequest, LiveDecoder};
use crate::encode::{encode_timeline, EncodeSpec};
use crate::probe::{probe_duration, probe_video_size_and_thumbnail};
use crate::waveform::extract_waveform;

mod blend;
use blend::{blend_rgba_transition, crop_rgba, decode_transition_scrub_frame};

mod types;
pub use types::FrameRequest;
use types::PlaybackCmd;

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

    /// Playback-session generation (see `PlaybackFrame::generation`). Bumped on
    /// every `start_playback` / `start_blend_playback` / `stop_playback` before
    /// the command is sent, so any frame still in flight from the previous
    /// session carries a stale generation the UI can reject. Atomic because the
    /// pb thread learns the value indirectly (via the command) while the UI thread
    /// reads the current value for its acceptance gate.
    pb_generation: AtomicU64,
    /// WAV-extraction epoch (see `PlaybackFrame::generation` for the video
    /// analog). Bumped on every `extract_audio` request — both the probe
    /// pipeline's full-file extraction and `extract_audio_trimmed` — and the
    /// value recorded per library id (in `audio_latest`) is what the UI gates
    /// `MediaResult::AudioPath` against: an extraction that was superseded by
    /// a newer request for the same library id must not overwrite the entry's
    /// `audio_path`/`audio_trimmed_offset`, because the two can carry different
    /// content windows and `audio_trimmed_offset` drives the `wav_seek_t`
    /// math. Atomic + Mutex because extraction threads run concurrently while
    /// the UI reads the latest value for its acceptance gate.
    audio_gen: AtomicU64,
    /// Latest WAV-extraction generation requested per library id. The UI
    /// compares each arriving `AudioPath::generation` against the entry for
    /// its id; an id that has never had a request maps to 0, which no real
    /// generation ever equals (the counter starts at 1).
    audio_latest: Mutex<HashMap<Uuid, u64>>,
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
    /// Latest-wins slot for on-demand coarse (L2b) prefetch scrub frames.
    /// Separate from `frame_req` so a per-2-second coarse prefetch fired in the
    /// same tick as an exact scrub decode can never overwrite the exact request
    /// in the slot — the old single-slot design made the coarse result (which
    /// can sit up to 2 s away from the playhead) impersonate the exact one.
    frame_req_coarse: Arc<(Mutex<Option<FrameRequest>>, Condvar)>,
    /// Latest-wins slot for transition scrub frames (L2 scrub in a transition zone).
    /// Mirrors `frame_req` but carries a full `TransitionScrubRequest` and is
    /// consumed by a dedicated thread that keeps two `LiveDecoder`s alive across
    /// requests — avoiding the per-frame decoder open/seek/close overhead that
    /// made transition scrubbing laggy on CPU-only systems.
    transition_scrub_req: Arc<(Mutex<Option<TransitionScrubRequest>>, Condvar)>,

    // ── Thread handles (for graceful shutdown) ─────────────────────────────────
    /// Handle for the dedicated scrub-frame decode thread.
    scrub_thread: Option<thread::JoinHandle<()>>,
    /// Handle for the dedicated coarse-prefetch scrub decode thread.
    coarse_scrub_thread: Option<thread::JoinHandle<()>>,
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
        let frame_req_coarse: Arc<(Mutex<Option<FrameRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));

        // ── Scrub frame decode threads ────────────────────────────────────────
        // One thread per tier (exact L2 + coarse L2b prefetch), each blocking on
        // its own latest-wins slot; both reuse a LiveDecoder across requests.
        // [Opt 3] Sends VideoFrame on scrub_tx (not tx) so scrub results bypass
        // the shared channel and are consumed with lower latency under probe load.
        let scrub_thread = thread::spawn({
            let slot = Arc::clone(&frame_req);
            let result_tx = scrub_tx.clone();
            move || scrub_frame_loop(slot, result_tx)
        });
        let coarse_scrub_thread = thread::spawn({
            let slot = Arc::clone(&frame_req_coarse);
            let result_tx = scrub_tx.clone();
            move || scrub_frame_loop(slot, result_tx)
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
                    req_ts: req.clip_a_ts,
                    req_seq: req.req_seq,
                    alpha: req.alpha,
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

        // Frames the UI has finished with come back here, so their pixel buffers
        // can be reused instead of being dropped and faulted in again. Bounded:
        // if the playback thread is not draining (it is not blending, or it is
        // stopped) the surplus buffers are released rather than queued forever.
        let (recycle_tx, recycle_rx) = bounded::<Vec<u8>>(8);
        let recycle: Arc<dyn Fn(Vec<u8>) + Send + Sync> = Arc::new(move |buf: Vec<u8>| {
            let _ = recycle_tx.try_send(buf);
        });

        let pb = PbThread {
            cmd_rx: pb_cmd_rx,
            frame_tx: pb_frame_tx,
            recycle_rx,
            recycle,
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
            frame_req_coarse,
            pb_tx,
            pb_rx,
            pb_generation: AtomicU64::new(0),
            audio_gen: AtomicU64::new(0),
            audio_latest: Mutex::new(HashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            probe_sem: Arc::new((Mutex::new(0), Condvar::new())),
            encode_cancels: Arc::new(Mutex::new(HashMap::new())),
            hq_sem: Arc::new((Mutex::new(0), Condvar::new())),
            transition_scrub_req,
            scrub_thread: Some(scrub_thread),
            coarse_scrub_thread: Some(coarse_scrub_thread),
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
        // Poison-pill the regular scrub thread and the coarse prefetch thread.
        let (lock, cvar) = &*self.frame_req;
        *lock.lock().unwrap() = Some(FrameRequest {
            id: Uuid::nil(),
            path: std::path::PathBuf::new(),
            timestamp: 0.0,
            aspect: 0.0,
            preview_size: None,
            req_seq: 0,
        });
        cvar.notify_one();
        let (lock, cvar) = &*self.frame_req_coarse;
        *lock.lock().unwrap() = Some(FrameRequest {
            id: Uuid::nil(),
            path: std::path::PathBuf::new(),
            timestamp: 0.0,
            aspect: 0.0,
            preview_size: None,
            req_seq: 0,
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
                req_seq: 0,
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
        if let Some(h) = self.coarse_scrub_thread.take() {
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
        // Bump the audio epoch on the caller's thread (see next_audio_generation):
        // the probe pipeline's extract_audio result is gated against this value,
        // so a superseded probe can never overwrite a newer extraction.
        let audio_gen = self.next_audio_generation(id);

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
                extract_audio(&path, id, 0.0, f64::MAX, audio_gen, &tx);
            }
        });
    }

    /// No-op for compat — thumbnails now come back via probe_clip as RGBA data.
    pub fn reload_thumbnail(&self, id: Uuid, path: PathBuf) {
        self.probe_clip(id, path);
    }

    /// Re-extract the WAV temp file for a library clip, optionally restricted
    /// to `[source_offset, source_offset + duration)`.
    ///
    /// Every current caller passes the full-file window (`0.0, f64::MAX`): the
    /// startup repair pass for an entry whose temp WAV no longer exists, and
    /// `ExtractAudioTrack` for the separated track's new library entry. Trimming
    /// a clip does NOT come through here — the full-file WAV is reused and the
    /// trim is applied at playback by `wav_seek_t` (and in the encoder), which
    /// is why `audio_trimmed_offset` stays 0.0 in practice.
    ///
    /// A windowed request is still supported and self-consistent: the result
    /// carries `trimmed_offset = source_offset`, which `wav_seek_t` subtracts so
    /// the offset is not counted twice, and the WAV then holds only the audible
    /// window (a 157-second MP3 trimmed to 28 seconds is ~55 MB vs ~10 MB of
    /// PCM). Nothing currently asks for that form.
    pub fn extract_audio_trimmed(
        &self,
        id: Uuid,
        path: PathBuf,
        source_offset: f64,
        duration: f64,
    ) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        // Same epoch discipline as probe_clip: the caller-thread bump is the
        // identity the AudioPath result is gated against.
        let audio_gen = self.next_audio_generation(id);
        thread::spawn(move || {
            if sd.load(Ordering::Acquire) {
                return;
            }
            extract_audio(&path, id, source_offset, duration, audio_gen, &tx);
        });
    }

    /// Queue a scrub-frame decode request on the tier's latest-wins slot.
    ///
    /// `coarse = false` routes to the exact (L2) slot: the decoded frame is
    /// displayed at the playhead position. `coarse = true` routes to the
    /// coarse (L2b) prefetch slot: the decoded frame is a look-ahead warm-up
    /// stored in the coarse bucket lane only. Keeping the two slots separate
    /// guarantees a coarse prefetch can never replace an exact request that
    /// was fired in the same tick.
    /// Queue a scrub-frame request, replacing whatever is pending.
    ///
    /// `coarse` is carried separately from the request because it selects the
    /// destination slot, not anything about the decode itself.
    pub fn request_frame(&self, req: FrameRequest, coarse: bool) {
        let (lock, cvar) = if coarse {
            &*self.frame_req_coarse
        } else {
            &*self.frame_req
        };
        *lock.lock().unwrap() = Some(req);
        cvar.notify_one();
    }

    pub fn request_frame_hq(
        &self,
        id: Uuid,
        path: PathBuf,
        timestamp: f64,
        preview_size: Option<(u32, u32)>,
        req_seq: u64,
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
            let req = DecodeFrameRequest {
                path: &path,
                id,
                timestamp,
                aspect: 0.0,
                save_png: false,
                dest: None,
                req_seq,
                preview_size,
            };
            if let Err(e) = decode_frame(req, &tx) {
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
                req_ts: req.clip_a_ts,
                req_seq: req.req_seq,
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
        //
        // The drain is not atomic with the pb thread's sends: a frame may land
        // here after the loop. The session generation bumps FIRST so every frame
        // in that race (and any the pb thread produces before it processes the
        // Start) carries a stale generation the UI rejects.
        let gen = self.pb_generation.fetch_add(1, Ordering::Relaxed) + 1;
        while self.pb_rx.try_recv().is_ok() {}
        if self
            .pb_tx
            .try_send(PlaybackCmd::Start {
                id,
                path,
                ts,
                aspect,
                preview_size,
                gen,
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
        //
        // Same non-atomic-drain caveat as `start_playback`: bumping the session
        // generation first guarantees the in-flight racers are rejectable.
        let gen = self.pb_generation.fetch_add(1, Ordering::Relaxed) + 1;
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
                gen,
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

    /// Current playback-session generation. The UI compares this against
    /// `PlaybackFrame::generation` before accepting a frame: a mismatch means
    /// the frame belongs to a session that has already been superseded.
    pub fn playback_generation(&self) -> u64 {
        self.pb_generation.load(Ordering::Relaxed)
    }

    /// Issue the next WAV-extraction generation for a library id and record it
    /// as that id's latest. Called synchronously on the caller's thread when a
    /// request is queued (before the extraction thread is spawned), so a test
    /// or the UI can read the expected value right after issuing the request.
    /// Returns the generation the resulting `MediaResult::AudioPath` will
    /// carry.
    pub fn next_audio_generation(&self, id: Uuid) -> u64 {
        let gen = self.audio_gen.fetch_add(1, Ordering::Relaxed) + 1;
        self.audio_latest.lock().unwrap().insert(id, gen);
        gen
    }

    /// Latest WAV-extraction generation requested for `id` — the value the UI
    /// compares `MediaResult::AudioPath::generation` against before accepting.
    /// 0 for an id with no request on record: the counter starts at 1, so a
    /// result for a never-requested id is always stale.
    pub fn audio_generation_of(&self, id: Uuid) -> u64 {
        self.audio_latest
            .lock()
            .unwrap()
            .get(&id)
            .copied()
            .unwrap_or(0)
    }

    pub fn stop_playback(&self) {
        // Bump the generation even though Stop carries none: if the command is
        // dropped (command channel full) or merely still in flight, the pb
        // thread keeps producing frames until it processes the Stop. The UI
        // compares frames against the post-bump generation and rejects every
        // frame of the dying session.
        self.pb_generation.fetch_add(1, Ordering::Relaxed);
        // A dropped Stop is not benign, so it is retried instead of given up
        // with a one-line "This is a bug" log (the audit's trigger for the
        // sessionless-carry leak). Until the Stop is processed the pb thread
        // keeps decoding the dying session — or sits wedged in a blocking send
        // once the frame channel fills (the UI stops draining after a stop),
        // and the NEXT start_* call would then find the command channel full
        // and silently drop its command too. Each retry first drains pb_rx so
        // the blocked send unblocks and the thread advances to the command
        // queue; the bounded attempt count keeps the UI thread from hanging
        // forever if the thread is genuinely stuck.
        const STOP_ATTEMPTS: usize = 10;
        for attempt in 0..STOP_ATTEMPTS {
            match self.pb_tx.try_send(PlaybackCmd::Stop) {
                Ok(()) => {
                    if attempt > 0 {
                        crate::media_log!(
                            "[pb] stop_playback: Stop accepted after {attempt} retry(ies)"
                        );
                    }
                    break;
                }
                // Worker teardown — the pb thread is already gone.
                Err(TrySendError::Disconnected(_)) => break,
                Err(TrySendError::Full(_)) => {
                    if attempt + 1 == STOP_ATTEMPTS {
                        crate::media_log!(
                            "[pb] stop_playback: Stop NOT accepted after {STOP_ATTEMPTS} attempts — \
                             the pb thread is not servicing the command channel. Frames of the \
                             dying session are rejected by the session-generation gate, but the \
                             decoder keeps running until the next start_* drains the channels."
                        );
                        break;
                    }
                    // Free the frame channel so the blocked send completes and
                    // the thread's loop reaches the command queue, then retry.
                    while self.pb_rx.try_recv().is_ok() {}
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
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
            let req = DecodeFrameRequest {
                path: &path,
                id,
                timestamp,
                aspect: 0.0,
                save_png: true,
                dest: Some(dest),
                req_seq: 0,
                preview_size: None,
            };
            if let Err(e) = decode_frame(req, &tx) {
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

// ── Scrub-frame decode loop ─────────────────────────────────────────────────────

/// Decode loop shared by the exact (L2) and coarse (L2b) scrub threads.
///
/// Blocks on the tier's latest-wins slot and reuses one LiveDecoder across
/// requests. Every result carries the requested timestamp as `req_ts` so the
/// UI ingest can reject frames that arrive after the playhead has moved
/// elsewhere (out-of-order completions of two concurrent one-shot decodes
/// would otherwise overwrite the live frame with a stale one).
fn scrub_frame_loop(
    slot: Arc<(Mutex<Option<FrameRequest>>, Condvar)>,
    result_tx: Sender<MediaResult>,
) {
    let mut live: Option<LiveDecoder> = None;
    // What the held decoder was opened for. The decoder is reopened whenever
    // any of these change — comparing only the path previously kept a decoder
    // with a stale aspect / output size serving frames after a preview resize
    // or aspect-ratio change.
    let mut live_path: PathBuf = PathBuf::new();
    let mut live_aspect: f32 = f32::NAN;
    let mut live_ps: Option<(u32, u32)> = None;
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

        let needs_open =
            live_path != req.path || live_aspect != req.aspect || live_ps != req.preview_size;

        if needs_open {
            // Different file, resolution mode, or first request — open a fresh decoder.
            let cached_sws = live
                .take()
                .map(|d| (d.scaler, d.decoder_fmt, d.decoder_w, d.decoder_h));
            // When preview_size is known, pass it as forced_size so the
            // decoder output matches the panel dimensions exactly.
            // Otherwise fall back to the aspect-based 320px scrub size.
            let forced = if req.preview_size.is_some() {
                req.preview_size
            } else {
                None
            };
            match LiveDecoder::open(&req.path, req.timestamp, req.aspect, cached_sws, forced) {
                Ok(mut d) => {
                    let target_pts = d.ts_to_pts(req.timestamp);
                    if let Some((data, w, h)) = d.advance_to(target_pts) {
                        let _ = result_tx.send(MediaResult::VideoFrame {
                            id: req.id,
                            req_ts: req.timestamp,
                            req_seq: req.req_seq,
                            width: w,
                            height: h,
                            data,
                        });
                    }
                    live = Some(d);
                    live_path = req.path.clone();
                    live_aspect = req.aspect;
                    live_ps = req.preview_size;
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
                    let _ = result_tx.send(MediaResult::VideoFrame {
                        id: req.id,
                        req_ts: req.timestamp,
                        req_seq: req.req_seq,
                        width: w,
                        height: h,
                        data,
                    });
                }
            } else if let Some((data, w, h)) = d.advance_to(tpts) {
                let _ = result_tx.send(MediaResult::VideoFrame {
                    id: req.id,
                    req_ts: req.timestamp,
                    req_seq: req.req_seq,
                    width: w,
                    height: h,
                    data,
                });
            }
        }
    }
}

#[cfg(test)]
mod playback_generation_tests {
    //! The pb-path wrong-alpha defense (see `PlaybackFrame::generation`): every
    //! Start / StartBlend / Stop opens a new playback session and the UI may
    //! only accept frames stamped with the current generation.

    use super::*;

    #[test]
    fn every_start_blend_and_stop_opens_a_new_session() {
        let worker = MediaWorker::new();
        let id = Uuid::nil();
        assert_eq!(worker.playback_generation(), 0);

        // A nonexistent path is fine — the pb thread logs the open failure; the
        // generation is bumped before the command is sent, which is what we pin.
        worker.start_playback(id, PathBuf::from("definitely-missing.mp4"), 0.0, 0.0, None);
        assert_eq!(worker.playback_generation(), 1);

        worker.start_blend_playback(
            id,
            PathBuf::from("definitely-missing.mp4"),
            0.0,
            0.0,
            PlaybackTransitionSpec {
                clip_b_id: id,
                clip_b_path: PathBuf::from("also-missing.mp4"),
                clip_b_source_start: 0.0,
                blend_start_ts: 0.0,
                duration: 1.0,
                kind: velocut_core::transitions::TransitionKind::Cut,
                alpha_start: 0.5,
                invert_ab: false,
            },
            None,
        );
        assert_eq!(worker.playback_generation(), 2);

        // Stop bumps too: a dropped Stop command (the audit's trigger for the
        // sessionless-carry leak) must still make the dying session rejectable.
        worker.stop_playback();
        assert_eq!(worker.playback_generation(), 3);
    }
}

#[cfg(test)]
mod audio_generation_tests {
    //! The WAV-extraction session epoch (see `MediaResult::AudioPath
    //! ::generation` and `next_audio_generation`): extraction requests for the
    //! same library id supersede each other, and the UI may only accept an
    //! AudioPath result stamped with the id's latest generation — a stale result
    //! would overwrite `audio_path` + `audio_trimmed_offset` with a content
    //! window the current WAV does not hold, desyncing `wav_seek_t`.

    use super::*;

    #[test]
    fn every_extraction_request_supersedes_the_previous_gen_for_its_id() {
        let worker = MediaWorker::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        // A never-requested id maps to 0, which no real generation equals.
        assert_eq!(worker.audio_generation_of(a), 0);

        // Missing paths are fine: extraction soft-fails and sends nothing. The
        // caller-thread epoch bump is what we pin here.
        worker.extract_audio_trimmed(a, PathBuf::from("missing-file.wav"), 0.0, f64::MAX);
        let gen_a1 = worker.audio_generation_of(a);
        assert_ne!(gen_a1, 0);

        // A different library id gets its own epoch — results must not cross ids.
        worker.extract_audio_trimmed(b, PathBuf::from("missing-file.wav"), 1.0, 2.0);
        let gen_b = worker.audio_generation_of(b);
        assert_ne!(gen_b, gen_a1);
        assert_eq!(worker.audio_generation_of(a), gen_a1);

        // A new request for a supersedes: from here the UI gate rejects gen_a1.
        worker.extract_audio_trimmed(a, PathBuf::from("missing-file.wav"), 0.0, f64::MAX);
        let gen_a2 = worker.audio_generation_of(a);
        assert_ne!(gen_a2, gen_a1);

        // The soft-fail on the missing path must send no AudioPath at all: a
        // failed extraction never clobbers the entry's current WAV bookkeeping.
        assert!(
            worker.rx.try_recv().is_err(),
            "a failed extraction must send no audio result"
        );
    }
}
