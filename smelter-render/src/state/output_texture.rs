use std::{io::Write, sync::Arc};

#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, FromRawFd, IntoRawFd, OwnedFd};

#[cfg(target_os = "linux")]
use ash::vk;
use bytes::BufMut;
use crossbeam_channel::bounded;
use tracing::{error, info, warn};
#[cfg(target_os = "linux")]
use wgpu::hal::api::Vulkan as VkApi;
use wgpu::{Buffer, BufferAsyncError};

#[cfg(target_os = "linux")]
use crate::DmaBufPlane;
use crate::{
    DmaBufAllocator, DmaBufFrame, DmaBufFrameOwner, DmaBufLayer, DmaBufObject,
    OutputFrameFormat, Resolution,
    wgpu::{
        WgpuCtx,
        texture::{
            NV12Texture, PlanarYuvPendingDownload, PlanarYuvTextures, PlanarYuvVariant,
            utils::pad_to_256,
        },
    },
};

pub enum OutputTexture {
    PlanarYuvTextures(Box<PlanarYuvOutput>),
    Rgba8UnormWgpuTexture { resolution: Resolution },
    Nv12WgpuTexture { resolution: Resolution },
    Nv12DmaBuf(Box<Nv12DmaBufOutput>),
}

#[derive(Debug, thiserror::Error)]
pub enum CreateOutputTextureError {
    #[error("Failed to allocate NV12 DMA-BUF output frame: {0}")]
    DmaBufAllocation(String),

    #[error("NV12 DMA-BUF allocator returned fourcc {0}, expected NV12")]
    InvalidDmaBufFourcc(u32),

    #[error(
        "NV12 DMA-BUF allocator returned resolution {actual:?}, expected {expected:?}"
    )]
    InvalidDmaBufResolution { actual: Resolution, expected: Resolution },

    #[error("NV12 DMA-BUF allocator returned a non-NV12 wgpu texture")]
    InvalidDmaBufTexture,
}

impl OutputTexture {
    pub fn new(
        ctx: &WgpuCtx,
        resolution: Resolution,
        format: OutputFrameFormat,
    ) -> Result<Self, CreateOutputTextureError> {
        match format {
            OutputFrameFormat::PlanarYuv420Bytes => {
                warn!(
                    ?resolution,
                    "creating planar YUV output texture with CPU readback"
                );
                Ok(Self::PlanarYuvTextures(Box::new(PlanarYuvOutput::new(
                    ctx,
                    resolution,
                    PlanarYuvVariant::YUV420,
                ))))
            }
            OutputFrameFormat::PlanarYuv422Bytes => {
                warn!(
                    ?resolution,
                    "creating planar YUV output texture with CPU readback"
                );
                Ok(Self::PlanarYuvTextures(Box::new(PlanarYuvOutput::new(
                    ctx,
                    resolution,
                    PlanarYuvVariant::YUV422,
                ))))
            }
            OutputFrameFormat::PlanarYuv444Bytes => {
                warn!(
                    ?resolution,
                    "creating planar YUV output texture with CPU readback"
                );
                Ok(Self::PlanarYuvTextures(Box::new(PlanarYuvOutput::new(
                    ctx,
                    resolution,
                    PlanarYuvVariant::YUV444,
                ))))
            }
            OutputFrameFormat::RgbaWgpuTexture => {
                Ok(Self::Rgba8UnormWgpuTexture { resolution })
            }
            OutputFrameFormat::Nv12WgpuTexture => {
                Ok(Self::Nv12WgpuTexture { resolution })
            }
            OutputFrameFormat::Nv12DmaBuf => {
                info!(?resolution, "creating zero-copy NV12 DMA-BUF output texture");
                Ok(Self::Nv12DmaBuf(Box::new(Nv12DmaBufOutput::new(
                    ctx, resolution, None,
                )?)))
            }
            OutputFrameFormat::Nv12DmaBufWithAllocator(allocator) => {
                info!(
                    ?resolution,
                    "creating zero-copy NV12 DMA-BUF output texture from custom allocator"
                );
                Ok(Self::Nv12DmaBuf(Box::new(Nv12DmaBufOutput::new(
                    ctx,
                    resolution,
                    Some(allocator),
                )?)))
            }
        }
    }
}

