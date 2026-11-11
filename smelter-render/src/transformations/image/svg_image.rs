use core::fmt;
use std::{str, sync::Arc};

use bytes::BytesMut;
use resvg::{
    tiny_skia,
    usvg::{self, TreeParsing},
};
use tracing::error;

use crate::{
    RenderingMode, Resolution,
    scene::Size,
    state::node_texture::NodeTextureState,
    wgpu::{
        WgpuCtx,
        texture::{RgbaLinearTexture, TextureExt},
    },
};

use super::SvgError;

pub struct SvgNodeState {
    was_rendered: bool,
    renderer: SvgRenderer,
    resolution: Resolution,
}

pub struct SvgAsset {
    tree: UnsafeInternalRc<resvg::Tree>,
}

impl fmt::Debug for SvgAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SvgAsset")
            .field("size", &self.tree.0.size)
            .field("view_box", &self.tree.0.view_box)
            .finish()
    }
}

/// Safety: Should be fine as long as internal type T is never cloned
#[derive(Debug, Clone)]
pub struct UnsafeInternalRc<T>(Arc<T>);
unsafe impl<T> Send for UnsafeInternalRc<T> {}
unsafe impl<T> Sync for UnsafeInternalRc<T> {}

impl SvgAsset {
    pub fn new(_ctx: &WgpuCtx, data: bytes::Bytes) -> Result<Self, SvgError> {
        let text_svg = str::from_utf8(&data)?;
        let tree = usvg::Tree::from_str(text_svg, &Default::default())?;
        let tree = resvg::Tree::from_usvg(&tree);

        Ok(Self {
            tree: UnsafeInternalRc(tree.into()),
        })
    }

    pub fn render(&self, ctx: &WgpuCtx, target: &NodeTextureState, state: &mut SvgNodeState) {
        if state.was_rendered {
            return;
        }

        let resolution = state.resolution();

        match (&state.renderer, target) {
            (
                SvgRenderer::Linearized(renderer),
                NodeTextureState::GpuOptimized { .. } | NodeTextureState::WebGl { .. },
            ) => {
                renderer.render(ctx, &self.tree.0, target.view(), resolution);
            }
            (SvgRenderer::CpuOptimized, NodeTextureState::CpuOptimized { texture, .. }) => {
                // input is already in sRGB with pre-multiplied alpha
                render_to_texture(ctx, &self.tree.0, texture.texture(), resolution);
            }
            _ => {
                error!("Wrong node texture type");
                return;
            }
        };

        state.was_rendered = true;
    }

    pub fn size(&self) -> Size {
        Size {
            width: self.tree.0.size.width(),
            height: self.tree.0.size.height(),
        }
    }
}

impl SvgNodeState {
    pub fn new(ctx: &WgpuCtx, resolution: Resolution) -> Self {
        Self {
            was_rendered: false,
            renderer: match ctx.mode {
                RenderingMode::GpuOptimized | RenderingMode::WebGl => {
                    SvgRenderer::Linearized(LinearizedSvgRenderer::new(ctx, resolution))
                }
                RenderingMode::CpuOptimized => SvgRenderer::CpuOptimized,
            },
            resolution,
        }
    }
    pub fn resolution(&self) -> Resolution {
        self.resolution
    }
}

enum SvgRenderer {
    Linearized(LinearizedSvgRenderer),
    CpuOptimized,
}

/// Render order:
/// - upload sRGB pre-multiplied alpha as linear
/// - remove pre-multiplied alpha from the sRGB values and add it back to their
///   linear ones, into an sRGB view
struct LinearizedSvgRenderer {
    original_texture: RgbaLinearTexture,
    original_texture_bg: wgpu::BindGroup,
}

impl LinearizedSvgRenderer {
    fn new(ctx: &WgpuCtx, resolution: Resolution) -> Self {
        let original_texture = RgbaLinearTexture::new(ctx, resolution);
        Self {
            original_texture_bg: original_texture.new_bind_group(ctx),
            original_texture,
        }
    }

    fn render(
        &self,
        ctx: &WgpuCtx,
        tree: &resvg::Tree,
        target: &wgpu::TextureView,
        resolution: Resolution,
    ) {
        render_to_texture(ctx, tree, self.original_texture.texture(), resolution);
        ctx.utils
            .srgb_rgba_linearize_premult_alpha
            .render(ctx, &self.original_texture_bg, target);
    }
}

fn render_to_texture(
    ctx: &WgpuCtx,
    tree: &resvg::Tree,
    texture: &wgpu::Texture,
    resolution: Resolution,
) {
    let mut buffer = BytesMut::zeroed(resolution.width * resolution.height * 4);
    // pre-multiplied sRGB, but in the wrong order
    // we need to remove pre-multiplication -> convert to linear -> add pre-multiplication
    let mut pixmap = tiny_skia::PixmapMut::from_bytes(
        &mut buffer,
        resolution.width as u32,
        resolution.height as u32,
    )
    .unwrap();

    let should_resize = resolution.width != (tree.size.width() as usize)
        || resolution.height != (tree.size.height() as usize);
    let transform = if should_resize {
        let scale_x = resolution.width as f32 / tree.size.width();
        let scale_y = resolution.height as f32 / tree.size.height();
        tiny_skia::Transform::from_scale(scale_x, scale_y)
    } else {
        tiny_skia::Transform::default()
    };

    tree.render(transform, &mut pixmap);

    texture.upload_data(&ctx.queue, pixmap.data_mut(), 4);
    ctx.queue.submit([]);
}
