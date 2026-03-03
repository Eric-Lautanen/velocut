# VeloCut — Compact File Reference

> Stack: Rust, Windows MINGW64, eframe/egui 0.33, ffmpeg-the-third 4 (custom fork), crossbeam-channel, rodio, rayon.
> Workspace: `velocut-core` (no UI/FFmpeg) → `velocut-media` (FFmpeg workers) → `velocut-ui` (egui app).

---

## velocut-core/src/

| File | What it does |
|------|-------------|
| `lib.rs` | Crate root — `pub mod` for `commands`, `helpers`, `media_types`, `state`, `transitions`, `filters`. No logic. |
| `commands.rs` | `EditorCommand` enum — all UI→logic intents (SetTransition, Undo/Redo, RenderMP4, CancelEncode, ExtractAudioTrack, SetClipVolume, SetClipFilter, etc.) |
| `state.rs` | `ProjectState` (serde) — owns `library`, `timeline`, `transitions`, playback fields, undo/redo. Key methods: `add_to_timeline`, `total_duration`. `TimelineClip` has `volume`, `audio_muted`, `linked_clip_id`, `filter: FilterParams`. |
| `media_types.rs` | Result/message types crossing thread boundaries: `MediaResult` variants (Duration, Thumbnail, Waveform, VideoFrame, AudioPath, EncodeProgress/Done/Error…), `PlaybackFrame` (dedicated pb channel), `TransitionScrubRequest` (scrub blend decode), `PlaybackTransitionSpec` (blend playback config with `alpha_start`/`invert_ab`). |
| `helpers/mod.rs` | `pub mod geometry; pub mod time;` — no logic. |
| `helpers/time.rs` | `format_time`, `format_duration` |
| `helpers/geometry.rs` | `aspect_ratio_value`, `aspect_ratio_label` |
| `transitions/mod.rs` | Transition registry. `declare_transitions!` macro generates `TransitionKind` enum + `registered()`/`registry()`. `TransitionType` is a plain struct `{kind, duration_secs}` — not an enum. `VideoTransition` trait: `apply()` → YUV420P (encode path), `apply_rgba()` → RGBA w/ rayon `par_chunks_mut` (playback/scrub path). |
| `transitions/helpers.rs` | Easing fns, `frame_alpha`, `blend_byte/buffers`, YUV plane layout utils (`split_planes`, `chroma_dims`, `u_offset`, `v_offset`), spatial helpers (`norm_xy`, `center_dist`, `wipe_alpha`), samplers (`sample_plane`, `sample_plane_clamped`). |
| `filters/mod.rs` | Filter registry. `declare_filters!` macro generates `FilterKind` enum + `all()` slice. `FilterParams` is a plain serde struct `{kind, brightness, contrast, saturation, gamma, hue, temperature, strength}` stored on `TimelineClip`. `FilterParams::none()` = all defaults, zero-cost skip. `FilterParams::from_preset(kind)` returns canonical values. `is_identity()` returns true when all params are at default so callers skip processing. `apply_strength()` blends params toward identity by `1-strength`. **15 presets**: None, Cinematic, Vintage, Cool, Vivid, BlackAndWhite, Faded, GoldenHour, NightBlue, Punchy, FalseColor, Infrared, Mist, Noir, TealOrange. |
| `filters/helpers.rs` | Pure pixel math, no FFmpeg. `apply_filter_rgba(pixels, params)` — rayon `par_chunks_mut(4)` over RGBA bytes: brightness/contrast/gamma on RGB, saturation via luma-weighted desaturate blend, hue rotation via RGB→HSV→RGB, temperature via R/B channel nudge. `apply_filter_yuv(y_plane, u_plane, v_plane, params)` — YUV420P in-place for encode path: Y plane = luma ops, UV planes = chroma ops (saturation scale, hue rotation on UV directly). |

---

## velocut-media/src/

