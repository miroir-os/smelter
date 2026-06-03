#[cfg(feature = "vaapi")]
mod imp {
    use std::{
        collections::{HashMap, VecDeque},
        rc::Rc,
        sync::Arc,
        time::Duration,
    };

    use libva::{
        Buffer, BufferType, Config, Context, Display, DrmPrimeSurfaceDescriptor,
        H264PicFields, H264SeqFields, IQMatrix, IQMatrixBufferH264, Picture, PictureH264,
        PictureParameter, PictureParameterBufferH264, SliceParameter,
        SliceParameterBufferH264, Surface, UsageHint, VA_FOURCC_NV12, VA_INVALID_ID,
        VA_PICTURE_H264_INVALID, VA_PICTURE_H264_LONG_TERM_REFERENCE,
        VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RT_FORMAT_YUV420,
        VA_SLICE_DATA_FLAG_ALL, VAConfigAttrib, VAConfigAttribType, VAEntrypoint,
        VAProfile,
    };
    use smelter_render::{
        DmaBufFrame, DmaBufLayer, DmaBufObject, DmaBufPlane, Frame, FrameData, Resolution,
    };
    use tracing::{debug, info, trace, warn};
    use vk_video::{
        parameters::MissedFrameHandling,
        parser::{
            decoder_instructions::{DecoderInstruction, compile_to_decoder_instructions},
            h264::{
                H264Parser,
                nal_types::{
                    pps::PicParameterSet,
                    slice::{
                        DecRefPicMarking, FieldPic, MemoryManagementControlOperation,
                        NumRefIdxActive, PredWeightTable, SliceFamily, SliceHeader,
                    },
                    sps::{
                        ChromaFormat, FrameMbsFlags, PicOrderCntType, Profile,
                        ScalingList, SeqParameterSet,
                    },
                },
            },
            reference_manager::{
                DecodeInformation, ReferenceContext, ReferenceId, ReferencePictureInfo,
            },
        },
    };

    use crate::{
        pipeline::{
            decoder::{
                EncodedInputEvent, KeyframeRequestSender, VideoDecoder,
                VideoDecoderInstance,
            },
            vaapi::open_display,
        },
        prelude::*,
    };

    const RETAINED_SURFACE_COUNT: usize = 64;

    pub struct VaapiH264Decoder {
        display: Rc<Display>,
        session: Option<VaapiDecodeSession>,
        parser: H264Parser,
        reference_ctx: ReferenceContext,
        references: HashMap<ReferenceId, DecodedReference>,
        free_surfaces: Vec<Surface<()>>,
        retained_surfaces: VecDeque<Surface<()>>,
        imported_frames: HashMap<libva::VASurfaceID, Arc<DmaBufFrame>>,
        sps: HashMap<u8, SeqParameterSet>,
        pps: HashMap<u8, PicParameterSet>,
        device: Arc<wgpu::Device>,
        keyframe_request_sender: Option<KeyframeRequestSender>,
        drop_frames: bool,
    }

    impl VideoDecoder for VaapiH264Decoder {
        const LABEL: &'static str = "VA-API H264 decoder";

        fn new(
            ctx: &Arc<PipelineCtx>,
            keyframe_request_sender: Option<KeyframeRequestSender>,
        ) -> Result<Self, DecoderInitError> {
            info!("Initializing VA-API H264 decoder");
            Self::new_with_device(
                Arc::clone(&ctx.graphics_context.device),
                keyframe_request_sender,
            )
            .map_err(DecoderInitError::VaapiH264DecoderUnavailable)
        }
    }

    impl VaapiH264Decoder {
        fn new_with_device(
            device: Arc<wgpu::Device>,
            keyframe_request_sender: Option<KeyframeRequestSender>,
        ) -> Result<Self, String> {
            let display = open_display()?;
            Ok(Self {
                display,
                session: None,
                parser: H264Parser::default(),
                reference_ctx: ReferenceContext::new(MissedFrameHandling::Strict),
                references: HashMap::new(),
                free_surfaces: Vec::new(),
                retained_surfaces: VecDeque::with_capacity(RETAINED_SURFACE_COUNT),
                imported_frames: HashMap::new(),
                sps: HashMap::new(),
                pps: HashMap::new(),
                device,
                keyframe_request_sender,
                drop_frames: false,
            })
        }
    }

    impl VideoDecoderInstance for VaapiH264Decoder {
        fn decode(&mut self, event: EncodedInputEvent) -> Vec<Frame> {
            trace!(?event, "VA-API H264 decoder received an event.");
            let instructions = match event {
                EncodedInputEvent::Chunk(chunk) => {
                    self.drop_frames = !chunk.present;
                    if MediaKind::Video(VideoCodec::H264) != chunk.kind {
                        warn!(
                            "VA-API H264 decoder received unsupported kind {:?}",
                            chunk.kind
                        );
                        return Vec::new();
                    }
                    self.parse_h264(&chunk.data, Some(duration_micros(chunk.pts)))
                }
                EncodedInputEvent::LostData => {
                    self.reference_ctx.mark_missed_frames();
                    self.request_keyframe();
                    return Vec::new();
                }
                EncodedInputEvent::AuDelimiter => self.flush_parser(),
            };

            match instructions {
                Ok(instructions) => self.process_instructions(instructions),
                Err(err) => {
                    self.request_keyframe();
                    debug!("VA-API H264 parser/reference error: {err}");
                    Vec::new()
                }
            }
        }

        fn flush(&mut self) -> Vec<Frame> {
            match self.flush_parser() {
                Ok(instructions) => self.process_instructions(instructions),
                Err(err) => {
                    warn!("Failed to flush VA-API H264 parser: {err}");
                    Vec::new()
                }
            }
        }
    }

    impl VaapiH264Decoder {
        fn parse_h264(
            &mut self,
            data: &[u8],
            pts: Option<u64>,
        ) -> Result<Vec<DecoderInstruction>, String> {
            let access_units =
                self.parser.parse(data, pts).map_err(|err| err.to_string())?;
            compile_to_decoder_instructions(&mut self.reference_ctx, access_units)
                .map_err(|err| err.to_string())
        }

