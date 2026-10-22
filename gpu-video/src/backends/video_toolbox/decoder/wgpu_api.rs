use objc2::runtime::ProtocolObject;
use objc2_core_foundation as cf;
use objc2_core_video as cv;
use objc2_metal as mtl;

use crate::VideoTexture;
use crate::backends::video_toolbox::{
    error::VTDecoderError, wgpu_api::video_texture_from_pixel_buffer,
};

pub(crate) fn to_video_texture(
    metal_device: &ProtocolObject<dyn mtl::MTLDevice>,
    device: &wgpu::Device,
    buffer: &cf::CFRetained<cv::CVBuffer>,
) -> Result<VideoTexture, VTDecoderError> {
    Ok(video_texture_from_pixel_buffer(
        metal_device,
        device,
        buffer,
        mtl::MTLTextureUsage::ShaderRead,
        wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::TEXTURE_BINDING,
        wgpu::TextureUses::RESOURCE,
        "gpu-video output",
    )?)
}
