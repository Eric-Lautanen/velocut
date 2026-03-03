// crates/velocut-core/src/filters/mod.rs
//
// Per-clip color filter system — mirrors transitions/mod.rs in structure.
//
// A `FilterParams` value lives on every `TimelineClip`.  When all fields are
// at their defaults (`FilterParams::none()`) `is_identity()` returns true and
// both the encode and scrub paths skip processing entirely — zero cost for
// clips with no filter applied.
//
// ## Adding a preset
// 1. Add a variant to `declare_filters!` below.
// 2. Add its parameter values to `FilterParams::from_preset()`.
// That's it — no other files need changing.

use serde::{Deserialize, Serialize};

// ── Preset registry macro ─────────────────────────────────────────────────────

/// Generates the `FilterKind` enum and the `presets()` list from a compact
/// declaration.  Mirrors the `declare_transitions!` pattern so adding a preset
/// is a one-liner.
///
/// Usage:
/// ```
/// declare_filters! {
///     None,
///     Cinematic,
///     Vintage,
///     ...
/// }
/// ```
macro_rules! declare_filters {
    ( $( $variant:ident ),* $(,)? ) => {
        /// Identifies a named filter preset.
        /// `None` = identity (no processing).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum FilterKind {
            $( $variant, )*
        }

        impl Default for FilterKind {
            fn default() -> Self { FilterKind::None }
        }

        impl FilterKind {
            /// Human-readable label shown in the UI preset grid.
            pub fn label(self) -> &'static str {
                match self {
                    $( FilterKind::$variant => stringify!($variant), )*
                }
            }

            /// All variants in declaration order — used to render the preset grid.
            pub fn all() -> &'static [FilterKind] {
                &[ $( FilterKind::$variant, )* ]
            }
        }
    };
}

declare_filters! {
    None,
    Cinematic,
    Vintage,
    Cool,
    Vivid,
    BlackAndWhite,
    Faded,
    GoldenHour,
    NightBlue,
    Punchy,
}

// ── FilterParams ──────────────────────────────────────────────────────────────

/// All color-correction parameters for one timeline clip.
///
/// Stored on `TimelineClip` (like `volume`).  Serde-serialized into project
/// files.  `FilterParams::none()` / `is_identity()` allow the encode and scrub
/// paths to skip processing with zero overhead when no filter is applied.
///
/// ### Parameter ranges (enforced by the UI sliders, not clamped here)
/// | Field        | Default | Range       | Notes                            |
/// |--------------|---------|-------------|----------------------------------|
/// | brightness   | 0.0     | -1.0 .. 1.0 | additive luma shift              |
/// | contrast     | 1.0     | 0.0 .. 3.0  | multiplicative around mid-gray   |
/// | saturation   | 1.0     | 0.0 .. 3.0  | 0 = greyscale, 1 = unchanged     |
/// | gamma        | 1.0     | 0.1 .. 4.0  | power curve on luma              |
/// | hue          | 0.0     | -180 .. 180 | degrees, wraps                   |
/// | temperature  | 0.0     | -1.0 .. 1.0 | negative = cool/blue, positive = warm/amber |
/// | strength     | 1.0     | 0.0 .. 1.0  | blends preset params with identity; 1.0 = full effect |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterParams {
    pub kind:        FilterKind,
    pub brightness:  f32,
    pub contrast:    f32,
    pub saturation:  f32,
    pub gamma:       f32,
    pub hue:         f32,
    pub temperature: f32,
    /// 0.0 = ignore all params (identity), 1.0 = full effect.
    /// Lets users dial-in partial preset intensity without exposing 6 sliders.
    pub strength:    f32,
}

impl Default for FilterParams {
    fn default() -> Self { Self::none() }
}

impl FilterParams {
    /// Identity — no processing, zero cost at runtime.
    pub fn none() -> Self {
        Self {
            kind:        FilterKind::None,
            brightness:  0.0,
            contrast:    1.0,
            saturation:  1.0,
            gamma:       1.0,
            hue:         0.0,
            temperature: 0.0,
            strength:    1.0,
        }
    }

