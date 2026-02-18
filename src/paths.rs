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

/// Full path to the ffmpeg executable.
pub fn ffmpeg_bin() -> PathBuf {
    #[cfg(target_os = "windows")]
    let name = "ffmpeg.exe";
    #[cfg(not(target_os = "windows"))]
    let name = "ffmpeg";

    // ffmpeg-sidecar extracts into a versioned subdirectory like
    // ffmpeg-7.1-essentials_build/bin/ffmpeg.exe — walk one level deep to find it.
    let dir = app_ffmpeg_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("bin").join(name);
            if candidate.exists() {
                return candidate;
            }
            // Also check directly inside the subdir (no bin/ subfolder)
            let candidate2 = entry.path().join(name);
            if candidate2.exists() {
                return candidate2;
            }
        }
    }
    // Fallback: flat layout (shouldn't happen but safe)
    dir.join(name)
}

/// Full path to the ffprobe executable.
pub fn ffprobe_bin() -> PathBuf {
    #[cfg(target_os = "windows")]
    let name = "ffprobe.exe";
    #[cfg(not(target_os = "windows"))]
    let name = "ffprobe";

    let dir = app_ffmpeg_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("bin").join(name);
            if candidate.exists() {
                return candidate;
            }
            let candidate2 = entry.path().join(name);
            if candidate2.exists() {
                return candidate2;
            }
        }
    }
    dir.join(name)
}