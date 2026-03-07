# VeloCut — Feature Implementation Roadmap
### Top 4 Priority Features: Lightweight, Optimized Approaches

> Stack context: Rust · egui 0.33 · ffmpeg-the-third (forked) · crossbeam-channel · rodio · rayon
> Architecture: `velocut-core` → `velocut-media` → `velocut-ui`

---

## Feature 1 — Text & Titles

**Goal:** Render styled text overlays at configurable in/out times, burned into export and displayed in preview.

### Why It Fits VeloCut's Architecture

Text clips are just another clip type on the timeline. They live in `ProjectState`, emit `EditorCommand`s, and render as a composited RGBA layer — no new threading model needed. Zero FFmpeg dependency for the UI path; only the encode path needs to composite text into YUV frames.

### Data Model (velocut-core)

```rust
// state.rs — add alongside TimelineClip
#[derive(Clone, Serialize, Deserialize)]
pub struct TextClip {
    pub id: Uuid,
    pub track: usize,          // New T1/T2 rows (e.g., row 4/5)
    pub start_time: f64,
    pub duration: f64,
    pub text: String,
    pub font_size: u32,        // px at export resolution
    pub color: [u8; 4],        // RGBA
    pub position: TextPosition,
    pub bold: bool,
    pub fade_in_secs: f64,
    pub fade_out_secs: f64,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum TextPosition {
    LowerThird,
    Center,
    UpperThird,
    Custom { x_norm: f32, y_norm: f32 }, // 0.0–1.0 normalized
}
```

Add `text_clips: Vec<TextClip>` to `ProjectState`.

Add to `EditorCommand`:
```rust
AddTextClip(TextClip),
UpdateTextClip { id: Uuid, new: TextClip },
DeleteTextClip(Uuid),
```

### Preview Path (velocut-ui — zero encode cost)

Use egui's `Painter` to draw text directly on the preview canvas after the video frame texture is rendered. This is essentially free — egui already tessellates text every frame anyway.

```rust
// preview_module.rs — after drawing the video texture
let canvas_rect = /* the video frame rect */;
for tc in &state.text_clips {
    if playhead >= tc.start_time && playhead < tc.start_time + tc.duration {
        let alpha = compute_text_alpha(tc, playhead); // fade in/out
        let pos = resolve_text_position(&tc.position, canvas_rect);
        painter.text(
            pos,
            egui::Align2::CENTER_CENTER,
            &tc.text,
            egui::FontId::proportional(tc.font_size as f32 * scale),
            egui::Color32::from_rgba_unmultiplied(
                tc.color[0], tc.color[1], tc.color[2],
                (tc.color[3] as f32 * alpha) as u8,
            ),
        );
    }
}
```

**Key point:** No texture upload, no RGBA blit — egui draws text natively via its mesh renderer. This costs ~0 ms per frame for typical lower-thirds.

### Encode Path (velocut-media)

For export, use the `ab_glyph` or `rusttype` crate (already transitive via egui) to rasterize text into an RGBA bitmap, then alpha-composite it onto the YUV frame:

```rust
// encode.rs — after apply_filter_yuv(), before writing YUV back
if let Some(text_overlay) = text_clips_at_pts(text_clips, current_pts) {
    let rgba_text = rasterize_text_overlay(&text_overlay, frame_w, frame_h);
    composite_rgba_over_yuv420p(&rgba_text, y_plane, u_plane, v_plane, frame_w, frame_h);
}
```

`composite_rgba_over_yuv420p` does the standard alpha-premultiplied blend:
```
Y_out = Y_bg * (1 - alpha) + Y_text * alpha
```
Use `rayon::par_chunks_mut` on the Y plane for parallelism — same pattern as `apply_filter_yuv`.

### UI (velocut-ui — timeline.rs)

Add T1/T2 as new rows below audio. Text clips render as accent-colored blocks (distinct from video/audio). Double-click opens an inline text editor popup (egui `text_edit_multiline`) with font size, color picker (`egui::color_picker`), position preset, and fade sliders — same popup pattern as the existing volume/filter popups.

### Memory/Performance Impact

