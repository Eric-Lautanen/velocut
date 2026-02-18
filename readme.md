# ⚡ VeloCut

VeloCut is a lightweight, fast video editing desktop application built with Rust, using the egui immediate-mode GUI library (via eframe), FFmpeg for media processing, and Rodio for audio playback. It's designed for quick cuts and simple edits, with a focus on performance and ease of use. Ideal for short-form content like YouTube Shorts, TikToks, or Reels.

## Features

- **Media Import**: Drag-and-drop or import video/audio files (MP4, MOV, MKV, AVI, MP3, WAV, etc.).
- **Timeline Editing**: Place clips on video/audio tracks, drag to rearrange, delete clips.
- **Preview Monitor**: Real-time preview with aspect ratio selection (16:9, 9:16, 1:1, etc.), play/pause, scrubbing, volume control, mute.
- **Thumbnails & Waveforms**: Auto-generated thumbnails for videos and waveforms for audio visualization.
- **Frame Extraction**: Extract first/last frame of selected clips as PNG.
- **Auto FFmpeg Setup**: Downloads and configures FFmpeg binaries if not installed.
- **Cross-Platform**: Runs on Windows, macOS, and Linux (via eframe).
- **Dark Theme**: Customizable UI with a modern dark palette.
- **Hotkeys**: Basic shortcuts like Space for play/pause, Del for delete, J/K/L for frame scrubbing.

## Installation

1. **Prerequisites**:
   - Rust (latest stable via rustup).
   - Git.

2. **Clone and Build**:
git clone https://github.com/yourusername/velocut.git
cd velocut
cargo build --release
text3. **Run**:
cargo run --release
text- On first run, it will auto-download FFmpeg if not detected (stored in `%APPDATA%/VeloCut/ffmpeg` on Windows or `~/.local/share/VeloCut/ffmpeg` elsewhere).

## Usage

- **Import Media**: Use the "＋ Import" button in the Media Bin or drag files into the window.
- **Add to Timeline**: Drag clips from the Media Bin to the Timeline.
- **Edit**: Select clips (click), drag to move, Del to remove.
- **Preview**: Use transport controls (play/pause/stop) or scrub the playhead.
- **Extract Frames**: Select a timeline clip, use "⬛ First Frame" or "⬛ Last Frame" in the toolbar.
- **Export**: Configure settings in the Export panel and click "⚡ Render MP4" (currently logs to console; actual export coming soon).
- **Aspect Ratio**: Change in the Preview header dropdown.

## Current State

VeloCut is in early development (alpha stage). Core features like importing, timeline placement, preview playback, and basic audio handling work well for simple projects. However:

- **Functional**: Media probing (duration, thumbnails, waveforms, video size), basic timeline snapping, real-time preview frames (via FFmpeg decoding), audio playback synced with preview.
- **Limitations**:
- Export is a placeholder (prints settings to console; no actual file output yet).
- No clip trimming/splitting (clips are full-length only).
- No undo/redo.
- No project save/load (state persists via eframe storage but is basic).
- Audio is extracted to temp WAV for playback; supports stereo at 44.1kHz.
- Performance: Handles short clips well; may lag with long/high-res media (optimizations needed).
- Tested primarily on Windows; macOS/Linux may have FFmpeg path issues.
- **Dependencies**: eframe (egui), ffmpeg-sidecar (for download/unpack), rodio (audio), uuid, serde, crossbeam-channel, rfd (file dialogs).

## TODO List

- [ ] Implement full export to MP4/H.264 using FFmpeg (concat clips, apply aspect ratio, FPS).
- [ ] Add clip trimming: Drag handles to adjust start/end offsets.
- [ ] Support clip splitting (cut at playhead).
- [ ] Undo/redo stack for edits.
- [ ] Project save/load (JSON/serde for state).
- [ ] Add transitions (fades, wipes) between clips.
- [ ] Text overlays and basic effects (crop, rotate).
- [ ] Improve timeline: Multi-track support, zoom/pan gestures.
- [ ] Optimize preview: Cache more frames, async loading.
- [ ] Audio mixing: Handle overlapping audio clips.
- [ ] Error handling: Better UI feedback for FFmpeg failures, unsupported formats.
- [ ] Settings panel: Custom FFmpeg path, theme tweaks.
- [ ] Cross-platform binaries: GitHub Actions for releases.
- [ ] Documentation: Screenshots, video demo.

## Contributing

Contributions welcome! Fork the repo, create a branch, and submit a PR. Focus on TODO items or bug fixes. Use `cargo fmt` and `cargo clippy` before committing.

## License

MIT License. See [LICENSE](LICENSE) for details.

---
Built by [Your Name] with ❤️ using Rust.