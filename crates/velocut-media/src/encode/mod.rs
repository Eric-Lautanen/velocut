// crates/velocut-media/src/encode.rs
//
// Multi-clip H.264 + AAC MP4 encode pipeline.
//
// Design:
//   • `ClipSpec`   — everything needed to locate and trim one source clip.
//   • `EncodeSpec` — the complete job description handed from the UI.
//   • `encode_timeline()` — blocking function meant to run on its own thread;
//     called from MediaWorker::start_encode. Sends EncodeProgress every
//     PROGRESS_INTERVAL frames and EncodeError / EncodeDone on exit.
//
// Stream layout in the output MP4:
//   Stream 0 — H.264 video (YUV420P, CRF 18, preset medium, or HW equivalent)
//   Stream 1 — AAC audio  (FLTP stereo, 44100 Hz, 128 kbps)
//
// Hardware encoding:
//   Attempted in priority order: AMF (D3D11) → NVENC (CUDA) → VAAPI → VideoToolbox → libx264.
//   Each HW path uploads YUV420P software frames to the device via an
//   AVHWFramesContext before calling send_frame; the muxer receives standard
//   H.264 Annex-B / AVCC output regardless of which encoder ran.
//   If all HW paths fail (missing driver, wrong platform) the pipeline falls
//   through to software x264 automatically.
//
// Hardware capability probe:
//   `probe_hw_encode_capabilities()` runs a lightweight dry-run at startup
//   (no actual encode) and returns `HwEncodeCapabilities`. The UI uses this
//   to annotate resolution options — SW-only machines see an informational
//   note at all resolutions since the encode thread is throttled (priority +
//   thread cap + per-frame yield) to stay responsive on any hardware.
//
// PTS strategy:
//   Video: monotonically increasing frame counter (output_frame_idx) in 1/fps.
//   Audio: monotonically increasing sample counter (out_sample_idx) in 1/44100.
//   Both reset to zero at the start of the encode, eliminating discontinuities
//   introduced by source file trimming and multi-clip concatenation.
//
// Audio FIFO:
//   AAC requires exactly `encoder.frame_size()` (typically 1024) samples per
//   input frame. Decoded audio may arrive in arbitrary chunk sizes, so all
//   decoded/resampled PCM is drained into a stereo FLTP ring buffer. Full
//   frames are popped from the front and sent to the encoder; any remainder
//   carries over into the next clip. At the very end the tail is zero-padded
//   and flushed.
//
//   Audio resampler tail flush (1080p dropout fix):
//   After the decoder EOF drain the resampler may hold a partial output block
//   (SwrContext defers output until it has enough samples). A null-frame flush
//   (swr_convert with null input) extracts these buffered samples before the
//   FIFO carry-over check. Without this, 1080p clips lose the last ~20 ms of
//   audio per clip, which accumulates into audible gaps on long timelines.
//
// Overlay audio boundary contract:
//   Audio overlay tracks are encoded exactly as the user placed them — no
//   gap-filling, no truncation.  If an overlay ends before the video it simply
//   stops contributing samples (the FIFO holds silence).  If an overlay extends
//   *past* the last video clip the encoder appends black (YUV black, Y=16) video
//   frames for the duration of the tail so the audio is preserved with a blank
//   screen rather than silently dropped.
//
// Cancellation:
//   `cancel` is an Arc<AtomicBool> checked after every video frame. When set,
//   EncodeError { msg: "cancelled" } is sent — the UI treats that as an aborted
//   state distinct from a real error, keeping the cancel path identical to the
//   error path.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crossbeam_channel::Sender;
use uuid::Uuid;

use ffmpeg::codec::{self, Id as CodecId};
use ffmpeg::encoder;
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::format::{output as open_output, Pixel, Sample};
use ffmpeg::util::channel_layout::ChannelLayout;
use ffmpeg::util::frame::video::Video as VideoFrame;
use ffmpeg::util::rational::Rational;
use ffmpeg::Packet;
use ffmpeg::packet::Mut as _;
use ffmpeg_the_third as ffmpeg;

use velocut_core::filters::FilterParams;
use velocut_core::media_types::MediaResult;
use velocut_core::transitions::{registry, ClipTransition, TransitionKind};