        fn flush_parser(&mut self) -> Result<Vec<DecoderInstruction>, String> {
            let access_units = self.parser.flush().map_err(|err| err.to_string())?;
            compile_to_decoder_instructions(&mut self.reference_ctx, access_units)
                .map_err(|err| err.to_string())
        }

        fn process_instructions(
            &mut self,
            instructions: Vec<DecoderInstruction>,
        ) -> Vec<Frame> {
            let mut frames = Vec::new();
            for instruction in instructions {
                let result = match instruction {
                    DecoderInstruction::Sps(sps) => self.process_sps(sps).map(|_| None),
                    DecoderInstruction::Pps(pps) => {
                        self.pps.insert(pps.pic_parameter_set_id.id(), pps);
                        Ok(None)
                    }
                    DecoderInstruction::Idr { decode_info, reference_id } => {
                        self.retain_references();
                        self.decode_picture(&decode_info, reference_id)
                    }
                    DecoderInstruction::Decode { decode_info, reference_id } => {
                        self.decode_picture(&decode_info, reference_id)
                    }
                    DecoderInstruction::Drop { reference_ids } => {
                        for reference_id in reference_ids {
                            self.drop_reference(reference_id);
                        }
                        Ok(None)
                    }
                };

                match result {
                    Ok(Some(frame)) => frames.push(frame),
                    Ok(None) => {}
                    Err(err) => {
                        self.request_keyframe();
                        warn!("VA-API H264 decode error: {err}");
                        break;
                    }
                }
            }
            frames
        }

        fn process_sps(&mut self, sps: SeqParameterSet) -> Result<(), String> {
            let stream = VaapiStreamInfo::from_sps(&sps)?;
            if self.session.as_ref().is_none_or(|session| !session.matches(&stream)) {
                self.session = Some(VaapiDecodeSession::new(&self.display, stream)?);
                self.references.clear();
                self.free_surfaces.clear();
                self.retained_surfaces.clear();
                self.imported_frames.clear();
            }
            self.sps.insert(sps.id().id(), sps);
            Ok(())
        }

        fn decode_picture(
            &mut self,
            decode_info: &DecodeInformation,
            reference_id: ReferenceId,
        ) -> Result<Option<Frame>, String> {
            let session = self
                .session
                .as_ref()
                .ok_or_else(|| "missing VA-API decode session".to_string())?;
            let context = Rc::clone(&session.context);
            let coded_resolution = session.coded_resolution;
            let display_resolution = session.display_resolution;
            let sps = self
                .sps
                .get(&decode_info.sps_id)
                .ok_or_else(|| format!("unknown SPS id {}", decode_info.sps_id))?
                .clone();
            let pps = self
                .pps
                .get(&decode_info.pps_id)
                .ok_or_else(|| format!("unknown PPS id {}", decode_info.pps_id))?
                .clone();
            validate_progressive(&decode_info.header)?;

            let surface = self.take_surface(coded_resolution)?;
            let buffers =
                self.create_buffers(&context, &surface, decode_info, &sps, &pps)?;
            let mut picture =
                Picture::new(decode_info.pts.unwrap_or_default(), context, surface);
            for buffer in buffers {
                picture.add_buffer(buffer);
            }

            let picture = picture
                .begin()
                .map_err(|err| format!("failed to begin VA-API picture: {err}"))?
                .render()
                .map_err(|err| format!("failed to render VA-API picture: {err}"))?
                .end()
                .map_err(|err| format!("failed to end VA-API picture: {err}"))?
                .sync()
                .map_err(|(err, _)| format!("failed to sync VA-API picture: {err}"))?;
            let surface = picture
                .take_surface()
                .map_err(|_| "VA-API picture kept a shared output surface".to_string())?;

            let frame = (!self.drop_frames)
                .then(|| {
                    self.frame_from_surface(&surface, display_resolution, decode_info.pts)
                })
                .transpose()?;
            self.references.insert(
                reference_id,
                DecodedReference {
                    surface,
                    picture: DecodedPictureInfo::from_decode_info(decode_info),
                },
            );
            Ok(frame)
        }

        fn create_buffers(
            &self,
            context: &Rc<Context>,
            surface: &Surface<()>,
            decode_info: &DecodeInformation,
            sps: &SeqParameterSet,
            pps: &PicParameterSet,
        ) -> Result<Vec<Buffer>, String> {
            [
                self.picture_parameter(surface.id(), decode_info, sps, pps)?,
                iq_matrix_parameter(sps, pps),
                self.slice_parameter(decode_info, sps, pps)?,
                BufferType::SliceData(decode_info.slice_data.clone()),
            ]
            .into_iter()
            .map(|buffer| {
                context
                    .create_buffer(buffer)
                    .map_err(|err| format!("failed to create VA-API buffer: {err}"))
            })
            .collect()
        }

