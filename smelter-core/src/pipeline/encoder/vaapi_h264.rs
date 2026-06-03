#[cfg(feature = "vaapi")]
mod imp {
    use std::{collections::HashMap, rc::Rc, sync::Arc, time::Duration};

    use bytes::Bytes;
    use libva::{
        Buffer, BufferType, Config, Context, Display, EncCodedBuffer, EncMiscParameter,
        EncMiscParameterFrameRate, EncMiscParameterRateControl, EncPictureParameter,
        EncPictureParameterBufferH264, EncSequenceParameter,
        EncSequenceParameterBufferH264, EncSliceParameter, EncSliceParameterBufferH264,
        H264EncFrameCropOffsets, H264EncPicFields, H264EncSeqFields, H264VuiFields,
        MappedCodedBuffer, Picture, PictureH264, RcFlags, Surface, UsageHint,
        VA_FOURCC_NV12, VA_INVALID_ID, VA_PICTURE_H264_INVALID,
        VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RC_CBR, VA_RT_FORMAT_YUV420,
        VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAProfile,
    };
    use smelter_render::{DmaBufFrame, FrameData, Framerate, OutputFrameFormat};
    use tracing::{error, info};

    use crate::{
        pipeline::{
            encoder::{
                VideoEncoder, VideoEncoderConfig,
                utils::{bitrate_from_resolution_framerate, gop_size_from_ms_framerate},
            },
            utils::{annexb_to_avcc, build_avc_decoder_config, h264_main_parameter_sets},
            vaapi::{VaapiDmaBufFrame, open_display},
        },
        prelude::*,
    };

    const H264_LEVEL_4_0: u8 = 40;
    const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 12;
    const LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4: u32 = 12;
    const DEFAULT_CODED_BUFFER_SIZE: usize = 1_500_000;

    pub struct VaapiH264Encoder {
        encoder: IntelVaapiH264Encoder,
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
            let bitrate = options.bitrate.unwrap_or_else(|| {
                let bitrate =
                    bitrate_from_resolution_framerate(options.resolution, framerate);
                VaapiH264EncoderRateControl::ConstantBitrate(bitrate.average_bitrate)
            });
            let VaapiH264EncoderRateControl::ConstantBitrate(bitrate) = bitrate;
            let bitrate = bitrate.min(u32::MAX as u64) as u32;
            let parameter_sets = h264_main_parameter_sets(options.resolution, framerate);
            let extradata = (options.bitstream_format == H264BitstreamFormat::Avcc)
                .then(|| build_avc_decoder_config(&parameter_sets))
                .flatten();

            let encoder = IntelVaapiH264Encoder::new(
                display,
                options.resolution,
                bitrate,
                gop_size,
                framerate,
                parameter_sets,
            )
            .map_err(|err| {
                error!("Failed to initialize VA-API H264 encoder: {err}");
                EncoderInitError::VaapiH264EncoderUnavailable(err)
            })?;

            info!(
                width = options.resolution.width,
                height = options.resolution.height,
                bitrate,
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

            match self.encoder.encode(dmabuf, frame.pts, force_keyframe) {
                Ok(frame) => vec![self.chunk_from_frame(frame)],
                Err(err) => {
                    error!("VA-API encoder error: {err}");
                    Vec::new()
                }
            }
        }

        fn flush(&mut self) -> Vec<EncodedOutputChunk> {
            Vec::new()
        }
    }

    impl VaapiH264Encoder {
        fn chunk_from_frame(&self, frame: EncodedVaapiFrame) -> EncodedOutputChunk {
            let data = if self.bitstream_format == H264BitstreamFormat::Avcc {
                annexb_to_avcc(&frame.data)
            } else {
                frame.data
            };
            EncodedOutputChunk {
                data,
                pts: frame.pts,
                dts: None,
                is_keyframe: frame.is_keyframe,
                kind: MediaKind::Video(VideoCodec::H264),
            }
        }
    }

    struct IntelVaapiH264Encoder {
        _config: Config,
        context: Rc<Context>,
        display: Rc<Display>,
        input_surfaces: HashMap<usize, Surface<VaapiDmaBufFrame>>,
        free_reconstructed_surfaces: Vec<Surface<()>>,
        reference: Option<EncodedReference>,
        resolution: smelter_render::Resolution,
        bitrate: u32,
        gop_size: u16,
        frames_since_keyframe: u16,
        frame_num: u16,
        idr_pic_id: u16,
        framerate: Framerate,
        parameter_sets: Bytes,
    }

