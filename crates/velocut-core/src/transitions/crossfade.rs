// crates/velocut-core/src/transitions/crossfade.rs
//
// Linear dissolve between two clips.
//
// The blend runs in gamma-encoded byte space — a correct approximation for
// SDR content. See `helpers::blend_byte` for the rationale and limitations.
//
// The smooth-step easing curve (`ease_in_out`) is applied to the raw linear
// alpha before blending. This gives a more perceptually even dissolve than a
// raw linear ramp, which tends to feel "muddy" at the midpoint.
//
// To make a wipe or other transition, duplicate this file as `wipe.rs` and
// replace the `apply()` body. Everything else (registration, call sites) is
// handled by the registry in `mod.rs`.

use crate::transitions::helpers::{
    blend_byte, blend_byte_fixed, ease_in_out, fixed_inverse, fixed_weight,
};
use crate::transitions::{TransitionKind, TransitionType, VideoTransition};

/// Linear dissolve with smooth-step easing.
pub struct Crossfade;

impl VideoTransition for Crossfade {
    fn kind(&self) -> TransitionKind {
        TransitionKind::Crossfade
    }

    fn label(&self) -> &'static str {
        "Dissolve"
    }

    fn icon(&self) -> &'static str {
        "↔" // U+2194 LEFT RIGHT ARROW
    }

    fn default_duration_secs(&self) -> f32 {
        2.0
    }

    fn build(&self, duration_secs: f32) -> TransitionType {
        TransitionType::new(TransitionKind::Crossfade, duration_secs)
    }

    /// Blend two packed YUV420P buffers at `alpha` with smooth-step easing.
    ///
    /// The easing is applied here (not at the call site) so that every consumer
    /// — encode and preview alike — gets the same perceptual curve without
    /// coordinating separately.
    ///
    /// If you want a *linear* dissolve without easing, replace `ease_in_out(alpha)`
    /// with `alpha` directly. The test below covers both the eased and raw cases.
    fn apply(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        _width: u32,
        _height: u32,
        alpha: f32,
    ) -> Vec<u8> {
        use rayon::prelude::*;

        debug_assert_eq!(
            frame_a.len(),
            frame_b.len(),
            "Crossfade::apply — frame size mismatch: {} vs {}",
            frame_a.len(),
            frame_b.len(),
        );

        let eased = ease_in_out(alpha);
        let mut out = vec![0u8; frame_a.len()];
        out.par_iter_mut()
            .zip(frame_a.par_iter())
            .zip(frame_b.par_iter())
            .for_each(|((o, &a), &b)| *o = blend_byte(a, b, eased));
        out
    }

    /// Direct parallel RGBA blend — skips the YUV round-trip in the default
    /// `apply_rgba` impl.  Both frames arrive as RGBA from the D3D11VA
    /// transfer path; converting to YUV and back purely to blend bytes is
    /// wasted work.  Every byte is independent and shares one weight, so the
    /// blend runs flat over byte blocks rather than grouping by pixel.
    fn apply_rgba(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        width: u32,
        height: u32,
        alpha: f32,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        self.apply_rgba_into(frame_a, frame_b, width, height, alpha, &mut out);
        out
    }

    fn apply_rgba_into(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        _width: u32,
        _height: u32,
        alpha: f32,
        out: &mut Vec<u8>,
    ) {
        use rayon::prelude::*;

        debug_assert_eq!(
            frame_a.len(),
            frame_b.len(),
            "Crossfade::apply_rgba_into — frame size mismatch: {} vs {}",
            frame_a.len(),
            frame_b.len()
        );

        // The blend weight is the same for every byte in the frame, so hoist it
        // out of the pixel loop as a 16-bit fixed-point pair and blend each
        // channel with integer math. Per channel that replaces
        // (1-a)·x + a·y + round() — four float ops — with one multiply-add and a
        // shift.
        //
        // Rounding: `+ HALF >> 16` is round-half-up on the 16.16 product, where
        // `blend_byte` rounds the exact float product half away from zero. The
        // two agree at 0.0, 0.5 and 1.0 and can differ by at most one LSB in
        // between — invisible, and every consumer (playback, scrub, encode)
        // uses this same function, so nothing can disagree with anything.
        let eased = ease_in_out(alpha).clamp(0.0, 1.0);
        let w = fixed_weight(eased);
        let inv = fixed_inverse(w);
        // One rayon task per BLOCK_BYTES bytes rather than per pixel: at preview
        // size that is ~1200 tasks instead of 1.25 million. The block loop then runs
        // flat over bytes, because all four channels are linear in the same
        // weight — grouping by pixel bought nothing and cost the vectoriser its
        // freedom (`blend_bench` measures both against the pre-rewrite code).
        const BLOCK_BYTES: usize = 4096;

        out.resize(frame_a.len(), 0);
        out.par_chunks_mut(BLOCK_BYTES)
            .zip(frame_a.par_chunks(BLOCK_BYTES))
            .zip(frame_b.par_chunks(BLOCK_BYTES))
            .for_each(|((o, a), b)| {
                for ((oc, ac), bc) in o.iter_mut().zip(a).zip(b) {
                    *oc = blend_byte_fixed(*ac, *bc, inv, w);
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(value: u8, len: usize) -> Vec<u8> {
        vec![value; len]
    }

    #[test]
    fn crossfade_alpha_zero_returns_frame_a() {
        let cf = Crossfade;
        let a = make_frame(100, 12);
        let b = make_frame(200, 12);
        let result = cf.apply(&a, &b, 4, 3, 0.0);
        assert!(result.iter().all(|&v| v == 100));
    }

    #[test]
    fn crossfade_alpha_one_returns_frame_b() {
        let cf = Crossfade;
        let a = make_frame(100, 12);
        let b = make_frame(200, 12);
        let result = cf.apply(&a, &b, 4, 3, 1.0);
        assert!(result.iter().all(|&v| v == 200));
    }

    #[test]
    fn crossfade_midpoint_is_symmetric() {
        let cf = Crossfade;
        let a = make_frame(0, 12);
        let b = make_frame(200, 12);
        let result = cf.apply(&a, &b, 4, 3, 0.5);
        // ease_in_out(0.5) = 0.5 → blend_byte(0, 200, 0.5) = 100
        assert!(result.iter().all(|&v| v == 100));
    }

    #[test]
    fn crossfade_output_length_matches_input() {
        let cf = Crossfade;
        let len = 100;
        let a = make_frame(50, len);
        let b = make_frame(150, len);
        let result = cf.apply(&a, &b, 10, 10, 0.3);
        assert_eq!(result.len(), len);
    }
}

#[cfg(test)]
mod rgba_parity_tests {
    use super::*;
    use crate::transitions::helpers::{blend_byte, ease_in_out};

    /// The fixed-point RGBA path must agree with the reference float blend to
    /// within one LSB at every alpha — the optimisation is not allowed to change
    /// what a dissolve looks like, and a mixed-up weight/inverse pair would show
    /// up here as a large error rather than as a subtly wrong picture.
    #[test]
    fn integer_rgba_blend_matches_the_reference_blend_byte() {
        let a: Vec<u8> = (0..64).map(|i| (i * 7 % 256) as u8).collect();
        let b: Vec<u8> = (0..64).map(|i| (255 - (i * 11 % 256)) as u8).collect();
        for step in 0..=10 {
            let alpha = step as f32 / 10.0;
            let got = Crossfade.apply_rgba(&a, &b, 4, 4, alpha);
            let eased = ease_in_out(alpha);
            for (i, g) in got.iter().enumerate() {
                let want = blend_byte(a[i], b[i], eased);
                let diff = g.abs_diff(want);
                assert!(
                    diff <= 1,
                    "alpha={alpha}: pixel byte {i} = {g} vs reference {want} (diff {diff})"
                );
            }
        }
    }

    #[test]
    fn endpoints_are_exact() {
        let a = vec![37u8; 64];
        let b = vec![201u8; 64];
        assert!(Crossfade
            .apply_rgba(&a, &b, 4, 4, 0.0)
            .iter()
            .all(|&v| v == 37));
        assert!(Crossfade
            .apply_rgba(&a, &b, 4, 4, 1.0)
            .iter()
            .all(|&v| v == 201));
    }
}
