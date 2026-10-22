use crate::{
    Resolution,
    wgpu::common_pipeline::{PRIMITIVE_STATE, Sampler, Vertex},
};

use super::WgpuCtx;

/// Makes a picture composited over black or white transparent, from its alpha coded at half its
/// size: rebuilds the alpha at full size along the picture's luma, then unkeys the picture's colour
/// and premultiplies it.
#[derive(Debug)]
pub struct AlphaRebuilder {
    texel_moments: wgpu::RenderPipeline,
    texel_alphas: wgpu::RenderPipeline,
    pixels: wgpu::RenderPipeline,
    sampler: Sampler,
}

/// The reduced grid's intermediates, at the coded alpha's size.
pub struct AlphaGrid {
    pub resolution: Resolution,
    moments: wgpu::TextureView,
    alphas: wgpu::TextureView,
}

impl AlphaGrid {
    pub fn new(ctx: &WgpuCtx, resolution: Resolution) -> Self {
        let view = |format| {
            ctx.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("alpha grid"),
                    size: wgpu::Extent3d {
                        width: resolution.width as u32,
                        height: resolution.height as u32,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        Self {
            resolution,
            moments: view(wgpu::TextureFormat::Rgba16Float),
            alphas: view(wgpu::TextureFormat::Rgba8Unorm),
        }
    }
}

impl AlphaRebuilder {
    /// Writes to views of `target_format`: through an sRGB view the colour is premultiplied in
    /// linear space, where premultiplied blending happens.
    pub fn new(
        device: &wgpu::Device,
        textures_layout: &wgpu::BindGroupLayout,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let shader_module = device.create_shader_module(wgpu::include_wgsl!("rebuild_alpha.wgsl"));
        let sampler = Sampler::new(device);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Rebuild alpha render pipeline layout"),
            bind_group_layouts: &[Some(textures_layout), Some(&sampler.bind_group_layout)],
            immediate_size: 0,
        });
        let srgb_target = [("SRGB_TARGET", f64::from(u8::from(target_format.is_srgb())))];
        let pipeline = |entry_point, format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry_point),
                layout: Some(&pipeline_layout),
                primitive: PRIMITIVE_STATE,
                vertex: wgpu::VertexState {
                    module: &shader_module,
                    entry_point: Some("vs_main"),
                    buffers: &[Some(Vertex::LAYOUT)],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader_module,
                    entry_point: Some(entry_point),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        write_mask: wgpu::ColorWrites::all(),
                        blend: None,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants: &srgb_target,
                        ..Default::default()
                    },
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                depth_stencil: None,
                cache: None,
            })
        };

        Self {
            texel_moments: pipeline("texel_moments", wgpu::TextureFormat::Rgba16Float),
            texel_alphas: pipeline("texel_alphas", wgpu::TextureFormat::Rgba8Unorm),
            pixels: pipeline("pixels", target_format),
            sampler,
        }
    }

    /// `coded` is the picture's limited-range luma, then its coded alpha's luma and chroma;
    /// `picture` its colour as stored, never through an sRGB view.
    pub fn rebuild(
        &self,
        ctx: &WgpuCtx,
        grid: &AlphaGrid,
        [luma, matte, keys]: [&wgpu::TextureView; 3],
        picture: &wgpu::TextureView,
        dst_view: &wgpu::TextureView,
    ) {
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Rebuild alpha encoder"),
            });
        for (pipeline, textures, target) in [
            (&self.texel_moments, [luma, matte, keys], &grid.moments),
            (
                &self.texel_alphas,
                [luma, &grid.moments, keys],
                &grid.alphas,
            ),
            (&self.pixels, [picture, &grid.alphas, keys], dst_view),
        ] {
            let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Rebuild alpha bind group"),
                layout: &ctx.format.planar_yuv_layout,
                entries: &textures
                    .into_iter()
                    .zip(0..)
                    .map(|(view, binding)| wgpu::BindGroupEntry {
                        binding,
                        resource: wgpu::BindingResource::TextureView(view),
                    })
                    .collect::<Vec<_>>(),
            });
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Rebuild alpha render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    view: target,
                    resolve_target: None,
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            render_pass.set_pipeline(pipeline);
            render_pass.set_bind_group(0, &bind_group, &[]);
            render_pass.set_bind_group(1, &self.sampler.bind_group, &[]);
            ctx.plane.draw(&mut render_pass);
        }
        ctx.queue.submit(Some(encoder.finish()));
    }
}
