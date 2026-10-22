use crate::{
    Frame, Resolution,
    state::node_texture::{NodeTexture, NodeTextureState},
    wgpu::{WgpuCtx, format::rebuild_alpha::AlphaGrid},
};

use super::InputTexture;

pub(super) struct WithAlphaInput {
    resolution: Resolution,
    color: InputTexture,
    matte: InputTexture,
    /// The color converted, which the alpha's rebuild reads.
    picture: NodeTexture,
    grid: AlphaGrid,
}

impl WithAlphaInput {
    pub fn new(ctx: &WgpuCtx, color: Frame, alpha: Frame) -> Self {
        let mut input = Self {
            resolution: color.resolution,
            color: InputTexture::new(),
            matte: InputTexture::new(),
            picture: NodeTexture::new(),
            grid: AlphaGrid::new(ctx, alpha.resolution),
        };
        input.upload(ctx, color, alpha);
        input
    }

    pub fn upload(&mut self, ctx: &WgpuCtx, color: Frame, alpha: Frame) {
        if self.grid.resolution != alpha.resolution {
            self.grid = AlphaGrid::new(ctx, alpha.resolution);
        }
        self.resolution = color.resolution;
        self.color.upload(ctx, color);
        self.matte.upload(ctx, alpha);
    }

    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    pub fn convert(&mut self, ctx: &WgpuCtx, dest: &NodeTextureState) {
        self.color.convert_to_node_texture(ctx, &mut self.picture);
        let picture = self
            .picture
            .state()
            .expect("the color is uploaded with the alpha");
        let (Some((luma, _)), Some((matte, keys))) =
            (self.color.yuv_views(), self.matte.yuv_views())
        else {
            unreachable!("MP4 decoders produce YUV pictures");
        };
        let (picture, dst_view, rebuilder) = match (picture, dest) {
            (
                NodeTextureState::GpuOptimized {
                    texture: picture, ..
                },
                NodeTextureState::GpuOptimized { texture, .. },
            ) => (
                picture.linear_view(),
                texture.srgb_view(),
                &ctx.format.rebuild_alpha_srgb,
            ),
            (
                NodeTextureState::CpuOptimized {
                    texture: picture, ..
                },
                NodeTextureState::CpuOptimized { texture, .. },
            ) => (
                picture.view(),
                texture.view(),
                &ctx.format.rebuild_alpha_linear,
            ),
            _ => unreachable!("WebGL renders no MP4 input"),
        };
        rebuilder.rebuild(ctx, &self.grid, [luma, matte, keys], picture, dst_view);
    }
}
