use std::{
    collections::HashMap,
    ffi::c_void,
    ptr::{NonNull, null_mut},
    sync::{Arc, Mutex},
};

use ash::vk;
use smelter_render::{FrameData, WgpuCtx};
use tracing::warn;

use crate::prelude::*;

/// DeckLink captures into its buffers in turn, so a frame's buffer is
/// overwritten `POOL_SIZE - 1` frames after delivery, references or not: the
/// GPU copy has that long to run. DeckLink asks for 14 buffers up front but
/// captures cleanly with fewer.
const POOL_SIZE: usize = 6;
/// The driver pins and IOMMU-maps every frame it captures, which costs about a
/// quarter as much on transparent huge pages.
const HUGE_PAGE: usize = 2 << 20;

/// Host memory DeckLink captures into and the GPU copies from, so captured
/// pixels never cross the CPU.
pub(super) struct FramePool {
    device: wgpu::Device,
    vulkan: ash::Device,
    host_memory: ash::ext::external_memory_host::Device,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    /// Keyed by address. DeckLink keeps its buffers until the format changes
    /// or capture stops, so released buffers are dropped rather than reused.
    lent: Mutex<HashMap<usize, HostBuffer>>,
}

struct HostBuffer {
    len: usize,
    buffer: wgpu::Buffer,
}

impl FramePool {
    pub fn new(ctx: &WgpuCtx) -> Result<Arc<Self>, DeckLinkInputError> {
        let hal = unsafe { ctx.device.as_hal::<wgpu::hal::api::Vulkan>() }
            .filter(|hal| {
                hal.enabled_device_extensions()
                    .contains(&ash::ext::external_memory_host::NAME)
            })
            .ok_or(DeckLinkInputError::ZeroCopyUnsupportedByGpu)?;
        let vulkan = hal.raw_device().clone();
        let memory_properties = unsafe {
            hal.shared_instance()
                .raw_instance()
                .get_physical_device_memory_properties(hal.raw_physical_device())
        };
        let host_memory = ash::ext::external_memory_host::Device::new(
            hal.shared_instance().raw_instance(),
            &vulkan,
        );
        Ok(Arc::new(Self {
            device: ctx.device.as_ref().clone(),
            vulkan,
            host_memory,
            memory_properties,
            lent: Default::default(),
        }))
    }

    /// The buffer DeckLink captured `frame` into, when its bytes are untouched.
    pub fn buffer(&self, frame: &Frame) -> Option<wgpu::Buffer> {
        let (FrameData::InterleavedUyvy422(data) | FrameData::Bgra(data)) = &frame.data else {
            return None;
        };
        let lent = self.lent.lock().unwrap();
        Some(lent.get(&(data.as_ptr() as usize))?.buffer.clone())
    }

    fn import(&self, len: usize) -> Result<(usize, HostBuffer), vk::Result> {
        let memory = HostMemory::import(self, len)?;
        let address = memory.address;
        let hal_buffer = unsafe {
            wgpu::hal::vulkan::Buffer::from_raw_externally_owned(
                memory.buffer,
                Box::new(move || drop(memory)),
            )
        };
        let buffer = unsafe {
            self.device
                .create_buffer_from_hal::<wgpu::hal::api::Vulkan>(
                    hal_buffer,
                    &wgpu::BufferDescriptor {
                        label: Some("DeckLink capture buffer"),
                        size: len as u64,
                        usage: wgpu::BufferUsages::COPY_SRC,
                        mapped_at_creation: false,
                    },
                )
        };
        Ok((address, HostBuffer { len, buffer }))
    }
}

impl decklink::FrameAllocator for FramePool {
    fn allocate(&self, size: usize, row_bytes: usize) -> Option<NonNull<u8>> {
        if !(row_bytes as u32).is_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) {
            warn!(row_bytes, "The GPU can't copy rows of this DeckLink format");
            return None;
        }
        let len = size.next_multiple_of(HUGE_PAGE);
        let mut lent = self.lent.lock().unwrap();
        // DeckLink allocates a new format's buffers before releasing the
        // previous format's.
        if lent.values().filter(|buffer| buffer.len == len).count() >= POOL_SIZE {
            return None;
        }
        let (address, buffer) = self
            .import(len)
            .inspect_err(|err| warn!(%err, "Failed to import a DeckLink capture buffer"))
            .ok()?;
        lent.insert(address, buffer);
        NonNull::new(address as *mut u8)
    }

    fn release(&self, bytes: NonNull<u8>) {
        self.lent
            .lock()
            .unwrap()
            .remove(&(bytes.as_ptr() as usize))
            .expect("DeckLink released a buffer it wasn't lent");
    }
}

