# VeloCut — Compact File Reference

> Stack: Rust, Windows MINGW64, eframe/egui 0.33, ffmpeg-the-third 4 (custom fork), crossbeam-channel, rodio, rayon.
> Workspace: `velocut-core` (no UI/FFmpeg) → `velocut-media` (FFmpeg workers) → `velocut-ui` (egui app).

---

## velocut-core/src/

| File | What it does |
|------|-------------|
| `lib.rs` | Crate root — `pub mod` for `commands`, `helpers`, `media_types`, `state`, `transitions`, `filters`. No logic. |
| `commands.rs` | `EditorCommand` enum — all UI→logic intents (SetTransition, Undo/Redo, RenderMP4, CancelEncode, ExtractAudioTrack, SetClipVolume, SetClipFadeIn, SetClipFadeInStart, SetClipFadeOut, SetClipFadeOutEnd, SetClipFilter, etc.) |
| `state.rs` | `ProjectState` (serde) — owns `library`, `timeline`, `transitions`, playback fields, undo/redo. Key methods: `add_to_timeline`, `total_duration`. `TimelineClip` has `volume`, `audio_muted`, `linked_clip_id`, `filter: FilterParams`, `fade_in_secs`, `fade_in_start_secs`, `fade_out_secs`, `fade_out_end_secs`. |
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
| `worker.rs` | `MediaWorker` — top-level thread/channel coordinator. Owns: scrub condvar slot (latest-wins), pb decode thread (3-frame bounded, preview-res), probe semaphore (max 4), shared `result_rx`, dedicated `scrub_rx` (cap=8, bypass), `encode_cancels` map, `hq_sem` (cap=2). Key methods: `probe_clip`, `request_frame` (L2 scrub, aspect>0), `request_frame_hq` (L3 idle, aspect=0, one-shot thread→scrub_rx), `request_transition_frame` (decode×2 + blend → scrub_rx), `start_playback`, `start_blend_playback`, `stop_playback` (drains pb_rx), `start_encode`, `cancel_encode`. `blend_rgba_transition(a,b,w,h,alpha,kind)` — CPU RGBA blend dispatcher. |
| `encode.rs` | `encode_timeline` — blocking encode thread. `ClipSpec {path, source_offset, duration, volume, skip_audio, fade_in_secs, fade_in_start_secs, fade_out_secs, fade_out_end_secs, filter}`, `EncodeSpec {job_id, clips, w, h, fps, output, transitions, audio_overlays}`. HW fallback chain: AMF (D3D11) → NVENC (CUDA) → VAAPI → VideoToolbox → libx264. `CropScaler`: center-crop, `srcSliceY=0`. Post-scaler: calls `apply_filter_yuv()` when `filter` is not identity. **Filter in transitions**: `tail_spec` inherits `clip.filter.clone()`, `head_spec` inherits `next_clip.filter.clone()` — so each side of the transition overlap carries its clip's color grade. `decode_clip_frames()` calls `apply_filter_to_yuv_frame()` after scaling in both the packet loop and EOF drain. Per-clip audio fade envelope via `fade_gain()` (equal-power sqrt ramp, four params: `fade_in_start_secs`, `fade_in_secs`, `fade_out_secs`, `fade_out_end_secs`) applied per-decoded-frame for both clip and overlay paths. `flush_audio_resampler()` null-frame SwrContext flush after decoder EOF (1080p audio dropout fix). Sends `EncodeProgress` every 15 frames. `AV_CODEC_FLAG_GLOBAL_HEADER` + `g=fps` before `open_as_with`. |
| `decode.rs` | `LiveDecoder` — stateful per-clip decoder. `open(path, ts, aspect, cached_scaler, forced_size)`. Resolution priority: `forced_size` > `aspect>0` (320px) > native. HW decode: D3D11VA. `ensure_cpu_frame()` transfers D3D11→CPU NV12. `skip_until_pts` for fast GOP burn. `decode_one_frame_rgba(path,ts)` — one-shot 320px for transition scrub. |
| `probe.rs` | `probe_duration`, `probe_video_size_and_thumbnail`. SwsContext built lazily from first decoded frame. Non-video streams discarded to prevent audio packet buffering. |
| `audio.rs` | FFmpeg-in-process WAV extraction. `extract_audio(path, id, source_offset, duration, tx)` — decodes, resamples to 44100 Hz / stereo / f32le, writes temp WAV. `cleanup_audio_temp(path)` — deletes `velocut_audio_*` in OS temp. |
| `waveform.rs` | `extract_waveform(path, id, tx)` — FFmpeg audio decode, downmix mono, 4000-column peak array → `MediaResult::Waveform`. |
| `helpers/seek.rs` | `seek_to_secs(ictx,ts,label)` — skips if `ts<=0.0` (Windows EPERM). Backward seek + PTS filter. All seeks must go here. |
| `helpers/yuv.rs` | Packed stride-free YUV420P: `extract_yuv`, `write_yuv`. All encode and crossfade paths go through these. |

---

## velocut-ui/src/

