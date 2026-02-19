// crates/velocut-core/src/helpers/geometry.rs
//
// Aspect-ratio utilities shared between velocut-ui modules.
//
// Previously: aspect_ratio_value() and aspect_ratio_label() lived as private
// free functions in velocut-ui/src/modules/export_module.rs.
//
// Moving them here means video_module.rs can compute the preview rect without
// importing UI internals, and any future crate (e.g. a headless renderer) gets
// them for free.

use crate::state::AspectRatio;

/// Numeric width-to-height ratio for the given `AspectRatio` variant.
///
/// Used when sizing the preview rect and when passing a ratio to the media
/// worker's `request_frame` / `start_playback` calls.
///
/// ```
/// use velocut_core::state::AspectRatio;
/// use velocut_core::helpers::geometry::aspect_ratio_value;
/// let r = aspect_ratio_value(AspectRatio::SixteenNine);
/// assert!((r - 16.0 / 9.0).abs() < 1e-6);
/// ```
pub fn aspect_ratio_value(ar: AspectRatio) -> f32 {
    match ar {
        AspectRatio::SixteenNine   => 16.0 / 9.0,
        AspectRatio::NineSixteen   => 9.0  / 16.0,
        AspectRatio::TwoThree      => 2.0  / 3.0,
        AspectRatio::ThreeTwo      => 3.0  / 2.0,
        AspectRatio::FourThree     => 4.0  / 3.0,
        AspectRatio::OneOne        => 1.0,
        AspectRatio::FourFive      => 4.0  / 5.0,
        AspectRatio::TwentyOneNine => 21.0 / 9.0,
        AspectRatio::Anamorphic    => 2.39,
    }
}

/// Short human-readable label for the given `AspectRatio` variant.
///
/// Shown in the export panel ComboBox and in the "Match Project" button hint.
pub fn aspect_ratio_label(ar: AspectRatio) -> &'static str {
    match ar {
        AspectRatio::SixteenNine   => "16:9  — Landscape / YouTube",
        AspectRatio::NineSixteen   => "9:16  — Portrait / Reels / Shorts",
        AspectRatio::FourThree     => "4:3   — Classic TV",
        AspectRatio::ThreeTwo      => "3:2   — Landscape photo",
        AspectRatio::TwoThree      => "2:3   — Portrait photo",
        AspectRatio::OneOne        => "1:1   — Square",
        AspectRatio::FourFive      => "4:5   — Instagram portrait",
        AspectRatio::TwentyOneNine => "21:9  — Ultrawide / Cinema",
        AspectRatio::Anamorphic    => "2.39  — Anamorphic widescreen",
    }
}