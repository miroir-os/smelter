use tracing::warn;

use crate::{Resolution, state::node_texture::NodeTextureState, wgpu::WgpuCtx};

#[derive(Debug, Clone, Copy)]
pub(super) enum Alpha {
    Straight,
    /// Premultiplied into sRGB values, which blend as they are only in CPU
    /// optimized rendering.
    PremultipliedSrgb,
}

pub(super) struct RgbaTextureInput {
    texture: wgpu::Texture,
    alpha: Alpha,
}

impl RgbaTextureInput {
    pub fn new(texture: wgpu::Texture, alpha: Alpha) -> Self {
        Self { texture, alpha }
    }

    pub fn resolution(&self) -> Resolution {
        self.texture.size().into()
    }

    fn bind_group(&self, ctx: &WgpuCtx) -> wgpu::BindGroup {
        let view = self.texture.create_view(&wgpu::TextureViewDescriptor {
            ..Default::default()
        });
        ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("RgbaTextureInput"),
            layout: &ctx.format.single_texture_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        })
    }

    pub fn convert(&self, ctx: &WgpuCtx, dest: &NodeTextureState) {
        match (self.alpha, dest, self.texture.format()) {
            (
                Alpha::Straight,
                NodeTextureState::GpuOptimized { texture, .. },
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ) => ctx.utils.srgb_rgba_add_premult_alpha.render(
                ctx,
                &self.bind_group(ctx),
                texture.srgb_view(),
            ),
            (
                Alpha::Straight,
                NodeTextureState::CpuOptimized { texture, .. },
                wgpu::TextureFormat::Rgba8Unorm,
            ) => ctx.utils.linear_rgba_add_premult_alpha.render(
                ctx,
                &self.bind_group(ctx),
                texture.view(),
            ),
            (
                Alpha::Straight,
                NodeTextureState::WebGl { texture, .. },
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ) => ctx.utils.srgb_rgba_add_premult_alpha.render(
                ctx,
                &self.bind_group(ctx),
                texture.view(),
            ),
            (
                Alpha::PremultipliedSrgb,
                NodeTextureState::GpuOptimized { .. } | NodeTextureState::WebGl { .. },
                wgpu::TextureFormat::Rgba8Unorm,
            ) => ctx.utils.srgb_rgba_linearize_premult_alpha.render(
                ctx,
                &self.bind_group(ctx),
                dest.view(),
            ),
            (
                Alpha::PremultipliedSrgb,
                NodeTextureState::CpuOptimized { texture, .. },
                wgpu::TextureFormat::Rgba8Unorm,
            ) => {
                let mut encoder =
                    ctx.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("RgbaTextureInput copy"),
                        });
                encoder.copy_texture_to_texture(
                    self.texture.as_image_copy(),
                    texture.texture().as_image_copy(),
                    self.texture.size(),
                );
                ctx.queue.submit(Some(encoder.finish()));
            }
            (alpha, _, format) => {
                warn!("Unsupported format: {format:?} with {alpha:?} alpha")
            }
        }
    }
}
