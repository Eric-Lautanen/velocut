// src/paths.rs
// Single source of truth for where VeloCut stores its FFmpeg binaries.

use std::path::PathBuf;

/// `%APPDATA%\VeloCut\ffmpeg` on Windows, `~/.local/share/velocut/ffmpeg` elsewhere.
pub fn app_ffmpeg_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local").join("share"))
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("VeloCut").join("ffmpeg")
}