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

use crate::transitions::{TransitionKind, TransitionType, VideoTransition};
use crate::transitions::helpers::{blend_byte, ease_in_out};

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
        "↔"  // U+1F32B U+FE0F — variation selector required for emoji rendering
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
        _width:  u32,
        _height: u32,
        alpha:   f32,
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
    /// wasted work.  Every byte is independent so rayon chunks across pixels
    /// with zero coordination overhead.
    fn apply_rgba(
        &self,
        frame_a: &[u8],
        frame_b: &[u8],
        _width:  u32,
        _height: u32,
        alpha:   f32,
    ) -> Vec<u8> {
        use rayon::prelude::*;

        debug_assert_eq!(frame_a.len(), frame_b.len(),
            "Crossfade::apply_rgba — frame size mismatch: {} vs {}",
            frame_a.len(), frame_b.len());

        let eased = ease_in_out(alpha);
        let mut out = vec![0u8; frame_a.len()];
        // 4-byte chunks keep whole pixels together per thread.
        out.par_chunks_mut(4)
            .zip(frame_a.par_chunks(4))
            .zip(frame_b.par_chunks(4))
            .for_each(|((o, a), b)| {
                o[0] = blend_byte(a[0], b[0], eased);
                o[1] = blend_byte(a[1], b[1], eased);
                o[2] = blend_byte(a[2], b[2], eased);
                o[3] = blend_byte(a[3], b[3], eased);
            });
        out
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