    impl IntelVaapiH264Encoder {
        fn new(
            display: Rc<Display>,
            resolution: smelter_render::Resolution,
            bitrate: u32,
            gop_size: u16,
            framerate: Framerate,
            parameter_sets: Bytes,
        ) -> Result<Self, String> {
            let profile = VAProfile::VAProfileH264Main;
            let entrypoint = h264_encode_entrypoint(&display, profile)?;
            let config = display
                .create_config(
                    vec![
                        VAConfigAttrib {
                            type_: VAConfigAttribType::VAConfigAttribRTFormat,
                            value: VA_RT_FORMAT_YUV420,
                        },
                        VAConfigAttrib {
                            type_: VAConfigAttribType::VAConfigAttribRateControl,
                            value: VA_RC_CBR,
                        },
                    ],
                    profile,
                    entrypoint,
                )
                .map_err(|err| format!("failed to create VA-API H264 config: {err}"))?;
            let context = display
                .create_context::<VaapiDmaBufFrame>(
                    &config,
                    resolution.width as u32,
                    resolution.height as u32,
                    None,
                    true,
                )
                .map_err(|err| format!("failed to create VA-API H264 context: {err}"))?;

            Ok(Self {
                _config: config,
                context,
                display,
                input_surfaces: HashMap::new(),
                free_reconstructed_surfaces: Vec::new(),
                reference: None,
                resolution,
                bitrate,
                gop_size,
                frames_since_keyframe: 0,
                frame_num: 0,
                idr_pic_id: 0,
                framerate,
                parameter_sets,
            })
        }

        fn encode(
            &mut self,
            frame: Arc<DmaBufFrame>,
            pts: Duration,
            force_keyframe: bool,
        ) -> Result<EncodedVaapiFrame, String> {
            let input_key = VaapiDmaBufFrame::cache_key(&frame);
            let input = match self.input_surfaces.remove(&input_key) {
                Some(surface) => surface,
                None => VaapiDmaBufFrame::new(frame).import_surface(&self.display)?,
            };
            let is_keyframe = force_keyframe
                || self.reference.is_none()
                || self.frames_since_keyframe >= self.gop_size;
            let reconstructed = self.take_reconstructed_surface()?;
            let coded_buffer = self
                .context
                .create_enc_coded(self.coded_buffer_size())
                .map_err(|err| format!("failed to create VA-API coded buffer: {err}"))?;
            let buffers =
                self.create_buffers(&coded_buffer, &reconstructed, is_keyframe)?;

            let mut picture =
                Picture::new(duration_micros(pts), Rc::clone(&self.context), input);
            for buffer in buffers {
                picture.add_buffer(buffer);
            }

            let picture = picture
                .begin()
                .map_err(|err| format!("failed to begin VA-API picture: {err}"))?;
            let picture = picture
                .render()
                .map_err(|err| format!("failed to render VA-API picture: {err}"))?;
            let picture = picture
                .end()
                .map_err(|err| format!("failed to end VA-API picture: {err}"))?;
            let picture = picture
                .sync()
                .map_err(|(err, _)| format!("failed to sync VA-API picture: {err}"))?;
            let input = picture
                .take_surface()
                .map_err(|_| "VA-API picture kept a shared input surface".to_string())?;
            let data = self.collect_coded_data(&coded_buffer, is_keyframe)?;

            self.input_surfaces.insert(input_key, input);
            self.rotate_reference(reconstructed, is_keyframe);

            Ok(EncodedVaapiFrame { data, pts, is_keyframe })
        }

        fn create_buffers(
            &self,
            coded_buffer: &EncCodedBuffer,
            reconstructed: &Surface<()>,
            is_keyframe: bool,
        ) -> Result<Vec<Buffer>, String> {
            [
                self.sequence_parameter(),
                self.picture_parameter(coded_buffer, reconstructed, is_keyframe),
                self.slice_parameter(is_keyframe),
                self.rate_control_parameter(),
                self.framerate_parameter(),
            ]
            .into_iter()
            .map(|buffer| {
                self.context
                    .create_buffer(buffer)
                    .map_err(|err| format!("failed to create VA-API buffer: {err}"))
            })
            .collect()
        }

