// crates/velocut-media/src/encode/audio.rs
//
// Audio FIFO, encoder state, overlay decode, and fade envelope.
// Extracted from encode/mod.rs.

use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling;
use ffmpeg::util::channel_layout::{ChannelLayout, ChannelLayoutMask};
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg::util::rational::Rational;
use ffmpeg::Packet;
use ffmpeg_the_third as ffmpeg;

use super::{AudioOverlay, AUDIO_RATE};

// ── Audio FIFO ────────────────────────────────────────────────────────────────

pub(super) struct AudioFifo {
    pub(super) left: Vec<f32>,
    pub(super) right: Vec<f32>,
}

impl AudioFifo {
    pub(super) fn new() -> Self {
        Self {
            left: Vec::new(),
            right: Vec::new(),
        }
    }
    pub(super) fn len(&self) -> usize {
        self.left.len()
    }

    /// Like `push_scaled` but discards the first `skip` samples (pre-roll trim).
    pub(super) fn push_scaled_from(&mut self, frame: &AudioFrame, volume: f32, skip: usize) {
        let n = frame.samples();
        if n <= skip {
            return;
        }
        unsafe {
            let l_bytes = frame.data(0);
            let l_f32 = std::slice::from_raw_parts(l_bytes.as_ptr() as *const f32, n);
            self.left
                .extend(l_f32[skip..].iter().map(|s| (s * volume).clamp(-1.0, 1.0)));

            let r_bytes = if frame.ch_layout().channels() >= 2 {
                frame.data(1)
            } else {
                frame.data(0)
            };
            let r_f32 = std::slice::from_raw_parts(r_bytes.as_ptr() as *const f32, n);
            self.right
                .extend(r_f32[skip..].iter().map(|s| (s * volume).clamp(-1.0, 1.0)));
        }
    }

    pub(super) fn push_scaled(&mut self, frame: &AudioFrame, volume: f32) {
        let n = frame.samples();
        if n == 0 {
            return;
        }
        unsafe {
            let l_bytes = frame.data(0);
            let l_f32 = std::slice::from_raw_parts(l_bytes.as_ptr() as *const f32, n);
            self.left
                .extend(l_f32.iter().map(|s| (s * volume).clamp(-1.0, 1.0)));

            let r_bytes = if frame.ch_layout().channels() >= 2 {
                frame.data(1)
            } else {
                frame.data(0)
            };
            let r_f32 = std::slice::from_raw_parts(r_bytes.as_ptr() as *const f32, n);
            self.right
                .extend(r_f32.iter().map(|s| (s * volume).clamp(-1.0, 1.0)));
        }
    }

    pub(super) fn pop_frame(&mut self, n: usize, sample_idx: i64) -> AudioFrame {
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
            let ldst = std::slice::from_raw_parts_mut(ldata.as_mut_ptr() as *mut f32, n);
            ldst[..available].copy_from_slice(&self.left[..available]);
            if available < n {
                ldst[available..].fill(0.0);
            }

            let rdata = frame.data_mut(1);
            let rdst = std::slice::from_raw_parts_mut(rdata.as_mut_ptr() as *mut f32, n);
            rdst[..available].copy_from_slice(&self.right[..available]);
            if available < n {
                rdst[available..].fill(0.0);
            }
        }

        self.left.drain(..available);
        self.right.drain(..available);
        frame
    }
}

// ── Audio encoder state ───────────────────────────────────────────────────────

pub(super) struct DecodedOverlay {
    pub(super) left: Vec<f32>,
    pub(super) right: Vec<f32>,
    pub(super) start_sample: i64,
    pub(super) sample_count: usize,
}

pub(super) struct AudioEncState {
    pub(super) encoder: ffmpeg::encoder::Audio,
    pub(super) out_sample_idx: i64,
    pub(super) frame_size: usize,
    pub(super) fifo: AudioFifo,
    pub(super) audio_tb: Rational,
    pub(super) ost_audio_tb: Rational,
    pub(super) overlays: Vec<DecodedOverlay>,
    /// Counts FIFO overrun events; used to throttle log spam at 1080p SW encode.
    pub(super) fifo_overrun_count: u64,
}