/// Huge-page-backed host memory imported into Vulkan. Dropping frees whatever
/// was created, the Vulkan objects before the memory under them.
struct HostMemory {
    vulkan: ash::Device,
    address: usize,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    driver_allocated: bool,
}

impl HostMemory {
    fn import(pool: &FramePool, len: usize) -> Result<Self, vk::Result> {
        if std::env::var_os("SMELTER_PROBE_DRIVER_MEMORY").is_some() {
            return Self::driver_allocated(pool, len);
        }
        let mut address = null_mut();
        if unsafe { libc::posix_memalign(&mut address, HUGE_PAGE, len) } != 0 {
            return Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY);
        }
        let mut memory = Self {
            vulkan: pool.vulkan.clone(),
            address: address as usize,
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            driver_allocated: false,
        };
        unsafe { libc::madvise(address, len, libc::MADV_HUGEPAGE) };

        let handle_type = vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT;
        let mut host_properties = vk::MemoryHostPointerPropertiesEXT::default();
        unsafe {
            (pool.host_memory.fp().get_memory_host_pointer_properties_ext)(
                pool.vulkan.handle(),
                handle_type,
                address,
                &mut host_properties,
            )
        }
        .result()?;
        memory.buffer = unsafe {
            pool.vulkan.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(len as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .push_next(
                        &mut vk::ExternalMemoryBufferCreateInfo::default()
                            .handle_types(handle_type),
                    ),
                None,
            )?
        };
        let requirements = unsafe { pool.vulkan.get_buffer_memory_requirements(memory.buffer) };
        let memory_type_bits = host_properties.memory_type_bits & requirements.memory_type_bits;
        memory.memory = unsafe {
            pool.vulkan.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(len as u64)
                    .memory_type_index(memory_type_bits.trailing_zeros())
                    .push_next(
                        &mut vk::ImportMemoryHostPointerInfoEXT::default()
                            .handle_type(handle_type)
                            .host_pointer(address),
                    ),
                None,
            )?
        };
        unsafe {
            pool.vulkan
                .bind_buffer_memory(memory.buffer, memory.memory, 0)?
        };
        Ok(memory)
    }
}

impl HostMemory {
    fn driver_allocated(pool: &FramePool, len: usize) -> Result<Self, vk::Result> {
        let mut memory = Self {
            vulkan: pool.vulkan.clone(),
            address: 0,
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            driver_allocated: true,
        };
        memory.buffer = unsafe {
            pool.vulkan.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(len as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                None,
            )?
        };
        let requirements = unsafe { pool.vulkan.get_buffer_memory_requirements(memory.buffer) };
        let props = &pool.memory_properties;
        let wanted = |flags: vk::MemoryPropertyFlags| {
            (0..props.memory_type_count).find(|&i| {
                requirements.memory_type_bits & (1 << i) != 0
                    && props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
        };
        let index = wanted(
            vk::MemoryPropertyFlags::HOST_VISIBLE
                | vk::MemoryPropertyFlags::HOST_COHERENT
                | vk::MemoryPropertyFlags::HOST_CACHED,
        )
        .or_else(|| {
            wanted(vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)
        })
        .ok_or(vk::Result::ERROR_FEATURE_NOT_PRESENT)?;
        warn!(index, flags = ?props.memory_types[index as usize].property_flags, "ZCPROBE driver memory type");
        memory.memory = unsafe {
            pool.vulkan.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(requirements.size)
                    .memory_type_index(index),
                None,
            )?
        };
        unsafe {
            pool.vulkan
                .bind_buffer_memory(memory.buffer, memory.memory, 0)?
        };
        let mapped = unsafe {
            pool.vulkan.map_memory(
                memory.memory,
                0,
                vk::WHOLE_SIZE,
                vk::MemoryMapFlags::empty(),
            )?
        };
        memory.address = mapped as usize;
        warn!(address = memory.address, "ZCPROBE driver memory mapped");
        Ok(memory)
    }
}

impl Drop for HostMemory {
    fn drop(&mut self) {
        unsafe {
            self.vulkan.destroy_buffer(self.buffer, None);
            self.vulkan.free_memory(self.memory, None);
            if !self.driver_allocated {
                libc::free(self.address as *mut c_void);
            }
        }
    }
}
