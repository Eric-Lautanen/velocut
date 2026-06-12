# VeloCut Improvement Implementation — Session Tracker

## Current Status
- **Last completed:** Reviewed all crates, created CRATE_REVIEW_SUGGESTIONS.md
- **Current crate:** velocut-core (starting)
- **Current task:** Add FilterParams::validate() method

## Completed Items from CRATE_REVIEW_SUGGESTIONS.md
- [x] Reviewed velocut-core
- [x] Reviewed velocut-media
- [x] Reviewed velocut-ui
- [x] Created CRATE_REVIEW_SUGGESTIONS.md

## In Progress / Not Started
- [ ] 1.1 FilterParams — Add validate() method
- [ ] 1.2 AspectRatio — impl Display
- [ ] 1.3 TimelineClip — has_filter() helper
- [ ] 1.4 commands.rs — Add validate() method
- [ ] 1.5 transitions/mod.rs — Registry static OnceLock
- [ ] 1.6 Run cargo check, clippy, fmt — fix ALL errors/warnings
- [ ] 2.1 worker.rs — Extract Thread Spawning Logic
- [ ] 2.2 decode.rs — LiveDecoder Builder
- [ ] 2.3 encode.rs — Split into Submodules
- [ ] 2.4 probe.rs — Thumbnail size configurable
- [ ] 2.5 audio.rs — Richer error handling
- [ ] 2.6 waveform.rs — Configurable chunk size
- [ ] 2.7 Missing tests for encode.rs
- [ ] 3.1 app.rs — Split process_command
- [ ] 3.2 timeline.rs — Split into submodules
- [ ] 3.3 video_module.rs — BlendSpecCalculator
- [ ] 3.4 context.rs — Dispatcher pattern
- [ ] 3.5 preview_module.rs — Extract transport bar
- [ ] 3.6 theme.rs — Runtime theme switching
- [ ] 3.7 clip_query.rs — More helpers
- [ ] 3.8 export_module.rs — Extract modal
- [ ] 3.9 Accessibility support
- [ ] 3.10 Keyboard navigation
- [ ] A. Unified error type
- [ ] B. Standardize logging
- [ ] C. Integration tests
- [ ] D. Architecture docs

## Session History
| Session | Date | Work Done |
|---------|------|-----------|
| 1 | TBD | Reviewed all crates, created suggestions file |
| 2 | TBD | Starting velocut-core improvements |

## Next Session Instructions

### IMMEDIATE FIRST TASK
**File:** `crates/velocut-core/src/filters/mod.rs`
**Task:** Add `FilterParams::validate()` method that clamps all fields to their valid ranges:
- brightness: -1.0..=1.0
- contrast: 0.0..=3.0
- saturation: 0.0..=3.0
- gamma: 0.1..=4.0
- hue: -180.0..=180.0
- temperature: -1.0..=1.0
- strength: 0.0..=1.0

Then call `.validated()` in `FilterParams::from_preset()` and anywhere else params are constructed.

### WORKFLOW
1. Implement one task at a time
2. Run: `cargo check`, `cargo clippy -- -D warnings`, `cargo fmt`
3. Fix ALL errors and warnings — no `#[allow(...)]` blocks
4. Update this RESUME.md with completed items
5. If context >70%, save RESUME.md and call `handoff()`

### CONTEXT WINDOW MANAGEMENT
- Current context usage: ~69% (starting fresh session)
- Check after every task
- Each velocut-core task is small (~20-50 lines), should fit 3-5 tasks per session
- velocut-media tasks are larger — may need full sessions for encode.rs split

### CRATE ORDER (do not deviate)
1. velocut-core (small, foundational)
2. velocut-media (large, complex)
3. velocut-ui (largest, most files)
