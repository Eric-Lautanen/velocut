# VeloCut — Compact File Reference

> Stack: Rust, Windows MINGW64, eframe/egui 0.33, ffmpeg-the-third 4 (custom fork), crossbeam-channel, rodio, rayon.
> Workspace: `velocut-core` (no UI/FFmpeg) → `velocut-media` (FFmpeg workers) → `velocut-ui` (egui app).

---

## velocut-core/src/

| File | What it does |
|------|-------------|
| `commands.rs` | `EditorCommand` enum — all UI→logic intents (SetTransition, Undo/Redo, RenderMP4, CancelEncode, ExtractAudioTrack, SetClipVolume, etc.) |
| `state.rs` | `ProjectState` (serde) — owns `library`, `timeline`, `transitions`, playback fields, undo/redo. Key methods: `add_to_timeline`, `total_duration`. `TimelineClip` has `volume`, `audio_muted`, `linked_clip_id`. |
| `media_types.rs` | Result/message types crossing thread boundaries: `MediaResult` variants (Duration, Thumbnail, Waveform, VideoFrame, EncodeProgress/Done/Error…), `PlaybackFrame` (dedicated pb channel), `TransitionScrubRequest` (scrub blend decode), `PlaybackTransitionSpec` (blend playback config with `alpha_start`/`invert_ab`). |
| `helpers/time.rs` | `format_time`, `format_duration` |
| `helpers/geometry.rs` | `aspect_ratio_value`, `aspect_ratio_label` |
| `transitions/mod.rs` | Transition registry. `declare_transitions!` macro generates `TransitionKind` enum + `registered()`/`registry()`. `TransitionType` is a plain struct `{kind, duration_secs}` — not an enum. `VideoTransition` trait: `apply()` → YUV420P (encode path), `apply_rgba()` → RGBA w/ rayon `par_chunks_mut` (playback/scrub path). Current transitions: Crossfade, DipToBlack, Iris, Wipe, Push. |
| `transitions/helpers.rs` | Easing fns, `frame_alpha`, `blend_byte/buffers`, YUV plane layout utils (`split_planes`, `chroma_dims`, `u_offset`, `v_offset`), spatial helpers (`norm_xy`, `center_dist`, `wipe_alpha`), samplers (`sample_plane`, `sample_plane_clamped`). |

---

## velocut-media/src/

