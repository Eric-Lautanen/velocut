// crates/velocut-media/src/helpers/resample.rs
//
// Shared swresample setup helpers.

use ffmpeg::util::channel_layout::ChannelLayout;
use ffmpeg::util::frame::audio::Audio as AudioFrame;
use ffmpeg_the_third as ffmpeg;

/// Give `frame` the standard channel layout for its channel count when the
/// codec reported no mask.
///
/// Plain PCM in a basic WAV header is a common case. `ffmpeg-the-third`'s
/// `Context::get2` unwraps `mask()` internally (panic), and `swr_convert_frame`
/// reads the *frame's* layout (error), so the frame itself has to be fixed
/// before the resampler is built or run.
pub fn ensure_layout(frame: &mut AudioFrame) {
    let (has_mask, channels) = {
        let layout = frame.ch_layout();
        (layout.mask().is_some(), layout.channels())
    };
    if !has_mask {
        frame.set_ch_layout(ChannelLayout::default_for_channels(channels.max(1)));
    }
}
