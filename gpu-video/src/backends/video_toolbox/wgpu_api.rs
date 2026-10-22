use std::sync::Arc;

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_core_foundation as cf;
use objc2_core_video as cv;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal as mtl;
use objc2_metal::MTLDevice;

use crate::{
    EncodedOutputChunk, OutputFrame, VideoTexture, WgpuTexturesDecoderH264,
    adapter::VideoAdapterInfo,
    backends::{
        WgpuBackend,
        video_toolbox::{
            VTBackend, VTDevice,
            decoders_h264::VTDecoderH264,
            encoder::{H264Codec, H265Codec, VTEncoder},
            error::VTInitError,
        },
    },
    device::WgpuVideoDeviceBackend,
    global_registry::{GlobalRegistry, VideoDeviceKey},
};

use super::{caps, query_api_version};

impl WgpuBackend for VTBackend {
    fn device_key_from_wgpu_device(
        &self,
        device: &wgpu::Device,
    ) -> crate::global_registry::VideoDeviceKey {
        let hal = unsafe { device.as_hal::<wgpu::hal::metal::Api>().unwrap() };
        let registry_id = hal.raw_device().registryID();
        VideoDeviceKey::Metal { registry_id }
    }

    fn retrieve_adapter_info(
        &self,
        wgpu_adapter: &wgpu::Adapter,
    ) -> Option<crate::capabilities::VideoAdapterInfo> {
        let info = wgpu_adapter.get_info();
        let decode_capabilities = caps::query_decode_capabilities();
        let encode_capabilities = caps::query_encode_capabilities();

        Some(VideoAdapterInfo {
            name: info.name,
            driver_name: info.driver,
            driver_info: info.driver_info,
            device: info.device.to_string(),
            device_type: info.device_type.into(),
            vendor: info.vendor.to_string(),
            api_version: query_api_version(),
            supports_decoding: decode_capabilities.h264.is_some(),
            supports_encoding: encode_capabilities.h264.is_some()
                || encode_capabilities.h265.is_some(),
            decode_capabilities,
            encode_capabilities,
        })
    }

    fn create_and_register_device(
        &self,
        wgpu_adapter: &wgpu::Adapter,
        desc: &crate::parameters::VideoDeviceDescriptor,
    ) -> Result<(wgpu::Device, wgpu::Queue), crate::VideoDeviceInitError> {
        let (device, queue) =
            pollster::block_on(wgpu_adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("wgpu device created by the videotoolbox decoder"),
                required_features: desc.wgpu_features,
                required_limits: desc.wgpu_limits.clone(),
                experimental_features: desc.wgpu_experimental_features,
                ..Default::default()
            }))
            .map_err(crate::WgpuInitError::WgpuRequestDeviceError)
            .map_err(VTInitError::from)?;

        let id = VTBackend.device_key_from_wgpu_device(&device);
        // VTDevice is empty, and MTLDevices actually only get destroyed at process exit.
        // Because of this, we never remove from the registry.
        GlobalRegistry::register_device(id, Arc::new(VTDevice {}));
        Ok((device, queue))
    }
}

impl WgpuVideoDeviceBackend for VTDevice {
    fn create_wgpu_textures_decoder_h264(
        self: Arc<Self>,
        wgpu_device: wgpu::Device,
        _wgpu_queue: wgpu::Queue,
        parameters: crate::device::DecoderParameters,
        on_frame_callback: Box<dyn FnMut(OutputFrame<VideoTexture>) + Send>,
    ) -> Result<WgpuTexturesDecoderH264, crate::VideoDecoderError> {
        let backend = VTDecoderH264::new_wgpu_textures(wgpu_device, parameters, on_frame_callback)?;

        Ok(WgpuTexturesDecoderH264 {
            backend: Box::new(backend),
        })
    }

    fn create_wgpu_textures_encoder_h264(
        self: Arc<Self>,
        wgpu_device: wgpu::Device,
        wgpu_queue: wgpu::Queue,
        parameters: crate::device::EncoderParametersH264,
        on_chunk_callback: Box<dyn FnMut(EncodedOutputChunk<Vec<u8>>) + Send>,
    ) -> Result<crate::WgpuTexturesEncoderH264, crate::VideoEncoderError> {
        let encoder = VTEncoder::<H264Codec>::new_wgpu(
            &wgpu_device,
            parameters.input_parameters,
            parameters.output_parameters,
            parameters.max_in_flight_submissions.unwrap_or(3),
            on_chunk_callback,
        )?;

        Ok(crate::WgpuTexturesEncoderH264 {
            wgpu_device,
            wgpu_queue,
            backend: Box::new(encoder),
        })
    }

