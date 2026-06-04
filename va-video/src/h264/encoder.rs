mod imp {
    use std::{
        collections::VecDeque,
        rc::Rc,
        sync::Arc,
        time::{Duration, Instant},
    };

    use bytes::Bytes;
    use libva::{
        BufferType, Config, Context, Display, EncCodedBuffer, EncMiscParameter,
        EncMiscParameterFrameRate, EncMiscParameterRateControl, EncPictureParameter,
        EncPictureParameterBufferH264, EncSequenceParameter,
        EncSequenceParameterBufferH264, EncSliceParameter, EncSliceParameterBufferH264,
        H264EncFrameCropOffsets, H264EncPicFields, H264EncSeqFields, H264VuiFields,
        MappedCodedBuffer, Picture, PictureH264, PictureNew, RcFlags, Surface, UsageHint,
        VA_INVALID_ID, VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RC_CBR,
        VA_RT_FORMAT_YUV420, VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAProfile,
        VASurfaceStatus,
    };
    use smelter_render::{DmaBufFrame, Framerate, Resolution};
    use tracing::{info, warn};

    use crate::display::{
        VaapiDmaBufFrame, duration_micros, invalid_h264_pictures, open_display,
        take_nv12_surface,
    };

    use super::super::parameter_sets::{
        H264_LEVEL_4_0, LOG2_MAX_FRAME_NUM_MINUS4, LOG2_MAX_PIC_ORDER_CNT_LSB_MINUS4,
        main_parameter_sets,
    };

    const RECONSTRUCTED_SURFACE_ALLOCATION_BATCH: usize = 4;
    const DEFAULT_CODED_BUFFER_SIZE: usize = 1_500_000;

    pub struct H264Encoder {
        encoder: IntelVaapiH264Encoder,
        parameter_sets: Bytes,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct H264EncoderConfig {
        pub resolution: Resolution,
        pub bitrate: u32,
        pub gop_size: u16,
        pub framerate: Framerate,
        pub max_pending_frames: usize,
    }

    pub struct EncodedFrame {
        pub data: Bytes,
        pub pts: Duration,
        pub is_keyframe: bool,
    }

    impl H264Encoder {
        pub fn new(config: H264EncoderConfig) -> Result<Self, String> {
            info!("Initializing VA-API H264 encoder");

            let display = open_display()?;
            let parameter_sets = main_parameter_sets(config.resolution, config.framerate);

            let encoder = IntelVaapiH264Encoder::new(
                display,
                config.resolution,
                config.bitrate,
                config.gop_size.max(1),
                config.framerate,
                config.max_pending_frames,
                parameter_sets.clone(),
            )?;

            info!(
                width = config.resolution.width,
                height = config.resolution.height,
                bitrate = config.bitrate,
                max_pending_frames = config.max_pending_frames,
                "Initialized zero-copy VA-API H264 encoder with NV12 DMA-BUF input"
            );

            Ok(Self { encoder, parameter_sets })
        }

        pub fn parameter_sets(&self) -> &Bytes {
            &self.parameter_sets
        }

        pub fn encode(
            &mut self,
            frame: Arc<DmaBufFrame>,
            pts: Duration,
            force_keyframe: bool,
        ) -> Result<Vec<EncodedFrame>, String> {
            self.encoder.encode(frame, pts, force_keyframe)
        }

        pub fn flush(&mut self) -> Result<Vec<EncodedFrame>, String> {
            self.encoder.flush()
        }
    }

    struct IntelVaapiH264Encoder {
        _config: Config,
        context: Rc<Context>,
        display: Rc<Display>,
        free_reconstructed_surfaces: Vec<Surface<()>>,
        pending: VecDeque<PendingEncode>,
        retired_after_producer: Vec<Surface<()>>,
        reference: Option<EncodedReference>,
        resolution: smelter_render::Resolution,
        bitrate: u32,
        gop_size: u16,
        max_pending_frames: usize,
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
            max_pending_frames: usize,
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
                .create_context::<()>(
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
                free_reconstructed_surfaces: Vec::new(),
                pending: VecDeque::new(),
                retired_after_producer: Vec::new(),
                reference: None,
                resolution,
                bitrate,
                gop_size,
                max_pending_frames,
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
        ) -> Result<Vec<EncodedFrame>, String> {
            let mut completed = self.collect_ready()?;
            let pending = self.submit(frame, pts, force_keyframe)?;
            self.pending.push_back(pending);
            while self.pending.len() > self.max_pending_frames {
                completed.push(self.complete_oldest()?);
            }
            Ok(completed)
        }

        fn flush(&mut self) -> Result<Vec<EncodedFrame>, String> {
            let mut completed = self.collect_ready()?;
            while !self.pending.is_empty() {
                completed.push(self.complete_oldest()?);
            }
            Ok(completed)
        }

        fn submit(
            &mut self,
            frame: Arc<DmaBufFrame>,
            pts: Duration,
            force_keyframe: bool,
        ) -> Result<PendingEncode, String> {
            let started_at = Instant::now();
            let input = VaapiDmaBufFrame::new(frame).import_surface(&self.display)?;
            let is_keyframe = force_keyframe
                || self.reference.is_none()
                || self.frames_since_keyframe >= self.gop_size;
            let reconstructed = self.take_reconstructed_surface()?;
            let coded_buffer = self
                .context
                .create_enc_coded(self.coded_buffer_size())
                .map_err(|err| format!("failed to create VA-API coded buffer: {err}"))?;

            let mut picture =
                Picture::new(duration_micros(pts), Rc::clone(&self.context), input);
            self.add_buffers(&mut picture, &coded_buffer, &reconstructed, is_keyframe)?;

            let picture = picture
                .begin()
                .map_err(|err| format!("failed to begin VA-API picture: {err}"))?
                .render()
                .map_err(|err| format!("failed to render VA-API picture: {err}"))?
                .end()
                .map_err(|err| format!("failed to end VA-API picture: {err}"))?;
            let elapsed = started_at.elapsed();
            if elapsed > Duration::from_millis(25) {
                warn!(
                    submit_ms = elapsed.as_millis(),
                    is_keyframe,
                    pts_us = pts.as_micros(),
                    "slow VA-API H264 encode submit"
                );
            }

            let reconstructed_id = reconstructed.id();
            let retired_reference = self.rotate_reference(reconstructed, is_keyframe);
            let retired_after_sync = match (is_keyframe, retired_reference) {
                (false, Some(reference)) => Some(reference.surface),
                (false, None) => None,
                (true, Some(reference))
                    if self.producer_is_pending(reference.surface.id()) =>
                {
                    self.retired_after_producer.push(reference.surface);
                    None
                }
                (true, Some(reference)) => {
                    self.free_reconstructed_surfaces.push(reference.surface);
                    None
                }
                (true, None) => None,
            };

            Ok(PendingEncode {
                picture,
                coded_buffer,
                reconstructed_id,
                retired_after_sync,
                pts,
                is_keyframe,
                submitted_at: started_at,
            })
        }

        fn add_buffers(
            &self,
            picture: &mut Picture<PictureNew, Surface<VaapiDmaBufFrame>>,
            coded_buffer: &EncCodedBuffer,
            reconstructed: &Surface<()>,
            is_keyframe: bool,
        ) -> Result<(), String> {
            for buffer in [
                self.sequence_parameter(),
                self.picture_parameter(coded_buffer, reconstructed, is_keyframe),
                self.slice_parameter(is_keyframe),
                self.rate_control_parameter(),
                self.framerate_parameter(),
            ] {
                let buffer = self
                    .context
                    .create_buffer(buffer)
                    .map_err(|err| format!("failed to create VA-API buffer: {err}"))?;
                picture.add_buffer(buffer);
            }
            Ok(())
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
            take_nv12_surface(
                &self.display,
                &mut self.free_reconstructed_surfaces,
                self.resolution,
                UsageHint::USAGE_HINT_ENCODER,
                RECONSTRUCTED_SURFACE_ALLOCATION_BATCH,
                "reconstructed",
            )
        }

        fn rotate_reference(
            &mut self,
            surface: Surface<()>,
            encoded_keyframe: bool,
        ) -> Option<EncodedReference> {
            let retired = self.reference.take();
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
            retired
        }

        fn producer_is_pending(&self, surface_id: libva::VASurfaceID) -> bool {
            self.pending.iter().any(|pending| pending.reconstructed_id == surface_id)
        }

        fn collect_ready(&mut self) -> Result<Vec<EncodedFrame>, String> {
            let mut completed = Vec::new();
            while self.pending.front().is_some_and(PendingEncode::is_ready) {
                completed.push(self.complete_oldest()?);
            }
            Ok(completed)
        }

        fn complete_oldest(&mut self) -> Result<EncodedFrame, String> {
            let pending = self
                .pending
                .pop_front()
                .ok_or_else(|| "VA-API encoder has no pending frame".to_string())?;
            self.complete_pending(pending)
        }

        fn complete_pending(
            &mut self,
            pending: PendingEncode,
        ) -> Result<EncodedFrame, String> {
            let sync_started_at = Instant::now();
            let picture = pending
                .picture
                .sync()
                .map_err(|(err, _)| format!("failed to sync VA-API picture: {err}"))?;
            let sync_elapsed = sync_started_at.elapsed();
            let input = picture
                .take_surface()
                .map_err(|_| "VA-API picture kept a shared input surface".to_string())?;
            drop(input);

            let map_started_at = Instant::now();
            let data =
                self.collect_coded_data(&pending.coded_buffer, pending.is_keyframe)?;
            let map_elapsed = map_started_at.elapsed();
            let elapsed = pending.submitted_at.elapsed();
            if sync_elapsed > Duration::from_millis(10)
                || map_elapsed > Duration::from_millis(10)
            {
                warn!(
                    elapsed_ms = elapsed.as_millis(),
                    sync_ms = sync_elapsed.as_millis(),
                    map_ms = map_elapsed.as_millis(),
                    is_keyframe = pending.is_keyframe,
                    bytes = data.len(),
                    pts_us = pending.pts.as_micros(),
                    "completed VA-API H264 encode frame"
                );
            }

            if let Some(surface) = pending.retired_after_sync {
                self.free_reconstructed_surfaces.push(surface);
            }
            self.release_retired_producer(pending.reconstructed_id);

            Ok(EncodedFrame { data, pts: pending.pts, is_keyframe: pending.is_keyframe })
        }

        fn release_retired_producer(&mut self, surface_id: libva::VASurfaceID) {
            if let Some(index) = self
                .retired_after_producer
                .iter()
                .position(|surface| surface.id() == surface_id)
            {
                let surface = self.retired_after_producer.swap_remove(index);
                self.free_reconstructed_surfaces.push(surface);
            }
        }

        fn collect_coded_data(
            &self,
            coded_buffer: &EncCodedBuffer,
            is_keyframe: bool,
        ) -> Result<Bytes, String> {
            let mapped = MappedCodedBuffer::new(coded_buffer)
                .map_err(|err| format!("failed to map VA-API coded buffer: {err}"))?;
            let slice_len = mapped.iter().map(|segment| segment.buf.len()).sum::<usize>();
            if slice_len == 0 {
                return Err("VA-API encoder returned empty coded data".into());
            }
            let starts_with_three_byte_code = mapped
                .iter()
                .flat_map(|segment| segment.buf.iter().copied())
                .take(3)
                .eq([0, 0, 1]);
            let parameter_sets_len =
                is_keyframe.then_some(self.parameter_sets.len()).unwrap_or_default();
            let mut out = Vec::with_capacity(
                parameter_sets_len + slice_len + starts_with_three_byte_code as usize,
            );
            if is_keyframe {
                out.extend_from_slice(&self.parameter_sets);
            }
            if starts_with_three_byte_code {
                out.push(0);
            }
            for segment in mapped.iter() {
                out.extend_from_slice(segment.buf);
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

    struct PendingEncode {
        picture: Picture<libva::PictureEnd, Surface<VaapiDmaBufFrame>>,
        coded_buffer: EncCodedBuffer,
        reconstructed_id: libva::VASurfaceID,
        retired_after_sync: Option<Surface<()>>,
        pts: Duration,
        is_keyframe: bool,
        submitted_at: Instant,
    }

    impl PendingEncode {
        fn is_ready(&self) -> bool {
            match self.picture.surface().query_status() {
                Ok(status) => status == VASurfaceStatus::VASurfaceReady,
                Err(_) => true,
            }
        }
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

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use std::{
            sync::Mutex,
            thread,
            time::{Duration, Instant},
        };

        use smelter_render::Resolution;

        use super::*;
        const TEST_RESOLUTION: Resolution = Resolution { width: 64, height: 64 };
        const STRESS_RESOLUTION: Resolution = Resolution { width: 1280, height: 720 };
        const TEST_FRAMERATE: Framerate = Framerate { num: 30, den: 1 };
        const MAX_PENDING_ENCODE_FRAMES: usize = 8;
        static VAAPI_TEST_LOCK: Mutex<()> = Mutex::new(());

        #[test]
        #[ignore = "requires a VA-API capable Linux host"]
        fn encodes_exported_nv12_dmabuf_frames_to_h264() {
            let _guard = VAAPI_TEST_LOCK.lock().unwrap();
            let device = crate::test_wgpu_device();
            let mut encoder = H264Encoder::new(H264EncoderConfig {
                resolution: TEST_RESOLUTION,
                bitrate: 500_000,
                gop_size: 30,
                framerate: TEST_FRAMERATE,
                max_pending_frames: MAX_PENDING_ENCODE_FRAMES,
            })
            .expect("failed to create VA-API H264 encoder");
            let mut frames = (0..2)
                .map(|_| {
                    smelter_render::export_nv12_dmabuf_texture(&device, TEST_RESOLUTION)
                })
                .collect::<Vec<_>>();

            let mut encoded = Vec::new();
            encoded.extend(
                encoder
                    .encode(frames.remove(0), Duration::ZERO, true)
                    .expect("failed to encode VA-API keyframe"),
            );
            encoded.extend(
                encoder
                    .encode(frames.remove(0), Duration::from_millis(33), false)
                    .expect("failed to encode VA-API delta frame"),
            );
            encoded.extend(encoder.flush().expect("failed to flush VA-API encoder"));
            assert_eq!(encoded.len(), 2);
            let keyframe = &encoded[0];
            let delta = &encoded[1];

            assert!(keyframe.is_keyframe);
            assert!(contains_h264_nal(&keyframe.data, 7));
            assert!(contains_h264_nal(&keyframe.data, 8));
            assert!(contains_h264_nal(&keyframe.data, 5));
            assert!(!delta.is_keyframe);
            assert!(contains_h264_nal(&delta.data, 1));
        }

        #[test]
        #[ignore = "requires a VA-API capable Linux host"]
        fn encodes_exported_nv12_dmabuf_frames_at_steady_30fps() {
            const FRAME_COUNT: usize = 120;
            let _guard = VAAPI_TEST_LOCK.lock().unwrap();
            let device = crate::test_wgpu_device();
            let mut encoder = H264Encoder::new(H264EncoderConfig {
                resolution: STRESS_RESOLUTION,
                bitrate: 4_000_000,
                gop_size: 30,
                framerate: TEST_FRAMERATE,
                max_pending_frames: MAX_PENDING_ENCODE_FRAMES,
            })
            .expect("failed to create VA-API H264 encoder");
            let frames = (0..MAX_PENDING_ENCODE_FRAMES + 1)
                .map(|_| {
                    smelter_render::export_nv12_dmabuf_texture(&device, STRESS_RESOLUTION)
                })
                .collect::<Vec<_>>();

            let mut encoded = Vec::new();
            let mut call_times = Vec::new();
            for index in 0..FRAME_COUNT {
                let started_at = Instant::now();
                encoded.extend(
                    encoder
                        .encode(
                            Arc::clone(&frames[index % frames.len()]),
                            Duration::from_micros(index as u64 * 1_000_000 / 30),
                            false,
                        )
                        .expect("failed to encode VA-API frame"),
                );
                let elapsed = started_at.elapsed();
                if index >= frames.len() {
                    call_times.push(elapsed);
                }
                if elapsed < Duration::from_millis(33) {
                    thread::sleep(Duration::from_millis(33) - elapsed);
                }
            }
            encoded.extend(encoder.flush().expect("failed to flush VA-API encoder"));

            let max_call_ms =
                call_times.iter().map(Duration::as_millis).max().unwrap_or_default();
            let keyframes = encoded.iter().filter(|frame| frame.is_keyframe).count();
            eprintln!(
                "encoded={}; keyframes={}; max_call_ms={}",
                encoded.len(),
                keyframes,
                max_call_ms
            );
            assert_eq!(encoded.len(), FRAME_COUNT);
            assert!(keyframes >= 4);
            assert!(max_call_ms < 40);
        }

        fn contains_h264_nal(data: &[u8], nal_type: u8) -> bool {
            data.windows(5)
                .any(|window| window[..4] == [0, 0, 0, 1] && window[4] & 0x1f == nal_type)
                || data.windows(4).any(|window| {
                    window[..3] == [0, 0, 1] && window[3] & 0x1f == nal_type
                })
        }
    }
}

pub use imp::{EncodedFrame, H264Encoder, H264EncoderConfig};
