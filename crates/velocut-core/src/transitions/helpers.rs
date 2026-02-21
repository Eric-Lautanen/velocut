// crates/velocut-core/src/transitions/helpers.rs
//
// Math utilities for transition implementors.
//
// All functions operate on plain f32 or raw byte slices — no FFmpeg, no egui.
// Buffer stride handling lives in `velocut_media::helpers::yuv`.
//
// Organised into sections:
//   - Clamp / lerp
//   - Easing curves
//   - Frame alpha
//   - Pixel blend
//   - Packed YUV420P plane layout  ← spatial transitions need these
//   - Spatial helpers              ← wipes, irises, directional effects

// ── Clamp / lerp ─────────────────────────────────────────────────────────────

/// Clamp `v` to [0.0, 1.0].
#[inline]
pub fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Linear interpolation between `a` and `b` at `t` ∈ [0, 1].
#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ── Easing curves ─────────────────────────────────────────────────────────────
//
// All functions take `t` ∈ [0.0, 1.0] and return a remapped value in [0.0, 1.0].
// Pass the result as `alpha` into `VideoTransition::apply()`.
// Visualize at https://easings.net — names match the site.

/// Smooth-step cubic ease-in/out. Good default for dissolves.
///
/// Zero derivative at both endpoints → no visible pop at start or end.
#[inline]
pub fn ease_in_out(t: f32) -> f32 {
    let t = clamp01(t);
    t * t * (3.0 - 2.0 * t)
}

/// Ease in — starts slow, accelerates.
#[inline]
pub fn ease_in(t: f32) -> f32 {
    let t = clamp01(t);
    t * t
}

