<div align="center">

# ⚡ VeloCut

**A fast, native desktop video editor written in Rust.**

![Rust](https://img.shields.io/badge/Rust-1.93+-orange?style=flat-square&logo=rust)
![Platform](https://img.shields.io/badge/Platform-Windows-blue?style=flat-square&logo=windows)
![License](https://img.shields.io/badge/License-MIT-green?style=flat-square)
![egui](https://img.shields.io/badge/UI-egui%200.33-purple?style=flat-square)
![FFmpeg](https://img.shields.io/badge/FFmpeg-static-red?style=flat-square)

</div>

---

## What is VeloCut?

VeloCut is a native desktop video editor built entirely in Rust. It targets the gap between heavyweight professional editors and simple clip trimmers — fast to launch, lightweight on resources, and designed for direct, keyboard-friendly editing workflows.

The UI is built with [egui](https://github.com/emilk/egui) / [eframe](https://github.com/emilk/egui/tree/master/crates/eframe). All media decoding and encoding is handled by a custom-forked [ffmpeg-the-third](https://github.com/eric-lautanen/velocut-ffmpeg-the-third) binding against a statically compiled FFmpeg.

---

## Features

- **Multi-track timeline** — Four lanes (V1/A1/V2/A2) with drag-and-drop from the media library
- **Real-time scrubbing** — Three-tier scrub system: instant nearest-cached frame, per-pixel exact decode, and 150ms idle precise frame
- **Smooth playback** — Dedicated 32-frame buffered playback pipeline, PTS-gated and clocked by `stable_dt` for accurate audio/video sync
- **Waveform display** — 4000-column waveform overlays on audio/video clips, rendered at clip pixel width
- **Per-clip volume** — dB-space volume slider per clip with visual waveform gain feedback
- **Transitions** — Cut and Crossfade transitions between clips with configurable duration, rendered via YUV frame blending
- **Multi-clip import** — Batch import from file dialog or drag-and-drop onto the window
- **Library management** — Thumbnail grid with multi-select (Ctrl, Shift, Ctrl+A), drag-to-timeline
- **Export** — H.264/MP4 encode at 480p/720p/1080p/1440p/2160p, 24/30/60 fps, with live progress and cancellation
- **Frame save** — Export any single frame to PNG from the preview panel
- **Undo/Redo** — 50-level snapshot-based undo with runtime field preservation (playback, encode state unaffected)
- **Session persistence** — Project state saved and restored between launches via eframe storage
- **Custom chrome** — Frameless window with custom title bar, software resize handles, and accent-colored branding

---

## Platform

| Platform | Status |
|----------|--------|
| Windows (MINGW64) | ✅ Supported |
| Linux | 🔬 Untested |
| macOS | 🔬 Untested |

---

## Workspace Structure

```
velocut/
  Cargo.toml               ← workspace root; shared [workspace.dependencies]
  assets/                  ← fonts, icons
  crates/
    velocut-core/          ← pure data & contracts (no UI, no FFmpeg)
    velocut-media/         ← FFmpeg worker threads (no egui)
    velocut-ui/            ← egui app + binary entry point
```

**Dependency rules:** `velocut-ui` → `core` + `media` · `velocut-media` → `core` · `core` and `media` → no egui

### velocut-core

Pure data types and contracts shared across the workspace. No UI, no FFmpeg.

- `state.rs` — Serializable `ProjectState`: library clips, timeline clips, playback state, encode status
- `commands.rs` — `EditorCommand` enum: every user action emitted by UI modules and dispatched by `app.rs`
- `transitions.rs` — `TransitionType` (Cut, Crossfade) and `TimelineTransition` storage
- `helpers/time.rs` — `format_time` (MM:SS:FF) and `format_duration` (H:MM:SS / M:SS / S.Xs)
- `helpers/geometry.rs` — Aspect ratio value and label helpers

### velocut-media

All FFmpeg work runs here on background threads. No egui dependency.

- `worker.rs` — `MediaWorker`: probe semaphore (max 4 concurrent), dedicated playback decode thread, dedicated scrub result channel, encode with per-job AtomicBool cancellation
- `encode.rs` — Multi-clip H.264/MP4 pipeline with crossfade blending, monotonic PTS reassignment, CRF 18 + fast preset
- `decode.rs` — `LiveDecoder` (stateful playback/scrub) and `decode_frame` (one-shot HQ extraction). `SwsContext` reused across scrub frames when source format/dimensions match
- `probe.rs` — Duration, video dimensions, thumbnail (scaled to 320px at 10% seek)
- `waveform.rs` — FFmpeg CLI → raw PCM → 4000-column peak array
- `audio.rs` — FFmpeg CLI → temp WAV for rodio playback
- `helpers/seek.rs` — `seek_to_secs` with Windows EPERM soft-fail guard
- `helpers/yuv.rs` — Stride-aware YUV420P pack/unpack and linear frame blending

### velocut-ui

The egui application and binary entry point.

- `main.rs` — FFmpeg init, window config (frameless, centered, custom icon), font setup
- `app.rs` — `VeloCutApp`: concrete module fields, command dispatch, undo/redo stacks, encode orchestration, media polling
- `context.rs` — `AppContext`: runtime-only handles (worker, caches, sinks). Sole translation layer between `MediaWorker` output and UI state
- `theme.rs` — Color constants and egui style configuration
- `modules/` — `LibraryModule`, `PreviewModule`, `TimelineModule`, `ExportModule`, `AudioModule`, `VideoModule`

---

## Key Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `eframe` / `egui` | 0.33 | UI framework |
| `ffmpeg-the-third` | forked | FFmpeg bindings (static) |
| `crossbeam-channel` | 0.5 | Worker thread channels |
| `rodio` | 0.21.1 | Audio playback |
| `rfd` | 0.14 | Native file dialogs |
| `serde` | 1.0 | Project serialization |
| `uuid` | 1.10 | Clip identity |
| `egui-desktop` | 0.2.2 | Custom title bar + resize handles |
| `png` | 0.18.1 | Icon loading, frame export |

### FFmpeg Fork

VeloCut uses a custom fork of `ffmpeg-the-third` at [`eric-lautanen/velocut-ffmpeg-the-third`](https://github.com/eric-lautanen/velocut-ffmpeg-the-third) (branch `master`). The fork exposes low-level encoder/decoder flush control that upstream does not provide. This is a long-term owned dependency, not a temporary patch — do not replace it with upstream.

---

## Building

### Prerequisites

- Rust 1.93+
- MSYS2 / MINGW64 (Windows)
- The forked FFmpeg static libraries (linked via the `ffmpeg-the-third` fork — see its README for build instructions)

### Build

```bash
cargo build --release
```

The release binary is at `target/release/velocut.exe`.

---

## Architecture Notes

**Command flow:** UI modules receive `&ProjectState` (read-only) and emit `EditorCommand` values into a `pending_cmds` vec. After each frame, `app.rs::process_command()` dispatches all commands and mutates state. Modules never mutate state directly.

**Frame cache pipeline (per tick, in order):**
1. `poll_media()` → `poll_playback()` — frame cache writes
2. `update()` — preview reads frame cache, panels render
3. `tick()` — additional cache evictions

Any eviction that must take effect before the next render belongs in `poll_playback()`, not `tick()`.

**Scrub tiers:**
- L1: nearest cached bucket frame — 0 ms latency
- L2: exact-timestamp decode on every drag pixel
- L2b: coarse 2s prefetch
- L3: precise frame after 150ms idle debounce

**Playback clock:** `stable_dt` is the master clock. `current_time += stable_dt` every frame. PTS from decoded frames is used only for frame promotion gating, never for advancing time.

**Undo snapshots:** Full `ProjectState` clones, capped at 50 entries. Runtime-only fields (playback position, encode progress, pending queues) are preserved from the live state after each undo/redo. Clips with empty `waveform_peaks` after a restore are automatically re-queued for probing.

---

## License

MIT License

Copyright (c) 2025 Eric Lautanen

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.