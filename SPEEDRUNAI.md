# VeloCut — AI Dev Reference

> Read fully before editing. Invariants = load-bearing. Check Debugging History before diagnosing frame/playback issues.

---

## Stack

Rust desktop app, Windows MINGW64, Cargo workspace.
Deps: `eframe`/`egui` 0.33, `ffmpeg-the-third` 4 (forked → `eric-lautanen/velocut-ffmpeg-the-third` branch `master` — owns this fork long-term, do not blindly rebase upstream), `crossbeam-channel` 0.5, `rodio` 0.21.1, `rfd` 0.14, `uuid` 1.10, `serde` 1.0, `png` 0.18.1.

FFmpeg: custom static build, MINGW64, linked via fork. Fork exposes flush control upstream lacks. Changes in encode/decode may also need fork changes + `Cargo.toml` version bump.

ffmpeg-the-third 4 API: no `Stream::codec()` — use `Context::from_parameters(stream.parameters())` for decoders, `Context::new_with_codec(codec)` for encoders. `set_parameters()` needs `AsPtr<AVCodecParameters>` — copy via `avcodec_parameters_from_context` FFI. `packets()` → `Result<(Stream, Packet), Error>` — always destructure with `?`. `set_frame_rate` needs explicit `Rational`.

---

## Workspace

```
velocut/
  Cargo.toml
  crates/
    velocut-core/    no UI, no FFmpeg
    velocut-media/   FFmpeg workers, no egui
    velocut-ui/      egui app + binary
```

Dep rules: ui → core + media; media → core; core/media → no egui.

---

## velocut-core/src/

**`lib.rs`** — `pub mod commands, helpers, media_types, state, transitions`. `transitions` is a folder (`transitions/mod.rs`).

**`commands.rs`** — `EditorCommand` enum. Every user action; processed by `app.rs::process_command()` post-UI pass.
Key variants: `SetTransition { after_clip_id: Uuid, kind: TransitionType }`, `RemoveTransition(Uuid)`, `SetCrossfadeDuration(f32)`, `ClearProject` (8-step teardown), `PushUndoSnapshot`, `Undo`, `Redo`, `ExtractAudioTrack(Uuid)`, `SetClipVolume { id, volume }`, `RenderMP4 { filename, width, height, fps }`, `CancelEncode(Uuid)`, `ClearEncodeStatus`, `SaveFrameToDisk`, `RequestSaveFramePicker`.

**`state.rs`** — `ProjectState` (serde). Fields: `library: Vec<LibraryClip>`, `timeline: Vec<TimelineClip>`, `transitions: Vec<TimelineTransition>`, playback fields, zoom, AR, selected IDs. `#[serde(skip)]` runtime: `pending_probes/extracts/audio_cleanup/save_pick`, `save_status`, encode fields (`encode_job`, `encode_progress`, `encode_done`, `encode_error`). Key methods: `add_to_timeline` (snap-to-zero + snap-to-end), `total_duration`, `active_video_ratio`. `TimelineClip` has `volume: f32` (default 1.0), `audio_muted: bool`, `linked_clip_id: Option<Uuid>`.

**`media_types.rs`** — `MediaResult` variants: `AudioPath, Duration, Thumbnail, Waveform, VideoSize, FrameSaved, VideoFrame, Error, EncodeProgress { job_id, frame, total_frames }, EncodeDone { job_id, path }, EncodeError { job_id, msg }`. Also `PlaybackFrame { data: Vec<u8>, timestamp: f64 }` for dedicated pb channel.

**`helpers/mod.rs`** — `pub mod geometry, time`.
**`helpers/time.rs`** — `format_time(s) -> MM:SS:FF` (ruler), `format_duration(s) -> H:MM:SS/M:SS/S.Xs` (library).
**`helpers/geometry.rs`** — `aspect_ratio_value(ar) -> f32`, `aspect_ratio_label(ar) -> &str`.

