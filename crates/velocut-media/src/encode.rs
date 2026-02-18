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
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Send a progress update every this many encoded video frames.
const PROGRESS_INTERVAL: u64 = 15;

/// Output audio sample rate for all exports.
const AUDIO_RATE: i32 = 44100;

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
        let n = frame.samples();
        if n == 0 { return; }
        unsafe {
            let l_bytes = frame.data(0);
            let l_f32 = std::slice::from_raw_parts(l_bytes.as_ptr() as *const f32, n);
            self.left.extend_from_slice(l_f32);

            // For stereo frames use plane 1; mono → duplicate plane 0.
            let r_bytes = if frame.ch_layout().channels() >= 2 { frame.data(1) } else { frame.data(0) };
            let r_f32 = std::slice::from_raw_parts(r_bytes.as_ptr() as *const f32, n);
            self.right.extend_from_slice(r_f32);
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

    let mut opts = ffmpeg::Dictionary::new();
    opts.set("crf",    "18");
    opts.set("preset", "fast");

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

    let audio_encoder = audio_enc.open_as_with(aac, ffmpeg::Dictionary::new())
        .map_err(|e| format!("open AAC encoder: {e}"))?;

    // Guard against a codec that returns 0 (shouldn't happen with AAC but be safe).
    let audio_frame_size = (audio_encoder.frame_size() as usize).max(1024);

    // Retrieve the muxer-assigned timebase for stream 1 before writing the header.
    let ost_audio_tb = octx.stream(1).unwrap().time_base();

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

    for clip in &spec.clips {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        output_frame_idx = encode_clip(
            clip,
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
        pkt.rescale_ts(frame_tb, ost_video_tb);
        pkt.write_interleaved(&mut octx)
            .map_err(|e| format!("write flush video packet: {e}"))?;
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
    clip:              &ClipSpec,
    spec:              &EncodeSpec,
    octx:              &mut ffmpeg::format::context::Output,
    video_encoder:     &mut ffmpeg::encoder::video::Video,
    audio_state:       &mut AudioEncState,
    mut out_frame_idx: i64,
    total_frames:      u64,
    frame_tb:          Rational,
    cancel:            &Arc<AtomicBool>,
    tx:                &Sender<MediaResult>,
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

    // ── Seek to source_offset ─────────────────────────────────────────────────
    // Skip the seek when source_offset is zero — the demuxer starts at the
    // beginning of the file automatically, and avformat_seek_file(max_ts=0)
    // returns EPERM on Windows when called on a freshly-opened context.
    if clip.source_offset > 0.0 {
        let seek_ts = (clip.source_offset * ffmpeg::ffi::AV_TIME_BASE as f64) as i64;
        ictx.seek(seek_ts, seek_ts..)
            .map_err(|e| format!("seek in '{}': {e}", clip.path.display()))?;
    }

    // ── Format converters (deferred until first frame of each type) ───────────
    let mut video_scaler:    Option<ScaleCtx>            = None;
    let mut audio_resampler: Option<resampling::Context> = None;

    let clip_end   = clip.source_offset + clip.duration;
    let ost_tb     = octx.stream(0).unwrap().time_base();
    let half_frame = 0.5 / spec.fps as f64;

    // ── Packet loop ───────────────────────────────────────────────────────────
    // packets() yields Result<(Stream, Packet), Error> — always destructure with ?.
    'packet_loop: for result in ictx.packets() {
        let (stream, packet) = result
            .map_err(|e| format!("read packet from '{}': {e}", clip.path.display()))?;

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let sidx = stream.index();

        // ── Video packet ──────────────────────────────────────────────────────
        if sidx == video_stream_idx {
            video_decoder.send_packet(&packet)
                .map_err(|e| format!("send video packet to decoder: {e}"))?;

            let mut decoded = VideoFrame::empty();
            while video_decoder.receive_frame(&mut decoded).is_ok() {
                let frame_pts_secs = decoded.pts()
                    .map(|pts| pts as f64 * f64::from(in_video_tb))
                    .unwrap_or(0.0);

                // Skip pre-roll frames before the clip's trim in-point.
                if frame_pts_secs < clip.source_offset - half_frame { continue; }

                // Stop once we've passed the clip's out-point.
                if frame_pts_secs >= clip_end { break 'packet_loop; }

                // Initialise scaler on the first valid frame so we know the
                // actual input format. Use the pre-computed display dimensions
                // (not decoded.width/height) to exclude macroblock padding rows.
                let sc = video_scaler.get_or_insert_with(|| {
                    ScaleCtx::get(
                        decoded.format(), src_display_w, src_display_h,
                        Pixel::YUV420P,   spec.width,    spec.height,
                        ScaleFlags::BILINEAR,
                    ).expect("create swscale context")
                });

                let mut yuv = VideoFrame::empty();
                sc.run(&decoded, &mut yuv)
                    .map_err(|e| format!("scale video frame: {e}"))?;

                yuv.set_pts(Some(out_frame_idx));
                yuv.set_kind(decoded.kind());
                // swscale inherits the source SAR onto the output frame; override
                // to 1:1 so players don't letterbox. No safe setter exists in
                // ffmpeg-the-third 4 — write the AVFrame field directly.
                unsafe {
                    (*yuv.as_mut_ptr()).sample_aspect_ratio =
                        ffmpeg::ffi::AVRational { num: 1, den: 1 };
                }

                video_encoder.send_frame(&yuv)
                    .map_err(|e| format!("send video frame to encoder: {e}"))?;

                let mut pkt = Packet::empty();
                while video_encoder.receive_packet(&mut pkt).is_ok() {
                    pkt.set_stream(0);
                    pkt.rescale_ts(frame_tb, ost_tb);
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

                    // Audio is NOT cut at clip_end here — the video loop controls
                    // the break. Letting audio slightly over-run into the FIFO is
                    // intentional: the carry-over is consumed at the start of the
                    // next clip (or flushed at the very end), maintaining a
                    // continuous, gap-free audio timeline.

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
                            audio_state.fifo.push(&resampled);
                        }
                    } else {
                        audio_state.fifo.push(&raw);
                    }

                    // Drain full frames from the FIFO immediately so we don't
                    // accumulate an unbounded buffer across long clips.
                    audio_state.drain_fifo(octx, false)?;
                }
            }
        }
    }

    // ── Drain video decoder at clip end ───────────────────────────────────────
    // Some codecs (e.g. H.264 with B-frames) hold frames internally; flush them.
    let _ = video_decoder.send_eof();
    let mut decoded = VideoFrame::empty();
    while video_decoder.receive_frame(&mut decoded).is_ok() {
        let pts_secs = decoded.pts()
            .map(|pts| pts as f64 * f64::from(in_video_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end { break; }

        if let Some(sc) = &mut video_scaler {
            let mut yuv = VideoFrame::empty();
            if sc.run(&decoded, &mut yuv).is_ok() {
                yuv.set_pts(Some(out_frame_idx));
                unsafe {
                    (*yuv.as_mut_ptr()).sample_aspect_ratio =
                        ffmpeg::ffi::AVRational { num: 1, den: 1 };
                }
                if video_encoder.send_frame(&yuv).is_ok() {
                    let mut pkt = Packet::empty();
                    while video_encoder.receive_packet(&mut pkt).is_ok() {
                        pkt.set_stream(0);
                        pkt.rescale_ts(frame_tb, ost_tb);
                        let _ = pkt.write_interleaved(octx);
                    }
                    out_frame_idx += 1;
                }
            }
        }
    }

    // ── Drain audio decoder at clip end ───────────────────────────────────────
    if let Some(ref mut adec) = audio_decoder {
        let _ = adec.send_eof();
        let mut raw = AudioFrame::empty();
        while adec.receive_frame(&mut raw).is_ok() {
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
                        audio_state.fifo.push(&resampled);
                    }
                }
            } else {
                audio_state.fifo.push(&raw);
            }
        }
        audio_state.drain_fifo(octx, false)?;
    }

    Ok(out_frame_idx)
}