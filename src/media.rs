// src/media.rs
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Condvar, atomic::{AtomicBool, Ordering}};
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use uuid::Uuid;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::{input, Pixel};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as SwsContext, flag::Flags};

// ── Result types ─────────────────────────────────────────────────────────────

pub enum MediaResult {
    Duration   { id: Uuid, seconds: f64 },
    Thumbnail  { id: Uuid, width: u32, height: u32, data: Vec<u8> },
    Waveform   { id: Uuid, peaks: Vec<f32> },
    VideoFrame { id: Uuid, width: u32, height: u32, data: Vec<u8> },
    VideoSize  { id: Uuid, width: u32, height: u32 },
    FrameSaved { path: PathBuf },
    AudioPath  { id: Uuid, path: PathBuf },
    Error      { id: Uuid, msg: String },
}

// ── Frame request (latest-wins) ───────────────────────────────────────────────

struct FrameRequest {
    id:        Uuid,
    path:      PathBuf,
    timestamp: f64,
    aspect:    f32,
}

// ── Stateful per-clip decoder (avoids re-open/seek every frame) ──────────────

struct LiveDecoder {
    path:      PathBuf,
    ictx:      ffmpeg::format::context::Input,
    decoder:   ffmpeg::decoder::video::Video,
    video_idx: usize,
    last_pts:  i64,
    tb_num:    i32,
    tb_den:    i32,
    out_w:     u32,
    out_h:     u32,
    scaler:    SwsContext,
}

impl LiveDecoder {
    fn open(path: &PathBuf, timestamp: f64, aspect: f32) -> anyhow::Result<Self> {
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

    fn ts_to_pts(&self, t: f64) -> i64 {
        (t * self.tb_den as f64 / self.tb_num as f64) as i64
    }

    /// Read forward until we find a frame at or past `target_pts`. Returns RGBA pixels.
    fn advance_to(&mut self, target_pts: i64) -> Option<(Vec<u8>, u32, u32)> {
        for (stream, packet) in self.ictx.packets().flatten() {
            if stream.index() != self.video_idx { continue; }
            if self.decoder.send_packet(&packet).is_err() { continue; }
            let mut decoded = ffmpeg::util::frame::video::Video::empty();
            while self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(self.last_pts + 1);
                self.last_pts = pts;
                if pts < target_pts { continue; }
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
                return Some((data, self.out_w, self.out_h));
            }
        }
        None
    }
}

// ── Worker ────────────────────────────────────────────────────────────────────

pub struct MediaWorker {
    pub rx:    Receiver<MediaResult>,
    tx:        Sender<MediaResult>,
    /// Latest-wins slot: sender always overwrites, decoder always takes newest.
    frame_req: Arc<(Mutex<Option<FrameRequest>>, Condvar)>,
    shutdown:  Arc<AtomicBool>,
}

impl MediaWorker {
    pub fn new() -> Self {
        let (tx, rx) = bounded(512);
        let frame_req: Arc<(Mutex<Option<FrameRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));

        let result_tx = tx.clone();
        let slot      = Arc::clone(&frame_req);
        thread::spawn(move || {
            let mut live: Option<LiveDecoder> = None;
            loop {
                // Block until a request is available.
                let req = {
                    let (lock, cvar) = &*slot;
                    let mut guard = lock.lock().unwrap();
                    while guard.is_none() {
                        guard = cvar.wait(guard).unwrap();
                    }
                    guard.take().unwrap()
                };

                // Reuse decoder if same file and timestamp is ahead; otherwise re-open + seek.
                let needs_reset = live.as_ref().map(|d| {
                    d.path != req.path
                        || d.ts_to_pts(req.timestamp) < d.last_pts - d.tb_den as i64
                }).unwrap_or(true);

                if needs_reset {
                    match LiveDecoder::open(&req.path, req.timestamp, req.aspect) {
                        Ok(mut d) => {
                            let tpts = d.ts_to_pts(req.timestamp);
                            if let Some((data, w, h)) = d.advance_to(tpts) {
                                let _ = result_tx.send(MediaResult::VideoFrame {
                                    id: req.id, width: w, height: h, data,
                                });
                            }
                            live = Some(d);
                        }
                        Err(e) => eprintln!("[media] LiveDecoder::open: {e}"),
                    }
                } else if let Some(d) = &mut live {
                    let tpts = d.ts_to_pts(req.timestamp);
                    if let Some((data, w, h)) = d.advance_to(tpts) {
                        let _ = result_tx.send(MediaResult::VideoFrame {
                            id: req.id, width: w, height: h, data,
                        });
                    }
                }
            }
        });

