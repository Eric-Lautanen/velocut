# VeloCut — Architecture & Developer Reference

> **For AI assistants:** This document is the authoritative reference for VeloCut. Read it fully before suggesting any edits. Invariants sections are load-bearing — violating them causes bugs that are hard to trace. Check "Debugging History" before diagnosing any frame/playback issue.

---

## What VeloCut Is

A desktop video editor written in Rust. Compiles and runs on Windows (MINGW64). Binary is a Cargo workspace.

**Key deps:** `eframe`/`egui` 0.33 (UI), `ffmpeg-the-third` 4 (decode + encode — **forked** to `eric-lautanen/velocut-ffmpeg-the-third`, branch `master`), `crossbeam-channel` 0.5, `rodio` 0.21.1 (audio), `rfd` 0.14 (dialogs), `uuid` 1.10, `serde` 1.0, `png` 0.18.1.

**FFmpeg build:** Custom static FFmpeg compiled from source (MINGW64) and linked via the forked `ffmpeg-the-third`. VeloCut owns this fork (`eric-lautanen/velocut-ffmpeg-the-third`, branch `master`) — it is not a temporary patch but a long-term owned dependency. The fork exists because upstream `ffmpeg-the-third` does not expose the low-level encoder/decoder flush control VeloCut requires, and waiting on upstream PRs is not viable for active development. When diagnosing encode or decode issues, changes may be needed in the fork **as well as** in `encode.rs` or `decode.rs`. The fork should be kept selectively in sync with upstream — pull upstream fixes that don't conflict with VeloCut's patches, but do not blindly rebase. Any change to the fork requires a matching version bump in the workspace `Cargo.toml` to ensure the correct fork commit is used.

---

## Workspace Structure

```
velocut/
  Cargo.toml          ← workspace root; shared [workspace.dependencies]
  crates/
    velocut-core/     ← pure data/contracts (no UI, no FFmpeg)
    velocut-media/    ← FFmpeg worker threads (no egui)
    velocut-ui/       ← egui app + binary entry point
```

**Dependency rules:** velocut-ui → core + media; velocut-media → core; core and media → no egui.

---

## velocut-core — `crates/velocut-core/src/`

| File | Purpose |
|------|---------|
| `lib.rs` | Declares and re-exports `pub mod commands`, `helpers`, `media_types`, `state`, `transitions`. |
| `commands.rs` | `EditorCommand` enum — every user action emitted by modules, processed by `app.rs::process_command()` after the UI pass. Variants: **Playback** — Play, Pause, Stop, SetPlayhead(f64), SetVolume(f32), ToggleMute. **Library** — ImportFile, DeleteLibraryClip, SelectLibraryClip. **Timeline** — AddToTimeline, DeleteTimelineClip, SelectTimelineClip, MoveTimelineClip, TrimClipStart, TrimClipEnd, SplitClipAt, SetClipVolume { id: Uuid, volume: f32 }, SetTransition { after_clip_id: Uuid, kind: TransitionType }. **Export** — RenderMP4 { filename, width, height, fps }, CancelEncode(Uuid), ClearEncodeStatus. **View/UI** — SetAspectRatio, SetTimelineZoom, ClearSaveStatus, RequestSaveFramePicker, SaveFrameToDisk. **Project** — `ClearProject` — full app reset; see app.rs entry for the 8-step ordered teardown sequence. |
| `state.rs` | Serializable `ProjectState` (serde). Owns: `library: Vec<LibraryClip>`, `timeline: Vec<TimelineClip>`, playback fields (current_time, is_playing, volume, muted, timeline_zoom, aspect_ratio), selected clip ids. Runtime-only `#[serde(skip)]` queues: `pending_probes`, `pending_extracts`, `pending_audio_cleanup`, `pending_save_pick`, `save_status`. **Encode status** (all `#[serde(skip)]`): `encode_job: Option<Uuid>`, `encode_progress: Option<(u64, u64)>`, `encode_done: Option<PathBuf>`, `encode_error: Option<String>`. Methods: `add_to_library`, `update_clip_duration`, `update_waveform`, `add_to_timeline` (with snap-to-zero and snap-to-track-end logic), `total_duration`, `active_video_ratio`. Also defines `LibraryClip`, `TimelineClip` (has a `volume: f32` field, default 1.0, used by `draw_waveform` and emitted via `SetClipVolume`), `AspectRatio`, `ClipType`. |
| `media_types.rs` | `MediaResult` enum sent from worker threads to UI over the result channel. Variants: AudioPath, Duration, Thumbnail, Waveform, VideoSize, FrameSaved, VideoFrame, Error, **EncodeProgress { job_id, frame, total_frames }**, **EncodeDone { job_id, path }**, **EncodeError { job_id, msg }**. Also `PlaybackFrame` struct (RGBA data + PTS timestamp) for the dedicated playback channel. |
| `transitions.rs` | `TransitionType` enum (`Cut`, `Crossfade { duration_secs: f32 }`) and `TimelineTransition` struct (`after_clip_id: Uuid`, `kind: TransitionType`). `ProjectState.transitions: Vec<TimelineTransition>` stores all active transitions. |
| `helpers/mod.rs` | Declares `pub mod geometry` and `pub mod time`. |
| `helpers/time.rs` | **`format_time(s: f64) -> String`** — formats seconds as `MM:SS:FF` (frame-level precision at 30 fps). Used on the timeline ruler and the preview transport bar. **`format_duration(secs: f64) -> String`** — formats seconds as `H:MM:SS` / `M:SS` / `S.Xs` depending on magnitude. Used in the library card grid and context menu. Centralised here so `timeline.rs` and `library.rs` share one implementation instead of carrying diverged private copies. |
| `helpers/geometry.rs` | **`aspect_ratio_value(ar: AspectRatio) -> f32`** — returns the numeric width-to-height ratio for a given `AspectRatio` variant. **`aspect_ratio_label(ar: AspectRatio) -> &'static str`** — returns the human-readable label shown in ComboBoxes and hints. Both were previously private free functions in `export_module.rs`; moved here so `video_module.rs` and any future headless renderer can use them without depending on UI internals. |

---

## velocut-media — `crates/velocut-media/src/`

