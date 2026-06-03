use std::{cell::RefCell, os::fd::AsRawFd, rc::Rc, sync::Arc};

use cros_codecs::{
    Fourcc, FrameLayout, PlaneLayout,
    backend::vaapi::surface_pool::{PooledVaSurface, VaSurfacePool},
    decoder::FramePool,
    libva::{
        Display, ExternalBufferDescriptor, MemoryType, Surface, UsageHint,
        VADRMPRIMESurfaceDescriptor, VADRMPRIMESurfaceDescriptorLayer,
        VADRMPRIMESurfaceDescriptorObject,
    },
    video_frame::{ReadMapping, VideoFrame, WriteMapping},
};
use smelter_render::{DmaBufFrame, DmaBufLayer};

#[derive(Debug)]
pub(crate) struct VaapiManagedFrame {
    resolution: cros_codecs::Resolution,
}

impl VaapiManagedFrame {
    pub(crate) fn new(resolution: cros_codecs::Resolution) -> Self {
        Self { resolution }
    }
}

impl VideoFrame for VaapiManagedFrame {
    type MemDescriptor = ();
    type NativeHandle = PooledVaSurface<()>;

    fn fourcc(&self) -> Fourcc {
        Fourcc::from(b"NV12")
    }

    fn resolution(&self) -> cros_codecs::Resolution {
        self.resolution
    }

    fn get_plane_size(&self) -> Vec<usize> {
        let y = self.resolution.width as usize * self.resolution.height as usize;
        vec![y, y / 2]
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        vec![self.resolution.width as usize; 2]
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Err("VA-API managed frames are not CPU-readable".into())
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Err("VA-API managed frames are not CPU-writable".into())
    }

    fn to_native_handle(
        &self,
        display: &Rc<Display>,
    ) -> Result<Self::NativeHandle, String> {
        thread_local! {
            static MANAGED_SURFACE_POOL: RefCell<Option<VaSurfacePool<()>>> =
                const { RefCell::new(None) };
        }

        MANAGED_SURFACE_POOL.with_borrow_mut(|pool| {
            let pool = pool.get_or_insert_with(|| {
                VaSurfacePool::new(
                    Rc::clone(display),
                    cros_codecs::libva::VA_RT_FORMAT_YUV420,
                    Some(UsageHint::USAGE_HINT_DECODER),
                    self.resolution,
                )
            });
            pool.set_coded_resolution(self.resolution);
            if pool.num_free_frames() == 0 {
                pool.add_frames(vec![(); VAAPI_MANAGED_SURFACE_BATCH]).map_err(
                    |err| format!("Failed to create VA-API managed surfaces: {err}"),
                )?;
            }
            pool.get_surface()
                .ok_or_else(|| "VA-API managed surface pool returned no surface".into())
        })
    }
}

const VAAPI_MANAGED_SURFACE_BATCH: usize = 8;

#[derive(Debug, Clone)]
pub(crate) struct VaapiDmaBufFrame(Arc<DmaBufFrame>);

impl VaapiDmaBufFrame {
    pub(crate) fn new(frame: Arc<DmaBufFrame>) -> Self {
        assert_eq!(frame.fourcc(), u32::from_le_bytes(*b"NV12"));
        assert_eq!(frame.layers().len(), 1);
        Self(frame)
    }

    pub(crate) fn layout(&self) -> FrameLayout {
        let layer = self.layer();
        FrameLayout {
            format: (Fourcc::from(self.0.fourcc()), self.modifier()),
            size: self.resolution(),
            planes: layer
                .planes
                .iter()
                .map(|plane| PlaneLayout {
                    buffer_index: plane.object_index,
                    offset: plane.offset as usize,
                    stride: plane.pitch as usize,
                })
                .collect(),
        }
    }

    fn layer(&self) -> &DmaBufLayer {
        &self.0.layers()[0]
    }

    fn modifier(&self) -> u64 {
        self.0.objects()[0].modifier
    }
}

