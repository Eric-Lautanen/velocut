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
    pub path:           PathBuf,
    pub ictx:           ffmpeg::format::context::Input,
    pub decoder:        ffmpeg::decoder::video::Video,
    pub video_idx:      usize,
    pub last_pts:       i64,
    pub tb_num:         i32,
    pub tb_den:         i32,
    pub out_w:          u32,
    pub out_h:          u32,
    pub scaler:         SwsContext,
    /// Source decoder format + dimensions — used as the cache key when the scrub
    /// thread tries to reuse a SwsContext across a LiveDecoder reset.  SwsContext
    /// only needs to be re-created when these change (different camera/format).
    pub decoder_fmt:    Pixel,
    pub decoder_w:      u32,
    pub decoder_h:      u32,
    /// If non-zero, next_frame() skips (decode-only, no scale/alloc) all frames
    /// whose PTS is below this threshold, then clears the field.
    /// Used to burn through the GOP after a keyframe-aligned seek without blocking
    /// the thread on advance_to() — which scales every skipped frame needlessly.
    pub skip_until_pts: i64,

    /// [Opt 1] Reusable RGBA output buffer — avoids a heap allocation per frame.
    /// Capacity is pre-sized to out_w * out_h * 4 at construction and maintained
    /// across calls. Each call: clear() + extend_from_slice per row (bulk memcpy),
    /// then clone() once to hand off to the caller.  Saves the flat_map().collect()
    /// iterator overhead and guarantees no mid-frame realloc.
    frame_buf: Vec<u8>,
}

impl LiveDecoder {
    /// Open a decoder at `timestamp` seconds.
    ///
    /// `cached_scaler` — if the caller holds a `SwsContext` from a previous
    /// `LiveDecoder` (same scrub thread, different clip), pass it here with
    /// its source key `(fmt, w, h)`.  If the key matches the new clip's codec
    /// parameters the context is reused and `SwsContext::get` is skipped,
    /// saving the internal lookup-table initialisation that dominates
    /// construction cost.
    pub fn open(
        path:          &PathBuf,
        timestamp:     f64,
        aspect:        f32,
        cached_scaler: Option<(SwsContext, Pixel, u32, u32)>,
    ) -> Result<Self> {
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

        let dec_fmt = decoder.format();
        let dec_w   = decoder.width();
        let dec_h   = decoder.height();

        // [Opt #1] Reuse cached SwsContext when source format/dimensions haven't
        // changed — avoids re-running lookup-table initialisation on every
        // backward scrub or cross-clip reset.  Out dimensions are fixed for the
        // lifetime of a session (same `aspect` is always passed), so a matching
        // source key is sufficient to guarantee safe reuse.
        let scaler = match cached_scaler {
            Some((sws, cf, cw, ch)) if cf == dec_fmt && cw == dec_w && ch == dec_h => sws,
            _ => SwsContext::get(dec_fmt, dec_w, dec_h, Pixel::RGBA, out_w, out_h, Flags::BILINEAR)?,
        };

        Ok(Self {
            path: path.clone(), ictx, decoder, video_idx,
            // seek_ts is where we ASKED to seek, not where FFmpeg actually landed.
            // The actual landing position is the nearest keyframe, which can be seconds
            // before seek_ts. Initialising last_pts to seek_ts - 1 ensures that
            // advance_to() fires correctly when called with target == seek_ts, since
            // the check is tpts > last_pts (strictly greater).
            last_pts: seek_ts.saturating_sub(1), tb_num, tb_den, out_w, out_h, scaler,
            decoder_fmt: dec_fmt, decoder_w: dec_w, decoder_h: dec_h,
            skip_until_pts: 0,
            // [Opt 1] Pre-allocate frame buffer at the exact output size.
            frame_buf: Vec::with_capacity(out_w as usize * out_h as usize * 4),
        })
    }

    pub fn ts_to_pts(&self, t: f64) -> i64 {
        (t * self.tb_den as f64 / self.tb_num as f64) as i64
    }

    pub fn pts_to_secs(&self, pts: i64) -> f64 {
        pts as f64 * self.tb_num as f64 / self.tb_den as f64
    }