| File | Purpose |
|------|---------|
| `lib.rs` | Declares all modules. Re-exports `MediaWorker`, `MediaResult`, `PlaybackFrame`, `ClipSpec`, `EncodeSpec` for clean imports in velocut-ui. |
| `worker.rs` | `MediaWorker` — all public API that velocut-ui calls. Owns: latest-wins condvar slot for scrub frames, dedicated playback decode thread (32-frame bounded channel), probe semaphore (max 4 concurrent), shared result channel `(tx, rx)`, **dedicated scrub result channel `(scrub_tx, scrub_rx)` (capacity 8)** — scrub `VideoFrame` results travel here instead of `rx` so scrub responsiveness is decoupled from probe/encode load, `encode_cancels: Arc<Mutex<HashMap<Uuid, Arc<AtomicBool>>>>` for per-job cancellation. **Methods:** `probe_clip` (gatekeeper thread → duration + thumbnail + video size under semaphore, then waveform + audio extraction after semaphore release — both in-process), `request_frame` (overwrites scrub slot, wakes decode thread), `start_playback` (sends `Start` command **before** draining `pb_rx` — this order guarantees the pb thread resets its decoder before any new frames are pushed, so the drain sees only stale frames from the previous session), `stop_playback` (sends Stop command then **drains `pb_rx` to empty** — frees ~30 MB of RGBA allocations immediately; doubly important because `stop_playback` is called on every intra-clip seek during playback, not only on explicit stop), `extract_frame_hq` (one-shot PNG thread — sends on shared `tx`), `start_encode` (registers cancel flag, spawns encode thread calling `encode_timeline`), `cancel_encode` (sets AtomicBool for target job), `shutdown` (cancels all active encodes, sends poison-pill to scrub thread). |
| `encode.rs` | Multi-clip H.264/MP4 encode pipeline. **`ClipSpec`** { path, source_offset, duration, volume, **skip_audio: bool** } — one timeline clip's contribution. `skip_audio = true` suppresses audio decoder construction for that clip entirely; used by `begin_render()` when needed but currently always `false` — the A-row exclusion in `begin_render()` makes this the correct default (see app.rs entry). **`EncodeSpec`** { job_id, clips, width, height, fps, output } — full job description. **`encode_timeline(spec, cancel, tx)`** — blocking, runs on its own thread. Creates output context via `ffmpeg::format::output`, builds encoder via `codec::context::Context::new_with_codec(h264)` (not via Stream — `Stream::codec()` does not exist in ffmpeg-the-third 4). Sets CRF 18 + preset fast + **`g = fps` (keyframe every second)** via `ffmpeg::Dictionary` — the keyframe interval must be `fps` so scrubbing an exported clip stays responsive; libx264 default of 250 would cause ~8 s GOP burn-through on every seek. **Sets `AV_CODEC_FLAG_GLOBAL_HEADER`** on both video and audio encoders (checked against `octx.format().flags()`) before calling `open_as_with` — required for MP4/AVCC so SPS/PPS land in the `avcC` box; without this the exported file cannot be re-opened by FFmpeg's demuxer (`AVERROR_INVALIDDATA`). **Fetches `ost_audio_tb` after `write_header`** — the MP4 muxer may normalize stream timebases during `avformat_write_header`, so reading it before that call produces a stale value that causes audio drift. Copies encoder params to stream via `avcodec_parameters_from_context` FFI. Per-clip: opens input, builds decoder via `Context::from_parameters`, seeks via `helpers::seek::seek_to_secs`, decodes, center-crops and scales to output resolution via `CropScaler`, packs YUV via `helpers::yuv::extract_yuv`, re-assigns monotonic PTS (eliminates discontinuities between trimmed clips), encodes. **`CropScaler`**: SwsContext wrapper that center-crops source to output AR — no letterboxing, no stretching, mismatched-AR clips lose edge content but are never distorted. Built with `(crop_w × crop_h)` as declared source dims; `run()` pre-advances data pointers by `crop_y * linesize[0]` (and `(crop_y/2) * linesize[1]` for UV half-height planes), then passes `srcSliceY=0` — **never pass `crop_y` as `srcSliceY`**: that makes `srcSliceY + srcSliceH > crop_h` → libswscale EINVAL (-22), which only manifests on portrait-into-landscape encodes where `crop_y > 0`. Horizontal crop uses the same pointer-advance pattern. Audio decoder construction is gated on `!clip.skip_audio` — if false, `audio_decoder` stays `None` and all audio packet/flush branches are no-ops. Decoder-flush path uses `VideoFrame::new(Pixel::YUV420P, w, h)` — **never `VideoFrame::empty()`** as sws_scale destination (null data pointers → UB/segfault). Crossfade transitions blend outgoing and incoming frames using `helpers::yuv::blend_yuv_frame` and write back with `helpers::yuv::write_yuv`. `packets()` yields `Result<(Stream, Packet), Error>` — always destructure with `?`. Sends `EncodeProgress` every 15 frames, `EncodeDone` or `EncodeError { msg: "cancelled" }` on exit. Checks `cancel: Arc<AtomicBool>` after each frame. |
| `decode.rs` | Two decode paths. **`LiveDecoder`**: stateful per-clip decoder — open+seek on construction, `next_frame()` for playback, `advance_to(pts)` for forward scrub, `burn_to_pts()` for synchronous pre-roll (decode-only, no scale). `LiveDecoder` owns a `frame_buf: Vec<u8>` pooled output buffer (pre-allocated to `out_w * out_h * 4` at construction); `next_frame()` and `advance_to()` fill it via `extend_from_slice` per row and `clone()` once for the return value — no per-frame heap allocation in steady state. **`advance_to()`** is decode-only for all pre-target frames (same as `burn_to_pts`), scaling exactly once on the frame that meets `target_pts` — ~4× faster than the old decode+scale+alloc-per-frame approach for large forward scrubs. **`open(path, timestamp, aspect, cached_scaler)`** accepts `cached_scaler: Option<(SwsContext, Pixel, u32, u32)>`; if the source format/dimensions match the new clip's codec parameters the context is reused instead of calling `SwsContext::get`. Three `pub` fields carry the reuse key: `decoder_fmt: Pixel`, `decoder_w: u32`, `decoder_h: u32`. Both `open()` and `decode_frame()` build the codec context **inside the existing stream-borrow block**, eliminating the second `input()` open entirely — `raw_w`/`raw_h` now come from `decoder.width()`/`decoder.height()` after the decoder is built (no unsafe raw pointer read). **Output dimensions always use source AR** (`out_h = 640 * src_h / src_w`) — the `aspect` parameter is accepted for API compatibility but ignored for sizing. **Never use project AR to size decode output**: doing so pre-stretches the RGBA frame, making `crop_uv_rect` in the preview see matching ARs and pass through distorted content, while also feeding CropScaler corrupted input. Both downstream consumers (preview UV crop, encode CropScaler) must receive an undistorted source frame and perform the project-AR crop themselves. **`decode_frame()`**: one-shot HQ extraction; seeks via `helpers::seek::seek_to_secs`, skips pre-target frames using seconds-based PTS, emits VideoFrame result or writes PNG to disk. |
| `probe.rs` | **`probe_duration`**: reads container/stream duration, sends Duration result. **`probe_video_size_and_thumbnail`**: reads raw dimensions (VideoSize), seeks to 10% of duration, decodes one frame, scales to 320px wide, sends Thumbnail as RGBA. Both run under the probe semaphore. **SwsContext is built lazily on the first decoded frame** (not upfront from `decoder.format()`/`decoder.width()`/`decoder.height()`) — for Annex-B H.264 (no extradata) the format is `AV_PIX_FMT_NONE` before any packets are fed so upfront construction fails silently; for AVCC H.264 (GLOBAL_HEADER exports) the decoder reports coded dimensions (e.g. 1088) rather than display dimensions (e.g. 1080), producing a corrupt scaler. Using the live decoded frame sidesteps both issues. |
| `waveform.rs` | Decodes audio in-process via `ffmpeg-the-third` (`ffmpeg::format::input` + `ffmpeg::codec::context::Context`). Handles all common sample formats (f32, i16, i32, f64, u8 — packed and planar). Downsamples to `WAVEFORM_COLS` (4000) columns (max absolute value per block), sends Waveform result. Runs after semaphore release. No CLI subprocess or PATH dependency. |
| `audio.rs` | **`extract_audio`**: decodes audio in-process via `ffmpeg-the-third`, resamples to 44100 Hz stereo f32le using `ffmpeg::software::resampling::Context` (built lazily on the first decoded frame), writes a WAV temp file to `$TEMP/velocut_audio_{uuid}.wav`, sends AudioPath. No CLI subprocess or PATH dependency. **`cleanup_audio_temp`**: deletes a path if it matches the `velocut_audio_*.wav` pattern in the OS temp dir. |
| `helpers/mod.rs` | Declares `pub mod seek` and `pub mod yuv`. |
| `helpers/seek.rs` | **`seek_to_secs(ictx, target_secs, label) -> bool`** — wraps `ictx.seek(seek_ts, seek_ts..)` with Windows EPERM soft-fail behaviour. Returns `true` on success or when `target_secs <= 0.0` (demuxer already at start; skipping avoids the EPERM that Windows returns on a freshly-opened context with `max_ts=0`). Returns `false` and logs a warning on failure, allowing the caller's PTS-based frame filtering to skip pre-roll. The `label` parameter is a caller description (e.g. `"encode_clip"`) used in log messages. Centralised here to avoid duplicating the guard + `eprintln!` pattern across `encode.rs` and `decode.rs`; the caller decides whether failure is a hard error or soft-fail — that policy stays at the call site. |
| `helpers/yuv.rs` | Three functions for working with packed (stride-free) YUV420P byte buffers. Previously inlined as nested functions inside `decode_clip_frames` in `encode.rs`; extracted so they can be unit tested independently, reused by future effects helpers, and used by the concurrent crossfade decode path. **Packed layout:** `[0 .. w*h]` Y plane, `[w*h .. w*h + uv_w*uv_h]` U plane, `[w*h + uv_w*uv_h .. end]` V plane — strides stripped, each row exactly `w` (or `uv_w`) bytes. **`extract_yuv(frame, w, h, uv_w, uv_h) -> Vec<u8>`** — strips ffmpeg stride padding from a scaled `VideoFrame`, producing a flat packed buffer. Frame must already be in `YUV420P` format (call swscale first). **`blend_yuv_frame(frame_a, frame_b, alpha) -> Vec<u8>`** — linear byte-space blend; `alpha=0.0` is 100% `frame_a` (outgoing clip), `alpha=1.0` is 100% `frame_b` (incoming clip). Blend is in gamma-encoded byte space — correct for SDR dissolves; HDR/wide-gamut content would require float intermediate storage. Panics in debug if slice lengths differ (both clips must be scaled to the same output dimensions). **`write_yuv(packed, frame, w, h, uv_w, uv_h)`** — inverse of `extract_yuv`; writes a packed buffer back into a strided `VideoFrame` so the encoder receives correctly padded planes. |

---

## velocut-ui — `crates/velocut-ui/src/`