| File | What it does |
|------|-------------|
| `worker.rs` | `MediaWorker` — top-level thread/channel coordinator. Owns: scrub condvar slot (latest-wins), pb decode thread (32-frame bounded), probe semaphore (max 4), shared `result_rx`, dedicated `scrub_rx` (cap=8, bypass), `encode_cancels` map. Key methods: `probe_clip`, `request_frame` (L2 scrub, aspect>0), `request_frame_hq` (L3 idle, aspect=0, one-shot thread→scrub_rx), `request_transition_frame` (decode×2 + blend → scrub_rx), `start_playback`, `start_blend_playback`, `stop_playback` (drains pb_rx), `start_encode`, `cancel_encode`. `blend_rgba_transition(a,b,w,h,alpha,kind)` — CPU RGBA blend dispatcher (parallelism is inside each transition's `apply_rgba`). |
| `encode.rs` | `encode_timeline` — blocking encode thread. `ClipSpec {path, source_offset, duration, volume, skip_audio}`, `EncodeSpec {job_id, clips, w, h, fps, output, transitions}`. HW fallback chain: `h264_nvenc → h264_amf → h264_qsv → libx264`. `apply_encoder_quality_opts(opts, codec_name)` — per-vendor quality params (NVENC: `cq`; AMF: `qp_i/p/b`; QSV: `b:v`; x264: `crf`). `CropScaler`: center-crop, pre-advance data ptrs, `srcSliceY=0`. `AV_CODEC_FLAG_GLOBAL_HEADER` + `g=fps` applied before `open_as_with`. Decoder flush uses `VideoFrame::new(YUV420P,w,h)` not `empty()`. |
| `decode.rs` | **Decode pipeline.** `LiveDecoder` — stateful per-clip decoder. `open(path, ts, aspect, cached_scaler, forced_size, preview_size)` — 6 args. Resolution priority: `forced_size` > `aspect>0` (320px scrub) > `preview_size Some` (player panel dims) > native. HW decode: `hw_device_ctx` field; D3D11VA (all GPUs) + NVDEC/CUVID (NVIDIA). `ensure_cpu_frame()` transfers D3D11/CUDA frames to CPU NV12. Scaler rebuilt lazily on first frame (handles HW→NV12 format change). `skip_until_pts` field for decode-only fast burn (~4× faster). `decode_one_frame_rgba(path,ts)` — one-shot for scrub transition blend (320px, no hwaccel). `decode_frame(path, ts, aspect, preview_size)` — 4 cases: PNG-save→native; `aspect>0`→640px; `preview_size Some`→player dims; None→native. |
| `probe.rs` | `probe_duration`, `probe_video_size_and_thumbnail`. SwsContext built lazily from first decoded frame (never upfront — AVCC/Annex-B compat). |
| `helpers/seek.rs` | `seek_to_secs(ictx,ts,label)` — skips if `ts<=0.0` (Windows EPERM). All seeks must go here. |
| `helpers/yuv.rs` | Packed stride-free YUV420P: `extract_yuv`, `write_yuv`, `blend_yuv_frame`. |

---

## velocut-ui/src/

| File | What it does |
|------|-------------|
| `app.rs` | `VeloCutApp` root. `process_command()` handles all `EditorCommand` variants. `poll_media()` drives result ingestion. `update()` is the egui frame loop. `begin_render()` builds ClipSpecs (filters extracted audio, handles muted V-row volumes). `restore_snapshot()` re-queues probes for missing waveforms. |
| `context.rs` | `AppContext` — runtime state not in `ProjectState`. Scrub tracking (`last_frame_req`, `scrub_last_moved`), playback tracking (`playback_media_id`, `prev_playing`), caches (`thumbnail_cache`, `frame_cache`, `frame_bucket_cache` byte-capped ~192MB), `pending_pb_frame` single-slot, audio (`audio_stream`, `audio_sinks`). `ingest_media_results()`: drains `scrub_rx` first, then `rx`. |
| `helpers/clip_query.rs` | Clip lookup helpers: `timeline_clip`, `clip_at_time`, `library_entry_for`, `is_extracted_audio_clip`, `linked_audio_clip`, `active_audio_clip`, `playhead_source_timestamp`. `active_transition_at()` → `TransitionZone` centered on cut `[clip_a_end−D/2, clip_a_end+D/2)` — uses `match...continue` (not `?`) in pair loop. |
| `modules/video_module.rs` | **Playback + scrub orchestration.** `tick(state, ctx, egui_ctx)` — 3-layer scrub (L1 bucket cache, L2 `request_frame`, L3 `request_frame_hq` at 150ms idle). Passes `preview_size` from `preview_module.last_render_size` to `start_playback`/`start_blend_playback`. `poll_playback()` — PTS-gated single-slot promotion, clip-transition eviction at top. `build_blend_spec` / `build_incoming_blend_spec` — determine `PlaybackTransitionSpec` for outgoing/incoming transition halves. |
| `modules/preview_module.rs` | Renders current frame texture. `last_render_size: Option<(u32,u32)>` written each frame. `crop_uv_rect(tex_w,tex_h,target_ar)` — GPU-side center-crop for AR mismatch. Transport bar. |
| `modules/export_module.rs` | Export UI: filename, quality preset, fps, aspect. Two-stage reset (5s). States: Idle / Encoding (progress+Stop) / Done / Error. |
| `modules/library.rs` | `LibraryModule { multi_selection: HashSet<Uuid> }` — not a unit struct. Grid uses `chunks(cols)+ui.horizontal()` (not `horizontal_wrapped`). DnD write on drag start only. |
| `modules/audio_module.rs` | Sole owner of rodio audio sinks. All audio tick logic here only. |

---

## Key Pipelines

### Decode Pipeline (playback)
`video_module::tick()` → `worker::start_playback/start_blend_playback` → `PlaybackCmd::Start/StartBlend` (carries `preview_size`) → pb thread → `LiveDecoder::open(path,ts,0.0,None,forced_size,preview_size)` → `next_frame()` → `ensure_cpu_frame()` if HW → sws_scale → RGBA `Vec<u8>` → `pb_tx` → `poll_playback()` → `pending_pb_frame` → `frame_cache` → `preview_module.current_frame`

### Decode Pipeline (scrub)
**L2**: `tick()` → `worker::request_frame(aspect=ratio)` → condvar scrub slot → decode thread → `LiveDecoder::open(aspect>0 → 320px)` → RGBA → `scrub_rx`
**L3**: `tick()` after 150ms idle → `worker::request_frame_hq(id,path,ts,preview_size)` → one-shot thread → `decode_frame(aspect=0.0,preview_size)` → RGBA → `scrub_rx` → `ingest_media_results()` → `frame_cache`
**Transition scrub**: `tick()` sees `active_transition_at` → `worker::request_transition_frame(TransitionScrubRequest)` → one-shot thread → `decode_one_frame_rgba×2` → `blend_rgba_transition` → `scrub_rx`

### Encode Pipeline
`app::begin_render()` → `worker::start_encode(EncodeSpec)` → own thread → `encode_timeline()` → per-clip: open source decoder → `CropScaler` → `VideoTransition::apply()` (YUV420P blend at boundaries) → HW encoder (`h264_nvenc→amf→qsv→libx264`) → mux audio → `EncodeProgress/Done/Error` → shared `result_rx` → `ingest_media_results()`

### Transition Blend (playback)
pb thread enters blend zone (`ts >= blend_start_ts`) → lazy open `decoder_b` with `forced_size=primary_dims` → `skip_until_pts` fast burn → during burn: send `held_blend` frozen frame → post-burn: `blend_rgba_transition(data_a, data_b, w, h, alpha, kind)` → `apply_rgba()` (rayon parallel rows) → blended RGBA frame

---

## Adding a Transition
1. `transitions/myname.rs` — impl `VideoTransition` (6 methods). `apply`→YUV420P. `apply_rgba`→RGBA w/ `par_chunks_mut`.
2. One line in `declare_transitions!`: `myname::MyName => MyKind,`

## Adding a Feature
`commands.rs` variant → `modules/mymodule.rs` → `modules/mod.rs` → `VeloCutApp` field → `update()` → `process_command()` → optional `MediaResult` variants in `media_types.rs` + `ingest_media_results()`.