    /// Returns true when this filter has no visible effect.
    /// Both encode and scrub paths call this before doing any work.
    pub fn is_identity(&self) -> bool {
        self.strength == 0.0
            || (self.brightness  == 0.0
                && self.contrast     == 1.0
                && self.saturation   == 1.0
                && self.gamma        == 1.0
                && self.hue          == 0.0
                && self.temperature  == 0.0)
    }

    /// Canonical parameter values for each named preset.
    /// `strength` is always left at 1.0 here — the UI slider handles partial blending.
    pub fn from_preset(kind: FilterKind) -> Self {
        match kind {
            FilterKind::None => Self::none(),

            // Slightly desaturated, lifted shadows, gentle S-curve feel
            FilterKind::Cinematic => Self {
                kind,
                brightness:  0.03,
                contrast:    1.15,
                saturation:  0.80,
                gamma:       0.92,
                hue:         0.0,
                temperature: 0.05,
                strength:    1.0,
            },

            // Warm tint, reduced saturation, lifted blacks (analog feel)
            FilterKind::Vintage => Self {
                kind,
                brightness:  0.06,
                contrast:    0.85,
                saturation:  0.70,
                gamma:       1.05,
                hue:         5.0,
                temperature: 0.30,
                strength:    1.0,
            },

            // Blue shift, slight contrast boost (overcast/moody)
            FilterKind::Cool => Self {
                kind,
                brightness:  -0.03,
                contrast:    1.10,
                saturation:  0.95,
                gamma:       1.0,
                hue:         -8.0,
                temperature: -0.35,
                strength:    1.0,
            },

            // Saturated, punchy (social media pop)
            FilterKind::Vivid => Self {
                kind,
                brightness:  0.02,
                contrast:    1.20,
                saturation:  1.50,
                gamma:       0.95,
                hue:         0.0,
                temperature: 0.0,
                strength:    1.0,
            },

            // Greyscale
            FilterKind::BlackAndWhite => Self {
                kind,
                brightness:  0.0,
                contrast:    1.05,
                saturation:  0.0,
                gamma:       1.0,
                hue:         0.0,
                temperature: 0.0,
                strength:    1.0,
            },

            // Low contrast, lifted shadows (film-fade / Instagram matte)
            FilterKind::Faded => Self {
                kind,
                brightness:  0.10,
                contrast:    0.75,
                saturation:  0.85,
                gamma:       1.08,
                hue:         0.0,
                temperature: 0.08,
                strength:    1.0,
            },

            // Warm orange/amber push (sunset / magic hour)
            FilterKind::GoldenHour => Self {
                kind,
                brightness:  0.04,
                contrast:    1.10,
                saturation:  1.15,
                gamma:       0.95,
                hue:         8.0,
                temperature: 0.45,
                strength:    1.0,
            },

            // Deep cool blue — night / lo-fi
            FilterKind::NightBlue => Self {
                kind,
                brightness:  -0.08,
                contrast:    1.05,
                saturation:  0.75,
                gamma:       1.10,
                hue:         -15.0,
                temperature: -0.50,
                strength:    1.0,
            },

            // High contrast, vivid, slight warm push — action / sport
            FilterKind::Punchy => Self {
                kind,
                brightness:  0.0,
                contrast:    1.35,
                saturation:  1.30,
                gamma:       0.88,
                hue:         3.0,
                temperature: 0.12,
                strength:    1.0,
            },
        }
    }

    /// Blend this preset's params toward identity by `1.0 - strength`.
    /// Called by the pixel-math functions — no need to call manually.
    ///
    /// Returns a `FilterParams` with `strength = 1.0` so the math functions
    /// can use the values directly without re-applying the blend factor.
    pub fn apply_strength(&self) -> Self {
        if self.strength >= 1.0 { return self.clone(); }
        let s = self.strength.clamp(0.0, 1.0);
        let id = Self::none();
        Self {
            kind:        self.kind,
            brightness:  lerp(id.brightness,  self.brightness,  s),
            contrast:    lerp(id.contrast,     self.contrast,    s),
            saturation:  lerp(id.saturation,   self.saturation,  s),
            gamma:       lerp(id.gamma,        self.gamma,       s),
            hue:         lerp(id.hue,          self.hue,         s),
            temperature: lerp(id.temperature,  self.temperature, s),
            strength:    1.0,
        }
    }
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}