        fn picture_parameter(
            &self,
            surface_id: libva::VASurfaceID,
            decode_info: &DecodeInformation,
            sps: &SeqParameterSet,
            pps: &PicParameterSet,
        ) -> Result<BufferType, String> {
            let seq_fields = H264SeqFields::new(
                chroma_format_idc(sps),
                sps.chroma_info.separate_colour_plane_flag as u32,
                sps.gaps_in_frame_num_value_allowed_flag as u32,
                matches!(&sps.frame_mbs_flags, FrameMbsFlags::Frames) as u32,
                mb_adaptive_frame_field_flag(sps) as u32,
                sps.direct_8x8_inference_flag as u32,
                (sps.level_idc >= 31) as u32,
                sps.log2_max_frame_num_minus4.into(),
                pic_order_cnt_type(sps),
                log2_max_pic_order_cnt_lsb_minus4(sps).into(),
                delta_pic_order_always_zero_flag(sps) as u32,
            );
            let pic_fields = H264PicFields::new(
                pps.entropy_coding_mode_flag as u32,
                pps.weighted_pred_flag as u32,
                pps.weighted_bipred_idc.into(),
                transform_8x8_mode_flag(pps) as u32,
                matches!(&decode_info.header.field_pic, FieldPic::Field(_)) as u32,
                pps.constrained_intra_pred_flag as u32,
                pps.bottom_field_pic_order_in_frame_present_flag as u32,
                pps.deblocking_filter_control_present_flag as u32,
                pps.redundant_pic_cnt_present_flag as u32,
                decode_info.header.dec_ref_pic_marking.is_some() as u32,
            );
            let picture_height_in_mbs_minus1 =
                picture_height_in_mbs_minus1(sps).try_into().map_err(|_| {
                    "H264 picture height does not fit VA-API fields".to_string()
                })?;
            if pps.slice_groups.is_some() {
                return Err(
                    "H264 flexible macroblock ordering is not supported by VA-API".into(),
                );
            }

            let pic_param = PictureParameterBufferH264::new(
                current_picture(surface_id, decode_info),
                self.reference_frames(),
                sps.pic_width_in_mbs_minus1.try_into().map_err(|_| {
                    "H264 picture width does not fit VA-API fields".to_string()
                })?,
                picture_height_in_mbs_minus1,
                sps.chroma_info.bit_depth_luma_minus8,
                sps.chroma_info.bit_depth_chroma_minus8,
                sps.max_num_ref_frames.try_into().unwrap_or(u8::MAX),
                &seq_fields,
                0,
                0,
                0,
                pps.pic_init_qp_minus26.try_into().unwrap_or(0),
                pps.pic_init_qs_minus26.try_into().unwrap_or(0),
                pps.chroma_qp_index_offset.try_into().unwrap_or(0),
                second_chroma_qp_index_offset(pps).try_into().unwrap_or(0),
                &pic_fields,
                decode_info.header.frame_num,
            );
            Ok(BufferType::PictureParameter(PictureParameter::H264(pic_param)))
        }

        fn slice_parameter(
            &self,
            decode_info: &DecodeInformation,
            sps: &SeqParameterSet,
            pps: &PicParameterSet,
        ) -> Result<BufferType, String> {
            if decode_info.slice_headers.len() != decode_info.slice_data_indices.len()
                || decode_info.slice_headers.len()
                    != decode_info.slice_header_bit_sizes.len()
            {
                return Err("H264 slice metadata is inconsistent".into());
            }

            let mut slices = SliceParameterBufferH264::new_array();
            for (index, header) in decode_info.slice_headers.iter().enumerate() {
                let ref_pic_list_0 =
                    self.reference_list(decode_info.reference_list_l0.as_deref())?;
                let ref_pic_list_1 =
                    self.reference_list(decode_info.reference_list_l1.as_deref())?;
                let offset = decode_info.slice_data_indices[index];
                let next_offset = decode_info
                    .slice_data_indices
                    .get(index + 1)
                    .copied()
                    .unwrap_or(decode_info.slice_data.len());
                let (weights, denominators) = prediction_weights(header, sps, pps);
                slices.add_slice_parameter(
                    (next_offset - offset).try_into().unwrap_or(u32::MAX),
                    offset.try_into().unwrap_or(u32::MAX),
                    VA_SLICE_DATA_FLAG_ALL,
                    8 + decode_info.slice_header_bit_sizes[index],
                    header.first_mb_in_slice.try_into().unwrap_or(u16::MAX),
                    slice_type(header),
                    header.direct_spatial_mv_pred_flag.unwrap_or(false) as u8,
                    num_ref_idx_l0_active_minus1(header, pps).try_into().unwrap_or(31),
                    num_ref_idx_l1_active_minus1(header, pps).try_into().unwrap_or(31),
                    header.cabac_init_idc.unwrap_or(0).try_into().unwrap_or(0),
                    header.slice_qp_delta.try_into().unwrap_or(0),
                    header.disable_deblocking_filter_idc,
                    0,
                    0,
                    ref_pic_list_0,
                    ref_pic_list_1,
                    denominators.luma,
                    denominators.chroma,
                    weights.luma_l0_flag,
                    weights.luma_l0,
                    weights.luma_offset_l0,
                    weights.chroma_l0_flag,
                    weights.chroma_l0,
                    weights.chroma_offset_l0,
                    weights.luma_l1_flag,
                    weights.luma_l1,
                    weights.luma_offset_l1,
                    weights.chroma_l1_flag,
                    weights.chroma_l1,
                    weights.chroma_offset_l1,
                );
            }
            Ok(BufferType::SliceParameter(SliceParameter::H264(slices)))
        }

        fn reference_frames(&self) -> [PictureH264; 16] {
            let mut refs = self.references.iter().collect::<Vec<_>>();
            refs.sort_by_key(|(id, _)| **id);
            let mut pictures = invalid_h264_pictures::<16>();
            for (slot, (_, reference)) in refs.into_iter().take(16).enumerate() {
                pictures[slot] = reference.picture.to_va_picture(reference.surface.id());
            }
            pictures
        }

        fn reference_list(
            &self,
            references: Option<&[ReferencePictureInfo]>,
        ) -> Result<[PictureH264; 32], String> {
            let mut pictures = invalid_h264_pictures::<32>();
            for (slot, reference) in references.unwrap_or(&[]).iter().take(32).enumerate()
            {
                let surface = self.references.get(&reference.id).ok_or_else(|| {
                    format!("missing VA-API H264 reference {:?}", reference.id)
                })?;
                pictures[slot] = reference_picture(reference, surface.surface.id());
            }
            Ok(pictures)
        }

        fn take_surface(
            &mut self,
            resolution: Resolution,
        ) -> Result<Surface<()>, String> {
            if let Some(surface) = self.free_surfaces.pop() {
                return Ok(surface);
            }
            self.display
                .create_surfaces(
                    VA_RT_FORMAT_YUV420,
                    Some(VA_FOURCC_NV12),
                    resolution.width as u32,
                    resolution.height as u32,
                    Some(UsageHint::USAGE_HINT_DECODER),
                    vec![()],
                )
                .map_err(|err| format!("failed to create VA-API decode surface: {err}"))?
                .pop()
                .ok_or_else(|| "VA-API returned no decode surface".to_string())
        }