        fn sequence_parameter(&self) -> BufferType {
            let (width_mbs, height_mbs) = self.macroblocks();
            let (crop_right, crop_bottom) = self.crop_offsets();
            let frame_crop = (crop_right > 0 || crop_bottom > 0)
                .then(|| H264EncFrameCropOffsets::new(0, crop_right, 0, crop_bottom));
            let seq_fields = H264EncSeqFields::new(
                1,
                1,
                0,
                0,
                1,
                LOG2_MAX_FRAME_NUM_MINUS4,
                0,
                LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4,
                0,
            );
            BufferType::EncSequenceParameter(EncSequenceParameter::H264(
                EncSequenceParameterBufferH264::new(
                    0,
                    H264_LEVEL_4_0,
                    self.gop_size.into(),
                    self.gop_size.into(),
                    1,
                    self.bitrate,
                    1,
                    width_mbs as u16,
                    height_mbs as u16,
                    &seq_fields,
                    0,
                    0,
                    0,
                    0,
                    0,
                    [0; 256],
                    frame_crop,
                    Some(H264VuiFields::new(1, 1, 0, 0, 0, 1, 0, 0)),
                    1,
                    1,
                    1,
                    self.framerate.den.max(1),
                    self.framerate.num.max(1).saturating_mul(2),
                ),
            ))
        }

        fn picture_parameter(
            &self,
            coded_buffer: &EncCodedBuffer,
            reconstructed: &Surface<()>,
            is_keyframe: bool,
        ) -> BufferType {
            let mut reference_frames = invalid_h264_pictures::<16>();
            if let Some(reference) = self.active_reference(is_keyframe) {
                reference_frames[0] = reference.picture();
            }
            let pic_fields =
                H264EncPicFields::new(is_keyframe as u32, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0);
            BufferType::EncPictureParameter(EncPictureParameter::H264(
                EncPictureParameterBufferH264::new(
                    PictureH264::new(
                        reconstructed.id(),
                        self.frame_num_for(is_keyframe).into(),
                        VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                        self.poc_for(is_keyframe).into(),
                        self.poc_for(is_keyframe).into(),
                    ),
                    reference_frames,
                    coded_buffer.id(),
                    0,
                    0,
                    0,
                    self.frame_num_for(is_keyframe),
                    26,
                    0,
                    0,
                    0,
                    0,
                    &pic_fields,
                ),
            ))
        }

        fn slice_parameter(&self, is_keyframe: bool) -> BufferType {
            let mut ref_pic_list_0 = invalid_h264_pictures::<32>();
            if let Some(reference) = self.active_reference(is_keyframe) {
                ref_pic_list_0[0] = reference.picture();
            }
            BufferType::EncSliceParameter(EncSliceParameter::H264(
                EncSliceParameterBufferH264::new(
                    0,
                    self.macroblock_count(),
                    VA_INVALID_ID,
                    if is_keyframe { 2 } else { 0 },
                    0,
                    self.idr_pic_id,
                    self.poc_for(is_keyframe),
                    0,
                    [0, 0],
                    1,
                    (!is_keyframe) as u8,
                    0,
                    0,
                    ref_pic_list_0,
                    invalid_h264_pictures::<32>(),
                    0,
                    0,
                    0,
                    [0; 32],
                    [0; 32],
                    0,
                    [[0; 2]; 32],
                    [[0; 2]; 32],
                    0,
                    [0; 32],
                    [0; 32],
                    0,
                    [[0; 2]; 32],
                    [[0; 2]; 32],
                    0,
                    0,
                    0,
                    2,
                    2,
                ),
            ))
        }

        fn rate_control_parameter(&self) -> BufferType {
            BufferType::EncMiscParameter(EncMiscParameter::RateControl(
                EncMiscParameterRateControl::new(
                    self.bitrate,
                    100,
                    1_500,
                    26,
                    10,
                    0,
                    RcFlags::new(0, 1, 0, 0, 0, 0, 0, 0, 0),
                    0,
                    51,
                    0,
                    0,
                ),
            ))
        }

        fn framerate_parameter(&self) -> BufferType {
            BufferType::EncMiscParameter(EncMiscParameter::FrameRate(
                EncMiscParameterFrameRate::new(
                    self.framerate.num / self.framerate.den.max(1),
                    0,
                ),
            ))
        }

        fn take_reconstructed_surface(&mut self) -> Result<Surface<()>, String> {
            match self.free_reconstructed_surfaces.pop() {
                Some(surface) => Ok(surface),
                None => self
                    .display
                    .create_surfaces(
                        VA_RT_FORMAT_YUV420,
                        Some(VA_FOURCC_NV12),
                        self.resolution.width as u32,
                        self.resolution.height as u32,
                        Some(UsageHint::USAGE_HINT_ENCODER),
                        vec![()],
                    )
                    .map_err(|err| {
                        format!("failed to create VA-API reconstructed surface: {err}")
                    })?
                    .pop()
                    .ok_or_else(|| {
                        "VA-API returned no reconstructed surface".to_string()
                    }),
            }
        }

