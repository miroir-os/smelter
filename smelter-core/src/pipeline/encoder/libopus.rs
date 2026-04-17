use std::sync::Arc;

use bytes::Bytes;
use tracing::{error, info};

use crate::prelude::*;

use super::{AudioEncoder, AudioEncoderConfig};

#[derive(Debug)]
pub struct OpusEncoder {
    encoder: opus::Encoder,
    output_buffer: Vec<u8>,
}

impl AudioEncoder for OpusEncoder {
    const LABEL: &'static str = "libopus encoder";

    type Options = OpusEncoderOptions;

    fn new(
        _ctx: &Arc<PipelineCtx>,
        options: Self::Options,
    ) -> Result<(Self, AudioEncoderConfig), EncoderInitError> {
        info!(?options, "Initializing libopus encoder");
        let mut encoder = opus::Encoder::new(
            options.sample_rate,
            options.channels.into(),
            options.preset.into(),
        )?;
        encoder.set_inband_fec(options.forward_error_correction)?;
        encoder.set_packet_loss_perc(options.packet_loss)?;

        // OpusHead pre_skip is in 48 kHz samples (RFC 7845 §4.2) but
        // `get_lookahead` returns input-rate samples; scale or decoders miss
        // part of the pre-roll on sub-48 kHz streams. Matches ffmpeg's
        // `libavcodec/libopusenc.c:100`.
        let pre_skip = (encoder.get_lookahead()? as u32 * 48_000 / options.sample_rate) as u16;
        let extradata = opus_head(options.channels, options.sample_rate, pre_skip);

        let output_buffer = vec![0u8; 1024 * 1024];

        Ok((
            Self {
                encoder,
                output_buffer,
            },
            AudioEncoderConfig {
                extradata: Some(extradata),
            },
        ))
    }

    fn set_packet_loss(&mut self, packet_loss: i32) {
        if let Err(e) = self.encoder.set_packet_loss_perc(packet_loss) {
            error!(%e, "Error while setting opus encoder packet loss.");
        }
    }

    fn encode(&mut self, batch: OutputAudioSamples) -> Vec<EncodedOutputChunk> {
        let raw_samples: Vec<_> = match batch.samples {
            AudioSamples::Mono(raw_samples) => raw_samples
                .iter()
                .map(|val| (*val * i16::MAX as f64) as i16)
                .collect(),
            AudioSamples::Stereo(stereo_samples) => stereo_samples
                .iter()
                .flat_map(|(l, r)| [(*l * i16::MAX as f64) as i16, (*r * i16::MAX as f64) as i16])
                .collect(),
        };

        match self.encoder.encode(&raw_samples, &mut self.output_buffer) {
            Ok(len) => vec![EncodedOutputChunk {
                data: bytes::Bytes::copy_from_slice(&self.output_buffer[..len]),
                pts: batch.start_pts,
                dts: None,
                is_keyframe: false,
                kind: MediaKind::Audio(AudioCodec::Opus),
            }],
            Err(err) => {
                error!("Opus encoding error: {}", err);
                vec![]
            }
        }
    }

    fn flush(&mut self) -> Vec<EncodedOutputChunk> {
        vec![]
    }
}

impl From<OpusEncoderPreset> for opus::Application {
    fn from(value: OpusEncoderPreset) -> Self {
        match value {
            OpusEncoderPreset::Quality => opus::Application::Audio,
            OpusEncoderPreset::Voip => opus::Application::Voip,
            OpusEncoderPreset::LowestLatency => opus::Application::LowDelay,
        }
    }
}

// RFC 7845 §5.1 OpusHead (mono/stereo, channel mapping family 0).
fn opus_head(channels: AudioChannels, sample_rate: u32, pre_skip: u16) -> Bytes {
    let channel_count: u8 = match channels {
        AudioChannels::Mono => 1,
        AudioChannels::Stereo => 2,
    };
    let mut buf = [0u8; 19];
    buf[0..8].copy_from_slice(b"OpusHead");
    buf[8] = 1;
    buf[9] = channel_count;
    buf[10..12].copy_from_slice(&pre_skip.to_le_bytes());
    buf[12..16].copy_from_slice(&sample_rate.to_le_bytes());
    buf[16..18].copy_from_slice(&0i16.to_le_bytes());
    buf[18] = 0;
    Bytes::copy_from_slice(&buf)
}
