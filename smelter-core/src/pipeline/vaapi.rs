use std::{os::fd::AsRawFd, rc::Rc, sync::Arc};

use libva::{
    Display, ExternalBufferDescriptor, MemoryType, Surface, UsageHint,
    VADRMPRIMESurfaceDescriptor, VADRMPRIMESurfaceDescriptorLayer,
    VADRMPRIMESurfaceDescriptorObject,
};
use smelter_render::{DmaBufFrame, DmaBufLayer};

#[derive(Debug, Clone)]
pub(crate) struct VaapiDmaBufFrame(Arc<DmaBufFrame>);

impl VaapiDmaBufFrame {
    pub(crate) fn new(frame: Arc<DmaBufFrame>) -> Self {
        assert_eq!(frame.fourcc(), u32::from_le_bytes(*b"NV12"));
        assert_eq!(frame.layers().len(), 1);
        Self(frame)
    }

    pub(crate) fn cache_key(frame: &Arc<DmaBufFrame>) -> usize {
        Arc::as_ptr(frame) as usize
    }

    pub(crate) fn import_surface(
        self,
        display: &Rc<Display>,
    ) -> Result<Surface<Self>, String> {
        let mut surfaces = display
            .create_surfaces(
                libva::VA_RT_FORMAT_YUV420,
                Some(self.0.fourcc()),
                self.0.width(),
                self.0.height(),
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![self],
            )
            .map_err(|err| format!("Failed to import DMA-BUF into VA-API: {err}"))?;
        Ok(surfaces.pop().expect("VA-API returned no imported surface"))
    }

    fn layer(&self) -> &DmaBufLayer {
        &self.0.layers()[0]
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
