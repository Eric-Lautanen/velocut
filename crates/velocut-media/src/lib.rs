// crates/velocut-media/src/lib.rs

pub mod audio;
pub mod decode;
pub mod encode;
mod helpers;
pub mod probe;
pub mod waveform;
pub mod worker; // internal — not pub, not re-exported

pub use encode::{ClipSpec, EncodeSpec};
pub use velocut_core::media_types::{MediaResult, PlaybackFrame};
pub use worker::MediaWorker;
