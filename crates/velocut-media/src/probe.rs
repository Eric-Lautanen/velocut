// crates/velocut-media/src/probe.rs
//
// In-process FFmpeg probing: duration, video dimensions, thumbnail extraction.

use std::path::PathBuf;
use crossbeam_channel::Sender;
use uuid::Uuid;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as SwsContext, flag::Flags};

use velocut_core::media_types::MediaResult;

pub fn probe_duration(path: &PathBuf, id: Uuid, tx: &Sender<MediaResult>) -> f64 {
    match input(path) {
        Ok(ctx) => {
            let dur = ctx.duration() as f64 / ffmpeg::ffi::AV_TIME_BASE as f64;
            if dur > 0.0 {
                eprintln!("[media] duration {dur:.2}s ← {}", path.display());
                let _ = tx.send(MediaResult::Duration { id, seconds: dur });
                return dur;
            }
            // Fall back to stream duration
            if let Some(stream) = ctx.streams().best(Type::Video)
                .or_else(|| ctx.streams().best(Type::Audio))
            {
                let tb = stream.time_base();
                let d  = stream.duration() as f64 * tb.numerator() as f64
                    / tb.denominator() as f64;
                if d > 0.0 {
                    let _ = tx.send(MediaResult::Duration { id, seconds: d });
                    return d;
                }
            }
            let _ = tx.send(MediaResult::Error { id, msg: "duration unknown".into() });
            0.0
        }
        Err(e) => {
            eprintln!("[media] probe_duration open failed: {e}");
            let _ = tx.send(MediaResult::Error { id, msg: e.to_string() });
            0.0
        }
    }
}

/// Probes video stream dimensions and extracts a thumbnail frame in one pass.
pub fn probe_video_size_and_thumbnail(
    path:     &PathBuf,
    id:       Uuid,
    duration: f64,
    tx:       &Sender<MediaResult>,
) {
    let Ok(mut ictx) = input(path) else { return };

    let video_stream_idx = match ictx.streams().best(Type::Video) {
        Some(s) => s.index(),
        None    => return, // audio-only file
    };

    let (raw_w, raw_h, seek_ts) = {
        let stream = ictx.stream(video_stream_idx).unwrap();
        let (w, h) = unsafe {
            let p = stream.parameters().as_ptr();
            ((*p).width as u32, (*p).height as u32)
        };
        let ts = if duration > 2.0 {
            let t  = (duration * 0.1).max(1.0);
            let tb = stream.time_base();
            (t * tb.denominator() as f64 / tb.numerator() as f64) as i64
        } else {
            0i64
        };
        (w, h, ts)
    };

    if raw_w > 0 && raw_h > 0 {
        eprintln!("[media] video size {raw_w}x{raw_h} ← {}", path.display());
        let _ = tx.send(MediaResult::VideoSize { id, width: raw_w, height: raw_h });
    }

    let _ = ictx.seek(seek_ts, ..=seek_ts);

    // Open a second context to build the decoder (avoids borrow-after-seek conflict).
    let Ok(ictx2) = input(path) else { return };
    let context = match ictx2.stream(video_stream_idx) {
        Some(s) => match ffmpeg::codec::context::Context::from_parameters(s.parameters()) {
            Ok(c)  => c,
            Err(e) => { eprintln!("[media] codec ctx: {e}"); return; }
        },
        None => return,
    };
    let mut decoder = context.decoder().video().unwrap();

    // Thumbnail output: 320 wide, proportional height
    let thumb_w: u32 = 320;
    let thumb_h: u32 = ((thumb_w as f64 * raw_h as f64 / raw_w.max(1) as f64) as u32)
        .max(2) & !1; // must be even

    let mut scaler = match SwsContext::get(
        decoder.format(), decoder.width(), decoder.height(),
        Pixel::RGBA, thumb_w, thumb_h, Flags::BILINEAR,
    ) {
        Ok(s)  => s,
        Err(e) => { eprintln!("[media] thumbnail scaler: {e}"); return; }
    };

    let mut found = false;
    'outer: for (stream, packet) in ictx.packets().flatten() {
        if stream.index() != video_stream_idx { continue; }
        if decoder.send_packet(&packet).is_err() { continue; }
        let mut decoded = ffmpeg::util::frame::video::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            let mut rgb_frame = ffmpeg::util::frame::video::Video::empty();
            if scaler.run(&decoded, &mut rgb_frame).is_err() { continue; }
            // Destripe: copy only visible pixels, not stride padding
            let stride = rgb_frame.stride(0);
            let raw    = rgb_frame.data(0);
            let row_bytes = thumb_w as usize * 4;
            let data: Vec<u8> = (0..thumb_h as usize)
                .flat_map(|row| &raw[row * stride..row * stride + row_bytes])
                .copied()
                .collect();
            eprintln!("[media] thumbnail {}x{} ← {}", thumb_w, thumb_h, path.display());
            let _ = tx.send(MediaResult::Thumbnail { id, width: thumb_w, height: thumb_h, data });
            found = true;
            break 'outer;
        }
    }
    if !found {
        eprintln!("[media] thumbnail: no frame decoded for {}", path.display());
    }
}