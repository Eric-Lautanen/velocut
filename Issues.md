# VeloCut — Architecture Fix Checklist
> For Claude Code. Stack: Rust · egui 0.33 · ffmpeg-the-third (fork) · crossbeam-channel · rodio · rayon
> Crates: `velocut-core` (no UI/FFmpeg) → `velocut-media` (FFmpeg workers) → `velocut-ui` (egui app)

---

## 🔴 PRIORITY 0 — Playback & Scrub Smoothness (Fix First)

These are user-facing quality blockers. Investigate and fix before anything else.

- [ ] **Scrubbing is glitchy at clip boundaries.** The 4-tier scrub system (L1 bucket cache → L2 exact decode → L2b prefetch → L3 idle) breaks down when the playhead crosses from one clip to another. Identify why frame continuity is lost at boundaries and fix it.
- [ ] **Transition playback is laggy.** Playing clips with transitions (crossfade, wipe, push, etc.) causes significant lag. The blend pipeline (`decoder_b` lazy open → `skip_until_pts` → `blend_rgba_transition` → `apply_rgba` rayon) is too slow for real-time. Find the bottleneck and make transitions play smoothly at full frame rate.
- [ ] **No pre-buffering across clip boundaries during playback.** The 32-frame playback buffer does not appear to pre-load frames from the *next* clip before the current one ends. Implement look-ahead buffering so the transition into the next clip is seamless.
- [ ] **Transition blend frames are not cached.** Every seek or scrub near a transition re-decodes and re-blends from scratch. Blend results should be cached in the frame bucket cache the same way single-clip frames are.

---

## 🔴 PRIORITY 1 — Correctness Bugs

- [ ] **Double filter application in playback pipeline.**
  - `apply_filter_rgba()` is called in the pb decode thread AND again in `poll_playback()` before `load_texture`. Filter effect is applied twice, compounding brightness/contrast/gamma incorrectly.
  - Remove the `apply_filter_rgba()` call from the pb thread. `poll_playback()` / `ingest_video_frame()` in `context.rs` is the single canonical application site.

- [ ] **Waveform peaks bloat undo snapshots.**
  - `waveform_peaks` (4000 × f32 = 16 KB per clip) lives in `ProjectState` and is cloned into every undo snapshot (50 levels). Waveforms are read-only derived data and must never participate in undo.
  - Move `waveform_peaks` out of `ProjectState` into `AppContext` keyed by `media_id: Uuid`. Mark the field `#[serde(skip)]`. Remove the `restore_snapshot()` re-probe workaround — it becomes unnecessary.

- [ ] **`encode_cancels` map never cleaned up.**
  - `HashMap<JobId, Arc<AtomicBool>>` in `worker.rs` grows by one entry per encode job and entries are never removed after `EncodeDone` / `EncodeError`.
  - Call `encode_cancels.remove(job_id)` when either result is received in `context.rs::ingest_media_results()`.

- [ ] **`linked_clip_id` deletion not atomic.**
  - Deleting a video clip does not guarantee its linked audio clip is deleted (and vice versa). Orphaned linked clips with dangling `linked_clip_id` UUIDs accumulate silently in `ProjectState.timeline`.
  - Add `ProjectState::remove_clip(id: Uuid)` that atomically removes the clip and its linked partner. All deletion paths must use this method exclusively.

---

## 🟠 PRIORITY 2 — Architecture / Performance

- [ ] **`process_command()` is a god function.**
  - All `EditorCommand` dispatch is in one match block in `app.rs`. Already large, will grow with every new feature.
  - Decompose into per-domain handler structs (`ClipCommandHandler`, `EncodeCommandHandler`, `ProjectCommandHandler`, etc.) each receiving a narrow mutable view of state. `process_command()` becomes a thin router.