        fn frame_from_surface(
            &mut self,
            surface: &Surface<()>,
            resolution: Resolution,
            pts: Option<u64>,
        ) -> Result<Frame, String> {
            let dmabuf = self.dmabuf_for_surface(surface)?;
            Ok(Frame {
                data: FrameData::Nv12DmaBuf(dmabuf),
                pts: Duration::from_micros(pts.unwrap_or_default()),
                resolution,
            })
        }

        fn dmabuf_for_surface(
            &mut self,
            surface: &Surface<()>,
        ) -> Result<Arc<DmaBufFrame>, String> {
            if let Some(dmabuf) = self.imported_frames.get(&surface.id()) {
                return Ok(Arc::clone(dmabuf));
            }

            let descriptor = surface
                .export_prime()
                .map_err(|err| format!("failed to export VA surface: {err}"))?;
            let dmabuf = import_vaapi_frame(&self.device, descriptor)?;
            self.imported_frames.insert(surface.id(), Arc::clone(&dmabuf));
            Ok(dmabuf)
        }

        fn drop_reference(&mut self, reference_id: ReferenceId) {
            let Some(reference) = self.references.remove(&reference_id) else {
                warn!(
                    "VA-API H264 decoder tried to drop missing reference {reference_id:?}"
                );
                return;
            };
            self.retain_surface(reference.surface);
        }

        fn retain_references(&mut self) {
            let references = std::mem::take(&mut self.references);
            for (_, reference) in references {
                self.retain_surface(reference.surface);
            }
        }

        fn retain_surface(&mut self, surface: Surface<()>) {
            self.retained_surfaces.push_back(surface);
            while self.retained_surfaces.len() > RETAINED_SURFACE_COUNT {
                if let Some(surface) = self.retained_surfaces.pop_front() {
                    self.free_surfaces.push(surface);
                }
            }
        }

        fn request_keyframe(&self) {
            if let Some(sender) = self.keyframe_request_sender.as_ref() {
                sender.send();
            }
        }
    }

    struct VaapiDecodeSession {
        _config: Config,
        context: Rc<Context>,
        profile: VAProfile::Type,
        rt_format: u32,
        coded_resolution: Resolution,
        display_resolution: Resolution,
    }

    impl VaapiDecodeSession {
        fn new(display: &Rc<Display>, stream: VaapiStreamInfo) -> Result<Self, String> {
            let entrypoints =
                display.query_config_entrypoints(stream.profile).map_err(|err| {
                    format!("failed to query VA-API H264 entrypoints: {err}")
                })?;
            if !entrypoints.contains(&VAEntrypoint::VAEntrypointVLD) {
                return Err("VA-API H264 VLD entrypoint is unavailable".into());
            }
            let config = display
                .create_config(
                    vec![VAConfigAttrib {
                        type_: VAConfigAttribType::VAConfigAttribRTFormat,
                        value: stream.rt_format,
                    }],
                    stream.profile,
                    VAEntrypoint::VAEntrypointVLD,
                )
                .map_err(|err| format!("failed to create VA-API H264 config: {err}"))?;
            let context = display
                .create_context::<()>(
                    &config,
                    stream.coded_resolution.width as u32,
                    stream.coded_resolution.height as u32,
                    None,
                    true,
                )
                .map_err(|err| format!("failed to create VA-API H264 context: {err}"))?;

            Ok(Self {
                _config: config,
                context,
                profile: stream.profile,
                rt_format: stream.rt_format,
                coded_resolution: stream.coded_resolution,
                display_resolution: stream.display_resolution,
            })
        }

        fn matches(&self, stream: &VaapiStreamInfo) -> bool {
            self.profile == stream.profile
                && self.rt_format == stream.rt_format
                && self.coded_resolution == stream.coded_resolution
                && self.display_resolution == stream.display_resolution
        }
    }

    struct VaapiStreamInfo {
        profile: VAProfile::Type,
        rt_format: u32,
        coded_resolution: Resolution,
        display_resolution: Resolution,
    }

    impl VaapiStreamInfo {
        fn from_sps(sps: &SeqParameterSet) -> Result<Self, String> {
            if !matches!(&sps.frame_mbs_flags, FrameMbsFlags::Frames) {
                return Err(
                    "interlaced H264 streams are not supported by this VA-API decoder"
                        .into(),
                );
            }
            let profile = va_profile(sps)?;
            let rt_format = va_rt_format(sps)?;
            let coded_resolution = Resolution {
                width: ((sps.pic_width_in_mbs_minus1 + 1) * 16) as usize,
                height: ((sps.pic_height_in_map_units_minus1 + 1) * 16) as usize,
            };
            let (width, height) = sps
                .pixel_dimensions()
                .map_err(|err| format!("invalid H264 display dimensions: {err:?}"))?;
            Ok(Self {
                profile,
                rt_format,
                coded_resolution,
                display_resolution: Resolution {
                    width: width as usize,
                    height: height as usize,
                },
            })
        }
    }

    struct DecodedReference {
        surface: Surface<()>,
        picture: DecodedPictureInfo,
    }

    #[derive(Clone, Copy)]
    struct DecodedPictureInfo {
        frame_num: u16,
        pic_order_cnt: [i32; 2],
        long_term_pic_num: Option<u64>,
    }

    impl DecodedPictureInfo {
        fn from_decode_info(decode_info: &DecodeInformation) -> Self {
            Self {
                frame_num: decode_info.picture_info.FrameNum,
                pic_order_cnt: decode_info.picture_info.PicOrderCnt_as_reference_pic,
                long_term_pic_num: current_long_term_pic_num(decode_info),
            }
        }

