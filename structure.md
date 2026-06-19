# VeloCut — Crate & File Structure

## Workspace (3 crates)

### `velocut-core` — Data model, domain logic, transitions, filters
No FFmpeg dependency; pure Rust types.

| File | Purpose |
|------|---------|
| `lib.rs` | Re-exports all modules as the crate root. |
| `commands.rs` | `EditorCommand` enum: all user actions (playback, library, timeline, undo/redo, export, filters, transitions) with `validate()` precondition checks. |
| `state.rs` | Serializable `ProjectState`, `TimelineClip`, `LibraryClip`, `AspectRatio`, `ClipType` — snapping, duration, transition management. |
| `media_types.rs` | `MediaResult` (worker→UI messages: duration, thumbnails, waveforms, frames, encode progress), `PlaybackFrame`, `TransitionScrubRequest`. |
| `windows.rs` | `lower_thread_priority()` — `SetThreadPriority` (Win) / `nice(10)` (POSIX) to prevent encode thread from starving UI. |
| `filters/mod.rs` | `FilterParams` (brightness, contrast, saturation, gamma, hue, temp, strength), `FilterKind` enum with 16 presets via `declare_filters!`. |
| `filters/helpers.rs` | `apply_filter_rgba` / `apply_filter_yuv` — parallel pixel-math with RGB↔HSV helpers. |
| `helpers/geometry.rs` | `aspect_ratio_value()` / `aspect_ratio_label()` — `AspectRatio`→f32/string. |
| `helpers/time.rs` | `format_time()` (MM:SS:FF) / `format_duration()` (compact H:MM:SS). |
| `transitions/mod.rs` | `VideoTransition` trait, `TransitionKind` enum (Cut + 8 variants), O(1) registry via `OnceLock`. |
| `transitions/helpers.rs` | Shared easing curves, `blend_byte`, YUV420P layout, spatial helpers, `rgba_to_yuv420p`/`yuv420p_to_rgba`, `blend_buffers`. |
| `transitions/crossfade.rs` | `Crossfade` — linear byte-blend with smooth-step easing (YUV420P + RGBA). |
| `transitions/dip_to_black.rs` | `DipToBlack` — fade to black then to frame_b, smooth-step within each half. |
| `transitions/dip_to_white.rs` | `DipToWhite` — fade to white (Y=255, UV=128) then to frame_b. |
| `transitions/push.rs` | `Push` — frame_b slides in from right, frame_a pushed left (cubic easing, no blend). |
| `transitions/wipe.rs` | `Wipe` — vertical bar sweeps left→right with 2% feathered edge. |
| `transitions/iris.rs` | `Iris` — expanding circle from center with feathered edge. |
| `transitions/clock_wipe.rs` | `ClockWipe` — sweep hand rotates clockwise from 12 o'clock. |
| `transitions/barn_doors.rs` | `BarnDoors` — left/right halves slide outward from center. |

### `velocut-media` — FFmpeg decoding, encoding, audio, waveform, worker threads

| File | Purpose |
|------|---------|
| `lib.rs` | Crate root; re-exports `MediaWorker`, `ClipSpec`, `EncodeSpec`, `MediaResult`, `PlaybackFrame`. |
| `probe.rs` | `probe_duration()` / `probe_video_size_and_thumbnail()` — file metadata & 160px RGBA thumbnail. |
| `decode.rs` | `LiveDecoder` — stateful per-clip decoder (D3D11VA hwaccel, cached SwsContext, GOP-burn, center-crop). Free functions `decode_frame()`, `decode_one_frame_rgba()`. |
| `audio.rs` | `extract_audio()` — decode audio to 44100 Hz stereo f32le WAV via ffmpeg-the-third (no CLI). |
| `waveform.rs` | `extract_waveform()` — decode audio, down-sample to 4000 amplitude peaks for timeline display. |
| `encode/mod.rs` | `EncodeSpec`, `ClipSpec`, `AudioOverlay`; `encode_timeline()` — H.264+AAC MP4 assembly with transitions, fade envelopes, FIFO mixing, HW encoder selection, cancellation. |
| `encode/clip.rs` | `CropScaler`, `encode_clip()`, `decode_clip_frames/audio()`, `apply_transition()`, `send_video_frame()`. |
| `encode/audio.rs` | `AudioFifo` (stereo f32 ring buffer), `AudioEncState`, `decode_overlay()`, `fade_gain()`. |
| `encode/hw.rs` | HW encoder probing (AMF/NVENC/VAAPI/VideoToolbox) and `upload_frame_to_hw()`. |
| `worker.rs` | `MediaWorker` — owns decode threads: latest-wins scrub, transition-scrub, playback thread, probe/encode dispatch, semaphore-limited HQ decode, poison-pill shutdown. |
| `worker/types.rs` | `FrameRequest` (latest-wins scrub slot), `PlaybackCmd` (Start/StartBlend/Stop/PreBuffer). |
| `worker/semaphore.rs` | `SemaphoreGuard` RAII — limits concurrent probe/HQ-decode threads via `(Mutex<u32>, Condvar)`. |
| `worker/pb_thread.rs` | `PbThread::run()` — state machine decoding frames, handling centered transitions (blend + bridge + coast), prebuffered decoders, rate-limited blocking send. |
| `worker/blend.rs` | RGBA transition helpers: `decode_transition_scrub_frame()`, `crop_rgba()`, `blend_rgba_transition()`. |
| `helpers/log.rs` | `media_log!` macro → `%TEMP%\velocut.log`, process-lifetime `OnceLock<Mutex<File>>`. |
| `helpers/seek.rs` | `seek_to_secs()` — `avformat_seek_file` wrapper with Windows EPERM handling. |
| `helpers/yuv.rs` | `extract_yuv()` / `write_yuv()` — YUV420P byte vectors ↔ ffmpeg `VideoFrame` planes. |

