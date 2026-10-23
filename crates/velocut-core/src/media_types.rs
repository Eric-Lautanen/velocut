// crates/velocut-core/src/media_types.rs
//
// Types that flow across the channel between velocut-media and velocut-ui.
// No egui, no ffmpeg — just plain data.

use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

/// Results sent from the MediaWorker background threads to the UI.
pub enum MediaResult {
    // ── Probe results ─────────────────────────────────────────────────────────
    Duration {
        id: Uuid,
        seconds: f64,
    },
    Thumbnail {
        id: Uuid,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    Waveform {
        id: Uuid,
        peaks: Vec<f32>,
    },
    /// A single video frame decoded on demand (one-shot HQ / extract paths).
    /// `req_ts` is the source-file timestamp the decoder was asked for; the UI
    /// uses it to reject results that arrive after the playhead has moved
    /// elsewhere (out-of-order completions of two concurrent one-shot decodes
    /// would otherwise overwrite the live frame with a stale one).
    VideoFrame {
        id: Uuid,
        req_ts: f64,
        /// Monotone UI request sequence; `0` marks an untracked (legacy)
        /// sender, which ingest accepts unconditionally.
        req_seq: u64,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    /// A blended transition frame from the transition scrub thread.
    /// Carries the same payload as VideoFrame but is routed separately so
    /// the UI can distinguish blended results from single-clip decodes.
    /// `req_ts` matches the request's `TransitionScrubRequest::clip_a_ts`.
    TransitionVideoFrame {
        id: Uuid,
        req_ts: f64,
        /// Echo of `TransitionScrubRequest::req_seq`.
        req_seq: u64,
        /// Echo of `TransitionScrubRequest::alpha` — the blend factor the
        /// result was rendered with. Ingest compares it against the zone
        /// currently under the playhead: the timestamp gate alone cannot
        /// distinguish two results from the same clip pair's zone, and a
        /// wrong-alpha blend must not replace the displayed one.
        alpha: f32,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    VideoSize {
        id: Uuid,
        width: u32,
        height: u32,
    },
    FrameSaved {
        path: PathBuf,
    },
    AudioPath {
        id: Uuid,
        path: PathBuf,
        trimmed_offset: f64,
        /// WAV-extraction epoch this result was produced under (see the doc on
        /// `extract_audio` in `velocut-media`). Bumped on every extraction
        /// request for any library id; the UI drops results whose generation
        /// is not the latest recorded for `id`.
        generation: u64,
    },
    Error {
        id: Uuid,
        msg: String,
    },

    // ── Encode results ────────────────────────────────────────────────────────
    /// Periodic progress report from the encode thread.
    /// `frame` is the number of output frames written so far;
    /// `total_frames` is the sum of all clip frame counts at the target fps.
    EncodeProgress {
        job_id: Uuid,
        frame: u64,
        total_frames: u64,
    },
    /// Encode completed successfully.
    EncodeDone {
        job_id: Uuid,
        path: PathBuf,
    },
    /// Encode failed or was cancelled.
    EncodeError {
        job_id: Uuid,
        msg: String,
    },
}

/// Pixels of a decoded frame, optionally able to hand their buffer back to the
/// thread that allocated it.
///
/// Playback allocates a frame-sized buffer for every displayed frame. If those
/// buffers are simply dropped in the UI, the allocator returns the pages to the
/// OS and the next frame faults all of them in again — about 1230 page faults
/// for one 5 MB preview frame, which is more expensive than the transition blend
/// that produced it. Stamping the buffer with a return channel means the UI only
/// has to drop the frame: the buffer goes back to the playback thread and the
/// frame after that writes into warm memory.
///
/// Derefs to `[u8]`, so consumers treat it as the pixel slice it is.
pub struct FrameBuf {
    data: Vec<u8>,
    recycle: Option<Arc<dyn Fn(Vec<u8>) + Send + Sync>>,
}

impl FrameBuf {
    /// A buffer with no return path: the allocation is freed with the frame.
    pub fn detached(data: Vec<u8>) -> Self {
        Self {
            data,
            recycle: None,
        }
    }

    /// A buffer that returns itself to `recycle` when the last owner drops it.
    pub fn recycled(data: Vec<u8>, recycle: Arc<dyn Fn(Vec<u8>) + Send + Sync>) -> Self {
        Self {
            data,
            recycle: Some(recycle),
        }
    }

    /// The pixels, without consuming the buffer.
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }
}

impl Deref for FrameBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl DerefMut for FrameBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl Drop for FrameBuf {
    fn drop(&mut self) {
        // Take the allocation out before handing it on, so a failing send cannot
        // leave the frame holding a buffer the worker also owns.
        if let Some(recycle) = self.recycle.take() {
            recycle(std::mem::take(&mut self.data));
        }
    }
}

impl std::fmt::Debug for FrameBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameBuf")
            .field("len", &self.data.len())
            .field("recycled", &self.recycle.is_some())
            .finish()
    }
}

/// A decoded frame from the dedicated playback pipeline.
pub struct PlaybackFrame {
    pub id: Uuid,
    pub timestamp: f64,
    pub width: u32,
    pub height: u32,
    pub data: FrameBuf, // RGBA
    /// Playback-session generation the frame was produced under.
    ///
    /// Every `Start` / `StartBlend` / `Stop` begins a new session; the pb thread
    /// stamps the session generation from the command that opened it onto every
    /// frame it produces. The UI compares this against the worker's current
    /// generation before accepting a frame: frames still in flight from an older
    /// session (the non-atomic `pb_rx` drain in `start_playback`/
    /// `start_blend_playback`, or a Stop whose command was dropped) can carry a
    /// clip id and timestamp that pass the `wrong_clip`/`too_old` windows but
    /// belong to a different transition spec — accepting them would flash a
    /// wrong-alpha frame at a session boundary. A generation mismatch rejects
    /// them regardless of id or timestamp.
    pub generation: u64,
}