pub struct Nv12DmaBufOutput {
    frames: Vec<PooledNv12DmaBufFrame>,
    next_index: usize,
    resolution: Resolution,
}

struct PooledNv12DmaBufFrame {
    dmabuf: Arc<DmaBufFrame>,
    texture: NV12Texture,
}

impl Nv12DmaBufOutput {
    const POOL_SIZE: usize = 16;

    fn new(
        ctx: &WgpuCtx,
        resolution: Resolution,
        allocator: Option<Arc<dyn DmaBufAllocator>>,
    ) -> Result<Self, CreateOutputTextureError> {
        let frames = (0..Self::POOL_SIZE)
            .map(|_| {
                let dmabuf = match allocator.as_ref() {
                    Some(allocator) => allocator
                        .allocate(&ctx.device, resolution)
                        .map_err(CreateOutputTextureError::DmaBufAllocation)?,
                    None => export_nv12_dmabuf_texture(&ctx.device, resolution),
                };
                validate_nv12_dmabuf(&dmabuf, resolution)?;
                let texture = NV12Texture::from_wgpu_texture(dmabuf.texture_arc())
                    .map_err(|_| CreateOutputTextureError::InvalidDmaBufTexture)?;
                Ok(PooledNv12DmaBufFrame { dmabuf, texture })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { frames, next_index: 0, resolution })
    }

    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    pub fn next_frame(&mut self) -> (&NV12Texture, Arc<DmaBufFrame>) {
        let index = self.next_index;
        self.next_index = (self.next_index + 1) % self.frames.len();
        let frame = &self.frames[index];
        (&frame.texture, Arc::clone(&frame.dmabuf))
    }
}

fn validate_nv12_dmabuf(
    dmabuf: &DmaBufFrame,
    expected: Resolution,
) -> Result<(), CreateOutputTextureError> {
    const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

    if dmabuf.fourcc() != DRM_FORMAT_NV12 {
        return Err(CreateOutputTextureError::InvalidDmaBufFourcc(dmabuf.fourcc()));
    }
    if dmabuf.resolution() != expected {
        return Err(CreateOutputTextureError::InvalidDmaBufResolution {
            actual: dmabuf.resolution(),
            expected,
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn export_nv12_dmabuf_texture(
    wgpu_device: &wgpu::Device,
    resolution: Resolution,
) -> Arc<DmaBufFrame> {
    const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

    unsafe {
        let hal_device_guard = wgpu_device
            .as_hal::<VkApi>()
            .expect("NV12 DMA-BUF output requires a Vulkan wgpu device");
        let hal_device = &*hal_device_guard;
        let vk_device = hal_device.raw_device().clone();
        let instance = hal_device.shared_instance().raw_instance();
        let physical_device = hal_device.raw_physical_device();
        let size = vk::Extent3D {
            width: resolution.width as u32,
            height: resolution.height as u32,
            depth: 1,
        };

        let modifier = select_nv12_modifier(instance, physical_device);
        let modifiers = [modifier];
        let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut drm_info = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&modifiers);
        let create_info = vk::ImageCreateInfo::default()
            .flags(
                vk::ImageCreateFlags::MUTABLE_FORMAT
                    | vk::ImageCreateFlags::EXTENDED_USAGE,
            )
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(size)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info)
            .push_next(&mut drm_info);

        let image = vk_device
            .create_image(&create_info, None)
            .expect("failed to create exportable NV12 Vulkan image");
        let mem_requirements = vk_device.get_image_memory_requirements(image);
        let memory_type_index =
            find_memory_type_index(instance, physical_device, &mem_requirements);
        let mut export_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut export_info)
            .push_next(&mut dedicated_info);
        let memory = vk_device
            .allocate_memory(&allocate_info, None)
            .expect("failed to allocate exportable NV12 Vulkan memory");
        vk_device
            .bind_image_memory(image, memory, 0)
            .expect("failed to bind exportable NV12 Vulkan memory");

        let external_memory_fd =
            ash::khr::external_memory_fd::Device::new(instance, &vk_device);
        let fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let fd = Arc::new(OwnedFd::from_raw_fd(
            external_memory_fd
                .get_memory_fd(&fd_info)
                .expect("failed to export NV12 DMA-BUF fd"),
        ));

        let plane0 = image_plane_layout(&vk_device, image, vk::ImageAspectFlags::PLANE_0);
        let plane1 = image_plane_layout(&vk_device, image, vk::ImageAspectFlags::PLANE_1);
        let texture = Arc::new(wrap_nv12_image_as_wgpu_texture(
            wgpu_device,
            image,
            memory,
            vk_device,
            resolution,
            "nv12 dma-buf output texture",
            true,
        ));

        Arc::new(DmaBufFrame::new(
            texture,
            DRM_FORMAT_NV12,
            resolution.width as u32,
            resolution.height as u32,
            vec![DmaBufObject {
                fd,
                size: mem_requirements
                    .size
                    .try_into()
                    .expect("DMA-BUF allocation is larger than VA-API can describe"),
                modifier,
            }],
            vec![DmaBufLayer {
                drm_format: DRM_FORMAT_NV12,
                planes: vec![
                    DmaBufPlane {
                        object_index: 0,
                        offset: plane0
                            .offset
                            .try_into()
                            .expect("NV12 Y offset does not fit u32"),
                        pitch: plane0
                            .row_pitch
                            .try_into()
                            .expect("NV12 Y pitch does not fit u32"),
                    },
                    DmaBufPlane {
                        object_index: 0,
                        offset: plane1
                            .offset
                            .try_into()
                            .expect("NV12 UV offset does not fit u32"),
                        pitch: plane1
                            .row_pitch
                            .try_into()
                            .expect("NV12 UV pitch does not fit u32"),
                    },
                ],
            }],
        ))
    }
}

#[cfg(target_os = "linux")]
pub fn import_nv12_dmabuf_texture(
    wgpu_device: &wgpu::Device,
    fourcc: u32,
    width: u32,
    height: u32,
    objects: Vec<DmaBufObject>,
    layers: Vec<DmaBufLayer>,
) -> Result<Arc<DmaBufFrame>, String> {
    import_nv12_dmabuf_texture_with_owner(
        wgpu_device,
        fourcc,
        width,
        height,
        objects,
        layers,
        None,
    )
}

#[cfg(target_os = "linux")]
pub fn import_nv12_dmabuf_texture_with_owner(
    wgpu_device: &wgpu::Device,
    fourcc: u32,
    width: u32,
    height: u32,
    objects: Vec<DmaBufObject>,
    layers: Vec<DmaBufLayer>,
    owner: Option<Arc<dyn DmaBufFrameOwner>>,
) -> Result<Arc<DmaBufFrame>, String> {
    import_nv12_dmabuf_texture_inner(
        wgpu_device,
        fourcc,
        width,
        height,
        objects,
        layers,
        owner,
        false,
    )
}

#[cfg(target_os = "linux")]
pub fn import_renderable_nv12_dmabuf_texture_with_owner(
    wgpu_device: &wgpu::Device,
    fourcc: u32,
    width: u32,
    height: u32,
    objects: Vec<DmaBufObject>,
    layers: Vec<DmaBufLayer>,
    owner: Option<Arc<dyn DmaBufFrameOwner>>,
) -> Result<Arc<DmaBufFrame>, String> {
    import_nv12_dmabuf_texture_inner(
        wgpu_device,
        fourcc,
        width,
        height,
        objects,
        layers,
        owner,
        true,
    )
}

#[cfg(target_os = "linux")]
fn import_nv12_dmabuf_texture_inner(
    wgpu_device: &wgpu::Device,
    fourcc: u32,
    width: u32,
    height: u32,
    objects: Vec<DmaBufObject>,
    layers: Vec<DmaBufLayer>,
    owner: Option<Arc<dyn DmaBufFrameOwner>>,
    render_attachment: bool,
) -> Result<Arc<DmaBufFrame>, String> {
    const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");
    if fourcc != DRM_FORMAT_NV12 {
        return Err(format!("expected NV12 DMA-BUF, got fourcc {fourcc}"));
    }
    if objects.len() != 1 || layers.len() != 1 {
        return Err(
            "only single-object single-layer NV12 DMA-BUF imports are supported".into()
        );
    }
    if layers[0].planes.len() != 2 {
        return Err("NV12 DMA-BUF import requires exactly two planes".into());
    }

    unsafe {
        let hal_device_guard = wgpu_device
            .as_hal::<VkApi>()
            .expect("NV12 DMA-BUF import requires a Vulkan wgpu device");
        let hal_device = &*hal_device_guard;
        let vk_device = hal_device.raw_device().clone();
        let instance = hal_device.shared_instance().raw_instance();
        let physical_device = hal_device.raw_physical_device();
        let size = vk::Extent3D { width, height, depth: 1 };
        let modifier = objects[0].modifier;
        let plane_layouts = layers[0]
            .planes
            .iter()
            .map(|plane| vk::SubresourceLayout {
                offset: plane.offset as u64,
                size: objects[plane.object_index].size as u64 - plane.offset as u64,
                row_pitch: plane.pitch as u64,
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect::<Vec<_>>();

        let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut drm_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);
        let mut usage = vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::TRANSFER_SRC;
        if render_attachment {
            usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
        }

        let create_info = vk::ImageCreateInfo::default()
            .flags(
                vk::ImageCreateFlags::MUTABLE_FORMAT
                    | vk::ImageCreateFlags::EXTENDED_USAGE,
            )
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(size)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info)
            .push_next(&mut drm_info);

        let image = vk_device.create_image(&create_info, None).map_err(|err| {
            format!("failed to create imported NV12 Vulkan image: {err}")
        })?;
        let mem_requirements = vk_device.get_image_memory_requirements(image);
        let memory_type_index =
            find_memory_type_index(instance, physical_device, &mem_requirements);
        let import_fd = objects[0]
            .fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|err| format!("failed to duplicate DMA-BUF fd: {err}"))?
            .into_raw_fd();
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(import_fd);
        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_info)
            .push_next(&mut dedicated_info);
        let memory = match vk_device.allocate_memory(&allocate_info, None) {
            Ok(memory) => memory,
            Err(err) => {
                vk_device.destroy_image(image, None);
                return Err(format!("failed to import NV12 DMA-BUF memory: {err}"));
            }
        };
        if let Err(err) = vk_device.bind_image_memory(image, memory, 0) {
            vk_device.destroy_image(image, None);
            vk_device.free_memory(memory, None);
            return Err(format!("failed to bind imported NV12 DMA-BUF memory: {err}"));
        }

        let texture = Arc::new(wrap_nv12_image_as_wgpu_texture(
            wgpu_device,
            image,
            memory,
            vk_device,
            Resolution { width: width as usize, height: height as usize },
            "imported nv12 dma-buf texture",
            render_attachment,
        ));

        Ok(Arc::new(DmaBufFrame::new_with_owner(
            texture, fourcc, width, height, objects, layers, owner,
        )))
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn import_nv12_dmabuf_texture(
    _device: &wgpu::Device,
    _fourcc: u32,
    _width: u32,
    _height: u32,
    _objects: Vec<DmaBufObject>,
    _layers: Vec<DmaBufLayer>,
) -> Result<Arc<DmaBufFrame>, String> {
    unreachable!("NV12 DMA-BUF import is only available on Linux")
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn import_nv12_dmabuf_texture_with_owner(
    _device: &wgpu::Device,
    _fourcc: u32,
    _width: u32,
    _height: u32,
    _objects: Vec<DmaBufObject>,
    _layers: Vec<DmaBufLayer>,
    _owner: Option<Arc<dyn DmaBufFrameOwner>>,
) -> Result<Arc<DmaBufFrame>, String> {
    unreachable!("NV12 DMA-BUF import is only available on Linux")
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn import_renderable_nv12_dmabuf_texture_with_owner(
    _device: &wgpu::Device,
    _fourcc: u32,
    _width: u32,
    _height: u32,
    _objects: Vec<DmaBufObject>,
    _layers: Vec<DmaBufLayer>,
    _owner: Option<Arc<dyn DmaBufFrameOwner>>,
) -> Result<Arc<DmaBufFrame>, String> {
    unreachable!("NV12 DMA-BUF import is only available on Linux")
}

#[cfg(not(target_os = "linux"))]
pub fn export_nv12_dmabuf_texture(
    _device: &wgpu::Device,
    _resolution: Resolution,
) -> Arc<DmaBufFrame> {
    unreachable!("NV12 DMA-BUF output is only available on Linux")
}

#[cfg(target_os = "linux")]
unsafe fn wrap_nv12_image_as_wgpu_texture(
    wgpu_device: &wgpu::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    device: ash::Device,
    resolution: Resolution,
    label: &'static str,
    render_attachment: bool,
) -> wgpu::Texture {
    let hal_device_guard = unsafe {
        wgpu_device
            .as_hal::<VkApi>()
            .expect("NV12 DMA-BUF output requires a Vulkan wgpu device")
    };
    let hal_texture = unsafe {
        let mut hal_usage = wgpu::TextureUses::RESOURCE
            | wgpu::TextureUses::COPY_DST
            | wgpu::TextureUses::COPY_SRC;
        if render_attachment {
            hal_usage |= wgpu::TextureUses::COLOR_TARGET;
        }

        (*hal_device_guard).texture_from_raw(
            image,
            &wgpu::hal::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: resolution.width as u32,
                    height: resolution.height as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12,
                usage: hal_usage,
                memory_flags: wgpu::hal::MemoryFlags::empty(),
                view_formats: vec![
                    wgpu::TextureFormat::R8Unorm,
                    wgpu::TextureFormat::Rg8Unorm,
                ],
            },
            Some(Box::new(move || {
                device.destroy_image(image, None);
                device.free_memory(memory, None);
            })),
            wgpu::hal::vulkan::TextureMemory::External,
        )
    };

    unsafe {
        let mut wgpu_usage = wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC;
        if render_attachment {
            wgpu_usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
        }

        wgpu_device.create_texture_from_hal::<VkApi>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: resolution.width as u32,
                    height: resolution.height as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12,
                usage: wgpu_usage,
                view_formats: &[
                    wgpu::TextureFormat::R8Unorm,
                    wgpu::TextureFormat::Rg8Unorm,
                ],
            },
        )
    }
}

#[cfg(target_os = "linux")]
unsafe fn image_plane_layout(
    device: &ash::Device,
    image: vk::Image,
    aspect_mask: vk::ImageAspectFlags,
) -> vk::SubresourceLayout {
    unsafe {
        device.get_image_subresource_layout(
            image,
            vk::ImageSubresource { aspect_mask, mip_level: 0, array_layer: 0 },
        )
    }
}

#[cfg(target_os = "linux")]
fn select_nv12_modifier(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> u64 {
    unsafe {
        let mut count = vk::DrmFormatModifierPropertiesList2EXT::default();
        let mut properties = vk::FormatProperties2::default().push_next(&mut count);
        instance.get_physical_device_format_properties2(
            physical_device,
            vk::Format::G8_B8R8_2PLANE_420_UNORM,
            &mut properties,
        );

        let mut modifiers = vec![
            vk::DrmFormatModifierProperties2EXT::default();
            count.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesList2EXT::default()
            .drm_format_modifier_properties(&mut modifiers);
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(
            physical_device,
            vk::Format::G8_B8R8_2PLANE_420_UNORM,
            &mut properties,
        );

        let required = vk::FormatFeatureFlags2::SAMPLED_IMAGE
            | vk::FormatFeatureFlags2::TRANSFER_DST
            | vk::FormatFeatureFlags2::TRANSFER_SRC;
        let supports_nv12_export = |modifier: &vk::DrmFormatModifierProperties2EXT| {
            modifier.drm_format_modifier_plane_count == 2
                && modifier.drm_format_modifier_tiling_features.contains(required)
        };

        modifiers
            .iter()
            .find(|modifier| {
                supports_nv12_export(modifier) && modifier.drm_format_modifier != 0
            })
            .or_else(|| modifiers.iter().find(|modifier| supports_nv12_export(modifier)))
            .copied()
            .expect("no exportable NV12 DRM modifier with transfer support available")
            .drm_format_modifier
    }
}

#[cfg(target_os = "linux")]
fn find_memory_type_index(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    mem_requirements: &vk::MemoryRequirements,
) -> u32 {
    let memory_properties =
        unsafe { instance.get_physical_device_memory_properties(physical_device) };

    (0..memory_properties.memory_type_count)
        .find(|index| {
            let allowed = mem_requirements.memory_type_bits & (1 << index) != 0;
            let flags = memory_properties.memory_types[*index as usize].property_flags;
            allowed && flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        })
        .expect("no device-local memory type available for exportable NV12 image")
}

pub struct PlanarYuvOutput {
    textures: PlanarYuvTextures,
    buffers: [wgpu::Buffer; 3],
    resolution: Resolution,
}

impl PlanarYuvOutput {
    pub fn new(
        ctx: &WgpuCtx,
        resolution: Resolution,
        pixel_format: PlanarYuvVariant,
    ) -> Self {
        let textures = PlanarYuvTextures::new(ctx, resolution, pixel_format);
        let buffers = textures.new_download_buffers(ctx);

        Self { textures, buffers, resolution }
    }

    pub fn yuv_textures(&self) -> &PlanarYuvTextures {
        &self.textures
    }

    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    pub fn start_download<'a>(
        &'a self,
        ctx: &WgpuCtx,
    ) -> PlanarYuvPendingDownload<
        'a,
        impl FnOnce() -> Result<bytes::Bytes, BufferAsyncError> + 'a,
        BufferAsyncError,
    > {
        self.textures.copy_to_buffers(ctx, &self.buffers);

        PlanarYuvPendingDownload::new(
            self.download_buffer(self.textures.plane_texture(0).size(), &self.buffers[0]),
            self.download_buffer(self.textures.plane_texture(1).size(), &self.buffers[1]),
            self.download_buffer(self.textures.plane_texture(2).size(), &self.buffers[2]),
        )
    }

    fn download_buffer<'a>(
        &'a self,
        size: wgpu::Extent3d,
        source: &'a Buffer,
    ) -> impl FnOnce() -> Result<bytes::Bytes, BufferAsyncError> + 'a {
        let buffer = bytes::BytesMut::with_capacity((size.width * size.height) as usize);
        let (s, r) = bounded(1);
        source.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            if let Err(err) = s.send(result) {
                error!("channel send error: {err}")
            }
        });

        move || {
            r.recv().unwrap()?;
            let mut buffer = buffer.writer();
            {
                let range = source.slice(..).get_mapped_range();
                let chunks = range.chunks(pad_to_256(size.width) as usize);
                for chunk in chunks {
                    buffer.write_all(&chunk[..size.width as usize]).unwrap();
                }
            };
            source.unmap();
            Ok(buffer.into_inner().into())
        }
    }
}