### `velocut-ui` — egui/eframe GUI application (binary = `velocut`)

| File | Purpose |
|------|---------|
| `main.rs` | Entry point: FFmpeg init, icon load, `eframe::run_native`, `fix_taskbar_icon()` (Win32 WS_EX_APPWINDOW + WM_SETICON). |
| `app.rs` | `VeloCutApp` — owns `ProjectState`, `AppContext`, modules, undo/redo stacks; implements `process_command()`, `build_encode_plan()`, `poll_media()`, drag-and-drop, main `logic()`/`ui()`. |
| `context.rs` | `AppContext` — `MediaWorker`, `CacheContext` (thumbnail/frame/bucket caches, 192MB ceiling), `PlaybackContext`, rodio `audio_stream`/`audio_sinks`. |
| `theme.rs` | Dark color palette constants; `configure_style()` — egui Visuals & spacing. |
| `build.rs` | Embeds Windows `.ico` icon via `winresource`. |
| `modules/mod.rs` | `EditorModule` trait (ui/tick), re-exports all panel modules. |
| `modules/timeline.rs` | `TimelineModule` — 4-track layout (V1/A1/V2/A2), ruler, clip thumbnails+waveforms, trim handles, drag-move, transition badges, volume/fade/color popups. |
| `modules/library.rs` | `LibraryModule` — thumbnail card grid, multi-select, drag-to-timeline, context menus, rfd file import, probe-priority tracking. |
| `modules/preview_module.rs` | `PreviewModule` — video canvas (center-cropped to AR), transport bar, volume slider, timecode. |
| `modules/export_module.rs` | `ExportModule` — filename/quality/FPS/AR settings, presets (480p–4K), HW capability annotation, render progress modal, uninstall button. |
| `modules/audio_module.rs` | `AudioModule` — rodio per-clip WAV sinks, seek, fade ramps, mix normalization (1/√N), exhausted-clip detection, soft drain. |
| `modules/video_module.rs` | `VideoModule` — 3-layer scrub (cached bucket → exact decode → coarse prefetch → idle HQ), playback start/stop with centered-transition blend spec, PTS gating, clip-change eviction, prebuffer look-ahead. |
| `helpers/clip_query.rs` | Clip lookup functions replacing inline filter chains (`clip_at_time`, `selected_timeline_clip`, `active_transition_at`, etc.). |
| `helpers/format.rs` | `fit_label()` (pixel-budget truncation), `truncate()` (byte-budget UTF-8-safe). |
| `helpers/log.rs` | `velocut_log!` macro → `%TEMP%\velocut.log`. |
| `helpers/memory_manager.rs` | `MemoryManager` — 2-stage eviction: 2s idle (buckets ±5s playhead), 30s idle (flush all caches + egui memory), 100-thumbnail cap. |
| `helpers/reset.rs` | `delete_app_data_dir()` / `delete_temp_files()` / `reset_context()` — filesystem cleanup, in-memory teardown, hard-exit, uninstall modal. |

## Dependency Graph

```
velocut-core  ←  velocut-media  ←  velocut-ui (binary)
      ↑                              ↑
  (no FFmpeg)          depends on both core & media
```
