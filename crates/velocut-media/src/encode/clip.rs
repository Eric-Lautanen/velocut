// crates/velocut-media/src/encode/clip.rs
//
// Per-clip encoding, scaling, transition application, and frame helpers.
// Extracted from encode/mod.rs.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crossbeam_channel::Sender;

use ffmpeg::codec;
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::format::{input as open_input, Pixel};
use ffmpeg::media::Type as MediaType;
use ffmpeg::packet::Mut as _;
use ffmpeg::software::resampling;
use ffmpeg::software::scaling::{Context as ScaleCtx, Flags as ScaleFlags};
use ffmpeg::util::channel_layout::{ChannelLayout, ChannelLayoutMask};
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg::util::frame::video::Video as VideoFrame;
use ffmpeg::util::rational::Rational;
use ffmpeg::Packet;
use ffmpeg_the_third as ffmpeg;

use crate::helpers::seek::seek_to_secs;
use crate::helpers::yuv::{extract_yuv, write_yuv};
use velocut_core::filters::helpers::apply_filter_yuv;
use velocut_core::filters::FilterParams;
use velocut_core::media_types::MediaResult;
use velocut_core::transitions::VideoTransition;

use super::audio::{cut_gate, fade_gain, flush_audio_resampler, AudioEncState};
use super::hw::{upload_frame_to_hw, HwBackend};
use super::{ClipSpec, EncodeSpec, AUDIO_RATE, PROGRESS_INTERVAL};

// ── Center-crop scaler ────────────────────────────────────────────────────────

pub(super) struct CropScaler {
    ctx: ScaleCtx,
    crop_x: u32,
    crop_y: u32,
    crop_h: u32,
}

