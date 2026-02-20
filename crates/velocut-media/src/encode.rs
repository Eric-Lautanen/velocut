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
//   Stream 0 — H.264 video (YUV420P, CRF 18, preset fast)
//   Stream 1 — AAC audio  (FLTP stereo, 44100 Hz, 128 kbps)
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
// Cancellation:
//   `cancel` is an Arc<AtomicBool> checked after every video frame. When set,
//   EncodeError { msg: "cancelled" } is sent — the UI treats that as an aborted
//   state distinct from a real error, keeping the cancel path identical to the
//   error path.
//
// Encoder ownership:
//   Both the video::Encoder and audio::Audio are created once in `run_encode`
//   and passed as `&mut` into `encode_clip` / the flush block. We never
//   retrieve either from the output stream — `Stream::codec()` does not exist
//   in this version of ffmpeg-the-third.

use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use crossbeam_channel::Sender;
use uuid::Uuid;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::packet::Mut as PacketMut;
use ffmpeg::codec::{self, Id as CodecId};
use ffmpeg::encoder;
use ffmpeg::format::{Pixel, Sample, input as open_input, output as open_output};
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::scaling::{Context as ScaleCtx, Flags as ScaleFlags};
use ffmpeg::software::resampling;
use ffmpeg::util::channel_layout::{ChannelLayout, ChannelLayoutMask};
use ffmpeg::util::frame::video::Video as VideoFrame;
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg::util::rational::Rational;
use ffmpeg::Packet;

use velocut_core::media_types::MediaResult;
use velocut_core::transitions::{ClipTransition, TransitionType};
use crate::helpers::yuv::{extract_yuv, blend_yuv_frame, write_yuv};
use crate::helpers::seek::seek_to_secs;

// ── Public types ──────────────────────────────────────────────────────────────

/// One source clip's contribution to the output timeline.
#[derive(Clone)]
pub struct ClipSpec {
    /// Absolute path to the source media file.
    pub path:          PathBuf,
    /// Seconds into the source file at which this clip begins.
    pub source_offset: f64,
    /// Duration in seconds to include from this clip.
    pub duration:      f64,
    /// Linear gain applied to decoded audio before encoding (1.0 = unity).
    pub volume:        f32,
    /// When true, no audio is decoded or pushed to the FIFO for this clip.
    ///
    /// Set by `begin_render()` for video-row clips whose audio has been
    /// extracted to a separate A-row clip (`audio_muted = true`). The A-row
    /// clip is included as a separate `ClipSpec` with `skip_audio = false`
    /// covering the same time range so the FIFO receives the correct PCM.
    /// See `begin_render()` in `app.rs` for the pairing logic.
    pub skip_audio:    bool,
}

/// Complete description of an encode job.
pub struct EncodeSpec {
    /// Unique identifier used in all progress / done / error results.
    pub job_id:  Uuid,
    /// Clips in timeline order. Gaps between clips are not filled.
    pub clips:   Vec<ClipSpec>,
    pub width:   u32,
    pub height:  u32,
    /// Output frame rate (integer; fractional rates not needed for NLE output).
    pub fps:     u32,
    /// Destination file, including extension (`.mp4`).
    pub output:  PathBuf,
    /// Transitions between adjacent clips.
    /// Index N means "between clips[N] and clips[N+1]".
    /// Missing entries default to Cut (hard splice, zero overhead).
    pub transitions: Vec<ClipTransition>,
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Send a progress update every this many encoded video frames.
const PROGRESS_INTERVAL: u64 = 15;

/// Output audio sample rate for all exports.
const AUDIO_RATE: i32 = 44100;

// ── Center-crop scaler ────────────────────────────────────────────────────────

/// SwsContext wrapper that center-crops the source to the output aspect ratio
/// before scaling — prevents squishing when source and output AR differ.
///
/// Crop rect is computed once at construction from `src_w/src_h` vs `out_w/out_h`.
/// For matching ARs the crop rect is the full source frame (zero overhead).
///
/// The horizontal crop is applied by advancing per-plane data pointers; the
/// vertical crop is passed as `srcSliceY / srcSliceH` to `sws_scale`. Both
/// offsets are always rounded to even so YUV420P sub-sampling stays aligned.
struct CropScaler {
    ctx:    ScaleCtx,
    /// Byte offset per Y-plane row (horizontal crop, advances data[0]).
    crop_x: u32,
    /// First source row handed to sws_scale as srcSliceY.
    crop_y: u32,
    /// Source row count handed to sws_scale as srcSliceH.
    crop_h: u32,
}

impl CropScaler {
    fn build(
        src_fmt: Pixel,
        src_w:   u32,
        src_h:   u32,
        out_w:   u32,
        out_h:   u32,
    ) -> Self {
        let src_ar = src_w as f64 / src_h.max(1) as f64;
        let out_ar = out_w as f64 / out_h.max(1) as f64;

        // Crop rect that maps source pixels to the output AR (center crop).
        // All edges rounded to even for YUV420P chroma alignment.
        let (crop_x, crop_y, crop_w, crop_h) = if (src_ar - out_ar).abs() < 1e-4 {
            (0, 0, src_w, src_h)
        } else if src_ar > out_ar {
            // Source wider — crop sides, keep full height.
            let cw = ((src_h as f64 * out_ar).round() as u32).min(src_w) & !1;
            let cx = ((src_w - cw) / 2) & !1;
            (cx, 0u32, cw, src_h)
        } else {
            // Source taller — crop top/bottom, keep full width.
            let ch = ((src_w as f64 / out_ar).round() as u32).min(src_h) & !1;
            let cy = ((src_h - ch) / 2) & !1;
            (0u32, cy, src_w, ch)
        };

        let ctx = ScaleCtx::get(
            src_fmt, crop_w.max(2), crop_h.max(2),
            Pixel::YUV420P, out_w, out_h,
            ScaleFlags::BILINEAR,
        ).expect("CropScaler: SwsContext");

        Self { ctx, crop_x, crop_y, crop_h }
    }

