// crates/velocut-media/src/decode.rs
//
// LiveDecoder: stateful per-clip decoder that avoids re-open/seek every frame.
// decode_frame: one-shot frame decode for preview and PNG save.

use std::path::PathBuf;
use anyhow::Result;
use crossbeam_channel::Sender;
use uuid::Uuid;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as SwsContext, flag::Flags};

use velocut_core::media_types::MediaResult;

// ── Stateful per-clip decoder ─────────────────────────────────────────────────

pub struct LiveDecoder {
    pub path:      PathBuf,
    pub ictx:      ffmpeg::format::context::Input,
    pub decoder:   ffmpeg::decoder::video::Video,
    pub video_idx: usize,
    pub last_pts:  i64,
    pub tb_num:    i32,
    pub tb_den:    i32,
    pub out_w:     u32,
    pub out_h:     u32,
    pub scaler:    SwsContext,
}

impl LiveDecoder {
    pub fn open(path: &PathBuf, timestamp: f64, aspect: f32) -> Result<Self> {
        let mut ictx = input(path)?;
        let video_idx = ictx.streams().best(Type::Video)
            .ok_or_else(|| anyhow::anyhow!("no video stream"))?.index();

        let (tb_num, tb_den, seek_ts, raw_w, raw_h) = {
            let stream = ictx.stream(video_idx).unwrap();
            let tb = stream.time_base();
            let seek_ts = (timestamp * tb.denominator() as f64 / tb.numerator() as f64) as i64;
            let (w, h) = unsafe {
                let p = stream.parameters().as_ptr();
                ((*p).width as u32, (*p).height as u32)
            };
            (tb.numerator(), tb.denominator(), seek_ts, w, h)
        };

        let _ = ictx.seek(seek_ts, ..=seek_ts);

        // Second context for decoder params (avoids borrow conflict with ictx).
        let ictx2   = input(path)?;
        let stream2 = ictx2.stream(video_idx).unwrap();
        let dec_ctx = ffmpeg::codec::context::Context::from_parameters(stream2.parameters())?;
        let decoder = dec_ctx.decoder().video()?;

        let (out_w, out_h) = if aspect <= 0.0 {
            (raw_w.max(2), raw_h.max(2))
        } else {
            let w: u32 = 640;
            let h: u32 = ((w as f32 / aspect.max(0.01)) as u32).max(2) & !1;
            (w, h)
        };

        let scaler = SwsContext::get(
            decoder.format(), decoder.width(), decoder.height(),
            Pixel::RGBA, out_w, out_h, Flags::BILINEAR,
        )?;

        Ok(Self {
            path: path.clone(), ictx, decoder, video_idx,
            last_pts: seek_ts, tb_num, tb_den, out_w, out_h, scaler,
        })
    }

    pub fn ts_to_pts(&self, t: f64) -> i64 {
        (t * self.tb_den as f64 / self.tb_num as f64) as i64
    }

    pub fn pts_to_secs(&self, pts: i64) -> f64 {
        pts as f64 * self.tb_num as f64 / self.tb_den as f64
    }

    /// Decode the next frame sequentially (no seek). Returns `(pixels, w, h, ts_secs)` or None at EOF.
    pub fn next_frame(&mut self) -> Option<(Vec<u8>, u32, u32, f64)> {
        for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                let ts_secs = self.pts_to_secs(pts);
                let mut out = ffmpeg::util::frame::video::Video::empty();
                if self.scaler.run(&decoded, &mut out).is_err() { return None; }
                let stride = out.stride(0);
                let raw    = out.data(0);
                let data: Vec<u8> = (0..self.out_h as usize)
                    .flat_map(|row| {
                        let s = row * stride;
                        &raw[s..s + self.out_w as usize * 4]
                    })
                    .copied()
                    .collect();
                return Some((data, self.out_w, self.out_h, ts_secs));
            }
        }
        None
    }

    /// Read forward until we find a frame at or past `target_pts`. Returns RGBA pixels.
    pub fn advance_to(&mut self, target_pts: i64) -> Option<(Vec<u8>, u32, u32)> {
        let mut last_good: Option<Vec<u8>> = None;
        for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                let mut out = ffmpeg::util::frame::video::Video::empty();
                if self.scaler.run(&decoded, &mut out).is_err() {
                    return last_good.map(|d| (d, self.out_w, self.out_h));
                }
                let stride = out.stride(0);
                let raw    = out.data(0);
                let data: Vec<u8> = (0..self.out_h as usize)
                    .flat_map(|row| {
                        let s = row * stride;
                        &raw[s..s + self.out_w as usize * 4]
                    })
                    .copied()
                    .collect();
                last_good = Some(data.clone());
                if pts < target_pts { continue; }
                return Some((data, self.out_w, self.out_h));
            }
        }
        last_good.map(|d| (d, self.out_w, self.out_h))
    }
}