impl CropScaler {
    pub(super) fn build(src_fmt: Pixel, src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> Self {
        let src_ar = src_w as f64 / src_h.max(1) as f64;
        let out_ar = out_w as f64 / out_h.max(1) as f64;

        let (crop_x, crop_y, crop_w, crop_h) = if (src_ar - out_ar).abs() < 1e-4 {
            (0, 0, src_w, src_h)
        } else if src_ar > out_ar {
            let cw = ((src_h as f64 * out_ar).round() as u32).min(src_w) & !1;
            let cx = ((src_w - cw) / 2) & !1;
            (cx, 0u32, cw, src_h)
        } else {
            let ch = ((src_w as f64 / out_ar).round() as u32).min(src_h) & !1;
            let cy = ((src_h - ch) / 2) & !1;
            (0u32, cy, src_w, ch)
        };

        let ctx = ScaleCtx::get(
            src_fmt,
            crop_w.max(2),
            crop_h.max(2),
            Pixel::YUV420P,
            out_w,
            out_h,
            ScaleFlags::BILINEAR,
        )
        .expect("CropScaler: SwsContext");

        Self {
            ctx,
            crop_x,
            crop_y,
            crop_h,
        }
    }

    pub(super) fn run(&mut self, src: &VideoFrame, dst: &mut VideoFrame) -> Result<(), String> {
        unsafe {
            let sf = src.as_ptr();
            let df = dst.as_mut_ptr();

            let (off_y, off_uv): (usize, usize) = match src.format() {
                Pixel::YUV420P | Pixel::YUVJ420P | Pixel::YUV422P | Pixel::YUVJ422P => {
                    (self.crop_x as usize, self.crop_x as usize / 2)
                }
                Pixel::YUV444P | Pixel::YUVJ444P => {
                    let o = self.crop_x as usize;
                    (o, o)
                }
                _ => (0, 0),
            };

            let ls = &(*sf).linesize;
            let y_row_off = self.crop_y as usize * ls[0] as usize;
            let uv_row_off = (self.crop_y as usize / 2) * ls[1] as usize;

            let src_planes: [*const u8; 4] = [
                (*sf).data[0].add(off_y + y_row_off),
                if (*sf).data[1].is_null() {
                    std::ptr::null()
                } else {
                    (*sf).data[1].add(off_uv + uv_row_off)
                },
                if (*sf).data[2].is_null() {
                    std::ptr::null()
                } else {
                    (*sf).data[2].add(off_uv + uv_row_off)
                },
                std::ptr::null(),
            ];

            let ret = ffmpeg::ffi::sws_scale(
                self.ctx.as_mut_ptr(),
                src_planes.as_ptr() as _,
                (*sf).linesize.as_ptr(),
                0,
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

// ── Video frame helpers ────────────────────────────────────────────────────────

/// Send one YUV420P software frame to the video encoder, uploading to the HW
/// surface if a HW backend is active.
///
/// This helper centralises the upload logic so encode_clip and apply_transition
/// both call a single function rather than duplicating the unsafe block.
pub(super) fn send_video_frame(
    yuv: &VideoFrame,
    video_encoder: &mut ffmpeg::encoder::Video,
    hw_frames_ctx: *mut ffmpeg::ffi::AVBufferRef,
    hw_backend: HwBackend,
) -> Result<(), String> {
    if !hw_frames_ctx.is_null()
        && hw_backend != HwBackend::Software
        && hw_backend != HwBackend::VideoToolbox
    {
        // CUDA / VAAPI / AMF: upload SW frame to HW surface before encoding.
        let hw_frame = unsafe { upload_frame_to_hw(yuv, hw_frames_ctx) }
            .map_err(|e| format!("HW frame upload: {e}"))?;
        video_encoder
            .send_frame(&hw_frame)
            .map_err(|e| format!("send HW video frame to encoder: {e}"))
    } else {
        // Software or VideoToolbox: pass frame directly.
        video_encoder
            .send_frame(yuv)
            .map_err(|e| format!("send video frame to encoder: {e}"))
    }
}

pub(super) fn apply_filter_to_yuv_frame(
    yuv: &mut VideoFrame,
    filter: &FilterParams,
    w: u32,
    h: u32,
) {
    if filter.is_identity() {
        return;
    }
    unsafe {
        let ptr = yuv.as_mut_ptr();
        let y_size = (w * h) as usize;
        let uv_size = ((w / 2) * (h / 2)) as usize;
        let y_plane = std::slice::from_raw_parts_mut((*ptr).data[0], y_size);
        let u_plane = std::slice::from_raw_parts_mut((*ptr).data[1], uv_size);
        let v_plane = std::slice::from_raw_parts_mut((*ptr).data[2], uv_size);
        apply_filter_yuv(y_plane, u_plane, v_plane, filter);
    }
}

// ── Per-clip encode ────────────────────────────────────────────────────────────

/// Encode-sink plumbing shared by the per-clip writer and the transition
/// writer: where frames go, how they are encoded, the audio FIFO to mix into,
/// and the counters that must carry across clips.
///
/// Grouped into one struct instead of a dozen positional arguments, which had
/// become impossible to read at the call sites — two of them were adjacent
/// `&mut`s and the frame counter was both an input and an output.
pub(super) struct EncodeCtx<'a> {
    pub(super) spec: &'a EncodeSpec,
    pub(super) octx: &'a mut ffmpeg::format::context::Output,
    pub(super) video_encoder: &'a mut ffmpeg::encoder::Video,
    pub(super) hw_frames_ctx: *mut ffmpeg::ffi::AVBufferRef,
    pub(super) hw_backend: HwBackend,
    pub(super) audio_state: &'a mut AudioEncState,
    /// Next output frame index; both writers return the advanced value.
    pub(super) out_frame_idx: i64,
    pub(super) total_frames: u64,
    pub(super) frame_tb: Rational,
    pub(super) cancel: &'a Arc<AtomicBool>,
    pub(super) tx: &'a Sender<MediaResult>,
    pub(super) last_video_dts: &'a mut i64,
}

pub(super) fn encode_clip(clip: &ClipSpec, ctx: EncodeCtx<'_>) -> Result<i64, String> {
    let EncodeCtx {
        spec,
        octx,
        video_encoder,
        hw_frames_ctx,
        hw_backend,
        audio_state,
        out_frame_idx,
        total_frames,
        frame_tb,
        cancel,
        tx,
        last_video_dts,
    } = ctx;
    let mut out_frame_idx = out_frame_idx;
    let mut ictx =
        open_input(&clip.path).map_err(|e| format!("open '{}': {e}", clip.path.display()))?;

    let video_stream_idx = ictx
        .streams()
        .best(MediaType::Video)
        .ok_or_else(|| format!("no video stream in '{}'", clip.path.display()))?
        .index();

    let audio_stream_idx: Option<usize> = ictx.streams().best(MediaType::Audio).map(|s| s.index());

    let in_video_tb = ictx.stream(video_stream_idx).unwrap().time_base();

    let vdec_ctx = codec::context::Context::from_parameters(
        ictx.stream(video_stream_idx).unwrap().parameters(),
    )
    .map_err(|e| format!("video decoder context: {e}"))?;

    let mut video_decoder = vdec_ctx
        .decoder()
        .video()
        .map_err(|e| format!("open video decoder: {e}"))?;

    let mut audio_decoder: Option<ffmpeg::decoder::audio::Audio> = None;
    let mut in_audio_tb = Rational::new(1, AUDIO_RATE);

    if !clip.skip_audio {
        if let Some(asi) = audio_stream_idx {
            let ast = ictx.stream(asi).unwrap();
            in_audio_tb = ast.time_base();
            match codec::context::Context::from_parameters(ast.parameters()) {
                Ok(ctx) => match ctx.decoder().audio() {
                    Ok(dec) => {
                        audio_decoder = Some(dec);
                    }
                    Err(e) => {
                        crate::media_log!(
                            "[encode] audio decoder open failed for '{}': {e}",
                            clip.path.display()
                        );
                    }
                },
                Err(e) => {
                    crate::media_log!(
                        "[encode] audio decoder params failed for '{}': {e}",
                        clip.path.display()
                    );
                }
            }
        } else {
            crate::media_log!(
                "[encode] clip '{}' has no audio stream — silence will be padded \
                 (overlay tracks still mix)",
                clip.path.display(),
            );
        }
    }

    let (src_display_w, src_display_h) = {
        let stream = ictx.stream(video_stream_idx).unwrap();
        let params = stream.parameters();
        let w = params.width() as u32;
        let h = params.height() as u32;
        if w > 0 && h > 0 {
            (w, h)
        } else {
            (video_decoder.width(), video_decoder.height())
        }
    };

    seek_to_secs(&mut ictx, clip.source_offset, "encode_clip");

    let mut video_scaler: Option<CropScaler> = None;
    let mut audio_resampler: Option<resampling::Context> = None;

    let clip_end = clip.source_offset + clip.duration;
    let ost_tb = octx.stream(0).unwrap().time_base();
    let half_frame = 0.5 / spec.fps as f64;

    let clip_start_frame_idx = out_frame_idx;
    let mut video_clip_done = false;

    for result in ictx.packets() {
        let (stream, packet) =
            result.map_err(|e| format!("read packet from '{}': {e}", clip.path.display()))?;

        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }

        let sidx = stream.index();

        // ── Video packet ──────────────────────────────────────────────────────
        if sidx == video_stream_idx {
            video_decoder
                .send_packet(&packet)
                .map_err(|e| format!("send video packet to decoder: {e}"))?;

            if video_clip_done {
                let mut _discard = VideoFrame::empty();
                while video_decoder.receive_frame(&mut _discard).is_ok() {}
                continue;
            }

            let mut decoded = VideoFrame::empty();
            while video_decoder.receive_frame(&mut decoded).is_ok() {
                let frame_pts_secs = decoded
                    .pts()
                    .map(|pts| pts as f64 * f64::from(in_video_tb))
                    .unwrap_or(0.0);

                if frame_pts_secs < clip.source_offset - half_frame {
                    continue;
                }

                if frame_pts_secs >= clip_end {
                    video_clip_done = true;
                    continue;
                }

                let sc = video_scaler.get_or_insert_with(|| {
                    CropScaler::build(
                        decoded.format(),
                        src_display_w,
                        src_display_h,
                        spec.width,
                        spec.height,
                    )
                });

                let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
                sc.run(&decoded, &mut yuv)?;
                apply_filter_to_yuv_frame(&mut yuv, &clip.filter, spec.width, spec.height);

                unsafe {
                    (*yuv.as_mut_ptr()).sample_aspect_ratio =
                        ffmpeg::ffi::AVRational { num: 1, den: 1 };
                }

                let src_rel_secs = (frame_pts_secs - clip.source_offset).max(0.0);
                let target_out_pts =
                    clip_start_frame_idx + (src_rel_secs * spec.fps as f64).round() as i64;

                if target_out_pts >= out_frame_idx {
                    loop {
                        yuv.set_pts(Some(out_frame_idx));

                        send_video_frame(&yuv, video_encoder, hw_frames_ctx, hw_backend)?;

                        let mut pkt = Packet::empty();
                        while video_encoder.receive_packet(&mut pkt).is_ok() {
                            pkt.set_stream(0);
                            pkt.rescale_ts(frame_tb, ost_tb);
                            let raw_dts = pkt.dts().unwrap_or(0);
                            if *last_video_dts != i64::MIN {
                                let prev_s = *last_video_dts as f64 * f64::from(ost_tb);
                                let dts_s = raw_dts as f64 * f64::from(ost_tb);
                                if dts_s < prev_s {
                                    let clamped = *last_video_dts + 1;
                                    crate::media_log!(
                                        "[encode] non-monotonic DTS ({prev_s:.4}s → {dts_s:.4}s); \
                                         clamping {raw_dts} → {clamped}"
                                    );
                                    unsafe {
                                        (*pkt.as_mut_ptr()).dts = clamped;
                                    }
                                }
                            }
                            *last_video_dts = pkt.dts().unwrap_or(raw_dts);
                            pkt.write_interleaved(octx)
                                .map_err(|e| format!("write video packet: {e}"))?;
                        }

                        out_frame_idx += 1;

                        if (out_frame_idx as u64).is_multiple_of(PROGRESS_INTERVAL) {
                            let _ = tx.send(MediaResult::EncodeProgress {
                                job_id: spec.job_id,
                                frame: out_frame_idx as u64,
                                total_frames,
                            });
                        }

                        if out_frame_idx > target_out_pts {
                            break;
                        }
                    }

                    // Yield to the OS scheduler after every encoded video frame.
                    // SW: thread cap + priority + yield = responsive at all resolutions.
                    // HW: yield lets UI/audio preempt the decode+scale hot loop.
                    std::thread::yield_now();
                }
            }
        }

        // ── Audio packet ──────────────────────────────────────────────────────
        if let Some(ref mut adec) = audio_decoder {
            if sidx == audio_stream_idx.unwrap_or(usize::MAX) {
                if adec.send_packet(&packet).is_err() {
                    continue;
                }

                let mut raw = AudioFrame::empty();
                while adec.receive_frame(&mut raw).is_ok() {
                    let pts_secs = raw
                        .pts()
                        .map(|pts| pts as f64 * f64::from(in_audio_tb))
                        .unwrap_or(0.0);

                    if pts_secs < clip.source_offset - 0.05 {
                        continue;
                    }
                    if pts_secs >= clip_end {
                        break;
                    }

                    let pre_roll = ((clip.source_offset - pts_secs).max(0.0) * AUDIO_RATE as f64)
                        .round() as usize;

                    let src_layout = {
                        crate::helpers::resample::ensure_layout(&mut raw);
                        raw.ch_layout()
                    };
                    let needs_resample = raw.format() != Sample::F32(SampleType::Planar)
                        || raw.rate() != AUDIO_RATE as u32
                        || src_layout.channels() != 2;

                    if needs_resample {
                        let rs = audio_resampler.get_or_insert_with(|| {
                            resampling::Context::get2(
                                raw.format(),
                                src_layout,
                                raw.rate(),
                                Sample::F32(SampleType::Planar),
                                ChannelLayout::STEREO,
                                AUDIO_RATE as u32,
                            )
                            .expect("create audio resampler")
                        });
                        let mut resampled = AudioFrame::empty();
                        if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                            let fg = fade_gain(
                                pts_secs,
                                clip.source_offset,
                                clip.duration,
                                clip.fade_in_secs,
                                clip.fade_in_start_secs,
                                clip.fade_out_secs,
                                clip.fade_out_end_secs,
                            );
                            audio_state.fifo.push_scaled_from(
                                &resampled,
                                clip.volume * fg,
                                pre_roll,
                            );
                        }
                    } else {
                        let fg = fade_gain(
                            pts_secs,
                            clip.source_offset,
                            clip.duration,
                            clip.fade_in_secs,
                            clip.fade_in_start_secs,
                            clip.fade_out_secs,
                            clip.fade_out_end_secs,
                        );
                        audio_state
                            .fifo
                            .push_scaled_from(&raw, clip.volume * fg, pre_roll);
                    }
                }
            }
        }

        // Drain audio FIFO for this iteration (interspersed with video packets).
        audio_state.drain_fifo(octx, false)?;
    }

    // ── Decoder EOF drain ─────────────────────────────────────────────────────
    let _ = video_decoder.send_eof();
    let mut decoded = VideoFrame::empty();
    while video_decoder.receive_frame(&mut decoded).is_ok() {
        let frame_pts_secs = decoded
            .pts()
            .map(|pts| pts as f64 * f64::from(in_video_tb))
            .unwrap_or(0.0);

        if frame_pts_secs < clip.source_offset - half_frame {
            continue;
        }
        if frame_pts_secs >= clip_end {
            break;
        }

        if let Some(sc) = &mut video_scaler {
            let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            if sc.run(&decoded, &mut yuv).is_ok() {
                apply_filter_to_yuv_frame(&mut yuv, &clip.filter, spec.width, spec.height);
                unsafe {
                    (*yuv.as_mut_ptr()).sample_aspect_ratio =
                        ffmpeg::ffi::AVRational { num: 1, den: 1 };
                }
                let src_rel_secs = (frame_pts_secs - clip.source_offset).max(0.0);
                let target_out_pts =
                    clip_start_frame_idx + (src_rel_secs * spec.fps as f64).round() as i64;

                if target_out_pts >= out_frame_idx {
                    yuv.set_pts(Some(out_frame_idx));
                    send_video_frame(&yuv, video_encoder, hw_frames_ctx, hw_backend)?;

                    let mut pkt = Packet::empty();
                    while video_encoder.receive_packet(&mut pkt).is_ok() {
                        pkt.set_stream(0);
                        pkt.rescale_ts(frame_tb, ost_tb);
                        let raw_dts = pkt.dts().unwrap_or(0);
                        if *last_video_dts != i64::MIN {
                            let prev_s = *last_video_dts as f64 * f64::from(ost_tb);
                            let dts_s = raw_dts as f64 * f64::from(ost_tb);
                            if dts_s < prev_s {
                                let clamped = *last_video_dts + 1;
                                unsafe {
                                    (*pkt.as_mut_ptr()).dts = clamped;
                                }
                            }
                        }
                        *last_video_dts = pkt.dts().unwrap_or(raw_dts);
                        pkt.write_interleaved(octx)
                            .map_err(|e| format!("write video packet (drain): {e}"))?;
                    }
                    out_frame_idx += 1;

                    if (out_frame_idx as u64).is_multiple_of(PROGRESS_INTERVAL) {
                        let _ = tx.send(MediaResult::EncodeProgress {
                            job_id: spec.job_id,
                            frame: out_frame_idx as u64,
                            total_frames,
                        });
                    }

                    std::thread::yield_now();
                }
            }
        }
    }

    if let Some(ref mut adec) = audio_decoder {
        let _ = adec.send_eof();
        let mut raw = AudioFrame::empty();
        while adec.receive_frame(&mut raw).is_ok() {
            let pts_secs = raw
                .pts()
                .map(|pts| pts as f64 * f64::from(in_audio_tb))
                .unwrap_or(0.0);
            if pts_secs >= clip_end {
                break;
            }

            let src_layout = {
                crate::helpers::resample::ensure_layout(&mut raw);
                raw.ch_layout()
            };
            let needs_resample = raw.format() != Sample::F32(SampleType::Planar)
                || raw.rate() != AUDIO_RATE as u32
                || src_layout.channels() != 2;

            if needs_resample {
                if let Some(rs) = &mut audio_resampler {
                    let mut resampled = AudioFrame::empty();
                    if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                        let fg = fade_gain(
                            pts_secs,
                            clip.source_offset,
                            clip.duration,
                            clip.fade_in_secs,
                            clip.fade_in_start_secs,
                            clip.fade_out_secs,
                            clip.fade_out_end_secs,
                        );
                        audio_state.fifo.push_scaled(&resampled, clip.volume * fg);
                    }
                }
            } else {
                let fg = fade_gain(
                    pts_secs,
                    clip.source_offset,
                    clip.duration,
                    clip.fade_in_secs,
                    clip.fade_in_start_secs,
                    clip.fade_out_secs,
                    clip.fade_out_end_secs,
                );
                audio_state.fifo.push_scaled(&raw, clip.volume * fg);
            }
        }

        // Flush resampler tail (1080p audio dropout fix).
        if let Some(ref mut rs) = audio_resampler {
            flush_audio_resampler(rs, &mut audio_state.fifo, clip.volume);
        }
    }

    // ── Align the audio stream to this clip's video boundary ──────────────────
    // AAC packet PTS come from `out_sample_idx`, so a clip that contributes no
    // audio (skip_audio, a source with no audio stream, or a track that ends
    // before the video) would end the audio stream early and drag every later
    // clip and every absolute-time overlay earlier than the picture. Pad the
    // shortfall with silence; keep the overshoot trim for the usual case where
    // the decoder delivers slightly more than the clip's span.
    let target_samples = out_frame_idx * AUDIO_RATE as i64 / spec.fps as i64;
    let have = audio_state.out_sample_idx + audio_state.fifo.len() as i64;
    if have < target_samples {
        let shortfall = (target_samples - have) as usize;
        crate::media_log!(
            "[encode] clip '{}' audio short by {:.3}s — padding silence",
            clip.path.display(),
            shortfall as f64 / AUDIO_RATE as f64,
        );
        audio_state
            .fifo
            .left
            .extend(std::iter::repeat_n(0.0f32, shortfall));
        audio_state
            .fifo
            .right
            .extend(std::iter::repeat_n(0.0f32, shortfall));
    } else if have > target_samples {
        let excess = ((have - target_samples) as usize).min(audio_state.fifo.len());
        if excess > 0 {
            crate::media_log!(
                "[encode] clip '{}' FIFO overrun: {} extra samples — trimming",
                clip.path.display(),
                excess,
            );
            let new_len = audio_state.fifo.left.len().saturating_sub(excess);
            audio_state.fifo.left.truncate(new_len);
            audio_state.fifo.right.truncate(new_len);
        }
    }

    audio_state.drain_fifo(octx, false)?;

    Ok(out_frame_idx)
}

// ── Crossfade helpers ─────────────────────────────────────────────────────────

pub(super) fn decode_clip_frames(
    clip: &ClipSpec,
    spec: &EncodeSpec,
) -> Result<Vec<Vec<u8>>, String> {
    let mut ictx = open_input(&clip.path)
        .map_err(|e| format!("crossfade open '{}': {e}", clip.path.display()))?;

    let video_stream_idx = ictx
        .streams()
        .best(MediaType::Video)
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
    )
    .map_err(|e| format!("crossfade video decoder context: {e}"))?;