- [ ] **Shared `result_rx` mixes high- and low-priority traffic.**
  - `Thumbnail`, `Duration`, `Waveform`, `VideoFrame`, `EncodeProgress`, `EncodeError`, `EncodeDone` all share one channel. Encode progress floods the channel at 4 msg/sec during active encodes.
  - Split into: `scrub_rx` (already exists, cap 8), `probe_rx` (Duration/Thumbnail/Waveform — unbounded), `encode_rx` (EncodeProgress/Done/Error — cap 32). Drain order in `ingest_media_results()`: scrub → probe → encode.

- [ ] **`ProjectState` mixes serializable data with runtime fields.**
  - `current_time`, `is_playing`, encode status are `#[serde(skip)]` fields alongside persistent project data. Undo snapshots must manually preserve runtime fields in `restore_snapshot()` — fragile.
  - Split into `ProjectData` (serde, undo-able: library, timeline, transitions, markers) and `PlaybackState` (runtime only, lives in `AppContext`: current_time, is_playing, encode status). Undo snapshots clone `ProjectData` only.

- [ ] **Probe semaphore runs at full concurrency during encode.**
  - The 4-permit probe semaphore allows up to 4 concurrent disk-reading probe threads even while an encode is saturating I/O.
  - Reduce probe concurrency to 1 while `encode_running` is true.

---

## 🟡 PRIORITY 3 — Quality / Correctness

- [ ] **`stable_dt` accumulates float drift over long playback.**
  - `current_time += stable_dt` each frame. Drift compounds over minutes of continuous playback.
  - Anchor playback time to `Instant::now()` from playback start. Reset anchor on pause/seek.

- [ ] **O(N) frame cache eviction on every cache miss.**
  - "32 furthest entries from playhead using O(N) partial select" runs on every eviction. Fragile at scale.
  - Replace with `BTreeMap` keyed by distance-from-playhead, or use an `IndexMap` with LRU generation counter. Eviction becomes O(log N).

- [ ] **Audio temp WAV writes the full source file regardless of clip range.**
  - `extract_audio()` decodes from file start to `source_offset + duration`. A 30s clip from a 2-hour source writes ~600 MB to disk before starting.
  - Seek to `source_offset` before beginning decode. Stop at `source_offset + duration`. Temp file size becomes proportional to clip duration only.

- [ ] **No error reporting for probe / waveform / audio failures.**
  - If a clip is corrupt or FFmpeg fails during probe, `result_tx` receives nothing. The clip appears in the library silently broken with no feedback.
  - Add `MediaResult::ProbeError { clip_id, message }`, `WaveformError`, `AudioError`. Handle in `context.rs` with a toast/banner (same pattern as encode error in `export_module.rs`).

---

## 🟢 PRIORITY 4 — Housekeeping

- [ ] **`target/` directory is committed to git.**
  - Run `git rm -r --cached target/` and ensure `/target` is in `.gitignore`.

- [ ] **Magic numbers scattered across codebase.**
  - `192 MB`, `32` (pb buffer), `50` (undo levels), `150ms` (L3 debounce), `4000` (waveform cols), `100` (max thumbnails), `15` (encode progress interval), `4` (probe concurrency) are hardcoded at use sites.
  - Create `velocut-core/src/constants.rs`, define all as `pub const`, reference everywhere.

---

## File → Issue Map

| File | Issues |
|------|--------|
| `worker.rs` | P0 scrub/transition lag · P1 encode_cancels leak · P2 probe semaphore · P2 result_rx split |
| `context.rs` | P0 scrub buffering · P1 double filter · P1 waveform in snapshots · P2 result_rx split |
| `video_module.rs` | P0 scrub boundaries · P0 transition lag · P3 stable_dt drift |
| `app.rs` | P1 waveform snapshots · P2 process_command god fn |
| `state.rs` | P1 waveform peaks · P1 linked_clip_id · P2 ProjectState split |
| `encode.rs` | P0 transition blend perf |
| `audio.rs` | P3 temp WAV range |
| `media_types.rs` | P3 error variants |
| `decode.rs` | P0 cross-clip buffering |
| `.gitignore` | P4 target/ |
| `velocut-core/src/` | P4 constants.rs (new file) |