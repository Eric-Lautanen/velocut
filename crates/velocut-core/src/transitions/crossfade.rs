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
        "🌫️"  // U+1F32B U+FE0F — variation selector required for emoji rendering
    }

    fn default_duration_secs(&self) -> f32 {
        0.5
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
        debug_assert_eq!(
            frame_a.len(),
            frame_b.len(),
            "Crossfade::apply — frame size mismatch: {} vs {}",
            frame_a.len(),
            frame_b.len(),
        );

        let eased = ease_in_out(alpha);

        frame_a.iter()
            .zip(frame_b.iter())
            .map(|(&a, &b)| blend_byte(a, b, eased))
            .collect()
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