**`transitions/mod.rs`** — Three layers:
1. **Serialized types**: `TransitionKind` (`Copy` enum, registry key: `Cut`, `Crossfade`; add new here + matching `TransitionType` variant), `TransitionType` (data enum: `Cut`, `Crossfade { duration_secs: f32 }` — **never rename/remove variants, they're on disk**), `TimelineTransition { after_clip_id, kind }` (in `ProjectState.transitions`), `ClipTransition { after_clip_index, kind }` (encode-only). `TransitionType::kind() -> TransitionKind`, `TransitionType::duration_secs() -> f32`.
2. **`VideoTransition` trait**: `kind()`, `label() -> &'static str`, `icon() -> &'static str` (badge emoji — must include `U+FE0F` variation selector), `default_duration_secs() -> f32`, `build(duration_secs) -> TransitionType` (**UI always calls this, never constructs `TransitionType` directly**), `apply(frame_a: &[u8], frame_b: &[u8], width: u32, height: u32, alpha: f32) -> Vec<u8>` (packed YUV420P blend, once per frame, all inner loops inside impl).
3. **Registry**: driven by `declare_transitions!` macro — **single add-point**. Expands each `module::Struct` entry into both the `mod` declaration and the `make_entries()` vec simultaneously. `registered() -> Vec<Box<dyn VideoTransition>>` (stable-ordered, for UI). `registry() -> HashMap<TransitionKind, Box<dyn VideoTransition>>` (O(1), for encode/preview). `Cut` has no registry entry — callers short-circuit on `TransitionKind::Cut`.
`pub mod helpers` declared explicitly (not via macro — it's a utility module, not a transition).

**`transitions/helpers.rs`** — Pure f32, no FFmpeg. Easing: `ease_in_out`, `ease_in`, `ease_out`, `ease_in_out_cubic`, `linear`, `ease_out_bounce`, `ease_in_bounce`, `ease_out_elastic`. `frame_alpha(i, n) -> f32` → `(i+1)/(n+1)` exclusive. `blend_byte(a, b, alpha) -> u8`. `clamp01`, `lerp`. Plane layout: `y_len(w,h)`, `uv_len(w,h)`, `u_offset(w,h)`, `v_offset(w,h)`, `split_planes(buf,w,h) -> (&Y,&U,&V)` (debug-asserts buffer size). Spatial: `norm_x(x,w)`, `norm_y(y,h)` (normalized pixel coords), `center_dist(nx,ny)` (distance from frame center, for iris wipes), `wipe_alpha(coord, edge, feather)` (hard or soft-edge wipe alpha from a single coordinate — core primitive for directional wipes).

**`transitions/crossfade.rs`** — `Crossfade`: `label`="Dissolve", `icon`="🌫️" (`U+1F32B U+FE0F` — variation selector required), `build(dur)`→`TransitionType::Crossfade{dur}`, `apply` uses `blend_byte(a,b,ease_in_out(alpha))`. Unit tests run without FFmpeg.

---

## velocut-media/src/

**`worker.rs`** — `MediaWorker`. Owns: latest-wins condvar scrub slot, playback decode thread (32-frame bounded channel), probe semaphore (max 4), shared result channel `(tx,rx)`, **dedicated scrub channel `(scrub_tx, scrub_rx)` cap=8** (scrub VideoFrames go here, not `rx`), `encode_cancels: Arc<Mutex<HashMap<Uuid, Arc<AtomicBool>>>>`.
Methods: `probe_clip` (duration+thumbnail+VideoSize under semaphore, waveform+audio after), `request_frame` (overwrites scrub slot), `start_playback` (sends Start **before** draining `pb_rx`), `stop_playback` (sends Stop then **drains `pb_rx` to empty** — frees ~30 MB), `extract_frame_hq`, `start_encode`, `cancel_encode`, `shutdown`.

**`encode.rs`** — `ClipSpec { path, source_offset, duration, volume, skip_audio: bool }`, `EncodeSpec { job_id, clips, width, height, fps, output, transitions: Vec<ClipTransition> }`. `encode_timeline` blocking, own thread.
Setup: `Context::new_with_codec(h264)`, CRF 18 + preset fast + **`g=fps` (keyframe/sec, critical for scrub)**, **`AV_CODEC_FLAG_GLOBAL_HEADER` before `open_as_with`** (MP4 needs SPS/PPS in avcC), **fetch `ost_audio_tb` after `write_header`** (muxer normalizes tb during header write). Copy params via `avcodec_parameters_from_context` FFI.
`CropScaler`: center-crop SwsContext, no letterbox. Built with `crop_w×crop_h` as source dims. `run()` pre-advances data ptrs to `crop_y` row (`crop_y*ls[0]` Y, `(crop_y/2)*ls[1]` UV) and passes `srcSliceY=0` — **never pass `crop_y` as `srcSliceY` → EINVAL** (only manifests portrait→landscape where `crop_y>0`).
Transition dispatch: `registry()` built **once** before clip loop. Per-boundary: `TransitionType::kind()` for key, `duration_secs()` for overlap. `apply_transition(&dyn VideoTransition, ...)` calls `transition.apply()` with `frame_alpha(i,n)` — no blend math in `encode.rs`. Decoder-flush uses `VideoFrame::new(YUV420P,w,h)` — **never `VideoFrame::empty()` as sws_scale dst**.

**`decode.rs`** — `LiveDecoder`: stateful per-clip, open+seek on construct. `next_frame()` playback, `advance_to(pts)` forward scrub (decode-only pre-target, scale once on hit ~4× faster), `burn_to_pts()` sync pre-roll. Owns `frame_buf: Vec<u8>` pre-alloc `out_w*out_h*4` (no per-frame alloc). `open(path, ts, aspect, cached_scaler)` — `cached_scaler: Option<(SwsContext, Pixel, u32, u32)>` reused if format+dims match. Pub fields: `decoder_fmt, decoder_w, decoder_h` (reuse key). **`aspect` param is legacy dead parameter — ignored, do not wire up.** **Output always source native AR** (`out_h = 640*src_h/src_w`), downstream consumers crop themselves. `decode_frame()`: one-shot HQ, seeks via `seek_to_secs`.

**`probe.rs`** — `probe_duration`, `probe_video_size_and_thumbnail`. **SwsContext built lazily from first decoded frame** (not from `decoder.format()`/dims upfront — AVCC reports coded dims e.g. 1088 not 1080; Annex-B has `AV_PIX_FMT_NONE` pre-packet). Matched invariant with GLOBAL_HEADER encoder change.

**`waveform.rs`** — In-process FFmpeg decode, `WAVEFORM_COLS=4000`, sends Waveform. After semaphore release.

**`audio.rs`** — In-process FFmpeg decode, resample to 44100 Hz stereo f32le, writes `$TEMP/velocut_audio_{uuid}.wav`, sends AudioPath. `cleanup_audio_temp` deletes matching pattern.

**`helpers/seek.rs`** — `seek_to_secs(ictx, ts, label) -> bool`. Skips seek if `ts<=0.0` (Windows EPERM on fresh ctx at offset 0). Soft-fails on EPERM. **All seeks must go through here.**

**`helpers/yuv.rs`** — Packed (stride-free) YUV420P buffers. Layout: `[Y: w*h][U: uv_w*uv_h][V: uv_w*uv_h]`. `extract_yuv(frame,w,h,uv_w,uv_h) -> Vec<u8>` (strips stride), `write_yuv(packed, frame, ...)` (inverse), `blend_yuv_frame(a,b,alpha) -> Vec<u8>` (still available but no longer called from `encode.rs` directly — blending delegated to `VideoTransition::apply()`).

---

## velocut-ui/src/

**`app.rs`** — `VeloCutApp`. Concrete module fields (no dyn trait Vec): `library, preview, timeline, export, audio, video, pending_cmds`.
`process_command()`: `RenderMP4` → `begin_render()` (opens save dialog, sorts timeline, **filters `is_extracted_audio_clip`**, builds `ClipSpec`s, for V-row `audio_muted=true` calls `linked_audio_clip` for effective_volume). `CancelEncode` sets AtomicBool only (state clears when `EncodeError{msg:"cancelled"}` arrives). `ClearEncodeStatus` zeros 4 encode fields. `ClearProject` **8-step order**: (1) queue WAV paths to `pending_audio_cleanup` before wiping library, (2) `stop_playback()`+drain `pb_rx`, (3) drop `audio_sinks`, (4) `cache.clear_all()`, (5) `playback.reset()`, (6) clear `library/timeline/transitions/time/playing/selections`, (7) zero encode fields, (8) clear `undo/redo` stacks + `sync_undo_len()`.
`SetPlayhead`: updates `current_time`, clears audio_sinks + `pending_pb_frame`. If playing: `stop_playback()` + reset `playback_media_id` + `prev_playing=false` → tick sees `just_started=true` next frame → fresh `start_playback` at correct pos.
`restore_snapshot()`: after undo/redo, re-queues probes for any library clip with empty `waveform_peaks` (snapshot may predate probe return).
`poll_media()`: cleanup → probes/extracts → save dialog → `VideoModule::poll_playback` → `AppContext::ingest_media_results`.
`update()`: layout panels → `preview.current_frame` from cache → drain `pending_cmds` → tick modules → `current_time += stable_dt` → `request_repaint()` during encode.

**`context.rs`** — `AppContext`. Fields: `media_worker`, scrub tracking (`last_frame_req: Option<(Uuid,f64)>` exact ts, `scrub_coarse_req`, `scrub_last_moved`), playback tracking (`playback_media_id`, `prev_playing`, `audio_was_playing`), `thumbnail_cache`, `frame_cache`, `frame_bucket_cache` (capped `MAX_FRAME_CACHE_BYTES`≈192 MB, evicts 32 furthest from playhead via O(N) `select_nth_unstable_by_key`, values are `(TextureHandle, usize)` byte size for accurate budget), `pending_pb_frame`, `audio_stream`, `audio_sinks`.
`ingest_media_results()`: drains `scrub_rx` **first** then shared `rx` — maintain this order. Calls `ingest_video_frame()` for both (bucket insert + eviction + `frame_cache` write). `CacheContext::clear_all()` drops all 4 caches + resets byte counter. `PlaybackContext::reset()` resets 6 tracking fields.

**`theme.rs`** — Color constants + `configure_style()`.

**`helpers/clip_query.rs`** — `timeline_clip`, `selected_timeline_clip`, `clip_at_time`, `library_clip`, `library_entry_for`, `selected_clip_library_entry`, **`is_extracted_audio_clip(clip)` = `track_row%2==1 && linked_clip_id.is_some()`**, `linked_audio_clip(state, video_clip)`.

**`helpers/format.rs`** — `truncate(s, max) -> &str` (UTF-8 safe). Time/duration formatting lives in core.

**`modules/library.rs`** — `LibraryModule { multi_selection: HashSet<Uuid> }`. **Not a unit struct — init with `LibraryModule::new()` in a `let` before struct literal** (parser misreads `TypeName::method()` in struct literal position). Card grid, multi-select, drag-to-timeline, right-click menu. Grid uses manual `chunks(cols)+ui.horizontal()` per row — **do NOT use `horizontal_wrapped` or `egui::Grid` inside `ScrollArea::vertical()`** (unbounded width measurement pass, never wraps). `multi_selection` is UI-only, not serialized. `visible_ids: HashSet<Uuid>` drives progressive probe dispatch (visible clips probe first).

**`modules/preview_module.rs`** — `current_frame: Option<TextureHandle>` set by app.rs pre-`ui()`. Renders frame → thumbnail → name+spinner. Transport bar, volume slider, timecode via `format_time`.

**`modules/timeline.rs`** — `TimelineModule { transition_popup, transition_popup_just_opened, vol_popup, vol_popup_just_opened, last_scrub_emitted_time: f64 }`.
Timeline: ruler, 4 lanes (V1/A1/V2/A2), clip blocks (thumbnail strips + waveforms), playhead.
DnD: reads `DND_PAYLOAD`, clears if no pointer button down.
**Transition badges**: `has_transition = kind()!=Cut` (not Crossfade-specific). Badge icon from `registry().get(kind).icon()`. Popup is **fully registry-driven**: button row = `for entry in registered()` loop; Cut always first as hardcoded remove action. Duration slider shows for any non-Cut; calls `entry.build(dur)` — **never constructs `TransitionType` variants directly**. `_just_opened` flag suppresses same-frame close for both popups.
**Volume popup**: speaker badge → floating Area 64×150px. dB readout (`allocate_ui` 64×13, clamped min/max-width so box doesn't resize), vertical slider (−60 to +6 dB, `add_sized([22,110])`), "0 dB" label. `SetClipVolume` on change.
`draw_waveform()` scales peaks by `clip.volume`.
Clip type detection: `is_extracted_audio_clip(clip)`.
**Scrub dedup**: `last_scrub_emitted_time` skips emit if `|t-last|<1/30s`; `drag_started`/`clicked` always emit.
Clip name labels: `fit_label(text, max_px)` at bottom of file (6.5px/char heuristic) — **do NOT use `layout_no_wrap`/`glyph_width`, they need `&mut Fonts`**.

**`modules/export_module.rs`** — `{ filename, quality: QualityPreset, fps, export_aspect, clear_confirm_at }`. Quality = short-side px (480/720/1080/1440/2160), dims rounded to even. `is_encoding` computed once at top of `ui()`. Header: label left, two-stage **⊘ Reset** right (`clear_confirm_at` arms 5s countdown as amber `"⚠ Xs?"`, second click fires `ClearProject`, disabled during encode). Three states: Idle / Encoding (progress bar + Stop) / Done (green banner, 5s auto-dismiss via `ui.memory` temp key) / Error.

**`modules/audio_module.rs`** — `{ exhausted: HashSet<Uuid> }`. `tick()` only. Manages rodio Sinks. **Top of every playing tick: diff `audio_sinks.keys()` vs current timeline IDs, drop stale** (handles undo during playback). `exhausted` prevents `File::open` per-tick on short WAVs — **cleared whenever `audio_sinks` is cleared** (playhead set, stop, undo, ClearProject) to avoid blocking re-added clips.

**`modules/video_module.rs`** — Unit struct. `tick()` + `poll_playback()`. `active_media_id()` static.
`poll_playback()`: PTS-gated single-slot → `pending_pb_frame` → `frame_cache` when `current_time >= frame.pts` (±1 frame, not older than 3s). `request_repaint()` after promotion is non-redundant (background thread, not input event). **Clip-transition eviction at top before any other logic** (before UI reads `frame_cache`).
`tick()`: playback → restart decode on clip change. Scrub → L1 nearest bucket (0ms), L2 exact-ts every drag px, L2b coarse 2s prefetch, L3 precise frame 150ms idle debounce. Scrub suppressed during playback. Both paths use `clip_at_time(state,...)`.

---

## Adding a Feature

1. `EditorCommand` variant in `commands.rs`
2. `modules/mymodule.rs` impl `EditorModule`
3. `pub mod mymodule` in `modules/mod.rs`
4. Concrete field in `VeloCutApp`, init in `new()`
5. `self.mymodule.ui(...)` in `update()`
6. Match arm in `process_command()`
7. New `MediaResult` variants → `media_types.rs` + `ingest_media_results()` only

## Adding a Transition

1. Add `TransitionKind` variant in `transitions/mod.rs`
2. Add `TransitionType` variant + arms in `kind()` and `duration_secs()`
3. Create `transitions/myname.rs`, impl `VideoTransition` (all 5 required methods: `kind`, `label`, `icon`, `build`, `apply`; use `transitions/helpers.rs`)
4. Add ONE line to `declare_transitions!` in `mod.rs`: `myname::MyTransition,`

Badge, tooltip, popup button, slider, encode, preview — all auto. Zero other changes.

---

## Architecture Invariants

- Modules: `&ProjectState` read-only + emit `EditorCommand` only. Never mutate state directly.
- `ingest_media_results()` sole translation layer. Drain `scrub_rx` before `rx` always.
- `preview.current_frame` set by app.rs pre-`ui()`. `thumbnail_cache` write-only from `ingest_media_results`.
- Audio: `audio_module.rs::tick()` only. No rodio sinks elsewhere.
- Scrub/playback: `video_module.rs` only. No frame decode logic in app.rs.
- Encode state in `ProjectState` `#[serde(skip)]`. Armed in `begin_render`. Updated in `ingest_media_results`. Zeroed by `ClearEncodeStatus`. Nowhere else.
- `CancelEncode` sets AtomicBool only. State clears via `EncodeError{msg:"cancelled"}` through normal channel.
- `last_frame_req` = `Option<(Uuid, f64)>` exact ts, not bucket index. Fires on every drag px.
- Playback: PTS-gated `pending_pb_frame`. One frame promoted per tick. Never drain full `pb_rx` in one tick.
- `stable_dt` master clock. `current_time += stable_dt`. Never advance from frame timestamps.
- Probe semaphore (max 4) gates duration+thumbnail only. Waveform+audio after `drop(_guard)`.
- `frame_bucket_cache` byte-capped. **Always via `insert_bucket_frame`** — never write directly. Evicts 32 furthest O(N).
- DnD: `DND_PAYLOAD` written by LibraryModule on drag, cleared by TimelineModule on drop or when no pointer button down.
- PTS comparisons in `decode.rs` always in seconds. Raw PTS only for seek target calc.
- All seeks via `helpers::seek::seek_to_secs`. Never `ictx.seek()` directly.
- **Never use project AR to size decode output.** Source native AR always. Both downstream consumers (preview UV crop, `CropScaler`) need undistorted source and crop themselves.
- **Never pass `crop_y` as `srcSliceY` to sws_scale.** Pre-advance data ptrs, pass 0.
- **`AV_CODEC_FLAG_GLOBAL_HEADER` before `open_as_with` for MP4.** Both video and audio encoders.
- **Fetch `ost_audio_tb` after `write_header`.**
- **`VideoFrame::new(...)` not `VideoFrame::empty()` as sws_scale dst.**
- **`opts.set("g", fps)` for NLE-friendly GOP.**
- **SwsContext in `probe.rs` built lazily from first decoded frame.** Matched invariant with GLOBAL_HEADER.
- **Registry is UI source of truth for transitions.** `timeline.rs` never matches specific `TransitionType` variants for rendering. UI always calls `entry.build(dur)`, never constructs variants.
- **`TransitionType` variants are serialized — never rename/remove without migration.**
- **`LibraryModule` not a unit struct.** `let library = LibraryModule::new();` before struct literal.
- `multi_selection` on `LibraryModule`, not `ProjectState`. Not serialized.
- New temp file → `state.pending_audio_cleanup`. New thread → `MediaWorker::shutdown()`.
- `ClearProject` 8-step order is load-bearing. See app.rs entry.
- Stale sinks evicted at top of every playing tick in `audio_module.rs`.

---

## Frame Lifetime

Exec order per tick:
```
poll_media() → poll_playback() [frame_cache WRITES]
update() → preview.current_frame = frame_cache.get() [READ]
         → panels render
         → tick() [more evictions, one frame late]
         → current_time += stable_dt
```

Evictions that must fire before render → `poll_playback()`. Clip-transition eviction is in `poll_playback()` top for this reason.

Cache roles:
- `frame_cache`: keyed by `media_id`, one entry, currently shown texture.
- `frame_bucket_cache`: `(media_id, bucket_index)`, rolling, byte-capped 192 MB.
- `pending_pb_frame`: single-slot between pb channel and `frame_cache`.

---

## Debugging History

**Ghost frame at clip boundary during playback** — stale scrub frame in `frame_cache[new_clip.media_id]` shown for one tick because eviction was in `tick()` (one frame late). Fix: clip-transition eviction at top of `poll_playback()` before all other logic. If regresses: check eviction fires before `preview.current_frame` is set.

**3× speed playback** — draining full `pb_rx` in one tick. Fix: `pending_pb_frame` single-slot, promote one frame/tick when PTS due.

**`horizontal_wrapped`/`Grid` refuses to wrap in `ScrollArea`** — `ScrollArea::vertical()` gives inner Ui unbounded horizontal space during measurement; both widgets conclude no wrap needed. Fix: manual `chunks(cols)+ui.horizontal()`. Never retry wrapped/Grid approaches.

**Export MP4 tail freeze (video shorter than audio)** — three causes: (1) `out_frame_idx` read from post-`rescale_ts` packet PTS (wrong units), (2) `out_frame_idx += 1` gated on `frame_written` (under-counted due to B-frame lookahead), (3) no audio ceiling in `encode_clip` so drain_fifo eagerly wrote 2s+ overrun before post-encode trim could catch it. Fixes: capture `frame_pts` before `rescale_ts`, unconditional `out_frame_idx += 1` after `send_frame` in decoder-flush, `pts_secs >= clip_end` guards in demuxer audio branch and audio-decoder flush. If regresses: check `out_frame_idx` units (must be frame-count/`1/fps`), check audio ceiling guards in both locations.

**Video freezes after seek then jumps** — `burn_to_pts` takes 600ms+ on long GOPs; `current_time` advances via `stable_dt`; first frame PTS T arrives but `local_t` is `T+burn_time`; old 3.0s lower-bound too tight. Fix: 3.0s lower bound in `poll_playback()` Step 3. If regresses: check `too_old` guard threshold.

**Waveform blank after undo of Extract Audio** — snapshot taken before probe returned → `waveform_peaks=[]`. Fix: `restore_snapshot()` re-queues probes for clips with empty peaks. If regresses: check re-queue happens after `pending_probes` is moved from live state.

**Phantom audio after undo during playback** — rodio Sink keyed by clip UUID not dropped when clip removed via undo. Fix: diff `audio_sinks.keys()` vs timeline IDs at top of every playing tick. If regresses: confirm eviction runs unconditionally before `active_clip` search.

**Extracted audio duplicating in export** — `begin_render()` built `ClipSpec` for every clip including A-row extracted clips (same source/offset/duration as V-row → double video+audio). Fix: filter `is_extracted_audio_clip`. For `audio_muted` V-row, use `linked_audio_clip` volume. If regresses: check A-row filter and effective_volume link traversal.

**Exported clip fails re-import (`AVERROR_INVALIDDATA`)** — missing `AV_CODEC_FLAG_GLOBAL_HEADER` → empty `avcC` box. Fix: set flag before `open_as_with` on both encoders. Matched pair with lazy `SwsContext` in probe.rs.

**Exported clip thumbnail/duration not loading on re-import** — `SwsContext` built upfront from `decoder.format()`/dims: AVCC reports coded dims (1088 not 1080), Annex-B has `AV_PIX_FMT_NONE`. Fix: lazy SwsContext from first decoded frame in `probe.rs`. Matched pair with GLOBAL_HEADER encoder fix.

**Choppy scrub on re-imported exports** — libx264 default `keyint=250` (~8s GOP), scrub burns 8× more frames per seek. Fix: `opts.set("g", fps)` → 1 keyframe/sec. If regresses: check `g` option and `spec.fps` is integer fps.

**Windows EPERM on seek** — `avformat_seek_file` returns EPERM on Windows with `max_ts=0` on fresh ctx. Fix: `seek_to_secs` skips seek if `ts<=0.0`, soft-fails on EPERM. Never call `ictx.seek()` directly.

**`CropScaler::run` EINVAL (-22) on portrait→landscape** — passing `srcSliceY=crop_y` makes `crop_y+crop_h>crop_h`. Fix: pre-advance data ptrs to `crop_y` row, pass `srcSliceY=0`. Only manifests when `crop_y>0` (portrait source, landscape output).

**Mixed-AR clips stretched** — decode used project AR (`out_h = 640/aspect`), pre-distorting source. Both `crop_uv_rect` and `CropScaler` received already-distorted input. Fix: `out_h = 640*src_h/src_w`, source native AR always. If regresses: check `decode.rs` `open()` and `decode_frame()` use `src_h/src_w`, not project aspect.

---

## Resource Lifecycle

- **TextureHandle** (all caches): dropping = GPU free. `frame_bucket_cache` write only via `insert_bucket_frame` to keep byte budget accurate.
- **Audio temp WAVs** (`$TEMP/velocut_audio_{uuid}.wav`): created by `audio.rs::extract_audio`, queued in `state.pending_audio_cleanup`, deleted by `poll_media()`. Must be queued before library wipe in `ClearProject`.
- **Rodio sinks**: `audio_module.rs::tick()` only.
- **`LiveDecoder` (FFmpeg ictx + decoder)**: dropped when `start_playback` replaces it on pb thread.
- **Encode cancel flags**: inserted on `start_encode`, removed by encode thread on exit (finish/error/cancel). `shutdown()` sets all + clears map.
- **Probe semaphore**: RAII guard. No early return inside semaphore scope without explicit `drop(_guard)`.

---

## Known Future Work

- **Lower-res bucket frames** (high impact): store at ≤640px wide (~1.2 MB vs ~8 MB/frame), fit ~160 frames in 192 MB budget. Full-res only on L3 precise frame. Needs downscale pass in scrub decode + resolution flag on bucket entry.
- **Velocity-scaled L2b prefetch**: currently fixed 2s window. Scale to 8–10s on fast fling, 1s backward. Track scrub velocity in `timeline.rs` (delta time/wall-clock, smoothed 3–5 frames).
- **Hover prefetch**: emit `RequestScrubPrefetch(hover_time)` at L2b priority on hover before drag starts. Same code path, different trigger.
- **Hover cursor frame preview**: small thumbnail above cursor on hover (not dragging, not playing). Read-only lookup in `frame_bucket_cache` by `(media_id, nearest_bucket_index)`. Pass `&ctx.frame_bucket_cache` read-only into `timeline.rs::ui()`.
- **`fit_label` to `helpers/format.rs`**: currently private in `timeline.rs`. Move when second callsite appears.
- **`thumbnail_cache` eviction**: currently never evicted. Fine for typical projects but leaks for large clip counts.

---

## Helpers Quick Reference

Use these, never inline:
- Timeline/library lookups: `crate::helpers::clip_query::{timeline_clip, library_entry_for, clip_at_time, selected_timeline_clip, selected_clip_library_entry, is_extracted_audio_clip, linked_audio_clip}` (`library_clip` is an alias for `library_entry_for` — use `library_entry_for`)
- Time display: `velocut_core::helpers::time::{format_time, format_duration}`
- AR: `velocut_core::helpers::geometry::{aspect_ratio_value, aspect_ratio_label}`
- String truncation: `crate::helpers::format::truncate`
- Seeks: `crate::helpers::seek::seek_to_secs`
- YUV pack/unpack: `crate::helpers::yuv::{extract_yuv, write_yuv}` (`blend_yuv_frame` available but delegated to `VideoTransition::apply()`)
- Transition UI: `velocut_core::transitions::{registered, registry}` (`registered()` = Vec for UI iteration, `registry()` = HashMap for O(1) encode lookup)
- Transition math: `transitions::helpers::{frame_alpha, blend_byte, clamp01, lerp}` — easing: `ease_in_out`, `ease_in`, `ease_out`, `ease_in_out_cubic`, `linear`, `ease_out_bounce`, `ease_out_elastic` — plane layout: `split_planes`, `y_len`, `uv_len`, `u_offset`, `v_offset` — spatial: `norm_x`, `norm_y`, `center_dist`, `wipe_alpha`