| File | Purpose |
|------|---------|
| `main.rs` | Entry point: initializes FFmpeg, sets native options (title, icon, vsync), runs eframe. Declares `mod helpers` alongside `mod app`, `mod context`, `mod modules`, `mod theme`. |
| `app.rs` | `VeloCutApp` + `eframe::App`. Concrete typed module fields (no `dyn EditorModule` Vec): `library`, `preview`, `timeline`, `export`, `audio`, `video`, `pending_cmds`. **`process_command()`**: single dispatch for all EditorCommand variants. `RenderMP4` → `begin_render()` which opens rfd save dialog, sorts timeline by start_time, **filters out A-row extracted clips** using `is_extracted_audio_clip` (canonical predicate from `clip_query`), joins remaining clips against library to build `Vec<ClipSpec>`. For V-row clips with `audio_muted = true`, calls `linked_audio_clip(state, clip)` to look up the A-row partner and uses its `volume` as `effective_volume` — the two previously diverged inline copies of this logic are gone. All ClipSpecs are built with `skip_audio: false`. Arms encode state in ProjectState, calls `media_worker.start_encode()`. `CancelEncode` → calls worker (does NOT clear state — waits for `EncodeError { msg: "cancelled" }` through the channel). `ClearEncodeStatus` → zeros all four encode fields. `ClearProject` → full app reset in this exact 8-step order: (1) queue every library clip's audio temp path into `state.pending_audio_cleanup` before wiping the library — once the library is cleared those paths are unrecoverable; (2) call `stop_playback()` and drain `pb_rx` — must happen before touching `ProjectState`, the decode thread holds clip references; (3) drop all `ctx.audio_sinks` — rodio decode threads reference WAV paths, drop before paths become invalid; (4) call `ctx.cache.clear_all()` — drops all GPU textures and resets the byte budget; (5) call `ctx.playback.reset()` — resets all scrub/playback tracking; (6) clear `state.library`, `state.timeline`, `state.transitions`, reset `state.current_time`, `state.is_playing`, and selection fields; (7) zero all four encode state fields; (8) clear `self.undo_stack` and `self.redo_stack` and call `sync_undo_len()` — stale snapshots waste memory and have nothing meaningful to restore after a full wipe. **`SetPlayhead`**: updates `current_time`, clears audio sinks and `pending_pb_frame`. If `is_playing`, also calls `stop_playback()` and resets `playback_media_id` + `prev_playing` to `false` — this makes `tick()` see `just_started = true` on the next frame and issue a fresh `start_playback` at the correct position. Without this, an intra-clip seek while playing would leave the pb thread decoding from the old position (tick() only restarts on `just_started || clip_changed`, not on position jumps within the same clip). **`restore_snapshot()`**: after applying an undo/redo snapshot, re-queues probes (`pending_probes`) for any library clip whose `waveform_peaks` is empty — handles the timing window where a snapshot was taken before the initial probe returned, which would otherwise leave the waveform blank after undo even though `audio_muted` is correctly restored. **`poll_media()`**: housekeeping (audio cleanup, pending probes/extracts, save-frame dialog) → `VideoModule::poll_playback` → `AppContext::ingest_media_results`. **`update()`**: lays out panels, sets `preview.current_frame` from frame_cache, drains pending_cmds, ticks VideoModule + AudioModule, advances current_time by stable_dt, calls `ctx.request_repaint()` while encode is active. |
| `context.rs` | `AppContext` — runtime-only handles. **Fields:** `media_worker`, scrub tracking (`last_frame_req: Option<(Uuid, f64)>` exact ts, `scrub_coarse_req`, `scrub_last_moved`), playback tracking (`playback_media_id`, `prev_playing`, `audio_was_playing`), `thumbnail_cache`, `frame_cache`, `frame_bucket_cache` (capped at `MAX_FRAME_CACHE_BYTES` ≈ 192 MB, evicts 32 furthest from playhead using O(N) `select_nth_unstable_by_key`; values are `(TextureHandle, usize)` tuples where the `usize` is that entry's exact RGBA byte size — eviction subtracts each entry's own count so the budget stays accurate on mixed-resolution projects), `pending_pb_frame`, `audio_stream`, `audio_sinks`. **`ingest_media_results()`**: sole translation layer between MediaWorker output and UI state. Drains `media_worker.scrub_rx` first (high-priority path) before the shared `rx` so scrub frames are never delayed behind probe or encode traffic. Both paths call the private `ingest_video_frame()` helper, which handles bucket-cache insert, eviction, and `frame_cache` write. Handles all MediaResult variants including EncodeProgress, EncodeDone, EncodeError (all guard on `state.encode_job == Some(job_id)`). **`CacheContext::clear_all()`** — drops all four cache fields (`thumbnail_cache`, `frame_cache`, `frame_bucket_cache`, `pending_pb_frame`) and resets `frame_cache_bytes` to zero; called by the `ClearProject` handler. **`PlaybackContext::reset()`** — sets all six tracking fields back to their initial values; called by the `ClearProject` handler so the scrub/playback pipeline starts clean after a wipe without requiring struct reconstruction. |
| `theme.rs` | Color constants: ACCENT, DARK_BG_0/2/3/4, DARK_BORDER, DARK_TEXT_DIM, CLIP_VIDEO, CLIP_AUDIO, CLIP_SELECTED. `configure_style()` sets egui visuals, spacing, rounding. |
| `helpers/mod.rs` | Declares `pub mod clip_query` and `pub mod format`. |
| `helpers/clip_query.rs` | Lookup helpers that replace repeated `state.timeline.iter().find(...)` / `state.library.iter().find(...)` chains (previously in at least 6 places across `app.rs`, `timeline.rs`, `library.rs`). **`timeline_clip(state, id)`** — returns `Option<&TimelineClip>` by id. **`selected_timeline_clip(state)`** — resolves `state.selected_timeline_clip` to a clip reference. **`clip_at_time(state, time)`** — returns the clip whose time range contains `time` (used for "what is under the playhead" queries). **`library_clip(state, id)`** — returns `Option<&LibraryClip>` by id. **`library_entry_for(state, clip)`** — resolves a `TimelineClip` to its `LibraryClip` via `media_id`. **`selected_clip_library_entry(state)`** — combines `selected_timeline_clip` + `library_entry_for`; used for the toolbar extract-frame enabled-check. **`is_extracted_audio_clip(clip)`** — canonical single-source predicate: `clip.track_row % 2 == 1 && clip.linked_clip_id.is_some()`; used by `app.rs::begin_render()` and `timeline.rs` render-type detection. **`linked_audio_clip(state, video_clip)`** — traverses the V↔A link: resolves `video_clip.linked_clip_id` to its `TimelineClip`; used by `app.rs::begin_render()` for effective-volume lookup on muted V-row clips. |
| `helpers/format.rs` | UI-layer string utilities that don't belong in `velocut-core`. **`truncate(s, max) -> &str`** — clips a string to at most `max` bytes on a valid UTF-8 boundary. Used by the library card grid and drag ghost label to prevent overflow in fixed-width tiles. Time and duration formatting lives in `velocut_core::helpers::time` — this module holds utilities that only make sense in a display context. |
| `modules/mod.rs` | Declares all submodules (`pub mod export_module` etc). `EditorModule` trait: `name() -> &str`, `ui(&mut self, ui, &ProjectState, &mut ThumbnailCache, &mut Vec<EditorCommand>)`, `tick()` (default no-op). `ThumbnailCache = HashMap<Uuid, TextureHandle>`. Trait must be in scope at call sites. |
| `modules/library.rs` | `LibraryModule { multi_selection: HashSet<Uuid> }`. **No longer a unit struct** — initialize with `LibraryModule::new()`, not a struct literal (see Pitfalls). Card thumbnail grid with multi-select, drag-to-timeline, right-click menu, and multi-file import. **Selection model:** plain click → single select; Ctrl+click → toggle; Shift+click → range from anchor; Ctrl+A → select all; Escape → clear; Delete/Backspace → delete all selected. `multi_selection` is the UI-only set; `state.selected_library_clip` is kept as the range-select anchor and single-clip reference for other modules. Drag always uses the card under pointer regardless of multi-select. Import uses `rfd::FileDialog::pick_files()` (multi-file). **Grid layout** uses manual `chunks(cols)` + `ui.horizontal()` per row — do NOT switch to `horizontal_wrapped` or `egui::Grid`; both fail to wrap inside `ScrollArea::vertical()` (see Debugging History). `✓` badge painted on multi-selected cards. Context menu offers "Remove N clips" when multi-select is active. Duration display uses `velocut_core::helpers::time::format_duration`; clip name truncation uses `crate::helpers::format::truncate`. |
| `modules/preview_module.rs` | `PreviewModule { current_frame: Option<TextureHandle> }`. Set by app.rs before each `ui()` call. Renders live frame → thumbnail fallback → name + spinner. Transport bar via raw coordinate math. Volume slider via `ui.put()`. Timecode display uses `velocut_core::helpers::time::format_time`. |
| `modules/timeline.rs` | `TimelineModule { transition_popup: Option<(Uuid, Pos2)>, transition_popup_just_opened: bool, vol_popup: Option<(Uuid, Pos2)>, vol_popup_just_opened: bool, last_scrub_emitted_time: f64 }`. Scrollable timeline: ruler (adaptive tick density), 4 track lanes (V1/A1/V2/A2), clip blocks with thumbnail strips + waveform overlays, playhead + draggable handle. DnD: reads `DND_PAYLOAD`, clears immediately if no pointer button down (prevents ghost indicators), snap indicator, emits `AddToTimeline` on drop. Clip drag with edge snapping. **Transition badges (✂ / ⇌)** between touching clips; clicking opens a floating `egui::Area` popup (`transition_popup`) anchored at the badge to set `TransitionType`. The `_just_opened` bool suppresses click-outside-to-close on the same frame the badge was clicked, for both popups. **Speaker badge / volume popup:** clips render a speaker icon badge; clicking opens a floating `egui::Area` (`vol_popup`) anchored below the badge. Popup is 64×150 px, nearly-transparent dark frame. Contains: a fixed-size monospace dB readout (`allocate_ui` 64×13, `set_min/max_width` both clamped so the box never resizes as text changes length), a vertical slider in dB space (−60 to +6 dB, `add_sized([22, 110])`), and a "0 dB" reference label. Slider change emits `SetClipVolume`. **`draw_waveform()`** scales peak amplitude by `clip.volume` so the waveform visually reflects the clip's gain. **Clip type detection** uses `is_extracted_audio_clip(clip)` from `crate::helpers::clip_query` — the inline `clip_type == ClipType::Video && track_row % 2 == 1` predicate is gone. **Extract Audio button** enabled-check uses `selected_timeline_clip` + `library_entry_for` helpers instead of raw `iter().find()`. **Scrub deduplication:** `last_scrub_emitted_time` (initialized to `f64::NEG_INFINITY`) guards both the ruler drag and the playhead-handle drag. On `drag_started` or `clicked` the emit is unconditional (immediate response). On subsequent drag frames, `SetPlayhead` is only pushed when `(t_clamped - last_scrub_emitted_time).abs() >= 1.0 / 30.0` — sub-frame deltas are dropped. `last_scrub_emitted_time` is updated only on actual emit. Observed result: ~66 MB startup RAM with media loaded, vs higher before — bucket cache no longer pre-fills with near-duplicate frames during the initial post-load scrub. **Clip name labels** are truncated to half the clip's pixel width via `fit_label(text, max_px)` — a private free function at the bottom of the file that uses a character-count heuristic (6.5 px/char at 11px proportional). Do NOT use egui font measurement APIs (`layout_no_wrap`, `glyph_width`) for this — both require `&mut Fonts` and cannot be called inside `ui.fonts(|f| ...)`. Hotkeys: Space, Delete, ←/→. `save_status` auto-clears after 3 s. Ruler timecodes use `velocut_core::helpers::time::format_time`; toolbar clip lookups use `crate::helpers::clip_query`. |
| `modules/export_module.rs` | `ExportModule { filename, quality: QualityPreset, fps, export_aspect: Option<AspectRatio>, clear_confirm_at: Option<std::time::Instant> }`. `QualityPreset` expresses output resolution as a short-side pixel count (480/720/1080/1440/2160); actual `(width, height)` is derived from quality + aspect ratio at render time (both rounded to even for YUV420P). Export aspect ratio defaults to the project ratio but can be overridden per-export. **`is_encoding`** is computed at the top of `ui()` (before the header renders) so both the header reset button and the progress overlay share the same value. **Header** is a `ui.horizontal` row: `🚀 Export` label on the left, two-stage **⊘ Reset** button on the right via `with_layout(right_to_left)`. The reset button implements Grok-style confirmation: first click sets `clear_confirm_at = Some(Instant::now())`; button re-renders as `"⚠ Xs?"` in amber with `request_repaint_after(250ms)` to drive the countdown; second click within 5 s emits `ClearProject` and resets to `None`; countdown expiry auto-resets to `None` on the next render frame without any timer thread. Button is disabled (`add_enabled`) while an encode is running, and any pending confirm is cancelled if encoding starts. **Three UI states:** Idle — filename input (Enter key consumed via `input_mut` to suppress Windows system beep), aspect ratio ComboBox, quality ComboBox with resolved pixel dimensions, fps toggle buttons (24/30/60), transitions info frame, stats frame (duration, clip count, output size, estimated frames, format), Render button disabled when timeline empty. Encoding — progress bar drawn with raw painter (TRACK_BG/TRACK_FG colors), "Rendering N% (frame / total)" label, full-width neutral-styled "◼ Stop Render" button (deliberately not red — cancelling is a normal action); settings remain visible but disabled via `add_enabled_ui`. Done — green ✓ banner auto-dismisses after 5 s via `ui.memory` temp timer (`encode_done_time` key) + `request_repaint()`; manual Dismiss button clears timer immediately. Error — red ✗ banner, manual dismiss only. Render button emits `RenderMP4`; Cancel emits `CancelEncode(job_id)`; Dismiss emits `ClearEncodeStatus`. Aspect ratio display uses `velocut_core::helpers::geometry::{aspect_ratio_value, aspect_ratio_label}`. |
| `modules/audio_module.rs` | `AudioModule { exhausted: HashSet<Uuid> }`. `tick()` only. Manages rodio Sink per active clip. `exhausted` prevents `File::open + Decoder::new` on every tick when a short WAV has already played to completion. Clears on stop/clip-change/playhead-leaving-clip. **Stale sink eviction**: at the start of every playing tick, any sink whose clip UUID is no longer present in `state.timeline` is dropped — handles undo/redo during active playback where the clip that owned the sink may have been removed. The `needs_sink` check is simply `!ctx.audio_sinks.contains_key(&clip.id)` — the map is cleared before inserting the new sink, so a stale entry for a different clip is removed automatically. |
| `modules/video_module.rs` | `VideoModule` (unit struct). `tick()` + `poll_playback()` only. `active_media_id()` static helper. `poll_playback()`: PTS-gated single-slot — fills `pending_pb_frame`, fast-forwards past overdue, promotes to `frame_cache` when `current_time >= frame.pts` (±1 frame, not older than 3s). The `request_repaint()` after frame promotion is correct and non-redundant — frames arrive from a background thread, not input events, so egui will not repaint without an explicit request; documented in a comment. `tick()`: playback → starts/restarts decode pipeline on clip change. Scrub → 3 layers: L1 nearest cached bucket (0 ms), L2 exact-ts decode every drag pixel, L2b coarse 2 s prefetch, L3 precise frame after 150 ms idle debounce. Scrub suppressed during playback. Both `poll_playback` and `tick` call `clip_at_time(state, ...)` (from `crate::helpers::clip_query`) instead of inlining the same find-predicate — the previously diverged duplicate copies are gone. |