        fn rotate_reference(&mut self, surface: Surface<()>, encoded_keyframe: bool) {
            if let Some(reference) = self.reference.take() {
                self.free_reconstructed_surfaces.push(reference.surface);
            }
            self.reference = Some(EncodedReference {
                surface,
                frame_num: self.frame_num_for(encoded_keyframe),
                poc: self.poc_for(encoded_keyframe),
            });
            self.frame_num = if encoded_keyframe {
                self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
                self.frames_since_keyframe = 1;
                1
            } else {
                self.frames_since_keyframe = self.frames_since_keyframe.saturating_add(1);
                self.frame_num.wrapping_add(1)
            };
        }

        fn collect_coded_data(
            &self,
            coded_buffer: &EncCodedBuffer,
            is_keyframe: bool,
        ) -> Result<Bytes, String> {
            let mapped = MappedCodedBuffer::new(coded_buffer)
                .map_err(|err| format!("failed to map VA-API coded buffer: {err}"))?;
            let mut out = Vec::new();
            if is_keyframe {
                out.extend_from_slice(&self.parameter_sets);
            }
            let slice_start = out.len();
            for segment in mapped.iter() {
                out.extend_from_slice(segment.buf);
            }
            if out.len() == slice_start {
                return Err("VA-API encoder returned empty coded data".into());
            }
            if out[slice_start..].starts_with(&[0, 0, 1]) {
                out.insert(slice_start, 0);
            }
            Ok(out.into())
        }

        fn active_reference(&self, is_keyframe: bool) -> Option<&EncodedReference> {
            (!is_keyframe).then_some(self.reference.as_ref()).flatten()
        }

        fn coded_buffer_size(&self) -> usize {
            let raw_size = self.resolution.width * self.resolution.height * 3 / 2;
            ((self.bitrate as usize / 8) * 2).max(DEFAULT_CODED_BUFFER_SIZE).max(raw_size)
        }

        fn macroblocks(&self) -> (u32, u32) {
            (
                (self.resolution.width as u32).div_ceil(16),
                (self.resolution.height as u32).div_ceil(16),
            )
        }

        fn macroblock_count(&self) -> u32 {
            let (width_mbs, height_mbs) = self.macroblocks();
            width_mbs * height_mbs
        }

        fn crop_offsets(&self) -> (u32, u32) {
            let (width_mbs, height_mbs) = self.macroblocks();
            (
                (width_mbs * 16 - self.resolution.width as u32) / 2,
                (height_mbs * 16 - self.resolution.height as u32) / 2,
            )
        }

        fn frame_num_for(&self, is_keyframe: bool) -> u16 {
            if is_keyframe { 0 } else { self.frame_num }
        }

        fn poc_for(&self, is_keyframe: bool) -> u16 {
            self.frame_num_for(is_keyframe).wrapping_mul(2)
        }
    }

    struct EncodedReference {
        surface: Surface<()>,
        frame_num: u16,
        poc: u16,
    }

    impl EncodedReference {
        fn picture(&self) -> PictureH264 {
            PictureH264::new(
                self.surface.id(),
                self.frame_num.into(),
                VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                self.poc.into(),
                self.poc.into(),
            )
        }
    }

    struct EncodedVaapiFrame {
        data: Bytes,
        pts: Duration,
        is_keyframe: bool,
    }

    fn h264_encode_entrypoint(
        display: &Display,
        profile: VAProfile::Type,
    ) -> Result<VAEntrypoint::Type, String> {
        let entrypoints = display
            .query_config_entrypoints(profile)
            .map_err(|err| format!("failed to query VA-API H264 entrypoints: {err}"))?;
        if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSliceLP) {
            Ok(VAEntrypoint::VAEntrypointEncSliceLP)
        } else if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
            Ok(VAEntrypoint::VAEntrypointEncSlice)
        } else {
            Err("VA-API H264 encode entrypoint is unavailable".into())
        }
    }

    fn invalid_h264_pictures<const N: usize>() -> [PictureH264; N] {
        std::array::from_fn(|_| {
            PictureH264::new(VA_INVALID_ID, 0, VA_PICTURE_H264_INVALID, 0, 0)
        })
    }

    fn duration_micros(duration: Duration) -> u64 {
        duration.as_micros().try_into().unwrap_or(u64::MAX)
    }
}

#[cfg(feature = "vaapi")]
pub use imp::VaapiH264Encoder;

#[cfg(not(feature = "vaapi"))]
mod imp {
    use std::sync::Arc;

    use smelter_render::Frame;

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
