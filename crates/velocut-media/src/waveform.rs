// crates/velocut-media/src/waveform.rs
//
// Waveform extraction via ffmpeg CLI — pipes raw mono f32 samples at 2 kHz.
// Simple and codec-agnostic; no Rust audio decoder needed.

use std::path::PathBuf;
use crossbeam_channel::Sender;
use uuid::Uuid;

use velocut_core::media_types::MediaResult;

const WAVEFORM_COLS: usize = 1000;

pub fn extract_waveform(path: &PathBuf, id: Uuid, tx: &Sender<MediaResult>) {
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