        Self { rx, tx, frame_req, shutdown: Arc::new(AtomicBool::new(false)) }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn probe_clip(&self, id: Uuid, path: PathBuf) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        thread::spawn(move || {
            if sd.load(Ordering::Relaxed) { return; }
            let dur = probe_duration(&path, id, &tx);
            if sd.load(Ordering::Relaxed) { return; }
            probe_video_size_and_thumbnail(&path, id, dur, &tx);
            if sd.load(Ordering::Relaxed) { return; }
            extract_waveform(&path, id, &tx);
            if sd.load(Ordering::Relaxed) { return; }
            if dur > 0.0 {
                extract_audio(&path, id, &tx);
            }
        });
    }

    /// No-op for compat — thumbnails now come back via probe_clip as RGBA data.
    /// Called on restore from saved state where thumbnail_path was persisted.
    /// We just re-probe instead.
    pub fn reload_thumbnail(&self, id: Uuid, path: PathBuf) {
        self.probe_clip(id, path);
    }

    pub fn request_frame(&self, id: Uuid, path: PathBuf, timestamp: f64, aspect: f32) {
        // Overwrite any pending request — the decode thread always gets the freshest one.
        let (lock, cvar) = &*self.frame_req;
        *lock.lock().unwrap() = Some(FrameRequest { id, path, timestamp, aspect });
        cvar.notify_one();
    }

    pub fn save_frame_with_dialog(&self, path: PathBuf, timestamp: f64) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        thread::spawn(move || {
            if sd.load(Ordering::Relaxed) { return; }
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let ts_label = format!("{timestamp:.3}").replace('.', "_");
            let default_name = format!("{stem}_t{ts_label}.png");
            let dest = match rfd::FileDialog::new()
                .set_file_name(&default_name)
                .add_filter("PNG", &["png"])
                .save_file()
            {
                Some(p) => p,
                None    => return,
            };
            if let Err(e) = decode_frame(
                &path, Uuid::nil(), timestamp, 0.0, true, Some(dest), &tx,
            ) {
                eprintln!("[media] save_frame_with_dialog: {e}");
            }
        });
    }

    pub fn extract_frame_hq(&self, _id: Uuid, path: PathBuf, timestamp: f64, dest: PathBuf) {
        let tx = self.tx.clone();
        let sd = self.shutdown.clone();
        thread::spawn(move || {
            if sd.load(Ordering::Relaxed) { return; }
            if let Err(e) = decode_frame(
                &path, Uuid::nil(), timestamp, 0.0, true, Some(dest), &tx,
            ) {
                eprintln!("[media] extract_frame_hq: {e}");
            }
        });
    }
}

// ── In-process probing ────────────────────────────────────────────────────────