    /// Scale `src` into `dst` with center-crop.
    ///
    /// `dst` must be pre-allocated (e.g. `VideoFrame::new(YUV420P, out_w, out_h)`).
    /// Handles planar YUV formats (YUV420P / 422P / 444P and their J-range
    /// variants) which cover effectively all H.264 / H.265 decoder output.
    /// For other formats the horizontal crop is skipped; vertical crop still applies.
    fn run(&mut self, src: &VideoFrame, dst: &mut VideoFrame) -> Result<(), String> {
        unsafe {
            let sf = src.as_ptr();
            let df = dst.as_mut_ptr();

            // Per-plane horizontal byte offsets for the crop.
            let (off_y, off_uv): (usize, usize) = match src.format() {
                Pixel::YUV420P | Pixel::YUVJ420P |
                Pixel::YUV422P | Pixel::YUVJ422P => {
                    (self.crop_x as usize, self.crop_x as usize / 2)
                }
                Pixel::YUV444P | Pixel::YUVJ444P => {
                    let o = self.crop_x as usize;
                    (o, o)
                }
                _ => (0, 0), // unknown packed/HBD format — skip horizontal crop
            };

            // Advance data pointers into the crop rect.
            //
            // The SwsContext was built with (crop_w × crop_h) as source dims.
            // sws_scale's srcSliceY is an offset INTO those declared dims, so
            // passing srcSliceY=crop_y makes (crop_y + crop_h) > crop_h → EINVAL.
            // Instead, pre-advance data pointers to row crop_y and pass srcSliceY=0,
            // which is consistent with the declared source dimensions.
            //
            // UV planes are half-height in YUV420P so their row offset is halved.
            let ls = &(*sf).linesize;
            let y_row_off  = self.crop_y as usize * ls[0] as usize;
            let uv_row_off = (self.crop_y as usize / 2) * ls[1] as usize;

            let src_planes: [*const u8; 4] = [
                (*sf).data[0].add(off_y + y_row_off),
                if (*sf).data[1].is_null() { std::ptr::null() } else { (*sf).data[1].add(off_uv + uv_row_off) },
                if (*sf).data[2].is_null() { std::ptr::null() } else { (*sf).data[2].add(off_uv + uv_row_off) },
                std::ptr::null(),
            ];

            let ret = ffmpeg::ffi::sws_scale(
                self.ctx.as_mut_ptr(),
                src_planes.as_ptr() as _,
                (*sf).linesize.as_ptr(),
                0,                // srcSliceY=0: pointer is already at crop_y
                self.crop_h as _,
                (*df).data.as_mut_ptr() as _,
                (*df).linesize.as_mut_ptr(),
            );

            if ret <= 0 {
                return Err(format!("CropScaler::run sws_scale returned {ret}"));
            }
        }
        Ok(())
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Encode `spec` to disk. Blocking — run this on a dedicated thread.
pub fn encode_timeline(
    spec:   EncodeSpec,
    cancel: Arc<AtomicBool>,
    tx:     Sender<MediaResult>,
) {
    let total_frames: u64 = spec.clips.iter()
        .map(|c| (c.duration * spec.fps as f64).ceil() as u64)
        .sum::<u64>()
        .max(1);

    match run_encode(&spec, cancel, total_frames, &tx) {
        Ok(()) => {
            let _ = tx.send(MediaResult::EncodeDone {
                job_id: spec.job_id,
                path:   spec.output.clone(),
            });
        }
        Err(e) => {
            let _ = tx.send(MediaResult::EncodeError {
                job_id: spec.job_id,
                msg:    e,
            });
        }
    }
}

// ── Audio FIFO ────────────────────────────────────────────────────────────────

/// Stereo FLTP (float planar) sample ring buffer.
///
/// Left channel samples are in `self.left`; right in `self.right`.
/// When a mono source is decoded, both planes are filled from channel 0
/// so the output is always properly stereo.
struct AudioFifo {
    left:  Vec<f32>,
    right: Vec<f32>,
}

impl AudioFifo {
    fn new() -> Self { Self { left: Vec::new(), right: Vec::new() } }

    /// How many samples are currently buffered (per channel).
    fn len(&self) -> usize { self.left.len() }

    /// Append one decoded / resampled FLTP audio frame.
    ///
    /// The frame must be in FLTP format (float planar); stereo or mono.
    /// Mono frames are duplicated to both output channels.
    fn push(&mut self, frame: &AudioFrame) {
        self.push_scaled(frame, 1.0);
    }

    /// Like `push`, but multiplies every sample by `volume` before buffering.
    /// Volume is a linear gain: 1.0 = unity, 0.0 = silence, 2.0 = +6 dB.
    fn push_scaled(&mut self, frame: &AudioFrame, volume: f32) {
        let n = frame.samples();
        if n == 0 { return; }
        unsafe {
            let l_bytes = frame.data(0);
            let l_f32 = std::slice::from_raw_parts(l_bytes.as_ptr() as *const f32, n);
            self.left.extend(l_f32.iter().map(|s| (s * volume).clamp(-1.0, 1.0)));

            let r_bytes = if frame.ch_layout().channels() >= 2 { frame.data(1) } else { frame.data(0) };
            let r_f32 = std::slice::from_raw_parts(r_bytes.as_ptr() as *const f32, n);
            self.right.extend(r_f32.iter().map(|s| (s * volume).clamp(-1.0, 1.0)));
        }
    }

    /// Pop one encoder-sized frame from the front of the FIFO.
    ///
    /// If fewer than `n` samples remain the tail is zero-padded (used only for
    /// the final flush frame so the AAC encoder receives a full fixed-size input).
    /// The returned frame has its PTS set to `sample_idx` in the 1/44100 timebase.
    fn pop_frame(&mut self, n: usize, sample_idx: i64) -> AudioFrame {
        let available = self.left.len().min(n);

        let mut frame = AudioFrame::new(
            Sample::F32(SampleType::Planar),
            n,
            ChannelLayoutMask::STEREO,
        );
        frame.set_rate(AUDIO_RATE as u32);
        frame.set_pts(Some(sample_idx));

        unsafe {
            let ldata = frame.data_mut(0);
            let ldst  = std::slice::from_raw_parts_mut(ldata.as_mut_ptr() as *mut f32, n);
            ldst[..available].copy_from_slice(&self.left[..available]);
            if available < n { ldst[available..].fill(0.0); }

            let rdata = frame.data_mut(1);
            let rdst  = std::slice::from_raw_parts_mut(rdata.as_mut_ptr() as *mut f32, n);
            rdst[..available].copy_from_slice(&self.right[..available]);
            if available < n { rdst[available..].fill(0.0); }
        }

        self.left.drain(..available);
        self.right.drain(..available);

        frame
    }
}

// ── Audio encoder state ───────────────────────────────────────────────────────

/// Everything needed to drive the AAC encoder across multiple clips.
struct AudioEncState {
    encoder:        ffmpeg::encoder::Audio,
    /// Next output frame's PTS in samples (audio stream timebase = 1/44100).
    out_sample_idx: i64,
    /// AAC frame size in samples (typically 1024).
    frame_size:     usize,
    fifo:           AudioFifo,
    /// 1/AUDIO_RATE — used for PTS rescaling when writing packets.
    audio_tb:       Rational,
    /// The muxer-assigned timebase for stream 1 (may differ from audio_tb).
    ost_audio_tb:   Rational,
}

impl AudioEncState {
    /// Drain buffered samples → encode → write interleaved to `octx`.
    ///
    /// In normal operation (`flush = false`) only full frames are sent.
    /// At the end of the encode (`flush = true`) a partial tail frame is
    /// zero-padded and flushed so no PCM is lost.
    fn drain_fifo(
        &mut self,
        octx:  &mut ffmpeg::format::context::Output,
        flush: bool,
    ) -> Result<(), String> {
        // Warn if the FIFO is growing unusually deep — indicates audio is outrunning
        // video consumption (e.g. audio packets pushing well past clip_end).
        if !flush && self.fifo.len() > 2 * self.frame_size {
            eprintln!(
                "[encode] audio FIFO overrun: {} samples buffered (threshold={}); \
                 audio may be running ahead of video",
                self.fifo.len(), 2 * self.frame_size
            );
        }
        while self.fifo.len() >= self.frame_size
            || (flush && self.fifo.len() > 0)
        {
            let frame = self.fifo.pop_frame(self.frame_size, self.out_sample_idx);
            self.out_sample_idx += self.frame_size as i64;

            self.encoder.send_frame(&frame)
                .map_err(|e| format!("send audio frame to encoder: {e}"))?;

            self.drain_packets(octx)?;
        }
        Ok(())
    }

    /// Receive all available encoded packets and write them to the muxer.
    fn drain_packets(
        &mut self,
        octx: &mut ffmpeg::format::context::Output,
    ) -> Result<(), String> {
        let mut pkt = Packet::empty();
        while self.encoder.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(1);
            pkt.rescale_ts(self.audio_tb, self.ost_audio_tb);
            pkt.write_interleaved(octx)
                .map_err(|e| format!("write audio packet: {e}"))?;
        }
        Ok(())
    }

    /// Send EOF to the AAC encoder and flush any remaining output packets.
    fn flush_encoder(
        &mut self,
        octx: &mut ffmpeg::format::context::Output,
    ) -> Result<(), String> {
        self.encoder.send_eof()
            .map_err(|e| format!("send EOF to audio encoder: {e}"))?;
        self.drain_packets(octx)
    }
}

// ── Internal implementation ───────────────────────────────────────────────────

fn run_encode(
    spec:         &EncodeSpec,
    cancel:       Arc<AtomicBool>,
    total_frames: u64,
    tx:           &Sender<MediaResult>,
) -> Result<(), String> {
    if spec.clips.is_empty() {
        return Err("nothing to encode: timeline is empty".into());
    }

    // ── Output context ────────────────────────────────────────────────────────
    let mut octx = open_output(&spec.output)
        .map_err(|e| format!("could not open output '{}': {e}", spec.output.display()))?;

    // ── Video encoder (stream 0) ──────────────────────────────────────────────
    // Create the codec context independently of the output stream — Stream does
    // not expose a .codec() accessor in this version of ffmpeg-the-third.
    let out_tb   = Rational::new(1, spec.fps as i32);
    let frame_tb = Rational::new(1, spec.fps as i32);

    let h264 = encoder::find(CodecId::H264)
        .ok_or_else(|| "H.264 encoder not found — is libx264 available?".to_string())?;

    let mut ost_video = octx.add_stream(h264)
        .map_err(|e| format!("add video stream: {e}"))?;
    ost_video.set_time_base(out_tb);

    let video_enc_ctx = codec::context::Context::new_with_codec(h264);
    let mut video_enc = video_enc_ctx.encoder().video()
        .map_err(|e| format!("create video encoder context: {e}"))?;

    video_enc.set_width(spec.width);
    video_enc.set_height(spec.height);
    video_enc.set_format(Pixel::YUV420P);
    video_enc.set_time_base(out_tb);
    video_enc.set_frame_rate(Some(Rational::new(spec.fps as i32, 1)));
    video_enc.set_bit_rate(0); // CRF controls quality; bit_rate 0 signals VBR

    // MP4 requires SPS/PPS in the avcC box (AVCC format). Setting GLOBAL_HEADER
    // tells libx264 to populate extradata during open so avcodec_parameters_from_context
    // copies it into codecpar before the muxer writes the file header.
    if octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER) {
        video_enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    let mut opts = ffmpeg::Dictionary::new();
    opts.set("crf",    "18");
    opts.set("preset", "fast");
    // Force a keyframe every second so scrubbing stays responsive after import.
    // libx264 default is keyint=250 (~8s at 30fps); camera files and NLE-ready
    // exports typically use 1-2s. The scrub thread seeks to the nearest keyframe
    // then burns through — a 250-frame GOP makes that 8x slower than a 30-frame
    // GOP for every seek, which is the primary cause of choppy scrub on exports.
    opts.set("g", &spec.fps.to_string());

    let mut video_encoder = video_enc.open_as_with(h264, opts)
        .map_err(|e| format!("open H.264 encoder: {e}"))?;

    // Force square pixels in the OPENED encoder context.  Must be set here —
    // after open_as_with — because libavcodec resets sample_aspect_ratio to
    // 0:1 during codec initialisation, clobbering anything set on video_enc
    // before the open.  avcodec_parameters_from_context reads from video_encoder
    // (the post-open context), so this is the only place that sticks.
    video_encoder.set_aspect_ratio(Rational::new(1, 1));

    // Copy encoder params into the stream's codecpar so the muxer has resolution,
    // format, and codec-private data. set_parameters() requires AsPtr<AVCodecParameters>;
    // encoder::Video does not implement that trait, so we use FFI directly.
    unsafe {
        let ret = ffmpeg::ffi::avcodec_parameters_from_context(
            (**(*octx.as_mut_ptr()).streams.add(0)).codecpar,
            video_encoder.as_ptr() as *mut ffmpeg::ffi::AVCodecContext,
        );
        if ret < 0 {
            return Err(format!("avcodec_parameters_from_context (video) failed: {ret}"));
        }
    }

    // ── Audio encoder (stream 1) ──────────────────────────────────────────────
    // Target format: 44100 Hz stereo FLTP — the native AAC encoder accepts this
    // without transcoding on the encoder side. All source audio is resampled to
    // this format before being pushed into the FIFO.
    let audio_tb = Rational::new(1, AUDIO_RATE);

    let aac = encoder::find(CodecId::AAC)
        .ok_or_else(|| "AAC encoder not found".to_string())?;

    let mut ost_audio = octx.add_stream(aac)
        .map_err(|e| format!("add audio stream: {e}"))?;
    ost_audio.set_time_base(audio_tb);

    let audio_enc_ctx = codec::context::Context::new_with_codec(aac);
    let mut audio_enc = audio_enc_ctx.encoder().audio()
        .map_err(|e| format!("create audio encoder context: {e}"))?;

    audio_enc.set_rate(AUDIO_RATE);
    audio_enc.set_ch_layout(ChannelLayout::STEREO);
    audio_enc.set_format(Sample::F32(SampleType::Planar));
    audio_enc.set_bit_rate(128_000);

    // Same GLOBAL_HEADER requirement as the video encoder above.
    if octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER) {
        audio_enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER);
    }