impl ExternalBufferDescriptor for VaapiDmaBufFrame {
    const MEMORY_TYPE: MemoryType = MemoryType::DrmPrime2;
    type DescriptorAttribute = VADRMPRIMESurfaceDescriptor;

    fn va_surface_attribute(&mut self) -> Self::DescriptorAttribute {
        let layer = self.layer();
        let mut objects =
            std::array::from_fn(|_| VADRMPRIMESurfaceDescriptorObject::default());
        for (dst, object) in objects.iter_mut().zip(self.0.objects()) {
            *dst = VADRMPRIMESurfaceDescriptorObject {
                fd: object.fd.as_ref().as_raw_fd(),
                size: object.size,
                drm_format_modifier: object.modifier,
            };
        }

        let layers = [
            VADRMPRIMESurfaceDescriptorLayer {
                drm_format: layer.drm_format,
                num_planes: layer.planes.len() as u32,
                object_index: fixed_u32(
                    layer.planes.iter().map(|plane| plane.object_index as u32),
                ),
                offset: fixed_u32(layer.planes.iter().map(|plane| plane.offset)),
                pitch: fixed_u32(layer.planes.iter().map(|plane| plane.pitch)),
            },
            Default::default(),
            Default::default(),
            Default::default(),
        ];

        VADRMPRIMESurfaceDescriptor {
            fourcc: self.0.fourcc(),
            width: self.0.width(),
            height: self.0.height(),
            num_objects: self.0.objects().len() as u32,
            objects,
            num_layers: 1,
            layers,
        }
    }
}

impl VideoFrame for VaapiDmaBufFrame {
    type MemDescriptor = VaapiDmaBufFrame;
    type NativeHandle = Surface<VaapiDmaBufFrame>;

    fn fourcc(&self) -> Fourcc {
        Fourcc::from(self.0.fourcc())
    }

    fn resolution(&self) -> cros_codecs::Resolution {
        cros_codecs::Resolution { width: self.0.width(), height: self.0.height() }
    }

    fn get_plane_size(&self) -> Vec<usize> {
        let layer = self.layer();
        layer
            .planes
            .iter()
            .map(|plane| {
                (self.0.objects()[plane.object_index].size - plane.offset) as usize
            })
            .collect()
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        self.layer().planes.iter().map(|plane| plane.pitch as usize).collect()
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Err("VA-API DMA-BUF frames are not CPU-readable".into())
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Err("VA-API DMA-BUF frames are not CPU-writable".into())
    }

    fn to_native_handle(
        &self,
        display: &Rc<Display>,
    ) -> Result<Self::NativeHandle, String> {
        let mut surfaces = display
            .create_surfaces(
                cros_codecs::libva::VA_RT_FORMAT_YUV420,
                Some(self.0.fourcc()),
                self.0.width(),
                self.0.height(),
                Some(UsageHint::USAGE_HINT_DECODER | UsageHint::USAGE_HINT_ENCODER),
                vec![self.clone()],
            )
            .map_err(|err| format!("Failed to import DMA-BUF into VA-API: {err}"))?;
        Ok(surfaces.pop().expect("VA-API returned no imported surface"))
    }
}

pub(crate) fn open_display() -> Result<Rc<Display>, String> {
    let configured = std::env::var("SMELTER_VAAPI_DRM_DEVICE").ok();
    for path in configured.as_deref().into_iter().chain(["/dev/dri/renderD128"]) {
        match Display::open_drm_display(path) {
            Ok(display) => return Ok(display),
            Err(err) => {
                tracing::error!("Failed to open VA-API DRM display {path}: {err}")
            }
        }
    }
    Display::open().ok_or_else(|| "no usable DRM display found".into())
}

fn fixed_u32(values: impl IntoIterator<Item = u32>) -> [u32; 4] {
    let mut out = [0; 4];
    for (dst, value) in out.iter_mut().zip(values) {
        *dst = value;
    }
    out
}
