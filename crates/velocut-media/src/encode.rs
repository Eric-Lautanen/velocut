// crates/velocut-media/src/encode.rs
//
// Multi-clip H.264/MP4 encode pipeline.
//
// Design:
//   • `ClipSpec`   — everything needed to locate and trim one source clip.
//   • `EncodeSpec` — the complete job description handed from the UI.
//   • `encode_timeline()` — blocking function meant to run on its own thread;
//     called from MediaWorker::start_encode. Sends EncodeProgress every
//     PROGRESS_INTERVAL frames and EncodeError / EncodeDone on exit.
//
// PTS strategy:
//   Input PTS is used only to decide which frames to keep. Output PTS is
//   re-assigned from a monotonically increasing counter (output_frame_idx)
//   in the output timebase (1/fps). This sidesteps every discontinuity
//   introduced by source file trimming and multi-clip concatenation.
//
// Cancellation:
//   `cancel` is an Arc<AtomicBool>. The loop checks it after each frame.
//   When set the encode is flushed and EncodeError { msg: "cancelled" } is
//   sent — the UI treats that as an "aborted" state distinct from a real error.
//
// Encoder ownership:
//   The opened `video::Encoder` is created once in `run_encode` and passed as
//   `&mut` to `encode_clip` and the flush block. We never retrieve it from the
//   output stream — `Stream::codec()` does not exist in this version of
//   ffmpeg-the-third.

use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use crossbeam_channel::Sender;
use uuid::Uuid;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::codec::{self, Id as CodecId};
use ffmpeg::encoder;
use ffmpeg::format::{Pixel, input as open_input, output as open_output};
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::scaling::{Context as ScaleCtx, Flags as ScaleFlags};
use ffmpeg::util::frame::video::Video as FfmpegFrame;
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

/// Send a progress update every this many encoded frames.
const PROGRESS_INTERVAL: u64 = 15;

// ── Public entry point ────────────────────────────────────────────────────────

