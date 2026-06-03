use std::{
    ffi::{CStr, c_void},
    fs::File,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
};

use libva::{
    Display, DrmPrimeSurfaceDescriptor, ExternalBufferDescriptor, MemoryType, Surface,
    UsageHint, VADRMPRIMESurfaceDescriptor, VADRMPRIMESurfaceDescriptorLayer,
    VADRMPRIMESurfaceDescriptorObject, VASurfaceID,
};
use smelter_render::{
    DmaBufAllocator, DmaBufFrame, DmaBufFrameOwner, DmaBufLayer, DmaBufObject,
    DmaBufPlane, Resolution,
};

pub(crate) const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");
const VA_EXPORT_SURFACE_READ_WRITE: u32 = 0x0003;
const VA_EXPORT_SURFACE_COMPOSED_LAYERS: u32 = 0x0008;

#[derive(Debug, Clone)]
pub(crate) struct VaapiDmaBufFrame(Arc<DmaBufFrame>);

impl VaapiDmaBufFrame {
    pub(crate) fn new(frame: Arc<DmaBufFrame>) -> Self {
        assert_eq!(frame.fourcc(), DRM_FORMAT_NV12);
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
        surfaces.pop().ok_or_else(|| "VA-API returned no imported surface".to_string())
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
    for path in vaapi_drm_paths() {
        match Display::open_drm_display(&path) {
            Ok(display) => return Ok(display),
            Err(err) => {
                tracing::error!("Failed to open VA-API DRM display {path}: {err}")
            }
        }
    }
    Display::open().ok_or_else(|| "no usable DRM display found".into())
}

pub(crate) fn export_surface_as_frame(
    device: &wgpu::Device,
    surface: &Surface<()>,
) -> Result<Arc<DmaBufFrame>, String> {
    surface
        .export_prime()
        .map_err(|err| format!("failed to export VA surface: {err}"))
        .and_then(ExportedVaSurface::from_drm_prime)
        .and_then(|surface| surface.import_sampled(device, None))
}

pub(crate) struct VaapiEncoderInputAllocator {
    display: RawVaapiDisplayOwner,
}

impl VaapiEncoderInputAllocator {
    pub(crate) fn new() -> Result<Self, String> {
        Ok(Self { display: RawVaapiDisplayOwner::open()? })
    }
}

impl DmaBufAllocator for VaapiEncoderInputAllocator {
    fn allocate(
        &self,
        device: &wgpu::Device,
        resolution: Resolution,
    ) -> Result<Arc<DmaBufFrame>, String> {
        let surface =
            Arc::new(self.display.create_nv12_encoder_input_surface(resolution)?);
        let descriptor = surface.export_for_write()?;
        let owner: Arc<dyn DmaBufFrameOwner> = surface;
        ExportedVaSurface::from_raw_prime(descriptor)?
            .import_renderable(device, Some(owner))
    }
}

#[derive(Clone)]
struct RawVaapiDisplayOwner(Arc<Mutex<RawVaapiDisplay>>);

impl RawVaapiDisplayOwner {
    fn open() -> Result<Self, String> {
        Ok(Self(Arc::new(Mutex::new(RawVaapiDisplay::open()?))))
    }

    fn lock(&self) -> MutexGuard<'_, RawVaapiDisplay> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn create_nv12_encoder_input_surface(
        &self,
        resolution: Resolution,
    ) -> Result<VaapiOwnedSurface, String> {
        let display = self.lock();
        let mut attrs = [
            libva::VASurfaceAttrib::new_usage_hint(
                UsageHint::USAGE_HINT_ENCODER | UsageHint::USAGE_HINT_EXPORT,
            ),
            libva::VASurfaceAttrib::new_pixel_format(libva::VA_FOURCC_NV12),
        ];
        let mut surface_id: VASurfaceID = 0;
        check_va_status("vaCreateSurfaces", unsafe {
            libva::vaCreateSurfaces(
                display.handle,
                libva::VA_RT_FORMAT_YUV420,
                resolution.width as u32,
                resolution.height as u32,
                &mut surface_id,
                1,
                attrs.as_mut_ptr(),
                attrs.len() as u32,
            )
        })?;

        Ok(VaapiOwnedSurface { display: self.clone(), id: surface_id })
    }
}

