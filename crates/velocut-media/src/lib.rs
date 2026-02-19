// crates/velocut-media/src/lib.rs

pub mod audio;
pub mod decode;
pub mod encode;
pub mod probe;
pub mod waveform;
pub mod worker;
mod helpers;   // internal — not pub, not re-exported

pub use encode::{ClipSpec, EncodeSpec};
pub use worker::MediaWorker;
pub use velocut_core::media_types::{MediaResult, PlaybackFrame};