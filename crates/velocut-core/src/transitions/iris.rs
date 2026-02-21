// crates/velocut-core/src/transitions/iris.rs
//
// Iris wipe: a circle expands from the center of the frame, revealing the
// incoming clip through the growing aperture.
//
// The circle radius is driven by the eased alpha, scaled so that at alpha=0
// the iris is fully closed (radius 0) and at alpha=1 it covers the furthest
// corner of the frame (~0.707 in normalized coords). A soft feathered edge
// is applied via `wipe_alpha` to avoid a harsh aliased ring.
//
// Per-pixel logic (Y plane, then repeated for U/V at half resolution):
//   1. Compute normalized (x, y) coords for the pixel.
//   2. Compute distance from frame center via `center_dist`.
//   3. Compare against the current iris radius with a feather band.
//   4. Blend frame_a and frame_b at the resulting per-pixel alpha.
//
// UV plane pixels are processed at (w/2 × h/2) — norm_x/norm_y are called
// with the chroma dims so the circle stays geometrically correct.

use crate::transitions::{TransitionKind, TransitionType, VideoTransition};
use crate::transitions::helpers::{
    blend_byte, center_dist, ease_in_out_cubic, norm_x, norm_y,
    split_planes, uv_len, wipe_alpha, y_len,
};

/// Feather width as a fraction of the frame — 4 % gives a smooth anti-aliased
/// edge that reads well on most content without going mushy.
const FEATHER: f32 = 0.04;

/// Maximum iris radius: distance from center to the furthest corner ≈ 0.707.
/// Adding a small margin ensures the iris is fully open at alpha=1.
const MAX_RADIUS: f32 = 0.75;

pub struct Iris;

impl VideoTransition for Iris {
    fn kind(&self) -> TransitionKind {
        TransitionKind::Iris
    }

    fn label(&self) -> &'static str {
        "Iris"
    }

    fn icon(&self) -> &'static str {
        "⭕️"  // U+2B55 U+FE0F — variation selector required
    }

    fn default_duration_secs(&self) -> f32 {
        0.7
    }

    fn build(&self, duration_secs: f32) -> TransitionType {
        TransitionType::new(TransitionKind::Iris, duration_secs)
    }

    /// Blend frame_a and frame_b using an expanding circular iris.
    ///
    /// Pixels inside the iris circle show frame_b; pixels outside show frame_a.
    /// The feathered edge blends both near the boundary.
    fn apply(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        width:   u32,
        height:  u32,
        alpha:   f32,
    ) -> Vec<u8> {
        debug_assert_eq!(
            frame_a.len(),
            frame_b.len(),
            "Iris::apply — frame size mismatch: {} vs {}",
            frame_a.len(),
            frame_b.len(),
        );

        // Ease the radius so the iris accelerates open rather than expanding
        // at a constant rate, which tends to feel mechanical.
        let radius = ease_in_out_cubic(alpha) * MAX_RADIUS;

        let yl  = y_len(width, height);
        let uvl = uv_len(width, height);
        let mut out = vec![0u8; yl + uvl * 2];

        // ── Y plane ───────────────────────────────────────────────────────────
        let (ya, ua, va) = split_planes(frame_a, width, height);
        let (yb, ub, vb) = split_planes(frame_b, width, height);

        for py in 0..height {
            let ny = norm_y(py, height);
            for px in 0..width {
                let nx   = norm_x(px, width);
                let dist = center_dist(nx, ny);
                // wipe_alpha(coord=radius, edge=dist): when dist < radius → coord > edge → 1.0 (frame_b)
                //                                      when dist > radius → coord < edge → 0.0 (frame_a)
                let a = wipe_alpha(radius, dist, FEATHER);
                let i = (py * width + px) as usize;
                out[i] = blend_byte(ya[i], yb[i], a);
            }
        }

        // ── U plane ───────────────────────────────────────────────────────────
        let uw = width  / 2;
        let uh = height / 2;
        for py in 0..uh {
            let ny = norm_y(py, uh);
            for px in 0..uw {
                let nx   = norm_x(px, uw);
                let dist = center_dist(nx, ny);
                let a    = wipe_alpha(radius, dist, FEATHER);
                let i    = (py * uw + px) as usize;
                out[yl + i] = blend_byte(ua[i], ub[i], a);
            }
        }

        // ── V plane ───────────────────────────────────────────────────────────
        for py in 0..uh {
            let ny = norm_y(py, uh);
            for px in 0..uw {
                let nx   = norm_x(px, uw);
                let dist = center_dist(nx, ny);
                let a    = wipe_alpha(radius, dist, FEATHER);
                let i    = (py * uw + px) as usize;
                out[yl + uvl + i] = blend_byte(va[i], vb[i], a);
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed_frame(y: u8, uv: u8, w: u32, h: u32) -> Vec<u8> {
        let yl  = y_len(w, h);
        let uvl = uv_len(w, h);
        let mut buf = vec![0u8; yl + uvl * 2];
        buf[..yl].fill(y);
        buf[yl..yl + uvl].fill(uv);
        buf[yl + uvl..].fill(uv);
        buf
    }

    #[test]
    fn iris_alpha_zero_returns_frame_a() {
        let iris = Iris;
        let (w, h) = (8, 4);
        let a = packed_frame(100, 128, w, h);
        let b = packed_frame(200, 64,  w, h);
        let result = iris.apply(&a, &b, w, h, 0.0);
        // radius = 0 → every pixel is outside the iris → all frame_a
        let yl = y_len(w, h);
        assert!(result[..yl].iter().all(|&v| v == 100),
            "Y plane should be frame_a at alpha=0");
    }

    #[test]
    fn iris_alpha_one_returns_frame_b() {
        let iris = Iris;
        let (w, h) = (8, 4);
        let a = packed_frame(100, 128, w, h);
        let b = packed_frame(200, 64,  w, h);
        let result = iris.apply(&a, &b, w, h, 1.0);
        // radius = MAX_RADIUS → every pixel is inside the iris → all frame_b
        let yl = y_len(w, h);
        // Allow a 1-value rounding tolerance at feather boundary pixels
        assert!(result[..yl].iter().all(|&v| v >= 198),
            "Y plane should be frame_b at alpha=1, got min={}", result[..yl].iter().min().unwrap());
    }

    #[test]
    fn iris_output_length_matches_input() {
        let iris = Iris;
        let (w, h) = (8, 4);
        let a = packed_frame(50, 128, w, h);
        let b = packed_frame(150, 128, w, h);
        let result = iris.apply(&a, &b, w, h, 0.5);
        assert_eq!(result.len(), a.len());
    }

    #[test]
    fn iris_center_pixel_leads_edge_pixels_at_midpoint() {
        let iris = Iris;
        let (w, h) = (8, 8);
        let a = packed_frame(0, 128, w, h);
        let b = packed_frame(200, 128, w, h);
        let result = iris.apply(&a, &b, w, h, 0.4);
        // Center pixel (4,4) should be more frame_b than corner pixel (0,0)
        let center = result[(h / 2 * w + w / 2) as usize];
        let corner = result[0];
        assert!(center > corner,
            "center ({center}) should be brighter than corner ({corner}) at mid-iris");
    }
}