mod hw;
pub use hw::probe_hw_encode_capabilities;
use hw::{try_open_hw_encoder, HwBackend};

mod audio;
use audio::{AudioEncState, AudioFifo, decode_overlay};

mod clip;
use clip::{apply_transition, encode_clip, send_video_frame};

// ── Public types ──────────────────────────────────────────────────────────────

/// One source clip's contribution to the output timeline.
#[derive(Clone)]
pub struct ClipSpec {
    /// Absolute path to the source media file.
    pub path: PathBuf,
    /// Seconds into the source file at which this clip begins.
    pub source_offset: f64,
    /// Duration in seconds to include from this clip.
    pub duration: f64,
    /// Linear gain applied to decoded audio before encoding (1.0 = unity).
    pub volume: f32,
    /// When true, no audio is decoded or pushed to the FIFO for this clip.
    pub skip_audio: bool,
    /// Fade-in ramp duration (0.0 = none). Ramp starts after `fade_in_start_secs` of silence.
    pub fade_in_secs: f32,
    /// Silence before the fade-in ramp begins (0.0 = ramp starts at clip boundary).
    pub fade_in_start_secs: f32,
    /// Fade-out ramp duration (0.0 = none). After ramp, silence for `fade_out_end_secs`.
    pub fade_out_secs: f32,
    /// Silence after fade-out ramp ends, before clip boundary (0.0 = ramp ends at boundary).
    pub fade_out_end_secs: f32,
    pub filter: FilterParams,
}

/// A standalone audio clip that runs in parallel with the video timeline.
#[derive(Clone)]
pub struct AudioOverlay {
    pub path: PathBuf,
    pub source_offset: f64,
    pub timeline_start: f64,
    pub duration: f64,
    pub volume: f32,
    pub fade_in_secs: f32,
    pub fade_in_start_secs: f32,
    pub fade_out_secs: f32,
    pub fade_out_end_secs: f32,
}

/// Complete description of an encode job.
pub struct EncodeSpec {
    pub job_id: Uuid,
    pub clips: Vec<ClipSpec>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub output: PathBuf,
    pub transitions: Vec<ClipTransition>,
    pub audio_overlays: Vec<AudioOverlay>,
}

// ── Hardware capability probe ─────────────────────────────────────────────────

/// Result of the startup hardware encoder probe.
///
/// The UI reads this to annotate resolution options.
/// SW-only machines can encode at any resolution — higher resolutions
/// will be slower but the encode thread is throttled (priority + thread
/// cap + per-frame yield) so the system stays responsive throughout.
#[derive(Debug, Clone)]
pub struct HwEncodeCapabilities {
    /// True when no HW encoder is available and libx264 will be used.
    /// The UI shows an informational note but does not restrict resolutions.
    pub sw_only: bool,
    /// Human-readable name of the winning backend, e.g. "AMF", "NVENC",
    /// "VAAPI", "VideoToolbox", or "Software (libx264)".
    pub backend_name: &'static str,
}
// ── Constants ─────────────────────────────────────────────────────────────────

pub(super) const PROGRESS_INTERVAL: u64 = 15;
pub(super) const AUDIO_RATE: i32 = 44100;

// ── Center-crop scaler ────────────────────────────────────────────────────────

// CropScaler moved to clip.rs

// ── Public entry point ────────────────────────────────────────────────────────

pub fn encode_timeline(spec: EncodeSpec, cancel: Arc<AtomicBool>, tx: Sender<MediaResult>) {
    let total_frames: u64 = spec
        .clips
        .iter()
        .map(|c| (c.duration * spec.fps as f64).ceil() as u64)
        .sum::<u64>()
        .max(1);

    match run_encode(&spec, cancel, total_frames, &tx) {
        Ok(()) => {
            let _ = tx.send(MediaResult::EncodeDone {
                job_id: spec.job_id,
                path: spec.output.clone(),
            });
        }
        Err(e) => {
            let _ = tx.send(MediaResult::EncodeError {
                job_id: spec.job_id,
                msg: e,
            });
        }
    }
}

// ── Audio ── see audio.rs

// ── Internal implementation ───────────────────────────────────────────────────