---

## Adding a Feature

1. Add `EditorCommand` variant in `velocut-core/src/commands.rs`.
2. Create `modules/mymodule.rs` implementing `EditorModule`.
3. Add `pub mod mymodule;` in `modules/mod.rs`.
4. Add concrete typed field in `VeloCutApp` (app.rs) and initialize in `new()`.
5. Call `self.mymodule.ui(...)` in the appropriate panel in `update()`.
6. Add match arm in `process_command()`.
7. If new `MediaResult` variants are needed: add to `media_types.rs`, handle in `context.rs::ingest_media_results()` only.

---

## Architecture Invariants

These are load-bearing. Violating them causes bugs that are very hard to trace.

- Modules receive `&ProjectState` (read-only) and emit `EditorCommand` only — never mutate state directly.
- `AppContext::ingest_media_results()` is the **sole** translation layer between MediaWorker output and UI state. Do not dispatch results anywhere else. It drains `media_worker.scrub_rx` (scrub `VideoFrame` results) **before** the shared `media_worker.rx` every frame — maintain this order so scrub latency is not affected by probe or encode traffic.
- `PreviewModule.current_frame` is set by app.rs before each `ui()` call. `thumbnail_cache` is write-only from `ingest_media_results` — no module writes to it.
- Audio isolated to `audio_module.rs::tick`. Do not manage rodio sinks elsewhere.
- Video scrub/playback logic isolated to `video_module.rs`. No frame decode logic in app.rs.
- Encode state (`encode_job`, `encode_progress`, `encode_done`, `encode_error`) lives in `ProjectState` as `#[serde(skip)]` fields. `begin_render()` arms them before calling `start_encode`. `ingest_media_results` updates them. `ClearEncodeStatus` zeros them. Do not touch encode state elsewhere.
- `CancelEncode` does **not** clear encode state — it only sets the AtomicBool. State clears only when `EncodeError { msg: "cancelled" }` arrives through the normal result channel, keeping the cancel path identical to the error path.
- `last_frame_req` stores `Option<(Uuid, f64)>` exact timestamp — NOT a bucket index. Scrub fires a decode on every drag pixel.
- Playback frame consumption is PTS-gated via `pending_pb_frame`. Never drain the full `pb_rx` in one tick — video races ahead at decode speed. Promote one frame at a time when `current_time` has caught up.
- `stable_dt` is the master clock. `current_time += stable_dt`. Do not advance current_time from decoded frame timestamps.
- Probe semaphore (max 4) gates in-process FFmpeg only (duration + thumbnail). Waveform + audio CLI run after `drop(_guard)` — do not move them before the semaphore release.
- `frame_bucket_cache` is capped by `MAX_FRAME_CACHE_BYTES` (192 MB). Eviction uses `select_nth_unstable_by_key` (O(N) partial select) to pick the 32 furthest-from-playhead entries without a full sort. `insert_bucket_frame` is the only write path — route all inserts through it so the byte budget stays accurate. If you add a new insert path, call `insert_bucket_frame`, do not write to `frame_bucket_cache` directly.
- DnD: `DND_PAYLOAD` written by LibraryModule on drag start, cleared by TimelineModule on drop **or** immediately when no pointer button is down.
- `AudioModule.exhausted` prevents per-tick `File::open` for clips whose WAV is shorter than their timeline duration.
- PTS comparisons in `decode.rs` are always in **seconds** (not raw PTS units). Raw PTS arithmetic is only used for seek target calculation.
- All FFmpeg seeks go through `helpers::seek::seek_to_secs`. Do not call `ictx.seek()` directly — the Windows EPERM soft-fail guard must be in place at every seek site.
- ffmpeg-the-third 4 API notes: `Stream` has no `.codec()` method — use `codec::context::Context::from_parameters(stream.parameters())` for decoders and `Context::new_with_codec(codec)` for encoders. `set_parameters()` on `StreamMut` requires `AsPtr<AVCodecParameters>`; copy encoder params via `avcodec_parameters_from_context` FFI instead. `packets()` yields `Result<(Stream, Packet), Error>` — always destructure with `?`. `set_frame_rate` requires an explicit `Rational` — do not use `.into()` on a tuple (type inference fails).
- Surgical str_replace edits only; no full rewrites.
- **Always check existing helpers before writing inline logic.** velocut-ui helpers to use: `crate::helpers::clip_query::{timeline_clip, library_entry_for, clip_at_time, selected_timeline_clip, selected_clip_library_entry, is_extracted_audio_clip, linked_audio_clip}` for all timeline/library lookups and V↔A link traversal; `velocut_core::helpers::time::{format_time, format_duration}` for all time display; `velocut_core::helpers::geometry::{aspect_ratio_value, aspect_ratio_label}` for aspect ratio display; `crate::helpers::format::truncate` for byte-safe string truncation in fixed-width UI tiles. velocut-media helpers: `helpers::seek::seek_to_secs` for all seeks; `helpers::yuv::{extract_yuv, write_yuv, blend_yuv_frame}` for all YUV frame operations.

---

## Frame Lifetime & Eviction Rules

This is the trickiest part of the system. Understand it before touching any cache or playback code.

```
Each frame tick, app.rs executes in this exact order:
  1. poll_media()
       └─ poll_playback()   ← frame_cache WRITES happen here
  2. update()
       ├─ preview.current_frame = frame_cache.get(active_media_id)  ← READ
       ├─ panels render (preview shows whatever step 2 just read)
       ├─ tick()            ← more frame_cache evictions happen here
       └─ current_time += stable_dt
```

**Consequence:** Any eviction that happens in `tick()` is one frame too late — `preview.current_frame` already captured the stale value. Evictions that must take effect *before* the next render must go in `poll_playback()`, not `tick()`.

**What lives where:**
- `frame_cache`: keyed by `media_id`. One entry per clip. Holds the texture currently shown in the preview. The scrub path writes here; the playback path writes here via `poll_playback`.
- `frame_bucket_cache`: keyed by `(media_id, bucket_index)`. Rolling cache of decoded scrub frames. Capped by byte budget (`MAX_FRAME_CACHE_BYTES` = 192 MB); evicts 32 furthest from playhead on overflow using O(N) partial select.
- `pending_pb_frame`: single-slot buffer between the 32-frame playback channel and `frame_cache`. Holds the next frame to be promoted when PTS is due.

**Eviction triggers and their locations:**

| Trigger | Where to evict | Why |
|---------|---------------|-----|
| Clip boundary during playback | `poll_playback()` | Must fire before preview reads cache (see Debugging History) |
| Clip boundary during scrub | `tick()` (OK here — scrub is idle when crossing) | `last_frame_req` id mismatch detected |
| Playback stopped | `tick()` → `just_stopped` branch | Clears `pending_pb_frame` and resets tracking |
| Asset reused on two clips | `poll_playback()` wrong_clip guard | Same media_id, different clip — timestamp mismatch catches it |

---

## Debugging History

Lessons learned from bugs that were actually fixed. Check here first when diagnosing frame/playback issues.

### Ghost frame on clip transition during playback (fixed)

**Symptom:** A flash of a wrong frame (from a different position in the incoming clip) for exactly one frame when playback crosses a clip boundary.

**Root cause:** Execution order. `poll_playback()` runs in `poll_media()` before `update()` reads `frame_cache`. `tick()` ran *after* the read, so its `frame_cache.remove()` was one frame too late. Any stale scrub frame left in `frame_cache[new_clip.media_id]` from a previous scrub would be shown for exactly one render tick.

**Fix:** Added a clip-transition eviction block at the top of `poll_playback()` (before all other logic), keyed on `current_media_id != ctx.playback_media_id`. This fires before the UI reads `frame_cache`. `tick()` retains its own eviction as a redundant safety net — it's idempotent.

