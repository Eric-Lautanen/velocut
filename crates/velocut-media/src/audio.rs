// crates/velocut-media/src/audio.rs
//
// Audio extraction (WAV for rodio playback) and temp file cleanup.
//
// Previously this module spawned an external `ffmpeg` CLI subprocess, which
// silently failed when VeloCut was launched by double-clicking the .exe because
// Windows does not inherit the MSYS2 PATH entries where ffmpeg.exe lives.
//
// Rewritten to use the statically-linked ffmpeg-the-third (same as waveform.rs,
// decode.rs, encode.rs).  No child process, no PATH dependency, works identically
// in every launch mode.

use std::io::Write;
use std::path::PathBuf;
use crossbeam_channel::Sender;
use uuid::Uuid;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::input;
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling;
use ffmpeg::util::channel_layout::ChannelLayout;
use ffmpeg::util::frame::audio::Audio as AudioFrame;

use velocut_core::media_types::MediaResult;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Output sample rate for extracted WAV files.  Matches the rodio sink rate and
/// the AAC encoder rate used in encode.rs.
const OUT_RATE: u32 = 44_100;

/// Output format: packed (interleaved) f32 le.  rodio / symphonia expects
/// interleaved stereo, not planar.  WAV format tag 3 = IEEE_FLOAT.
const OUT_FMT: Sample = Sample::F32(SampleType::Packed);

/// Output channel layout: stereo.
const OUT_LAYOUT: ChannelLayout = ChannelLayout::STEREO;

// ── Public API ────────────────────────────────────────────────────────────────

/// Decode audio from `path`, restricted to `[source_offset, source_offset + duration)`,
/// resample to 44100 Hz stereo f32le, write a WAV temp file, and send the path
/// back via `tx` as `MediaResult::AudioPath`.
///
/// Pass `source_offset = 0.0` and `duration = f64::MAX` to decode the full file
/// (used by the probe pipeline for clips that are not audio-overlay trimmed).
///
/// Soft-fails on any error (logs via eprintln, sends nothing on tx) so the UI
/// degrades gracefully to silence rather than crashing.
pub fn extract_audio(path: &PathBuf, id: Uuid, source_offset: f64, duration: f64, tx: &Sender<MediaResult>) {
    let wav_path = std::env::temp_dir().join(format!("velocut_audio_{id}.wav"));

    match decode_to_wav(path, &wav_path, source_offset, duration) {
        Ok(bytes) => {
            eprintln!("[media] audio WAV written ({bytes} bytes PCM) ← {}", path.display());
            let _ = tx.send(MediaResult::AudioPath { id, path: wav_path, trimmed_offset: source_offset });
        }
        Err(e) => {
            eprintln!("[media] audio extract failed for '{}': {e}", path.display());
        }
    }
}

/// Delete a temp WAV that was extracted for a clip.
/// Only deletes files matching the `velocut_audio_<uuid>.wav` pattern in the OS temp dir.
pub fn cleanup_audio_temp(path: &std::path::Path) {
    let in_temp = path.parent()
        .map(|p| p == std::env::temp_dir())
        .unwrap_or(false);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if in_temp && name.starts_with("velocut_audio_") && name.ends_with(".wav") {
        if let Err(e) = std::fs::remove_file(path) {
            eprintln!("[media] cleanup_audio_temp: {e}");
        } else {
            eprintln!("[media] cleaned up temp WAV: {}", path.display());
        }
    }
}

// ── Internal implementation ───────────────────────────────────────────────────