/// Parameters for a one-shot blended scrub-frame request.
/// Sent from `video_module` to `MediaWorker::request_transition_frame`.
/// The worker decodes a frame from each clip and returns a blended RGBA result
/// via the scrub channel, keyed by `clip_a_id`.
pub struct TransitionScrubRequest {
    pub clip_a_id: Uuid,
    pub clip_a_path: PathBuf,
    /// Source-file timestamp for clip_a (seconds).
    pub clip_a_ts: f64,
    pub clip_b_id: Uuid,
    pub clip_b_path: PathBuf,
    /// Source-file timestamp for clip_b (seconds).
    pub clip_b_ts: f64,
    /// Blend factor: 0.0 = fully clip_a, 1.0 = fully clip_b.
    pub alpha: f32,
    pub kind: crate::transitions::TransitionKind,
    /// Monotone sequence number the UI assigns at request time. Echoed back on
    /// the result so ingest can reject out-of-order completions from the two
    /// independent transition executors (the persistent L2 slot thread and the
    /// one-shot HQ threads both feed the same result channel): an older request
    /// finishing late must never overwrite a newer result already shown.
    pub req_seq: u64,
}

/// Passed to `MediaWorker::start_blend_playback` so the playback thread can
/// open a second decoder for clip_b and blend frames at the clip boundary.
pub struct PlaybackTransitionSpec {
    pub clip_b_id: Uuid,
    pub clip_b_path: PathBuf,
    /// clip_b.source_offset — where to seek clip_b's decoder on open.
    pub clip_b_source_start: f64,
    /// Source timestamp in clip_a's decoded stream at which blending begins.
    /// For the clip_a side of a centered transition:
    ///   = clip_a.source_offset + clip_a.duration − transition.duration_secs / 2
    /// For the clip_b side (when clip_b takes over as primary decoder):
    ///   = clip_b.source_offset  (blend starts immediately from clip_b's first frame)
    pub blend_start_ts: f64,
    pub duration: f32,
    pub kind: crate::transitions::TransitionKind,
    /// Alpha value at the start of blending for this decoder's half of the transition.
    /// Clip_a side: 0.0  (fades out from pure clip_a → 0.5 at cut point).
    /// Clip_b side: 0.5  (continues from the cut point → 1.0 pure clip_b).
    pub alpha_start: f32,
    /// When true, swap a and b in the blend call.
    /// Set for the clip_b (incoming) side of the transition: the main decoder is
    /// clip_b but blend_rgba_transition expects clip_a as the first argument.
    pub invert_ab: bool,
}

#[cfg(test)]
mod frame_buf_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Collects the length of every buffer handed back through the recycle hook.
    type ReturnLog = Arc<Mutex<Vec<usize>>>;
    /// The hook a `FrameBuf` calls when it is dropped.
    type RecycleHook = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

    fn recorder() -> (ReturnLog, RecycleHook) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let recycle: Arc<dyn Fn(Vec<u8>) + Send + Sync> =
            Arc::new(move |buf: Vec<u8>| sink.lock().unwrap().push(buf.len()));
        (log, recycle)
    }

    #[test]
    fn a_recycled_buffer_goes_back_when_the_frame_is_dropped() {
        let (log, recycle) = recorder();
        {
            let frame = PlaybackFrame {
                id: Uuid::nil(),
                timestamp: 0.0,
                width: 4,
                height: 2,
                data: FrameBuf::recycled(vec![7u8; 32], recycle),
                generation: 0,
            };
            assert_eq!(frame.data.len(), 32);
            assert_eq!(&frame.data[..4], &[7, 7, 7, 7]);
        }
        assert_eq!(*log.lock().unwrap(), vec![32], "buffer was not returned");
    }

    #[test]
    fn a_detached_buffer_is_not_returned() {
        let (log, recycle) = recorder();
        // Keep one clone alive so the closure's target is still shared.
        {
            let frame = PlaybackFrame {
                id: Uuid::nil(),
                timestamp: 0.0,
                width: 4,
                height: 2,
                data: FrameBuf::detached(vec![1u8; 16]),
                generation: 0,
            };
            drop(frame);
        }
        drop(recycle);
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn pixels_can_be_read_and_filtered_in_place_through_the_newtype() {
        let (_, recycle) = recorder();
        let mut frame = PlaybackFrame {
            id: Uuid::nil(),
            timestamp: 0.0,
            width: 2,
            height: 1,
            data: FrameBuf::recycled(vec![10u8; 8], recycle),
            generation: 0,
        };

        // The UI does exactly this: `&mut f.data` into a slice-taking filter.
        let pixels: &mut [u8] = &mut frame.data;
        pixels[0] = 99;
        let read: &[u8] = &frame.data;
        assert_eq!(read, &[99, 10, 10, 10, 10, 10, 10, 10]);
    }

    #[test]
    fn dropping_a_frame_mid_flight_does_not_lose_or_duplicate_the_buffer() {
        let (log, recycle) = recorder();
        let buffers: Vec<Vec<u8>> = (0..3)
            .map(|i| PlaybackFrame {
                id: Uuid::nil(),
                timestamp: i as f64,
                width: 1,
                height: 1,
                data: FrameBuf::recycled(vec![i as u8; 4], recycle.clone()),
                generation: 0,
            })
            .map(|f| f.data.to_vec())
            .collect();
        assert_eq!(buffers.len(), 3);
        assert_eq!(*log.lock().unwrap(), vec![4, 4, 4]);
    }
}
