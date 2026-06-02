use std::{io::Write, sync::Arc};

#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};

#[cfg(target_os = "linux")]
use ash::vk;
use bytes::BufMut;
use crossbeam_channel::bounded;
use tracing::{error, info, warn};
#[cfg(target_os = "linux")]
use wgpu::hal::api::Vulkan as VkApi;
use wgpu::{Buffer, BufferAsyncError};

use crate::{
    DmaBufFrame, OutputFrameFormat, Resolution,
    wgpu::{
        WgpuCtx,
        texture::{
            PlanarYuvPendingDownload, PlanarYuvTextures, PlanarYuvVariant,
            utils::pad_to_256,
        },
    },
};
#[cfg(target_os = "linux")]
use crate::{DmaBufLayer, DmaBufObject};

pub enum OutputTexture {
    PlanarYuvTextures(Box<PlanarYuvOutput>),
    Rgba8UnormWgpuTexture { resolution: Resolution },
    Nv12WgpuTexture { resolution: Resolution },
    Nv12DmaBuf { resolution: Resolution },
}

impl OutputTexture {
    pub fn new(ctx: &WgpuCtx, resolution: Resolution, format: OutputFrameFormat) -> Self {
        match format {
            OutputFrameFormat::PlanarYuv420Bytes => {
                warn!(
                    ?resolution,
                    "creating planar YUV output texture with CPU readback"
                );
                Self::PlanarYuvTextures(Box::new(PlanarYuvOutput::new(
                    ctx,
                    resolution,
                    PlanarYuvVariant::YUV420,
                )))
            }
            OutputFrameFormat::PlanarYuv422Bytes => {
                warn!(
                    ?resolution,
                    "creating planar YUV output texture with CPU readback"
                );
                Self::PlanarYuvTextures(Box::new(PlanarYuvOutput::new(
                    ctx,
                    resolution,
                    PlanarYuvVariant::YUV422,
                )))
            }
            OutputFrameFormat::PlanarYuv444Bytes => {
                warn!(
                    ?resolution,
                    "creating planar YUV output texture with CPU readback"
                );
                Self::PlanarYuvTextures(Box::new(PlanarYuvOutput::new(
                    ctx,
                    resolution,
                    PlanarYuvVariant::YUV444,
                )))
            }
            OutputFrameFormat::RgbaWgpuTexture => {
                Self::Rgba8UnormWgpuTexture { resolution }
            }
            OutputFrameFormat::Nv12WgpuTexture => Self::Nv12WgpuTexture { resolution },
            OutputFrameFormat::Nv12DmaBuf => {
                info!(?resolution, "creating zero-copy NV12 DMA-BUF output texture");
                Self::Nv12DmaBuf { resolution }
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub fn export_nv12_dmabuf_texture(
    ctx: &WgpuCtx,
    resolution: Resolution,
) -> Arc<DmaBufFrame> {
    const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

    unsafe {
        let hal_device_guard = ctx
            .device
            .as_hal::<VkApi>()
            .expect("NV12 DMA-BUF output requires a Vulkan wgpu device");
        let hal_device = &*hal_device_guard;
        let device = hal_device.raw_device().clone();
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
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info)
            .push_next(&mut drm_info);

        let image = device
            .create_image(&create_info, None)
            .expect("failed to create exportable NV12 Vulkan image");
        let mem_requirements = device.get_image_memory_requirements(image);
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
        let memory = device
            .allocate_memory(&allocate_info, None)
            .expect("failed to allocate exportable NV12 Vulkan memory");
        device
            .bind_image_memory(image, memory, 0)
            .expect("failed to bind exportable NV12 Vulkan memory");

        let external_memory_fd =
            ash::khr::external_memory_fd::Device::new(instance, &device);
        let fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let fd = Arc::new(OwnedFd::from_raw_fd(
            external_memory_fd
                .get_memory_fd(&fd_info)
                .expect("failed to export NV12 DMA-BUF fd"),
        ));

        let plane0 = image_plane_layout(&device, image, vk::ImageAspectFlags::PLANE_0);
        let plane1 = image_plane_layout(&device, image, vk::ImageAspectFlags::PLANE_1);

        let texture = Arc::new(wrap_nv12_image_as_wgpu_texture(
            ctx, image, memory, device, resolution,
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
                object_index: vec![0, 0],
                offset: vec![
                    plane0.offset.try_into().expect("NV12 Y offset does not fit u32"),
                    plane1.offset.try_into().expect("NV12 UV offset does not fit u32"),
                ],
                pitch: vec![
                    plane0.row_pitch.try_into().expect("NV12 Y pitch does not fit u32"),
                    plane1.row_pitch.try_into().expect("NV12 UV pitch does not fit u32"),
                ],
            }],
        ))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn export_nv12_dmabuf_texture(
    _ctx: &WgpuCtx,
    _resolution: Resolution,
) -> Arc<DmaBufFrame> {
    unreachable!("NV12 DMA-BUF output is only available on Linux")
}

#[cfg(target_os = "linux")]
unsafe fn wrap_nv12_image_as_wgpu_texture(
    ctx: &WgpuCtx,
    image: vk::Image,
    memory: vk::DeviceMemory,
    device: ash::Device,
    resolution: Resolution,
) -> wgpu::Texture {
    let hal_device_guard = unsafe {
        ctx.device
            .as_hal::<VkApi>()
            .expect("NV12 DMA-BUF output requires a Vulkan wgpu device")
    };
    let hal_texture = unsafe {
        (*hal_device_guard).texture_from_raw(
            image,
            &wgpu::hal::TextureDescriptor {
                label: Some("nv12 dma-buf output texture"),
                size: wgpu::Extent3d {
                    width: resolution.width as u32,
                    height: resolution.height as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12,
                usage: wgpu::TextureUses::RESOURCE
                    | wgpu::TextureUses::COPY_DST
                    | wgpu::TextureUses::COPY_SRC,
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
        ctx.device.create_texture_from_hal::<VkApi>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("nv12 dma-buf output texture"),
                size: wgpu::Extent3d {
                    width: resolution.width as u32,
                    height: resolution.height as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::NV12,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC,
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
        modifiers
            .into_iter()
            .find(|modifier| {
                modifier.drm_format_modifier_plane_count == 2
                    && modifier.drm_format_modifier_tiling_features.contains(required)
            })
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