- State overhead: ~200 bytes per text clip (negligible)
- Preview overhead: ~0 ms (egui native text)
- Encode overhead: one rasterize per unique text clip per encode job, then cached; composite is O(W×H) with rayon — ~2 ms at 1080p

---

## Feature 2 — Clip Speed Control

**Goal:** Per-clip playback speed multiplier (0.25× – 4×) with pitch-preserving audio.

### Why It's Clean in VeloCut's Architecture

Speed is stored as a scalar on `TimelineClip`. The `LiveDecoder` already has a `source_offset + duration` model — speed just compresses/expands the source window. The encode path applies `setpts` and `atempo` FFmpeg filters. Preview path adjusts PTS arithmetic.

### Data Model (velocut-core)

```rust
// state.rs — add to TimelineClip
pub speed: f64,   // default 1.0; range 0.25–4.0
```

Add to `EditorCommand`:
```rust
SetClipSpeed { clip_id: Uuid, speed: f64 },
```

The **effective source duration** read from disk is `timeline_duration / speed`. So a 10s clip at 0.5× needs 20s of source material; at 2× it needs only 5s. Update `ClipSpec`:

```rust
// media_types.rs / ClipSpec
pub speed: f64,
// effective source duration = duration / speed
```

### Encode Path — FFmpeg filter_complex

In `encode.rs`, when `clip.speed != 1.0`, insert `setpts` and `atempo` into the filter graph before scaling. The PTS math is:

```
video: setpts = (1.0 / speed) * PTS
audio: atempo = speed   (chain two if outside 0.5–2.0 range)
```

For speeds outside atempo's 0.5–2.0 range, chain filters:
- 4× speed: `atempo=2.0,atempo=2.0`
- 0.25× speed: `atempo=0.5,atempo=0.5`

Helper function:
```rust
fn atempo_chain(speed: f64) -> String {
    // Build a comma-separated atempo chain
    // Each stage is clamped to [0.5, 2.0]
    let mut remaining = speed;
    let mut stages = Vec::new();
    while (remaining - 1.0).abs() > 0.001 {
        let stage = remaining.clamp(0.5, 2.0);
        stages.push(format!("atempo={:.4}", stage));
        remaining /= stage;
    }
    stages.join(",")
}
```

### Preview/Scrub Path

In `video_module.rs`, the PTS lookup for scrub becomes:
```rust
let source_ts = clip.source_offset + (playhead - clip.start_time) * clip.speed;
worker.request_frame(media_id, source_ts, aspect);
```

For playback, the `PlaybackTransitionSpec` and `build_blend_spec` already pass source timestamps — simply multiply the delta by `clip.speed` before passing to the decoder.

The waveform display stretches/compresses horizontally based on speed — multiply `clip_pixel_width` by `(1.0 / speed)` when sampling the peak array. A `🐢` or `⚡` badge on the timeline clip block (colored accent) communicates speed visually to the user.

### UI

Speed slider in the volume/fade popup — a third column or a new "Speed" section. Use a `DragValue` with step 0.05 and display as `{speed:.2}×`. Preset buttons: `0.25×`, `0.5×`, `1×`, `2×`, `4×` for quick selection.

### Memory/Performance Impact

- State overhead: 8 bytes per clip
- Scrub overhead: zero — just changes the `source_ts` arithmetic
- Encode overhead: filter graph setup is O(1); actual re-encoding happens anyway

---

## Feature 3 — Dynamic Track Count

**Goal:** Let users add/remove video and audio tracks beyond the current 4-lane (V1/A1/V2/A2) limit.

### Why the Current 4-Lane Model Is a Ceiling

The current `track: usize` field on `TimelineClip` already supports arbitrary lane indices — only the UI and `ProjectState` are hardcoded to 4 rows. This means expansion is mostly a UI change with small state additions.

### Data Model (velocut-core)

```rust
// state.rs — add to ProjectState
pub video_track_count: usize,  // default 2
pub audio_track_count: usize,  // default 2
```

Replace the hardcoded 4-row loop in `timeline.rs` with:
```rust
let total_rows = state.video_track_count + state.audio_track_count;
// Even rows = video, odd rows = audio (existing convention preserved)
```

