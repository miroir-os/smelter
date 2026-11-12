use crate::{
    Frame, Resolution,
    state::node_texture::NodeTextureState,
    wgpu::{WgpuCtx, format::rebuild_alpha::AlphaGrid},
};

use super::InputTexture;

/// A guided matte: a picture and its matte, rebuilt into one premultiplied texture.
pub(super) struct WithAlphaInput {
    resolution: Resolution,
    picture: InputTexture,
    matte: InputTexture,
    grid: AlphaGrid,
}

impl WithAlphaInput {
    pub fn new(ctx: &WgpuCtx, picture: Frame, matte: Frame) -> Self {
        let mut input = Self {
            resolution: picture.resolution,
            picture: InputTexture::new(),
            matte: InputTexture::new(),
            grid: AlphaGrid::new(ctx, matte.resolution),
        };
        input.upload(ctx, picture, matte);
        input
    }

    pub fn upload(&mut self, ctx: &WgpuCtx, picture: Frame, matte: Frame) {
        if self.grid.resolution != matte.resolution {
            self.grid = AlphaGrid::new(ctx, matte.resolution);
        }
        self.resolution = picture.resolution;
        self.picture.upload(ctx, picture);
        self.matte.upload(ctx, matte);
    }

    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    pub fn convert(
        &mut self,
        ctx: &WgpuCtx,
        dest: &NodeTextureState,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let (Some(picture), Some(matte)) = (self.picture.yuv_views(), self.matte.yuv_views())
        else {
            unreachable!("MP4 decoders produce YUV pictures");
        };
        let (dst_view, srgb) = match dest {
            NodeTextureState::GpuOptimized { texture, .. } => (texture.srgb_view(), true),
            NodeTextureState::CpuOptimized { texture, .. } => (texture.view(), false),
            _ => unreachable!("WebGL renders no MP4 input"),
        };
        self.grid
            .rebuild(ctx, picture, matte, dst_view, srgb, encoder);
    }
}
