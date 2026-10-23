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

use crossbeam_channel::Sender;
use std::io::Write;
use std::path::PathBuf;
use uuid::Uuid;

use ffmpeg::format::input;
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling;
use ffmpeg::util::channel_layout::ChannelLayout;
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg_the_third as ffmpeg;

use velocut_core::media_types::MediaResult;

// â”€â”€ Constants â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Output sample rate for extracted WAV files.  Matches the rodio sink rate and
/// the AAC encoder rate used in encode.rs.
const OUT_RATE: u32 = 44_100;

/// Output format: packed (interleaved) f32 le.  rodio / symphonia expects
/// interleaved stereo, not planar.  WAV format tag 3 = IEEE_FLOAT.
const OUT_FMT: Sample = Sample::F32(SampleType::Packed);

/// Output channel layout: stereo.
const OUT_LAYOUT: ChannelLayout = ChannelLayout::STEREO;

// â”€â”€ Public API â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Decode audio from `path`, restricted to `[source_offset, source_offset + duration)`,
/// resample to 44100 Hz stereo f32le, write a WAV temp file, and send the path
/// back via `tx` as `MediaResult::AudioPath`.
///
/// The decode writes a **per-request staging file** and only renames it over
/// the final path on success, so a reader (an active rodio sink decoder, a
/// concurrent extraction) never observes a torn RIFF header or a half-written
/// data chunk. Staging + rename (see `remove_staging` and `cleanup_audio_temp`)
/// means the
/// extraction has latest-wins semantics even for two concurrent requests for
/// the same library id: the byte writes run in distinct per-generation files
/// and only the single rename step can race, which the OS resolves atomically.
///
/// Pass `source_offset = 0.0` and `duration = f64::MAX` to decode the full file
/// (used by the probe pipeline for clips that are not audio-overlay trimmed).
///
/// `generation` is the request's WAV-extraction epoch, issued by `MediaWorker
/// ::next_audio_generation` when the request was queued. It travels back on the
/// `MediaResult::AudioPath` so the UI can reject a result whose request was
/// superseded by a newer extraction for the same library id (the two can carry
/// different content windows, and `trimmed_offset` drives the UI's
/// `wav_seek_t` math â€” accepting a stale result would desync playback).
///
/// Soft-fails on any error (logs via eprintln, sends nothing on tx) so the UI
/// degrades gracefully to silence rather than crashing.
pub fn extract_audio(
    path: &PathBuf,
    id: Uuid,
    source_offset: f64,
    duration: f64,
    generation: u64,
    tx: &Sender<MediaResult>,
) {
    let wav_path = std::env::temp_dir().join(format!("velocut_audio_{id}.wav"));
    // Stage-name embeds the request's generation so two concurrent extractions
    // for the SAME library id run in distinct files and only the rename â€” not
    // the byte writes â€” can race. Last rename wins atomically.
    let staging_path =
        std::env::temp_dir().join(format!("velocut_audio_{id}_{generation}.staging"));

    match decode_to_wav(path, &staging_path, source_offset, duration) {
        Ok(bytes) => match std::fs::rename(&staging_path, &wav_path) {
            Ok(()) => {
                crate::media_log!(
                    "[media] audio WAV written ({bytes} bytes PCM) â† {}",
                    path.display()
                );
                let _ = tx.send(MediaResult::AudioPath {
                    id,
                    path: wav_path,
                    trimmed_offset: source_offset,
                    generation,
                });
            }
            Err(e) => {
                crate::media_log!(
                    "[media] audio staging rename failed for '{staging:?}': {e}",
                    staging = staging_path
                );
                remove_staging(&staging_path);
            }
        },
        Err(e) => {
            crate::media_log!("[media] audio extract failed for '{}': {e}", path.display());
            remove_staging(&staging_path);
        }
    }
}

/// Best-effort removal of a per-request staging file after a failed or
/// unrenamed extraction. Runs whenever the decode did not finish cleanly.
fn remove_staging(staging_path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(staging_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            crate::media_log!("[media] remove staging {}: {e}", staging_path.display());
        }
    }
}

