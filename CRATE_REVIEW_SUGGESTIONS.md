# VeloCut Crate Review — Improvement Suggestions

> Reviewed crates: `velocut-core`, `velocut-media`, `velocut-ui`
> Each crate reviewed independently with actionable suggestions.

---

## 1. velocit-core

### Overview
Pure data crate — no I/O, no UI, no FFmpeg. Contains: commands, state types, media types, filters, transitions, and shared helpers.

### Strengths
- Clean separation of concerns — no egui/FFmpeg leaks into core
- Excellent macro-driven transition registration (`declare_transitions!`)
- Good test coverage for filter pixel math and transition helpers
- Serde serialization for project state is well thought out

### Suggestions

#### 1.1 FilterParams — Add Validation
**Current:** `FilterParams` fields are pub with no bounds checking. UI enforces ranges but the core type doesn't.

**Suggestion:** Add a `validate()` method or use a builder pattern to clamp values at construction:
```rust
impl FilterParams {
    pub fn validated(self) -> Self {
        Self {
            brightness: self.brightness.clamp(-1.0, 1.0),
            contrast: self.contrast.clamp(0.0, 3.0),
            // ... etc
            ..self
        }
    }
}
```

#### 1.2 AspectRatio — Missing `Display` impl
**Current:** `aspect_ratio_label()` is a free function in `helpers::geometry`. The enum itself has no human-readable representation.

**Suggestion:** Implement `std::fmt::Display` for `AspectRatio` that delegates to `aspect_ratio_label()`. This makes logging and UI code more ergonomic.

#### 1.3 TimelineClip — Consider Making filter Non-Optional via Default
**Current:** `filter: FilterParams` with `#[serde(default)]` — works but means every clip carries filter state even when identity.

**Suggestion:** This is actually fine given `is_identity()` fast-path. But consider adding `has_filter()` helper for clarity.

