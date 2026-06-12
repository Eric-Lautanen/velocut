# Handoff: Split encode.rs and worker.rs into sub-modules

## Status
- 18/20 review tasks completed in prior sessions
- 2 remaining: split `encode.rs` (3011 lines) and finish splitting `worker.rs` (~1580 lines remain)
- `worker/blend.rs` was already extracted (ActiveBlend, crop_rgba, blend_rgba_transition, decode_transition_scrub_frame)

## Task 1: Split encode.rs

**File**: `crates/velocut-media/src/encode.rs` (3011 lines → ~118 KB)

**Proposed structure**:
```
crates/velocut-media/src/encode/
  mod.rs       — Public types (ClipSpec, AudioOverlay, EncodeSpec, HwEncodeCapabilities),
                 re-exports, encode_timeline() orchestrator, run_encode() helper
  hw.rs        — HwBackend enum, HwDeviceContext RAII, try_open_hw_encoder(),
                 try_amf_encoder(), try_nvenc_encoder(), try_vaapi_encoder(),
                 try_videotoolbox_encoder(), open_software_encoder(),
                 probe_hw_encode_capabilities(), probe_*_device()
  audio.rs     — AudioFifo, AudioEncState, DecodedOverlay, fade_gain(),
                 decode_overlay(), flush_audio_resampler(),
                 drain_fifo(), drain_packets(), flush_encoder()
  clip.rs      — encode_clip(), decode_clip_frames(), decode_clip_audio(),
                 CropScaler, send_video_frame(), apply_filter_to_yuv_frame(),
                 push_frame(), apply_transition(), constants
```

**Line mapping** (approximate, from `Select-String` output):
| Lines | Content | Target file |
|-------|---------|-------------|
| 1-100 | imports | mod.rs |
| 101-174 | ClipSpec, AudioOverlay, EncodeSpec, HwEncodeCapabilities, probe_hw_encode_capabilities | mod.rs |
| 175-347 | probe_*_device(), PROGRESS_INTERVAL, AUDIO_RATE, HwBackend | hw.rs |
| 348-857 | try_open_hw_encoder, HwDeviceContext, try_amf/nvenc/vaapi/videotoolbox, open_software_encoder | hw.rs |
| 858-1012 | CropScaler, encode_timeline (start) | clip.rs / mod.rs |
| 1013-1286 | AudioFifo, AudioEncState, DecodedOverlay, decode_overlay, flush_audio_resampler | audio.rs |
| 1287-1982 | run_encode, send_video_frame, apply_filter_to_yuv_frame, fade_gain, encode_clip | clip.rs |
| 1983-2602 | encode_clip (cont), decode_clip_frames, decode_clip_audio, push_frame, apply_transition | clip.rs |
| 2603-3011 | Tests (fade_gain tests) | Keep in mod.rs or move to integration test |

**Approach**:
1. Create `encode/` directory
2. Move current `encode.rs` → `encode/mod.rs` 
3. Extract each module one at a time, updating `mod.rs` imports
4. Run `cargo check` after each extraction

## Task 2: Finish splitting worker.rs

**File**: `crates/velocut-media/src/worker.rs` (~1580 lines after blend.rs extraction)

**Already extracted**: `worker/blend.rs` (ActiveBlend, crop_rgba, blend_rgba_transition, decode_transition_scrub_frame)

**Remaining work**:

### 2a. Extract types to `worker/types.rs`
```
FrameRequest struct (lines 32-41)
PlaybackCmd enum (lines 43-74)
```
These are pure data types with no dependencies on FFmpeg.

### 2b. Extract playback thread to `worker/pb_thread.rs`
The massive closure inside `MediaWorker::new()` (roughly lines 321-1145) is a standalone state machine. Extract it as:

```rust
pub(super) struct PbThread {
    cmd_rx: Receiver<PlaybackCmd>,
    frame_tx: Sender<PlaybackFrame>,
}

impl PbThread {
    pub fn run(self) {
        // ... the existing closure body
    }
}
```

Internal state to move with it: `decoder`, `blend`, `held_blend`, `coasting`, `coast_*`, `prebuffered`, frame counters.

### 2c. Deduplicate semaphore guards to `worker/semaphore.rs`
`SemGuard` (line 1243) and `G` (lines 1349, 1391) are identical — both wrap `Arc<(Mutex<u32>, Condvar)>` and decrement on drop. Create a single generic `SemaphoreGuard`.

## Task 3: Verify build

After all splits:
```bash
cargo check -p velocut-media
cargo check -p velocut-ui  
cargo build --release   # if FFmpeg toolchain available
```

## Key files modified in prior sessions
- `.gitignore` — new file
- `crates/velocut-core/src/state.rs` — version field, path canonicalization, dedup aspect_ratio
- `crates/velocut-core/src/windows.rs` — new: shared FFI helper
- `crates/velocut-core/src/lib.rs` — added `pub mod windows`
- `crates/velocut-core/src/transitions/crossfade.rs` — icon comment fix
- `crates/velocut-media/src/helpers/log.rs` — new: media_log! macro
- `crates/velocut-media/src/helpers/mod.rs` — added `pub mod log`
- `crates/velocut-media/src/helpers/yuv.rs` — simplified extract_yuv/write_yuv params
- `crates/velocut-media/src/decode.rs` — improved safety comments, eprintln→media_log
- `crates/velocut-media/src/encode.rs` — eprintln→media_log, fade_gain tests, simplified yuv calls
- `crates/velocut-media/src/worker.rs` — eprintln→media_log, extracted blend.rs, removed libc
- `crates/velocut-media/src/worker/blend.rs` — new: extracted blend helpers
- `crates/velocut-media/src/{audio,probe,waveform,helpers/seek}.rs` — eprintln→media_log
- `crates/velocut-ui/src/modules/timeline.rs` — removed stray comment
- `readme.md` — removed tempfile from dependency table

## Important: media_log! macro
Throughout velocut-media, `eprintln!` was replaced with `crate::media_log!`. This writes to `%TEMP%/velocut.log`. The macro is defined in `crates/velocut-media/src/helpers/log.rs` and exported via `#[macro_export]`. New files in velocut-media should use `crate::media_log!` instead of `eprintln!`.