impl AudioEncState {
    pub(super) fn drain_fifo(
        &mut self,
        octx: &mut ffmpeg::format::context::Output,
        flush: bool,
    ) -> Result<(), String> {
        if !flush && self.fifo.len() > 2 * self.frame_size {
            self.fifo_overrun_count += 1;
            if self.fifo_overrun_count == 1 || self.fifo_overrun_count.is_multiple_of(500) {
                crate::media_log!(
                    "[encode] audio FIFO overrun: {} samples buffered (threshold={}); \
                     audio running ahead of video (occurrence #{})",
                    self.fifo.len(),
                    2 * self.frame_size,
                    self.fifo_overrun_count,
                );
            }
        }
        while self.fifo.len() >= self.frame_size || (flush && self.fifo.len() > 0) {
            let mut frame = self.fifo.pop_frame(self.frame_size, self.out_sample_idx);

            if !self.overlays.is_empty() {
                let n = self.frame_size;
                unsafe {
                    let fptr = frame.as_mut_ptr();
                    let ldst = std::slice::from_raw_parts_mut((*fptr).data[0] as *mut f32, n);
                    let rdst = std::slice::from_raw_parts_mut((*fptr).data[1] as *mut f32, n);
                    for ov in &self.overlays {
                        for i in 0..n {
                            let ov_s = self.out_sample_idx + i as i64 - ov.start_sample;
                            if ov_s >= 0 && (ov_s as usize) < ov.sample_count {
                                let idx = ov_s as usize;
                                ldst[i] = (ldst[i] + ov.left[idx]).clamp(-1.0, 1.0);
                                rdst[i] = (rdst[i] + ov.right[idx]).clamp(-1.0, 1.0);
                            }
                        }
                    }
                }
            }

            self.out_sample_idx += self.frame_size as i64;

            self.encoder
                .send_frame(&frame)
                .map_err(|e| format!("send audio frame to encoder: {e}"))?;
            self.drain_packets(octx)?;
        }
        Ok(())
    }

