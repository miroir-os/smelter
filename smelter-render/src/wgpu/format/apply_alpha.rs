use crate::wgpu::common_pipeline::{PRIMITIVE_STATE, Sampler, Vertex};

use super::WgpuCtx;

/// Makes an RGBA texture transparent where a luma texture says: multiplies its color by the
/// alpha and sets its alpha, leaving it premultiplied.
#[derive(Debug)]
pub struct AlphaApplier {
    pipeline: wgpu::RenderPipeline,
    sampler: Sampler,
}

impl AlphaApplier {
    pub fn new(
        device: &wgpu::Device,
        texture_layout: &wgpu::BindGroupLayout,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let shader_module = device.create_shader_module(wgpu::include_wgsl!("apply_alpha.wgsl"));
        let sampler = Sampler::new(device);

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Apply alpha render pipeline layout"),
            bind_group_layouts: &[Some(texture_layout), Some(&sampler.bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Apply alpha render pipeline"),
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
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    write_mask: wgpu::ColorWrites::all(),
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::Zero,
                            dst_factor: wgpu::BlendFactor::SrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent::REPLACE,
                    }),
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),

            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            depth_stencil: None,
            cache: None,
        });

        Self { pipeline, sampler }
    }

    /// `dst_view` must match the `target_format` this applier was built with. Blending through
    /// an sRGB view multiplies the color in linear space, where premultiplied blending happens.
    pub fn apply(&self, ctx: &WgpuCtx, luma_bg: &wgpu::BindGroup, dst_view: &wgpu::TextureView) {
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Apply alpha encoder"),
            });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Apply alpha render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    view: dst_view,
                    resolve_target: None,
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, luma_bg, &[]);
            render_pass.set_bind_group(1, &self.sampler.bind_group, &[]);

            ctx.plane.draw(&mut render_pass);
        }

        ctx.queue.submit(Some(encoder.finish()));
    }
}
