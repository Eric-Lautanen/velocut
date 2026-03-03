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
| `filters/mod.rs` | Filter registry. `declare_filters!` macro generates `FilterKind` enum + `presets()` list. `FilterParams` is a plain serde struct `{kind, brightness, contrast, saturation, gamma, hue, temperature, strength}` stored on `TimelineClip`. `FilterParams::none()` = all defaults, zero-cost skip. `FilterParams::from_preset(kind)` returns canonical values for each named preset. `is_identity()` — returns true when all params are at default so callers can skip processing. |
| `filters/helpers.rs` | Pure pixel math, no FFmpeg. `apply_filter_rgba(pixels, w, h, params)` — rayon `par_chunks_mut(4)` over RGBA bytes: brightness/contrast/gamma on RGB channels, saturation via luma-weighted desaturate blend, hue rotation via RGB→HSV→RGB, temperature via R/B channel nudge. `apply_filter_yuv(y_plane, u_plane, v_plane, params)` — YUV420P in-place for the encode path: Y plane = luma ops (brightness/contrast/gamma), UV planes = chroma ops (saturation scale, hue rotation on UV directly). |

---

## velocut-media/src/

| File | What it does |
|------|-------------|
| `lib.rs` | Crate root — `pub mod` for `audio`, `decode`, `encode`, `probe`, `waveform`, `worker`; `mod helpers` (private, not re-exported). Re-exports `ClipSpec`, `EncodeSpec`, `MediaWorker`, `MediaResult`, `PlaybackFrame`. |
| `worker.rs` | `MediaWorker` — top-level thread/channel coordinator. Owns: scrub condvar slot (latest-wins), pb decode thread (32-frame bounded), probe semaphore (max 4), shared `result_rx`, dedicated `scrub_rx` (cap=8, bypass), `encode_cancels` map. Key methods: `probe_clip`, `request_frame` (L2 scrub, aspect>0), `request_frame_hq` (L3 idle, aspect=0, one-shot thread→scrub_rx), `request_transition_frame` (decode×2 + blend → scrub_rx), `start_playback`, `start_blend_playback`, `stop_playback` (drains pb_rx), `start_encode`, `cancel_encode`. `blend_rgba_transition(a,b,w,h,alpha,kind)` — CPU RGBA blend dispatcher. |
| `encode.rs` | `encode_timeline` — blocking encode thread. `ClipSpec {path, source_offset, duration, volume, skip_audio, filter}`, `EncodeSpec {job_id, clips, w, h, fps, output, transitions}`. HW fallback chain: `h264_nvenc → h264_amf → h264_qsv → libx264`. `apply_encoder_quality_opts` — per-vendor quality params. `CropScaler`: center-crop, `srcSliceY=0`. Post-scaler: calls `apply_filter_yuv()` when `filter` is not identity. `AV_CODEC_FLAG_GLOBAL_HEADER` + `g=fps` before `open_as_with`. Decoder flush uses `VideoFrame::new(YUV420P,w,h)` not `empty()`. |
| `decode.rs` | **Decode pipeline.** `LiveDecoder` — stateful per-clip decoder. `open(path, ts, aspect, cached_scaler, forced_size, preview_size)` — 6 args. Resolution priority: `forced_size` > `aspect>0` (320px scrub) > `preview_size Some` > native. HW decode: D3D11VA + NVDEC/CUVID. `ensure_cpu_frame()` transfers D3D11/CUDA→CPU NV12. `skip_until_pts` for fast burn. `decode_one_frame_rgba(path,ts)` — one-shot 320px no-hwaccel for transition scrub. `decode_frame(path, ts, aspect, preview_size)` — 4 resolution cases. Post-decode: calls `apply_filter_rgba()` on RGBA buffer when filter is not identity. |
| `probe.rs` | `probe_duration`, `probe_video_size_and_thumbnail`. SwsContext built lazily from first decoded frame (AVCC/Annex-B compat). |
| `audio.rs` | FFmpeg-in-process WAV extraction (no child process — avoids MSYS2 PATH issues on double-click launch). `extract_audio(path, id, source_offset, duration, tx)` — decodes audio stream, resamples to 44100 Hz / stereo / f32le (`OUT_FMT=F32(Packed)`), writes temp WAV to `%TEMP%\velocut_audio_<uuid>.wav` with placeholder RIFF sizes fixed up after streaming, sends `MediaResult::AudioPath`. Trims pre-roll samples to align audio to `source_offset`. `cleanup_audio_temp(path)` — deletes files matching `velocut_audio_*` pattern in OS temp dir. |
| `waveform.rs` | `extract_waveform(path, id, tx)` — FFmpeg-in-process audio decode, downmix to mono, compute per-column peak over `WAVEFORM_COLS=4000` blocks, send `MediaResult::Waveform`. `append_frame_samples` handles packed/planar variants of F32, I16, I32, F64, U8. |
| `helpers/mod.rs` | `pub mod yuv; pub mod seek;` — internal only, not re-exported from `lib.rs`. |
| `helpers/seek.rs` | `seek_to_secs(ictx,ts,label)` — skips if `ts<=0.0` (Windows EPERM). All seeks must go here. |
| `helpers/yuv.rs` | Packed stride-free YUV420P: `extract_yuv`, `write_yuv`, `blend_yuv_frame`. |