    pub(super) fn drain_packets(
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

    pub(super) fn flush_encoder(
        &mut self,
        octx: &mut ffmpeg::format::context::Output,
    ) -> Result<(), String> {
        self.encoder
            .send_eof()
            .map_err(|e| format!("send audio EOF: {e}"))?;
        self.drain_packets(octx)
    }
}

// ── Audio resampler flush ─────────────────────────────────────────────────────

/// Flush any buffered samples from the SwrContext after decoder EOF.
///
/// After the last `receive_frame()` call the SwrContext may still hold internal
/// samples that never appear in `receive_frame` output. This function extracts
/// them by calling swr_convert with a null input pointer (the documented API for
/// flushing the internal delay line).
///
/// This is the primary fix for audio dropouts at 1080p: at high resolution the
/// encoder is slow enough that the packet loop's send_packet/receive_frame
/// interleaving leaves the resampler partially filled at clip_end, and without
/// this flush those samples are silently discarded.
pub(super) fn flush_audio_resampler(
    resampler: &mut resampling::Context,
    fifo: &mut AudioFifo,
    volume: f32,
) {
    loop {
        let mut out_frame = AudioFrame::new(
            Sample::F32(SampleType::Planar),
            4096,
            ChannelLayoutMask::STEREO,
        );
        out_frame.set_rate(AUDIO_RATE as u32);

        unsafe {
            let n_out = ffmpeg::ffi::swr_convert(
                resampler.as_mut_ptr(),
                (*out_frame.as_mut_ptr()).data.as_mut_ptr(),
                4096,
                std::ptr::null_mut(),
                0,
            );
            if n_out <= 0 {
                break;
            }
            (*out_frame.as_mut_ptr()).nb_samples = n_out;
        }

        fifo.push_scaled(&out_frame, volume);
    }
}

// ── Overlay decode ────────────────────────────────────────────────────────────

pub(super) fn decode_overlay(overlay: &AudioOverlay) -> Result<DecodedOverlay, String> {
    use ffmpeg::format::input as open_input;

    let target_fmt = Sample::F32(SampleType::Planar);
    const OUT_RATE: u32 = 44_100;

    let mut ictx = open_input(&overlay.path)
        .map_err(|e| format!("overlay open '{}': {e}", overlay.path.display()))?;

    let audio_idx = ictx
        .streams()
        .best(MediaType::Audio)
        .ok_or_else(|| format!("no audio stream in overlay '{}'", overlay.path.display()))?
        .index();

    let ast = ictx.stream(audio_idx).unwrap();
    let in_tb = ast.time_base();
    let adec_ctx = ffmpeg::codec::context::Context::from_parameters(ast.parameters())
        .map_err(|e| format!("overlay codec ctx: {e}"))?;
    let mut adec = adec_ctx
        .decoder()
        .audio()
        .map_err(|e| format!("overlay audio decoder: {e}"))?;

    let seek_ts = {
        let tb = in_tb;
        (overlay.source_offset * tb.denominator() as f64 / tb.numerator() as f64) as i64
    };
    let _ = ictx.seek(seek_ts, ..=seek_ts);

    let mut resampler: Option<resampling::Context> = None;
    let mut left: Vec<f32> = Vec::new();
    let mut right: Vec<f32> = Vec::new();

    let clip_end = overlay.source_offset + overlay.duration;

    let push_frame = |frame: &AudioFrame, left: &mut Vec<f32>, right: &mut Vec<f32>, vol: f32| {
        let n = frame.samples();
        if n == 0 {
            return;
        }
        unsafe {
            let l = std::slice::from_raw_parts(frame.data(0).as_ptr() as *const f32, n);
            let channels = frame.ch_layout().channels();
            let r_plane = if channels >= 2 {
                frame.data(1)
            } else {
                frame.data(0)
            };
            let r = std::slice::from_raw_parts(r_plane.as_ptr() as *const f32, n);
            left.extend(l.iter().map(|s| (s * vol).clamp(-1.0, 1.0)));
            right.extend(r.iter().map(|s| (s * vol).clamp(-1.0, 1.0)));
        }
    };

    for result in ictx.packets() {
        let (stream, packet) = match result {
            Ok(p) => p,
            Err(_) => continue,
        };
        if stream.index() != audio_idx {
            continue;
        }
        if adec.send_packet(&packet).is_err() {
            continue;
        }

        let mut raw = AudioFrame::empty();
        while adec.receive_frame(&mut raw).is_ok() {
            let pts_secs = raw
                .pts()
                .map(|p| p as f64 * f64::from(in_tb))
                .unwrap_or(0.0);

            if pts_secs < overlay.source_offset - 0.05 {
                continue;
            }
            if pts_secs >= clip_end {
                break;
            }

            let src_channels = raw.ch_layout().channels();
            let needs_resample =
                raw.format() != target_fmt || raw.rate() != OUT_RATE || src_channels != 2;

            if needs_resample {
                let rs = resampler.get_or_insert_with(|| {
                    let src_layout = if src_channels >= 2 {
                        raw.ch_layout()
                    } else {
                        ChannelLayout::MONO
                    };
                    resampling::Context::get2(
                        raw.format(),
                        src_layout,
                        raw.rate(),
                        target_fmt,
                        ChannelLayout::STEREO,
                        OUT_RATE,
                    )
                    .expect("overlay resampler")
                });
                let mut resampled = AudioFrame::empty();
                if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                    let fg = fade_gain(
                        pts_secs,
                        overlay.source_offset,
                        overlay.duration,
                        overlay.fade_in_secs,
                        overlay.fade_in_start_secs,
                        overlay.fade_out_secs,
                        overlay.fade_out_end_secs,
                    );
                    push_frame(&resampled, &mut left, &mut right, overlay.volume * fg);
                }
            } else {
                let fg = fade_gain(
                    pts_secs,
                    overlay.source_offset,
                    overlay.duration,
                    overlay.fade_in_secs,
                    overlay.fade_in_start_secs,
                    overlay.fade_out_secs,
                    overlay.fade_out_end_secs,
                );
                push_frame(&raw, &mut left, &mut right, overlay.volume * fg);
            }
        }
    }

