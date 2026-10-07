use std::sync::Arc;

use smelter_render::{Framerate, Resolution};

use crate::queue::QueueInputOptions;

#[derive(Debug, Clone, PartialEq)]
pub struct V4l2InputOptions {
    pub path: Arc<std::path::Path>,
    pub resolution: Option<Resolution>,
    pub format: V4l2Format,
    pub framerate: Option<Framerate>,
    /// Capture into host memory the GPU copies from, so the CPU never copies
    /// frames. Requires a video side channel, a GPU that can read host memory,
    /// and YUYV frames whose rows the GPU can copy.
    pub zero_copy: bool,
    pub queue_options: QueueInputOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V4l2Format {
    Yuyv,
    Nv12,
}

#[derive(Debug, thiserror::Error)]
pub enum V4l2InputError {
    #[error("Device does not support video capture")]
    CaptureNotSupported,

    #[error("Opening device {0} failed")]
    OpeningDeviceFailed(Arc<std::path::Path>, std::io::Error),

    #[error("Device IO error.")]
    IoError(#[from] std::io::Error),

    #[error("Device is set to an unsupported format: {0}.")]
    UnsupportedFormat(String),

    #[error("Zero-copy capture requires a video side channel.")]
    ZeroCopyWithoutSideChannel,

    #[error("Zero-copy capture does not support {0:?} frames of {1} bytes per row.")]
    ZeroCopyUnsupportedFormat(V4l2Format, u32),

    #[error("The device can't capture into user memory, required by zero-copy capture.")]
    ZeroCopyUnsupportedByDevice,

    #[cfg(target_os = "linux")]
    #[error(transparent)]
    ZeroCopyHostMemory(#[from] gpu_video::HostMemoryError),
}