    fn create_wgpu_textures_encoder_h265(
        self: Arc<Self>,
        wgpu_device: wgpu::Device,
        wgpu_queue: wgpu::Queue,
        parameters: crate::device::EncoderParametersH265,
        on_chunk_callback: Box<dyn FnMut(EncodedOutputChunk<Vec<u8>>) + Send>,
    ) -> Result<crate::WgpuTexturesEncoderH265, crate::VideoEncoderError> {
        let encoder = VTEncoder::<H265Codec>::new_wgpu(
            &wgpu_device,
            parameters.input_parameters,
            parameters.output_parameters,
            parameters.max_in_flight_submissions.unwrap_or(3),
            on_chunk_callback,
        )?;

        Ok(crate::WgpuTexturesEncoderH265 {
            wgpu_device,
            wgpu_queue,
            backend: Box::new(encoder),
        })
    }
}

pub(crate) fn metal_device(
    device: &wgpu::Device,
) -> Result<Retained<ProtocolObject<dyn mtl::MTLDevice>>, VTInitError> {
    let hal =
        unsafe { device.as_hal::<wgpu::hal::metal::Api>() }.ok_or(VTInitError::NotMetalBackend)?;
    Ok(hal.raw_device().clone())
}

pub(crate) struct SendSyncCVBuffer(pub(crate) cf::CFRetained<cv::CVBuffer>);
unsafe impl Send for SendSyncCVBuffer {}
unsafe impl Sync for SendSyncCVBuffer {}

#[derive(Debug, thiserror::Error)]
pub enum MetalTextureError {
    #[error("The pixel buffer is not backed by an IOSurface")]
    NoIOSurface,

    #[error("Failed to create a Metal texture on plane {0} of the pixel buffer's IOSurface")]
    TextureCreationFailed(usize),
}

/// Textures over `buffer`'s IOSurface planes. Each holds `buffer`, which VideoToolbox reuses
/// only once released: unlike a `CVMetalTextureCache` texture, whose release updates the
/// cache, dropping one on any thread touches no shared state.
pub(crate) fn video_texture_from_pixel_buffer(
    metal_device: &ProtocolObject<dyn mtl::MTLDevice>,
    device: &wgpu::Device,
    buffer: &cf::CFRetained<cv::CVBuffer>,
    metal_usage: mtl::MTLTextureUsage,
    usage: wgpu::TextureUsages,
    initial_use: wgpu::TextureUses,
    label: &str,
) -> Result<VideoTexture, MetalTextureError> {
    let surface =
        cv::CVPixelBufferGetIOSurface(Some(buffer)).ok_or(MetalTextureError::NoIOSurface)?;

    let y_texture = plane_texture_from_pixel_buffer(
        metal_device,
        device,
        buffer,
        &surface,
        0,
        wgpu::TextureFormat::R8Unorm,
        mtl::MTLPixelFormat::R8Unorm,
        metal_usage,
        usage,
        initial_use,
        &format!("{label} y plane"),
    )?;
    let uv_texture = plane_texture_from_pixel_buffer(
        metal_device,
        device,
        buffer,
        &surface,
        1,
        wgpu::TextureFormat::Rg8Unorm,
        mtl::MTLPixelFormat::RG8Unorm,
        metal_usage,
        usage,
        initial_use,
        &format!("{label} uv plane"),
    )?;

    Ok(VideoTexture::from_planes(y_texture, uv_texture))
}

#[expect(clippy::too_many_arguments)]
fn plane_texture_from_pixel_buffer(
    metal_device: &ProtocolObject<dyn mtl::MTLDevice>,
    device: &wgpu::Device,
    buffer: &cf::CFRetained<cv::CVBuffer>,
    surface: &IOSurfaceRef,
    plane_index: usize,
    format: wgpu::TextureFormat,
    mtl_format: mtl::MTLPixelFormat,
    metal_usage: mtl::MTLTextureUsage,
    usage: wgpu::TextureUsages,
    initial_use: wgpu::TextureUses,
    label: &str,
) -> Result<wgpu::Texture, MetalTextureError> {
    let width = cv::CVPixelBufferGetWidthOfPlane(buffer, plane_index);
    let height = cv::CVPixelBufferGetHeightOfPlane(buffer, plane_index);

    let descriptor = unsafe {
        mtl::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            mtl_format, width, height, false,
        )
    };
    descriptor.setUsage(metal_usage);
    let mtl_texture = metal_device
        .newTextureWithDescriptor_iosurface_plane(&descriptor, surface, plane_index)
        .ok_or(MetalTextureError::TextureCreationFailed(plane_index))?;
    let guard = SendSyncCVBuffer(buffer.clone());

    let size = wgpu::Extent3d {
        width: width as u32,
        height: height as u32,
        depth_or_array_layers: 1,
    };

    unsafe {
        let hal_texture = wgpu::hal::metal::Device::texture_from_raw(
            mtl_texture,
            format,
            mtl::MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent {
                width: size.width,
                height: size.height,
                depth: 1,
            },
            Some(Box::new(move || drop(guard))),
        );

        Ok(device.create_texture_from_hal::<wgpu::hal::metal::Api>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            },
            initial_use,
        ))
    }
}