// ── One-shot frame decode (preview + PNG save) ────────────────────────────────

pub fn decode_frame(
    path:      &PathBuf,
    id:        Uuid,
    timestamp: f64,
    aspect:    f32,        // 0.0 = use native resolution
    save_png:  bool,       // true = write PNG to dest, false = send VideoFrame
    dest:      Option<PathBuf>,
    tx:        &Sender<MediaResult>,
) -> Result<()> {
    let mut ictx = input(path)?;

    let video_stream_idx = ictx.streams().best(Type::Video)
        .ok_or_else(|| anyhow::anyhow!("no video stream"))?
        .index();

    let seek_ts = {
        let stream = ictx.stream(video_stream_idx).unwrap();
        let tb     = stream.time_base();
        (timestamp * tb.denominator() as f64 / tb.numerator() as f64) as i64
    };
    ictx.seek(seek_ts, ..=seek_ts)?;

    // Second context for decoder construction (Parameters borrows from Stream/ictx).
    let ictx2       = input(path)?;
    let stream2     = ictx2.stream(video_stream_idx).ok_or_else(|| anyhow::anyhow!("stream gone"))?;
    let decoder_ctx = ffmpeg::codec::context::Context::from_parameters(stream2.parameters())?;
    let mut decoder = decoder_ctx.decoder().video()?;

    let (out_w, out_h) = if save_png || aspect <= 0.0 {
        (decoder.width(), decoder.height())
    } else {
        let w: u32 = 640;
        let h: u32 = ((w as f32 / aspect.max(0.01)) as u32).max(2) & !1;
        (w, h)
    };

    let out_fmt = if save_png { Pixel::RGB24 } else { Pixel::RGBA };

    let mut scaler = SwsContext::get(
        decoder.format(), decoder.width(), decoder.height(),
        out_fmt, out_w, out_h,
        Flags::BILINEAR,
    )?;

    // last_good holds the most-recently scaled frame in case we hit EOF before
    // reaching seek_ts (e.g. requesting the final frame of a clip).
    let mut last_good: Option<ffmpeg::util::frame::video::Video> = None;

    for (stream, packet) in ictx.packets().flatten() {
        if stream.index() != video_stream_idx { continue; }
        decoder.send_packet(&packet)?;
        let mut decoded = ffmpeg::util::frame::video::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            let mut out_frame = ffmpeg::util::frame::video::Video::empty();
            scaler.run(&decoded, &mut out_frame)?;
            last_good = Some(out_frame.clone());
            // Skip frames that landed before our target due to keyframe-aligned seek.
            if let Some(pts) = decoded.pts() {
                if pts + 2 < seek_ts { continue; }
            }
            emit_frame(&out_frame, id, out_w, out_h, save_png, &dest, tx)?;
            return Ok(());
        }
    }

    // EOF reached without hitting seek_ts — emit the last frame we saw.
    if let Some(out_frame) = last_good {
        emit_frame(&out_frame, id, out_w, out_h, save_png, &dest, tx)?;
        return Ok(());
    }

    Err(anyhow::anyhow!("no frame found at t={timestamp:.3}"))
}

/// Emit a decoded frame: either write a PNG to disk or send a VideoFrame result.
fn emit_frame(
    out_frame: &ffmpeg::util::frame::video::Video,
    id:        Uuid,
    out_w:     u32,
    out_h:     u32,
    save_png:  bool,
    dest:      &Option<PathBuf>,
    tx:        &Sender<MediaResult>,
) -> Result<()> {
    let stride = out_frame.stride(0);
    let raw    = out_frame.data(0);

    if save_png {
        use std::io::BufWriter;
        let dest_path = dest.clone()
            .ok_or_else(|| anyhow::anyhow!("no dest path for PNG save"))?;
        let file = std::fs::File::create(&dest_path)?;
        let w    = &mut BufWriter::new(file);
        let mut encoder = png::Encoder::new(w, out_w, out_h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let row_bytes = out_w as usize * 3;
        let rows: Vec<&[u8]> = (0..out_h as usize)
            .map(|row| &raw[row * stride..row * stride + row_bytes])
            .collect();
        writer.write_image_data(&rows.concat())?;
        eprintln!("[media] PNG saved → {}", dest_path.display());
        let _ = tx.send(MediaResult::FrameSaved { path: dest_path });
    } else {
        let data: Vec<u8> = (0..out_h as usize)
            .flat_map(|row| {
                let start = row * stride;
                &raw[start..start + out_w as usize * 4]
            })
            .copied()
            .collect();
        let _ = tx.send(MediaResult::VideoFrame { id, width: out_w, height: out_h, data });
    }
    Ok(())
}