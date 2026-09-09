#![doc = include_str!("../README.md")]

#[cfg(feature = "expose-parsers")]
pub mod parser;
#[cfg(not(feature = "expose-parsers"))]
pub(crate) mod parser;

// TODO: The modules below should compile on macos
#[cfg(all(vulkan, feature = "expose-backends"))]
pub mod backends;
#[cfg(all(vulkan, not(feature = "expose-backends")))]
pub(crate) mod backends;

#[cfg(vulkan)]
mod adapter;
#[cfg(vulkan)]
pub mod capabilities;
#[cfg(vulkan)]
pub(crate) mod decoders;
#[cfg(vulkan)]
mod device;
#[cfg(vulkan)]
pub(crate) mod encoders;
#[cfg(vulkan)]
mod frame_sorter;
#[cfg(all(vulkan, feature = "wgpu"))]
mod global_registry;
#[cfg(vulkan)]
mod instance;
#[cfg(all(vulkan, feature = "transcoder"))]
mod transcoder;
#[cfg(all(vulkan, feature = "wgpu"))]
pub(crate) mod wgpu_helpers;

#[cfg(vulkan)]
mod exports;
#[cfg(vulkan)]
pub use exports::*;

#[cfg(all(feature = "quicksync", target_os = "linux"))]
mod dmabuf;
#[cfg(all(feature = "quicksync", target_os = "linux"))]
pub mod quicksync;

#[cfg(all(feature = "quicksync", target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoResolution {
    pub width: u32,
    pub height: u32,
}

#[cfg(all(feature = "quicksync", target_os = "linux"))]
impl VideoResolution {
    pub(crate) fn extent_2d(self) -> wgpu::Extent3d {
        wgpu::Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        }
    }
}

#[cfg(all(feature = "quicksync", target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoFramerate {
    pub num: std::num::NonZeroU32,
    pub den: std::num::NonZeroU32,
}

#[cfg(all(feature = "quicksync", target_os = "linux"))]
impl VideoFramerate {
    pub fn new(num: u32, den: u32) -> Option<Self> {
        Some(Self {
            num: std::num::NonZeroU32::new(num)?,
            den: std::num::NonZeroU32::new(den)?,
        })
    }
}

// If vulkan is unsupported and parsers are not exposed
#[cfg(not(any(vulkan, feature = "expose-parsers")))]
compile_error!("gpu-video can be only compiled on platforms supported by vulkan.");