struct RawVaapiDisplay {
    handle: libva::VADisplay,
    _drm_file: File,
}

// Raw VA calls are serialized by RawVaapiDisplayOwner's mutex; the display
// and DRM fd move together.
unsafe impl Send for RawVaapiDisplay {}

impl RawVaapiDisplay {
    fn open() -> Result<Self, String> {
        for path in vaapi_drm_paths() {
            match Self::open_drm(&path) {
                Ok(display) => return Ok(display),
                Err(err) => {
                    tracing::error!("Failed to open raw VA-API DRM display {path}: {err}")
                }
            }
        }
        Err("no usable DRM display found".into())
    }

    fn open_drm(path: impl AsRef<Path>) -> Result<Self, String> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(path.as_ref())
            .map_err(|err| format!("cannot open DRM device: {err}"))?;
        let display = unsafe { libva::vaGetDisplayDRM(file.as_raw_fd()) };
        if display.is_null() {
            return Err("vaGetDisplayDRM returned NULL".into());
        }

        let mut major = 0;
        let mut minor = 0;
        check_va_status("vaInitialize", unsafe {
            libva::vaInitialize(display, &mut major, &mut minor)
        })?;

        Ok(Self { handle: display, _drm_file: file })
    }
}

impl Drop for RawVaapiDisplay {
    fn drop(&mut self) {
        unsafe {
            libva::vaTerminate(self.handle);
        }
    }
}

struct VaapiOwnedSurface {
    display: RawVaapiDisplayOwner,
    id: VASurfaceID,
}

impl VaapiOwnedSurface {
    fn export_for_write(&self) -> Result<libva::VADRMPRIMESurfaceDescriptor, String> {
        let display = self.display.lock();
        let mut descriptor = libva::VADRMPRIMESurfaceDescriptor::default();
        check_va_status("vaExportSurfaceHandle", unsafe {
            libva::vaExportSurfaceHandle(
                display.handle,
                self.id,
                libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                VA_EXPORT_SURFACE_READ_WRITE | VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                &mut descriptor as *mut _ as *mut c_void,
            )
        })?;
        Ok(descriptor)
    }
}

impl Drop for VaapiOwnedSurface {
    fn drop(&mut self) {
        let display = self.display.lock();
        unsafe {
            libva::vaDestroySurfaces(display.handle, &mut self.id, 1);
        }
    }
}

struct ExportedVaSurface {
    fourcc: u32,
    width: u32,
    height: u32,
    objects: Vec<DmaBufObject>,
    layers: Vec<DmaBufLayer>,
}