    let audio_encoder = audio_enc.open_as_with(aac, ffmpeg::Dictionary::new())
        .map_err(|e| format!("open AAC encoder: {e}"))?;

    // Guard against a codec that returns 0 (shouldn't happen with AAC but be safe).
    let audio_frame_size = (audio_encoder.frame_size() as usize).max(1024);

    unsafe {
        let ret = ffmpeg::ffi::avcodec_parameters_from_context(
            (**(*octx.as_mut_ptr()).streams.add(1)).codecpar,
            audio_encoder.as_ptr() as *mut ffmpeg::ffi::AVCodecContext,
        );
        if ret < 0 {
            return Err(format!("avcodec_parameters_from_context (audio) failed: {ret}"));
        }
    }

    // ── Write output header ───────────────────────────────────────────────────
    ffmpeg::format::context::output::dump(&octx, 0, Some(&spec.output.to_string_lossy()));
    octx.write_header()
        .map_err(|e| format!("write output header: {e}"))?;

    // Fetch the muxer-assigned timebase for stream 1 AFTER write_header — the MP4
    // muxer may normalize stream timebases during avformat_write_header, so any
    // value read before that call can be stale and cause audio drift.
    let ost_audio_tb = octx.stream(1).unwrap().time_base();

    let mut audio_state = AudioEncState {
        encoder:        audio_encoder,
        out_sample_idx: 0,
        frame_size:     audio_frame_size,
        fifo:           AudioFifo::new(),
        audio_tb,
        ost_audio_tb,
    };