---

## velocut-ui/src/

| File | What it does |
|------|-------------|
| `main.rs` | Binary entry point. `ffmpeg_the_third::init()`, `load_icon()` via `png` crate from `assets/linux/icon-256.png`. `NativeOptions`: centered, 1465x965, min 900x600, `with_decorations(false)`, resizable. `egui_extras::install_image_loaders` called here only (not in `VeloCutApp::new`). `fix_taskbar_icon()` — Windows-only: `EnumThreadWindows` patches `WS_EX_APPWINDOW` and propagates class `HICON` to window instance so borderless `WS_POPUP` windows show correctly in the taskbar and alt-tab. No extra crate — uses `user32`/`kernel32` always linked on Windows. |
| `app.rs` | `VeloCutApp` root. `process_command()` handles all `EditorCommand` variants including `SetClipFilter`. `poll_media()` drives result ingestion. `update()` is the egui frame loop. `begin_render()` builds ClipSpecs (filters extracted audio, handles muted V-row volumes, passes `filter` into `ClipSpec`). `restore_snapshot()` re-queues probes for missing waveforms. |
| `context.rs` | `AppContext` — runtime state not in `ProjectState`. Scrub tracking (`last_frame_req`, `scrub_last_moved`), playback tracking (`playback_media_id`, `prev_playing`), caches (`thumbnail_cache`, `frame_cache`, `frame_bucket_cache` byte-capped ~192MB), `pending_pb_frame` single-slot, audio (`audio_stream`, `audio_sinks`, `audio_overlay_sinks`). `ingest_media_results()`: drains `scrub_rx` first, then `rx`. |
| `theme.rs` | All `Color32` palette constants: `ACCENT/ACCENT_DIM/ACCENT_HOVER`, `RENDER_BTN`, `ACTION_BTN_FILL/STROKE`, `PLAYHEAD_BTN_FILL/STROKE`, `DARK_BG_0/1/2/3/4`, `DARK_TEXT/DARK_TEXT_DIM`, `DARK_BORDER`, `CLIP_VIDEO/AUDIO/SELECTED`, `SEL_MULTI`, `SEL_CHECK`, `ACCENT_DUR`. `configure_style(ctx)` — sets egui `Visuals` + `Style`: spacing, button padding, scroll bar, corner radii, all widget state colors (noninteractive/inactive/hovered/active/open), `override_text_color`. Single source of truth for the UI palette. |
| `helpers/mod.rs` | `pub mod clip_query, format, log, reset, memory_manager;` — no logic. |
| `helpers/clip_query.rs` | Clip lookup helpers: `timeline_clip`, `selected_timeline_clip`, `clip_at_time`, `library_clip`, `library_entry_for`, `selected_clip_library_entry`, `is_extracted_audio_clip` (odd row + `linked_clip_id`), `linked_audio_clip`, `playhead_source_timestamp`. `active_transition_at()` — `TransitionZone` centered on cut `[clip_a_end-D/2, clip_a_end+D/2)`, uses `match...continue` (not `?`) in pair loop. `active_audio_clip` — priority: extracted A-row (has `linked_clip_id`) over V-row (non-muted). `active_overlay_clips` — standalone A-row clips (no `linked_clip_id`) that mix additively with primary audio. |
| `helpers/format.rs` | `fit_label(text, max_px)` — truncates with ellipsis using 6.5 px/char heuristic; no egui `Fonts` required. `truncate(s, max)` — byte-boundary-safe UTF-8 clipping for library card names. Both have unit tests. |
| `helpers/log.rs` | `vlog(msg)` — writes timestamped lines to `%TEMP%\velocut.log` via `OnceLock<Option<Mutex<File>>>` (file opened once at first call, held for process lifetime to avoid per-call syscall overhead on high-frequency paths). `velocut_log!(...)` macro for format-string convenience. In release `windows_subsystem="windows"` builds there is no console; this file is the only log output. |
| `helpers/memory_manager.rs` | `MemoryManager` — two-stage proactive cache eviction. Stage 1 (2s scrub idle): evict `frame_bucket_cache` outside +-5s window around playhead. Stage 2 (30s deep idle): clear `frame_cache`, `frame_bucket_cache`, `scrub_textures`; call `ctx.forget_all_images()`; replace `egui::Memory` with default preserving `options`. Activity detection resets timers on playhead move, play/pause, import, or active encode; encode-finish also resets. Thumbnail cap: evict oldest beyond `MAX_THUMBNAILS=100` each tick. `CacheContext::evict_outside_window(keep_min, keep_max)` defined here as an extension impl. Requires `frame_cache_bytes` to be `pub(crate)` in `context.rs`. |
| `helpers/reset.rs` | App reset / uninstall helpers. `delete_app_data_dir()` — removes `%APPDATA%\VeloCut\` (walks up one level from `eframe::storage_dir`). `delete_temp_files()` — sweeps `velocut_*` and `velocut.log` from OS temp dirs; on Windows also scans `%LOCALAPPDATA%\Temp` to cover the MSYS2 path split (MSYS2 `temp_dir()` → `C:\msys64\tmp\`, not real Windows temp). `reset_context(context, ctx)` — soft in-memory teardown: `stop_playback` → drop audio sinks (before stream — order is load-bearing in rodio) → drop `audio_stream` → `cache.clear_all()` → `playback.reset()` → clear egui memory data. Does NOT shut down worker threads (safe for "keep using" path). `schedule_app_data_dir_deletion(context)` — calls `reset_context`, then `worker.shutdown()`, then `delete_app_data_dir()`, then `std::process::exit(0)`. `show_uninstall_modal(ctx, visible)` — full-screen scrim + card overlay (`egui::Order::Tooltip`) with "Close VeloCut" / "Keep Using" buttons. |
| `modules/mod.rs` | Re-exports all module structs; defines the `EditorModule` trait (`name`, `ui`). `ThumbnailCache` type alias lives here. |
| `modules/timeline.rs` | `TimelineModule` — toolbar, ruler, clip rendering, trim handles, DnD drop zone, transition badges, volume/fade popup (`vol_popup`), filter popup (`filter_popup`), hotkey popup, playhead. Filter badge (`🎨`) sits next to the speaker badge; accent-colored when filter is not identity, dim otherwise. |
| `modules/video_module.rs` | **Playback + scrub orchestration.** `tick(state, ctx, egui_ctx)` — 3-layer scrub (L1 bucket cache, L2 `request_frame`, L3 `request_frame_hq` at 150ms idle). Passes `preview_size` from `preview_module.last_render_size` to `start_playback`/`start_blend_playback`. `poll_playback()` — PTS-gated single-slot promotion, clip-transition eviction at top. `build_blend_spec` / `build_incoming_blend_spec` — determine `PlaybackTransitionSpec` for outgoing/incoming transition halves. |
| `modules/preview_module.rs` | Renders current frame texture. `last_render_size: Option<(u32,u32)>` written each frame. `crop_uv_rect(tex_w,tex_h,target_ar)` — GPU-side center-crop for AR mismatch. Transport bar. |
| `modules/export_module.rs` | Export UI: filename, quality preset, fps, aspect. Two-stage reset (5s). States: Idle / Encoding (progress+Stop) / Done / Error. |
| `modules/library.rs` | `LibraryModule { multi_selection: HashSet<Uuid> }` — not a unit struct. Grid uses `chunks(cols)+ui.horizontal()` (not `horizontal_wrapped`). DnD write on drag start only. |
| `modules/audio_module.rs` | Sole owner of rodio audio sinks. All audio tick logic here only. |

---

## Key Pipelines

### Decode Pipeline (playback)
`video_module::tick()` → `worker::start_playback/start_blend_playback` → `PlaybackCmd::Start/StartBlend` (carries `preview_size`) → pb thread → `LiveDecoder::open(path,ts,0.0,None,forced_size,preview_size)` → `next_frame()` → `ensure_cpu_frame()` if HW → sws_scale → RGBA `Vec<u8>` → **`apply_filter_rgba()` if not identity** → `pb_tx` → `poll_playback()` → `pending_pb_frame` → `frame_cache` → `preview_module.current_frame`

### Decode Pipeline (scrub)
**L2**: `tick()` → `worker::request_frame(aspect=ratio)` → condvar scrub slot → decode thread → `LiveDecoder::open(aspect>0 → 320px)` → RGBA → **`apply_filter_rgba()` if not identity** → `scrub_rx`
**L3**: `tick()` after 150ms idle → `worker::request_frame_hq(id,path,ts,preview_size)` → one-shot thread → `decode_frame(aspect=0.0,preview_size)` → RGBA → **`apply_filter_rgba()` if not identity** → `scrub_rx` → `ingest_media_results()` → `frame_cache`
**Transition scrub**: `tick()` sees `active_transition_at` → `worker::request_transition_frame(TransitionScrubRequest)` → one-shot thread → `decode_one_frame_rgba×2` → `blend_rgba_transition` → **`apply_filter_rgba()` on each side** → `scrub_rx`

### Encode Pipeline
`app::begin_render()` → `worker::start_encode(EncodeSpec)` → own thread → `encode_timeline()` → per-clip: open source decoder → `CropScaler` → **`apply_filter_yuv()` if not identity** → `VideoTransition::apply()` (YUV420P blend at boundaries) → HW encoder (`h264_nvenc→amf→qsv→libx264`) → mux audio → `EncodeProgress/Done/Error` → shared `result_rx` → `ingest_media_results()`

### Transition Blend (playback)
pb thread enters blend zone (`ts >= blend_start_ts`) → lazy open `decoder_b` with `forced_size=primary_dims` → `skip_until_pts` fast burn → during burn: send `held_blend` frozen frame → post-burn: `blend_rgba_transition(data_a, data_b, w, h, alpha, kind)` → `apply_rgba()` (rayon parallel rows) → blended RGBA frame

---

## Adding a Transition
1. `transitions/myname.rs` — impl `VideoTransition` (6 methods). `apply`→YUV420P. `apply_rgba`→RGBA w/ `par_chunks_mut`.
2. One line in `declare_transitions!`: `myname::MyName => MyKind,`

## Adding a Filter Preset
1. Add a variant to `declare_filters!` in `filters/mod.rs`.
2. Add its `FilterParams` values to the `from_preset()` match arm.
That's it — no other files need changing.

## Adding a Feature
`commands.rs` variant → `modules/mymodule.rs` → `modules/mod.rs` → `VeloCutApp` field → `update()` → `process_command()` → optional `MediaResult` variants in `media_types.rs` + `ingest_media_results()`.