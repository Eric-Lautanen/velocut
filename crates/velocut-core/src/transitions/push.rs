// crates/velocut-core/src/transitions/push.rs
//
// Horizontal push transition.
//
// frame_b slides in from the right while frame_a is pushed off to the left.
// At any alpha the two clips fill the screen exactly — no black bars, no
// blending.  The only operation is pixel displacement.
//
// Spatial layout at a given eased alpha `p` ∈ [0, 1]:
//
//   Screen column x → source:
//     if x <  width * (1−p)  →  frame_a at source_x = x + width * p
//     if x >= width * (1−p)  →  frame_b at source_x = x − width * (1−p)
//
//   Verification:
//     p=0: boundary=width (right edge) → every pixel from frame_a at src=x ✓
//     p=1: boundary=0    (left edge)   → every pixel from frame_b at src=x ✓
//     p=½: left half shows right half of frame_a, right half shows left half of frame_b ✓
//
// Easing (`ease_in_out_cubic`) is applied to alpha before computing the
// boundary so the motion reads like a physical object with momentum rather
// than a mechanical linear slide.
//
// UV planes use the same displacement logic at half-resolution: dividing
// pixel coordinates by 2 (integer) gives the correct chroma sample.

use crate::transitions::{TransitionKind, TransitionType, VideoTransition};
use crate::transitions::helpers::{ease_in_out_cubic, split_planes};

/// Horizontal push: frame_b enters from the right, frame_a exits left.
pub struct Push;

impl VideoTransition for Push {
    fn kind(&self) -> TransitionKind {
        TransitionKind::Push
    }