    let mut video_decoder = vdec_ctx
        .decoder()
        .video()
        .map_err(|e| format!("crossfade open video decoder: {e}"))?;

    seek_to_secs(&mut ictx, clip.source_offset, "decode_clip_frames");

    let mut video_scaler: Option<CropScaler> = None;
    let clip_end = clip.source_offset + clip.duration;
    let half_frame = 0.5 / spec.fps as f64;
    let w = spec.width as usize;
    let h = spec.height as usize;
    let _uv_w = w / 2;
    let _uv_h = h / 2;

    let mut frames: Vec<Vec<u8>> = Vec::new();

    'packet_loop: for result in ictx.packets() {
        let (stream, packet) = result.map_err(|e| format!("crossfade read packet: {e}"))?;

        if stream.index() != video_stream_idx {
            continue;
        }

        video_decoder
            .send_packet(&packet)
            .map_err(|e| format!("crossfade send packet: {e}"))?;

        let mut decoded = VideoFrame::empty();
        while video_decoder.receive_frame(&mut decoded).is_ok() {
            let pts_secs = decoded
                .pts()
                .map(|pts| pts as f64 * f64::from(in_video_tb))
                .unwrap_or(0.0);

            if pts_secs < clip.source_offset - half_frame {
                continue;
            }
            if pts_secs >= clip_end {
                break 'packet_loop;
            }

            let sc = video_scaler.get_or_insert_with(|| {
                CropScaler::build(
                    decoded.format(),
                    src_display_w,
                    src_display_h,
                    spec.width,
                    spec.height,
                )
            });

            let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
            sc.run(&decoded, &mut yuv)
                .map_err(|e| format!("crossfade scale: {e}"))?;
            apply_filter_to_yuv_frame(&mut yuv, &clip.filter, spec.width, spec.height);

            frames.push(extract_yuv(&yuv, w, h));
        }
    }

    // Decoder flush. These are the frames still held back by B-frame reordering,
    // i.e. the ones closest to the clip's end — for a transition tail that is the
    // most visible part of the blend, so it has to go through exactly the same
    // start guard, scaler and colour filter as the main loop. Skipping the filter
    // here made graded clips lose their grade on the final frames of the overlap.
    let _ = video_decoder.send_eof();
    let mut decoded = VideoFrame::empty();
    while video_decoder.receive_frame(&mut decoded).is_ok() {
        let pts_secs = decoded
            .pts()
            .map(|pts| pts as f64 * f64::from(in_video_tb))
            .unwrap_or(0.0);
        if pts_secs < clip.source_offset - half_frame {
            continue;
        }
        if pts_secs >= clip_end {
            break;
        }

        // Build the scaler here too: a clip short enough that the main loop
        // produced no frame would otherwise drop its only (flushed) frames.
        let sc = video_scaler.get_or_insert_with(|| {
            CropScaler::build(
                decoded.format(),
                src_display_w,
                src_display_h,
                spec.width,
                spec.height,
            )
        });
        let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
        if sc.run(&decoded, &mut yuv).is_ok() {
            apply_filter_to_yuv_frame(&mut yuv, &clip.filter, spec.width, spec.height);
            frames.push(extract_yuv(&yuv, w, h));
        }
    }

    Ok(frames)
}

