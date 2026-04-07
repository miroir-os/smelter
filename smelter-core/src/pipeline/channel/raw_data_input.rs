use std::{sync::Arc, thread, time::Duration};

use crossbeam_channel::{Receiver, Sender, bounded};
use tracing::{debug, trace};

use crate::{pipeline::input::Input, queue::QueueDataReceiver};

use crate::prelude::*;

pub struct RawDataInput;

impl RawDataInput {
    pub fn new_input(
        ctx: Arc<PipelineCtx>,
        input_ref: Ref<InputId>,
        options: RawDataInputOptions,
    ) -> Result<(Input, RawDataInputSender, QueueDataReceiver), InputInitError> {
        let buffer_duration = options
            .buffer_duration
            .unwrap_or(ctx.default_buffer_duration);
        let (video_sender, video_receiver) = match options.video {
            true => {
                let (sender, receiver) =
                    spawn_video_repacking_thread(ctx.clone(), &input_ref, buffer_duration);
                (Some(sender), Some(receiver))
            }
            false => (None, None),
        };
        let (audio_sender, audio_receiver) = match options.audio {
            true => {
                let (sender, receiver) =
                    spawn_audio_repacking_thread(ctx.clone(), &input_ref, buffer_duration);
                (Some(sender), Some(receiver))
            }
            false => (None, None),
        };
        Ok((
            Input::RawDataChannel,
            RawDataInputSender {
                video: video_sender,
                audio: audio_sender,
            },
            QueueDataReceiver {
                video: video_receiver,
                audio: audio_receiver,
            },
        ))
    }
}

fn spawn_video_repacking_thread(
    ctx: Arc<PipelineCtx>,
    input_ref: &Ref<InputId>,
    buffer_duration: Duration,
) -> (Sender<PipelineEvent<Frame>>, Receiver<PipelineEvent<Frame>>) {
    let (output_sender, output_receiver) = bounded(5);
    let (input_sender, input_receiver) = bounded::<PipelineEvent<Frame>>(1000);

    let input_ref_clone = input_ref.clone();
    thread::Builder::new()
        .name(format!(
            "Raw channel video synchronization thread for input {input_ref}"
        ))
        .spawn(move || {
            let mut start_pts = None;
            let mut first_frame_pts = None;
            let mut frame_count = 0u64;
            for event in input_receiver.into_iter() {
                let event = match event {
                    PipelineEvent::Data(mut frame) => {
                        let is_first = start_pts.is_none();
                        let start_pts =
                            *start_pts.get_or_insert_with(|| ctx.queue_sync_point.elapsed());
                        let first_frame_pts = *first_frame_pts.get_or_insert(frame.pts);
                        let original_pts = frame.pts;
                        frame.pts = frame.pts + start_pts + buffer_duration - first_frame_pts;
                        frame_count += 1;
                        if is_first || frame_count % 150 == 0 {
                            tracing::warn!(
                                "[AV_SYNC] smelter::raw_video_repack input={}: original_pts={:.1}ms start_pts={:.1}ms buffer={:.1}ms first_frame_pts={:.1}ms -> output_pts={:.1}ms (frame #{})",
                                input_ref_clone,
                                original_pts.as_secs_f64() * 1000.0,
                                start_pts.as_secs_f64() * 1000.0,
                                buffer_duration.as_secs_f64() * 1000.0,
                                first_frame_pts.as_secs_f64() * 1000.0,
                                frame.pts.as_secs_f64() * 1000.0,
                                frame_count,
                            );
                        }
                        PipelineEvent::Data(frame)
                    }
                    PipelineEvent::EOS => PipelineEvent::EOS,
                };
                trace!(?event, "Sending raw frame");
                if output_sender.send(event).is_err() {
                    debug!("Failed to send packet. Channel closed.");
                    break;
                }
            }
        })
        .unwrap();

    (input_sender, output_receiver)
}

fn spawn_audio_repacking_thread(
    ctx: Arc<PipelineCtx>,
    input_ref: &Ref<InputId>,
    buffer_duration: Duration,
) -> (
    Sender<PipelineEvent<InputAudioSamples>>,
    Receiver<PipelineEvent<InputAudioSamples>>,
) {
    let (output_sender, output_receiver) = bounded(5);
    let (input_sender, input_receiver) = bounded::<PipelineEvent<InputAudioSamples>>(1000);

    let input_ref_clone = input_ref.clone();
    thread::Builder::new()
        .name(format!(
            "Raw channel audio synchronization thread for input {input_ref}"
        ))
        .spawn(move || {
            let mut start_pts = None;
            let mut first_frame_pts = None;
            let mut batch_count = 0u64;
            for event in input_receiver.into_iter() {
                let event = match event {
                    PipelineEvent::Data(mut batch) => {
                        let is_first = start_pts.is_none();
                        let start_pts =
                            *start_pts.get_or_insert_with(|| ctx.queue_sync_point.elapsed());
                        let first_frame_pts = *first_frame_pts.get_or_insert(batch.start_pts);
                        let original_pts = batch.start_pts;
                        batch.start_pts =
                            batch.start_pts + start_pts + buffer_duration - first_frame_pts;
                        batch_count += 1;
                        // Log first + every ~5s (250 batches at 20ms)
                        if is_first || batch_count % 250 == 0 {
                            tracing::warn!(
                                "[AV_SYNC] smelter::raw_audio_repack input={}: original_pts={:.1}ms start_pts={:.1}ms buffer={:.1}ms first_frame_pts={:.1}ms -> output_pts={:.1}ms (batch #{})",
                                input_ref_clone,
                                original_pts.as_secs_f64() * 1000.0,
                                start_pts.as_secs_f64() * 1000.0,
                                buffer_duration.as_secs_f64() * 1000.0,
                                first_frame_pts.as_secs_f64() * 1000.0,
                                batch.start_pts.as_secs_f64() * 1000.0,
                                batch_count,
                            );
                        }
                        PipelineEvent::Data(batch)
                    }
                    PipelineEvent::EOS => PipelineEvent::EOS,
                };
                trace!(?event, "Sending raw samples");
                if output_sender.send(event).is_err() {
                    debug!("Failed to send packet. Channel closed.");
                    break;
                }
            }
        })
        .unwrap();

    (input_sender, output_receiver)
}