    /// Decode the next frame sequentially (no seek). Returns `(pixels, w, h, ts_secs)` or None at EOF.
    ///
    /// If `skip_until_pts` is set (non-zero), frames before that PTS are decoded
    /// but not scaled — decode-only is ~4x faster than decode+scale+alloc, so this
    /// burns through a GOP in ~50 ms instead of ~200 ms.  Once the threshold is
    /// reached the field is cleared and normal decode+scale resumes.
    ///
    /// [Opt 1] RGBA output is assembled via extend_from_slice (bulk memcpy per row)
    /// into a reused self.frame_buf, then cloned once for the return value.  This
    /// avoids the flat_map().collect() iterator overhead and the implicit Vec growth
    /// that could occur when the size_hint is imprecise.
    pub fn next_frame(&mut self) -> Option<(Vec<u8>, u32, u32, f64)> {
        for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                // Burn-through: skip scaler for pre-target frames after a seek.
                // Avoids decode+scale+alloc for every GOP frame we don't need.
                if self.skip_until_pts > 0 && pts < self.skip_until_pts {
                    continue;
                }
                self.skip_until_pts = 0; // reached target — disable skip
                let ts_secs = self.pts_to_secs(pts);
                let mut out = ffmpeg::util::frame::video::Video::empty();
                if self.scaler.run(&decoded, &mut out).is_err() { return None; }
                let data = copy_frame_rgba(&mut self.frame_buf, &out, self.out_w, self.out_h);
                return Some((data, self.out_w, self.out_h, ts_secs));
            }
        }
        None
    }

    /// [Opt 2] Read forward until we find a frame at or past `target_pts`.
    ///
    /// Pre-target frames are now decoded-only (no swscale, no alloc) — identical to
    /// the burn_to_pts fast-path.  The scaler runs exactly once on the frame that
    /// meets or exceeds target_pts.  For a 5 s GOP at 60 fps that's ~300 frames
    /// decoded without scaling instead of the original 300 × (decode + swscale + alloc).
    ///
    /// Behaviour change vs original: if the stream reaches EOF before target_pts is
    /// found, None is returned (the caller keeps the last displayed frame).  The
    /// original returned the last *scaled* frame on EOF, but that path triggered a
    /// scaler alloc for every pre-target frame.  The EOF case (scrub past clip end)
    /// is benign — the scrub thread's needs_reset logic handles re-open on the next
    /// request.
    pub fn advance_to(&mut self, target_pts: i64) -> Option<(Vec<u8>, u32, u32)> {
        for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                // [Opt 2] Decode-only for all frames before the target PTS.
                // ~4x faster than decode+scale+alloc for the same set of frames.
                if pts < target_pts { continue; }
                // Target reached — scale exactly this one frame and return.
                let mut out = ffmpeg::util::frame::video::Video::empty();
                if self.scaler.run(&decoded, &mut out).is_err() { return None; }
                let data = copy_frame_rgba(&mut self.frame_buf, &out, self.out_w, self.out_h);
                return Some((data, self.out_w, self.out_h));
            }
        }
        // EOF before target_pts: return None. Caller retains its current frame.
        None
    }

    /// Decode and discard frames without scaling until `last_pts >= target_pts`.
    ///
    /// This is the fast seek used by the playback thread after a keyframe-aligned
    /// open().  Skipping the scaler+alloc makes it ~4-8x faster than advance_to().
    ///
    /// Runs synchronously — call this in the Start handler BEFORE entering the
    /// frame-send loop so the first frame the thread sends is at the correct
    /// position.  The caller is blocked for the duration but the channel is empty
    /// at this point, so the UI simply shows held_frame until playback begins.
    pub fn burn_to_pts(&mut self, target_pts: i64) {
        if target_pts <= 0 || target_pts <= self.last_pts { return; }
        'outer: for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                if pts >= target_pts { break 'outer; }
            }
        }
    }
}

// ── Frame copy helper ─────────────────────────────────────────────────────────

/// [Opt 1] Copy an RGBA-format ffmpeg VideoFrame into `buf`, stripping stride
/// padding, and return a clone of the filled buffer.
///
/// `buf` is reused across calls — it is only reallocated when `out_w * out_h * 4`
/// exceeds its current capacity (i.e. never in steady state for a fixed-resolution
/// source).  The returned Vec is a single allocation of the exact frame size.
#[inline]
fn copy_frame_rgba(
    buf:   &mut Vec<u8>,
    frame: &ffmpeg::util::frame::video::Video,
    out_w: u32,
    out_h: u32,
) -> Vec<u8> {
    let stride    = frame.stride(0);
    let raw       = frame.data(0);
    let row_bytes = out_w as usize * 4;
    let needed    = row_bytes * out_h as usize;

    buf.clear();
    // Reserve only triggers a realloc when dimensions change (e.g. first frame,
    // or aspect ratio change mid-session).
    if buf.capacity() < needed {
        buf.reserve(needed);
    }
    for row in 0..out_h as usize {
        let s = row * stride;
        buf.extend_from_slice(&raw[s..s + row_bytes]);
    }
    buf.clone()
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

    let (seek_ts, tb_num, tb_den) = {
        let stream = ictx.stream(video_stream_idx).unwrap();
        let tb     = stream.time_base();
        let ts     = (timestamp * tb.denominator() as f64 / tb.numerator() as f64) as i64;
        (ts, tb.numerator() as f64, tb.denominator() as f64)
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
            // Compare in seconds — pts+2 in raw units is timebase-dependent (22µs at
            // 1/90000 but 80ms at 1/25), which would incorrectly skip real frames.
            if let Some(pts) = decoded.pts() {
                let pts_secs = pts as f64 * tb_num / tb_den;
                if pts_secs < timestamp - (1.0 / 60.0) { continue; }
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