pub(super) fn decode_clip_audio(clip: &ClipSpec) -> Result<(Vec<f32>, Vec<f32>), String> {
    if clip.skip_audio {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut ictx = open_input(&clip.path)
        .map_err(|e| format!("transition audio open '{}': {e}", clip.path.display()))?;

    let audio_stream_idx = match ictx.streams().best(MediaType::Audio) {
        Some(s) => s.index(),
        None => return Ok((Vec::new(), Vec::new())),
    };

    let ast = ictx.stream(audio_stream_idx).unwrap();
    let in_audio_tb = ast.time_base();

    let adec_ctx = codec::context::Context::from_parameters(ast.parameters()).map_err(|e| {
        format!(
            "transition audio decoder ctx '{}': {e}",
            clip.path.display()
        )
    })?;
    let mut adec = adec_ctx.decoder().audio().map_err(|e| {
        format!(
            "transition audio decoder open '{}': {e}",
            clip.path.display()
        )
    })?;

    seek_to_secs(&mut ictx, clip.source_offset, "decode_clip_audio");

    let clip_end = clip.source_offset + clip.duration;
    let target_fmt = Sample::F32(SampleType::Planar);
    let mut audio_resampler: Option<resampling::Context> = None;
    let mut left = Vec::<f32>::new();
    let mut right = Vec::<f32>::new();

    fn push_frame(
        frame: &AudioFrame,
        vol: f32,
        left: &mut Vec<f32>,
        right: &mut Vec<f32>,
        skip: usize,
    ) {
        let n = frame.samples();
        if n <= skip {
            return;
        }
        unsafe {
            let l_bytes = frame.data(0);
            let l_f32 = std::slice::from_raw_parts(l_bytes.as_ptr() as *const f32, n);
            left.extend(l_f32[skip..].iter().map(|s| (s * vol).clamp(-1.0, 1.0)));

            let r_bytes = if frame.ch_layout().channels() >= 2 {
                frame.data(1)
            } else {
                frame.data(0)
            };
            let r_f32 = std::slice::from_raw_parts(r_bytes.as_ptr() as *const f32, n);
            right.extend(r_f32[skip..].iter().map(|s| (s * vol).clamp(-1.0, 1.0)));
        }
    }

    'pkt: for result in ictx.packets() {
        let (stream, packet) = result.map_err(|e| format!("transition audio read packet: {e}"))?;
        if stream.index() != audio_stream_idx {
            continue;
        }

        if adec.send_packet(&packet).is_err() {
            continue;
        }

        let mut raw = AudioFrame::empty();
        while adec.receive_frame(&mut raw).is_ok() {
            let pts_secs = raw
                .pts()
                .map(|pts| pts as f64 * f64::from(in_audio_tb))
                .unwrap_or(0.0);
            if pts_secs < clip.source_offset - 0.05 {
                continue;
            }
            if pts_secs >= clip_end {
                break 'pkt;
            }

            let pre_roll =
                ((clip.source_offset - pts_secs).max(0.0) * AUDIO_RATE as f64).round() as usize;

            let src_layout = {
                crate::helpers::resample::ensure_layout(&mut raw);
                raw.ch_layout()
            };
            let needs_resample = raw.format() != target_fmt
                || raw.rate() != AUDIO_RATE as u32
                || src_layout.channels() != 2;

            if needs_resample {
                let rs = audio_resampler.get_or_insert_with(|| {
                    resampling::Context::get2(
                        raw.format(),
                        src_layout,
                        raw.rate(),
                        target_fmt,
                        ChannelLayout::STEREO,
                        AUDIO_RATE as u32,
                    )
                    .expect("create audio resampler (transition)")
                });
                let mut resampled = AudioFrame::empty();
                if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                    push_frame(&resampled, clip.volume, &mut left, &mut right, pre_roll);
                }
            } else {
                push_frame(&raw, clip.volume, &mut left, &mut right, pre_roll);
            }
        }
    }

    let _ = adec.send_eof();
    let mut raw = AudioFrame::empty();
    while adec.receive_frame(&mut raw).is_ok() {
        let pts_secs = raw
            .pts()
            .map(|pts| pts as f64 * f64::from(in_audio_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end {
            break;
        }

        let src_layout = {
            crate::helpers::resample::ensure_layout(&mut raw);
            raw.ch_layout()
        };
        let needs_resample = raw.format() != target_fmt
            || raw.rate() != AUDIO_RATE as u32
            || src_layout.channels() != 2;

        if needs_resample {
            if let Some(rs) = &mut audio_resampler {
                let mut resampled = AudioFrame::empty();
                if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                    push_frame(&resampled, clip.volume, &mut left, &mut right, 0);
                }
            }
        } else {
            push_frame(&raw, clip.volume, &mut left, &mut right, 0);
        }
    }

    // Flush resampler tail (same fix as encode_clip).
    if let Some(ref mut rs) = audio_resampler {
        let n_buffered = unsafe { ffmpeg::ffi::swr_get_delay(rs.as_mut_ptr(), AUDIO_RATE as i64) };
        if n_buffered > 0 {
            let mut tmp_frame = AudioFrame::new(
                Sample::F32(SampleType::Planar),
                4096,
                ChannelLayoutMask::STEREO,
            );
            tmp_frame.set_rate(AUDIO_RATE as u32);
            unsafe {
                let n_out = ffmpeg::ffi::swr_convert(
                    rs.as_mut_ptr(),
                    (*tmp_frame.as_mut_ptr()).data.as_mut_ptr(),
                    4096,
                    std::ptr::null_mut(),
                    0,
                );
                if n_out > 0 {
                    (*tmp_frame.as_mut_ptr()).nb_samples = n_out;
                    push_frame(&tmp_frame, clip.volume, &mut left, &mut right, 0);
                }
            }
        }
    }

    Ok((left, right))
}