/// Ease out — decelerates to the end.
#[inline]
pub fn ease_out(t: f32) -> f32 {
    let t = clamp01(t);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Stronger cubic ease-in/out — more cinematic than smooth-step.
#[inline]
pub fn ease_in_out_cubic(t: f32) -> f32 {
    let t = clamp01(t);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// No easing — pass alpha through unchanged.
///
/// Use for wipes and hard-edge shapes where the boundary should be crisp.
#[inline]
pub fn linear(t: f32) -> f32 {
    clamp01(t)
}

/// Ease out with a single bounce at the end.
///
/// Useful for a wipe edge that "snaps" into position.
#[inline]
pub fn ease_out_bounce(t: f32) -> f32 {
    let t = clamp01(t);
    const N: f32 = 7.5625;
    const D: f32 = 2.75;
    if t < 1.0 / D {
        N * t * t
    } else if t < 2.0 / D {
        let t = t - 1.5 / D;
        N * t * t + 0.75
    } else if t < 2.5 / D {
        let t = t - 2.25 / D;
        N * t * t + 0.9375
    } else {
        let t = t - 2.625 / D;
        N * t * t + 0.984375
    }
}

/// Ease in with a bounce (reverse of `ease_out_bounce`).
#[inline]
pub fn ease_in_bounce(t: f32) -> f32 {
    1.0 - ease_out_bounce(1.0 - t)
}

/// Elastic ease out — overshoots then settles. Good for a "snap reveal" wipe.
#[inline]
pub fn ease_out_elastic(t: f32) -> f32 {
    let t = clamp01(t);
    if t == 0.0 || t == 1.0 { return t; }
    let c4 = std::f32::consts::TAU / 3.0;
    2.0_f32.powf(-10.0 * t) * ((t * 10.0 - 0.75) * c4).sin() + 1.0
}

// ── Frame alpha ───────────────────────────────────────────────────────────────

/// Compute the blend alpha for frame `i` of `n` total blended frames.
///
/// Returns a value in (0.0, 1.0) exclusive — the pure-A and pure-B frames are
/// encoded by `encode_clip` on each side; blend frames are strictly in-between.
///
/// ```
/// use velocut_core::transitions::helpers::frame_alpha;
/// // 4 blend frames → alphas ≈ 0.2, 0.4, 0.6, 0.8
/// assert!((frame_alpha(0, 4) - 0.2).abs() < 1e-6);
/// assert!((frame_alpha(3, 4) - 0.8).abs() < 1e-6);
/// ```
#[inline]
pub fn frame_alpha(i: usize, n: usize) -> f32 {
    (i + 1) as f32 / (n + 1) as f32
}

// ── Pixel blend ───────────────────────────────────────────────────────────────

/// Blend two gamma-encoded byte values at `alpha` ∈ [0.0, 1.0].
///
/// `alpha = 0.0` → `a`, `alpha = 1.0` → `b`.
///
/// Operates in gamma-encoded byte space — correct for SDR dissolves. For
/// linear-light accuracy, convert to f32, blend, then convert back.
#[inline]
pub fn blend_byte(a: u8, b: u8, alpha: f32) -> u8 {
    ((1.0 - alpha) * a as f32 + alpha * b as f32).round() as u8
}

// ── Packed YUV420P plane layout ───────────────────────────────────────────────
//
// Packed buffers produced by `velocut_media::helpers::yuv::extract_yuv` have
// this layout (no stride padding):
//
//   [ Y plane : w * h bytes          ]
//   [ U plane : (w/2) * (h/2) bytes  ]
//   [ V plane : (w/2) * (h/2) bytes  ]
//
// Use these helpers instead of hard-coding offsets in transition impls.

/// Byte length of the Y (luma) plane for a frame of `w × h` pixels.
#[inline]
pub fn y_len(w: u32, h: u32) -> usize {
    (w * h) as usize
}

/// Byte length of one chroma (U or V) plane for a frame of `w × h` pixels.
#[inline]
pub fn uv_len(w: u32, h: u32) -> usize {
    ((w / 2) * (h / 2)) as usize
}

/// Byte offset of the U plane within a packed YUV420P buffer.
#[inline]
pub fn u_offset(w: u32, h: u32) -> usize {
    y_len(w, h)
}

/// Byte offset of the V plane within a packed YUV420P buffer.
#[inline]
pub fn v_offset(w: u32, h: u32) -> usize {
    y_len(w, h) + uv_len(w, h)
}

/// Return (Y slice, U slice, V slice) views into a packed YUV420P buffer.
///
/// Panics in debug builds if the buffer length does not match `w × h`.
/// Use this in `apply()` impls that need to process planes separately
/// (e.g. wipes that treat luma and chroma differently).
#[inline]
pub fn split_planes(buf: &[u8], w: u32, h: u32) -> (&[u8], &[u8], &[u8]) {
    let yl = y_len(w, h);
    let cl = uv_len(w, h);
    debug_assert_eq!(
        buf.len(), yl + cl * 2,
        "split_planes: buffer length {} ≠ expected {} for {}×{}",
        buf.len(), yl + cl * 2, w, h
    );
    (&buf[..yl], &buf[yl..yl + cl], &buf[yl + cl..])
}

// ── Spatial helpers ───────────────────────────────────────────────────────────
//
// For wipes, iris transitions, and any effect that must decide per-pixel
// whether a pixel belongs to frame_a or frame_b (or a blend of both).

/// Normalized X coordinate of pixel column `x` in a frame of width `w`.
///
/// Returns a value in [0.0, 1.0] — 0.0 = left edge, 1.0 = right edge.
#[inline]
pub fn norm_x(x: u32, w: u32) -> f32 {
    (x as f32 + 0.5) / w as f32
}

/// Normalized Y coordinate of pixel row `y` in a frame of height `h`.
///
/// Returns a value in [0.0, 1.0] — 0.0 = top edge, 1.0 = bottom edge.
#[inline]
pub fn norm_y(y: u32, h: u32) -> f32 {
    (y as f32 + 0.5) / h as f32
}

/// Distance from the frame center for a pixel at normalized (`nx`, `ny`).
///
/// Returns a value in [0.0, ~0.707] for pixels within the frame — 0.0 at
/// center, ~0.707 at the furthest corner. Use for iris / circle wipes.
///
/// ```
/// use velocut_core::transitions::helpers::center_dist;
/// assert!((center_dist(0.5, 0.5) - 0.0).abs() < 1e-6);
/// ```
#[inline]
pub fn center_dist(nx: f32, ny: f32) -> f32 {
    let dx = nx - 0.5;
    let dy = ny - 0.5;
    (dx * dx + dy * dy).sqrt()
}

/// Convert a wipe edge position `edge` ∈ [0.0, 1.0] and a pixel coordinate
/// `coord` ∈ [0.0, 1.0] into a soft-edge blend alpha using a feather width.
///
/// - `coord < edge - feather/2`  → 0.0 (fully frame_a)
/// - `coord > edge + feather/2`  → 1.0 (fully frame_b)
/// - between                      → smooth linear ramp
///
/// `feather = 0.0` gives a hard binary wipe. `feather = 0.05` (5 % of frame
/// width) gives a soft anti-aliased edge that reads well on most content.
#[inline]
pub fn wipe_alpha(coord: f32, edge: f32, feather: f32) -> f32 {
    if feather <= 0.0 {
        return if coord >= edge { 1.0 } else { 0.0 };
    }
    clamp01((coord - (edge - feather * 0.5)) / feather)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_in_out_endpoints() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn ease_out_bounce_endpoints() {
        assert!((ease_out_bounce(0.0)).abs() < 1e-6);
        assert!((ease_out_bounce(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ease_out_elastic_endpoints() {
        assert_eq!(ease_out_elastic(0.0), 0.0);
        assert_eq!(ease_out_elastic(1.0), 1.0);
    }

    #[test]
    fn frame_alpha_exclusive_range() {
        let n = 5;
        for i in 0..n {
            let a = frame_alpha(i, n);
            assert!(a > 0.0 && a < 1.0, "alpha {a} out of exclusive range for i={i}");
        }
    }

    #[test]
    fn blend_byte_endpoints() {
        assert_eq!(blend_byte(0, 255, 0.0), 0);
        assert_eq!(blend_byte(0, 255, 1.0), 255);
        assert_eq!(blend_byte(100, 200, 0.5), 150);
    }

    #[test]
    fn plane_layout_1080p() {
        let (w, h) = (1920_u32, 1080_u32);
        assert_eq!(y_len(w, h),   1920 * 1080);
        assert_eq!(uv_len(w, h),  960 * 540);
        assert_eq!(u_offset(w, h), 1920 * 1080);
        assert_eq!(v_offset(w, h), 1920 * 1080 + 960 * 540);
    }

    #[test]
    fn split_planes_correct_lengths() {
        let (w, h) = (4_u32, 2_u32);
        let buf = vec![0u8; y_len(w, h) + uv_len(w, h) * 2];
        let (y, u, v) = split_planes(&buf, w, h);
        assert_eq!(y.len(), 8);
        assert_eq!(u.len(), 2);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn wipe_alpha_hard_edge() {
        assert_eq!(wipe_alpha(0.3, 0.5, 0.0), 0.0);
        assert_eq!(wipe_alpha(0.7, 0.5, 0.0), 1.0);
    }

    #[test]
    fn wipe_alpha_soft_midpoint() {
        // At the edge itself with feather, should be 0.5
        let a = wipe_alpha(0.5, 0.5, 0.1);
        assert!((a - 0.5).abs() < 1e-5);
    }

    #[test]
    fn center_dist_at_center() {
        assert!((center_dist(0.5, 0.5)).abs() < 1e-6);
    }

    #[test]
    fn norm_xy_center_pixel() {
        // A 4-wide frame: pixel 1 (0-indexed) should be near center
        assert!((norm_x(1, 4) - 0.375).abs() < 1e-6);
    }
}