fn run_encode(
    spec: &EncodeSpec,
    cancel: Arc<AtomicBool>,
    total_frames: u64,
    tx: &Sender<MediaResult>,
) -> Result<(), String> {
    if spec.clips.is_empty() {
        return Err("nothing to encode: timeline is empty".into());
    }

    // ── Output context ────────────────────────────────────────────────────────
    let mut octx = open_output(&spec.output)
        .map_err(|e| format!("could not open output '{}': {e}", spec.output.display()))?;

    // ── Video encoder (stream 0) ──────────────────────────────────────────────
    let out_tb = Rational::new(1, spec.fps as i32);
    let frame_tb = Rational::new(1, spec.fps as i32);

    // Determine which codec we'll be registering for the stream.  HW encoders
    // expose themselves under their own codec ID (hevc_nvenc, h264_nvenc, etc.)
    // but we always want stream 0 to carry H.264, so use the H264 codec ID for
    // the output stream regardless of which actual encoder won.
    let h264_for_stream =
        encoder::find(CodecId::H264).ok_or_else(|| "H.264 codec not registered".to_string())?;

    let mut ost_video = octx
        .add_stream(h264_for_stream)
        .map_err(|e| format!("add video stream: {e}"))?;
    ost_video.set_time_base(out_tb);

    // Open the best available encoder. This MUST happen before write_header
    // so we can copy codecpar in.  HW context (if any) is kept alive here.
    let (mut video_encoder, hw_backend, hw_device) =
        try_open_hw_encoder(spec.width, spec.height, spec.fps, out_tb, &octx);

    crate::media_log!("[encode] video encoder backend: {hw_backend:?}");

    // For HW backends we need the hw_frames_ctx pointer to upload frames later.
    // It lives inside the opened encoder's AVCodecContext.
    let hw_frames_ctx_ptr: *mut ffmpeg::ffi::AVBufferRef =
        if hw_backend != HwBackend::Software && hw_backend != HwBackend::VideoToolbox {
            unsafe { (*video_encoder.as_ptr()).hw_frames_ctx }
        } else {
            std::ptr::null_mut()
        };

    // Force square pixels — same as the original code path.
    video_encoder.set_aspect_ratio(Rational::new(1, 1));

    unsafe {
        let ret = ffmpeg::ffi::avcodec_parameters_from_context(
            (**(*octx.as_mut_ptr()).streams.add(0)).codecpar,
            video_encoder.as_ptr() as *mut ffmpeg::ffi::AVCodecContext,
        );
        if ret < 0 {
            return Err(format!(
                "avcodec_parameters_from_context (video) failed: {ret}"
            ));
        }
    }

    // ── Audio encoder (stream 1) ──────────────────────────────────────────────
    let audio_tb = Rational::new(1, AUDIO_RATE);

    let aac = encoder::find(CodecId::AAC).ok_or_else(|| "AAC encoder not found".to_string())?;

    let mut ost_audio = octx
        .add_stream(aac)
        .map_err(|e| format!("add audio stream: {e}"))?;
    ost_audio.set_time_base(audio_tb);

    let audio_enc_ctx = codec::context::Context::new_with_codec(aac);
    let mut audio_enc = audio_enc_ctx
        .encoder()
        .audio()
        .map_err(|e| format!("create audio encoder context: {e}"))?;

    audio_enc.set_rate(AUDIO_RATE);
    audio_enc.set_ch_layout(ChannelLayout::STEREO);
    audio_enc.set_format(Sample::F32(SampleType::Planar));
    audio_enc.set_bit_rate(128_000);

    if octx
        .format()
        .flags()
        .contains(ffmpeg::format::Flags::GLOBAL_HEADER)
    {
        audio_enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    let audio_encoder = audio_enc
        .open_as_with(aac, ffmpeg::Dictionary::new())
        .map_err(|e| format!("open AAC encoder: {e}"))?;

    let audio_frame_size = (audio_encoder.frame_size() as usize).max(1024);

    unsafe {
        let ret = ffmpeg::ffi::avcodec_parameters_from_context(
            (**(*octx.as_mut_ptr()).streams.add(1)).codecpar,
            audio_encoder.as_ptr() as *mut ffmpeg::ffi::AVCodecContext,
        );
        if ret < 0 {
            return Err(format!(
                "avcodec_parameters_from_context (audio) failed: {ret}"
            ));
        }
    }

    // ── Write output header ───────────────────────────────────────────────────
    ffmpeg::format::context::output::dump(&octx, 0, Some(&spec.output.to_string_lossy()));
    octx.write_header()
        .map_err(|e| format!("write output header: {e}"))?;

    let ost_audio_tb = octx.stream(1).unwrap().time_base();

    let mut audio_state = AudioEncState {
        encoder: audio_encoder,
        out_sample_idx: 0,
        frame_size: audio_frame_size,
        fifo: AudioFifo::new(),
        audio_tb,
        ost_audio_tb,
        overlays: spec
            .audio_overlays
            .iter()
            .filter_map(|ov| match decode_overlay(ov) {
                Ok(d) => Some(d),
                Err(e) => {
                    crate::media_log!("[encode] overlay decode failed: {e}");
                    // Surface to the UI — a silent drop here means the render
                    // completes with no audio and no visible diagnostic.
                    let _ = tx.send(MediaResult::EncodeError {
                        job_id: spec.job_id,
                        msg: format!("audio overlay decode failed: {e}"),
                    });
                    None
                }
            })
            .collect(),
        fifo_overrun_count: 0,
    };

    // ── Per-clip encode loop ──────────────────────────────────────────────────
    let mut output_frame_idx: i64 = 0;
    let mut last_video_dts: i64 = i64::MIN;
    let mut incoming_skip_secs: f64 = 0.0;
    let transition_registry = registry();

    for (clip_idx, clip) in spec.clips.iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }

        let skip = incoming_skip_secs;
        incoming_skip_secs = 0.0;

        let transition_entry = if clip_idx + 1 < spec.clips.len() {
            spec.transitions
                .iter()
                .find(|t| t.after_clip_index == clip_idx)
                .filter(|t| t.kind.kind != TransitionKind::Cut)
        } else {
            None
        };

        let transition_secs: f64 = transition_entry
            .map(|t| t.kind.duration_secs as f64)
            .unwrap_or(0.0);

        let effective = ClipSpec {
            path: clip.path.clone(),
            source_offset: clip.source_offset + skip,
            duration: (clip.duration - skip - transition_secs).max(0.0),
            volume: clip.volume,
            skip_audio: clip.skip_audio,
            fade_in_secs: clip.fade_in_secs,
            fade_in_start_secs: clip.fade_in_start_secs,
            fade_out_secs: clip.fade_out_secs,
            fade_out_end_secs: clip.fade_out_end_secs,
            filter: clip.filter.clone(),
        };

        output_frame_idx = encode_clip(
            &effective,
            spec,
            &mut octx,
            &mut video_encoder,
            hw_frames_ctx_ptr,
            hw_backend,
            &mut audio_state,
            output_frame_idx,
            total_frames,
            frame_tb,
            &cancel,
            tx,
            &mut last_video_dts,
        )?;

        if let Some(entry) = transition_entry {
            let next_clip = &spec.clips[clip_idx + 1];

            let tail_spec = ClipSpec {
                path: clip.path.clone(),
                source_offset: effective.source_offset + effective.duration,
                duration: transition_secs,
                volume: clip.volume,
                skip_audio: false,
                fade_in_secs: 0.0,
                fade_in_start_secs: 0.0,
                fade_out_secs: 0.0,
                fade_out_end_secs: 0.0,
                filter: clip.filter.clone(), // inherit the outgoing clip's filter
            };
            let head_spec = ClipSpec {
                path: next_clip.path.clone(),
                source_offset: next_clip.source_offset,
                duration: transition_secs,
                volume: next_clip.volume,
                skip_audio: false,
                fade_in_secs: 0.0,
                fade_in_start_secs: 0.0,
                fade_out_secs: 0.0,
                fade_out_end_secs: 0.0,
                filter: next_clip.filter.clone(), // inherit the incoming clip's filter
            };

            if let Some(transition_impl) = transition_registry.get(&entry.kind.kind) {
                output_frame_idx = apply_transition(
                    transition_impl.as_ref(),
                    &tail_spec,
                    &head_spec,
                    spec,
                    &mut octx,
                    &mut video_encoder,
                    hw_frames_ctx_ptr,
                    hw_backend,
                    &mut audio_state,
                    output_frame_idx,
                    total_frames,
                    frame_tb,
                    &cancel,
                    tx,
                    &mut last_video_dts,
                )?;

                incoming_skip_secs = transition_secs;
            }
        }

        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
    }

    // ── Extend video for overlay tail ─────────────────────────────────────────
    // If any audio overlay extends past the last video frame, generate black
    // (YUV limited-range black: Y=16, U=128, V=128) video frames for the
    // duration of the tail.  This preserves the user's explicit overlay
    // placement — the overlay ends exactly where they set it, not where the
    // video ends.  The FIFO is silence-padded each frame so drain_fifo mixes
    // the overlay in normally.
    {
        let video_end_sample = output_frame_idx * AUDIO_RATE as i64 / spec.fps as i64;
        let overlay_end_sample = audio_state
            .overlays
            .iter()
            .map(|ov| ov.start_sample + ov.sample_count as i64)
            .max()
            .unwrap_or(0);

        if overlay_end_sample > video_end_sample {
            let extra_samples = overlay_end_sample - video_end_sample;
            // Round up so the last partial AAC frame is always included.
            let extra_frames =
                ((extra_samples as f64 * spec.fps as f64 / AUDIO_RATE as f64).ceil() as i64).max(0);

            crate::media_log!(
                "[encode] overlay tail: {:.3}s past video end — appending {} blank frame(s)",
                extra_samples as f64 / AUDIO_RATE as f64,
                extra_frames,
            );

            let ost_video_tb = octx.stream(0).unwrap().time_base();

            // Pre-allocate one black YUV420P frame and reuse it for all blank frames.
            // Y=16 (black, BT.709 limited), U=V=128 (neutral chroma).
            let mut blank = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            unsafe {
                let ptr = blank.as_mut_ptr();
                let y_stride = (*ptr).linesize[0] as usize;
                let uv_stride = (*ptr).linesize[1] as usize;
                for row in 0..spec.height as usize {
                    let p = (*ptr).data[0].add(row * y_stride);
                    std::slice::from_raw_parts_mut(p, spec.width as usize).fill(16u8);
                }
                for row in 0..(spec.height as usize / 2) {
                    let pu = (*ptr).data[1].add(row * uv_stride);
                    let pv = (*ptr).data[2].add(row * uv_stride);
                    std::slice::from_raw_parts_mut(pu, spec.width as usize / 2).fill(128u8);
                    std::slice::from_raw_parts_mut(pv, spec.width as usize / 2).fill(128u8);
                }
                (*ptr).sample_aspect_ratio = ffmpeg::ffi::AVRational { num: 1, den: 1 };
            }

            for _ in 0..extra_frames {
                if cancel.load(Ordering::Acquire) {
                    return Err("cancelled".into());
                }

                blank.set_pts(Some(output_frame_idx));
                send_video_frame(&blank, &mut video_encoder, hw_frames_ctx_ptr, hw_backend)?;

                let mut pkt = Packet::empty();
                while video_encoder.receive_packet(&mut pkt).is_ok() {
                    pkt.set_stream(0);
                    pkt.rescale_ts(frame_tb, ost_video_tb);
                    let raw_dts = pkt.dts().unwrap_or(0);
                    if last_video_dts != i64::MIN {
                        let prev_s = last_video_dts as f64 * f64::from(ost_video_tb);
                        let dts_s = raw_dts as f64 * f64::from(ost_video_tb);
                        if dts_s < prev_s {
                            let clamped = last_video_dts + 1;
                            crate::media_log!(
                                "[encode] blank-frame non-monotonic DTS \
                                 ({prev_s:.4}s → {dts_s:.4}s); clamping {raw_dts} → {clamped}"
                            );
                            unsafe {
                                (*pkt.as_mut_ptr()).dts = clamped;
                            }
                        }
                    }
                    last_video_dts = pkt.dts().unwrap_or(raw_dts);
                    pkt.write_interleaved(&mut octx)
                        .map_err(|e| format!("write blank video packet: {e}"))?;
                }

                output_frame_idx += 1;

                // Yield to the OS scheduler after every encoded frame.
                // SW encodes: combined with the n_cores/2 thread cap and
                // BELOW_NORMAL priority this keeps the system responsive at
                // all resolutions (480p through 4K), not just at 2K/4K.
                // HW encodes: the CPU decode+scale loop feeding the GPU runs
                // flat-out; yield_now() lets UI, audio, and scrub threads
                // preempt it whenever they need a core.
                std::thread::yield_now();

                // Pad silence into the FIFO so drain_fifo can mix the overlay tail.
                let expected = output_frame_idx * AUDIO_RATE as i64 / spec.fps as i64;
                let have = audio_state.out_sample_idx + audio_state.fifo.len() as i64;
                let gap = (expected - have).max(0) as usize;
                if gap > 0 {
                    audio_state
                        .fifo
                        .left
                        .extend(std::iter::repeat_n(0.0f32, gap));
                    audio_state
                        .fifo
                        .right
                        .extend(std::iter::repeat_n(0.0f32, gap));
                }

                audio_state.drain_fifo(&mut octx, false)?;

                if (output_frame_idx as u64).is_multiple_of(PROGRESS_INTERVAL) {
                    let _ = tx.send(MediaResult::EncodeProgress {
                        job_id: spec.job_id,
                        frame: output_frame_idx as u64,
                        total_frames,
                    });
                }
            }
        }
    }

    // ── Flush video encoder ───────────────────────────────────────────────────
    video_encoder
        .send_eof()
        .map_err(|e| format!("send EOF to video encoder: {e}"))?;

    let ost_video_tb = octx.stream(0).unwrap().time_base();
    let mut pkt = Packet::empty();
    while video_encoder.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(0);
        let frame_pts = pkt.pts().unwrap_or(pkt.dts().unwrap_or(0));
        pkt.rescale_ts(frame_tb, ost_video_tb);
        let raw_dts = pkt.dts().unwrap_or(0);
        if last_video_dts != i64::MIN {
            let prev_s = last_video_dts as f64 * f64::from(ost_video_tb);
            let dts_s = raw_dts as f64 * f64::from(ost_video_tb);
            if dts_s < prev_s {
                let clamped = last_video_dts + 1;
                crate::media_log!(
                    "[encode] encoder-flush non-monotonic DTS ({prev_s:.4}s → {dts_s:.4}s); \
                     clamping {raw_dts} → {clamped}"
                );
                unsafe {
                    (*pkt.as_mut_ptr()).dts = clamped;
                }
            }
        }
        last_video_dts = pkt.dts().unwrap_or(raw_dts);
        pkt.write_interleaved(&mut octx)
            .map_err(|e| format!("write flush video packet: {e}"))?;
        output_frame_idx = output_frame_idx.max(frame_pts + 1);
    }

    // ── Trim clip audio to video boundary ─────────────────────────────────────
    // Removes clip audio (silence padding, decoded PCM) that overshot the video
    // endpoint.  Overlay audio has already been mixed into encoded AAC packets
    // by drain_fifo — the FIFO at this point contains only the clip-audio tail,
    // so trimming here never discards overlay content.
    //
    // Note: if overlays extended past video (handled above) output_frame_idx was
    // already advanced to cover the full overlay tail.  Any residual FIFO content
    // is a sub-frame rounding artifact (< frame_size samples) that is safe to
    // trim or flush.
    {
        let target_audio_samples = output_frame_idx * AUDIO_RATE as i64 / spec.fps as i64;
        let total_audio = audio_state.out_sample_idx + audio_state.fifo.len() as i64;
        let excess = (total_audio - target_audio_samples).max(0) as usize;
        if excess > 0 {
            crate::media_log!(
                "[encode] trimming {} trailing clip-audio samples ({:.3}s) — \
                 clip audio ran past video end ({:.3}s)",
                excess,
                excess as f64 / AUDIO_RATE as f64,
                output_frame_idx as f64 / spec.fps as f64,
            );
            let new_len = audio_state.fifo.left.len().saturating_sub(excess);
            audio_state.fifo.left.truncate(new_len);
            audio_state.fifo.right.truncate(new_len);
        } else {
            crate::media_log!(
                "[encode] audio/video end aligned: video={:.3}s audio={:.3}s",
                output_frame_idx as f64 / spec.fps as f64,
                total_audio as f64 / AUDIO_RATE as f64,
            );
        }
        if audio_state.fifo_overrun_count > 1 {
            crate::media_log!(
                "[encode] audio FIFO overrun occurred {} time(s) total \
                 (normal for SW encode at 1080p — audio decoded faster than video encoded)",
                audio_state.fifo_overrun_count,
            );
        }
    }

    // ── Flush audio FIFO then encoder ─────────────────────────────────────────
    audio_state.drain_fifo(&mut octx, true)?;
    audio_state.flush_encoder(&mut octx)?;

    octx.write_trailer()
        .map_err(|e| format!("write trailer: {e}"))?;

    // Keep hw_device alive until after write_trailer — the encoder may still
    // reference device memory during the trailer flush.
    drop(hw_device);

    Ok(())
}