    // ── Per-clip encode loop ──────────────────────────────────────────────────
    let mut output_frame_idx: i64 = 0;
    // Shared DTS monotonicity guard — persists across all encode_clip calls and
    // into the final encoder flush so B-frame reorder packets are clamped
    // consistently even at clip boundaries and at the very end.
    let mut last_video_dts: i64 = i64::MIN;
    // How many seconds to skip at the START of the next clip (set when an
    // outgoing crossfade blends the incoming clip's head, so encode_clip
    // doesn't re-encode those frames).
    let mut incoming_skip_secs: f64 = 0.0;

    for (clip_idx, clip) in spec.clips.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        // Snapshot the incoming skip for this clip, then reset for the next one.
        let skip = incoming_skip_secs;
        incoming_skip_secs = 0.0;

        // Check for an outgoing crossfade after this clip.
        let crossfade_secs: f64 = if clip_idx + 1 < spec.clips.len() {
            spec.transitions.iter()
                .find(|t| t.after_clip_index == clip_idx)
                .and_then(|t| match &t.kind {
                    TransitionType::Crossfade { duration_secs } => Some(*duration_secs as f64),
                    TransitionType::Cut => None,
                })
                .unwrap_or(0.0)
        } else {
            0.0
        };

        // Build the effective ClipSpec for this clip:
        //   - skip: skip the head that was already blended by the incoming crossfade
        //   - crossfade_secs: stop that many seconds early so apply_crossfade can
        //     blend the tail with the next clip's head
        let effective = ClipSpec {
            path:          clip.path.clone(),
            source_offset: clip.source_offset + skip,
            duration:      (clip.duration - skip - crossfade_secs).max(0.0),
            volume:        clip.volume,
            skip_audio:    clip.skip_audio,
        };

        output_frame_idx = encode_clip(
            &effective,
            spec,
            &mut octx,
            &mut video_encoder,
            &mut audio_state,
            output_frame_idx,
            total_frames,
            frame_tb,
            &cancel,
            tx,
            &mut last_video_dts,
        )?;

        // ── Transition hook ───────────────────────────────────────────────────
        if crossfade_secs > 0.0 {
            let next_clip = &spec.clips[clip_idx + 1];

            // Tail: last `crossfade_secs` of the current clip (just after encode_clip stopped).
            // skip_audio = false: the crossfade audio blend needs PCM from both clips.
            let tail_spec = ClipSpec {
                path:          clip.path.clone(),
                source_offset: effective.source_offset + effective.duration,
                duration:      crossfade_secs,
                volume:        clip.volume,
                skip_audio:    false,
            };
            // Head: first `crossfade_secs` of the next clip.
            let head_spec = ClipSpec {
                path:          next_clip.path.clone(),
                source_offset: next_clip.source_offset,
                duration:      crossfade_secs,
                volume:        next_clip.volume,
                skip_audio:    false,
            };

            output_frame_idx = apply_crossfade(
                &tail_spec,
                &head_spec,
                spec,
                &mut octx,
                &mut video_encoder,
                &mut audio_state,
                output_frame_idx,
                total_frames,
                frame_tb,
                &cancel,
                tx,
            )?;

            // Tell the next iteration to skip the head it already blended.
            incoming_skip_secs = crossfade_secs;
        }

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
    }

    // ── Flush video encoder ───────────────────────────────────────────────────
    video_encoder.send_eof()
        .map_err(|e| format!("send EOF to video encoder: {e}"))?;

    let ost_video_tb = octx.stream(0).unwrap().time_base();
    let mut pkt = Packet::empty();
    while video_encoder.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(0);
        // Capture PTS in frame_tb units (frame-count) BEFORE rescale_ts.
        // After rescale_ts the packet PTS is in ost_video_tb units (e.g. 1/12800),
        // which is orders of magnitude larger than the frame index.  Using the
        // post-rescale value for output_frame_idx would make target_audio_samples
        // astronomically large, completely defeating the audio-trim block below.
        let frame_pts = pkt.pts().unwrap_or(pkt.dts().unwrap_or(0));
        pkt.rescale_ts(frame_tb, ost_video_tb);
        let raw_dts = pkt.dts().unwrap_or(0);
        let raw_pts = pkt.pts().unwrap_or(raw_dts);
        let dts_s   = raw_dts as f64 * f64::from(ost_video_tb);
        let pts_s   = raw_pts as f64 * f64::from(ost_video_tb);
        eprintln!("[encode] encoder-flush pkt dts={dts_s:.4}s pts={pts_s:.4}s");
        if last_video_dts != i64::MIN {
            let prev_s = last_video_dts as f64 * f64::from(ost_video_tb);
            if dts_s < prev_s {
                let clamped = last_video_dts + 1;
                eprintln!(
                    "[encode] encoder-flush non-monotonic DTS ({prev_s:.4}s → {dts_s:.4}s); \
                     clamping {raw_dts} → {clamped}"
                );
                unsafe { (*pkt.as_mut_ptr()).dts = clamped; }
            }
        }
        last_video_dts = pkt.dts().unwrap_or(raw_dts);
        pkt.write_interleaved(&mut octx)
            .map_err(|e| format!("write flush video packet: {e}"))?;
        // Track the highest PTS seen so output_frame_idx reflects the true
        // video duration including all B-frame lookahead frames.  The audio
        // trim below uses this value, so it must be updated here.
        // Use frame_pts (pre-rescale, in frame_tb / frame-count units) so
        // the frame index stays comparable to the fps-based audio calculation.
        output_frame_idx = output_frame_idx.max(frame_pts + 1);
    }

    // ── Trim audio to video boundary ──────────────────────────────────────────
    // The demuxer loop in encode_clip continues reading audio packets until
    // file EOF — not until clip_end.  On the last clip this pushes potentially
    // 1–2 s of extra PCM into the FIFO that has no corresponding video frames.
    // drain_fifo(flush=true) below would write all of it, making the audio
    // stream longer than the video stream.  Container duration = max(audio,
    // video), so the player waits for video that never arrives → tail freeze.
    //
    // Trim the FIFO right now so that (out_sample_idx + fifo.len) does not
    // exceed the exact sample count that matches the last video frame.
    {
        // Subtract one AAC frame_size from the target.
        //
        // After the FIFO trim, drain_fifo(flush=true) zero-pads the remaining
        // partial frame to exactly frame_size (1024 samples = 23 ms) before
        // sending it to the encoder.  Without this adjustment the padded tail
        // always pushes audio end ~23 ms past the last video frame, which some
        // players render as a 1-2 frame hold.  Trimming one extra frame_size
        // ensures the final padded AAC frame lands at or just before the last
        // video frame rather than after it.
        let target_audio_samples =
            output_frame_idx as i64 * AUDIO_RATE as i64 / spec.fps as i64;
        let total_audio =
            audio_state.out_sample_idx + audio_state.fifo.len() as i64;
        let excess = (total_audio - target_audio_samples).max(0) as usize;
        if excess > 0 {
            eprintln!(
                "[encode] trimming {} trailing audio samples ({:.3}s) — \
                 audio ran past video end ({:.3}s)",
                excess,
                excess as f64 / AUDIO_RATE as f64,
                output_frame_idx as f64 / spec.fps as f64,
            );
            let new_len = audio_state.fifo.left.len().saturating_sub(excess);
            audio_state.fifo.left.truncate(new_len);
            audio_state.fifo.right.truncate(new_len);
        } else {
            eprintln!(
                "[encode] audio/video end aligned: video={:.3}s audio={:.3}s",
                output_frame_idx as f64 / spec.fps as f64,
                total_audio as f64 / AUDIO_RATE as f64,
            );
        }
    }

    // ── Flush audio FIFO then encoder ─────────────────────────────────────────
    // drain_fifo(flush=true) zero-pads the tail and sends the final partial frame.
    audio_state.drain_fifo(&mut octx, true)?;
    audio_state.flush_encoder(&mut octx)?;

    octx.write_trailer()
        .map_err(|e| format!("write trailer: {e}"))?;

    Ok(())
}

