// crates/velocut-media/src/waveform.rs
//
// Waveform extraction via ffmpeg-the-third (in-process audio decoding).
// Replaces the old ffmpeg CLI subprocess approach — no child process spawn,
// no PATH dependency, no stdout pipe. Handles all common sample formats.

use std::path::PathBuf;
use crossbeam_channel::Sender;
use uuid::Uuid;

use velocut_core::media_types::MediaResult;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::format::sample::{Sample, Type as SampleType};

const WAVEFORM_COLS: usize = 4000;

pub fn extract_waveform(path: &PathBuf, id: Uuid, tx: &Sender<MediaResult>) {
    let samples = match decode_audio_samples(path) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            eprintln!("[media] waveform: no samples for {}", path.display());
            return;
        }
        Err(e) => {
            eprintln!("[media] waveform decode {}: {e}", path.display());
            return;
        }
    };

    let block = (samples.len() / WAVEFORM_COLS).max(1);
    let peaks: Vec<f32> = samples
        .chunks(block)
        .take(WAVEFORM_COLS)
        .map(|chunk| chunk.iter().map(|s| s.abs()).fold(0.0f32, f32::max))
        .collect();

    eprintln!("[media] waveform {} peaks <- {}", peaks.len(), path.display());
    let _ = tx.send(MediaResult::Waveform { id, peaks });
}

fn decode_audio_samples(path: &PathBuf) -> Result<Vec<f32>, String> {
    let mut ictx = ffmpeg::format::input(path)
        .map_err(|e| format!("open: {e}"))?;

    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .ok_or_else(|| "no audio stream".to_string())?;
    let stream_index = stream.index();

    let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| format!("codec context: {e}"))?;
    let mut decoder = ctx
        .decoder()
        .audio()
        .map_err(|e| format!("audio decoder: {e}"))?;

    let mut samples: Vec<f32> = Vec::new();

    for result in ictx.packets() {
        let (stream, packet) = match result {
            Ok(p) => p,
            Err(_) => continue, // Skip malformed packets
        };

        if stream.index() != stream_index {
            continue;
        }
        
        if decoder.send_packet(&packet).is_ok() {
            let mut frame = ffmpeg::frame::Audio::empty();
            while decoder.receive_frame(&mut frame).is_ok() {
                append_frame_samples(&frame, &mut samples);
            }
        }
    }

    let _ = decoder.send_eof();
    let mut frame = ffmpeg::frame::Audio::empty();
    while decoder.receive_frame(&mut frame).is_ok() {
        append_frame_samples(&frame, &mut samples);
    }

    Ok(samples)
}

/// Append mono f32 samples from a decoded frame into `out`.
/// Packed formats: step by channel count to extract channel 0 only.
/// Planar formats: plane 0 is already channel 0.
fn append_frame_samples(frame: &ffmpeg::frame::Audio, out: &mut Vec<f32>) {
    let channels = frame.ch_layout().channels() as usize;
    let data      = frame.data(0);

    match frame.format() {
        Sample::F32(SampleType::Packed) => {
            out.extend(data.chunks_exact(4).step_by(channels.max(1))
                .map(|b| f32::from_le_bytes([b[0],b[1],b[2],b[3]]).clamp(-1.0, 1.0)));
        }
        Sample::F32(SampleType::Planar) => {
            out.extend(data.chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0],b[1],b[2],b[3]]).clamp(-1.0, 1.0)));
        }
        Sample::I16(SampleType::Packed) => {
            out.extend(data.chunks_exact(2).step_by(channels.max(1))
                .map(|b| i16::from_le_bytes([b[0],b[1]]) as f32 / 32768.0));
        }
        Sample::I16(SampleType::Planar) => {
            out.extend(data.chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0],b[1]]) as f32 / 32768.0));
        }
        Sample::I32(SampleType::Packed) => {
            out.extend(data.chunks_exact(4).step_by(channels.max(1))
                .map(|b| i32::from_le_bytes([b[0],b[1],b[2],b[3]]) as f32 / 2_147_483_648.0));
        }
        Sample::I32(SampleType::Planar) => {
            out.extend(data.chunks_exact(4)
                .map(|b| i32::from_le_bytes([b[0],b[1],b[2],b[3]]) as f32 / 2_147_483_648.0));
        }
        Sample::F64(SampleType::Packed) => {
            out.extend(data.chunks_exact(8).step_by(channels.max(1))
                .map(|b| f64::from_le_bytes([b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7]]) as f32)
                .map(|s| s.clamp(-1.0, 1.0)));
        }
        Sample::F64(SampleType::Planar) => {
            out.extend(data.chunks_exact(8)
                .map(|b| f64::from_le_bytes([b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7]]) as f32)
                .map(|s| s.clamp(-1.0, 1.0)));
        }
        Sample::U8(SampleType::Packed) => {
            out.extend(data.iter().step_by(channels.max(1))
                .map(|&b| (b as f32 / 128.0) - 1.0));
        }
        Sample::U8(SampleType::Planar) => {
            out.extend(data.iter().map(|&b| (b as f32 / 128.0) - 1.0));
        }
        fmt => {
            eprintln!("[media] waveform: unhandled sample format {fmt:?} — skipping frame");
        }
    }
}