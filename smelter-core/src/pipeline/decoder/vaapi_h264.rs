#[cfg(feature = "vaapi")]
mod imp {
    use std::{
        collections::{HashMap, VecDeque},
        rc::Rc,
        sync::Arc,
        time::Duration,
    };

    use cros_codecs::libva::{DrmPrimeSurfaceDescriptor, VASurfaceID};
    use cros_codecs::{
        BlockingMode,
        backend::vaapi::decoder::VaapiBackend,
        decoder::{
            DecodedHandle as _, DecoderEvent,
            stateless::{
                DecodeError, StatelessDecoder, StatelessVideoDecoder, h264::H264,
            },
        },
    };
    use smelter_render::{
        DmaBufFrame, DmaBufLayer, DmaBufObject, DmaBufPlane, Frame, FrameData,
    };
    use tracing::{debug, error, info, trace, warn};

    use crate::{
        pipeline::{
            decoder::{
                EncodedInputEvent, KeyframeRequestSender, VideoDecoder,
                VideoDecoderInstance,
            },
            utils::H264AuSplitter,
            vaapi::{VaapiManagedFrame, open_display},
        },
        prelude::*,
    };

    type Decoder = StatelessDecoder<H264, VaapiBackend<VaapiManagedFrame>>;
    type DecodedFrameHandle = <Decoder as StatelessVideoDecoder>::Handle;

    pub struct VaapiH264Decoder {
        decoder: Decoder,
        frame_cache: VaapiDecodedFrameCache,
        device: Arc<wgpu::Device>,
        keyframe_request_sender: Option<KeyframeRequestSender>,
        au_splitter: H264AuSplitter,
        drop_frames: bool,
    }

    impl VideoDecoder for VaapiH264Decoder {
        const LABEL: &'static str = "VA-API H264 decoder";

        fn new(
            ctx: &Arc<PipelineCtx>,
            keyframe_request_sender: Option<KeyframeRequestSender>,
        ) -> Result<Self, DecoderInitError> {
            info!("Initializing VA-API H264 decoder");
            let display =
                open_display().map_err(DecoderInitError::VaapiH264DecoderUnavailable)?;
            let decoder = Decoder::new_vaapi(Rc::clone(&display), BlockingMode::Blocking)
                .map_err(|err| {
                    error!("Failed to initialize VA-API H264 decoder: {err}");
                    DecoderInitError::VaapiH264DecoderUnavailable(err.to_string())
                })?;
            Ok(Self {
                decoder,
                frame_cache: VaapiDecodedFrameCache::new(),
                device: Arc::clone(&ctx.graphics_context.device),
                keyframe_request_sender,
                au_splitter: H264AuSplitter::default(),
                drop_frames: false,
            })
        }
    }

    impl VideoDecoderInstance for VaapiH264Decoder {
        fn decode(&mut self, event: EncodedInputEvent) -> Vec<Frame> {
            trace!(?event, "VA-API H264 decoder received an event.");
            let chunks = match event {
                EncodedInputEvent::Chunk(chunk) => {
                    self.drop_frames = !chunk.present;
                    match self.au_splitter.put_chunk(chunk) {
                        Ok(chunks) => chunks,
                        Err(err) => {
                            self.request_keyframe();
                            debug!(
                                "H264 AU splitter could not process the chunks: {err}"
                            );
                            return Vec::new();
                        }
                    }
                }
                EncodedInputEvent::LostData => {
                    self.au_splitter.mark_missing_data();
                    return Vec::new();
                }
                EncodedInputEvent::AuDelimiter => match self.au_splitter.flush() {
                    Ok(chunks) => chunks,
                    Err(err) => {
                        self.request_keyframe();
                        debug!("H264 AU splitter could not process the chunks: {err}");
                        return Vec::new();
                    }
                },
            };

            let mut frames = Vec::new();
            for chunk in chunks {
                frames.extend(self.decode_access_unit(&chunk.data, chunk.pts));
            }
            frames.extend(self.poll());
            frames
        }

        fn flush(&mut self) -> Vec<Frame> {
            if let Err(err) = self.decoder.flush() {
                warn!("Failed to flush the VA-API decoder: {err}");
            }
            self.poll()
        }
    }