        fn to_va_picture(self, surface_id: libva::VASurfaceID) -> PictureH264 {
            let flags = if self.long_term_pic_num.is_some() {
                VA_PICTURE_H264_LONG_TERM_REFERENCE
            } else {
                VA_PICTURE_H264_SHORT_TERM_REFERENCE
            };
            PictureH264::new(
                surface_id,
                self.long_term_pic_num.unwrap_or(self.frame_num.into()) as u32,
                flags,
                self.pic_order_cnt[0],
                self.pic_order_cnt[1],
            )
        }
    }

    fn current_picture(
        surface_id: libva::VASurfaceID,
        decode_info: &DecodeInformation,
    ) -> PictureH264 {
        let long_term_pic_num = current_long_term_pic_num(decode_info);
        let flags = if long_term_pic_num.is_some() {
            VA_PICTURE_H264_LONG_TERM_REFERENCE
        } else if decode_info.header.dec_ref_pic_marking.is_some() {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        } else {
            0
        };
        PictureH264::new(
            surface_id,
            long_term_pic_num.unwrap_or(decode_info.picture_info.FrameNum.into()) as u32,
            flags,
            decode_info.picture_info.PicOrderCnt_for_decoding[0],
            decode_info.picture_info.PicOrderCnt_for_decoding[1],
        )
    }

    fn reference_picture(
        reference: &ReferencePictureInfo,
        surface_id: libva::VASurfaceID,
    ) -> PictureH264 {
        let flags = if reference.LongTermPicNum.is_some() {
            VA_PICTURE_H264_LONG_TERM_REFERENCE
        } else {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        };
        PictureH264::new(
            surface_id,
            reference.LongTermPicNum.unwrap_or(reference.FrameNum.into()) as u32,
            flags,
            reference.PicOrderCnt[0],
            reference.PicOrderCnt[1],
        )
    }

    fn current_long_term_pic_num(decode_info: &DecodeInformation) -> Option<u64> {
        match decode_info.header.dec_ref_pic_marking.as_ref()? {
            DecRefPicMarking::Idr { long_term_reference_flag, .. } => {
                (*long_term_reference_flag).then_some(0)
            }
            DecRefPicMarking::Adaptive(operations) => operations.iter().find_map(|op| {
                if let MemoryManagementControlOperation::CurrentUsedForLongTerm {
                    long_term_frame_idx,
                } = op
                {
                    Some((*long_term_frame_idx).into())
                } else {
                    None
                }
            }),
            DecRefPicMarking::SlidingWindow => None,
        }
    }

    fn va_profile(sps: &SeqParameterSet) -> Result<VAProfile::Type, String> {
        match sps.profile() {
            Profile::Baseline if sps.constraint_flags.flag0() => {
                Ok(VAProfile::VAProfileH264ConstrainedBaseline)
            }
            Profile::Baseline => {
                Err("unsupported unconstrained H264 Baseline profile".into())
            }
            Profile::Main => Ok(VAProfile::VAProfileH264Main),
            Profile::Extended if sps.constraint_flags.flag1() => {
                Ok(VAProfile::VAProfileH264Main)
            }
            Profile::Extended => {
                Err("unsupported unconstrained H264 Extended profile".into())
            }
            Profile::High | Profile::High10 | Profile::High422 => {
                Ok(VAProfile::VAProfileH264High)
            }
            profile => Err(format!("unsupported H264 profile {profile:?}")),
        }
    }

    fn va_rt_format(sps: &SeqParameterSet) -> Result<u32, String> {
        match (sps.chroma_info.bit_depth_luma_minus8 + 8, sps.chroma_info.chroma_format) {
            (8, ChromaFormat::Monochrome | ChromaFormat::YUV420) => {
                Ok(VA_RT_FORMAT_YUV420)
            }
            (depth, format) => Err(format!(
                "unsupported H264 VA-API surface format: {depth}-bit {format:?}"
            )),
        }
    }

    fn validate_progressive(header: &SliceHeader) -> Result<(), String> {
        if matches!(&header.field_pic, FieldPic::Frame) {
            Ok(())
        } else {
            Err("interlaced H264 slices are not supported by this VA-API decoder".into())
        }
    }

    fn chroma_format_idc(sps: &SeqParameterSet) -> u32 {
        match sps.chroma_info.chroma_format {
            ChromaFormat::Monochrome => 0,
            ChromaFormat::YUV420 => 1,
            ChromaFormat::YUV422 => 2,
            ChromaFormat::YUV444 => 3,
            ChromaFormat::Invalid(value) => value,
        }
    }

    fn mb_adaptive_frame_field_flag(sps: &SeqParameterSet) -> bool {
        match &sps.frame_mbs_flags {
            FrameMbsFlags::Frames => false,
            FrameMbsFlags::Fields { mb_adaptive_frame_field_flag } => {
                *mb_adaptive_frame_field_flag
            }
        }
    }

    fn pic_order_cnt_type(sps: &SeqParameterSet) -> u32 {
        match &sps.pic_order_cnt {
            PicOrderCntType::TypeZero { .. } => 0,
            PicOrderCntType::TypeOne { .. } => 1,
            PicOrderCntType::TypeTwo => 2,
        }
    }

    fn log2_max_pic_order_cnt_lsb_minus4(sps: &SeqParameterSet) -> u8 {
        match &sps.pic_order_cnt {
            PicOrderCntType::TypeZero { log2_max_pic_order_cnt_lsb_minus4 } => {
                *log2_max_pic_order_cnt_lsb_minus4
            }
            _ => 0,
        }
    }

    fn delta_pic_order_always_zero_flag(sps: &SeqParameterSet) -> bool {
        match &sps.pic_order_cnt {
            PicOrderCntType::TypeOne { delta_pic_order_always_zero_flag, .. } => {
                *delta_pic_order_always_zero_flag
            }
            _ => false,
        }
    }

    fn picture_height_in_mbs_minus1(sps: &SeqParameterSet) -> u32 {
        let interlaced = (!matches!(&sps.frame_mbs_flags, FrameMbsFlags::Frames)) as u32;
        ((sps.pic_height_in_map_units_minus1 + 1) << interlaced) - 1
    }

    fn transform_8x8_mode_flag(pps: &PicParameterSet) -> bool {
        pps.extension.as_ref().is_some_and(|extra| extra.transform_8x8_mode_flag)
    }