/// Encode `spec` to disk. Blocking — run this on a dedicated thread.
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

    // ── Encoder setup ─────────────────────────────────────────────────────────
    // Create the codec context independently of the output stream — Stream does
    // not expose a .codec() accessor in this version of ffmpeg-the-third.
    let out_tb   = Rational::new(1, spec.fps as i32);
    let frame_tb = Rational::new(1, spec.fps as i32);

    let h264 = encoder::find(CodecId::H264)
        .ok_or_else(|| "H.264 encoder not found — is libx264 available?".to_string())?;

    // Add the output stream so the muxer knows the codec type.
    let mut ost = octx.add_stream(h264)
        .map_err(|e| format!("add output stream: {e}"))?;
    ost.set_time_base(out_tb);

    // Build the video encoder context from the codec.
    let enc_ctx = codec::context::Context::new_with_codec(h264);
    let mut video_enc = enc_ctx
        .encoder()
        .video()
        .map_err(|e| format!("create video encoder context: {e}"))?;

    video_enc.set_width(spec.width);
    video_enc.set_height(spec.height);
    video_enc.set_format(Pixel::YUV420P);
    video_enc.set_time_base(out_tb);
    video_enc.set_frame_rate(Some(Rational::new(spec.fps as i32, 1)));
    video_enc.set_bit_rate(0); // CRF controls quality; bit_rate 0 signals VBR

    // Open the encoder. x264 options (CRF + preset) use the safe Dictionary API.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("crf",    "18");
    opts.set("preset", "fast");

    let mut encoder = video_enc
        .open_as_with(h264, opts)
        .map_err(|e| format!("open H.264 encoder: {e}"))?;

    // Copy encoder parameters into the stream's codecpar so the muxer has
    // resolution, format, and codec-private data for the container header.
    // set_parameters() requires AsPtr<AVCodecParameters>; encoder::Video does
    // not implement that trait, so we call the FFI function directly.
    unsafe {
        let ret = ffmpeg::ffi::avcodec_parameters_from_context(
            (**(*octx.as_mut_ptr()).streams.add(0)).codecpar,
            encoder.as_ptr() as *mut ffmpeg::ffi::AVCodecContext,
        );
        if ret < 0 {
            return Err(format!("avcodec_parameters_from_context failed: {ret}"));
        }
    }

    ffmpeg::format::context::output::dump(&octx, 0, Some(&spec.output.to_string_lossy()));
    octx.write_header()
        .map_err(|e| format!("write output header: {e}"))?;

    // ── Per-clip encode loop ──────────────────────────────────────────────────
    // `encoder` is passed mutably so encode_clip can send/receive packets.
    // `octx` is also passed mutably so encode_clip can call write_interleaved.
    let mut output_frame_idx: i64 = 0;

    for clip in &spec.clips {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        output_frame_idx = encode_clip(
            clip,
            spec,
            &mut octx,
            &mut encoder,
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

    // ── Flush encoder ─────────────────────────────────────────────────────────
    encoder.send_eof()
        .map_err(|e| format!("send EOF to encoder: {e}"))?;

    let ost_tb = octx.stream(0).unwrap().time_base();
    let mut pkt = Packet::empty();
    while encoder.receive_packet(&mut pkt).is_ok() {
        pkt.set_stream(0);
        pkt.rescale_ts(frame_tb, ost_tb);
        pkt.write_interleaved(&mut octx)
            .map_err(|e| format!("write flush packet: {e}"))?;
    }

    octx.write_trailer()
        .map_err(|e| format!("write trailer: {e}"))?;

    Ok(())
}

/// Encode one `ClipSpec` into `octx`, starting output PTS at `output_frame_idx`.
/// Returns the next unused `output_frame_idx`.
fn encode_clip(
    clip:              &ClipSpec,
    spec:              &EncodeSpec,
    octx:              &mut ffmpeg::format::context::Output,
    encoder:           &mut ffmpeg::encoder::video::Video,
    mut out_frame_idx: i64,
    total_frames:      u64,
    frame_tb:          Rational,
    cancel:            &Arc<AtomicBool>,
    tx:                &Sender<MediaResult>,
) -> Result<i64, String> {
    // ── Open input ────────────────────────────────────────────────────────────
    let mut ictx = open_input(&clip.path)
        .map_err(|e| format!("open '{}': {e}", clip.path.display()))?;

    let in_stream_idx = ictx
        .streams()
        .best(MediaType::Video)
        .ok_or_else(|| format!("no video stream in '{}'", clip.path.display()))?
        .index();

    let in_tb = ictx.stream(in_stream_idx).unwrap().time_base();

    // ── Open decoder ─────────────────────────────────────────────────────────
    // Build decoder context from the input stream's parameters. Stream does not
    // expose .codec() directly; use Context::from_parameters instead.
    let dec_ctx = codec::context::Context::from_parameters(
        ictx.stream(in_stream_idx).unwrap().parameters(),
    )
    .map_err(|e| format!("decoder context from params: {e}"))?;

    let mut decoder = dec_ctx
        .decoder()
        .video()
        .map_err(|e| format!("open decoder: {e}"))?;

    // ── Seek to source_offset ─────────────────────────────────────────────────
    // Skip the seek when source_offset is zero — the demuxer starts at the
    // beginning of the file automatically, and avformat_seek_file(max_ts=0)
    // returns EPERM on Windows when called on a freshly-opened context.
    // For non-zero offsets use an open-ended forward range (seek_ts..) so
    // FFmpeg can land on the nearest keyframe at or after the target; a
    // closed range ..seek_ts (max=seek_ts) also triggers EPERM when the
    // keyframe before the target is at a PTS lower than seek_ts.
    if clip.source_offset > 0.0 {
        let seek_ts = (clip.source_offset * ffmpeg::ffi::AV_TIME_BASE as f64) as i64;
        ictx.seek(seek_ts, seek_ts..)
            .map_err(|e| format!("seek in '{}': {e}", clip.path.display()))?;
    }

    // ── Pixel format converter (deferred until first decoded frame) ───────────
    let mut scaler: Option<ScaleCtx> = None;

    let clip_end   = clip.source_offset + clip.duration;
    let ost_tb     = octx.stream(0).unwrap().time_base();
    let half_frame = 0.5 / spec.fps as f64;

    // ── Decode / scale / encode loop ──────────────────────────────────────────
    // packets() yields Result<(Stream, Packet), Error> — destructure with ?.
    'packet_loop: for result in ictx.packets() {
        let (stream, packet) = result
            .map_err(|e| format!("read packet from '{}': {e}", clip.path.display()))?;

        if stream.index() != in_stream_idx {
            continue;
        }

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        decoder.send_packet(&packet)
            .map_err(|e| format!("send packet to decoder: {e}"))?;

        let mut decoded = FfmpegFrame::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            let frame_pts_secs = decoded
                .pts()
                .map(|pts| pts as f64 * f64::from(in_tb))
                .unwrap_or(0.0);

            // Skip pre-roll frames before the clip's trim point.
            if frame_pts_secs < clip.source_offset - half_frame {
                continue;
            }

            // Stop once we've passed the clip's end boundary.
            if frame_pts_secs >= clip_end {
                break 'packet_loop;
            }

            // Initialise scaler on the first valid frame so we know
            // the actual input format and dimensions.
            let sc = scaler.get_or_insert_with(|| {
                ScaleCtx::get(
                    decoded.format(), decoded.width(), decoded.height(),
                    Pixel::YUV420P,   spec.width,       spec.height,
                    ScaleFlags::BILINEAR,
                )
                .expect("create swscale context")
            });

            // Scale into a fresh YUV420P output frame.
            let mut yuv = FfmpegFrame::empty();
            sc.run(&decoded, &mut yuv)
                .map_err(|e| format!("scale frame: {e}"))?;

            // Assign monotonic PTS for a continuous output timeline.
            yuv.set_pts(Some(out_frame_idx));
            yuv.set_kind(decoded.kind());

            // Encode and write any immediately available output packets.
            encoder.send_frame(&yuv)
                .map_err(|e| format!("send frame to encoder: {e}"))?;

            let mut pkt = Packet::empty();
            while encoder.receive_packet(&mut pkt).is_ok() {
                pkt.set_stream(0);
                pkt.rescale_ts(frame_tb, ost_tb);
                pkt.write_interleaved(octx)
                    .map_err(|e| format!("write packet: {e}"))?;
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

    // ── Drain decoder at clip end ─────────────────────────────────────────────
    decoder.send_eof()
        .map_err(|e| format!("decoder EOF: {e}"))?;

    let mut decoded = FfmpegFrame::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        let frame_pts_secs = decoded
            .pts()
            .map(|pts| pts as f64 * f64::from(in_tb))
            .unwrap_or(0.0);

        if frame_pts_secs >= clip_end {
            break;
        }

        if let Some(sc) = &mut scaler {
            let mut yuv = FfmpegFrame::empty();
            sc.run(&decoded, &mut yuv)
                .map_err(|e| format!("scale drain frame: {e}"))?;
            yuv.set_pts(Some(out_frame_idx));

            encoder.send_frame(&yuv)
                .map_err(|e| format!("send drain frame: {e}"))?;

            let mut pkt = Packet::empty();
            while encoder.receive_packet(&mut pkt).is_ok() {
                pkt.set_stream(0);
                pkt.rescale_ts(frame_tb, ost_tb);
                pkt.write_interleaved(octx)
                    .map_err(|e| format!("write drain packet: {e}"))?;
            }
            out_frame_idx += 1;
        }
    }

    Ok(out_frame_idx)
}