#### 1.4 commands.rs — Add Command Validation
**Current:** `EditorCommand` is a large enum with no validation. Some variants carry invalid combinations (e.g., `MoveTimelineClip` with `new_row` that doesn't match clip type).

**Suggestion:** Add a `validate(&self, state: &ProjectState) -> Result<(), String>` method to catch invalid commands before they reach `process_command`.

#### 1.5 transitions/mod.rs — Registry Could Be Static
**Current:** `registry()` builds a `HashMap` on every call. The `OnceLock` in `velocut-media/src/worker.rs` works around this.

**Suggestion:** Move the `OnceLock<HashMap<...>>` into `transitions/mod.rs` itself so the registry is built once and shared. This removes the need for `velocut-media` to know about registry caching.

#### 1.6 Missing Documentation for `declare_filters!` and `declare_transitions!`
**Current:** Macros have inline docs but no module-level explanation of the registration pattern.

**Suggestion:** Add a top-level `REGISTRY.md` or expand `lib.rs` docs explaining how to add filters/transitions.

---

## 2. velocut-media

### Overview
FFmpeg wrapper crate. Owns all I/O: decode, encode, probe, audio extraction, waveform. Contains the `MediaWorker` thread pool.

### Strengths
- Excellent thread architecture with dedicated channels for scrub vs. probe vs. playback
- Good use of semaphores to limit concurrency (probe, HQ decode)
- Hardware acceleration path (D3D11VA) with graceful CPU fallback
- Reusable `LiveDecoder` avoids per-frame open/seek/close overhead

### Suggestions

#### 2.1 worker.rs — Extract Thread Spawning Logic
**Current:** `MediaWorker::new()` is ~1100 lines with 3+ thread spawn blocks inline. Hard to test, hard to read.

**Suggestion:** Extract each thread's logic into a dedicated struct:
```rust
// scrub_thread.rs
pub struct ScrubThread { ... }
impl ScrubThread {
    pub fn spawn(slot: Arc<...>, tx: Sender<MediaResult>) -> JoinHandle<()> { ... }
}
```
This makes each thread independently testable and reduces `worker.rs` to coordination glue.

#### 2.2 decode.rs — `LiveDecoder` Could Use a Builder
**Current:** `LiveDecoder::open()` takes 5+ parameters including `Option<(SwsContext, ...)>`. The function is ~200 lines.

**Suggestion:** Add `LiveDecoderBuilder`:
```rust
let decoder = LiveDecoder::builder()
    .path(&path)
    .timestamp(ts)
    .aspect(aspect)
    .cached_scaler(cached)
    .forced_size(size)
    .build()?;
```

#### 2.3 encode.rs — Split into Submodules
**Current:** `encode.rs` is ~3000+ lines covering: HW probe, SW fallback, clip encoding, transition blending, audio FIFO, and muxing.

**Suggestion:** Split into:
- `encode/mod.rs` — public types (`EncodeSpec`, `ClipSpec`)
- `encode/hw.rs` — hardware encoder probing
- `encode/clip.rs` — single clip encode logic
- `encode/transition.rs` — crossfade/transition blending
- `encode/audio.rs` — audio FIFO and resampling
- `encode/muxer.rs` — output format setup

#### 2.4 probe.rs — Thumbnail Size Should Be Configurable
**Current:** Hardcoded `thumb_w = 160`.

**Suggestion:** Make this a parameter or constant in `velocut-core` so the UI can request different sizes for retina displays.

#### 2.5 audio.rs — Error Handling Could Be Richer
**Current:** `extract_audio` soft-fails with `eprintln!` and returns nothing on `tx`.

**Suggestion:** Send an `MediaResult::Error` on failure so the UI can show a "WAV extraction failed" indicator instead of silently missing audio.

#### 2.6 waveform.rs — Chunk Size Should Be Configurable
**Current:** `WAVEFORM_COLS = 4000` is hardcoded.

**Suggestion:** Make this a parameter or derive from UI panel width for crisper waveforms on high-DPI displays.

#### 2.7 Missing Tests for encode.rs
**Current:** No unit tests for the encode pipeline.

**Suggestion:** Add at minimum:
- A test that `probe_hw_encode_capabilities()` doesn't panic
- A test that `ClipSpec` → `EncodeSpec` round-trips correctly
- Mock tests for the audio FIFO (inject known samples, verify output)

---

## 3. velocut-ui

### Overview
Egui-based UI crate. Contains the app loop, all panels (library, timeline, preview, export), and the module trait system.

### Strengths
- Clean `EditorModule` trait makes adding panels straightforward
- Good separation between `AppContext` (runtime handles) and `ProjectState` (serializable data)
- Excellent memory management with `MemoryManager` idle trimming
- Comprehensive keyboard shortcuts and hotkey reference

### Suggestions

#### 3.1 app.rs — `process_command` is Too Long (~450 lines)
**Current:** Single match statement with 30+ arms, some arms are 20+ lines.

**Suggestion:** Extract each command category into a method:
```rust
fn process_playback_cmd(&mut self, cmd: PlaybackCommand, ctx: &egui::Context) { ... }
fn process_timeline_cmd(&mut self, cmd: TimelineCommand) { ... }
fn process_export_cmd(&mut self, cmd: ExportCommand) { ... }
```
Or use a command handler trait:
```rust
trait CommandHandler {
    fn handle(&mut self, cmd: EditorCommand, ctx: &egui::Context);
}
```

#### 3.2 modules/timeline.rs — Too Long (~2000+ lines)
**Current:** Contains: toolbar, ruler, clip rendering, drag-and-drop, transition popups, volume popups, filter popups, waveform drawing, hotkey reference.

**Suggestion:** Split into submodules:
- `timeline/toolbar.rs` — toolbar buttons and shortcuts
- `timeline/ruler.rs` — time ruler and zoom
- `timeline/clip.rs` — clip rendering and drag-and-drop
- `timeline/popup.rs` — transition/volume/filter popups
- `timeline/waveform.rs` — waveform drawing

#### 3.3 modules/video_module.rs — Blend Spec Logic Could Be Simpler
**Current:** `build_blend_spec` and `build_incoming_blend_spec` are complex with many edge cases.

**Suggestion:** Consider a `BlendSpecCalculator` that takes the transition and two clips and returns the spec. This would be testable in isolation.

#### 3.4 context.rs — `ingest_media_results` Could Use a Dispatcher Pattern
**Current:** Large match statement with 10+ arms for different `MediaResult` variants.

**Suggestion:** Use a trait-based dispatcher:
```rust
trait MediaResultHandler {
    fn handle(&self, result: MediaResult, ctx: &mut AppContext, state: &mut ProjectState);
}
```
This makes it easier to add new result types without modifying a central switch.

#### 3.5 preview_module.rs — Transport Bar Could Be Extracted
**Current:** Transport bar (play, pause, stop, timecode, volume) is ~200 lines inline.

**Suggestion:** Extract to `modules/transport_bar.rs` or make it a reusable component. The same controls might be useful in a fullscreen mode.

#### 3.6 theme.rs — Consider Using egui's Theme System
**Current:** Manual color constants and `configure_style()` call.

**Suggestion:** Egui 0.34 supports custom themes more formally. Consider defining a `VeloCutTheme` struct that can be swapped at runtime (e.g., for a light mode).

#### 3.7 helpers/clip_query.rs — Excellent, But Could Use More
**Current:** Great helper functions for common lookups.

**Suggestion:** Add more helpers to reduce duplication:
```rust
pub fn clip_ends_at(clip: &TimelineClip) -> f64 { clip.start_time + clip.duration }
pub fn clips_overlap(a: &TimelineClip, b: &TimelineClip) -> bool { ... }
pub fn next_clip_after(state: &ProjectState, clip: &TimelineClip) -> Option<&TimelineClip> { ... }
```

#### 3.8 modules/export_module.rs — Modal Could Be a Separate Module
**Current:** `show_render_modal` and its sub-methods are ~300 lines.

**Suggestion:** Extract to `modules/render_modal.rs` for better separation.

#### 3.9 Missing Accessibility Support
**Current:** No ARIA labels, screen reader hints, or high-contrast mode.

**Suggestion:** Add `egui::accessibility` hints to critical controls. Consider a high-contrast theme variant.

#### 3.10 Keyboard Navigation Could Be Enhanced
**Current:** Good shortcuts exist but tab navigation between panels is not implemented.

**Suggestion:** Add `ui.push_id` and focus management so users can tab between library, timeline, and preview panels.

---

## Cross-Cutting Concerns

### A. Error Handling
**Current:** Many errors are logged via `eprintln!` and silently dropped.

**Suggestion:** Adopt a unified error type:
```rust
pub enum VelocutError {
    Media(ffmpeg::Error),
    Io(std::io::Error),
    InvalidCommand(String),
    // ...
}
```
Propagate errors to the UI for user-visible messages.

### B. Logging
**Current:** Mix of `eprintln!`, `velocut_log!`, and `audio_log`.

**Suggestion:** Standardize on `tracing` crate with spans for async contexts. This gives structured logs, filtering, and performance profiling.

### C. Testing
**Current:** Good unit tests in core, minimal in media/ui.

**Suggestion:** Add integration tests that:
- Import a test video, verify duration/thumbnail arrive
- Place on timeline, verify playhead scrubbing
- Export a 1-second clip, verify output file exists

### D. Documentation
**Current:** Inline docs are excellent. Missing: architecture overview, contribution guide.

**Suggestion:** Add `ARCHITECTURE.md` with diagrams showing data flow between crates and threads.

---

## Priority Ranking

| Priority | Item | Crate |
|----------|------|-------|
| **High** | Split `encode.rs` into submodules | velocut-media |
| **High** | Extract thread spawn logic from `worker.rs` | velocut-media |
| **High** | Split `timeline.rs` into submodules | velocut-ui |
| **Medium** | Add `validate()` to `FilterParams` | velocut-core |
| **Medium** | Extract transport bar from `preview_module.rs` | velocut-ui |
| **Medium** | Add unified error type | all |
| **Low** | Add `Display` for `AspectRatio` | velocut-core |
| **Low** | Runtime theme switching | velocut-ui |
| **Low** | Accessibility improvements | velocut-ui |