| File | What it does |
|------|-------------|
| `lib.rs` | Crate root — `pub mod` for `audio`, `decode`, `encode`, `probe`, `waveform`, `worker`; `mod helpers` (private). Re-exports `ClipSpec`, `EncodeSpec`, `MediaWorker`, `MediaResult`, `PlaybackFrame`. |
| `worker.rs` | `MediaWorker` — top-level thread/channel coordinator. Owns: scrub condvar slot (latest-wins), pb decode thread (32-frame bounded), probe semaphore (max 4), shared `result_rx`, dedicated `scrub_rx` (cap=8, bypass), `encode_cancels` map. Key methods: `probe_clip`, `request_frame` (L2 scrub, aspect>0), `request_frame_hq` (L3 idle, aspect=0, one-shot thread→scrub_rx), `request_transition_frame` (decode×2 + blend → scrub_rx), `start_playback`, `start_blend_playback`, `stop_playback` (drains pb_rx), `start_encode`, `cancel_encode`. `blend_rgba_transition(a,b,w,h,alpha,kind)` — CPU RGBA blend dispatcher. |
| `encode.rs` | `encode_timeline` — blocking encode thread. `ClipSpec {path, source_offset, duration, volume, skip_audio, filter}`, `EncodeSpec {job_id, clips, w, h, fps, output, transitions}`. HW fallback chain: `h264_nvenc → h264_amf → h264_qsv → libx264`. `CropScaler`: center-crop, `srcSliceY=0`. Post-scaler: calls `apply_filter_yuv()` when `filter` is not identity. `AV_CODEC_FLAG_GLOBAL_HEADER` + `g=fps` before `open_as_with`. Decoder flush uses `VideoFrame::new(YUV420P,w,h)` not `empty()`. |
| `decode.rs` | `LiveDecoder` — stateful per-clip decoder. `open(path, ts, aspect, cached_scaler, forced_size, preview_size)` — 6 args. Resolution priority: `forced_size` > `aspect>0` (320px scrub) > `preview_size Some` > native. HW decode: D3D11VA + NVDEC/CUVID. `ensure_cpu_frame()` transfers D3D11/CUDA→CPU NV12. `skip_until_pts` for fast burn. `decode_one_frame_rgba(path,ts)` — one-shot 320px no-hwaccel for transition scrub. `decode_frame(path, ts, aspect, preview_size)` — 4 resolution cases. Post-decode: calls `apply_filter_rgba()` on RGBA buffer when filter is not identity. |
| `probe.rs` | `probe_duration`, `probe_video_size_and_thumbnail`. SwsContext built lazily from first decoded frame. |
| `audio.rs` | FFmpeg-in-process WAV extraction. `extract_audio(path, id, source_offset, duration, tx)` — decodes, resamples to 44100 Hz / stereo / f32le, writes temp WAV to `%TEMP%\velocut_audio_<uuid>.wav`, sends `MediaResult::AudioPath`. `cleanup_audio_temp(path)` — deletes `velocut_audio_*` in OS temp. |
| `waveform.rs` | `extract_waveform(path, id, tx)` — FFmpeg audio decode, downmix mono, compute per-column peak over `WAVEFORM_COLS=4000` blocks, send `MediaResult::Waveform`. |
| `helpers/mod.rs` | `pub mod yuv; pub mod seek;` — internal only. |
| `helpers/seek.rs` | `seek_to_secs(ictx,ts,label)` — skips if `ts<=0.0` (Windows EPERM). All seeks must go here. |
| `helpers/yuv.rs` | Packed stride-free YUV420P: `extract_yuv`, `write_yuv`, `blend_yuv_frame`. |

---

## velocut-ui/src/

| File | What it does |
|------|-------------|
| `main.rs` | Binary entry point. `ffmpeg_the_third::init()`, `load_icon()`, `NativeOptions`: centered, 1465x965, min 900x600, `with_decorations(false)`, resizable. `fix_taskbar_icon()` — Windows-only: `EnumThreadWindows` patches `WS_EX_APPWINDOW`. |
| `app.rs` | `VeloCutApp` root. `process_command()` handles all `EditorCommand` variants. `SetClipFilter` arm evicts `frame_cache` and `frame_bucket_cache` for the affected `media_id` and resets `last_frame_req` so `video_module::tick()` re-issues a scrub decode with the new filter applied on the very next frame. `begin_render()` passes `filter` into each `ClipSpec`. |
| `context.rs` | `AppContext` — runtime state. Scrub/playback tracking, caches (`thumbnail_cache`, `frame_cache`, `frame_bucket_cache` byte-capped ~192MB), `pending_pb_frame` single-slot, audio sinks. `ingest_media_results()`: drains `scrub_rx` first (scrub frames high-priority), then shared `rx`. `ingest_video_frame()` applies the active clip's `FilterParams` to raw RGBA bytes before GPU upload (covers scrub + HQ one-shot paths). |
| `theme.rs` | All `Color32` palette constants. `configure_style(ctx)` — single source of truth for the UI palette. |
| `helpers/mod.rs` | `pub mod clip_query, format, log, reset, memory_manager;` |
| `helpers/clip_query.rs` | Clip lookup helpers: `clip_at_time`, `library_entry_for`, `is_extracted_audio_clip`, `active_transition_at`, `active_audio_clip`, `active_overlay_clips`, `playhead_source_timestamp`. |
| `helpers/format.rs` | `fit_label`, `truncate` — UTF-8-safe, unit-tested. |
| `helpers/log.rs` | `velocut_log!(...)` macro → `%TEMP%\velocut.log` via `OnceLock<Mutex<File>>`. |
| `helpers/memory_manager.rs` | Two-stage proactive cache eviction. Stage 1 (2s scrub idle): evict `frame_bucket_cache` outside ±5s window. Stage 2 (30s deep idle): clear all frame caches + `forget_all_images`. Thumbnail cap: evict oldest beyond `MAX_THUMBNAILS=100`. |
| `helpers/reset.rs` | `reset_context`, `delete_app_data_dir`, `delete_temp_files`, `schedule_app_data_dir_deletion`, `show_uninstall_modal`. |
| `modules/mod.rs` | Re-exports all module structs; `EditorModule` trait; `ThumbnailCache` type alias. |
| `modules/timeline.rs` | Toolbar, ruler, clip rendering, trim handles, DnD drop zone, transition badges, volume/fade popup, hotkey popup, playhead. **Filter badge** (`🎨`) per video clip — accent-colored when filter is not identity. **Filter popup**: 3-column uniform-width preset grid (`add_sized` inside `ui.horizontal` rows via `chunks(3)` — not `Grid`, which auto-sizes columns), inline strength row (label + full-width slider + `%` value), manual sliders (label 50px | `add_sized` slider 80px | value 38px). Changing any filter value pushes `SetClipFilter` which triggers immediate cache eviction + re-decode in `app.rs`. |
| `modules/video_module.rs` | Playback + scrub orchestration. `tick()` — 3-layer scrub (L1 bucket cache, L2 `request_frame`, L3 `request_frame_hq` at 150ms idle). `poll_playback()` — PTS-gated single-slot promotion; applies active clip `FilterParams` to raw `pb_rx` RGBA bytes before `load_texture` (playback filter path). `build_blend_spec` / `build_incoming_blend_spec` — `PlaybackTransitionSpec` for transition halves. |
| `modules/preview_module.rs` | Renders current frame. `last_canvas_size` written each frame for `VideoModule::tick`. `crop_uv_rect` — GPU center-crop. Transport bar. |
| `modules/export_module.rs` | Export UI: filename, quality preset, fps, aspect. Two-stage reset. |
| `modules/library.rs` | `LibraryModule { multi_selection: HashSet<Uuid> }`. Grid uses `chunks(cols)+ui.horizontal()`. |
| `modules/audio_module.rs` | Sole owner of rodio audio sinks. All audio tick logic here only. |