**Key files:** `video_module.rs` — `poll_playback()` top block, `tick()` playback branch.

**If this regresses:** The eviction in `poll_playback()` is the primary guard. Check that it runs before `preview.current_frame` is set in `app.rs::update()`. If `app.rs` call order has changed, this is the first place to look.

### 3× speed playback bug (pre-existing, already fixed in design)

**Symptom:** Video plays at 3–4× speed.

**Root cause:** Draining the entire `pb_rx` channel in one tick and showing the last frame. The decode thread fills the 32-frame channel faster than wall-clock time passes.

**Fix (already in place):** The `pending_pb_frame` single-slot design. Only one frame is promoted per tick, and only when `current_time >= frame.pts`.

### `horizontal_wrapped` / `Grid` refuses to wrap inside `ScrollArea` (fixed)

**Symptom:** Library clips render in a single horizontal row regardless of panel width, never wrapping to a new row.

**Root cause:** `egui::ScrollArea::vertical()` defaults to `auto_shrink = [true, true]`, which gives the inner `Ui` *unbounded* horizontal space during the layout measurement pass so it can then shrink-to-fit. Both `horizontal_wrapped` and `egui::Grid` see this unbounded width and conclude they never need to wrap. `set_max_width`, `auto_shrink([false, true])`, and `allocate_ui` wrappers all fail to fix this reliably across egui versions because the measurement pass still sees unbounded space at some point in the layout chain.

