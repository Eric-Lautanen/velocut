// crates/velocut-core/src/transitions.rs
//
// Transition types and clip-boundary definitions.
//
// Design:
//   TransitionType   — the kind of transition and its parameters.
//   ClipTransition   — binds a TransitionType to a position in the clip list
//                      (0-based index of the clip BEFORE the transition).
//
// Both types are serializable so transitions are saved with the project.
//
// Adding a new transition type:
//   1. Add a variant to TransitionType with whatever parameters it needs.
//   2. Implement the blend logic in velocut-media/src/transitions/ (Section 4+).
//   3. No changes needed here, in ProjectState, or in EncodeSpec — the registry
//      picks up the new variant automatically.
//
// Encode pipeline integration:
//   EncodeSpec carries a Vec<ClipTransition>. encode_timeline() checks this vec
//   between every pair of adjacent clips. TransitionType::Cut is a zero-cost
//   pass-through — the encode output is identical to a timeline with no
//   transitions field at all.

use serde::{Deserialize, Serialize};

/// The kind of transition to apply between two adjacent clips.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransitionType {
    /// Hard cut — no blending, frames are spliced directly.
    /// This is the default and has zero encode overhead.
    Cut,

    /// Linear crossfade (dissolve) between the outgoing and incoming clip.
    /// The last `duration_secs` of clip A is blended with the first
    /// `duration_secs` of clip B. Both clips are shortened by that amount
    /// so the total output duration is preserved.
    Crossfade { duration_secs: f32 },

    // ── Future variants ───────────────────────────────────────────────────────
    // DipToBlack { duration_secs: f32 },
    // Wipe { direction: WipeDir, duration_secs: f32 },
}

impl Default for TransitionType {
    fn default() -> Self { TransitionType::Cut }
}

/// A transition placed between two adjacent clips in the encode pipeline.
///
/// `after_clip_index` is 0-based: a value of 0 means "between clip 0 and clip 1".
/// Values >= clips.len() - 1 are ignored by the encoder (out of range).
///
/// Stored in both ProjectState (serialized) and EncodeSpec (runtime).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipTransition {
    /// Index of the clip that comes BEFORE this transition in the ordered clip list.
    pub after_clip_index: usize,
    /// The kind of transition to apply.
    pub kind: TransitionType,
}