#[cfg(feature = "vaapi")]
mod imp {
    use std::{rc::Rc, sync::Arc, time::Duration};

    use cros_codecs::{
        BlockingMode, Fourcc,
        backend::vaapi::encoder::VaapiBackend,
        codec::h264::{
            parser::{Level, Pps, PpsBuilder, Profile, Sps, SpsBuilder},
            synthesizer::Synthesizer,
        },
        encoder::{
            FrameMetadata, PredictionStructure, RateControl, Tunings,
            VideoEncoder as CrosVideoEncoder, h264::EncoderConfig,
            stateless::h264::StatelessEncoder as CrosH264Encoder,
        },
        libva::Surface,
    };
    use smelter_render::{FrameData, OutputFrameFormat};
    use tracing::{error, info};

    use crate::{
        pipeline::{
            encoder::{
                VideoEncoder, VideoEncoderConfig,
                utils::{bitrate_from_resolution_framerate, gop_size_from_ms_framerate},
            },
            utils::{annexb_to_avcc, build_avc_decoder_config},
            vaapi::{VaapiDmaBufFrame, open_display},
        },
        prelude::*,
    };

    type Encoder = CrosH264Encoder<
        VaapiDmaBufFrame,
        VaapiBackend<VaapiDmaBufFrame, Surface<VaapiDmaBufFrame>>,
    >;

    pub struct VaapiH264Encoder {
        encoder: Encoder,
        bitstream_format: H264BitstreamFormat,
    }

    impl VideoEncoder for VaapiH264Encoder {
        const LABEL: &'static str = "VA-API H264 encoder";

        type Options = VaapiH264EncoderOptions;

        fn new(
            ctx: &Arc<PipelineCtx>,
            options: Self::Options,
        ) -> Result<(Self, VideoEncoderConfig), EncoderInitError> {
            info!("Initializing VA-API H264 encoder");

            let display =
                open_display().map_err(EncoderInitError::VaapiH264EncoderUnavailable)?;
            let framerate = ctx.output_framerate;
            let gop_size =
                gop_size_from_ms_framerate(options.keyframe_interval, framerate)
                    .clamp(1, u16::MAX as u64) as u16;
            let extradata = avc_decoder_config(&options, framerate);

            let bitrate = options.bitrate.unwrap_or_else(|| {
                let bitrate =
                    bitrate_from_resolution_framerate(options.resolution, framerate);
                VaapiH264EncoderRateControl::ConstantBitrate(bitrate.average_bitrate)
            });
            let rate_control = match &bitrate {
                VaapiH264EncoderRateControl::ConstantBitrate(bitrate) => {
                    RateControl::ConstantBitrate(*bitrate)
                }
            };

            let resolution = cros_codecs::Resolution {
                width: options.resolution.width as u32,
                height: options.resolution.height as u32,
            };
            let config = EncoderConfig {
                resolution,
                profile: Profile::Main,
                level: Level::L4,
                pred_structure: PredictionStructure::LowDelay { limit: gop_size },
                initial_tunings: Tunings {
                    rate_control,
                    framerate: framerate.num / u32::max(framerate.den, 1),
                    ..Default::default()
                },
            };

            let encoder = Encoder::new_vaapi(
                Rc::clone(&display),
                config,
                Fourcc::from(b"NV12"),
                resolution,
                false,
                BlockingMode::Blocking,
            )
            .map_err(|err| {
                error!("Failed to initialize VA-API H264 encoder: {err}");
                EncoderInitError::VaapiH264EncoderUnavailable(err.to_string())
            })?;

            info!(
                width = options.resolution.width,
                height = options.resolution.height,
                ?bitrate,
                bitstream_format = ?options.bitstream_format,
                "Initialized zero-copy VA-API H264 encoder with NV12 DMA-BUF input"
            );

            Ok((
                Self { encoder, bitstream_format: options.bitstream_format },
                VideoEncoderConfig {
                    resolution: options.resolution,
                    output_format: OutputFrameFormat::Nv12DmaBuf,
                    extradata,
                },
            ))
        }

        fn encode(
            &mut self,
            frame: Frame,
            force_keyframe: bool,
        ) -> Vec<EncodedOutputChunk> {
            let FrameData::Nv12DmaBuf(dmabuf) = frame.data else {
                error!("Unsupported pixel format {:?}. Dropping frame.", frame.data);
                return Vec::new();
            };

            let input = VaapiDmaBufFrame::new(dmabuf);
            let metadata = FrameMetadata {
                timestamp: duration_micros(frame.pts),
                layout: input.layout(),
                force_keyframe,
            };

            if let Err(err) = self.encoder.encode(metadata, input) {
                error!("VA-API encoder error: {err}");
                return Vec::new();
            }

            self.poll(frame.pts)
        }