// ── Centred transition math ───────────────────────────────────────────────────
//
// Export renders a transition the same way the preview does: a zone of `D`
// seconds centred on the cut, taking D/2 from each side. Each half therefore
// only has D/2 seconds of material, so the blend holds:
//
//   output frames 0 .. n/2   — outgoing half plays, incoming half holds its first frame
//   output frames n/2 .. n   — outgoing half holds its last frame, incoming half plays
//
// Keeping this identical to `clip_query::active_transition_at` is what makes an
// exported transition look like what the timeline showed.

/// Source frame of each half that drives output frame `i` of a centred blend.
///
/// `n` is the total number of blend frames, `tail_len`/`head_len` the number of
/// decoded frames available per half. Returns `None` when either half is empty.
pub(super) fn centred_blend_indices(
    i: usize,
    n: usize,
    tail_len: usize,
    head_len: usize,
) -> Option<(usize, usize)> {
    if tail_len == 0 || head_len == 0 || n == 0 {
        return None;
    }
    let half = n / 2;
    let tail_idx = i.min(tail_len - 1);
    let head_idx = i.saturating_sub(half).min(head_len - 1);
    Some((tail_idx, head_idx))
}

// ── Transition application ────────────────────────────────────────────────────

pub(super) fn apply_transition(
    transition: &dyn VideoTransition,
    tail_spec: &ClipSpec,
    head_spec: &ClipSpec,
    ctx: EncodeCtx<'_>,
) -> Result<i64, String> {
    let EncodeCtx {
        spec,
        octx,
        video_encoder,
        hw_frames_ctx,
        hw_backend,
        audio_state,
        out_frame_idx,
        total_frames,
        frame_tb,
        cancel,
        tx,
        last_video_dts,
    } = ctx;
    let mut out_frame_idx = out_frame_idx;
    let tail_frames = decode_clip_frames(tail_spec, spec)?;
    let head_frames = decode_clip_frames(head_spec, spec)?;

    let (tail_audio_l, tail_audio_r) = decode_clip_audio(tail_spec)?;
    let (head_audio_l, head_audio_r) = decode_clip_audio(head_spec)?;

    let samples_per_frame_f = AUDIO_RATE as f64 / spec.fps as f64;

    // Each half carries D/2 seconds of material, and the blend as a whole lasts
    // the full D — the same span the preview's centred zone covers.
    let n = ((tail_spec.duration * 2.0) * spec.fps as f64)
        .round()
        .max(1.0) as usize;
    if tail_frames.is_empty() || head_frames.is_empty() {
        return Ok(out_frame_idx);
    }

    let w = spec.width as usize;
    let h = spec.height as usize;
    let _uv_w = w / 2;
    let _uv_h = h / 2;
    // Output sample offset of the blend's second half, in the head half's own
    // audio timebase.
    let head_sample_offset = ((n / 2) as f64 * samples_per_frame_f).round() as usize;
    let ost_tb = octx.stream(0).unwrap().time_base();

    for i in 0..n {
        if cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }

        let alpha = velocut_core::transitions::helpers::frame_alpha(i, n);
        let Some((tail_idx, head_idx)) =
            centred_blend_indices(i, n, tail_frames.len(), head_frames.len())
        else {
            return Ok(out_frame_idx);
        };
        let blended = transition.apply(
            &tail_frames[tail_idx],
            &head_frames[head_idx],
            spec.width,
            spec.height,
            alpha,
        );

        let mut yuv = VideoFrame::new(Pixel::YUV420P, spec.width, spec.height);
        yuv.set_pts(Some(out_frame_idx));
        unsafe {
            (*yuv.as_mut_ptr()).sample_aspect_ratio = ffmpeg::ffi::AVRational { num: 1, den: 1 };
        }

        write_yuv(&blended, &mut yuv, w, h);

        send_video_frame(&yuv, video_encoder, hw_frames_ctx, hw_backend)?;

        let mut pkt = Packet::empty();
        while video_encoder.receive_packet(&mut pkt).is_ok() {
            pkt.set_stream(0);
            pkt.rescale_ts(frame_tb, ost_tb);
            let raw_dts = pkt.dts().unwrap_or(0);
            if *last_video_dts != i64::MIN {
                let prev_s = *last_video_dts as f64 * f64::from(ost_tb);
                let dts_s = raw_dts as f64 * f64::from(ost_tb);
                if dts_s < prev_s {
                    let clamped = *last_video_dts + 1;
                    crate::media_log!(
                        "[transition] non-monotonic DTS ({prev_s:.4}s → {dts_s:.4}s); \
                         clamping {raw_dts} → {clamped}"
                    );
                    unsafe {
                        (*pkt.as_mut_ptr()).dts = clamped;
                    }
                }
            }
            *last_video_dts = pkt.dts().unwrap_or(raw_dts);
            pkt.write_interleaved(octx)
                .map_err(|e| format!("transition write packet: {e}"))?;
        }

        let sample_start = (i as f64 * samples_per_frame_f).round() as usize;
        let sample_end = ((i + 1) as f64 * samples_per_frame_f).round() as usize;
        let first_half = i < n / 2;
        // Each half's audio covers D/2 seconds only, so it is indexed from its own
        // start while that half is the one playing. The other half stays silent
        // rather than holding a sample — a held sample is a DC offset.
        let tail_last = tail_audio_l.len().saturating_sub(1);
        let head_last = head_audio_l.len().saturating_sub(1);

        for s in sample_start..sample_end {
            let (t_l, t_r) = if first_half && !tail_audio_l.is_empty() {
                let idx = s.min(tail_last);
                (tail_audio_l[idx], tail_audio_r[idx])
            } else {
                (0.0, 0.0)
            };
            let (h_l, h_r) = if !first_half && !head_audio_l.is_empty() {
                let idx = s.saturating_sub(head_sample_offset).min(head_last);
                (head_audio_l[idx], head_audio_r[idx])
            } else {
                (0.0, 0.0)
            };
            // Level matches playback: full across each half, with only the 40 ms
            // gate around the cut. Ramping across a whole half made an exported
            // transition sound ducked and stepping 0.707/0.707 at the cut clicks.
            let (gain_out, gain_in) = cut_gate(s as i64 - head_sample_offset as i64, AUDIO_RATE);
            audio_state
                .fifo
                .left
                .push((t_l * gain_out + h_l * gain_in).clamp(-1.0, 1.0));
            audio_state
                .fifo
                .right
                .push((t_r * gain_out + h_r * gain_in).clamp(-1.0, 1.0));
        }

        audio_state.drain_fifo(octx, false)?;

        out_frame_idx += 1;

        std::thread::yield_now();

        if (out_frame_idx as u64).is_multiple_of(PROGRESS_INTERVAL) {
            let _ = tx.send(MediaResult::EncodeProgress {
                job_id: spec.job_id,
                frame: out_frame_idx as u64,
                total_frames,
            });
        }
    }

    Ok(out_frame_idx)
}

