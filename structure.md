# VeloCut — Crate & File Structure

## Workspace (3 crates)

### `velocut-core` — Data model, domain logic, transitions, filters
No FFmpeg dependency; pure Rust types.

| File | Purpose |
|------|---------|
| `lib.rs` | Re-exports all modules as the crate root. |
| `commands.rs` | `EditorCommand` enum: all user actions (playback, library, timeline, undo/redo, export, filters, transitions) with `validate()` precondition checks. |
| `state.rs` | Serializable `ProjectState`, `TimelineClip`, `LibraryClip`, `AspectRatio`, `ClipType` — snapping, duration, transition management. A/V link sync helpers: `sync_linked_clip` / `sync_linked_fades` (called by trim/move/fade handlers), separated-audio lifecycle: `audio_library_source`, `audio_path_in_use`, `remove_library_clip`. |
| `media_types.rs` | `MediaResult` (worker→UI messages: duration, thumbnails, waveforms, frames, encode progress), `PlaybackFrame`, `FrameBuf` (pixel buffer with an optional return path — dropping the frame hands the allocation back to the playback thread), `TransitionScrubRequest`. |
| `windows.rs` | `lower_thread_priority()` — `SetThreadPriority` (Win) / `nice(10)` (POSIX) to prevent encode thread from starving UI. |
| `filters/mod.rs` | `FilterParams` (brightness, contrast, saturation, gamma, hue, temp, strength), `FilterKind` enum with 16 presets via `declare_filters!`. |
| `filters/helpers.rs` | `apply_filter_rgba` / `apply_filter_yuv` — parallel pixel-math with RGB↔HSV helpers. |
| `helpers/geometry.rs` | `aspect_ratio_value()` / `aspect_ratio_label()` — `AspectRatio`→f32/string. |
| `helpers/time.rs` | `format_time()` (MM:SS:FF) / `format_duration()` (compact H:MM:SS). |
| `transitions/mod.rs` | `VideoTransition` trait, `TransitionKind` enum (Cut + 8 variants), O(1) registry via `OnceLock`. `TransitionType::clamped_secs(clip_a_dur, clip_b_dur)` — THE single-source duration clamp for the centered blend model (`min(D, 2×min)`, alpha 0.5 at the cut); used by preview zone math, playback blend builders, `SetTransition` and the encoder. |
| `transitions/helpers.rs` | Shared easing curves, `blend_byte` and the fixed-point pair `fixed_weight`/`blend_byte_fixed`, YUV420P layout, spatial helpers, `rgba_to_yuv420p`/`yuv420p_to_rgba`, `blend_buffers`. |
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
| `decode.rs` | `LiveDecoder` — stateful per-clip decoder (D3D11VA hwaccel, cached SwsContext, GOP-burn, center-crop). `next_frame_into(&mut buf)` decodes into a caller-owned buffer (playback passes a recycled one); `next_frame()` allocates for one-shot callers. Three reusable AVFrame slots (`dec_frame`, `cpu_frame`, `scale_frame`) mean the decode loops allocate no frame struct per frame. Free functions `decode_frame()`, `decode_one_frame_rgba()` (scrub/conform, one-shot by design). |
| `audio.rs` | `extract_audio()` — decode audio to 44100 Hz stereo f32le WAV via ffmpeg-the-third (no CLI). |
| `waveform.rs` | `extract_waveform()` — decode audio, down-sample to 4000 amplitude peaks for timeline display. |
| `encode/mod.rs` | `EncodeSpec`, `ClipSpec` (including `timeline_start`, which preserves timeline gaps), `AudioOverlay`; `encode_timeline()` — H.264+AAC MP4 assembly with transitions, fade envelopes, FIFO mixing, HW encoder selection, cancellation. |
| `encode/clip.rs` | `CropScaler`, `encode_clip()`, `decode_clip_frames/audio()`, `apply_transition()`, `send_video_frame()`. `encode_clip` aligns the audio FIFO to the global video boundary at every clip end (pad silence for missing/short audio, trim overshoot) so silent or video-only clips cannot shift later audio. `apply_transition` renders the CENTERED model (same as preview): `centred_blend_indices` (outgoing half plays then holds last frame; incoming holds frame 0 until the cut then plays; short decodes clamp instead of panicking), `centred_audio_gains` (equal-power, side-gated), blend spans the full D from D/2 half-length buffers. |
| `encode/audio.rs` | `AudioFifo` (stereo f32 ring buffer), `AudioEncState`, `decode_overlay()` (absolute-time PCM decode for standalone A clips and for extracted partners nudged off their V anchor), `fade_gain()`. `primary_spans` (merged sample ranges of non-`skip_audio` V clips) drives the preview's equal-power mix: primary + active overlays are scaled by `1/sqrt(N)` before the clamp. Resampler setup uses `helpers::resample::ensure_layout` so PCM frames with a channel count but no channel mask decode instead of panicking. |
| `encode/hw.rs` | HW encoder probing (AMF/NVENC/VAAPI/VideoToolbox) and `upload_frame_to_hw()`. |
| `worker.rs` | `MediaWorker` — owns decode threads: latest-wins scrub, transition-scrub, playback thread, probe/encode dispatch, semaphore-limited HQ decode, poison-pill shutdown. Also owns the frame-buffer return channel: the recycle hook stamped on each `PlaybackFrame` sends the buffer back to the pb thread's pool. |
| `worker/types.rs` | `FrameRequest` (latest-wins scrub slot), `PlaybackCmd` (Start/StartBlend/Stop/PreBuffer). |
| `worker/semaphore.rs` | `SemaphoreGuard` RAII — limits concurrent probe/HQ-decode threads via `(Mutex<u32>, Condvar)`. |
| `worker/pb_thread.rs` | `PbThread::run()` — state machine decoding frames, handling centered transitions (blend + bridge + coast), prebuffered decoders, rate-limited blocking send. Holds every buffer the frame path needs across frames — `blend_out`, `decoder_b_buf`, `held_blend`, and a capped pool of returned frame buffers (`refill_pool`) — so decoding, blending, the channel copy and the held/coast repeat paths allocate nothing in steady state. |
| `worker/blend.rs` | RGBA transition helpers: `decode_transition_scrub_frame()`, `crop_rgba()`, `blend_rgba_transition()` and `blend_rgba_transition_into()` (writes into a caller-owned buffer), `rotate_blended_frame()` (recycles the playback thread's scratch buffer against `held_blend`). |
| `helpers/log.rs` | `media_log!` macro → `%TEMP%\velocut.log`, process-lifetime `OnceLock<Mutex<File>>`. |
| `helpers/seek.rs` | `seek_to_secs()` — `avformat_seek_file` wrapper with Windows EPERM handling. |
| `helpers/resample.rs` | `ensure_layout()` — fills in the standard channel layout when a decoded frame reports channels without a mask (basic PCM WAV), which `swresample` otherwise rejects/panics on. Used by WAV extraction, clip audio, transition audio and overlays. |
| `helpers/yuv.rs` | `extract_yuv()` / `write_yuv()` — YUV420P byte vectors ↔ ffmpeg `VideoFrame` planes. |
| `tests/encode_transition.rs` | End-to-end transition encode checks over real media (`sequence_01.mp4`): 0.6s crossfade and overlong-clamped cases must both preserve the timeline length and audio stream. |
| `tests/encode_gaps.rs` | End-to-end gap check over real media: a leading + inter-clip gap must keep the timeline length, render black (Y=16) frames in both gaps, and leave each clip in its absolute slot. |
| `tests/encode_audio_align.rs` | End-to-end audio-alignment check over real media: a silent (`skip_audio`) first or second clip must still yield ~2 s of audio for a 2 s timeline, proving the per-clip FIFO padding keeps later audio and overlays locked to the picture. |
| `tests/encode_overlay.rs` | End-to-end overlay-mix checks: a synthesized 440 Hz WAV over a silent 2 s clip must appear only in its 0.5–1.5 s timeline window (RMS per 0.5 s window: silent / tone / tone / silent), and an overlay over a primary must be 1/√2 of its solo level (preview equal-power parity). Also covers mask-less PCM decoding through `ensure_layout`. |

### `velocut-ui` — egui/eframe GUI application (binary = `velocut`)

| File | Purpose |
|------|---------|
| `main.rs` | Entry point: FFmpeg init, icon load, `eframe::run_native`, `fix_taskbar_icon()` (Win32 WS_EX_APPWINDOW + WM_SETICON). |
| `app.rs` | `VeloCutApp` — owns `ProjectState`, `AppContext`, modules, undo/redo stacks; implements `process_command()`, `build_encode_plan()`, `poll_media()`, drag-and-drop, main `logic()`/`ui()`. Split is atomic via pure helpers `split_target_at` / `split_clip_at` / `split_clip_and_partners` (transition key follows the cut to the tail; source attributes preserved; extracted-audio partners cut in lockstep); trim handlers call `sync_linked_clip`; `SetTransition` clamps durations via `clip_query::transition_boundary` + `clamped_secs`. `build_encode_plan()` sets `ClipSpec.timeline_start` (gaps preserved), drops transitions across gaps/lanes, and routes an extracted A partner whose window was nudged/trimmed off its V anchor through `AudioOverlay` with the V clip silenced — matching what preview plays. |
| `context.rs` | `AppContext` — `MediaWorker`, `CacheContext` (thumbnail/frame/bucket caches, persistent `scrub_textures` and `pb_textures` handles, 192MB ceiling), `PlaybackContext`, rodio `audio_stream`/`audio_sinks`. |
| `theme.rs` | Dark color palette constants; `configure_style()` — egui Visuals & spacing. |
| `build.rs` | Embeds Windows `.ico` icon via `winresource`. |
| `modules/mod.rs` | `EditorModule` trait (ui/tick), re-exports all panel modules. |
| `modules/timeline.rs` | `TimelineModule` — 4-track layout (V1/A1/V2/A2), ruler, clip thumbnails+waveforms, trim handles, drag-move, transition badges, volume/fade/color popups. |
| `modules/library.rs` | `LibraryModule` — thumbnail card grid, multi-select, drag-to-timeline, context menus, rfd file import, probe-priority tracking. |
| `modules/preview_module.rs` | `PreviewModule` — video canvas (center-cropped to AR), transport bar, volume slider, timecode. |
| `modules/export_module.rs` | `ExportModule` — filename/quality/FPS/AR settings, presets (480p–4K), HW capability annotation, render progress modal, uninstall button. |
| `modules/audio_module.rs` | `AudioModule` — rodio per-clip WAV sinks, seek, fade ramps, mix normalization (1/√N), exhausted-clip detection, soft drain. |
| `modules/video_module.rs` | `VideoModule` — 3-layer scrub (cached bucket → exact decode → coarse prefetch → idle HQ), playback start/stop with centered-transition blend spec, PTS gating, clip-change eviction, prebuffer look-ahead, and the per-clip playback texture upload (`upload_playback_texture`). |
| `helpers/clip_query.rs` | Clip lookup helpers replacing inline filter chains: `video_clip_at_time` (video-row-aware playhead lookup), `active_transition_at` (clamped centered zone), `transition_boundary`, `selected_timeline_clip`, `is_extracted_audio_clip`, `active_audio_clip`, `active_overlay_clips`. |
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
