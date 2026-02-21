// crates/velocut-core/src/transitions/helpers.rs
//
// Math utilities for transition implementors.
//
// All functions operate on plain f32 — no FFmpeg, no egui, no buffer math.
// Buffer math lives in `velocut_media::helpers::yuv` (stride-aware extract/write)
// and in each transition's own `apply()` impl.
//
// Import in a transition impl:
//   use crate::transitions::helpers::{ease_in_out, clamp01};

// ── Alpha / progress helpers ──────────────────────────────────────────────────

/// Clamp a value to [0.0, 1.0].
#[inline]
pub fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Linear interpolation between `a` and `b` at position `t` ∈ [0, 1].
#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ── Easing curves ─────────────────────────────────────────────────────────────
//
// All easing functions take `t` ∈ [0.0, 1.0] and return a remapped value in
// the same range. Pass the result as `alpha` to `VideoTransition::apply()`.
//
// Visualize these at https://easings.net — the function names match.

/// Smooth step — cubic ease in/out. Good default for dissolves.
///
/// `t=0` → 0.0, `t=0.5` → 0.5, `t=1` → 1.0, with zero derivative at endpoints.
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

/// Ease out — decelerates into the end.
#[inline]
pub fn ease_out(t: f32) -> f32 {
    let t = clamp01(t);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Cubic ease in/out — stronger than smooth step, more cinematic feel.
#[inline]
pub fn ease_in_out_cubic(t: f32) -> f32 {
    let t = clamp01(t);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// No easing — pass alpha through unchanged. Useful for wipes and hard shapes
/// where the edge should be crisp rather than blended.
#[inline]
pub fn linear(t: f32) -> f32 {
    clamp01(t)
}

// ── Frame alpha computation ────────────────────────────────────────────────────

/// Compute the blend alpha for frame `i` of `n` total blended frames.
///
/// Returns a value in (0.0, 1.0) exclusive — the pure-A and pure-B frames
/// are encoded by `encode_clip` on each side; blend frames are the in-between.
///
/// ```
/// use velocut_core::transitions::helpers::frame_alpha;
/// // 4 blend frames: alphas ≈ 0.2, 0.4, 0.6, 0.8
/// assert!((frame_alpha(0, 4) - 0.2).abs() < 1e-6);
/// assert!((frame_alpha(3, 4) - 0.8).abs() < 1e-6);
/// ```
#[inline]
pub fn frame_alpha(i: usize, n: usize) -> f32 {
    (i + 1) as f32 / (n + 1) as f32
}

// ── Pixel blend ───────────────────────────────────────────────────────────────

/// Blend two byte values at alpha in [0.0, 1.0].
///
/// Performs linear interpolation in gamma-encoded byte space — a correct
/// approximation for SDR dissolves. For linear-light blending, convert to f32
/// first, blend, then convert back.
#[inline]
pub fn blend_byte(a: u8, b: u8, alpha: f32) -> u8 {
    ((1.0 - alpha) * a as f32 + alpha * b as f32).round() as u8
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
    fn frame_alpha_range() {
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
}