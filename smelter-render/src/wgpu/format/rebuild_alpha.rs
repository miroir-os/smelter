use crate::{
    Resolution,
    wgpu::common_pipeline::{PRIMITIVE_STATE, Vertex},
};

use super::WgpuCtx;

/// Makes a guided matte's picture transparent, as `rebuild_alpha.wgsl` describes.
#[derive(Debug)]
pub struct AlphaRebuilder {
    /// The texels' pass, by chroma (interleaved, then planar).
    texels: [wgpu::RenderPipeline; 2],
    /// The pixels' pass, by target (linear, then sRGB).
    pixels: [wgpu::RenderPipeline; 2],
    texels_layout: wgpu::BindGroupLayout,
    pixels_layout: wgpu::BindGroupLayout,
}

/// What the texels' pass writes for the pixels', at the matte's size.
pub struct AlphaGrid {
    pub resolution: Resolution,
    /// Each texel's alphas, its pixels' lumas unkeyed, and its chroma cell.
    targets: [wgpu::TextureView; 3],
}

impl AlphaGrid {
    pub fn new(ctx: &WgpuCtx, resolution: Resolution) -> Self {
        let target = |label| {
            ctx.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: resolution.width as u32,
                        height: resolution.height as u32,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        Self {
            resolution,
            targets: ["alphas", "straight lumas", "cells"].map(target),
        }
    }

    /// Rebuilds into `dst_view`, an sRGB view if `srgb`.
    pub fn rebuild(
        &self,
        ctx: &WgpuCtx,
        picture: YuvViews,
        matte: YuvViews,
        dst_view: &wgpu::TextureView,
        srgb: bool,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let rebuilder = &ctx.format.rebuild_alpha;
        let texels = &rebuilder.texels[usize::from(!picture.interleaved)];
        let pixels = &rebuilder.pixels[usize::from(srgb)];
        let [alphas, lumas, cells] = &self.targets;
        for (pipeline, layout, textures, targets) in [
            (
                texels,
                &rebuilder.texels_layout,
                &[picture.luma, matte.luma, matte.u, picture.u, picture.v][..],
                &self.targets.each_ref()[..],
            ),
            (
                pixels,
                &rebuilder.pixels_layout,
                &[alphas, lumas, cells][..],
                &[dst_view][..],
            ),
        ] {
            let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Rebuild alpha bind group"),
                layout,
                entries: &textures
                    .iter()
                    .zip(0..)
                    .map(|(view, binding)| wgpu::BindGroupEntry {
                        binding,
                        resource: wgpu::BindingResource::TextureView(view),
                    })
                    .collect::<Vec<_>>(),
            });
            let attachments: Vec<_> = targets
                .iter()
                .map(|view| {
                    Some(wgpu::RenderPassColorAttachment {
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                        view,
                        resolve_target: None,
                        depth_slice: None,
                    })
                })
                .collect();
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Rebuild alpha render pass"),
                color_attachments: &attachments,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            render_pass.set_pipeline(pipeline);
            render_pass.set_bind_group(0, &bind_group, &[]);
            ctx.plane.draw(&mut render_pass);
        }
    }
}

/// A YUV picture's planes, U read from the red of `u`: NV12's chroma interleaved, binding it as
/// both, V in green; planar YUV's U and V planes, V in red.
pub struct YuvViews<'a> {
    pub luma: &'a wgpu::TextureView,
    pub u: &'a wgpu::TextureView,
    pub v: &'a wgpu::TextureView,
    pub interleaved: bool,
}

impl AlphaRebuilder {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader_module = device.create_shader_module(wgpu::include_wgsl!("rebuild_alpha.wgsl"));
        let textures = |count| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Rebuild alpha bind group layout"),
                entries: &(0..count)
                    .map(|binding| wgpu::BindGroupLayoutEntry {
                        binding,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        count: None,
                    })
                    .collect::<Vec<_>>(),
            })
        };
        let (texels_layout, pixels_layout) = (textures(5), textures(3));
        let pipeline = |entry_point,
                        layout: &wgpu::BindGroupLayout,
                        formats: &[wgpu::TextureFormat],
                        constants: &[(&str, f64)]| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Rebuild alpha render pipeline layout"),
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            let targets: Vec<_> = formats
                .iter()
                .map(|&format| {
                    Some(wgpu::ColorTargetState {
                        format,
                        write_mask: wgpu::ColorWrites::all(),
                        blend: None,
                    })
                })
                .collect();
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry_point),
                layout: Some(&layout),
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
                    targets: &targets,
                    compilation_options: wgpu::PipelineCompilationOptions {
                        constants,
                        ..Default::default()
                    },
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                depth_stencil: None,
                cache: None,
            })
        };
        let grid = [wgpu::TextureFormat::Rgba8Unorm; 3];
        let texels = [1.0, 0.0].map(|v_channel| {
            pipeline("texels", &texels_layout, &grid, &[("V_CHANNEL", v_channel)])
        });
        // Through an sRGB view the colour is premultiplied in linear space, where premultiplied
        // blending happens.
        let pixels = [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ]
        .map(|format| {
            let srgb = f64::from(u8::from(format.is_srgb()));
            pipeline(
                "pixels",
                &pixels_layout,
                &[format],
                &[("SRGB_TARGET", srgb)],
            )
        });
        Self {
            texels,
            pixels,
            texels_layout,
            pixels_layout,
        }
    }
}
