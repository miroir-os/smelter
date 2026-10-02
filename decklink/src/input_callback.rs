use std::ptr::NonNull;

use crate::{
    AudioInputPacket, DisplayMode, VideoInputFrame,
    enums::ffi::{DetectedVideoInputFormatFlags, VideoInputFormatChangedEvents},
};

pub enum InputCallbackResult {
    Ok,
    Failure,
}

pub trait InputCallback {
    fn video_input_frame_arrived(
        &self,
        video_frame: Option<&mut VideoInputFrame>,
        audio_packet: Option<&mut AudioInputPacket>,
    ) -> InputCallbackResult;

    fn video_input_format_changed(
        &self,
        events: VideoInputFormatChangedEvents,
        display_mode: DisplayMode,
        flags: DetectedVideoInputFormatFlags,
    ) -> InputCallbackResult;
}

/// Supplies the memory DeckLink captures video frames into. DeckLink captures
/// into its buffers in turn, whatever still references their frames.
pub trait FrameAllocator: Send + Sync {
    /// Lends `size` bytes, holding rows of `row_bytes`, until `release`. With
    /// `None`, DeckLink keeps capturing into the buffers it already holds, or
    /// fails to enable the format when it holds none.
    fn allocate(&self, size: usize, row_bytes: usize) -> Option<NonNull<u8>>;

    /// Takes back a buffer DeckLink no longer captures into.
    fn release(&self, bytes: NonNull<u8>);
}
