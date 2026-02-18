// crates/velocut-media/src/lib.rs
//
// No egui dependency — communicates with velocut-ui via channels only.
//
// To add a new media capability:
//   1. Create a new module file here
//   2. Add `mod mymodule;` below
//   3. Call it from worker.rs (probe_clip or a new MediaWorker method)

pub mod audio;
pub mod decode;
pub mod encode;
pub mod probe;
pub mod waveform;
pub mod worker;

// Re-export the main public API so velocut-ui imports are simple.
pub use worker::MediaWorker;
pub use velocut_core::media_types::{MediaResult, PlaybackFrame};

// Re-export encode types so app.rs can build EncodeSpec without a deep import path.
pub use encode::{ClipSpec, EncodeSpec};