/// Encode one `ClipSpec` into `octx`, starting video output PTS at `out_frame_idx`.
///
/// Video and audio are multiplexed from the same demuxer packet loop so their
/// relative timing is preserved. Audio packets arriving before the clip's
/// `source_offset` are discarded; those after `clip_end` are still pushed into
/// the FIFO (they will be interleaved into subsequent clips). The video loop
/// break-out on `clip_end` is the authoritative end-of-clip trigger.
///
/// Returns the next unused `out_frame_idx`.
fn encode_clip(
    clip:               &ClipSpec,
    spec:               &EncodeSpec,
    octx:               &mut ffmpeg::format::context::Output,
    video_encoder:      &mut ffmpeg::encoder::video::Video,
    audio_state:        &mut AudioEncState,
    mut out_frame_idx:  i64,
    total_frames:       u64,
    frame_tb:           Rational,
    cancel:             &Arc<AtomicBool>,
    tx:                 &Sender<MediaResult>,
    last_video_dts:     &mut i64,
) -> Result<i64, String> {
    // ── Open input ────────────────────────────────────────────────────────────
    let mut ictx = open_input(&clip.path)
        .map_err(|e| format!("open '{}': {e}", clip.path.display()))?;

    let video_stream_idx = ictx
        .streams()
        .best(MediaType::Video)
        .ok_or_else(|| format!("no video stream in '{}'", clip.path.display()))?
        .index();

    // Audio stream is optional — clips with no audio (muted recordings, etc.)
    // produce silence in the output for their duration via FIFO carry-over.
    let audio_stream_idx: Option<usize> = ictx
        .streams()
        .best(MediaType::Audio)
        .map(|s| s.index());

    let in_video_tb = ictx.stream(video_stream_idx).unwrap().time_base();

    // ── Video decoder ─────────────────────────────────────────────────────────
    let vdec_ctx = codec::context::Context::from_parameters(
        ictx.stream(video_stream_idx).unwrap().parameters(),
    ).map_err(|e| format!("video decoder context: {e}"))?;

    let mut video_decoder = vdec_ctx.decoder().video()
        .map_err(|e| format!("open video decoder: {e}"))?;

    // ── Audio decoder (optional) ──────────────────────────────────────────────
    let mut audio_decoder: Option<ffmpeg::decoder::audio::Audio> = None;
    let mut in_audio_tb = Rational::new(1, AUDIO_RATE);

    // Only open the audio decoder when this clip contributes audio.
    // skip_audio is set by begin_render() for video-row clips whose audio has
    // been extracted to a paired A-row ClipSpec — opening the decoder here
    // would push duplicate PCM into the FIFO alongside the A-row clip's audio.
    if !clip.skip_audio {
        if let Some(asi) = audio_stream_idx {
            let ast = ictx.stream(asi).unwrap();
            in_audio_tb = ast.time_base();
            // Soft-fail: a corrupt/unsupported audio stream should not abort the
            // entire encode; video will still be processed correctly.
            match codec::context::Context::from_parameters(ast.parameters()) {
                Ok(ctx) => match ctx.decoder().audio() {
                    Ok(dec) => { audio_decoder = Some(dec); }
                    Err(e)  => { eprintln!("[encode] audio decoder open failed for '{}': {e}", clip.path.display()); }
                },
                Err(e) => { eprintln!("[encode] audio decoder params failed for '{}': {e}", clip.path.display()); }
            }
        }
    }

    // ── Display dimensions (visible pixels, no macroblock padding) ───────────
    // AVFrame.width/height (decoded.*) are the *coded* dimensions — H.264/H.265
    // pads the frame height to the next multiple of 16 for macroblock alignment
    // (e.g. 1920×1088 for a 1080p clip). AVCodecParameters.width/height are the
    // *display* dimensions (1920×1080). Feeding the coded height to sws_scale
    // causes it to include the black padding rows in the output, producing
    // visible letterboxing even when source and output share the same DAR.
    let (src_display_w, src_display_h) = {
        let stream = ictx.stream(video_stream_idx).unwrap();
        let params = stream.parameters();
        let w = params.width() as u32;
        let h = params.height() as u32;
        // Paranoia: fall back to decoder context dims if the container is missing them.
        if w > 0 && h > 0 { (w, h) } else { (video_decoder.width(), video_decoder.height()) }
    };

    // Skip the seek when source_offset is zero — the demuxer starts at the
    // beginning of the file automatically, and avformat_seek_file(max_ts=0)
    // returns EPERM on Windows when called on a freshly-opened context.
    // seek_to_secs handles the 0.0 guard and Windows soft-fail internally.
    // encode_clip treats seek failure as a hard error since a missed seek on
    // a trimmed clip would produce incorrect output frames.
    if clip.source_offset > 0.0 && !seek_to_secs(&mut ictx, clip.source_offset, "encode_clip") {
        return Err(format!("seek failed in '{}' at {:.3}s", clip.path.display(), clip.source_offset));
    }

    // ── Format converters (deferred until first frame of each type) ───────────
    let mut video_scaler:    Option<CropScaler>          = None;
    let mut audio_resampler: Option<resampling::Context> = None;

    let clip_end   = clip.source_offset + clip.duration;
    let ost_tb     = octx.stream(0).unwrap().time_base();
    let half_frame = 0.5 / spec.fps as f64;

    // ── Packet loop ───────────────────────────────────────────────────────────
    // packets() yields Result<(Stream, Packet), Error> — always destructure with ?.
    //
    // IMPORTANT: we do NOT break the demuxer loop the moment a decoded frame
    // hits clip_end.  H.264 B-frames are delivered in display (PTS) order, which
    // differs from decode (DTS) order.  When the first frame with pts >= clip_end
    // emerges from the decoder, its reorder buffer may still hold earlier-PTS
    // B-frames that are waiting for a later reference packet.  Breaking the
    // demuxer loop here deprives those frames of their reference data;
    // send_eof then forces them out incomplete and libavcodec drops them —
    // causing the last ~0.5-1 s of each clip to be missing, which manifests
    // as a freeze at clip boundaries and at the very end of the export.
    //
    // Fix: `video_clip_done` stops *encoding* output once we've seen a frame
    // at/past clip_end, but the demuxer loop continues so all in-flight
    // B-frames can receive their reference packets.  The decoder is flushed
    // properly by the send_eof block below.  Audio packets continue to drive
    // the FIFO normally regardless of video_clip_done.
    // Capture output frame index at start of this clip for fps-conversion mapping.
    let clip_start_frame_idx = out_frame_idx;

    let mut video_clip_done = false;
    // DTS monotonicity guard — passed in from run_encode so it persists across
    // clips and into the final encoder flush.
    // first_pts_logged is still local since it resets per clip.
    let mut first_pts_logged = false;

    for result in ictx.packets() {
        let (stream, packet) = result
            .map_err(|e| format!("read packet from '{}': {e}", clip.path.display()))?;

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let sidx = stream.index();

        // ── Video packet ──────────────────────────────────────────────────────
        if sidx == video_stream_idx {
            // ALWAYS feed the decoder every packet, even after video_clip_done.
            // B-frames near clip_end (PTS) may depend on I/P packets that arrive
            // after clip_end in DTS order.  Stopping send_packet early starves
            // the decoder of those reference packets and silently drops frames.
            video_decoder.send_packet(&packet)
                .map_err(|e| format!("send video packet to decoder: {e}"))?;

            if video_clip_done {
                // Drain decoder output to prevent EAGAIN on next send_packet.
                let mut _discard = VideoFrame::empty();
                while video_decoder.receive_frame(&mut _discard).is_ok() {}
                continue;
            }

            let mut decoded = VideoFrame::empty();
            while video_decoder.receive_frame(&mut decoded).is_ok() {
                let frame_pts_secs = decoded.pts()
                    .map(|pts| pts as f64 * f64::from(in_video_tb))
                    .unwrap_or(0.0);

                // Skip pre-roll frames before the clip's trim in-point.
                if frame_pts_secs < clip.source_offset - half_frame { continue; }

                // Log the first frame that passes the seek/pre-roll filter so we
                // can verify the seek landed accurately.
                if !first_pts_logged {
                    eprintln!(
                        "[encode] first decoded PTS after seek: {frame_pts_secs:.4}s \
                         (expected source_offset={:.4}s, file='{}')",
                        clip.source_offset, clip.path.display()
                    );
                    first_pts_logged = true;
                }

                // Past the out-point: stop encoding output and stop feeding
                // the decoder, but do NOT break the demuxer loop so audio
                // packets can still reach the FIFO.
                if frame_pts_secs >= clip_end {
                    video_clip_done = true;
                    continue;
                }

                // Initialise center-crop scaler on the first valid frame.
                // Uses display dimensions (not coded) to exclude macroblock padding.
                // When source AR matches output AR the crop rect is the full frame.
                let sc = video_scaler.get_or_insert_with(|| {
                    CropScaler::build(
                        decoded.format(), src_display_w, src_display_h,
                        spec.width, spec.height,
                    )
                });

                let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
                sc.run(&decoded, &mut yuv)?;

                // swscale inherits source SAR; override to 1:1.
                unsafe {
                    (*yuv.as_mut_ptr()).sample_aspect_ratio =
                        ffmpeg::ffi::AVRational { num: 1, den: 1 };
                }

                // ── Frame-rate conversion ─────────────────────────────────────
                // Map source timestamp to output frame slot.  When source fps <
                // output fps (e.g. 24→30) a source frame covers multiple output
                // slots; duplicate it to fill them.  When source fps > output fps,
                // the slot is already covered; skip.
                let src_rel_secs = (frame_pts_secs - clip.source_offset).max(0.0);
                let target_out_pts = clip_start_frame_idx
                    + (src_rel_secs * spec.fps as f64).round() as i64;

                if target_out_pts >= out_frame_idx {
                    loop {
                        yuv.set_pts(Some(out_frame_idx));

                        video_encoder.send_frame(&yuv)
                            .map_err(|e| format!("send video frame to encoder: {e}"))?;

                        let mut pkt = Packet::empty();
                        while video_encoder.receive_packet(&mut pkt).is_ok() {
                            pkt.set_stream(0);
                            pkt.rescale_ts(frame_tb, ost_tb);
                            let raw_dts = pkt.dts().unwrap_or(0);
                            let raw_pts = pkt.pts().unwrap_or(raw_dts);
                            let dts_s   = raw_dts as f64 * f64::from(ost_tb);
                            let pts_s   = raw_pts as f64 * f64::from(ost_tb);
                            eprintln!("[encode] video pkt dts={dts_s:.4}s pts={pts_s:.4}s");
                            if *last_video_dts != i64::MIN {
                                let prev_s = *last_video_dts as f64 * f64::from(ost_tb);
                                if dts_s < prev_s {
                                    let clamped = *last_video_dts + 1;
                                    eprintln!(
                                        "[encode] non-monotonic DTS ({prev_s:.4}s → {dts_s:.4}s); \
                                         clamping {raw_dts} → {clamped}"
                                    );
                                    unsafe { (*pkt.as_mut_ptr()).dts = clamped; }
                                }
                            }
                            *last_video_dts = pkt.dts().unwrap_or(raw_dts);
                            pkt.write_interleaved(octx)
                                .map_err(|e| format!("write video packet: {e}"))?;
                        }

                        out_frame_idx += 1;

                        if out_frame_idx as u64 % PROGRESS_INTERVAL == 0 {
                            let _ = tx.send(MediaResult::EncodeProgress {
                                job_id:       spec.job_id,
                                frame:        out_frame_idx as u64,
                                total_frames,
                            });
                        }

                        if out_frame_idx > target_out_pts { break; }
                    }
                }
            }
        }

        // ── Audio packet ──────────────────────────────────────────────────────
        else if Some(sidx) == audio_stream_idx {
            if let Some(ref mut adec) = audio_decoder {
                // Soft-fail: a bad audio packet should not abort the encode.
                if adec.send_packet(&packet).is_err() { continue; }

                let mut raw = AudioFrame::empty();
                while adec.receive_frame(&mut raw).is_ok() {
                    let pts_secs = raw.pts()
                        .map(|pts| pts as f64 * f64::from(in_audio_tb))
                        .unwrap_or(0.0);

                    // Discard pre-roll audio (before the clip's in-point).
                    // Use a slightly generous window (-0.05 s) to avoid silencing
                    // audio frames that span the exact trim boundary.
                    if pts_secs < clip.source_offset - 0.05 { continue; }

                    // Hard-cut audio at clip_end.
                    //
                    // drain_fifo() is called eagerly on every decoded audio frame,
                    // so any audio past clip_end gets encoded and written to the
                    // output file immediately — before the post-encode trim block
                    // in run_encode even runs.  For a source file that extends 2 s
                    // past clip_end (~88 k samples), drain_fifo would write ~86
                    // full AAC frames that can never be un-written; the trim block
                    // can only remove the ≤1023 leftover FIFO samples.
                    //
                    // A partial encoder frame (≤1023 samples) naturally stays in
                    // the FIFO and carries over into the next clip — that is the
                    // only carry-over needed for a seamless audio timeline.
                    if pts_secs >= clip_end { continue; }

                    // Resample to FLTP stereo 44100 if the source differs in any way.
                    // The resampler is created lazily on the first audio frame so we
                    // know the real input format before building the SwrContext.
                    let target_fmt = Sample::F32(SampleType::Planar);
                    let raw_channels = raw.ch_layout().channels();
                    let needs_resample = raw.format()  != target_fmt
                        || raw.rate()                  != AUDIO_RATE as u32
                        || raw_channels                != 2;

                    if needs_resample {
                        let rs = audio_resampler.get_or_insert_with(|| {
                            // Mono sources must be declared as MONO or swr will
                            // misinterpret the channel layout.
                            let src_layout = if raw.ch_layout().channels() >= 2 {
                                raw.ch_layout()
                            } else {
                                ChannelLayout::MONO
                            };
                            resampling::Context::get2(
                                raw.format(), src_layout,            raw.rate(),
                                target_fmt,   ChannelLayout::STEREO, AUDIO_RATE as u32,
                            ).expect("create audio resampler")
                        });

                        let mut resampled = AudioFrame::empty();
                        if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                            audio_state.fifo.push_scaled(&resampled, clip.volume as f32);
                        }
                    } else {
                        audio_state.fifo.push_scaled(&raw, clip.volume as f32);
                    }

                    // Drain full frames from the FIFO immediately so we don't
                    // accumulate an unbounded buffer across long clips.
                    audio_state.drain_fifo(octx, false)?;
                }
            }
        }
    }

    // ── Drain video decoder at clip end ───────────────────────────────────────
    // After the demuxer loop the decoder may still hold B-frames in its reorder
    // buffer (frames whose reference packets arrived but which haven't been
    // output yet).  send_eof flushes them.  Because the demuxer loop above
    // continued past clip_end (via the video_clip_done flag) all reference
    // packets were delivered, so these flushed frames are complete.
    // Frames with pts >= clip_end are discarded here — they belong to a future
    // GOP and must not pollute the output timeline.
    video_decoder.send_eof()
        .map_err(|e| format!("send EOF to video decoder '{}': {e}", clip.path.display()))?;
    let mut decoded = VideoFrame::empty();
    while video_decoder.receive_frame(&mut decoded).is_ok() {
        let pts_secs = decoded.pts()
            .map(|pts| pts as f64 * f64::from(in_video_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end { break; }

        if let Some(sc) = &mut video_scaler {
            let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            if sc.run(&decoded, &mut yuv).is_ok() {
                unsafe {
                    (*yuv.as_mut_ptr()).sample_aspect_ratio =
                        ffmpeg::ffi::AVRational { num: 1, den: 1 };
                }
                // Same fps-conversion logic as the main packet loop.
                let src_rel_secs = (pts_secs - clip.source_offset).max(0.0);
                let target_out_pts = clip_start_frame_idx
                    + (src_rel_secs * spec.fps as f64).round() as i64;
                if target_out_pts >= out_frame_idx {
                    loop {
                        yuv.set_pts(Some(out_frame_idx));
                        video_encoder.send_frame(&yuv)
                            .map_err(|e| format!("send decoder-flush frame to encoder: {e}"))?;
                        let mut pkt = Packet::empty();
                        while video_encoder.receive_packet(&mut pkt).is_ok() {
                            pkt.set_stream(0);
                            pkt.rescale_ts(frame_tb, ost_tb);
                            let raw_dts = pkt.dts().unwrap_or(0);
                            let raw_pts = pkt.pts().unwrap_or(raw_dts);
                            let dts_s   = raw_dts as f64 * f64::from(ost_tb);
                            let pts_s   = raw_pts as f64 * f64::from(ost_tb);
                            eprintln!("[encode] decoder-flush pkt dts={dts_s:.4}s pts={pts_s:.4}s");
                            if *last_video_dts != i64::MIN {
                                let prev_s = *last_video_dts as f64 * f64::from(ost_tb);
                                if dts_s < prev_s {
                                    let clamped = *last_video_dts + 1;
                                    eprintln!(
                                        "[encode] decoder-flush non-monotonic DTS \
                                         ({prev_s:.4}s → {dts_s:.4}s); clamping {raw_dts} → {clamped}"
                                    );
                                    unsafe { (*pkt.as_mut_ptr()).dts = clamped; }
                                }
                            }
                            *last_video_dts = pkt.dts().unwrap_or(raw_dts);
                            pkt.write_interleaved(octx)
                                .map_err(|e| format!("decoder-flush write video packet: {e}"))?;
                        }
                        out_frame_idx += 1;
                        if out_frame_idx > target_out_pts { break; }
                    }
                }
            }
        }
    }

    // ── Drain audio decoder at clip end ───────────────────────────────────────
    if let Some(ref mut adec) = audio_decoder {
        let _ = adec.send_eof();
        let mut raw = AudioFrame::empty();
        while adec.receive_frame(&mut raw).is_ok() {
            // Same clip_end ceiling as the packet loop — decoder-buffered frames
            // can also extend past clip_end if the source file continues.
            let pts_secs = raw.pts()
                .map(|pts| pts as f64 * f64::from(in_audio_tb))
                .unwrap_or(0.0);
            if pts_secs >= clip_end { break; }

            // Same resample path as the packet loop above.
            let target_fmt = Sample::F32(SampleType::Planar);
            let raw_channels = raw.ch_layout().channels();
            let needs_resample = raw.format()  != target_fmt
                || raw.rate()                  != AUDIO_RATE as u32
                || raw_channels                != 2;

            if needs_resample {
                if let Some(rs) = &mut audio_resampler {
                    let mut resampled = AudioFrame::empty();
                    if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                        audio_state.fifo.push_scaled(&resampled, clip.volume as f32);
                    }
                }
            } else {
                audio_state.fifo.push_scaled(&raw, clip.volume as f32);
            }
        }
        audio_state.drain_fifo(octx, false)?;
    }

    Ok(out_frame_idx)
}
// ── Crossfade helpers ─────────────────────────────────────────────────────────