**Fix:** Manual row chunking. Compute column count from `ui.available_width()` *before* the `ScrollArea` opens (where it's still accurate), then `for row in state.library.chunks(cols) { ui.horizontal(...) }`. There is no layout negotiation — each row is just a plain horizontal strip. This is unconditionally correct.

**If this is ever revisited:** Do not attempt `horizontal_wrapped`, `Grid::num_columns`, or any `set_max_width` approach. The chunk+horizontal pattern is two lines and works forever. The only reason to change it would be if egui adds a true fixed-column flow layout as a first-class widget.

**Key file:** `modules/library.rs` — `cols` computed before `ScrollArea`, `state.library.chunks(cols)` loop inside it.

### Export MP4 tail freeze — video shorter than audio (fixed)

**Symptom:** Exported MP4 video stream ends 2–3 seconds before the audio stream. Player shows a frozen last frame during the audio tail. `ffprobe` confirms `video.duration < audio.duration`. Occurs only when 2+ clips are on the timeline; single-clip exports are unaffected.

**Root cause (multi-part):**

1. **`output_frame_idx` in wrong units** (`encode.rs` encoder-flush block, `run_encode`).
   After `pkt.rescale_ts(frame_tb, ost_video_tb)`, the packet PTS is in `ost_video_tb` units (MP4 muxer rewrites stream timebase after `write_header()`; observed value `1/15360`). The old code read `raw_pts` from the already-rescaled packet and stored it in `output_frame_idx`. At 30 fps with `ost_video_tb=1/15360`, frame 289 → `raw_pts=147,968` → `target_audio_samples ≈ 225 million` → audio trim fires with `excess=0` → full 2.35 s overrun written.

2. **`out_frame_idx` gated on `frame_written`** in the `encode_clip` decoder-flush block.
   libx264 B-frame lookahead holds 2–3 frames without emitting a packet. The conditional `if frame_written { out_frame_idx += 1 }` under-counted frames returned from `encode_clip`, compounding the audio trim error.

3. **No audio ceiling in `encode_clip`** demuxer loop or audio-decoder flush.
   `drain_fifo(false)` is called eagerly on every decoded audio frame. For a source file that extends 2 s past `clip_end`, ~86 full AAC frames were encoded and written before the post-encode trim block in `run_encode` ever ran. The trim block could only remove the ≤1023 leftover FIFO samples — the already-written overrun was permanent.

**Patches applied** (all in `encode.rs`):
- Capture `frame_pts = pkt.pts()` **before** `rescale_ts`; use `frame_pts + 1` for `output_frame_idx.max(...)`.
- Move `out_frame_idx += 1` to immediately after `send_frame` in the decoder-flush block (unconditional, mirrors main loop).
- Add `if pts_secs >= clip_end { continue; }` in the demuxer-loop audio branch and `if pts_secs >= clip_end { break; }` in the audio-decoder flush.

**Status:** Fixed. All three patches confirmed working.

**Key files:** `encode.rs` — `run_encode` encoder-flush block, `encode_clip` decoder-flush block, audio ceiling guards.

**If this regresses:** Check `output_frame_idx` units first (must be frame-count / `1/fps`, not `ost_video_tb` units). Then check audio ceiling guards are present in both demuxer-loop audio branch AND audio-decoder flush. The post-encode trim block in `run_encode` is a final safety net for ≤1 AAC frame of overrun — it cannot compensate for multi-second overruns that were already written.

---

### Video freezes after seek, then jumps forward (fixed)

**Symptom:** Video freezes after a seek, then jumps forward.

**Root cause:** `burn_to_pts` runs synchronously and can take 600ms+ on long GOPs. During that time, `current_time` advances via `stable_dt`. The first frame sent by the playback thread has PTS `T`, but by the time it arrives `local_t` is `T + burn_time`. The lower-bound check (`f.timestamp >= lt - 3.0`) was too tight and rejected valid frames permanently.

**Fix (already in place):** 3.0s lower bound in `poll_playback()` Step 3. The `too_old` guard in the stale-frame check uses the same 3.0s threshold.

### Minimize-to-idle / focus memory reduction (investigated, not implemented)

**Goal:** Reduce memory usage when the window is minimized by evicting frame caches and suppressing repaints.

**What was tried:** Gating `ctx.request_repaint()` on `focused && !minimized`; clearing `preview.current_frame` while unfocused; calling a `clear_frame_caches()` method on `CacheContext` on the minimize edge (dropping `frame_cache`, `frame_bucket_cache`, `pending_pb_frame`).

**Why it was reverted:** Gating `request_repaint` while minimized also suppresses `tick()` running (egui doesn't call `update()` without a pending repaint), which is where the normal `frame_bucket_cache` eviction happens. This broke the organic memory recovery that already works correctly — after playback stops, memory settles back down dynamically without any intervention. Adding the minimize logic caused memory to spike to ~160 MB after playback and not recover, whereas the unmodified app recovers to ~100 MB on its own.

**Baseline figures (release build, 2 clips loaded, 2 on timeline):** ~72 MB minimized, ~80 MB focused/idle, ~100 MB after play+stop (recovers organically), ~160 MB peak during playback.

**If revisited:** The correct approach would be to evict caches in `poll_media()` (which runs unconditionally before the repaint gate) rather than in `update()`, so eviction is decoupled from the repaint schedule. Alternatively, use `ctx.request_repaint_after(Duration::from_secs(N))` to keep `tick()` alive at a low rate while minimized instead of suppressing it entirely.

---

### Waveform disappears after undoing Extract Audio Track (fixed)

**Symptom:** After using Extract Audio Track and then undoing it, the video clip's waveform is blank even though `audio_muted` is correctly restored to `false`.

**Root cause:** `waveform_peaks` is populated by the media worker after probing completes. If the undo snapshot was taken (via `PushUndoSnapshot` at the start of the extract action) before the probe had returned the waveform data, the snapshot contains a `LibraryClip` with `waveform_peaks = []`. Restoring that snapshot correctly rewinds `audio_muted` but also rewinds the peaks back to empty.

**Fix:** `restore_snapshot()` in `app.rs` now walks the restored library and re-queues a probe (via `pending_probes`) for any clip with empty `waveform_peaks`. The probe completes within a frame or two, repopulating the peaks. The `already_queued` check prevents duplicates with anything already in `pending_probes` from the live state.

**Key file:** `app.rs` — `restore_snapshot()`.

**If this regresses:** Check that `restore_snapshot()` still does the empty-peaks probe re-queue after applying all the runtime-field preservations. The re-queue must happen after `pending_probes` is moved in from the live state (via `mem::take`) so the dedup check against `already_queued` is accurate.

---

### Phantom audio playing after undo during playback (fixed)

**Symptom:** After undoing a timeline change during active playback, audio from a clip that no longer exists continues playing.

**Root cause:** `audio_module.rs` creates a rodio Sink keyed by `clip.id`. Undo removes the clip from `state.timeline`, but the sink lives in `ctx.audio_sinks` and is never dropped — rodio's decode thread keeps running indefinitely.

**Fix:** At the top of every playing tick, `audio_module.rs::tick()` diffs `ctx.audio_sinks.keys()` against a `HashSet` of current `state.timeline` clip IDs and drops any stale entries before doing any other work.

**Key file:** `modules/audio_module.rs` — top of `tick()`, just after `audio_was_playing = true`.

**If this regresses:** Confirm the stale-sink eviction loop runs unconditionally on every playing tick, before the `active_clip` search. If it's gated or moved after the sink rebuild logic, a phantom sink can be re-created before the eviction fires.

---

### Extracted audio clips duplicating video/audio in export (fixed)

**Symptom:** Exported MP4 has doubled video frames and doubled audio for any clip that had Extract Audio Track applied.

**Root cause:** `begin_render()` was building a `ClipSpec` for every `TimelineClip` in sorted order. An extracted A-row clip shares the same `source_offset`, `duration`, and `media_id` as its V-row partner — so `encode_clip` opened the same file twice and wrote two full video streams plus two audio streams for that time range.

**Fix:** `begin_render()` now filters out any clip where `is_extracted_audio_clip(clip)` is true (canonical predicate: `track_row % 2 == 1 && linked_clip_id.is_some()`). The V-row clip's `ClipSpec` handles the complete encode for that segment. For V-row clips with `audio_muted = true`, `begin_render()` calls `linked_audio_clip(state, clip)` to look up the A-row clip's `volume` and uses it as `effective_volume` so the export honours the user's per-clip gain setting on the extracted track.

**Key file:** `app.rs` — `begin_render()` `clip_specs` filter/map chain; `crate::helpers::clip_query` — `is_extracted_audio_clip`, `linked_audio_clip`.

**If this regresses:** Check that extracted A-row clips are still being filtered by `is_extracted_audio_clip`. Also verify that `effective_volume` correctly reads from the A-row clip via `linked_audio_clip` — if the link is broken (e.g. after a split), it falls back to `tc.volume` on the V-row clip.

---

### Exported clip fails to re-import — "Invalid data found when processing input" (fixed)

**Symptom:** An MP4 exported by VeloCut imports fine visually (thumbnail, timeline placement) but attempting to render it produces `Error: open 'file.mp4': Invalid data found when processing input`. Only affects VeloCut-exported files; camera originals and AI-generated clips are unaffected.

**Root cause:** MP4 containers require H.264 SPS/PPS parameter sets in the `avcC` box in the file header (AVCC format). FFmpeg signals this via `AVFMT_GLOBALHEADER` on the output format. Without `AV_CODEC_FLAG_GLOBAL_HEADER` set on the encoder before `avcodec_open2`, libx264 does not populate `extradata` with SPS/PPS. `avcodec_parameters_from_context` then copies empty extradata into `codecpar`, the muxer writes an empty `avcC` box, and FFmpeg's demuxer returns `AVERROR_INVALIDDATA` on any subsequent `input()` open.

**Fix:** Before calling `open_as_with` on both the video and audio encoders, check `octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER)` and call `enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER)`. This causes libx264 (and the AAC encoder) to store parameter sets in `extradata` rather than inline in the bitstream, giving the muxer a valid `avcC` box to write.

**Key file:** `encode.rs` — `run_encode`, video and audio encoder setup blocks, before each `open_as_with` call.

**If this regresses:** Check that both the video AND audio encoders have the flag set. A missing flag on just the audio encoder won't produce `AVERROR_INVALIDDATA` but can cause AAC decoder init failures on re-import.

---

### Exported clip thumbnail/duration not loading on re-import (fixed)

**Symptom:** After the GLOBAL_HEADER fix above, re-imported VeloCut exports no longer produce `Invalid data found`, but the library card shows no thumbnail and no duration. The file loads and places on the timeline correctly. Camera originals still probe fine.

**Root cause:** `probe_video_size_and_thumbnail` in `probe.rs` built its `SwsContext` upfront from `decoder.format()` / `decoder.width()` / `decoder.height()`. For AVCC H.264 (the new export format post-GLOBAL_HEADER fix), the decoder initialises its codec context from `extradata` during `avcodec_open2`, but reports coded dimensions (e.g. 1088 for a 1080p clip) rather than display dimensions (1080). The `SwsContext` was then built for a 1088-pixel source, causing a dimension mismatch that corrupted the scaler output and prevented any thumbnail from being sent. The previous Annex-B exports had `decoder.format() == AV_PIX_FMT_NONE` before any packets, so `SwsContext::get` failed and returned early — thumbnail probing always silently failed for VeloCut exports but nobody noticed because they were mostly used as source clips, not re-imported.

**Fix:** Build the `SwsContext` lazily on the first decoded frame using `decoded.format()` / `decoded.width()` / `decoded.height()` — identical to the lazy pattern used in `CropScaler` and `LiveDecoder`. This sidesteps both the `NONE` format problem (Annex-B) and the coded-vs-display dimension mismatch (AVCC).

**This fix and the GLOBAL_HEADER encoder fix are a matched pair** — enabling GLOBAL_HEADER changes the bitstream format the encoder emits, and the lazy probe scaler is required to handle that format correctly on import. Reverting one without the other restores the original failure mode.

**Key file:** `probe.rs` — `probe_video_size_and_thumbnail`, SwsContext construction moved inside the `receive_frame` loop.

---

### Choppy scrub on re-imported exported clips (fixed)

**Symptom:** After fixing re-import and thumbnail probing, scrubbing an exported clip is noticeably choppier than scrubbing the original camera clip it was derived from. The issue only affects VeloCut exports, not camera originals or AI-generated clips.

**Root cause:** libx264 default GOP size is `keyint=250` — a keyframe every 250 frames (~8.3 s at 30 fps). The scrub thread (`LiveDecoder`) seeks to the nearest keyframe then burns through frames decode-only to reach the target position. Camera files and AI-generated clips typically have a keyframe every 1–2 s (30–60 frames). With a 250-frame GOP, every scrub seek burns through up to 8× more frames than a camera original, making the scrub thread block much longer between delivered frames.

**Fix:** Add `opts.set("g", &spec.fps.to_string())` to the encoder dictionary in `run_encode`. This sets `keyint=fps` — one keyframe per second regardless of output frame rate. The increase in file size is negligible (~2–5%) for CRF-encoded content.

**Key file:** `encode.rs` — `run_encode`, the `ffmpeg::Dictionary` block before `open_as_with`.

**If this regresses:** Check that the `g` option is being set and that `spec.fps` is the integer frame rate (30, 60, etc.), not a rational. If GOP size crept back to 250, exported clips will scrub choppy again even though playback is unaffected.

---

### Windows EPERM on seek (fixed)

**Symptom:** Encode or decode silently fails to seek, producing frames from the wrong position.

**Root cause:** `avformat_seek_file` returns EPERM on Windows when called with `max_ts=0` on a freshly-opened context, or on container formats that don't support random access.

**Fix (already in place):** All seeks go through `helpers::seek::seek_to_secs`, which skips the seek entirely when `target_secs <= 0.0` (demuxer already at start) and soft-fails with a warning on EPERM, allowing the caller's PTS filter to skip pre-roll frames. Do not call `ictx.seek()` directly.

---

## Resource Lifecycle & Cleanup Checklist

When modifying anything that allocates, make sure these are all handled:

**egui TextureHandle** (`frame_cache`, `thumbnail_cache`, `frame_bucket_cache`)
- egui holds GPU textures behind `TextureHandle`. Dropping the handle releases the GPU resource.
- `frame_cache.remove(&id)` is therefore not just a map cleanup — it's a GPU texture free.
- `frame_bucket_cache` is budget-capped at `MAX_FRAME_CACHE_BYTES` (192 MB). The eviction logic in `CacheContext::insert_bucket_frame` (evict 32 furthest, O(N) partial select) must run on every insert that would overflow. **Never write to `frame_bucket_cache` directly** — always call `insert_bucket_frame` so the byte counter stays accurate.
- `thumbnail_cache` is currently never evicted. This is fine for typical project sizes but will leak for projects with hundreds of unique clips. Future work: evict thumbnails for clips removed from the library.

**Audio temp files** (`$TEMP/velocut_audio_{uuid}.wav`)
- Created by `audio.rs::extract_audio`.
- Queued for deletion in `state.pending_audio_cleanup`.
- `app.rs::poll_media()` drains the queue and calls `cleanup_audio_temp`.
- If a clip is deleted from the timeline, its WAV must be added to `pending_audio_cleanup`. Verify this path in `process_command(DeleteTimelineClip)`.
- If the app crashes, orphaned WAVs are left in `$TEMP`. On restart there is currently no sweep — future improvement.

**Rodio Sinks** (`ctx.audio_sinks`)
- Managed entirely by `audio_module.rs::tick()`. Sinks are dropped when the playhead leaves a clip or playback stops.
- Do not touch `audio_sinks` outside `audio_module.rs`.

**FFmpeg contexts inside `LiveDecoder`**
- `LiveDecoder` owns `ictx` (input context) and `decoder`. These hold file handles and codec state.
- `LiveDecoder` is dropped when `start_playback` replaces the previous decoder on the playback thread.
- The playback thread's `stop_playback` path must drop the `LiveDecoder` — confirm this in `worker.rs`.

**Encode cancel flags** (`encode_cancels: Arc<Mutex<HashMap<Uuid, Arc<AtomicBool>>>>`)
- Flags are inserted on `start_encode` and must be removed after the encode thread exits.
- The encode thread should remove its own entry from the map before returning, whether it finished, errored, or was cancelled. Confirm this in `worker.rs` and `encode.rs`.
- `shutdown()` sets all flags and should also clear the map.

**Probe semaphore permits**
- The semaphore is `Arc<Semaphore>`. Permits are RAII-guarded; they release when `_guard` is dropped.
- Never `return` early inside the probe semaphore scope without dropping the guard first. Use a block scope or explicit `drop(_guard)` before any early return.

---

## Optimization Opportunities

These are not regressions — the app works correctly without them. They're ranked roughly by impact-to-effort ratio.

### Suggested New Helpers (not yet created)

These patterns appear in multiple places and are good candidates for extraction into helpers. Create them when a third callsite appears, or earlier if doing a refactor pass.

**`crate::helpers::format::fit_label(text: &str, max_px: f32) -> String`** (velocut-ui)
Currently a private free function at the bottom of `timeline.rs`. If any other module needs to truncate painter-level text to a pixel budget (e.g. future track header labels, waveform overlays), it should import from a shared location rather than copy the heuristic. Move to `helpers/format.rs` and make pub when a second callsite appears.

### Completed

**✓ Pooled frame buffer in `LiveDecoder` (was #1)**
`LiveDecoder` now owns a `frame_buf: Vec<u8>` pre-allocated to `out_w * out_h * 4`. `next_frame()` and `advance_to()` fill it with `extend_from_slice` per row and `clone()` once for the caller — no per-frame heap allocation in steady state. Eliminates the `flat_map().collect()` iterator overhead and ~55 MB/s of allocation at 640×360 60 fps.

**✓ Decode-only skip in `advance_to()` (was #2)**
`advance_to()` now decodes-only (no swscale, no alloc) for all pre-target frames and scales exactly once on the hit frame, matching the existing `burn_to_pts` fast-path. ~4× faster for high-resolution source material with large GOPs.

**✓ Dedicated scrub result channel (was #3)**
`MediaWorker` now owns a separate `(scrub_tx, scrub_rx)` bounded channel (capacity 8) for scrub `VideoFrame` results. The scrub decode thread sends there instead of the shared `rx`. `ingest_media_results` drains `scrub_rx` first each frame, before the shared `rx`. Scrub responsiveness is fully decoupled from probe/encode load.

**✓ `frame_bucket_cache` eviction O(N) (was #4)**
`CacheContext::insert_bucket_frame` now uses `select_nth_unstable_by_key` (O(N) partial select) to identify the 32 furthest entries, replacing the previous O(N log N) full sort. The cap is now a byte budget (`MAX_FRAME_CACHE_BYTES` = 192 MB) rather than a fixed entry count, so raising the budget doesn't require code changes.

**✓ Reuse `SwsContext` across frames in scrub path (was High #1)**
`LiveDecoder::open()` now accepts `cached_scaler: Option<(SwsContext, Pixel, u32, u32)>`. The scrub thread in `worker.rs` extracts the previous `LiveDecoder`'s scaler (along with its source `decoder_fmt`, `decoder_w`, `decoder_h` key fields) before dropping it on a reset, then passes it to the next `open()` call. If the source format and dimensions match, `SwsContext::get` — which re-runs internal lookup-table initialisation — is skipped entirely. Playback thread always passes `None` (seeks to arbitrary positions; no benefit). Three new `pub` fields on `LiveDecoder`: `decoder_fmt`, `decoder_w`, `decoder_h`.

**✓ Waveform resolution bumped to 4000 columns (was High #2)**
`WAVEFORM_COLS` in `waveform.rs` raised from 1000 → 4000. `draw_waveform()` in `timeline.rs` already sub-samples to the clip's pixel width at render time (`visible = w.min(peaks.len())`), so narrow/zoomed-out clips are completely unaffected. The gain is at high zoom where clips wider than 1000 px previously repeated every 4th peak. Memory cost: ~16 KB per clip (4 000 × f32), negligible for typical library sizes.

**✓ Progressive thumbnail generation (was High #1)**
`LibraryModule` tracks `visible_ids: HashSet<Uuid>` — populated each frame via `ui.is_rect_visible(card_resp.rect)` for every card in the scroll viewport. `poll_media()` in `app.rs` sorts `pending_probes` by this set before dispatching to `probe_clip`, so visible clips race for the semaphore first. Off-screen clips are still probed, just after. No priority queue needed — spawn order is sufficient because the semaphore gatekeeper threads race on creation order.

**✓ Stop-playback drains `pb_rx` (was Low #6)**
`stop_playback()` in `worker.rs` now drains `pb_rx` to empty after sending the Stop command, freeing ~30 MB of RGBA allocations immediately. Doubly important because `stop_playback` is called on every intra-clip seek during playback, not only on explicit user stop.

**✓ Clip-boundary queries consolidated in `video_module.rs` (was Low #5)**
Both `poll_playback()` and `tick()` now call `clip_at_time(state, ...)` from `crate::helpers::clip_query` instead of inlining the same `state.timeline.iter().find(...)` predicate. The previously diverged duplicate copies are gone.

**✓ Eliminate double `input()` open in `decode.rs` (was Low #4)**
Both `LiveDecoder::open()` and `decode_frame()` now build the codec context inside the existing stream-borrow block, eliminating the second `input()` open entirely. The unsafe raw-pointer read for `raw_w`/`raw_h` is also gone — those values now come from `decoder.width()`/`decoder.height()` after the decoder is built.

**✓ `is_extracted_audio_clip` and `linked_audio_clip` helpers added to `clip_query.rs` (was Suggested)**
`is_extracted_audio_clip(clip)` is the canonical single-source predicate for `track_row % 2 == 1 && linked_clip_id.is_some()`. `linked_audio_clip(state, video_clip)` traverses the V↔A link cleanly. Both are now used in `app.rs::begin_render()`, `timeline.rs` render-type detection, and anywhere V↔A traversal is needed. The previously diverged inline copies are gone.

**✓ `request_repaint()` audit in `poll_playback()` (was Low #7)**
The `request_repaint()` call in `poll_playback()` after frame promotion is confirmed correct and non-redundant — frames arrive from a background thread, not input events, so egui will not repaint without an explicit request. Documented in a comment. The scrub-path `request_repaint()` calls in `tick()` were not redundant either, for the same reason.

**✓ Scrub deduplication in `timeline.rs` (was Low #4)**
`TimelineModule` now carries `last_scrub_emitted_time: f64` (initialized to `f64::NEG_INFINITY`). Both the ruler drag and the playhead-handle drag skip `SetPlayhead` emission when `|t - last_t| < 1/30 s`. `drag_started` and plain clicks always emit unconditionally for instant response. Result: ~66 MB startup RAM with media loaded and a clip on the timeline, down from higher values — the bucket cache no longer pre-fills with near-duplicate RGBA frames on every sub-pixel mouse movement.

**✓ `ClearProject` command + two-stage reset button (new feature)**
`ClearProject` added to `commands.rs`. `CacheContext::clear_all()` and `PlaybackContext::reset()` added to `context.rs`. `app.rs::process_command` handles the 8-step ordered teardown. `ExportModule` gains `clear_confirm_at: Option<std::time::Instant>` and a Grok-style two-stage **⊘ Reset** button in the panel header (right of the 🚀 Export label) — first click arms a 5-second countdown displayed as amber `"⚠ Xs?"`; second click within the window fires `ClearProject`; timeout auto-resets with no action. Button is disabled during active encodes.

**✓ Export re-import pipeline hardened — GLOBAL_HEADER, probe lazy scaler, GOP size (fixed)**
Four related fixes landed together. (1) **`AV_CODEC_FLAG_GLOBAL_HEADER`** set on both encoders before `open_as_with` — MP4 container requires SPS/PPS in `avcC` box; without it re-importing the file returns `AVERROR_INVALIDDATA`. (2) **`ost_audio_tb` fetched after `write_header`** — MP4 muxer normalizes timebases during header write; reading before that point gives stale value causing audio drift. (3) **`VideoFrame::new(...)` instead of `VideoFrame::empty()`** in the decoder-flush path of `encode_clip` — empty frame has null data pointers, UB/segfault on sws_scale. (4) **`opts.set("g", &spec.fps.to_string())`** — forces keyframe every second; libx264 default of 250 caused ~8 s GOP burn-through on every scrub seek of an exported clip. In `probe.rs`: **`SwsContext` built lazily from first decoded frame** rather than upfront from `decoder.format()`/`decoder.width()` — fixes thumbnail probing on AVCC exports (coded-dims mismatch) and Annex-B exports (`AV_PIX_FMT_NONE` before first packet). The encoder GLOBAL_HEADER change and probe lazy-scaler fix are a matched pair.
**Key files:** `encode.rs` — `run_encode` encoder setup; `probe.rs` — `probe_video_size_and_thumbnail`.

**✓ `CropScaler::run` EINVAL on portrait-into-landscape encode (fixed)**
**Symptom:** `CropScaler::run sws_scale returned -22` on any encode that mixed portrait and landscape clips (e.g. 16:9 first, 2:3 second). Pure same-AR projects never triggered it.
**Root cause:** `CropScaler::build` creates a `SwsContext` with declared source dims `crop_w × crop_h`. `run()` was passing `srcSliceY = crop_y` to `sws_scale`. libswscale interprets `srcSliceY` as an offset *within* the declared source height, so `crop_y + crop_h > crop_h` → EINVAL. The "source taller" crop branch (portrait clip, landscape output) is the only path where `crop_y > 0`, which is why landscape-only projects never hit it.
**Fix:** Pre-advance data pointers into row `crop_y` (`crop_y * linesize[0]` for Y, `(crop_y/2) * linesize[1]` for U and V — UV planes are half-height in YUV420P) and pass `srcSliceY = 0`. Consistent with the declared dims; identical pointer-advance pattern already used for horizontal crop.
**Key file:** `encode.rs` — `CropScaler::run()`.

**✓ Mixed-AR clips stretched in preview and export (fixed)**
**Symptom:** A 2:3 clip on a 16:9 timeline appeared horizontally squished in the preview monitor and in the exported file when the project AR was 16:9.
**Root cause:** `LiveDecoder::open()` and `decode_frame()` in `decode.rs` computed `out_h = 640 / project_aspect`. A 2:3 source in a 16:9 project decoded to 640×360 RGBA — already squished. `crop_uv_rect` in `preview_module.rs` then saw matching ARs (both 16:9) and passed full UV `(0,0)→(1,1)`, displaying the distortion. `CropScaler` in `encode.rs` also received pre-distorted input.
**Fix:** Both `open()` and `decode_frame()` now compute `out_h = 640 * src_h / src_w` — source native AR, always. The `aspect` parameter is still accepted (API compat) but silenced with `let _ = aspect`. A 2:3 source now decodes to ~640×960; `crop_uv_rect` crops the top/bottom UV to fit the 16:9 canvas; `CropScaler` center-crops identically from the same undistorted frame. Preview and export match exactly.
**Invariant:** Never use project AR to size decode output. Both downstream consumers need an undistorted source frame and perform the AR crop themselves.
**Key file:** `decode.rs` — `LiveDecoder::open()` and `decode_frame()`.

### High impact

**1. Lower-resolution frames in `frame_bucket_cache` (scrub path)**
Bucket frames are currently stored at full source resolution (up to ~8 MB per 1080p RGBA frame). With a 192 MB budget that's only ~24 frames — cold scrubs miss cache constantly. Storing bucket frames at half resolution (max 640px wide via an extra `SwsContext` pass at insert time) reduces each frame to ~1.2 MB, fitting ~160 frames in the same budget. Full resolution only matters for the L3 precise frame (150 ms idle debounce); when L3 fires, replace the existing bucket entry with the full-res frame. Requires a separate scaler in the scrub decode path in `worker.rs` / `decode.rs` for the downscale insert, and a flag or resolution field on the bucket entry so `ingest_media_results` knows whether the arriving frame is a half-res bucket insert or a full-res L3 replacement.

**2. Velocity-scaled L2b prefetch window**
L2b coarse prefetch is currently a fixed 2-second window forward from the drag position. Fast scrubs (flinging through a 60-second clip) overshoot the window immediately, causing cold misses. Track scrub velocity in `timeline.rs` (delta time / delta wall-clock, smoothed over 3–5 frames) and scale the prefetch window: slow drag = 2 s, fast fling = 8–10 s. Also extend 1 s backward — users frequently reverse direction. Pass velocity or a computed window size through the scrub command or as a separate `RequestScrubPrefetch` variant so `video_module.rs` can use it.

**3. Hover prefetch before drag starts**
L2b prefetch only fires during active drag. Users typically hover over the timeline before clicking to scrub, giving a window to warm the cache. When the mouse is hovering over the track area (not dragging), emit a lightweight `RequestScrubPrefetch(hover_time)` at L2b priority. By the time the drag starts, the area around the initial click position is often already cached. No new infrastructure needed — same code path as L2b, just triggered by hover rather than drag.

### Low impact / code quality

**4. Hover cursor frame preview in `timeline.rs`**
Draw a small floating thumbnail above the cursor when hovering over the track area (not dragging, not playing). Look up the nearest bucket in `frame_bucket_cache` for the clip under cursor — pure cache read, no decode, sub-millisecond. Gives immediate visual feedback while the precise decode is pending. Pass `frame_bucket_cache` as a read-only reference into `timeline.rs::ui()`. Skip the preview if no bucket entry is within a reasonable time window to avoid showing a frame from the wrong position.

---

## Common Pitfalls for Future Development

- **`LibraryModule` is no longer a unit struct.** Always initialize with `let library = LibraryModule::new()` in a `let` binding *before* the `Self { ... }` struct literal, then use the field shorthand `library,`. Do NOT write `library: LibraryModule::new()` inside a struct literal — Rust's parser misreads `TypeName::method()` in that position as return-type notation and refuses to compile. This affects any module that grows owned state in the future.
- **`multi_selection` lives on `LibraryModule`, not `ProjectState`.** It is UI-only and intentionally not serialized. `state.selected_library_clip` is the single-clip anchor used by other modules (preview, timeline DnD). If you need to know the full selection set outside `library.rs`, read `library.multi_selection` directly — do not add it to `ProjectState`. Any new `HashMap` or `Vec` that grows with project content needs a corresponding eviction strategy. The `frame_bucket_cache` cap is the model to follow.
- **Adding a new temp file → add it to cleanup.** Any file created by the media layer must be registered in `state.pending_audio_cleanup` (or a new analogous queue) so it's deleted on clip removal and app exit.
- **Adding a new thread → add it to `shutdown()`.** `MediaWorker::shutdown()` must join or signal every thread it owns. Threads that hold file handles or codec contexts will leak FFmpeg resources otherwise.
- **`poll_playback()` runs before `update()` reads `frame_cache`.** If you add any eviction logic, decide which side of that boundary it belongs on based on whether it needs to take effect before or after the current frame renders.
- **Never call `pb_rx.recv()` (blocking) on the UI thread.** Only `try_recv()`. The playback decode thread may be busy; blocking the UI thread stalls rendering.
- **`stable_dt` is authoritative for timing.** Do not derive timing from frame PTS, decoded frame count, or wall clock in the playback path. `stable_dt` is what keeps audio and video in sync.
- **All seeks must go through `helpers::seek::seek_to_secs`.** The Windows EPERM soft-fail guard must be present at every seek site. Adding a direct `ictx.seek()` call bypasses the guard and will silently produce wrong-position frames on Windows with certain container formats or at offset 0.
- **YUV frame packing/unpacking must go through `helpers::yuv`.** `extract_yuv` / `write_yuv` handle ffmpeg stride correctly. Accessing frame planes directly with naive index math will produce corrupted output whenever ffmpeg adds row padding (which it does for alignment on many resolutions). `blend_yuv_frame` operates on packed buffers — always extract before blending and write back after.
- **egui font measurement requires `&mut Fonts` — use a heuristic instead for painter-level text truncation.** Both `Fonts::layout_no_wrap` and `Fonts::glyph_width` mutate the font atlas (lazy glyph rasterization) and thus require `&mut Fonts`. They cannot be called inside `ui.fonts(|f| ...)` which only yields `&Fonts`. For clipping text in `painter.text()` calls (e.g. clip name labels), use the `fit_label(text, max_px)` character-count heuristic in `timeline.rs` — 6.5 px/char at 11px proportional is accurate enough for label truncation. If you need this in another module, consider moving `fit_label` to `crate::helpers::format`.
- **Extracted audio clips must be excluded from `begin_render()` ClipSpecs.** A-row clips with `linked_clip_id.is_some()` are identified as extracted audio by `is_extracted_audio_clip(clip)` in `crate::helpers::clip_query` (`track_row % 2 == 1 && linked_clip_id.is_some()`). Including them as separate `ClipSpec` entries would cause `encode_clip` to open the same source file twice, writing duplicate video frames AND double audio. The V-row clip's `ClipSpec` handles the full encode for that time range. When the V-row clip has `audio_muted = true`, call `linked_audio_clip(state, clip)` to look up its linked A-row clip's `volume` and use that as `effective_volume` — otherwise the volume slider on the A-row clip is silently ignored in the export.
- **After undo/redo, library clips may have empty `waveform_peaks`.** Undo snapshots are full `ProjectState` clones. If a snapshot was taken while an initial probe was still in flight, `waveform_peaks` in that snapshot will be `[]`. After undo restores the snapshot, `audio_muted` is correctly cleared but the waveform doesn't render because the peaks are missing. `restore_snapshot()` in `app.rs` re-queues probes for any library clip with empty peaks to recover them. If you add other `#[serde(skip)]` fields that are derived from probing, apply the same re-queue pattern here.
- **`ClearProject` must clean up in the correct order (8 steps).** Queue WAV paths *before* clearing `state.library` — once the library is cleared those paths are unrecoverable. Stop and drain the playback thread *before* touching `ProjectState` — the decode thread holds clip references. Drop audio sinks before clearing state — rodio threads reference WAV paths. Call `ctx.cache.clear_all()` to drop GPU textures and reset the byte budget. Call `ctx.playback.reset()` to clean scrub tracking. Clear undo/redo stacks after wiping state — stale snapshots waste memory. See app.rs entry for the complete ordered sequence.
- **`frame_bucket_cache` is accessible to `timeline.rs` for the hover cursor preview.** Pass `&ctx.frame_bucket_cache` into `timeline.rs::ui()` as a read-only reference (same pattern as `thumbnail_cache`). `timeline.rs` must never write to it — all inserts go through `CacheContext::insert_bucket_frame`. The hover preview is a read-only lookup by `(media_id, nearest_bucket_index)` — if nothing is close enough, skip the preview entirely.
- **Never size decode output using project AR.** `LiveDecoder::open()` and `decode_frame()` always decode at source native AR (`out_h = 640 * src_h / src_w`). The `aspect` parameter is API-compat only and ignored for sizing. Both downstream consumers — `crop_uv_rect` in `preview_module.rs` (UV crop to canvas AR) and `CropScaler` in `encode.rs` (center-crop to output AR) — require an undistorted source frame and perform the AR mapping themselves. Using project AR to size decode output pre-stretches the frame, makes both consumers see matching ARs, and silently passes through distorted content with no error.
- **`CropScaler::run` — always pass `srcSliceY = 0`, never `crop_y`.** The SwsContext is built with `crop_w × crop_h` as declared source dims. Passing `srcSliceY = crop_y` makes `crop_y + crop_h > crop_h` → libswscale EINVAL (-22). Instead, pre-advance data pointers to row `crop_y` (`crop_y * linesize[0]` for Y; `(crop_y/2) * linesize[1]` for U/V) and pass `srcSliceY = 0`. This only manifests on portrait-into-landscape encodes where `crop_y > 0`; same-AR or landscape-only projects always have `crop_y = 0` and will never trigger it in testing.
- **MP4 encoder requires `AV_CODEC_FLAG_GLOBAL_HEADER` before `open_as_with`.** For MP4 output, check `octx.format().flags().contains(ffmpeg::format::Flags::GLOBAL_HEADER)` and call `video_enc.set_flags(ffmpeg::codec::flag::Flags::GLOBAL_HEADER)` (and likewise for audio) **before** calling `open_as_with`. Without this, libx264 does not populate `extradata` with SPS/PPS during open, so `avcodec_parameters_from_context` copies empty extradata into `codecpar`, the muxer writes an empty `avcC` box, and any subsequent FFmpeg `input()` open on the file returns `AVERROR_INVALIDDATA`. This flag and the probe.rs lazy-scaler fix are a matched pair — changing one without the other breaks re-import.
- **Fetch `ost_audio_tb` after `write_header`, not before.** The MP4 muxer normalizes stream timebases during `avformat_write_header`. Reading `octx.stream(1).unwrap().time_base()` before that call gives a stale value; audio packets rescaled with it will have wrong PTS, causing drift or reject by the interleaver. Always store it into `AudioEncState.ost_audio_tb` immediately after `write_header`.
- **Never use `VideoFrame::empty()` as the destination for `sws_scale` / `CropScaler::run`.** An empty frame has null data pointers — writing into it is UB and will segfault or silently corrupt memory. Always allocate with `VideoFrame::new(Pixel::YUV420P, width, height)` before passing as `dst`. The decoder-flush path in `encode_clip` previously used `VideoFrame::empty()` here; it is now `VideoFrame::new(...)`.
- **Set GOP size (`g`) to `fps` in encode options for NLE-friendly exports.** libx264 default `keyint=250` means a keyframe only every ~8 s at 30 fps. The scrub thread seeks to the nearest keyframe and burns through frames to reach the target — a 250-frame GOP makes every seek 8× slower than a 30-frame GOP. Always set `opts.set("g", &spec.fps.to_string())` so exported clips scrub as responsively as camera originals.
- **`probe.rs` SwsContext must be built lazily from the first decoded frame, not upfront from `decoder.format()`/`decoder.width()`.** For Annex-B H.264 (no extradata), `decoder.format()` is `AV_PIX_FMT_NONE` before any packets, causing `SwsContext::get` to fail and return early with no thumbnail sent. For AVCC H.264 (GLOBAL_HEADER exports), the decoder initializes from extradata but reports coded dimensions (e.g. 1088) not display dimensions (1080), producing a scaler that maps the wrong source rect. Building from the live `decoded` frame in the packet loop sidesteps both. This is a matched invariant with the GLOBAL_HEADER encoder change.
- **Undo/redo during active playback can leave phantom audio sinks.** After undo removes a timeline clip, `audio_module.rs::tick()` evicts any sink whose clip UUID is no longer in `state.timeline`. This is done at the top of every playing tick via a `timeline_ids` HashSet diff. If you add any other runtime resource keyed by timeline clip UUID (e.g. per-clip decode contexts), apply the same eviction pattern in the same tick location.