fn probe_duration(path: &PathBuf, id: Uuid, tx: &Sender<MediaResult>) -> f64 {
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
fn probe_video_size_and_thumbnail(
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
        Pixel::RGBA,
        thumb_w, thumb_h,
        Flags::BILINEAR,
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
            let data = rgb_frame.data(0).to_vec();
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

// ── Frame decode (preview + save) ────────────────────────────────────────────

fn decode_frame(
    path:      &PathBuf,
    id:        Uuid,
    timestamp: f64,
    aspect:    f32,     // used for preview sizing; 0.0 = use native
    save_png:  bool,    // true = write PNG to dest, false = send VideoFrame
    dest:      Option<PathBuf>,
    tx:        &Sender<MediaResult>,
) -> anyhow::Result<()> {
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

    for (stream, packet) in ictx.packets().flatten() {
        if stream.index() != video_stream_idx { continue; }
        decoder.send_packet(&packet)?;
        let mut decoded = ffmpeg::util::frame::video::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            // Skip frames that landed before our target due to keyframe-aligned seek.
            if let Some(pts) = decoded.pts() {
                if pts + 2 < seek_ts { continue; }
            }
            let mut out_frame = ffmpeg::util::frame::video::Video::empty();
            scaler.run(&decoded, &mut out_frame)?;

            if save_png {
                use std::io::BufWriter;
                let dest_path = dest.clone()
                    .ok_or_else(|| anyhow::anyhow!("no dest path for PNG save"))?;
                let stride = out_frame.stride(0);
                let raw    = out_frame.data(0);
                let file   = std::fs::File::create(&dest_path)?;
                let w      = &mut BufWriter::new(file);
                let mut encoder = png::Encoder::new(w, out_w, out_h);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header()?;
                // Write row-by-row to avoid a destripe allocation
                let row_bytes = out_w as usize * 3;
                let rows: Vec<&[u8]> = (0..out_h as usize)
                    .map(|row| &raw[row * stride..row * stride + row_bytes])
                    .collect();
                writer.write_image_data(&rows.concat())?;
                eprintln!("[media] PNG saved → {}", dest_path.display());
                let _ = tx.send(MediaResult::FrameSaved { path: dest_path });
            } else {
                // RGBA preview frame — destripe
                let stride = out_frame.stride(0);
                let raw    = out_frame.data(0);
                let data: Vec<u8> = (0..out_h as usize)
                    .flat_map(|row| {
                        let start = row * stride;
                        &raw[start..start + out_w as usize * 4]
                    })
                    .copied()
                    .collect();
                let _ = tx.send(MediaResult::VideoFrame { id, width: out_w, height: out_h, data });
            }
            return Ok(());
        }
    }
    Err(anyhow::anyhow!("no frame found at t={timestamp:.3}"))
}

// ── Waveform ──────────────────────────────────────────────────────────────────

const WAVEFORM_COLS: usize = 1000;

fn extract_waveform(path: &PathBuf, id: Uuid, tx: &Sender<MediaResult>) {
    // Pipe raw mono f32 samples at 2 kHz from ffmpeg — simple and codec-agnostic.
    let result = std::process::Command::new("ffmpeg")
        .args([
            "-i",  path.to_string_lossy().as_ref(),
            "-vn",
            "-acodec", "pcm_f32le",
            "-ar", "2000",
            "-ac", "1",
            "-f",  "f32le",
            "pipe:1",
        ])
        .output();

    let samples: Vec<f32> = match result {
        Ok(out) if out.status.success() => out.stdout
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).clamp(-1.0, 1.0))
            .collect(),
        Ok(out) => {
            eprintln!("[media] waveform ffmpeg failed: {}",
                String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
            return;
        }
        Err(e) => { eprintln!("[media] waveform spawn: {e}"); return; }
    };

    if samples.is_empty() {
        eprintln!("[media] waveform: no samples for {}", path.display());
        return;
    }

    let block = (samples.len() / WAVEFORM_COLS).max(1);
    let peaks: Vec<f32> = samples.chunks(block).take(WAVEFORM_COLS)
        .map(|chunk| chunk.iter().map(|s| s.abs()).fold(0.0f32, f32::max))
        .collect();

    eprintln!("[media] waveform {} peaks ← {}", peaks.len(), path.display());
    let _ = tx.send(MediaResult::Waveform { id, peaks });
}

// ── Audio extraction (WAV for rodio playback) ─────────────────────────────────

fn extract_audio(path: &PathBuf, id: Uuid, tx: &Sender<MediaResult>) {
    let wav_path = std::env::temp_dir().join(format!("velocut_audio_{id}.wav"));

    // Use the ffmpeg CLI — handles every codec correctly with no resampler fiddling.
    let result = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-i",  path.to_string_lossy().as_ref(),
            "-vn",
            "-acodec", "pcm_f32le",
            "-ar", "44100",
            "-ac", "2",
            wav_path.to_string_lossy().as_ref(),
        ])
        .output();

    match result {
        Ok(out) if out.status.success() => {
            let bytes = std::fs::metadata(&wav_path).map(|m| m.len()).unwrap_or(0);
            eprintln!("[media] audio WAV written ({bytes} bytes PCM) ← {}", path.display());
            let _ = tx.send(MediaResult::AudioPath { id, path: wav_path });
        }
        Ok(out) => {
            eprintln!("[media] ffmpeg audio extract failed: {}",
                String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""));
        }
        Err(e) => eprintln!("[media] ffmpeg spawn failed: {e}"),
    }
}