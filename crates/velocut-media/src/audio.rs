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

/// Decode audio from `path`, resample to 44100 Hz stereo f32le, write a WAV
/// temp file, and send the path back via `tx` as `MediaResult::AudioPath`.
///
/// Soft-fails on any error (logs via eprintln, sends nothing on tx) so the UI
/// degrades gracefully to silence rather than crashing.
pub fn extract_audio(path: &PathBuf, id: Uuid, tx: &Sender<MediaResult>) {
    let wav_path = std::env::temp_dir().join(format!("velocut_audio_{id}.wav"));

    match decode_to_wav(path, &wav_path) {
        Ok(bytes) => {
            eprintln!("[media] audio WAV written ({bytes} bytes PCM) ← {}", path.display());
            let _ = tx.send(MediaResult::AudioPath { id, path: wav_path });
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

/// Decode all audio from `src`, resample to `OUT_RATE`/stereo/f32le, and write
/// a WAV file to `dst`.  Returns the total number of bytes written on success.
fn decode_to_wav(src: &PathBuf, dst: &PathBuf) -> Result<u64, String> {
    // ── Open input and find audio stream ─────────────────────────────────────
    let mut ictx = input(src).map_err(|e| format!("open: {e}"))?;

    let audio_stream_idx = ictx
        .streams()
        .best(MediaType::Audio)
        .ok_or_else(|| "no audio stream".to_string())?
        .index();

    // ── Build decoder ─────────────────────────────────────────────────────────
    let stream = ictx.stream(audio_stream_idx).unwrap();
    let dec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| format!("codec context: {e}"))?;
    let mut decoder = dec_ctx.decoder().audio()
        .map_err(|e| format!("audio decoder: {e}"))?;

    // ── Collect resampled f32le interleaved samples ───────────────────────────
    // The resampler is built lazily on the first decoded frame so we know the
    // real source format/layout/rate before constructing the SwrContext.
    // (Same pattern as encode.rs encode_clip().)
    let mut resampler: Option<resampling::Context> = None;
    let mut pcm: Vec<f32> = Vec::new();

    for result in ictx.packets() {
        let (stream, packet) = match result {
            Ok(p)  => p,
            Err(_) => continue,
        };
        if stream.index() != audio_stream_idx { continue; }
        if decoder.send_packet(&packet).is_err() { continue; }

        let mut frame = AudioFrame::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            append_resampled(&frame, &mut resampler, &mut pcm)?;
        }
    }

    // Flush decoder
    let _ = decoder.send_eof();
    let mut frame = AudioFrame::empty();
    while decoder.receive_frame(&mut frame).is_ok() {
        append_resampled(&frame, &mut resampler, &mut pcm)?;
    }

    if pcm.is_empty() {
        return Err("no audio samples decoded".into());
    }

    // ── Write WAV file ────────────────────────────────────────────────────────
    let bytes = write_wav(dst, &pcm).map_err(|e| format!("write WAV: {e}"))?;
    Ok(bytes)
}

/// Resample `frame` to OUT_FMT/OUT_LAYOUT/OUT_RATE and append the resulting
/// interleaved f32 samples to `out`.  Builds `resampler` on first call.
fn append_resampled(
    frame:     &AudioFrame,
    resampler: &mut Option<resampling::Context>,
    out:       &mut Vec<f32>,
) -> Result<(), String> {
    // Check whether resampling is actually needed.
    let src_channels = frame.ch_layout().channels();
    let needs_resample = frame.format() != OUT_FMT
        || frame.rate()                != OUT_RATE
        || src_channels                != 2;

    if needs_resample {
        // Build resampler lazily on the first frame that needs it.
        // Mirrors encode.rs: mono sources must be declared as MONO so swr
        // doesn't misinterpret the channel count.
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
        if rs.run(frame, &mut resampled).is_ok() && resampled.samples() > 0 {
            append_packed_f32(&resampled, out);
        }
    } else {
        // Source is already the right format — copy directly.
        append_packed_f32(frame, out);
    }

    Ok(())
}

/// Copy the packed f32 samples from `frame` into `out`.
/// OUT_FMT is Packed (interleaved), so all channel data is in plane 0.
fn append_packed_f32(frame: &AudioFrame, out: &mut Vec<f32>) {
    let data = frame.data(0);
    out.extend(
        data.chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
    );
}

/// Write interleaved stereo f32le PCM to a WAV file at `path`.
/// Returns total bytes written (header + data).
///
/// WAV layout:
///   RIFF  <file_size - 8>  WAVE
///   fmt   16  <format=3 IEEE_FLOAT>  <channels=2>  <rate=44100>
///             <byte_rate=352800>  <block_align=8>  <bits=32>
///   data  <data_size>  <samples…>
fn write_wav(path: &PathBuf, samples: &[f32]) -> std::io::Result<u64> {
    const CHANNELS:       u16 = 2;
    const BITS:           u16 = 32;
    const FORMAT_FLOAT:   u16 = 3;   // IEEE_FLOAT
    const BLOCK_ALIGN:    u16 = CHANNELS * (BITS / 8); // 8

    let data_size  = (samples.len() * 4) as u32;
    let byte_rate  = OUT_RATE * BLOCK_ALIGN as u32;

    let mut file = std::fs::File::create(path)?;
    let mut w    = std::io::BufWriter::new(&mut file);

    // RIFF header
    w.write_all(b"RIFF")?;
    w.write_all(&(36u32 + data_size).to_le_bytes())?;
    w.write_all(b"WAVE")?;

    // fmt  chunk
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;          // chunk size
    w.write_all(&FORMAT_FLOAT.to_le_bytes())?;
    w.write_all(&CHANNELS.to_le_bytes())?;
    w.write_all(&OUT_RATE.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&BLOCK_ALIGN.to_le_bytes())?;
    w.write_all(&BITS.to_le_bytes())?;

    // data chunk
    w.write_all(b"data")?;
    w.write_all(&data_size.to_le_bytes())?;
    for s in samples {
        w.write_all(&s.to_le_bytes())?;
    }
    w.flush()?;

    Ok((44 + data_size) as u64)
}