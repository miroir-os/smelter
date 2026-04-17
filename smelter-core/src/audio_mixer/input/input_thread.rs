use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use tracing::{trace, warn};

use crate::audio_mixer::input::{
    AudioMixerInputEvent, AudioMixerInputResult, resampler::InputResampler,
};

use crate::prelude::*;

pub(super) fn start_input_thread(
    mixing_sample_rate: u32,
    input_receiver: Receiver<AudioMixerInputEvent>,
    result_sender: Sender<AudioMixerInputResult>,
) {
    std::thread::Builder::new()
        .name("audio mixer input".to_string())
        .spawn(move || {
            let mut processor = InputProcessor::new(mixing_sample_rate);
            let mut call_count = 0u64;

            for event in input_receiver {
                call_count += 1;
                let batch_count = event.batches.len();
                let batch_total_samples: usize =
                    event.batches.iter().map(|b| b.len()).sum();
                let batch_pts_range = if event.batches.is_empty() {
                    None
                } else {
                    Some((
                        event.batches.first().unwrap().start_pts,
                        event.batches.last().unwrap().pts_range().1,
                    ))
                };

                for batch in event.batches {
                    processor.write_batch(batch);
                }

                let pts_range = event.pts_range;
                let samples = processor.get_samples(pts_range);

                if call_count <= 5 || call_count % 50 == 0 {
                    warn!(
                        "[SMELTER_TRACE] MIXER_INPUT call={call_count} \
                         requested=[{:.3}ms,{:.3}ms] batches={batch_count} \
                         batch_samples={batch_total_samples} \
                         batch_pts=[{},{}] output_samples={}",
                        pts_range.0.as_secs_f64() * 1000.0,
                        pts_range.1.as_secs_f64() * 1000.0,
                        batch_pts_range
                            .map(|(s, _)| format!("{:.3}ms", s.as_secs_f64() * 1000.0))
                            .unwrap_or_else(|| "none".to_string()),
                        batch_pts_range
                            .map(|(_, e)| format!("{:.3}ms", e.as_secs_f64() * 1000.0))
                            .unwrap_or_else(|| "none".to_string()),
                        samples.len(),
                    );
                }

                let result = AudioMixerInputResult { samples, pts_range };
                if result_sender.send(result).is_err() {
                    trace!("Closing audio mixer input processing thread. Channel closed.");
                    return;
                }
            }
        })
        .unwrap();
}

struct InputProcessor {
    mixing_sample_rate: u32,
    resampler: Option<InputResampler>,
}

impl InputProcessor {
    pub fn new(mixing_sample_rate: u32) -> Self {
        Self {
            mixing_sample_rate,
            resampler: None,
        }
    }

    pub fn write_batch(&mut self, batch: InputAudioSamples) {
        let channels = match batch.samples {
            AudioSamples::Mono(_) => AudioChannels::Mono,
            AudioSamples::Stereo(_) => AudioChannels::Stereo,
        };
        let input_sample_rate = batch.sample_rate;

        let resampler = self.resampler.get_or_insert_with(|| {
            InputResampler::new(
                input_sample_rate,
                self.mixing_sample_rate,
                channels,
                batch.start_pts,
            )
            .unwrap()
        });
        if resampler.channels() != channels || resampler.input_sample_rate() != input_sample_rate {
            *resampler = InputResampler::new(
                input_sample_rate,
                self.mixing_sample_rate,
                channels,
                batch.start_pts,
            )
            .unwrap();
        }
        resampler.write_batch(batch);
    }

    pub fn get_samples(&mut self, pts_range: (Duration, Duration)) -> Vec<(f64, f64)> {
        match &mut self.resampler {
            Some(resampler) => match resampler.get_samples(pts_range) {
                AudioSamples::Mono(items) => {
                    items.into_iter().map(|sample| (sample, sample)).collect()
                }
                AudioSamples::Stereo(items) => items,
            },
            None => {
                let sample_count = f64::floor(
                    (pts_range.1 - pts_range.0).as_secs_f64() * self.mixing_sample_rate as f64,
                ) as usize;
                vec![(0.0, 0.0); sample_count]
            }
        }
    }
}