impl ExportedVaSurface {
    fn from_drm_prime(descriptor: DrmPrimeSurfaceDescriptor) -> Result<Self, String> {
        let objects = descriptor
            .objects
            .into_iter()
            .map(|object| DmaBufObject {
                fd: Arc::new(object.fd),
                size: object.size,
                modifier: object.drm_format_modifier,
            })
            .collect();
        let layers = descriptor
            .layers
            .into_iter()
            .map(|layer| {
                let plane_count =
                    checked_va_array_count("DRM PRIME layer plane", layer.num_planes)?;
                Ok(DmaBufLayer {
                    drm_format: layer.drm_format,
                    planes: (0..plane_count)
                        .map(|index| DmaBufPlane {
                            object_index: layer.object_index[index] as usize,
                            offset: layer.offset[index],
                            pitch: layer.pitch[index],
                        })
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self {
            fourcc: descriptor.fourcc,
            width: descriptor.width,
            height: descriptor.height,
            objects,
            layers,
        })
    }

    fn from_raw_prime(
        descriptor: libva::VADRMPRIMESurfaceDescriptor,
    ) -> Result<Self, String> {
        let object_count =
            checked_va_array_count("DRM PRIME object", descriptor.num_objects)?;
        let layer_count =
            checked_va_array_count("DRM PRIME layer", descriptor.num_layers)?;
        let objects = (0..object_count)
            .map(|index| descriptor.objects[index])
            .map(|object| DmaBufObject {
                fd: Arc::new(unsafe { OwnedFd::from_raw_fd(object.fd) }),
                size: object.size,
                modifier: object.drm_format_modifier,
            })
            .collect();
        let layers = (0..layer_count)
            .map(|index| {
                let layer = descriptor.layers[index];
                let plane_count =
                    checked_va_array_count("DRM PRIME layer plane", layer.num_planes)?;
                Ok(DmaBufLayer {
                    drm_format: layer.drm_format,
                    planes: (0..plane_count)
                        .map(|index| DmaBufPlane {
                            object_index: layer.object_index[index] as usize,
                            offset: layer.offset[index],
                            pitch: layer.pitch[index],
                        })
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self {
            fourcc: descriptor.fourcc,
            width: descriptor.width,
            height: descriptor.height,
            objects,
            layers,
        })
    }

    fn import_sampled(
        self,
        device: &wgpu::Device,
        owner: Option<Arc<dyn DmaBufFrameOwner>>,
    ) -> Result<Arc<DmaBufFrame>, String> {
        smelter_render::import_nv12_dmabuf_texture_with_owner(
            device,
            self.fourcc,
            self.width,
            self.height,
            self.objects,
            self.layers,
            owner,
        )
    }

    fn import_renderable(
        self,
        device: &wgpu::Device,
        owner: Option<Arc<dyn DmaBufFrameOwner>>,
    ) -> Result<Arc<DmaBufFrame>, String> {
        smelter_render::import_renderable_nv12_dmabuf_texture_with_owner(
            device,
            self.fourcc,
            self.width,
            self.height,
            self.objects,
            self.layers,
            owner,
        )
    }
}

fn vaapi_drm_paths() -> impl Iterator<Item = String> {
    std::env::var("SMELTER_VAAPI_DRM_DEVICE")
        .into_iter()
        .chain(std::iter::once("/dev/dri/renderD128".to_string()))
}

fn checked_va_array_count(name: &str, count: u32) -> Result<usize, String> {
    let count = count as usize;
    if count > 4 {
        return Err(format!("{name} count {count} exceeds VA-API descriptor limit"));
    }
    Ok(count)
}

fn check_va_status(action: &str, status: libva::VAStatus) -> Result<(), String> {
    if status as u32 == libva::VA_STATUS_SUCCESS {
        return Ok(());
    }

    let message = unsafe { CStr::from_ptr(libva::vaErrorStr(status)) }.to_string_lossy();
    Err(format!("{action} failed: {message} ({status})"))
}

fn fixed_u32(values: impl IntoIterator<Item = u32>) -> [u32; 4] {
    let mut out = [0; 4];
    for (dst, value) in out.iter_mut().zip(values) {
        *dst = value;
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::Mutex;

    use crate::graphics_context::{GraphicsContext, GraphicsContextOptions};

    use super::*;

    static VAAPI_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    #[ignore = "requires a VA-API capable Linux host"]
    fn allocates_va_owned_encoder_input_dmabuf_frame() {
        let _guard = VAAPI_TEST_LOCK.lock().unwrap();
        let graphics_context = GraphicsContext::new(GraphicsContextOptions {
            force_gpu: true,
            ..Default::default()
        })
        .expect("failed to create WGPU graphics context");
        let allocator =
            VaapiEncoderInputAllocator::new().expect("failed to create allocator");
        let resolution = Resolution { width: 64, height: 64 };

        let frame = allocator
            .allocate(&graphics_context.device, resolution)
            .expect("failed to allocate VA-owned encoder input frame");

        assert_eq!(frame.fourcc(), DRM_FORMAT_NV12);
        assert_eq!(frame.resolution(), resolution);
        assert_eq!(frame.layers().len(), 1);
        assert_eq!(frame.layers()[0].planes.len(), 2);
    }
}
