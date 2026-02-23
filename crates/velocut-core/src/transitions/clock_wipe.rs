// crates/velocut-core/src/transitions/clock_wipe.rs
//
// Clock wipe: a sweep hand rotates clockwise from 12 o'clock, revealing
// frame_b through the swept region and leaving frame_a in the unswept region.
//
// The sweep angle is driven by ease_in_out_cubic(alpha), scaled to [0, 2π].
// A feathered angular edge is applied via wipe_alpha so the sweep boundary
// softens rather than aliasing into a hard line.
//
// Per-pixel logic (Y plane, then U/V at half resolution):
//   1. Compute normalized (x, y) for the pixel.
//   2. Compute the pixel's clock angle from 12 o'clock, clockwise, ∈ [0, 2π].
//   3. Compare against current sweep angle using wipe_alpha.
//   4. Blend frame_a and frame_b at the resulting per-pixel alpha.

use crate::transitions::{TransitionKind, TransitionType, VideoTransition};
use crate::transitions::helpers::{
    alloc_frame, blend_byte, ease_in_out_cubic, norm_x, norm_y,
    split_planes, uv_len, wipe_alpha, y_len,
};

/// Angular feather width in radians — ~8° gives a crisp but anti-aliased edge.
const FEATHER: f32 = 0.14;

pub struct ClockWipe;

/// Pixel angle from 12 o'clock going clockwise, in [0, 2π].
///
/// Inputs are normalized pixel coordinates in [0, 1].
/// The center (0.5, 0.5) returns 0.0 — treat as a degenerate case (handled
/// by wipe_alpha clamping).
#[inline]
fn clock_angle(nx: f32, ny: f32) -> f32 {
    // Standard atan2(dy, dx) with screen-space y (y increases downward):
    //   12 o'clock (top):    dy < 0, dx = 0  → atan2(-ε, 0) ≈ -π/2
    //   3 o'clock (right):   dy = 0, dx > 0  → atan2(0, +)  = 0
    //   6 o'clock (bottom):  dy > 0, dx = 0  → atan2(+, 0)  = π/2
    //   9 o'clock (left):    dy = 0, dx < 0  → atan2(0, -)  = ±π
    //
    // Adding π/2 shifts so 12 o'clock → 0, then rem_euclid(2π) wraps to [0, 2π].
    let raw = (ny - 0.5_f32).atan2(nx - 0.5_f32);
    (raw + std::f32::consts::FRAC_PI_2).rem_euclid(std::f32::consts::TAU)
}

impl VideoTransition for ClockWipe {
    fn kind(&self) -> TransitionKind {
        TransitionKind::ClockWipe
    }