| File | What it does |
|------|-------------|
| `main.rs` | Binary entry point. `ffmpeg_the_third::init()`, `load_icon()`, `NativeOptions`: centered, 1465x965, min 900x600, `with_decorations(false)`, resizable. `fix_taskbar_icon()` — Windows-only: `EnumThreadWindows` patches `WS_EX_APPWINDOW`. |
| `app.rs` | `VeloCutApp` root. `process_command()` handles all `EditorCommand` variants. `SetClipFilter` arm evicts `frame_cache` and `frame_bucket_cache` for the affected `media_id` and resets `last_frame_req` so re-decode fires immediately. `SetClipFadeIn/FadeInStart/FadeOut/FadeOutEnd` arms update the respective `TimelineClip` field. `begin_render()` passes all four fade params and `filter` into each `ClipSpec`. |
| `context.rs` | `AppContext` — runtime state. Scrub/playback tracking, caches (`thumbnail_cache`, `frame_cache`, `frame_bucket_cache` byte-capped ~192MB), `pending_pb_frame` single-slot, audio sinks. `ingest_media_results()`: drains `scrub_rx` first (scrub frames high-priority), then shared `rx`. `ingest_video_frame()` applies the active clip's `FilterParams` to raw RGBA bytes before GPU upload (covers scrub + HQ one-shot paths). |
| `theme.rs` | All `Color32` palette constants. `configure_style(ctx)` — single source of truth for the UI palette. |
| `helpers/mod.rs` | `pub mod clip_query, format, log, reset, memory_manager;` |
| `helpers/clip_query.rs` | Clip lookup helpers: `clip_at_time`, `library_entry_for`, `is_extracted_audio_clip`, `active_transition_at`, `active_audio_clip`, `active_overlay_clips`, `playhead_source_timestamp`. |
| `helpers/format.rs` | `fit_label`, `truncate` — UTF-8-safe, unit-tested. |
| `helpers/log.rs` | `velocut_log!(...)` macro → `%TEMP%\velocut.log` via `OnceLock<Mutex<File>>`. |
| `helpers/memory_manager.rs` | Two-stage proactive cache eviction. Stage 1 (2s scrub idle): evict `frame_bucket_cache` outside ±5s window. Stage 2 (30s deep idle): clear all frame caches + `forget_all_images`. Thumbnail cap: evict oldest beyond `MAX_THUMBNAILS=100`. |
| `helpers/reset.rs` | `reset_context`, `delete_app_data_dir`, `delete_temp_files`, `schedule_app_data_dir_deletion`, `show_uninstall_modal`. |
| `modules/mod.rs` | Re-exports all module structs; `EditorModule` trait; `ThumbnailCache` type alias. |
| `modules/timeline.rs` | Toolbar, ruler, clip rendering, trim handles, DnD drop zone, transition badges, volume/fade popup, filter popup, hotkey popup, playhead. **Cross-track drag**: `target_row` computed from `interact_pointer_pos().y` each frame; enforced by `render_type` (video→even rows 0/2, audio→odd rows 1/3); `drag_target: Option<(Uuid, usize)>` stored for one-frame-ahead lane highlight; edge-snap neighbors filtered by target row not original row. **Playhead snap**: `video_clip_ends` uses `start_time + duration − (1/30)` so snapping lands on the last valid frame, not one past the end (prevents black-screen snap). **Volume/fade popup**: three-column panel — Fade In (ramp slider + delay slider) \| Volume (dB slider −60..+6) \| Fade Out (ramp slider + tail slider). **Filter badge** (`🎨`): accent-colored when filter not identity; popup has 3-column uniform-width preset grid (`add_sized` in `ui.horizontal` rows via `chunks(3)`) + strength slider + manual sliders (brightness/contrast/saturation/gamma/hue/temperature). Changing any filter/fade value pushes the corresponding command immediately. |
| `modules/video_module.rs` | Playback + scrub orchestration. `tick()` — 3-layer scrub (L1 bucket cache, L2 `request_frame`, L3 `request_frame_hq` at 150ms idle). `poll_playback()` — PTS-gated single-slot promotion; applies active clip `FilterParams` to raw `pb_rx` RGBA bytes before `load_texture`. `build_blend_spec` / `build_incoming_blend_spec` — `PlaybackTransitionSpec` for transition halves. |
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
`app::begin_render()` → `worker::start_encode(EncodeSpec)` → `encode_timeline()` → per-clip: open decoder → `CropScaler` → **`apply_filter_yuv()` if not identity** → `VideoTransition::apply()` (YUV420P blend at boundaries, each side with its own filter already baked in via `decode_clip_frames`) → HW encoder → mux audio (with `fade_gain()` envelope per sample) → `EncodeProgress/Done/Error`

### Transition Blend (playback)
pb thread enters blend zone → lazy open `decoder_b` → `skip_until_pts` → `blend_rgba_transition(data_a, data_b, w, h, alpha, kind)` → `apply_rgba()` (rayon parallel rows) → blended RGBA frame

### Filter Change (realtime preview)
User moves slider in filter popup → `SetClipFilter` cmd → `app::process_command()` evicts `frame_cache[media_id]` + `frame_bucket_cache` entries for that clip + resets `last_frame_req = None` → `video_module::tick()` sees scrub_moved=true next frame → fires `request_frame` → decoded RGBA has filter applied → `scrub_rx` → `ingest_video_frame()` → `frame_cache` → preview updates within one frame

### Audio Fade Envelope
`TimelineClip` carries four fade params → `SetClipFade*` commands update them → waveform redraws with amber ramp lines and blue silence strips (live in `draw_waveform`) → at export: `fade_gain(pts_secs, source_offset, duration, fi, fi_start, fo, fo_end)` called per decoded audio frame for both clip path and overlay path → equal-power sqrt ramp applied as scalar to `volume * fade_gain` before pushing to FIFO

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