        fn flush(&mut self) -> Vec<EncodedOutputChunk> {
            if let Err(err) = self.encoder.drain() {
                error!("VA-API encoder drain error: {err}");
            }
            self.poll(Duration::ZERO)
        }
    }

    impl VaapiH264Encoder {
        fn poll(&mut self, fallback_pts: Duration) -> Vec<EncodedOutputChunk> {
            let mut chunks = Vec::new();
            loop {
                match self.encoder.poll() {
                    Ok(Some(coded)) => {
                        let is_keyframe = coded.metadata.force_keyframe
                            || contains_idr(&coded.bitstream);
                        let data = if self.bitstream_format == H264BitstreamFormat::Avcc {
                            annexb_to_avcc(&coded.bitstream)
                        } else {
                            coded.bitstream.into()
                        };
                        chunks.push(EncodedOutputChunk {
                            data,
                            pts: Duration::from_micros(coded.metadata.timestamp)
                                .max(fallback_pts),
                            dts: None,
                            is_keyframe,
                            kind: MediaKind::Video(VideoCodec::H264),
                        });
                    }
                    Ok(None) => break,
                    Err(err) => {
                        error!("VA-API encoder poll error: {err}");
                        break;
                    }
                }
            }
            chunks
        }
    }

    fn duration_micros(duration: Duration) -> u64 {
        duration.as_micros().try_into().unwrap_or(u64::MAX)
    }

    fn avc_decoder_config(
        options: &VaapiH264EncoderOptions,
        framerate: smelter_render::Framerate,
    ) -> Option<bytes::Bytes> {
        let gop_size = gop_size_from_ms_framerate(options.keyframe_interval, framerate)
            .clamp(1, u16::MAX as u64) as u32;
        let fps = framerate.num / u32::max(framerate.den, 1);
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .max_frame_num(gop_size)
            .pic_order_cnt_type(0)
            .max_pic_order_cnt_lsb(gop_size * 2)
            .max_num_ref_frames(1)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .resolution(options.resolution.width as u32, options.resolution.height as u32)
            .bit_depth_luma(8)
            .bit_depth_chroma(8)
            .aspect_ratio(1, 1)
            .timing_info(1, u32::max(fps, 1) * 2, false)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .deblocking_filter_control_present_flag(true)
            .num_ref_idx_l0_default_active(1)
            .num_ref_idx_l1_default_active_minus1(0)
            .build();

        let mut headers = Vec::new();
        Synthesizer::<Sps, &mut Vec<u8>>::synthesize(3, &sps, &mut headers, true).ok()?;
        Synthesizer::<Pps, &mut Vec<u8>>::synthesize(3, &pps, &mut headers, true).ok()?;
        build_avc_decoder_config(&headers)
    }

    fn contains_idr(data: &[u8]) -> bool {
        annexb_nalu_types(data).any(|kind| kind == 5)
    }

    fn annexb_nalu_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
        let mut index = 0;
        std::iter::from_fn(move || {
            while index + 4 <= data.len() {
                let start_len = if data[index..].starts_with(&[0, 0, 1]) {
                    3
                } else if data[index..].starts_with(&[0, 0, 0, 1]) {
                    4
                } else {
                    index += 1;
                    continue;
                };
                index += start_len;
                let kind = data.get(index).map(|byte| byte & 0x1f);
                index += 1;
                return kind;
            }
            None
        })
    }
}

#[cfg(feature = "vaapi")]
pub use imp::VaapiH264Encoder;

#[cfg(not(feature = "vaapi"))]
mod imp {
    use std::sync::Arc;

    use crate::{
        pipeline::encoder::{VideoEncoder, VideoEncoderConfig},
        prelude::*,
    };

    pub struct VaapiH264Encoder;

    impl VideoEncoder for VaapiH264Encoder {
        const LABEL: &'static str = "VA-API H264 encoder";

        type Options = VaapiH264EncoderOptions;

        fn new(
            _ctx: &Arc<PipelineCtx>,
            _options: Self::Options,
        ) -> Result<(Self, VideoEncoderConfig), EncoderInitError> {
            Err(EncoderInitError::VaapiH264EncoderUnavailable(
                "support was not compiled into smelter-core".into(),
            ))
        }

        fn encode(
            &mut self,
            _frame: Frame,
            _force_keyframe: bool,
        ) -> Vec<EncodedOutputChunk> {
            Vec::new()
        }

        fn flush(&mut self) -> Vec<EncodedOutputChunk> {
            Vec::new()
        }
    }
}

#[cfg(not(feature = "vaapi"))]
pub use imp::VaapiH264Encoder;