/// Decode all video frames from `clip` into packed YUV420P byte vectors.
///
/// Each entry in the returned Vec is one frame's worth of YUV420P data laid out
/// as: [Y plane (w×h)] ++ [U plane (w/2 × h/2)] ++ [V plane (w/2 × h/2)].
/// Strides are removed — each row is packed tightly to exactly `w` (or `w/2`) bytes.
///
/// Used by apply_crossfade to collect the tail frames of clip A and head frames
/// of clip B before blending them.
fn decode_clip_frames(
    clip: &ClipSpec,
    spec: &EncodeSpec,
) -> Result<Vec<Vec<u8>>, String> {
    let mut ictx = open_input(&clip.path)
        .map_err(|e| format!("crossfade open '{}': {e}", clip.path.display()))?;

    let video_stream_idx = ictx.streams().best(MediaType::Video)
        .ok_or_else(|| format!("no video stream in '{}' for crossfade", clip.path.display()))?
        .index();

    let in_video_tb = ictx.stream(video_stream_idx).unwrap().time_base();

    let (src_display_w, src_display_h) = {
        let stream = ictx.stream(video_stream_idx).unwrap();
        let params = stream.parameters();
        let w = params.width() as u32;
        let h = params.height() as u32;
        (w, h)
    };

    let vdec_ctx = codec::context::Context::from_parameters(
        ictx.stream(video_stream_idx).unwrap().parameters(),
    ).map_err(|e| format!("crossfade video decoder context: {e}"))?;

    let mut video_decoder = vdec_ctx.decoder().video()
        .map_err(|e| format!("crossfade open video decoder: {e}"))?;

    // decode_clip_frames treats seek failure as soft — PTS filtering below
    // will skip pre-roll frames if the seek didn't land accurately.
    seek_to_secs(&mut ictx, clip.source_offset, "decode_clip_frames");

    let mut video_scaler: Option<CropScaler> = None;
    let clip_end   = clip.source_offset + clip.duration;
    let half_frame = 0.5 / spec.fps as f64;
    let w = spec.width  as usize;
    let h = spec.height as usize;
    let uv_w = w / 2;
    let uv_h = h / 2;

    let mut frames: Vec<Vec<u8>> = Vec::new();

    'packet_loop: for result in ictx.packets() {
        let (stream, packet) = result
            .map_err(|e| format!("crossfade read packet: {e}"))?;

        if stream.index() != video_stream_idx { continue; }

        video_decoder.send_packet(&packet)
            .map_err(|e| format!("crossfade send packet: {e}"))?;

        let mut decoded = VideoFrame::empty();
        while video_decoder.receive_frame(&mut decoded).is_ok() {
            let pts_secs = decoded.pts()
                .map(|pts| pts as f64 * f64::from(in_video_tb))
                .unwrap_or(0.0);

            if pts_secs < clip.source_offset - half_frame { continue; }
            if pts_secs >= clip_end { break 'packet_loop; }

            let sc = video_scaler.get_or_insert_with(|| {
                CropScaler::build(
                    decoded.format(), src_display_w, src_display_h,
                    spec.width, spec.height,
                )
            });

            let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            sc.run(&decoded, &mut yuv)
                .map_err(|e| format!("crossfade scale: {e}"))?;

            frames.push(extract_yuv(&yuv, w, h, uv_w, uv_h));
        }
    }

    // Flush decoder tail
    let _ = video_decoder.send_eof();
    let mut decoded = VideoFrame::empty();
    while video_decoder.receive_frame(&mut decoded).is_ok() {
        let pts_secs = decoded.pts()
            .map(|pts| pts as f64 * f64::from(in_video_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end { break; }

        if let Some(sc) = &mut video_scaler {
            let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            if sc.run(&decoded, &mut yuv).is_ok() {
                frames.push(extract_yuv(&yuv, w, h, uv_w, uv_h));
            }
        }
    }

    Ok(frames)
}

/// Encode the crossfade blend between `tail_spec` (end of clip A) and
/// `head_spec` (start of clip B).
///
/// Steps:
///   1. Decode all frames from both specs into packed YUV420P.
///   2. For each frame pair blend with alpha going from 0 (exclusive) → 1 (exclusive).
///   3. Encode each blended frame into octx, advancing `out_frame_idx`.
///   4. Drain the audio FIFO after each video frame (audio carries over naturally
///      from clip A's tail that was pushed during encode_clip).
///
/// Returns the next unused `out_frame_idx`.
fn apply_crossfade(
    tail_spec:     &ClipSpec,
    head_spec:     &ClipSpec,
    spec:          &EncodeSpec,
    octx:          &mut ffmpeg::format::context::Output,
    video_encoder: &mut ffmpeg::encoder::video::Video,
    audio_state:   &mut AudioEncState,
    mut out_frame_idx: i64,
    total_frames:  u64,
    frame_tb:      Rational,
    cancel:        &Arc<AtomicBool>,
    tx:            &Sender<MediaResult>,
) -> Result<i64, String> {
    let tail_frames = decode_clip_frames(tail_spec, spec)?;
    let head_frames = decode_clip_frames(head_spec, spec)?;

    let n = tail_frames.len().min(head_frames.len());
    if n == 0 {
        return Ok(out_frame_idx);
    }

    let w = spec.width  as usize;
    let h = spec.height as usize;
    let uv_w = w / 2;
    let uv_h = h / 2;
    let ost_tb = octx.stream(0).unwrap().time_base();

    for i in 0..n {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        // alpha goes from just above 0 to just below 1 — no pure A or pure B frame
        // (those are handled by encode_clip on each side).
        let alpha = (i + 1) as f32 / (n + 1) as f32;
        let blended = blend_yuv_frame(&tail_frames[i], &head_frames[i], alpha);

        // Build a VideoFrame from the packed blended data.
        let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
        yuv.set_pts(Some(out_frame_idx));
        unsafe {
            (*yuv.as_mut_ptr()).sample_aspect_ratio =
                ffmpeg::ffi::AVRational { num: 1, den: 1 };
        }

        write_yuv(&blended, &mut yuv, w, h, uv_w, uv_h);

        video_encoder.send_frame(&yuv)
            .map_err(|e| format!("crossfade encode frame: {e}"))?;

        let mut pkt = Packet::empty();
        while video_encoder.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(0);
            pkt.rescale_ts(frame_tb, ost_tb);
            pkt.write_interleaved(octx)
                .map_err(|e| format!("crossfade write packet: {e}"))?;
        }

        // Drain whatever audio the FIFO has from clip A's tail.
        audio_state.drain_fifo(octx, false)?;

        out_frame_idx += 1;

        if out_frame_idx as u64 % PROGRESS_INTERVAL == 0 {
            let _ = tx.send(MediaResult::EncodeProgress {
                job_id:       spec.job_id,
                frame:        out_frame_idx as u64,
                total_frames,
            });
        }
    }

    Ok(out_frame_idx)
}