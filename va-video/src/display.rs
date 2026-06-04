use std::{os::fd::AsRawFd, rc::Rc, sync::Arc, time::Duration};

use libva::{
    Display, DrmPrimeSurfaceDescriptor, ExternalBufferDescriptor, MemoryType,
    PictureH264, Surface, UsageHint, VA_INVALID_ID, VA_PICTURE_H264_INVALID,
    VADRMPRIMESurfaceDescriptor, VADRMPRIMESurfaceDescriptorLayer,
    VADRMPRIMESurfaceDescriptorObject,
};
use smelter_render::{
    DRM_FORMAT_NV12, DmaBufFrame, DmaBufLayer, DmaBufObject, DmaBufPlane,
    Nv12DmaBufImportUsage, Resolution,
};

const DEFAULT_DRM_RENDER_NODE: &str = "/dev/dri/renderD128";

#[derive(Debug, Clone)]
pub(crate) struct VaapiDmaBufFrame(Arc<DmaBufFrame>);

impl VaapiDmaBufFrame {
    pub(crate) fn new(frame: Arc<DmaBufFrame>) -> Self {
        assert_eq!(frame.fourcc(), DRM_FORMAT_NV12);
        assert_eq!(frame.layers().len(), 1);
        Self(frame)
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
}

impl ExternalBufferDescriptor for VaapiDmaBufFrame {
    const MEMORY_TYPE: MemoryType = MemoryType::DrmPrime2;
    type DescriptorAttribute = VADRMPRIMESurfaceDescriptor;

    fn va_surface_attribute(&mut self) -> Self::DescriptorAttribute {
        let layer = &self.0.layers()[0];
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
    let paths = vaapi_drm_paths();
    for path in &paths {
        match Display::open_drm_display(path) {
            Ok(display) => return Ok(display),
            Err(err) => {
                tracing::error!("Failed to open VA-API DRM display {path}: {err}")
            }
        }
    }
    Err(no_usable_drm_display_error(&paths))
}

pub(crate) fn export_surface_as_frame(
    device: &wgpu::Device,
    surface: &Surface<()>,
) -> Result<Arc<DmaBufFrame>, String> {
    let descriptor = surface
        .export_prime()
        .map_err(|err| format!("failed to export VA surface: {err}"))?;
    import_drm_prime_surface(device, descriptor, None, Nv12DmaBufImportUsage::Sampled)
}

pub(crate) fn take_nv12_surface(
    display: &Rc<Display>,
    free_surfaces: &mut Vec<Surface<()>>,
    resolution: Resolution,
    usage_hint: UsageHint,
    batch_size: usize,
    label: &str,
) -> Result<Surface<()>, String> {
    if let Some(surface) = free_surfaces.pop() {
        return Ok(surface);
    }

    let mut surfaces = display
        .create_surfaces(
            libva::VA_RT_FORMAT_YUV420,
            Some(libva::VA_FOURCC_NV12),
            resolution.width as u32,
            resolution.height as u32,
            Some(usage_hint),
            vec![(); batch_size],
        )
        .map_err(|err| format!("failed to create VA-API {label} surfaces: {err}"))?;
    let surface =
        surfaces.pop().ok_or_else(|| format!("VA-API returned no {label} surface"))?;
    free_surfaces.extend(surfaces);
    Ok(surface)
}

pub(crate) fn invalid_h264_pictures<const N: usize>() -> [PictureH264; N] {
    std::array::from_fn(|_| {
        PictureH264::new(VA_INVALID_ID, 0, VA_PICTURE_H264_INVALID, 0, 0)
    })
}

pub(crate) fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().try_into().unwrap_or(u64::MAX)
}

fn import_drm_prime_surface(
    device: &wgpu::Device,
    descriptor: DrmPrimeSurfaceDescriptor,
    owner: Option<Arc<dyn Send + Sync>>,
    usage: Nv12DmaBufImportUsage,
) -> Result<Arc<DmaBufFrame>, String> {
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
            dmabuf_layer_from_prime_parts(
                layer.drm_format,
                layer.num_planes,
                layer.object_index.map(|index| index as usize),
                layer.offset,
                layer.pitch,
            )
        })
        .collect::<Result<Vec<_>, String>>()?;

    smelter_render::import_nv12_dmabuf_texture(
        device,
        descriptor.fourcc,
        descriptor.width,
        descriptor.height,
        objects,
        layers,
        owner,
        usage,
    )
}

fn dmabuf_layer_from_prime_parts(
    drm_format: u32,
    num_planes: u32,
    object_index: [usize; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
) -> Result<DmaBufLayer, String> {
    let plane_count = checked_va_array_count("DRM PRIME layer plane", num_planes)?;
    Ok(DmaBufLayer {
        drm_format,
        planes: (0..plane_count)
            .map(|index| DmaBufPlane {
                object_index: object_index[index],
                offset: offset[index],
                pitch: pitch[index],
            })
            .collect(),
    })
}

fn vaapi_drm_paths() -> Vec<String> {
    vaapi_drm_paths_from(
        std::env::var("SMELTER_VAAPI_DRM_DEVICE").ok().filter(|path| !path.is_empty()),
        discover_drm_render_nodes(),
    )
}

fn discover_drm_render_nodes() -> Vec<String> {
    let mut paths = std::fs::read_dir("/dev/dri")
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let index = entry
                .file_name()
                .to_str()?
                .strip_prefix("renderD")?
                .parse::<u32>()
                .ok()?;
            let path = entry.path().to_str()?.to_string();
            Some((index, path))
        })
        .collect::<Vec<_>>();

    paths.sort_by_key(|(index, _)| *index);
    paths.into_iter().map(|(_, path)| path).collect()
}

fn vaapi_drm_paths_from(
    configured: Option<String>,
    discovered: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = configured {
        push_unique_drm_path(&mut paths, path);
    }
    for path in discovered {
        push_unique_drm_path(&mut paths, path);
    }
    if paths.is_empty() {
        paths.push(DEFAULT_DRM_RENDER_NODE.to_string());
    }
    paths
}

fn push_unique_drm_path(paths: &mut Vec<String>, path: String) {
    if !path.is_empty() && !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn no_usable_drm_display_error(paths: &[String]) -> String {
    format!("no usable DRM display found in {}", paths.join(", "))
}

fn checked_va_array_count(name: &str, count: u32) -> Result<usize, String> {
    let count = count as usize;
    if count > 4 {
        return Err(format!("{name} count {count} exceeds VA-API descriptor limit"));
    }
    Ok(count)
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
    use super::*;

    #[test]
    fn drm_paths_keep_configured_device_first() {
        let paths = vaapi_drm_paths_from(
            Some("/dev/dri/renderD129".into()),
            ["/dev/dri/renderD128".into(), "/dev/dri/renderD129".into()],
        );

        assert_eq!(paths, vec!["/dev/dri/renderD129", "/dev/dri/renderD128"]);
    }

    #[test]
    fn drm_paths_use_default_when_no_render_nodes_are_discovered() {
        let paths = vaapi_drm_paths_from(None, []);

        assert_eq!(paths, vec![DEFAULT_DRM_RENDER_NODE]);
    }
}