    fn label(&self) -> &'static str {
        "Push"
    }

    fn icon(&self) -> &'static str {
        "➡️"
    }

    fn default_duration_secs(&self) -> f32 {
        2.0
    }

    fn build(&self, duration_secs: f32) -> TransitionType {
        TransitionType::new(TransitionKind::Push, duration_secs)
    }

    /// Displace pixels from frame_a and frame_b to produce the push frame.
    ///
    /// No alpha blending — every output pixel is copied verbatim from exactly
    /// one source frame.  This keeps the clip content crisp and avoids the
    /// "double-image" ghosting of a crossfade during motion.
    fn apply(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        width:   u32,
        height:  u32,
        alpha:   f32,
    ) -> Vec<u8> {
        debug_assert_eq!(
            frame_a.len(), frame_b.len(),
            "Push::apply — frame size mismatch: {} vs {}",
            frame_a.len(), frame_b.len(),
        );

        // Cubic ease for a momentum-based feel.
        let p = ease_in_out_cubic(alpha);

        // The screen x-column at which frame_b's left edge sits.
        // Everything to the left of this column belongs to frame_a (shifted).
        let boundary_f = (1.0 - p) * width as f32;
        let boundary   = boundary_f.round() as u32;

        let (ay, au, av) = split_planes(frame_a, width, height);
        let (by, bu, bv) = split_planes(frame_b, width, height);

        let mut out = Vec::with_capacity(frame_a.len());

        // ── Y plane (full resolution) ────────────────────────────────────────
        // frame_a offset: how many columns frame_a has shifted to the left.
        // A pixel at screen column x samples frame_a at x + shift_a.
        let shift_a = (p * width as f32).round() as u32;

        for py in 0..height {
            for px in 0..width {
                let idx = (py * width + px) as usize;
                if px < boundary {
                    // frame_a region — sample shifted right within frame_a
                    let src_x  = (px + shift_a).min(width - 1);
                    let src_idx = (py * width + src_x) as usize;
                    out.push(ay[src_idx]);
                } else {
                    // frame_b region — sample from frame_b's left edge
                    let src_x   = px - boundary;
                    let src_idx = (py * width + src_x) as usize;
                    out.push(by[src_idx]);
                }
                let _ = idx; // suppress warning; idx unused (direct push)
            }
        }

        // ── Chroma planes (half resolution each dimension) ───────────────────
        // Scale boundary and shift to chroma resolution by halving.
        let uw         = width  / 2;
        let uh         = height / 2;
        let c_boundary = (boundary_f * 0.5).round() as u32;
        let c_shift_a  = ((p * width as f32) * 0.5).round() as u32;

        let process_chroma = |src_a: &[u8], src_b: &[u8], out: &mut Vec<u8>| {
            for py in 0..uh {
                for px in 0..uw {
                    if px < c_boundary {
                        let src_x   = (px + c_shift_a).min(uw - 1);
                        let src_idx = (py * uw + src_x) as usize;
                        out.push(src_a[src_idx]);
                    } else {
                        let src_x   = px - c_boundary;
                        let src_idx = (py * uw + src_x) as usize;
                        out.push(src_b[src_idx]);
                    }
                }
            }
        };

        process_chroma(au, bu, &mut out);
        process_chroma(av, bv, &mut out);

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transitions::helpers::{y_len, uv_len};

    fn yuv_frame(y_val: u8, uv_val: u8, w: u32, h: u32) -> Vec<u8> {
        let mut buf = vec![y_val; y_len(w, h)];
        buf.extend(vec![uv_val; uv_len(w, h) * 2]);
        buf
    }

    /// Build a Y-gradient frame: column x gets value `x as u8` (repeated each row).
    /// Useful for verifying that the correct source column was sampled.
    fn gradient_frame(w: u32, h: u32) -> Vec<u8> {
        let yl = y_len(w, h);
        let cl = uv_len(w, h);
        let mut buf = Vec::with_capacity(yl + cl * 2);
        for _py in 0..h {
            for px in 0..w {
                buf.push(px as u8);
            }
        }
        buf.extend(vec![128u8; cl * 2]); // neutral chroma
        buf
    }

    #[test]
    fn push_alpha_zero_returns_frame_a() {
        let t = Push;
        let (w, h) = (8_u32, 4_u32);
        let a = yuv_frame(200, 100, w, h);
        let b = yuv_frame(50,  128, w, h);
        let out = t.apply(&a, &b, w, h, 0.0);
        // p=0 → boundary=width → all pixels from frame_a at src_x=x+0 → 200
        assert!(
            out[..y_len(w, h)].iter().all(|&v| v == 200),
            "all Y should be frame_a at alpha=0"
        );
    }

    #[test]
    fn push_alpha_one_returns_frame_b() {
        let t = Push;
        let (w, h) = (8_u32, 4_u32);
        let a = yuv_frame(200, 100, w, h);
        let b = yuv_frame(50,  128, w, h);
        let out = t.apply(&a, &b, w, h, 1.0);
        // p=1 → boundary=0 → all pixels from frame_b at src_x=x−0 → 50
        assert!(
            out[..y_len(w, h)].iter().all(|&v| v == 50),
            "all Y should be frame_b at alpha=1"
        );
    }

    #[test]
    fn push_output_length_matches_input() {
        let t = Push;
        let (w, h) = (8_u32, 4_u32);
        let a = yuv_frame(100, 128, w, h);
        let b = yuv_frame(200, 128, w, h);
        for alpha in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let out = t.apply(&a, &b, w, h, alpha);
            assert_eq!(out.len(), a.len(), "length mismatch at alpha={alpha}");
        }
    }

    #[test]
    fn push_no_black_bars_at_midpoint() {
        let t = Push;
        let (w, h) = (8_u32, 4_u32);
        let a = yuv_frame(200, 100, w, h);
        let b = yuv_frame(50,  128, w, h);
        let out = t.apply(&a, &b, w, h, 0.5);
        // No pixel should be 0 (black) — both clips fill the screen.
        assert!(
            out[..y_len(w, h)].iter().all(|&v| v == 200 || v == 50),
            "no black pixels expected at midpoint — got unexpected values"
        );
    }

    #[test]
    fn push_sources_correct_columns() {
        // Use gradient frames so we can verify which column was sampled.
        // frame_a: col x → value x (0..7)
        // frame_b: col x → value x + 100 (to distinguish from frame_a)
        let t = Push;
        let (w, h) = (8_u32, 2_u32);

        let a = gradient_frame(w, h); // Y values: 0,1,2,3,4,5,6,7 per row
        // Build frame_b manually: col x → x+100
        let yl = y_len(w, h);
        let cl = uv_len(w, h);
        let mut b = Vec::with_capacity(yl + cl * 2);
        for _py in 0..h {
            for px in 0..w {
                b.push((px + 100) as u8);
            }
        }
        b.extend(vec![128u8; cl * 2]);

        // alpha chosen so ease_in_out_cubic gives p ≈ 0.5 (boundary = 4).
        // Exact midpoint of cubic ease is at alpha=0.5 → p = ease_in_out_cubic(0.5) = 0.5.
        let out = t.apply(&a, &b, w, h, 0.5);

        // p=0.5 → boundary=4, shift_a=4
        // Screen cols 0-3: frame_a at src_x = col+4 → values 4,5,6,7
        assert_eq!(out[0], 4, "col 0 should sample frame_a at src 4");
        assert_eq!(out[3], 7, "col 3 should sample frame_a at src 7");
        // Screen cols 4-7: frame_b at src_x = col-4 → values 100,101,102,103
        assert_eq!(out[4], 100, "col 4 should sample frame_b at src 0");
        assert_eq!(out[7], 103, "col 7 should sample frame_b at src 3");
    }
}