    fn second_chroma_qp_index_offset(pps: &PicParameterSet) -> i32 {
        pps.extension
            .as_ref()
            .map(|extra| extra.second_chroma_qp_index_offset)
            .unwrap_or(pps.chroma_qp_index_offset)
    }

    fn iq_matrix_parameter(sps: &SeqParameterSet, pps: &PicParameterSet) -> BufferType {
        let mut scaling_list4x4 = [[16; 16]; 6];
        let mut scaling_list8x8 = [[16; 64]; 2];

        if let Some(matrix) = sps.chroma_info.scaling_matrix.as_ref() {
            fill_scaling_4x4(&matrix.scaling_list4x4, &mut scaling_list4x4);
            fill_scaling_8x8(&matrix.scaling_list8x8, &mut scaling_list8x8);
        }
        if let Some(matrix) =
            pps.extension.as_ref().and_then(|extra| extra.pic_scaling_matrix.as_ref())
        {
            fill_scaling_4x4(&matrix.scaling_list4x4, &mut scaling_list4x4);
            if let Some(scaling) = matrix.scaling_list8x8.as_ref() {
                fill_scaling_8x8(scaling, &mut scaling_list8x8);
            }
        }

        BufferType::IQMatrix(IQMatrix::H264(IQMatrixBufferH264::new(
            scaling_list4x4,
            scaling_list8x8,
        )))
    }

    fn fill_scaling_4x4(source: &[ScalingList<16>], target: &mut [[u8; 16]; 6]) {
        for (source, target) in source.iter().zip(target.iter_mut()) {
            if let ScalingList::List(values) = source {
                let zigzag = values.map(|value| value.get());
                get_raster_from_zigzag_4x4(zigzag, target);
            }
        }
    }

    fn fill_scaling_8x8(source: &[ScalingList<64>], target: &mut [[u8; 64]; 2]) {
        for (source, target) in source.iter().zip(target.iter_mut()) {
            if let ScalingList::List(values) = source {
                let zigzag = values.map(|value| value.get());
                get_raster_from_zigzag_8x8(zigzag, target);
            }
        }
    }

