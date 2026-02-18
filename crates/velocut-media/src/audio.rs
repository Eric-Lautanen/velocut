// crates/velocut-media/src/audio.rs
//
// Audio extraction (WAV for rodio playback) and temp file cleanup.

use std::path::PathBuf;
use crossbeam_channel::Sender;
use uuid::Uuid;

use velocut_core::media_types::MediaResult;

/// Extract audio from `path` to a temp WAV file and send the path back via `tx`.
pub fn extract_audio(path: &PathBuf, id: Uuid, tx: &Sender<MediaResult>) {
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