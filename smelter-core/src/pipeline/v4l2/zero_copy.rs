use std::{io, mem, os::raw::c_void, sync::Arc};

use gpu_video::HostMemoryBuffer;
use smelter_render::{BufferFormat, BufferLayout, FramePreProcessor, Resolution, WgpuCtx};
use tracing::warn;
use v4l::{
    FourCC, buffer::Type, device::Handle, memory::Memory, prelude::*, v4l_sys::*, v4l2,
    video::Capture,
};

use crate::prelude::*;

use super::v4l2_input::frame_ready;

/// The driver keeps the others queued while one frame is copied.
const BUFFER_COUNT: u32 = 4;

/// Captures into host memory the GPU copies from (`V4L2_MEMORY_USERPTR`), so
/// the CPU never touches the pixels. A buffer goes back to the driver once its
/// frame's copy finished, checked when the next frame uploads, so at most two
/// buffers are out of the driver at once.
pub(super) struct ZeroCopyStream {
    handle: Arc<Handle>,
    device: Arc<wgpu::Device>,
    pre_processor: FramePreProcessor,
    buffers: Vec<HostMemoryBuffer>,
    layout: BufferLayout,
    copying: Option<(u32, wgpu::SubmissionIndex)>,
}

impl ZeroCopyStream {
    pub fn start(device: &Device, wgpu_ctx: Arc<WgpuCtx>) -> Result<Self, V4l2InputError> {
        let format = device.format()?;
        if format.fourcc != FourCC::from(V4l2Format::Yuyv)
            || !format
                .stride
                .is_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        {
            return Err(V4l2InputError::ZeroCopyUnsupportedFormat(
                format.fourcc.try_into()?,
                format.stride,
            ));
        }
        let handle = device.handle();
        let mut request = request_buffers(BUFFER_COUNT);
        // SAFETY: VIDIOC_REQBUFS takes a `v4l2_requestbuffers`.
        unsafe { ioctl(&handle, v4l2::vidioc::VIDIOC_REQBUFS, &mut request) }.map_err(|err| {
            match err.raw_os_error() {
                Some(libc::EINVAL) => V4l2InputError::ZeroCopyUnsupportedByDevice,
                _ => err.into(),
            }
        })?;
        let buffers = (0..request.count)
            .map(|_| HostMemoryBuffer::new(&wgpu_ctx.device, format.size as usize))
            .collect::<Result<Vec<_>, _>>()?;
        let stream = Self {
            handle,
            device: wgpu_ctx.device.clone(),
            pre_processor: FramePreProcessor::new(wgpu_ctx),
            buffers,
            layout: BufferLayout {
                format: BufferFormat::InterleavedYuyv422,
                resolution: Resolution {
                    width: format.width as usize,
                    height: format.height as usize,
                },
                bytes_per_row: format.stride,
            },
            copying: None,
        };
        for index in 0..request.count {
            stream.queue(index)?;
        }
        let mut kind = Type::VideoCapture as u32;
        // SAFETY: VIDIOC_STREAMON takes the buffer type.
        unsafe { ioctl(&stream.handle, v4l2::vidioc::VIDIOC_STREAMON, &mut kind)? };
        Ok(stream)
    }

    pub fn resolution(&self) -> Resolution {
        self.layout.resolution
    }

    /// The next captured frame, `None` when none arrived in time or it was
    /// cut short.
    pub fn next(&mut self) -> io::Result<Option<Arc<wgpu::Texture>>> {
        if !frame_ready(&self.handle)? {
            return Ok(None);
        }
        let mut captured = descriptor();
        // SAFETY: VIDIOC_DQBUF takes a `v4l2_buffer`.
        unsafe { ioctl(&self.handle, v4l2::vidioc::VIDIOC_DQBUF, &mut captured)? };
        let frame_size = self.layout.bytes_per_row as usize * self.layout.resolution.height;
        if (captured.bytesused as usize) < frame_size {
            warn!(
                bytes_used = captured.bytesused,
                frame_size, "Dropping a frame shorter than its format"
            );
            self.queue(captured.index)?;
            return Ok(None);
        }
        let buffer = self.buffers[captured.index as usize].buffer();
        let (texture, copy) = self.pre_processor.process_buffer(buffer, self.layout);
        if let Some((index, previous)) = self.copying.replace((captured.index, copy)) {
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(previous),
                    timeout: None,
                })
                .map_err(io::Error::other)?;
            self.queue(index)?;
        }
        Ok(Some(texture))
    }

    fn queue(&self, index: u32) -> io::Result<()> {
        let memory = &self.buffers[index as usize];
        let mut buffer = v4l2_buffer {
            index,
            m: v4l2_buffer__bindgen_ty_1 {
                userptr: memory.as_ptr().as_ptr() as std::ffi::c_ulong,
            },
            length: memory.buffer().size() as u32,
            ..descriptor()
        };
        // SAFETY: VIDIOC_QBUF takes a `v4l2_buffer`, whose user pointer stays
        // valid until the buffers are released in `drop`.
        unsafe { ioctl(&self.handle, v4l2::vidioc::VIDIOC_QBUF, &mut buffer) }
    }
}

/// The driver lets go of every buffer before the memory is freed. wgpu frees
/// it once the copies reading it finished.
impl Drop for ZeroCopyStream {
    fn drop(&mut self) {
        let mut kind = Type::VideoCapture as u32;
        // SAFETY: VIDIOC_STREAMOFF takes the buffer type.
        if let Err(err) = unsafe { ioctl(&self.handle, v4l2::vidioc::VIDIOC_STREAMOFF, &mut kind) }
        {
            warn!(%err, "Failed to stop V4L2 streaming");
        }
        let mut release = request_buffers(0);
        // SAFETY: VIDIOC_REQBUFS takes a `v4l2_requestbuffers`.
        if let Err(err) = unsafe { ioctl(&self.handle, v4l2::vidioc::VIDIOC_REQBUFS, &mut release) }
        {
            warn!(%err, "Failed to release V4L2 buffers");
        }
    }
}

fn request_buffers(count: u32) -> v4l2_requestbuffers {
    v4l2_requestbuffers {
        count,
        type_: Type::VideoCapture as u32,
        memory: Memory::UserPtr as u32,
        ..unsafe { mem::zeroed() }
    }
}

fn descriptor() -> v4l2_buffer {
    v4l2_buffer {
        type_: Type::VideoCapture as u32,
        memory: Memory::UserPtr as u32,
        ..unsafe { mem::zeroed() }
    }
}

/// # Safety
/// `T` must be the argument type `request` reads and writes.
unsafe fn ioctl<T>(
    handle: &Handle,
    request: v4l2::vidioc::_IOC_TYPE,
    argument: &mut T,
) -> io::Result<()> {
    unsafe { v4l2::ioctl(handle.fd(), request, argument as *mut T as *mut c_void) }
}
