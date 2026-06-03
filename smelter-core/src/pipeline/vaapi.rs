use std::{
    ffi::{CStr, c_void},
    fs::File,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    rc::Rc,
    sync::Arc,
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

pub(crate) fn export_surface_as_frame(
    device: &wgpu::Device,
    surface: &Surface<()>,
) -> Result<Arc<DmaBufFrame>, String> {
    surface
        .export_prime()
        .map_err(|err| format!("failed to export VA surface: {err}"))
        .and_then(|descriptor| import_prime_surface(device, descriptor, None))
}

fn import_prime_surface(
    device: &wgpu::Device,
    descriptor: DrmPrimeSurfaceDescriptor,
    owner: Option<Arc<dyn DmaBufFrameOwner>>,
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

    smelter_render::import_nv12_dmabuf_texture_with_owner(
        device, fourcc, width, height, objects, layers, owner,
    )
}

pub(crate) struct VaapiEncoderInputAllocator {
    display: Arc<RawVaapiDisplay>,
}

impl VaapiEncoderInputAllocator {
    pub(crate) fn new() -> Result<Self, String> {
        Ok(Self { display: RawVaapiDisplay::open()? })
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
        import_raw_prime_surface(device, descriptor, Some(owner))
    }
}

struct RawVaapiDisplay {
    handle: libva::VADisplay,
    _drm_file: File,
}

unsafe impl Send for RawVaapiDisplay {}
unsafe impl Sync for RawVaapiDisplay {}

impl RawVaapiDisplay {
    fn open() -> Result<Arc<Self>, String> {
        let configured = std::env::var("SMELTER_VAAPI_DRM_DEVICE").ok();
        for path in configured.as_deref().into_iter().chain(["/dev/dri/renderD128"]) {
            match Self::open_drm(path) {
                Ok(display) => return Ok(display),
                Err(err) => {
                    tracing::error!("Failed to open raw VA-API DRM display {path}: {err}")
                }
            }
        }
        Err("no usable DRM display found".into())
    }

    fn open_drm(path: impl AsRef<Path>) -> Result<Arc<Self>, String> {
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

        Ok(Arc::new(Self { handle: display, _drm_file: file }))
    }

    fn create_nv12_encoder_input_surface(
        self: &Arc<Self>,
        resolution: Resolution,
    ) -> Result<VaapiOwnedSurface, String> {
        let mut attrs = [
            libva::VASurfaceAttrib::new_usage_hint(
                UsageHint::USAGE_HINT_ENCODER | UsageHint::USAGE_HINT_EXPORT,
            ),
            libva::VASurfaceAttrib::new_pixel_format(libva::VA_FOURCC_NV12),
        ];
        let mut surface_id: VASurfaceID = 0;
        check_va_status("vaCreateSurfaces", unsafe {
            libva::vaCreateSurfaces(
                self.handle,
                libva::VA_RT_FORMAT_YUV420,
                resolution.width as u32,
                resolution.height as u32,
                &mut surface_id,
                1,
                attrs.as_mut_ptr(),
                attrs.len() as u32,
            )
        })?;

        Ok(VaapiOwnedSurface { display: Arc::clone(self), id: surface_id })
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
    display: Arc<RawVaapiDisplay>,
    id: VASurfaceID,
}

impl VaapiOwnedSurface {
    fn export_for_write(&self) -> Result<libva::VADRMPRIMESurfaceDescriptor, String> {
        let mut descriptor = libva::VADRMPRIMESurfaceDescriptor::default();
        check_va_status("vaExportSurfaceHandle", unsafe {
            libva::vaExportSurfaceHandle(
                self.display.handle,
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
        unsafe {
            libva::vaDestroySurfaces(self.display.handle, &mut self.id, 1);
        }
    }
}

fn import_raw_prime_surface(
    device: &wgpu::Device,
    descriptor: libva::VADRMPRIMESurfaceDescriptor,
    owner: Option<Arc<dyn DmaBufFrameOwner>>,
) -> Result<Arc<DmaBufFrame>, String> {
    let fourcc = descriptor.fourcc;
    let width = descriptor.width;
    let height = descriptor.height;
    let objects = (0..descriptor.num_objects as usize)
        .take(4)
        .map(|index| descriptor.objects[index])
        .map(|object| DmaBufObject {
            fd: Arc::new(unsafe { OwnedFd::from_raw_fd(object.fd) }),
            size: object.size,
            modifier: object.drm_format_modifier,
        })
        .collect::<Vec<_>>();
    let layers = (0..descriptor.num_layers as usize)
        .take(4)
        .map(|index| descriptor.layers[index])
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

    smelter_render::import_renderable_nv12_dmabuf_texture_with_owner(
        device, fourcc, width, height, objects, layers, owner,
    )
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