---

## Key Pipelines

### Decode Pipeline (playback)
`video_module::tick()` → `worker::start_playback/start_blend_playback` → pb thread → `LiveDecoder::open()` → `next_frame()` → `ensure_cpu_frame()` → sws_scale → RGBA → **`apply_filter_rgba()` if not identity** → `pb_tx` → `poll_playback()` applies filter again on raw bytes before `load_texture` → `frame_cache` → `preview_module.current_frame`

### Decode Pipeline (scrub)
**L2**: `tick()` → `worker::request_frame(aspect=ratio)` → condvar scrub slot → `LiveDecoder::open(aspect>0 → 320px)` → RGBA → **`apply_filter_rgba()` if not identity** → `scrub_rx`
**L3**: `tick()` after 150ms idle → `worker::request_frame_hq()` → one-shot thread → `decode_frame(aspect=0.0, preview_size)` → RGBA → **`apply_filter_rgba()` if not identity** → `scrub_rx` → `ingest_video_frame()` → `frame_cache`
**Transition scrub**: `tick()` sees `active_transition_at` → `worker::request_transition_frame()` → `decode_one_frame_rgba×2` → `blend_rgba_transition` → `scrub_rx`

### Encode Pipeline
`app::begin_render()` → `worker::start_encode(EncodeSpec)` → `encode_timeline()` → per-clip: open decoder → `CropScaler` → **`apply_filter_yuv()` if not identity** → `VideoTransition::apply()` (YUV420P blend at boundaries) → HW encoder → mux audio → `EncodeProgress/Done/Error`

### Transition Blend (playback)
pb thread enters blend zone → lazy open `decoder_b` → `skip_until_pts` → `blend_rgba_transition(data_a, data_b, w, h, alpha, kind)` → `apply_rgba()` (rayon parallel rows) → blended RGBA frame

### Filter Change (realtime preview)
User moves slider in filter popup → `SetClipFilter` cmd → `app::process_command()` evicts `frame_cache[media_id]` + `frame_bucket_cache` entries for that clip + resets `last_frame_req = None` → `video_module::tick()` sees scrub_moved=true next frame → fires `request_frame` → decoded RGBA has filter applied → `scrub_rx` → `ingest_video_frame()` → `frame_cache` → preview updates within one frame

---

## Adding a Transition
1. `transitions/myname.rs` — impl `VideoTransition`. `apply`→YUV420P. `apply_rgba`→RGBA w/ `par_chunks_mut`.
2. One line in `declare_transitions!`: `myname::MyName => MyKind,`

## Adding a Filter Preset
1. Add variant to `declare_filters!` in `filters/mod.rs`.
2. Add `FilterParams` values to the `from_preset()` match arm.
No other files need changing.

## Adding a Feature
`commands.rs` variant → `modules/mymodule.rs` → `modules/mod.rs` → `VeloCutApp` field → `update()` → `process_command()` → optional `MediaResult` variants in `media_types.rs` + `ingest_media_results()`.