    fn get_raster_from_zigzag_4x4(src: [u8; 16], dst: &mut [u8; 16]) {
        const ZIGZAG: [usize; 16] =
            [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
        for i in 0..16 {
            dst[ZIGZAG[i]] = src[i];
        }
    }

    fn get_raster_from_zigzag_8x8(src: [u8; 64], dst: &mut [u8; 64]) {
        const ZIGZAG: [usize; 64] = [
            0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40,
            48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29,
            22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54,
            47, 55, 62, 63,
        ];
        for i in 0..64 {
            dst[ZIGZAG[i]] = src[i];
        }
    }

    #[derive(Clone, Copy)]
    struct PredictionDenominators {
        luma: u8,
        chroma: u8,
    }

    #[derive(Clone, Copy)]
    struct PredictionWeights {
        luma_l0_flag: u8,
        luma_l0: [i16; 32],
        luma_offset_l0: [i16; 32],
        chroma_l0_flag: u8,
        chroma_l0: [[i16; 2]; 32],
        chroma_offset_l0: [[i16; 2]; 32],
        luma_l1_flag: u8,
        luma_l1: [i16; 32],
        luma_offset_l1: [i16; 32],
        chroma_l1_flag: u8,
        chroma_l1: [[i16; 2]; 32],
        chroma_offset_l1: [[i16; 2]; 32],
    }

    fn prediction_weights(
        header: &SliceHeader,
        sps: &SeqParameterSet,
        pps: &PicParameterSet,
    ) -> (PredictionWeights, PredictionDenominators) {
        let mut weights = PredictionWeights {
            luma_l0_flag: 0,
            luma_l0: [0; 32],
            luma_offset_l0: [0; 32],
            chroma_l0_flag: 0,
            chroma_l0: [[0; 2]; 32],
            chroma_offset_l0: [[0; 2]; 32],
            luma_l1_flag: 0,
            luma_l1: [0; 32],
            luma_offset_l1: [0; 32],
            chroma_l1_flag: 0,
            chroma_l1: [[0; 2]; 32],
            chroma_offset_l1: [[0; 2]; 32],
        };
        let Some(table) = header.pred_weight_table.as_ref() else {
            return (weights, PredictionDenominators { luma: 0, chroma: 0 });
        };

        fill_l0_prediction_weights(&mut weights, table);
        if sps.chroma_info.chroma_format != ChromaFormat::Monochrome {
            fill_l0_chroma_prediction_weights(&mut weights, table);
        }

        if pps.weighted_pred_flag && matches!(&header.slice_type.family, SliceFamily::P) {
            weights.luma_l0_flag = 1;
            weights.chroma_l0_flag =
                (sps.chroma_info.chroma_format != ChromaFormat::Monochrome) as u8;
        }

        (
            weights,
            PredictionDenominators {
                luma: table.luma_log2_weight_denom.try_into().unwrap_or(u8::MAX),
                chroma: table
                    .chroma_log2_weight_denom
                    .unwrap_or_default()
                    .try_into()
                    .unwrap_or(u8::MAX),
            },
        )
    }

    fn fill_l0_prediction_weights(
        weights: &mut PredictionWeights,
        table: &PredWeightTable,
    ) {
        let default_weight =
            1i16.checked_shl(table.luma_log2_weight_denom).unwrap_or_default();
        for (index, weight) in table.luma_weights.iter().take(32).enumerate() {
            match weight {
                Some(weight) => {
                    weights.luma_l0[index] = weight.weight.try_into().unwrap_or(0);
                    weights.luma_offset_l0[index] = weight.offset.try_into().unwrap_or(0);
                }
                None => {
                    weights.luma_l0[index] = default_weight;
                }
            }
        }
    }

    fn fill_l0_chroma_prediction_weights(
        weights: &mut PredictionWeights,
        table: &PredWeightTable,
    ) {
        let default_weight = 1i16
            .checked_shl(table.chroma_log2_weight_denom.unwrap_or_default())
            .unwrap_or_default();
        for (index, chroma_weights) in table.chroma_weights.iter().take(32).enumerate() {
            for component in 0..2 {
                if let Some(weight) = chroma_weights.get(component) {
                    weights.chroma_l0[index][component] =
                        weight.weight.try_into().unwrap_or(0);
                    weights.chroma_offset_l0[index][component] =
                        weight.offset.try_into().unwrap_or(0);
                } else {
                    weights.chroma_l0[index][component] = default_weight;
                }
            }
        }
    }

    fn num_ref_idx_l0_active_minus1(header: &SliceHeader, pps: &PicParameterSet) -> u32 {
        header
            .num_ref_idx_active
            .as_ref()
            .map(|num| match num {
                NumRefIdxActive::P { num_ref_idx_l0_active_minus1 }
                | NumRefIdxActive::B { num_ref_idx_l0_active_minus1, .. } => {
                    *num_ref_idx_l0_active_minus1
                }
            })
            .unwrap_or(pps.num_ref_idx_l0_default_active_minus1)
    }

    fn num_ref_idx_l1_active_minus1(header: &SliceHeader, pps: &PicParameterSet) -> u32 {
        header
            .num_ref_idx_active
            .as_ref()
            .and_then(|num| match num {
                NumRefIdxActive::B { num_ref_idx_l1_active_minus1, .. } => {
                    Some(*num_ref_idx_l1_active_minus1)
                }
                NumRefIdxActive::P { .. } => None,
            })
            .unwrap_or(pps.num_ref_idx_l1_default_active_minus1)
    }

    fn slice_type(header: &SliceHeader) -> u8 {
        match &header.slice_type.family {
            SliceFamily::P => 0,
            SliceFamily::B => 1,
            SliceFamily::I => 2,
            SliceFamily::SP => 3,
            SliceFamily::SI => 4,
        }
    }

    fn invalid_h264_pictures<const N: usize>() -> [PictureH264; N] {
        std::array::from_fn(|_| {
            PictureH264::new(VA_INVALID_ID, 0, VA_PICTURE_H264_INVALID, 0, 0)
        })
    }

    fn import_vaapi_frame(
        device: &wgpu::Device,
        descriptor: DrmPrimeSurfaceDescriptor,
    ) -> Result<Arc<DmaBufFrame>, String> {
        let fourcc = descriptor.fourcc;
        let width = descriptor.width;
        let height = descriptor.height;
        let objects = descriptor
            .objects
            .into_iter()
            .map(|object| DmaBufObject {
                fd: Arc::new(object.fd),
                size: object.size,
                modifier: object.drm_format_modifier,
            })
            .collect::<Vec<_>>();
        let layers = descriptor
            .layers
            .into_iter()
            .map(|layer| DmaBufLayer {
                drm_format: layer.drm_format,
                planes: (0..layer.num_planes as usize)
                    .map(|index| DmaBufPlane {
                        object_index: layer.object_index[index] as usize,
                        offset: layer.offset[index],
                        pitch: layer.pitch[index],
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        smelter_render::import_nv12_dmabuf_texture(
            device, fourcc, width, height, objects, layers,
        )
    }

    fn duration_micros(duration: Duration) -> u64 {
        duration.as_micros().try_into().unwrap_or(u64::MAX)
    }

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use std::{
            fs,
            path::{Path, PathBuf},
            process::Command,
            sync::Mutex,
            time::{Duration, SystemTime, UNIX_EPOCH},
        };

        use bytes::Bytes;

        use super::*;
        use crate::{
            graphics_context::{GraphicsContext, GraphicsContextOptions},
            pipeline::{
                decoder::BytestreamTransformer,
                utils::{H264AvcDecoderConfig, H264AvccToAnnexB},
            },
        };

        const TEST_WIDTH: usize = 64;
        const TEST_HEIGHT: usize = 64;
        const TEST_FRAME_COUNT: usize = 4;
        static VAAPI_TEST_LOCK: Mutex<()> = Mutex::new(());

        #[test]
        #[ignore = "requires ffmpeg and a VA-API capable Linux host"]
        fn decodes_ffmpeg_annexb_stream_to_nv12_dmabuf_frames() {
            let _guard = VAAPI_TEST_LOCK.lock().unwrap();
            let video = GeneratedVideo::new("stream.h264", "h264");
            let stream = fs::read(&video.path).expect("failed to read generated stream");
            let mut decoder = test_decoder();

            let mut frames =
                decoder.decode(EncodedInputEvent::Chunk(EncodedInputChunk {
                    data: Bytes::from(stream),
                    pts: Duration::ZERO,
                    dts: None,
                    kind: MediaKind::Video(VideoCodec::H264),
                    present: true,
                }));
            frames.extend(decoder.decode(EncodedInputEvent::AuDelimiter));
            frames.extend(decoder.flush());

            assert_eq!(frames.len(), TEST_FRAME_COUNT);
            for frame in frames {
                assert_eq!(
                    frame.resolution,
                    Resolution { width: TEST_WIDTH, height: TEST_HEIGHT }
                );
                let FrameData::Nv12DmaBuf(frame) = frame.data else {
                    panic!("expected NV12 DMA-BUF frame");
                };
                assert_eq!(frame.fourcc(), u32::from_le_bytes(*b"NV12"));
                assert_eq!(
                    frame.resolution(),
                    Resolution { width: TEST_WIDTH, height: TEST_HEIGHT }
                );
                assert_eq!(frame.layers().len(), 1);
                assert_eq!(frame.layers()[0].planes.len(), 2);
            }
        }

        #[test]
        #[ignore = "requires ffmpeg and a VA-API capable Linux host"]
        fn decodes_ffmpeg_mp4_samples_to_nv12_dmabuf_frames() {
            let _guard = VAAPI_TEST_LOCK.lock().unwrap();
            let video = GeneratedVideo::new("stream.mp4", "mp4");
            let (config, samples) = read_mp4_h264_samples(&video.path);
            assert_eq!(samples.len(), TEST_FRAME_COUNT);

            let mut transformer = H264AvccToAnnexB::new(config);
            let mut decoder = test_decoder();
            let mut frames = Vec::new();
            for sample in samples {
                frames.extend(decoder.decode(EncodedInputEvent::Chunk(
                    EncodedInputChunk {
                        data: transformer.transform(sample.bytes),
                        pts: sample.pts,
                        dts: Some(sample.dts),
                        kind: MediaKind::Video(VideoCodec::H264),
                        present: true,
                    },
                )));
            }
            frames.extend(decoder.decode(EncodedInputEvent::AuDelimiter));
            frames.extend(decoder.flush());

            assert_eq!(frames.len(), TEST_FRAME_COUNT);
            for frame in frames {
                assert_eq!(
                    frame.resolution,
                    Resolution { width: TEST_WIDTH, height: TEST_HEIGHT }
                );
                assert!(matches!(frame.data, FrameData::Nv12DmaBuf(_)));
            }
        }

        fn test_decoder() -> VaapiH264Decoder {
            let graphics_context = GraphicsContext::new(GraphicsContextOptions {
                force_gpu: true,
                ..Default::default()
            })
            .expect("failed to create WGPU graphics context");
            VaapiH264Decoder::new_with_device(graphics_context.device, None)
                .expect("failed to create VA-API H264 decoder")
        }

        struct GeneratedVideo {
            path: PathBuf,
            dir: PathBuf,
        }

        impl GeneratedVideo {
            fn new(filename: &str, muxer: &str) -> Self {
                let dir = std::env::temp_dir().join(format!(
                    "smelter-vaapi-h264-{}",
                    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
                ));
                fs::create_dir(&dir).expect("failed to create temp dir");
                let path = dir.join(filename);
                generate_video(&path, muxer);
                Self { path, dir }
            }
        }

        impl Drop for GeneratedVideo {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.dir).ok();
            }
        }

        fn generate_video(output: &Path, muxer: &str) {
            let input = format!("testsrc2=size={TEST_WIDTH}x{TEST_HEIGHT}:rate=5");
            let frame_count = TEST_FRAME_COUNT.to_string();
            let status = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    &input,
                    "-frames:v",
                    &frame_count,
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-preset",
                    "ultrafast",
                    "-tune",
                    "zerolatency",
                    "-g",
                    &frame_count,
                    "-bf",
                    "0",
                    "-f",
                    muxer,
                ])
                .arg(output)
                .status()
                .expect("failed to execute ffmpeg");
            assert!(status.success(), "ffmpeg failed with status {status}");
        }

        struct Mp4H264Sample {
            bytes: Bytes,
            pts: Duration,
            dts: Duration,
        }

        fn read_mp4_h264_samples(
            path: &Path,
        ) -> (H264AvcDecoderConfig, Vec<Mp4H264Sample>) {
            let file = fs::File::open(path).expect("failed to open generated MP4");
            let size = file.metadata().expect("failed to stat generated MP4").len();
            let mut reader =
                mp4::Mp4Reader::read_header(file, size).expect("failed to read MP4");
            let (track_id, sample_count, timescale, config) = {
                let (&track_id, track, avc) = reader
                    .tracks()
                    .iter()
                    .find_map(|(id, track)| {
                        let avc = track.avc1_or_3_inner()?;
                        (track.track_type().ok()? == mp4::TrackType::Video
                            && track.media_type().ok()? == mp4::MediaType::H264)
                            .then_some((id, track, avc))
                    })
                    .expect("generated MP4 has no H264 video track");
                let config = H264AvcDecoderConfig {
                    nalu_length_size: avc.avcc.length_size_minus_one as usize + 1,
                    spss: avc
                        .avcc
                        .sequence_parameter_sets
                        .iter()
                        .map(|nalu| Bytes::copy_from_slice(&nalu.bytes))
                        .collect(),
                    ppss: avc
                        .avcc
                        .picture_parameter_sets
                        .iter()
                        .map(|nalu| Bytes::copy_from_slice(&nalu.bytes))
                        .collect(),
                };
                (track_id, track.sample_count(), track.timescale(), config)
            };

            let samples = (1..=sample_count)
                .map(|index| {
                    let sample = reader
                        .read_sample(track_id, index)
                        .expect("failed to read MP4 sample")
                        .expect("missing MP4 sample");
                    let dts = Duration::from_secs_f64(
                        sample.start_time as f64 / timescale as f64,
                    );
                    let pts = Duration::from_secs_f64(
                        (sample.start_time as f64 + sample.rendering_offset as f64)
                            / timescale as f64,
                    );
                    Mp4H264Sample { bytes: sample.bytes, pts, dts }
                })
                .collect();
            (config, samples)
        }
    }
}