/// Decode all audio from `src` within `[source_offset, source_offset + duration)`,
/// resample to `OUT_RATE`/stereo/f32le, and write a WAV file to `dst` by streaming
/// samples directly from the resampler to disk.
///
/// Pass `source_offset = 0.0` and `duration = f64::MAX` to decode the full file.
fn decode_to_wav(src: &PathBuf, dst: &PathBuf, source_offset: f64, duration: f64) -> Result<u64, String> {
    use std::io::{Seek, SeekFrom};

    // ── Open input and find audio stream ─────────────────────────────────────
    let mut ictx = input(src).map_err(|e| format!("open: {e}"))?;

    let audio_stream_idx = ictx
        .streams()
        .best(MediaType::Audio)
        .ok_or_else(|| "no audio stream".to_string())?
        .index();

    // ── Build decoder ─────────────────────────────────────────────────────────
    let stream = ictx.stream(audio_stream_idx).unwrap();
    let in_tb  = stream.time_base();
    let dec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| format!("codec context: {e}"))?;
    let mut decoder = dec_ctx.decoder().audio()
        .map_err(|e| format!("audio decoder: {e}"))?;

    // Seek to source_offset so we don't decode the leading portion of the file.
    // Guard against ts=0 (Windows EPERM on seek-to-start).
    let clip_end = if duration >= f64::MAX / 2.0 { f64::MAX } else { source_offset + duration };
    if source_offset > 0.0 {
        let seek_ts = (source_offset * in_tb.denominator() as f64 / in_tb.numerator() as f64) as i64;
        let _ = ictx.seek(seek_ts, ..=seek_ts);
    }

    // ── Open output file and write WAV header with placeholder sizes ──────────
    const CHANNELS:     u16 = 2;
    const BITS:         u16 = 32;
    const FORMAT_FLOAT: u16 = 3;   // IEEE_FLOAT
    const BLOCK_ALIGN:  u16 = CHANNELS * (BITS / 8); // 8

    let byte_rate = OUT_RATE * BLOCK_ALIGN as u32;

    let mut file = std::fs::File::create(dst).map_err(|e| format!("create WAV: {e}"))?;
    let mut w    = std::io::BufWriter::new(&mut file);

    // RIFF header — data_size placeholder = 0; we'll seek back to fix it.
    w.write_all(b"RIFF").map_err(|e| e.to_string())?;
    let riff_size_offset = 4u64;  // byte offset of the RIFF chunk size field
    w.write_all(&0u32.to_le_bytes()).map_err(|e| e.to_string())?;  // placeholder
    w.write_all(b"WAVE").map_err(|e| e.to_string())?;

    // fmt  chunk
    w.write_all(b"fmt ").map_err(|e| e.to_string())?;
    w.write_all(&16u32.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&FORMAT_FLOAT.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&CHANNELS.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&OUT_RATE.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&byte_rate.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&BLOCK_ALIGN.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&BITS.to_le_bytes()).map_err(|e| e.to_string())?;

    // data chunk header — size placeholder; offset recorded for fixup.
    w.write_all(b"data").map_err(|e| e.to_string())?;
    let data_size_offset = 40u64;  // byte offset of the data chunk size field
    w.write_all(&0u32.to_le_bytes()).map_err(|e| e.to_string())?;  // placeholder

    // ── Stream resampled frames directly to disk ───────────────────────────────
    let mut resampler: Option<resampling::Context> = None;
    let mut data_bytes: u64 = 0;

    let write_frame = |frame: &AudioFrame,
                           resampler: &mut Option<resampling::Context>,
                           w: &mut std::io::BufWriter<&mut std::fs::File>,
                           data_bytes: &mut u64,
                           pts_secs: f64| -> Result<bool, String> {
        // Skip frames entirely before the pre-roll window.
        if pts_secs < source_offset - 0.05 { return Ok(false); }
        // Signal caller to stop when we've passed clip_end.
        if pts_secs >= clip_end { return Ok(true); }

        // Trim pre-roll: after a keyframe-aligned seek the first decoded frame
        // may start before source_offset.  Writing those samples shifts playback
        // audio early — same bug as encode.rs.  Only write samples from
        // source_offset onwards (decode.rs skips video frames the same way).
        let pre_roll_samples = ((source_offset - pts_secs).max(0.0) * OUT_RATE as f64)
            .round() as usize;

        let src_channels   = frame.ch_layout().channels();
        let needs_resample = frame.format() != OUT_FMT
            || frame.rate()                != OUT_RATE
            || src_channels                != 2;

        if needs_resample {
            let rs = resampler.get_or_insert_with(|| {
                let src_layout = if src_channels >= 2 {
                    frame.ch_layout()
                } else {
                    ChannelLayout::MONO
                };
                resampling::Context::get2(
                    frame.format(), src_layout,  frame.rate(),
                    OUT_FMT,        OUT_LAYOUT,  OUT_RATE,
                ).expect("create audio resampler for WAV extraction")
            });

            let mut resampled = AudioFrame::empty();
            if rs.run(frame, &mut resampled).is_ok() && resampled.samples() > pre_roll_samples {
                // OUT_FMT is packed interleaved f32: each sample is 2 channels × 4 bytes = 8 bytes.
                let skip_bytes = pre_roll_samples * 2 * 4;
                let data = &resampled.data(0)[skip_bytes..];
                w.write_all(data).map_err(|e| format!("write WAV samples: {e}"))?;
                *data_bytes += data.len() as u64;
            }
        } else {
            let skip_bytes = pre_roll_samples * 2 * 4;
            let data = &frame.data(0)[skip_bytes..];
            w.write_all(data).map_err(|e| format!("write WAV samples: {e}"))?;
            *data_bytes += data.len() as u64;
        }
        Ok(false)
    };

    'packets: for result in ictx.packets() {
        let (stream, packet) = match result {
            Ok(p)  => p,
            Err(_) => continue,
        };
        if stream.index() != audio_stream_idx { continue; }
        if decoder.send_packet(&packet).is_err() { continue; }

        let mut frame = AudioFrame::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            let pts_secs = frame.pts()
                .map(|p| p as f64 * in_tb.numerator() as f64 / in_tb.denominator() as f64)
                .unwrap_or(source_offset);
            if write_frame(&frame, &mut resampler, &mut w, &mut data_bytes, pts_secs)? {
                break 'packets;
            }
        }
    }

    // Flush decoder
    let _ = decoder.send_eof();
    let mut frame = AudioFrame::empty();
    while decoder.receive_frame(&mut frame).is_ok() {
        let pts_secs = frame.pts()
            .map(|p| p as f64 * in_tb.numerator() as f64 / in_tb.denominator() as f64)
            .unwrap_or(source_offset);
        if write_frame(&frame, &mut resampler, &mut w, &mut data_bytes, pts_secs)? {
            break;
        }
    }

    if data_bytes == 0 {
        return Err("no audio samples decoded".into());
    }

    // ── Seek back and fix the two placeholder size fields ─────────────────────
    w.flush().map_err(|e| format!("flush WAV: {e}"))?;
    drop(w); // release BufWriter borrow so we can seek on `file` directly

    let riff_size = (36 + data_bytes) as u32;  // total file size − 8
    file.seek(SeekFrom::Start(riff_size_offset))
        .and_then(|_| file.write_all(&riff_size.to_le_bytes()))
        .map_err(|e| format!("fixup RIFF size: {e}"))?;

    let data_size = data_bytes as u32;
    file.seek(SeekFrom::Start(data_size_offset))
        .and_then(|_| file.write_all(&data_size.to_le_bytes()))
        .map_err(|e| format!("fixup data size: {e}"))?;

    Ok(44 + data_bytes)
}