#[cfg(test)]
mod centred_transition_tests {
    use super::centred_blend_indices;
    use crate::encode::audio::cut_gate;

    #[test]
    fn outgoing_half_plays_then_holds() {
        // 20 output frames (10 per half), 10 tail frames, 10 head frames.
        let (t0, h0) = centred_blend_indices(0, 20, 10, 10).unwrap();
        assert_eq!(
            (t0, h0),
            (0, 0),
            "blend must start on both clips' first tail/head frame"
        );

        let (t9, h9) = centred_blend_indices(9, 20, 10, 10).unwrap();
        assert_eq!(
            (t9, h9),
            (9, 0),
            "incoming half holds frame 0 until the cut"
        );

        let (t10, h10) = centred_blend_indices(10, 20, 10, 10).unwrap();
        assert_eq!(
            (t10, h10),
            (10 - 1, 0),
            "tail holds its last frame at the cut"
        );
    }

    #[test]
    fn incoming_half_starts_at_the_cut_and_plays_out() {
        let (t15, h5) = centred_blend_indices(15, 20, 10, 10).unwrap();
        // Second half index 5 → head frame 5, tail frozen on its last frame.
        assert_eq!((t15, h5), (9, 5));

        let (t19, h9) = centred_blend_indices(19, 20, 10, 10).unwrap();
        assert_eq!(
            (t19, h9),
            (9, 9),
            "blend must end on the incoming clip's last head frame"
        );
    }