#[cfg(feature = "vaapi")]
pub use imp::VaapiH264Decoder;

#[cfg(not(feature = "vaapi"))]
mod imp {
    use std::sync::Arc;

    use smelter_render::Frame;

    use crate::{
        pipeline::decoder::{
            EncodedInputEvent, KeyframeRequestSender, VideoDecoder, VideoDecoderInstance,
        },
        prelude::*,
    };

    pub struct VaapiH264Decoder;

    impl VideoDecoder for VaapiH264Decoder {
        const LABEL: &'static str = "VA-API H264 decoder";

        fn new(
            _ctx: &Arc<PipelineCtx>,
            _keyframe_request_sender: Option<KeyframeRequestSender>,
        ) -> Result<Self, DecoderInitError> {
            Err(DecoderInitError::VaapiH264DecoderUnavailable(
                "support was not compiled into smelter-core".into(),
            ))
        }
    }

    impl VideoDecoderInstance for VaapiH264Decoder {
        fn decode(&mut self, _chunk: EncodedInputEvent) -> Vec<Frame> {
            Vec::new()
        }

        fn flush(&mut self) -> Vec<Frame> {
            Vec::new()
        }
    }
}

#[cfg(not(feature = "vaapi"))]
pub use imp::VaapiH264Decoder;