Add to `EditorCommand`:
```rust
AddVideoTrack,
AddAudioTrack,
RemoveTrack { row: usize },   // only if empty
```

### UI Changes (timeline.rs)

The `+` track button (small `⊕` at the left edge of the track header area) adds a row. Each track header shows a `🗑` remove button that is only enabled when the row contains no clips — this prevents accidental data loss with zero extra validation logic.

The track label convention:
- V1, V2, V3... for even rows
- A1, A2, A3... for odd rows

### Cross-track Drag

The existing cross-track drag code in `timeline.rs` already computes `target_row` from pointer Y position and enforces `render_type` constraints (video→even, audio→odd). This logic scales automatically — just ensure the row count comes from `state.video_track_count + state.audio_track_count`.

### Overlay Clips

`active_overlay_clips` in `clip_query.rs` currently returns all non-V1/A1 clips. Expand it to return all clips on rows ≥ 2 — this covers the N-track case without any other changes to the encode pipeline.

### Scroll Behavior

The timeline vertical scroll area needs a minimum height update:
```rust
let min_timeline_height = total_rows * TRACK_HEIGHT + RULER_HEIGHT;
```

### Memory/Performance Impact

- State: 2 extra `usize` fields (negligible)
- UI: the row loop is already O(total_rows) — linear scaling
- Encode: `active_overlay_clips` returns more clips; each is already processed correctly
- The 192 MB frame cache budget is independent of track count

---

## Feature 4 — Keyframes (Opacity & Volume)

**Goal:** Place keyframe points on a clip's opacity and volume over time, with linear interpolation. This is the "Phase 1" minimal keyframe system — enough to unlock fade-ins, fade-outs beyond the current 4-param system, and animated overlays.

### Design Philosophy — Keep It Minimal

Full keyframe systems (position, scale, rotation, Bezier curves) are architecturally expensive. A targeted first pass — **opacity and volume only, linear interpolation** — delivers 80% of the creative value at ~15% of the complexity. This integrates cleanly with the existing `fade_in/fade_out` model and `FilterParams` pipeline.

### Data Model (velocut-core)

```rust
// state.rs
#[derive(Clone, Serialize, Deserialize)]
pub struct Keyframe {
    pub time_offset: f64,  // seconds from clip start
    pub value: f64,        // 0.0–1.0 for opacity; dB for volume
}

// Add to TimelineClip:
pub opacity_keyframes: Vec<Keyframe>,   // empty = constant 1.0
pub volume_keyframes: Vec<Keyframe>,    // empty = use existing `volume` field
```

Interpolation helper (velocut-core/helpers):
```rust
pub fn lerp_keyframes(keyframes: &[Keyframe], t: f64) -> f64 {
    if keyframes.is_empty() { return 1.0; }
    if t <= keyframes[0].time_offset { return keyframes[0].value; }
    if t >= keyframes.last().unwrap().time_offset {
        return keyframes.last().unwrap().value;
    }
    // Binary search for the surrounding pair
    let i = keyframes.partition_point(|k| k.time_offset <= t) - 1;
    let a = &keyframes[i];
    let b = &keyframes[i + 1];
    let alpha = (t - a.time_offset) / (b.time_offset - a.time_offset);
    a.value + (b.value - a.value) * alpha
}
```

Add to `EditorCommand`:
```rust
SetOpacityKeyframe { clip_id: Uuid, keyframe: Keyframe },
DeleteOpacityKeyframe { clip_id: Uuid, time_offset: f64 },
SetVolumeKeyframe { clip_id: Uuid, keyframe: Keyframe },
DeleteVolumeKeyframe { clip_id: Uuid, time_offset: f64 },
```

### Opacity in Preview (velocut-ui — context.rs)

`ingest_video_frame()` already applies `FilterParams` to raw RGBA bytes before GPU upload. Add opacity multiplication:

```rust
// After apply_filter_rgba():
let opacity = lerp_keyframes(&active_clip.opacity_keyframes, clip_local_t);
if (opacity - 1.0).abs() > 0.001 {
    for pixel in rgba_bytes.chunks_exact_mut(4) {
        pixel[3] = (pixel[3] as f64 * opacity) as u8;
    }
}
```

`clip_local_t` = `playhead - clip.start_time`. This runs on the CPU before texture upload — same as the existing filter path, same rayon opportunity.

### Opacity in Encode (velocut-media — encode.rs)

After `apply_filter_yuv()`, multiply the Y plane by the opacity scalar (which approximates alpha for overlay blending):

```rust
let opacity = lerp_keyframes(&clip.opacity_keyframes, frame_pts - clip.source_offset);
if (opacity - 1.0).abs() > 0.001 {
    for y in y_plane.iter_mut() {
        *y = (*y as f64 * opacity + 16.0 * (1.0 - opacity)) as u8;
        // 16 = YUV black; blend toward black as opacity decreases
    }
}
```

For overlay clips (V2+), this naturally creates fade-in/fade-out animation on the overlay layer — which is the primary use case.

### Volume Keyframes in Encode

`fade_gain()` in `encode.rs` already computes a per-sample gain scalar. Extend it to multiply by the keyframe volume:

```rust
let kf_gain = lerp_keyframes(&clip.volume_keyframes, pts_from_clip_start);
let gain = volume_linear * fade_gain(...) * kf_gain;
```

### Timeline UI — Keyframe Strip

Add a thin "keyframe lane" (12px tall) directly below each clip block in `timeline.rs`. Diamond-shaped keyframe markers (`◆`) are drawn via `painter.add(Shape::convex_polygon(...))` at their normalized timeline positions.

Interactions:
- **Click on clip keyframe lane**: sets a keyframe at that time with the current clip's opacity/volume value
- **Right-click on existing keyframe**: delete it
- **Drag keyframe diamond**: moves its `time_offset`

The keyframe lane toggles visible via a small `⟡` button on the clip's context menu — hidden by default to keep the timeline clean for new users. This keeps the feature discoverable but not overwhelming.

### Waveform Integration

The existing waveform rendering in `timeline.rs` draws the amplitude overlay. Draw a translucent amber "opacity envelope" line over video clips when keyframes are present — analogous to how the existing fade ramp lines are drawn:

```rust
if !clip.opacity_keyframes.is_empty() {
    // Draw polyline connecting keyframe values across clip width
    let points: Vec<Pos2> = clip.opacity_keyframes.iter().map(|kf| {
        let x = clip_x + (kf.time_offset / clip.duration) as f32 * clip_w;
        let y = clip_y + clip_h * (1.0 - kf.value as f32);
        Pos2::new(x, y)
    }).collect();
    painter.add(Shape::line(points, Stroke::new(1.5, AMBER)));
}
```

### Memory/Performance Impact

- State: `Vec<Keyframe>` — empty for most clips (zero allocation); typical use is 2–8 keyframes per clip
- Scrub/playback: one binary search per frame = O(log N) where N is keyframe count — negligible
- Encode: same O(log N) per frame
- Undo: full `ProjectState` clone already includes `Vec<Keyframe>` — no new undo logic needed

---

## Implementation Order Recommendation

| Phase | Feature | Complexity | Impact |
|-------|---------|------------|--------|
| 1 | Clip Speed Control | Low | High — immediately useful for all users |
| 2 | Dynamic Track Count | Low-Medium | High — unlocks real multi-layer projects |
| 3 | Text & Titles | Medium | Very High — most-requested missing feature |
| 4 | Keyframes (Phase 1) | Medium | High — enables animated overlays + pro fades |

Speed and track count can likely be shipped in the same PR. Text and keyframes are best as separate PRs due to the new `TextClip` type and keyframe strip UI work respectively.

---

## Shared Utilities to Add Once

These helpers benefit all four features and should be added to `velocut-core` upfront:

```rust
// helpers/interpolate.rs
pub fn lerp(a: f64, b: f64, t: f64) -> f64 { a + (b - a) * t.clamp(0.0, 1.0) }
pub fn lerp_keyframes(keyframes: &[Keyframe], t: f64) -> f64 { /* see Feature 4 */ }
pub fn ease_in_out(t: f64) -> f64 { t * t * (3.0 - 2.0 * t) } // already in transitions/helpers.rs — move here

// helpers/yuv.rs additions
pub fn composite_rgba_over_yuv420p(rgba: &[u8], y: &mut [u8], u: &mut [u8], v: &mut [u8], w: usize, h: usize);
pub fn multiply_yuv_opacity(y: &mut [u8], opacity: f64);
```

These are pure functions with no dependencies — safe to add to `velocut-core` and used from both `velocut-media` (encode path) and `velocut-ui` (preview path).

---

## Feature 5 — Linux Port

**Goal:** Ship a working `.AppImage` for x86_64 Linux. The egui/eframe stack is already cross-platform — the blockers are FFmpeg static linking, platform-specific `#[cfg]` guards, and the custom chrome.

### Blockers to Fix

**1. `fix_taskbar_icon()` — Windows-only Win32 API**

```rust
// main.rs — current code
#[cfg(target_os = "windows")]
fn fix_taskbar_icon() { /* EnumThreadWindows / WS_EX_APPWINDOW */ }
```

This is already isolated. Just ensure the call site is also `#[cfg(target_os = "windows")]` and add a no-op stub for other platforms:

```rust
#[cfg(not(target_os = "windows"))]
fn fix_taskbar_icon() {}
```

**2. `helpers/seek.rs` — Windows EPERM soft-fail guard**

The comment says "skips if `ts<=0.0` (Windows EPERM)". Wrap the skip-guard with:
```rust
#[cfg(target_os = "windows")]
if ts <= 0.0 { return Ok(()); }
```
On Linux/macOS, seeking to 0.0 is safe — the guard caused by Windows container quirks does not apply.

**3. `velocut_log!` — `%TEMP%` path**

Replace the Windows-specific temp path with `std::env::temp_dir()`, which returns the correct platform temp dir on all OSes:
```rust
// helpers/log.rs
let log_path = std::env::temp_dir().join("velocut.log");
```

**4. `reset.rs` — App data directory**

Use the `dirs` crate (`dirs = "5"`) for cross-platform config/data paths:
```rust
// Instead of Windows %APPDATA% hardcoding:
let data_dir = dirs::data_dir().unwrap_or_default().join("velocut");
```

**5. HW Decode on Linux — D3D11VA → VAAPI**

`decode.rs` currently tries D3D11VA. Add a fallback chain via `#[cfg]`:
```rust
#[cfg(target_os = "windows")]
let hw_type = ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA;
#[cfg(target_os = "linux")]
let hw_type = ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI;
#[cfg(target_os = "macos")]
let hw_type = ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX;
```

The encode path in `encode.rs` already has `VAAPI` in its HW fallback chain — confirm it correctly initializes a VAAPI device context on Linux (needs `/dev/dri/renderD128` access).

**6. Static FFmpeg Build on Linux**

The `howtostaticffmpegw64.md` doc covers Windows MINGW64. Add a `howtostaticffmpeglinux.md` covering the Linux build. Use the `ffmpeg-build-script` approach:

```bash
git clone https://github.com/markus-perl/ffmpeg-build-script
./build-ffmpeg --build --enable-gpl-and-non-free
# Outputs static .a libs to workspace/
export FFMPEG_DIR=$(pwd)/workspace
export CARGO_FEATURE_STATIC=1
cargo build --release
```