    fn label(&self) -> &'static str {
        "Clock Wipe"
    }

    fn icon(&self) -> &'static str {
        "◔"
    }

    fn default_duration_secs(&self) -> f32 {
        1.5
    }

    fn build(&self, duration_secs: f32) -> TransitionType {
        TransitionType::new(TransitionKind::ClockWipe, duration_secs)
    }

    fn apply(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        width:   u32,
        height:  u32,
        alpha:   f32,
    ) -> Vec<u8> {
        debug_assert_eq!(frame_a.len(), frame_b.len(),
            "ClockWipe::apply — frame size mismatch");

        // Sweep goes from 0 → 2π as alpha goes 0 → 1.
        let sweep = ease_in_out_cubic(alpha) * std::f32::consts::TAU;

        let yl  = y_len(width, height);
        let uvl = uv_len(width, height);
        let mut out = alloc_frame(width, height);

        let (ya, ua, va) = split_planes(frame_a, width, height);
        let (yb, ub, vb) = split_planes(frame_b, width, height);

        // ── Y plane (full res) ───────────────────────────────────────────────
        for py in 0..height {
            let ny = norm_y(py, height);
            for px in 0..width {
                let nx    = norm_x(px, width);
                let angle = clock_angle(nx, ny);
                // wipe_alpha(coord=sweep, edge=angle, feather):
                //   sweep > angle → pixel has been swept → a=1.0 → frame_b
                //   sweep < angle → not yet swept         → a=0.0 → frame_a
                let a  = wipe_alpha(sweep, angle, FEATHER);
                let i  = (py * width + px) as usize;
                out[i] = blend_byte(ya[i], yb[i], a);
            }
        }

        // ── Chroma planes (half res) ─────────────────────────────────────────
        let uw = width  / 2;
        let uh = height / 2;

        let mut write_chroma = |src_a: &[u8], src_b: &[u8], off: usize| {
            for py in 0..uh {
                let ny = norm_y(py, uh);
                for px in 0..uw {
                    let nx    = norm_x(px, uw);
                    let angle = clock_angle(nx, ny);
                    let a     = wipe_alpha(sweep, angle, FEATHER);
                    let i     = off + (py * uw + px) as usize;
                    out[i]    = blend_byte(src_a[(py * uw + px) as usize],
                                          src_b[(py * uw + px) as usize], a);
                }
            }
        };

        write_chroma(ua, ub, yl);
        write_chroma(va, vb, yl + uvl);

        out
    }

    /// Direct RGBA clock wipe — same angular sweep geometry as `apply`, no YUV round-trip.
    /// Rayon parallelises over rows via `enumerate` so each row knows its `py`.
    fn apply_rgba(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        width:   u32,
        height:  u32,
        alpha:   f32,
    ) -> Vec<u8> {
        use rayon::prelude::*;
        debug_assert_eq!(frame_a.len(), frame_b.len(),
            "ClockWipe::apply_rgba — frame size mismatch");
        let sweep     = ease_in_out_cubic(alpha) * std::f32::consts::TAU;
        let row_bytes = (width * 4) as usize;
        let mut out   = vec![0u8; frame_a.len()];
        out.par_chunks_mut(row_bytes)
            .zip(frame_a.par_chunks(row_bytes))
            .zip(frame_b.par_chunks(row_bytes))
            .enumerate()
            .for_each(|(py, ((o_row, a_row), b_row))| {
                let ny = norm_y(py as u32, height);
                for px in 0..width as usize {
                    let nx    = norm_x(px as u32, width);
                    let angle = clock_angle(nx, ny);
                    // Same convention as apply: blend_byte(ya, yb, a) → a=0→frame_a, a=1→frame_b
                    let a     = wipe_alpha(sweep, angle, FEATHER);
                    let base  = px * 4;
                    o_row[base]     = blend_byte(a_row[base],     b_row[base],     a);
                    o_row[base + 1] = blend_byte(a_row[base + 1], b_row[base + 1], a);
                    o_row[base + 2] = blend_byte(a_row[base + 2], b_row[base + 2], a);
                    o_row[base + 3] = 255;
                }
            });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(y: u8, uv: u8, w: u32, h: u32) -> Vec<u8> {
        let yl = y_len(w, h); let uvl = uv_len(w, h);
        let mut b = vec![0u8; yl + uvl * 2];
        b[..yl].fill(y); b[yl..yl+uvl].fill(uv); b[yl+uvl..].fill(uv); b
    }

    #[test]
    fn output_length_matches_input() {
        let (w, h) = (8, 8);
        let a = packed(100, 128, w, h);
        let b = packed(200, 64, w, h);
        assert_eq!(ClockWipe.apply(&a, &b, w, h, 0.5).len(), a.len());
    }

    #[test]
    fn alpha_zero_returns_frame_a() {
        let (w, h) = (8, 8);
        let a = packed(100, 128, w, h);
        let b = packed(200, 64, w, h);
        let result = ClockWipe.apply(&a, &b, w, h, 0.0);
        // sweep=0 → nothing swept → all frame_a
        assert!(result[..y_len(w,h)].iter().all(|&v| v == 100));
    }

    #[test]
    fn alpha_one_returns_frame_b() {
        let (w, h) = (8, 8);
        let a = packed(100, 128, w, h);
        let b = packed(200, 64, w, h);
        let result = ClockWipe.apply(&a, &b, w, h, 1.0);
        // sweep=2π → full revolution → all frame_b (allow 1-value feather tolerance)
        assert!(result[..y_len(w,h)].iter().all(|&v| v >= 198));
    }

    #[test]
    fn clock_angle_12_oclock() {
        // Top-center pixel (nx=0.5, ny slightly above center) → ~0.0
        let angle = clock_angle(0.5, 0.1);
        assert!(angle < 0.2, "12 o'clock should be near 0, got {angle}");
    }

    #[test]
    fn clock_angle_3_oclock() {
        // Right-center pixel → ~π/2
        let angle = clock_angle(0.9, 0.5);
        let expected = std::f32::consts::FRAC_PI_2;
        assert!((angle - expected).abs() < 0.2,
            "3 o'clock should be near π/2, got {angle}");
    }

    #[test]
    fn sweep_reveals_top_before_bottom() {
        let (w, h) = (16, 16);
        let a = packed(0, 128, w, h);
        let b = packed(200, 128, w, h);
        // At alpha=0.25, only the top-right quadrant should be mostly revealed
        let result = ClockWipe.apply(&a, &b, w, h, 0.25);
        let top_right = result[(h / 4 * w + w * 3 / 4) as usize];
        let bottom_left = result[(h * 3 / 4 * w + w / 4) as usize];
        assert!(top_right > bottom_left,
            "top-right ({top_right}) should be more revealed than bottom-left ({bottom_left})");
    }
}