/// Send one YUV420P software frame to the video encoder, uploading to the HW
/// surface if a HW backend is active.
///
/// This helper centralises the upload logic so encode_clip and apply_transition
/// both call a single function rather than duplicating the unsafe block.
// clip encoding moved to clip.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_gain_no_fades_returns_unity() {
        // No fade in/out configured — gain should be 1.0 throughout.
        let g = fade_gain(5.0, 0.0, 10.0, 0.0, 0.0, 0.0, 0.0);
        assert!((g - 1.0).abs() < 1e-6);
    }

    #[test]
    fn fade_gain_before_fade_in_start_is_zero() {
        // fade_in_start_secs = 2.0: first 2 seconds should be silent.
        let g = fade_gain(1.0, 0.0, 10.0, 1.0, 2.0, 0.0, 0.0);
        assert!((g - 0.0).abs() < 1e-6);
    }

    #[test]
    fn fade_gain_during_fade_in_ramp_is_between_zero_and_one() {
        // Halfway through a 2-second fade-in ramp that starts at 0s.
        let g = fade_gain(1.0, 0.0, 10.0, 2.0, 0.0, 0.0, 0.0);
        assert!(g > 0.0 && g < 1.0);
        // At 1s into 2s ramp: sqrt(0.5) ≈ 0.707
        assert!((g - 0.707).abs() < 0.01);
    }

    #[test]
    fn fade_gain_after_fade_in_is_unity() {
        let g = fade_gain(3.0, 0.0, 10.0, 1.0, 0.0, 0.0, 0.0);
        assert!((g - 1.0).abs() < 1e-6);
    }

    #[test]
    fn fade_gain_during_fade_out_ramp_is_between_zero_and_one() {
        // fade_out_secs = 2.0: last 2 seconds of a 10s clip ramp down.
        let g = fade_gain(9.0, 0.0, 10.0, 0.0, 0.0, 2.0, 0.0);
        assert!(g > 0.0 && g < 1.0);
        // At 9s, remain=1s into 2s ramp: sqrt(0.5) ≈ 0.707
        assert!((g - 0.707).abs() < 0.01);
    }

    #[test]
    fn fade_gain_after_fade_out_end_is_zero() {
        // fade_out_end_secs = 1.0: last 1 second is silence after fade-out ramp.
        // clip duration = 10s, current = 9.5s, remain = 0.5s < 1.0 → silence.
        let g = fade_gain(9.5, 0.0, 10.0, 0.0, 0.0, 1.0, 1.0);
        assert!((g - 0.0).abs() < 1e-6);
    }

    #[test]
    fn fade_gain_with_source_offset() {
        // source_offset = 5.0, clip duration = 10.0, fade_in = 1.0
        // pts_secs = 5.5 → elapsed = 0.5, halfway through fade-in → sqrt(0.5) ≈ 0.707
        let g = fade_gain(5.5, 5.0, 10.0, 1.0, 0.0, 0.0, 0.0);
        assert!((g - 0.707).abs() < 0.01);
    }

    #[test]
    fn fade_gain_fade_out_with_end_silence() {
        // 10s clip, fade_out_secs=2.0, fade_out_end_secs=1.0
        // At pts=8.5 (elapsed=8.5, remain=1.5):
        //   remain=1.5 > fade_out_end_secs=1.0 → in ramp
        //   (1.5 - 1.0) / 2.0 = 0.25 → sqrt(0.25) = 0.5
        let g = fade_gain(8.5, 0.0, 10.0, 0.0, 0.0, 2.0, 1.0);
        assert!((g - 0.5).abs() < 0.01);
    }
}

// Crossfade helpers moved to clip.rs