    let _ = adec.send_eof();
    let mut raw = AudioFrame::empty();
    while adec.receive_frame(&mut raw).is_ok() {
        let pts_secs = raw
            .pts()
            .map(|p| p as f64 * f64::from(in_tb))
            .unwrap_or(0.0);
        if pts_secs >= clip_end {
            break;
        }

        let src_channels = raw.ch_layout().channels();
        let needs_resample =
            raw.format() != target_fmt || raw.rate() != OUT_RATE || src_channels != 2;

        if needs_resample {
            if let Some(rs) = &mut resampler {
                let mut resampled = AudioFrame::empty();
                if rs.run(&raw, &mut resampled).is_ok() && resampled.samples() > 0 {
                    let fg = fade_gain(
                        pts_secs,
                        overlay.source_offset,
                        overlay.duration,
                        overlay.fade_in_secs,
                        overlay.fade_in_start_secs,
                        overlay.fade_out_secs,
                        overlay.fade_out_end_secs,
                    );
                    push_frame(&resampled, &mut left, &mut right, overlay.volume * fg);
                }
            }
        } else {
            let fg = fade_gain(
                pts_secs,
                overlay.source_offset,
                overlay.duration,
                overlay.fade_in_secs,
                overlay.fade_in_start_secs,
                overlay.fade_out_secs,
                overlay.fade_out_end_secs,
            );
            push_frame(&raw, &mut left, &mut right, overlay.volume * fg);
        }
    }

    // Flush resampler tail — mirrors the same fix in encode_clip.
    if let Some(ref mut rs) = resampler {
        loop {
            let mut tmp = AudioFrame::new(
                Sample::F32(SampleType::Planar),
                4096,
                ChannelLayoutMask::STEREO,
            );
            tmp.set_rate(OUT_RATE);
            unsafe {
                let n_out = ffmpeg::ffi::swr_convert(
                    rs.as_mut_ptr(),
                    (*tmp.as_mut_ptr()).data.as_mut_ptr(),
                    4096,
                    std::ptr::null_mut(),
                    0,
                );
                if n_out <= 0 {
                    break;
                }
                (*tmp.as_mut_ptr()).nb_samples = n_out;
            }
            push_frame(&tmp, &mut left, &mut right, overlay.volume);
        }
    }

    if left.is_empty() {
        return Err(format!(
            "overlay '{}': no audio decoded",
            overlay.path.display()
        ));
    }

    let sample_count = left.len();
    let start_sample = (overlay.timeline_start * OUT_RATE as f64).round() as i64;

    crate::media_log!(
        "[encode] overlay decoded: {} samples ({:.2}s) start_sample={} ← {}",
        sample_count,
        sample_count as f64 / OUT_RATE as f64,
        start_sample,
        overlay.path.display(),
    );

    Ok(DecodedOverlay {
        left,
        right,
        start_sample,
        sample_count,
    })
}

// ── Fade envelope ─────────────────────────────────────────────────────────────

/// Equal-power fade envelope with anchor support.
///
/// Fade-in:  silence for `fade_in_start_secs`, then sqrt-ramp over `fade_in_secs`.
/// Fade-out: sqrt-ramp over `fade_out_secs`, then silence for `fade_out_end_secs`.
#[inline]
pub(super) fn fade_gain(
    pts_secs: f64,
    source_offset: f64,
    duration: f64,
    fade_in_secs: f32,
    fade_in_start_secs: f32,
    fade_out_secs: f32,
    fade_out_end_secs: f32,
) -> f32 {
    let elapsed = (pts_secs - source_offset).max(0.0) as f32;
    let remain = (duration as f32 - elapsed).max(0.0);
    let in_gain = if elapsed < fade_in_start_secs {
        0.0
    } else if fade_in_secs > 0.0 {
        ((elapsed - fade_in_start_secs) / fade_in_secs)
            .clamp(0.0, 1.0)
            .sqrt()
    } else {
        1.0
    };
    let out_gain = if remain < fade_out_end_secs {
        0.0
    } else if fade_out_secs > 0.0 {
        ((remain - fade_out_end_secs) / fade_out_secs)
            .clamp(0.0, 1.0)
            .sqrt()
    } else {
        1.0
    };
    in_gain.min(out_gain)
}
