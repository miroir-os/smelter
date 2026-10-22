use crate::{
    AlphaPlane, Frame, Resolution,
    state::node_texture::NodeTextureState,
    wgpu::{WgpuCtx, texture::TextureExt},
};

use super::InputTexture;

pub(super) struct WithAlphaInput {
    color: InputTexture,
    luma: Luma,
}

impl WithAlphaInput {
    pub fn new(ctx: &WgpuCtx, color: Frame, alpha: AlphaPlane) -> Self {
        let mut input = Self {
            luma: Luma::new(ctx, alpha, color.resolution, None),
            color: InputTexture::new(),
        };
        input.color.upload(ctx, color);
        input
    }

    pub fn upload(&mut self, ctx: &WgpuCtx, color: Frame, alpha: AlphaPlane) {
        self.luma = Luma::new(ctx, alpha, color.resolution, Some(&self.luma));
        self.color.upload(ctx, color);
    }

    pub fn resolution(&self) -> Resolution {
        self.color
            .resolution()
            .expect("the color is uploaded with the alpha")
    }

    pub fn convert(&mut self, ctx: &WgpuCtx, dest: &NodeTextureState) {
        self.color.convert(ctx, dest);
        let bind_group = &self.luma.bind_group;
        match dest {
            NodeTextureState::GpuOptimized { texture, .. } => {
                ctx.format
                    .apply_alpha_srgb
                    .apply(ctx, bind_group, texture.srgb_view())
            }
            NodeTextureState::CpuOptimized { texture, .. } => {
                ctx.format
                    .apply_alpha_linear
                    .apply(ctx, bind_group, texture.view())
            }
            NodeTextureState::WebGl { texture, .. } => {
                ctx.format
                    .apply_alpha_srgb
                    .apply(ctx, bind_group, texture.view())
            }
        }
    }
}

struct Luma {
    /// Written by the CPU, for alpha planes in memory.
    uploaded: Option<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
}

impl Luma {
    fn new(
        ctx: &WgpuCtx,
        alpha: AlphaPlane,
        resolution: Resolution,
        previous: Option<&Luma>,
    ) -> Self {
        match alpha {
            AlphaPlane::Bytes(bytes) => {
                let texture = previous
                    .and_then(|luma| luma.uploaded.clone())
                    .filter(|texture| Resolution::from(texture.size()) == resolution)
                    .unwrap_or_else(|| {
                        ctx.device.create_texture(&wgpu::TextureDescriptor {
                            label: Some("alpha plane"),
                            size: wgpu::Extent3d {
                                width: resolution.width as u32,
                                height: resolution.height as u32,
                                depth_or_array_layers: 1,
                            },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: wgpu::TextureFormat::R8Unorm,
                            usage: wgpu::TextureUsages::COPY_DST
                                | wgpu::TextureUsages::TEXTURE_BINDING,
                            view_formats: &[],
                        })
                    });
                texture.upload_data(&ctx.queue, &bytes, 1);
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                Self {
                    bind_group: Self::bind_group(ctx, &view),
                    uploaded: Some(texture),
                }
            }
            AlphaPlane::R8Texture(texture) => Self {
                bind_group: Self::bind_group(
                    ctx,
                    &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                ),
                uploaded: None,
            },
            AlphaPlane::Nv12Texture(texture) => Self {
                bind_group: Self::bind_group(
                    ctx,
                    &texture.create_view(&wgpu::TextureViewDescriptor {
                        label: Some("alpha nv12 y plane view"),
                        dimension: Some(wgpu::TextureViewDimension::D2),
                        format: Some(wgpu::TextureFormat::R8Unorm),
                        aspect: wgpu::TextureAspect::Plane0,
                        ..Default::default()
                    }),
                ),
                uploaded: None,
            },
        }
    }

    fn bind_group(ctx: &WgpuCtx, view: &wgpu::TextureView) -> wgpu::BindGroup {
        ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("alpha plane bind group"),
            layout: &ctx.format.single_texture_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            }],
        })
    }
}
