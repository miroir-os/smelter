use std::sync::Arc;

use crossbeam_channel::bounded;
use smelter_render::Frame;
use tracing::{Level, error, span};

use crate::pipeline::input::Input;
use crate::queue::{QueueTrackOffset, QueueTrackOptions};
use crate::utils::input_buffer::{InputDelayBuffer, spawn_delay_repacking_thread};
use crate::{pipeline::decklink::format::Format, queue::QueueInput};

use crate::prelude::*;

use self::{capture::ChannelCallbackAdapter, find_device::find_decklink};

mod capture;
mod find_device;
mod format;

// sample rate returned from DeckLink
const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// DeckLink input - captures raw video (and optionally audio) from a Blackmagic
/// DeckLink capture card via the DeckLink SDK callback interface.
///
/// ## Timestamps
///
/// - On first video/audio packet, compute offset as `sync_point.elapsed() - stream_time`.
///   PTS of each subsequent packet is `stream_time + offset + 40ms`.
/// - The 40ms buffer accounts for delivery latency, value could lower for video, but
///   but for audio we need at least 40ms.
/// - The SDK callback never blocks: frames/samples are try-sent into per-track
///   repacking threads (dropped with a warning if that channel is full).
/// - `buffer_duration` delay is materialized by the repacking threads'
///   `InputDelayBuffer` plus the `Pts(buffer_duration)` track offset; PTS
///   itself is never shifted by it.
/// - On format change the PTS offsets reset while up to `buffer_duration` of
///   old-offset media is still draining — accepted, a format change is already
///   a visible stream restart.
///
/// ### Format detection
/// - Initial video mode is provisional (HD720p50). `enable_format_detection` is set,
///   so the SDK calls `video_input_format_changed` when the real format is detected.
/// - On format change, streams are paused, video is re-enabled with the new mode,
///   streams are flushed and restarted, and video/audio offsets are reset (recomputed
///   on the next packet).
///
/// ### Unsupported scenarios
/// - If ahead of time processing is enabled, initial registration will happen on pts already
///   processed by the queue, but queue will wait and eventually stream will show up, with
///   the portion at the start cut off.
/// - If queue is to slow (e.g. other input required and to slow), media will be delivered to
///   queue too late and dropped
pub struct DeckLink {
    input: Arc<decklink::Input>,
}

impl DeckLink {
    pub fn new_input(
        ctx: Arc<PipelineCtx>,
        input_ref: Ref<InputId>,
        opts: DeckLinkInputOptions,
    ) -> Result<(Input, InputInitInfo, QueueInput), InputInitError> {
        let span = span!(
            Level::INFO,
            "DeckLink input",
            input_id = input_ref.to_string()
        );
        let input = Arc::new(
            find_decklink(&opts)?
                .input()
                .map_err(DeckLinkInputError::DecklinkError)?,
        );
        let initial_mode = decklink::DisplayModeType::ModeHD720p50;
        let initial_pixel_format = opts
            .pixel_format
            .unwrap_or(decklink::PixelFormat::Format8BitYUV);

        // Initial options, real config should be set based on detected format, thanks
        // to the `enable_format_detection` option. When enabled it will call
        // `video_input_format_changed` method with a detected format.
        input
            .enable_video(
                initial_mode,
                initial_pixel_format,
                decklink::VideoInputFlags {
                    enable_format_detection: true,
                    ..Default::default()
                },
            )
            .map_err(DeckLinkInputError::DecklinkError)?;
        input
            .enable_audio(AUDIO_SAMPLE_RATE, decklink::AudioSampleType::Sample32bit, 2)
            .map_err(DeckLinkInputError::DecklinkError)?;

        let queue_input = QueueInput::new(&ctx, &input_ref, opts.queue_options);
        let (video_sender, audio_sender) = queue_input.queue_new_track(QueueTrackOptions {
            video: true,
            audio: opts.enable_audio,
            offset: QueueTrackOffset::Pts(opts.buffer_duration),
        });

        // The SDK callback must never block, so it feeds per-track repacking
        // threads which hold the delay buffer and do the blocking queue send.
        let video_sender = video_sender.map(|sender| {
            let (raw_sender, raw_receiver) = bounded::<Frame>(1000);
            spawn_delay_repacking_thread(
                format!("DeckLink video repacking thread for input {input_ref}"),
                raw_receiver,
                InputDelayBuffer::new(opts.buffer_duration),
                sender,
            );
            raw_sender
        });
        let audio_sender = audio_sender.map(|sender| {
            let (raw_sender, raw_receiver) = bounded::<InputAudioSamples>(1000);
            spawn_delay_repacking_thread(
                format!("DeckLink audio repacking thread for input {input_ref}"),
                raw_receiver,
                InputDelayBuffer::new(opts.buffer_duration),
                sender,
            );
            raw_sender
        });

        let callback = ChannelCallbackAdapter::new(
            &ctx,
            span,
            video_sender,
            audio_sender,
            Arc::<decklink::Input>::downgrade(&input),
            Format::new(initial_mode, initial_pixel_format),
        );
        input
            .set_callback(Box::new(callback))
            .map_err(DeckLinkInputError::DecklinkError)?;
        input
            .start_streams()
            .map_err(DeckLinkInputError::DecklinkError)?;

        Ok((
            Input::DeckLink(Self { input }),
            InputInitInfo::Other,
            queue_input,
        ))
    }
}

impl Drop for DeckLink {
    fn drop(&mut self) {
        if let Err(err) = self.input.stop_streams() {
            error!("Failed to stop streams: {:?}", err);
        }
    }
}