    #[test]
    fn holds_clamp_instead_of_panicking_when_a_half_is_short() {
        // A decode that yielded fewer frames than requested must not index out
        // of bounds — it holds the last frame it has.
        assert_eq!(centred_blend_indices(18, 20, 3, 2).unwrap(), (2, 1));
        assert_eq!(centred_blend_indices(19, 20, 1, 1).unwrap(), (0, 0));
    }

    #[test]
    fn empty_half_reports_no_blend() {
        assert_eq!(centred_blend_indices(0, 20, 0, 10), None);
        assert_eq!(centred_blend_indices(0, 20, 10, 0), None);
        assert_eq!(centred_blend_indices(0, 0, 10, 10), None);
    }

    #[test]
    fn audio_is_full_level_away_from_the_cut() {
        let rate: i32 = 44_100;
        let rate_i = rate as i64;
        // Well before the cut: outgoing at unity, incoming silent.
        assert_eq!(cut_gate(-rate_i / 2, rate), (1.0, 0.0));
        // Well after it: incoming at unity, outgoing silent.
        assert_eq!(cut_gate(rate_i / 2, rate), (0.0, 1.0));
    }

    #[test]
    fn audio_gate_straddles_the_cut() {
        let rate: i32 = 44_100;
        // Just inside the 40 ms window on either side: halfway through the ramp.
        let half = (rate as f64 * 0.020).round() as i64;
        let (out, inc) = cut_gate(-half, rate);
        assert!((out - 0.5).abs() < 0.02, "outgoing {out}");
        assert_eq!(inc, 0.0);
        let (out, inc) = cut_gate(half, rate);
        assert_eq!(out, 0.0);
        assert!((inc - 0.5).abs() < 0.02, "incoming {inc}");
    }

    #[test]
    fn audio_gate_is_silent_exactly_at_the_cut() {
        // The swap itself must be silent on both sides, which is what stops the
        // click playback avoids with the same 40 ms gate.
        assert_eq!(cut_gate(0, 44_100), (0.0, 0.0));
    }
}