Note: VAAPI cannot be statically linked on Linux (it's a runtime driver interface). Build FFmpeg with `--enable-vaapi` for dynamic linking of the VAAPI bridge only — the rest stays static.

**7. egui-desktop Custom Chrome on Linux**

`egui-desktop` draws minimize/maximize/close buttons identically across Windows, macOS, and Linux since they're all egui-painted. The `render_resize_handles` call in `main.rs` should work on Linux without changes. Test on both X11 and Wayland (eframe supports both via winit). Wayland may need:
```rust
// NativeOptions
viewport: egui::ViewportBuilder::default()
    .with_decorations(false)
    // Wayland: set app_id for compositor window grouping
    .with_app_id("velocut"),
```

**8. Linux System Dependencies for eframe**

On Linux, eframe requires: `libclang-dev libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev`. Document these in a `BUILDING_LINUX.md` and bundle them in the AppImage.

### AppImage Packaging

Use `cargo-appimage` or `linuxdeploy`:

```bash
cargo build --release --target x86_64-unknown-linux-gnu
linuxdeploy --appdir AppDir \
    --executable target/release/velocut \
    --desktop-file assets/velocut.desktop \
    --icon-file assets/icon.png \
    --output appimage
```

The resulting `VeloCut-x86_64.AppImage` is self-contained and runs on any Linux kernel ≥ 3.2.

### CI Matrix

Add to `.github/workflows/build.yml`:
```yaml
strategy:
  matrix:
    os: [windows-latest, ubuntu-22.04]
```

---

## Feature 6 — macOS Port

**Goal:** Ship a signed `.dmg` for Apple Silicon (arm64) and Intel (x86_64). This is the harder of the two ports due to Apple's security requirements and VideoToolbox integration.

### Blockers to Fix

**1. Static FFmpeg with VideoToolbox on macOS**

VideoToolbox cannot be fully statically linked — it's a macOS framework. Build FFmpeg with:
```bash
./configure \
  --enable-videotoolbox \
  --enable-audiotoolbox \
  --enable-static \
  --disable-shared \
  --extra-ldflags="-framework VideoToolbox -framework CoreMedia -framework CoreVideo"
```

The existing HW encoder fallback chain in `encode.rs` already includes `VideoToolbox` — ensure the framework linker flags are set in `build.rs` for macOS:
```rust
#[cfg(target_os = "macos")]
println!("cargo:rustc-link-lib=framework=VideoToolbox");
println!("cargo:rustc-link-lib=framework=CoreMedia");
println!("cargo:rustc-link-lib=framework=CoreVideo");
println!("cargo:rustc-link-lib=framework=AudioToolbox");
```

**2. Universal Binary (arm64 + x86_64)**

Build both targets and combine with `lipo`:
```bash
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
lipo -create \
  target/aarch64-apple-darwin/release/velocut \
  target/x86_64-apple-darwin/release/velocut \
  -output target/universal/velocut
```

**3. macOS Custom Chrome — Traffic Light Buttons**

The frameless window with custom chrome from `egui-desktop` works on macOS, but the convention for macOS apps is that the traffic light buttons (close/minimize/maximize) sit in the top-left, not top-right. Add a platform branch in `main.rs`:
```rust
#[cfg(target_os = "macos")]
let title_bar = TitleBar::new("VeloCut").with_traffic_lights_left(true);
#[cfg(not(target_os = "macos"))]
let title_bar = TitleBar::new("VeloCut");
```

Alternatively, on macOS only, use the native titlebar and rely on `NSWindow` styling to achieve the dark chrome look — this is the path of least resistance for App Store compliance.

**4. Code Signing and Notarization**

Apple requires all distributed binaries to be signed and notarized or macOS Gatekeeper will block them. The process:

```bash
# Sign
codesign --deep --force --sign "Developer ID Application: Your Name (TEAMID)" \
  --entitlements entitlements.plist target/universal/velocut.app

# Notarize
xcrun notarytool submit velocut.dmg \
  --apple-id your@email.com --team-id TEAMID --password APP_SPECIFIC_PW \
  --wait

# Staple
xcrun stapler staple velocut.dmg
```

The `entitlements.plist` needs at minimum:
```xml
<key>com.apple.security.cs.allow-unsigned-executable-memory</key><true/>
```
(Required by egui's JIT-style rendering on Apple Silicon.)

**5. `rfd` File Dialog on macOS**

The `rfd` crate (used for native file dialogs) already supports macOS. No changes needed, but test that the file dialog does not block the egui event loop — use `rfd::AsyncFileDialog` if blocking is observed.

**6. Audio on macOS — rodio + CoreAudio**

`rodio` uses CoreAudio on macOS. The WAV temp file extraction path in `audio.rs` uses `std::env::temp_dir()` (once the Linux fix from Feature 5 is applied), which resolves correctly to `$TMPDIR` on macOS. No further changes needed.

**7. App Store Considerations (Optional)**

For Mac App Store distribution, the app must use the App Sandbox entitlement. This restricts file access to user-chosen locations via `NSOpenPanel` (which `rfd` already uses). The main concern is that temp file writes in `audio.rs` must use `NSTemporaryDirectory()` — which is what `std::env::temp_dir()` returns on macOS anyway.

### DMG Packaging

```bash
# create-dmg handles background image, icon layout, and signing
brew install create-dmg
create-dmg \
  --volname "VeloCut" \
  --background assets/dmg-background.png \
  --window-pos 200 120 \
  --window-size 800 400 \
  --icon-size 100 \
  --icon "VeloCut.app" 200 190 \
  --app-drop-link 600 185 \
  "VeloCut.dmg" \
  "dist/VeloCut.app"
```

### CI Matrix (Full)

```yaml
strategy:
  matrix:
    include:
      - os: windows-latest
        target: x86_64-pc-windows-gnu
        artifact: velocut.exe
      - os: ubuntu-22.04
        target: x86_64-unknown-linux-gnu
        artifact: VeloCut-x86_64.AppImage
      - os: macos-14          # Apple Silicon runner
        target: aarch64-apple-darwin
        artifact: VeloCut-arm64.dmg
      - os: macos-13          # Intel runner
        target: x86_64-apple-darwin
        artifact: VeloCut-x86_64.dmg
```

---

## Additional Improvements

### 7. Fix: `target/` Directory Committed to Git

The repo file tree shows a `target/` directory tracked in git. This is the Rust build output folder and should never be committed — it can be hundreds of MB and causes `git clone` to download all compiled artifacts. Ensure `.gitignore` contains:

```
/target
```

Then remove the cached tree:
```bash
git rm -r --cached target/
git commit -m "chore: remove target/ from tracking"
```

---

### 8. Project File Format (`.vcp`)

Currently `ProjectState` is saved via eframe's built-in storage (a key-value store backed by a platform config file). This works for session restore but has three problems: projects can't be named or shared between machines, there's only one "slot", and the storage location isn't user-visible.

**Proposed:** A simple `.vcp` (VeloCut Project) file — just `serde_json::to_string_pretty(&project_state)` written to a user-chosen path via `rfd`.

```rust
// commands.rs
SaveProject { path: Option<PathBuf> },   // None = "Save As" dialog
OpenProject,
NewProject,
RecentProjects,
```

The `ProjectState` is already fully `serde`-serializable. This is essentially a two-line implementation — the only nuance is that media file paths need to be stored as absolute paths and validated on open (show a "relink media" dialog if files have moved).

Keep eframe storage as the auto-save slot for crash recovery — save there every 60 seconds in `tick()`, independently of the named project file.

---

### 9. More Export Codecs

Currently only H.264/MP4. The FFmpeg static build already includes the codecs — only the UI and `EncodeSpec` need additions:

```rust
// state.rs / EncodeSpec
pub enum VideoCodec {
    H264,       // current default — CRF 18, preset fast
    H265,       // ~40% smaller files, slower encode
    AV1,        // best compression; use libaom-av1 or svt-av1
    ProRes422,  // lossless-ish, for post-production handoffs
}
```

In `encode.rs`, dispatch on `spec.codec`:
- H.265: `libx265`, CRF 24, preset medium
- AV1: `libsvtav1` (faster) with `crf 35`; fallback to `libaom-av1`
- ProRes: `prores_ks`, profile 2 (422 HQ)

Note: ProRes is the killer feature for macOS users who hand off to Final Cut or DaVinci Resolve. Add it as a Mac-specific export preset in the UI.

---

### 10. Timeline Markers

Named markers are low-complexity, high workflow value. The data model is minimal:

```rust
// state.rs
#[derive(Clone, Serialize, Deserialize)]
pub struct TimelineMarker {
    pub id: Uuid,
    pub time: f64,
    pub label: String,
    pub color: [u8; 3],
}

// Add to ProjectState:
pub markers: Vec<TimelineMarker>,
```

In `timeline.rs`, render marker lines on the ruler as vertical accent-colored bars with label tooltips on hover. `M` hotkey drops a marker at the playhead (same UX as Premiere/Resolve). Clicking a marker in the ruler snaps the playhead to it. Right-click → rename or delete.

Markers also serve as chapter points for future SRT/subtitle export and as reference points for the keyframe system.

---

### 11. Export Presets (Save/Load)

The export panel currently has inline resolution/fps/codec controls. Let users save named presets:

```rust
// state.rs
#[derive(Clone, Serialize, Deserialize)]
pub struct ExportPreset {
    pub name: String,
    pub resolution: Resolution,
    pub fps: f64,
    pub codec: VideoCodec,
    pub quality: u32,
}
```

Store presets in a separate JSON file alongside the app config (not in `ProjectState` — presets are user-global, not project-specific). A `+` button in the export panel saves the current settings as a preset. A dropdown loads them. Ship 3 built-in presets: "Web 1080p", "Archive ProRes", "Mobile 720p".

---

### 12. CI/CD and Automated Testing

VeloCut's `velocut-core` crate is pure logic with no UI or FFmpeg dependency — it's ideal for unit testing. Suggested test coverage:

```rust
// velocut-core/src/tests/
#[test] fn test_lerp_keyframes_empty_returns_one()
#[test] fn test_lerp_keyframes_interpolates_correctly()
#[test] fn test_atempo_chain_4x_produces_two_stages()
#[test] fn test_atempo_chain_025x_produces_two_stages()
#[test] fn test_format_time_zero()
#[test] fn test_format_time_rounding()
#[test] fn test_filter_params_identity_skip()
#[test] fn test_transition_registry_all_registered()
```

Add a GitHub Actions workflow:
```yaml
# .github/workflows/ci.yml
on: [push, pull_request]
jobs:
  test:
    runs-on: windows-latest   # core tests; no FFmpeg needed
    steps:
      - uses: actions/checkout@v4
      - run: cargo test -p velocut-core
      - run: cargo clippy -p velocut-core -- -D warnings
```

The `velocut-media` and `velocut-ui` crates can be CI-checked with `cargo check` (no full build needed) once the static FFmpeg libs are cached in CI.

---

### 13. Audio Metering (VU Meter)

A simple VU meter in the preview transport bar would significantly improve the professional feel during playback. The data needed is already flowing — `rodio` audio is decoded to f32le samples in `audio.rs`. Add a `Arc<AtomicU32>` peak level shared between the audio decode thread and the UI, updated every ~50ms:

```rust
// audio_module.rs
// After pushing audio samples to rodio sink, update peak:
let peak = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
peak_level.store(peak.to_bits(), Ordering::Relaxed);
```

In `preview_module.rs`, read the peak and draw a simple stereo meter bar (green → yellow → red) using `painter.rect_filled` calls. Total cost: 2 atomic reads per frame, ~4 rect draws. Zero threading overhead.

---

## Updated Implementation Priority Table

| Phase | Feature | Platform | Complexity | Impact |
|-------|---------|----------|------------|--------|
| 1 | Fix `target/` in git | All | Trivial | Repo hygiene |
| 2 | Clip Speed Control | All | Low | High |
| 3 | Dynamic Track Count | All | Low-Med | High |
| 4 | Linux Port | Linux | Medium | Very High — opens OSS audience |
| 5 | Project File Format `.vcp` | All | Low-Med | High — shareable projects |
| 6 | Text & Titles | All | Medium | Very High |
| 7 | Timeline Markers | All | Low | Medium-High |
| 8 | macOS Port | macOS | Medium-High | High |
| 9 | Keyframes Phase 1 | All | Medium | High |
| 10 | More Export Codecs | All | Low-Med | High (esp. ProRes on Mac) |
| 11 | Export Presets | All | Low | Medium |
| 12 | Audio Metering | All | Low | Medium — polish |
| 13 | CI/CD + Tests | All | Low | High — long-term health |