    impl VaapiH264Decoder {
        fn decode_access_unit(&mut self, mut data: &[u8], pts: Duration) -> Vec<Frame> {
            let mut frames = Vec::new();
            while !data.is_empty() {
                if !has_nalu(data) {
                    break;
                }
                let timestamp = pts.as_micros().try_into().unwrap_or(u64::MAX);
                let stream_info = self.decoder.stream_info().cloned();
                match self.decoder.decode(timestamp, data, &mut || {
                    Some(allocate_vaapi_managed_frame(stream_info.as_ref()))
                }) {
                    Ok(0) => {
                        warn!("VA-API decoder consumed no bytes from H264 access unit");
                        break;
                    }
                    Ok(consumed) => data = &data[consumed..],
                    Err(DecodeError::CheckEvents) => {
                        frames.extend(self.poll());
                    }
                    Err(err) => {
                        warn!("Failed to decode H264 access unit with VA-API: {err}");
                        break;
                    }
                }
            }
            frames
        }

        fn poll(&mut self) -> Vec<Frame> {
            let mut frames = Vec::new();
            while let Some(event) = self.decoder.next_event() {
                match event {
                    DecoderEvent::FrameReady(frame) => {
                        if let Err(err) = frame.sync() {
                            warn!("Failed to sync VA-API decoded frame: {err}");
                            continue;
                        }
                        if !self.drop_frames {
                            let pts = Duration::from_micros(frame.timestamp());
                            match self.frame_cache.frame_from_decoded(
                                &self.device,
                                frame,
                                pts,
                            ) {
                                Ok(output_frame) => frames.push(output_frame),
                                Err(err) => {
                                    warn!(
                                        "Failed to import VA-API decoded frame into WGPU: {err}"
                                    );
                                }
                            }
                        }
                    }
                    DecoderEvent::FormatChanged => {
                        self.frame_cache.clear_imported_frames();
                        trace!(
                            stream_info = ?self.decoder.stream_info(),
                            "VA-API decoder format changed"
                        );
                    }
                }
            }
            frames
        }

        fn request_keyframe(&self) {
            if let Some(sender) = self.keyframe_request_sender.as_ref() {
                sender.send();
            }
        }
    }

    fn has_nalu(data: &[u8]) -> bool {
        data.windows(3)
            .position(|window| window == [0, 0, 1])
            .is_some_and(|offset| data.len() > offset + 3)
    }

    const RETAINED_HANDLE_COUNT: usize = 64;

    struct VaapiDecodedFrameCache {
        retained_handles: VecDeque<DecodedFrameHandle>,
        imported_frames: HashMap<VASurfaceID, Arc<DmaBufFrame>>,
    }

    impl VaapiDecodedFrameCache {
        fn new() -> Self {
            Self {
                retained_handles: VecDeque::with_capacity(RETAINED_HANDLE_COUNT),
                imported_frames: HashMap::new(),
            }
        }

        fn frame_from_decoded(
            &mut self,
            device: &wgpu::Device,
            handle: DecodedFrameHandle,
            pts: Duration,
        ) -> Result<Frame, String> {
            let surface_id = handle.borrow().surface_id();
            let dmabuf = self.dmabuf_for_surface(device, surface_id, &handle)?;
            self.retain_handle(handle);
            Ok(frame_from_dmabuf(dmabuf, pts))
        }

        fn clear_imported_frames(&mut self) {
            self.imported_frames.clear();
        }

        fn dmabuf_for_surface(
            &mut self,
            device: &wgpu::Device,
            surface_id: VASurfaceID,
            handle: &DecodedFrameHandle,
        ) -> Result<Arc<DmaBufFrame>, String> {
            if let Some(dmabuf) = self.imported_frames.get(&surface_id) {
                return Ok(Arc::clone(dmabuf));
            }

            let descriptor = handle
                .borrow()
                .export_prime()
                .map_err(|err| format!("failed to export VA surface: {err}"))?;
            let dmabuf = import_vaapi_frame(device, descriptor)?;
            self.imported_frames.insert(surface_id, Arc::clone(&dmabuf));
            Ok(dmabuf)
        }

        fn retain_handle(&mut self, handle: DecodedFrameHandle) {
            self.retained_handles.push_back(handle);
            while self.retained_handles.len() > RETAINED_HANDLE_COUNT {
                self.retained_handles.pop_front();
            }
        }
    }

    fn frame_from_dmabuf(dmabuf: Arc<DmaBufFrame>, pts: Duration) -> Frame {
        let resolution = dmabuf.resolution();
        Frame { data: FrameData::Nv12DmaBuf(dmabuf), pts, resolution }
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

    fn allocate_vaapi_managed_frame(
        stream_info: Option<&cros_codecs::decoder::StreamInfo>,
    ) -> VaapiManagedFrame {
        let coded_resolution = stream_info
            .map(|info| info.coded_resolution)
            .unwrap_or(cros_codecs::Resolution { width: 16, height: 16 });
        VaapiManagedFrame::new(coded_resolution)
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