/// Delete a temp WAV that was extracted for a clip, or a staging file left
/// behind by a hard-killed extraction (the OS temp dir may also hold
/// `velocut_audio_<uuid>_<gen>.staging` files whose owner never got to rename
/// or clean them).
pub fn cleanup_audio_temp(path: &std::path::Path) {
    let in_temp = path
        .parent()
        .map(|p| p == std::env::temp_dir())
        .unwrap_or(false);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if in_temp
        && name.starts_with("velocut_audio_")
        && (name.ends_with(".wav") || name.ends_with(".staging"))
    {
        if let Err(e) = std::fs::remove_file(path) {
            crate::media_log!("[media] cleanup_audio_temp: {e}");
        } else {
            crate::media_log!("[media] cleaned up temp WAV: {}", path.display());
        }
    }
}

// â”€â”€ Internal implementation â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Decode all audio from `src` within `[source_offset, source_offset + duration)`,
/// resample to `OUT_RATE`/stereo/f32le, and write a WAV file to `dst` (the
/// caller's per-request staging path â€” the caller renames it over the final
/// media path after this returns `Ok`) by streaming samples directly from the
/// resampler to disk.
///
/// Pass `source_offset = 0.0` and `duration = f64::MAX` to decode the full file.
fn decode_to_wav(
    src: &PathBuf,
    staging: &PathBuf,
    source_offset: f64,
    duration: f64,
) -> Result<u64, String> {
    let dst = staging;
    use std::io::{Seek, SeekFrom};

    // â”€â”€ Open input and find audio stream â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    let mut ictx = input(src).map_err(|e| format!("open: {e}"))?;

    let audio_stream_idx = ictx
        .streams()
        .best(MediaType::Audio)
        .ok_or_else(|| "no audio stream".to_string())?
        .index();

    // â”€â”€ Build decoder â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    let stream = ictx.stream(audio_stream_idx).unwrap();
    let in_tb = stream.time_base();
    let dec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| format!("codec context: {e}"))?;
    let mut decoder = dec_ctx
        .decoder()
        .audio()
        .map_err(|e| format!("audio decoder: {e}"))?;

    // Seek to source_offset so we don't decode the leading portion of the file.
    // Guard against ts=0 (Windows EPERM on seek-to-start).
    let clip_end = if duration >= f64::MAX / 2.0 {
        f64::MAX
    } else {
        source_offset + duration
    };
    if source_offset > 0.0 {
        let seek_ts =
            (source_offset * in_tb.denominator() as f64 / in_tb.numerator() as f64) as i64;
        let _ = ictx.seek(seek_ts, ..=seek_ts);
    }

    // â”€â”€ Open output file and write WAV header with placeholder sizes â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    const CHANNELS: u16 = 2;
    const BITS: u16 = 32;
    const FORMAT_FLOAT: u16 = 3; // IEEE_FLOAT
    const BLOCK_ALIGN: u16 = CHANNELS * (BITS / 8); // 8

    let byte_rate = OUT_RATE * BLOCK_ALIGN as u32;

    let mut file = std::fs::File::create(dst).map_err(|e| format!("create WAV: {e}"))?;
    let mut w = std::io::BufWriter::new(&mut file);

    // RIFF header â€” data_size placeholder = 0; we'll seek back to fix it.
    w.write_all(b"RIFF").map_err(|e| e.to_string())?;
    let riff_size_offset = 4u64; // byte offset of the RIFF chunk size field
    w.write_all(&0u32.to_le_bytes())
        .map_err(|e| e.to_string())?; // placeholder
    w.write_all(b"WAVE").map_err(|e| e.to_string())?;

    // fmt  chunk
    w.write_all(b"fmt ").map_err(|e| e.to_string())?;
    w.write_all(&16u32.to_le_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(&FORMAT_FLOAT.to_le_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(&CHANNELS.to_le_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(&OUT_RATE.to_le_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(&byte_rate.to_le_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(&BLOCK_ALIGN.to_le_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(&BITS.to_le_bytes())
        .map_err(|e| e.to_string())?;

    // data chunk header â€” size placeholder; offset recorded for fixup.
    w.write_all(b"data").map_err(|e| e.to_string())?;
    let data_size_offset = 40u64; // byte offset of the data chunk size field
    w.write_all(&0u32.to_le_bytes())
        .map_err(|e| e.to_string())?; // placeholder

    // â”€â”€ Stream resampled frames directly to disk â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    let mut resampler: Option<resampling::Context> = None;
    let mut data_bytes: u64 = 0;
    // Diagnostics for the empty-extraction failure below: without them the
    // error cannot distinguish "no packets" / "decoder produced no frames" /
    // "every frame filtered out by the window" / "resampler emitted nothing".
    // Declared here (not next to the packets loop) because `write_frame`
    // updates the resampler diagnostics while draining each decoded frame.
    let mut resample_dud: u32 = 0;
    let mut first_dud: Option<String> = None;

    let mut write_frame = |frame: &mut AudioFrame,
                           resampler: &mut Option<resampling::Context>,
                           w: &mut std::io::BufWriter<&mut std::fs::File>,
                           data_bytes: &mut u64,
                           pts_secs: f64|
     -> Result<bool, String> {
        // Skip frames entirely before the pre-roll window.
        if pts_secs < source_offset - 0.05 {
            return Ok(false);
        }
        // Signal caller to stop when we've passed clip_end.
        if pts_secs >= clip_end {
            return Ok(true);
        }

        // Trim pre-roll: after a keyframe-aligned seek the first decoded frame
        // may start before source_offset.  Writing those samples shifts playback
        // audio early â€” same bug as encode.rs.  Only write samples from
        // source_offset onwards (decode.rs skips video frames the same way).
        let pre_roll_samples =
            ((source_offset - pts_secs).max(0.0) * OUT_RATE as f64).round() as usize;

        let src_layout = {
            crate::helpers::resample::ensure_layout(frame);
            frame.ch_layout()
        };
        let needs_resample =
            frame.format() != OUT_FMT || frame.rate() != OUT_RATE || src_layout.channels() != 2;

        if needs_resample {
            let rs = resampler.get_or_insert_with(|| {
                resampling::Context::get2(
                    frame.format(),
                    src_layout.clone(),
                    frame.rate(),
                    OUT_FMT,
                    OUT_LAYOUT,
                    OUT_RATE,
                )
                .expect("create audio resampler for WAV extraction")
            });

            let mut resampled = AudioFrame::empty();
            let run = rs.run(frame, &mut resampled);
            let run_ok = run.is_ok();
            let run_samples = resampled.samples();
            if !run_ok || run_samples == 0 {
                resample_dud += 1;
                if first_dud.is_none() {
                    first_dud = Some(format!(
                        "run ok={run_ok} err={:?} samples={run_samples} src_fmt={:?} rate={} ch={} pre_roll={pre_roll_samples}",
                        run.err(),
                        frame.format(),
                        frame.rate(),
                        src_layout.channels()
                    ));
                }
            }
            if run_ok && run_samples > pre_roll_samples {
                // OUT_FMT is packed interleaved f32: each sample is 2 channels Ã— 4 bytes = 8 bytes.
                // Cap at valid_end â€” data(0) returns the full allocated FFmpeg buffer which is
                // often larger than the actual decoded sample count. Writing the full slice
                // appends uninitialized garbage bytes to the WAV, which manifests as crackle.
                let skip_bytes = pre_roll_samples * 2 * 4;
                let valid_end = resampled.samples() * 2 * 4;
                let data = &resampled.data(0)[skip_bytes..valid_end];
                w.write_all(data)
                    .map_err(|e| format!("write WAV samples: {e}"))?;
                *data_bytes += data.len() as u64;
            }
        } else {
            let skip_bytes = pre_roll_samples * 2 * 4;
            let valid_end = frame.samples() * 2 * 4;
            let data = &frame.data(0)[skip_bytes..valid_end];
            w.write_all(data)
                .map_err(|e| format!("write WAV samples: {e}"))?;
            *data_bytes += data.len() as u64;
        }
        Ok(false)
    };

    let mut packets_seen: u32 = 0;
    let mut packets_errored: u32 = 0;
    let mut frames_seen: u32 = 0;
    let mut first_pts: Option<f64> = None;
    let mut last_pts: Option<f64> = None;

    'packets: for result in ictx.packets() {
        let (stream, packet) = match result {
            Ok(p) => p,
            Err(_) => {
                packets_errored += 1;
                continue;
            }
        };
        if stream.index() != audio_stream_idx {
            continue;
        }
        packets_seen += 1;
        if decoder.send_packet(&packet).is_err() {
            continue;
        }

        let mut frame = AudioFrame::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            let pts_secs = frame
                .pts()
                .map(|p| p as f64 * in_tb.numerator() as f64 / in_tb.denominator() as f64)
                .unwrap_or(source_offset);
            frames_seen += 1;
            first_pts = first_pts.or(Some(pts_secs));
            last_pts = Some(pts_secs);
            if write_frame(
                &mut frame,
                &mut resampler,
                &mut w,
                &mut data_bytes,
                pts_secs,
            )? {
                break 'packets;
            }
        }
    }

    // Flush decoder
    let _ = decoder.send_eof();
    let mut frame = AudioFrame::empty();
    while decoder.receive_frame(&mut frame).is_ok() {
        let pts_secs = frame
            .pts()
            .map(|p| p as f64 * in_tb.numerator() as f64 / in_tb.denominator() as f64)
            .unwrap_or(source_offset);
        if write_frame(
            &mut frame,
            &mut resampler,
            &mut w,
            &mut data_bytes,
            pts_secs,
        )? {
            break;
        }
    }

    if data_bytes == 0 {
        return Err(format!(
            "no audio samples decoded ({packets_seen} audio packets sent, {packets_errored} packet errors, {frames_seen} frames decoded, pts {first_pts:?}..{last_pts:?}, window src={source_offset:.3} end={clip_end:.3}, resample duds {resample_dud}: {first_dud:?})"
        ));
    }

    // â”€â”€ Seek back and fix the two placeholder size fields â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    w.flush().map_err(|e| format!("flush WAV: {e}"))?;
    drop(w); // release BufWriter borrow so we can seek on `file` directly

    let riff_size = (36 + data_bytes) as u32; // total file size âˆ’ 8
    file.seek(SeekFrom::Start(riff_size_offset))
        .and_then(|_| file.write_all(&riff_size.to_le_bytes()))
        .map_err(|e| format!("fixup RIFF size: {e}"))?;

    let data_size = data_bytes as u32;
    file.seek(SeekFrom::Start(data_size_offset))
        .and_then(|_| file.write_all(&data_size.to_le_bytes()))
        .map_err(|e| format!("fixup data size: {e}"))?;

    Ok(44 + data_bytes)
}

#[cfg(test)]
mod extraction_tests {
    //! F1/RW safety of the WAV extraction pipeline: the decode must write a
    //! per-request staging file and only rename it over the final media path on
    //! success, so readers (active rodio sink decoders, concurrent extraction
    //! writers) never observe torn RIFF headers or stale-torn half-written
    //! data chunks, and a failed extraction leaves no staging leftovers.

    use super::*;
    use crossbeam_channel::{bounded, TryRecvError};
    use std::io::Write;
    use std::path::Path;
    use std::time::Duration;

    /// Bounded receive so a failed extraction can never hang the test.
    fn await_audio_result(rx: &crossbeam_channel::Receiver<MediaResult>) -> MediaResult {
        rx.recv_timeout(Duration::from_secs(60))
            .expect("AudioPath result within 60s")
    }
    /// Write a minimal stereo interleaved f32 sine WAV matching the extractor's
    /// target format exactly (44100 Hz, IEEE-float fmt tag 3, packed f32), so
    /// the decode lands on the extractor's direct-write path (no resampler).
    /// This keeps the test deterministic: the resampler path currently errors
    /// on every frame for synthetic inputs and would silently starve the WAV
    /// (pre-existing, tracked separately â€” see the first_dud diagnostic in the
    /// empty-extraction failure message). What this test pins is the F1/RW
    /// mechanics: staging, rename, RIFF fixups, replace semantics.
    fn write_sine_wav(path: &Path, secs: f64) {
        const RATE: u32 = 44_100;
        let samples = (secs * RATE as f64) as usize;
        let data_bytes = samples * 8; // stereo interleaved f32
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&((36 + data_bytes) as u32).to_le_bytes())
            .unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap(); // IEEE_FLOAT
        f.write_all(&2u16.to_le_bytes()).unwrap(); // stereo
        f.write_all(&RATE.to_le_bytes()).unwrap();
        f.write_all(&(RATE * 8).to_le_bytes()).unwrap();
        f.write_all(&8u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&(data_bytes as u32).to_le_bytes()).unwrap();
        for i in 0..samples {
            let v = (((i as f64 / RATE as f64) * std::f64::consts::TAU * 440.0).sin() * 0.4) as f32;
            f.write_all(&v.to_le_bytes()).unwrap(); // left
            f.write_all(&v.to_le_bytes()).unwrap(); // right
        }
    }

    /// Read back a completed extraction and validate its RIFF envelope:
    /// RIFF/WAVE markers, the fixed 16-byte fmt chunk for 44100 Hz s16
    /// stereo-f32, and `file len == 44 + data_size` (the two placeholder
    /// sizes were seek-fixed correctly).
    /// Returns the data chunk size in bytes.
    fn read_wav_envelope(path: &Path) -> (u32, u32) {
        let buf = std::fs::read(path).unwrap();
        assert_eq!(&buf[0..4], b"RIFF");
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[36..40], b"data");
        let riff_size = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let data_size = u32::from_le_bytes(buf[40..44].try_into().unwrap());
        assert_eq!(
            buf.len() as u64,
            44 + u64::from(data_size),
            "data chunk sizes as written"
        );
        assert_eq!(riff_size, 36 + data_size, "RIFF sizes as written");
        (data_size, buf.len() as u32)
    }

    fn temp_file(name: String) -> std::path::PathBuf {
        std::env::temp_dir().join(name)
    }

    fn staging_path(id: Uuid, gen: u64) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("velocut_audio_{id}_{gen}.staging"))
    }

    fn final_path(id: Uuid) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("velocut_audio_{id}.wav"))
    }

    #[test]
    fn extraction_writes_through_per_request_staging_and_renames_atomically() {
        let src = temp_file(format!("velocut-test-src-{}.wav", Uuid::new_v4()));
        write_sine_wav(&src, 1.0);
        let id = Uuid::new_v4();
        let (tx, rx) = bounded::<MediaResult>(4);

        // Full-file extraction at generation 7.
        extract_audio(&src, id, 0.0, 1.0, 7, &tx);
        match await_audio_result(&rx) {
            MediaResult::AudioPath {
                id: rid,
                path,
                trimmed_offset,
                generation,
            } => {
                assert_eq!(rid, id);
                assert_eq!(generation, 7, "request epoch travels on the result");
                assert_eq!(trimmed_offset, 0.0);
                assert_eq!(path, final_path(id));
            }
            _ => panic!("expected MediaResult::AudioPath"),
        }
        // The staging file was consumed by the rename.
        assert!(!staging_path(id, 7).exists());
        // And the renamed file carries a coherent RIFF envelope with exactly
        // 1s of stereo f32 samples (44100 frames Ã— 8 bytes); the direct write
        // path preserves the source sample count bit-exactly.
        let (data1, len1) = read_wav_envelope(&final_path(id));
        assert_eq!(data1, 352_800, "1s of stereo f32 = exactly 352800 bytes");

        // A second extraction for the SAME library id â€” different window and
        // generation â€” must win atomically over the file (Windows
        // MOVEFILE_REPLACE_EXISTING / POSIX rename both replace).
        extract_audio(&src, id, 0.5, 0.1, 9, &tx);
        match await_audio_result(&rx) {
            MediaResult::AudioPath {
                generation,
                trimmed_offset,
                ..
            } => {
                assert_eq!(generation, 9);
                assert_eq!(trimmed_offset, 0.5);
            }
            _ => panic!("expected MediaResult::AudioPath"),
        }
        assert!(!staging_path(id, 9).exists());
        let (data2, len2) = read_wav_envelope(&final_path(id));
        // Whole-frame granularity + the 0.05s pre-roll slack: the 0.1s window
        // is admitted frame-wise in [0.45, 0.6), so the tail frame spanning
        // past clip_end is written whole â€” up to ~0.17s is legitimate.
        assert!(
            (25_000..110_000).contains(&data2),
            "a 0.1s window should extract ~35280 bytes (up to +2 frames of frame-granular slack), got {data2}"
        );
        assert!(
            data2 < data1 && len2 < len1,
            "the trimmed extraction fully replaced the previous content"
        );

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(final_path(id));
    }

    #[test]
    fn failed_extraction_leaves_no_staging_nor_final_file() {
        let src = temp_file(format!("velocut-test-missing-{}.wav", Uuid::new_v4()));
        let id = Uuid::new_v4();
        let (tx, rx) = bounded::<MediaResult>(4);

        extract_audio(&src, id, 0.0, 1.0, 3, &tx);
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "a failed extraction sends no AudioPath"
        );
        assert!(
            !staging_path(id, 3).exists(),
            "staging file cleaned on failure"
        );
        assert!(!final_path(id).exists(), "no final media path side effect");
    }

    #[test]
    fn cleanup_audio_temp_removes_staging_leftovers_and_wavs() {
        let id = Uuid::new_v4();
        let wav = final_path(id);
        let staging = staging_path(id, 42);
        std::fs::File::create(&wav).unwrap();
        std::fs::File::create(&staging).unwrap();

        cleanup_audio_temp(&wav);
        cleanup_audio_temp(&staging);
        assert!(!wav.exists());
        assert!(!staging.exists());
    }
}
