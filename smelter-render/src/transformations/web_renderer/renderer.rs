use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use tracing::{error, info};

use crate::{
    RendererId, RenderingMode, Resolution,
    state::{RegisterCtx, RenderCtx, node_texture::NodeTexture},
    transformations::web_renderer::{
        browser_client::BrowserClient, chromium_sender::ChromiumSender,
    },
    wgpu::{
        WgpuCtx,
        common_pipeline::CreateShaderError,
        texture::{BgraLinearTexture, BgraSrgbTexture},
    },
};

use super::{
    FrameData, SourceTransforms, WebEmbeddingMethod, WebRendererSpec,
    embedder::{EmbedError, EmbeddingHelper, RenderInfo},
    node::EmbeddingData,
    shader::WebRendererShader,
};

#[derive(Debug)]
pub struct WebRenderer {
    spec: WebRendererSpec,
    frame_data: FrameData,
    /// Whether the texture holds a paint yet.
    painted: AtomicBool,
    source_transforms: SourceTransforms,
    embedding_helper: EmbeddingHelper,

    website_texture: WebsiteTexture,
    render_website_shader: WebRendererShader,
}

#[derive(Debug)]
pub enum WebsiteTexture {
    Srgb(BgraSrgbTexture),
    Linear(BgraLinearTexture),
}

impl WebRenderer {
    pub fn new(
        ctx: &RegisterCtx,
        instance_id: &RendererId,
        spec: WebRendererSpec,
    ) -> Result<Self, CreateWebRendererError> {
        let Some(chromium_context) = ctx.chromium_context.clone() else {
            return Err(CreateWebRendererError::WebRendererDisabled);
        };

        info!("Starting web renderer for {}", &spec.url);

        let frame_data = Arc::new(Mutex::new(None));
        let source_transforms = Arc::new(Mutex::new(Vec::new()));

        let client = BrowserClient::new(
            frame_data.clone(),
            source_transforms.clone(),
            spec.resolution,
        );
        let chromium_sender =
            ChromiumSender::new(chromium_context, instance_id, &spec, client, ctx.framerate);
        let embedding_helper = EmbeddingHelper::new(ctx, chromium_sender, spec.embedding_method);
        let render_website_shader = WebRendererShader::new(&ctx.wgpu_ctx)?;
        let website_texture = WebsiteTexture::new(&ctx.wgpu_ctx, spec.resolution);

        Ok(Self {
            spec,
            frame_data,
            painted: AtomicBool::new(false),
            source_transforms,
            embedding_helper,
            website_texture,
            render_website_shader,
        })
    }

    pub fn render(
        &self,
        ctx: &mut RenderCtx,
        sources: &[&NodeTexture],
        embedding_data: &EmbeddingData,
        target: &mut NodeTexture,
    ) -> Result<(), RenderWebsiteError> {
        // A page embedding nothing asks its renderer for no frame positions.
        if !embedding_data.children_ids.is_empty() {
            self.embedding_helper
                .prepare_embedding(ctx, sources, embedding_data)
                .map_err(|err| RenderWebsiteError::EmbeddingFailed(self.spec.url.clone(), err))?;
        }

        if self.painted.load(Ordering::Acquire) {
            let target = target.ensure_size(ctx.wgpu_ctx, self.spec.resolution);
            let render_textures = self.prepare_textures(sources);

            self.render_website_shader.render(
                ctx.wgpu_ctx,
                &render_textures,
                target,
                &mut ctx.encoder,
            );
        }

        Ok(())
    }

    /// Once a tick of the pipeline, before any output renders, on air or not:
    /// uploads the page's new paint, if any, for every placement of this tick,
    /// and asks for the next tick's.
    pub fn tick(&self, wgpu_ctx: &WgpuCtx) {
        if let Some(paint) = self.frame_data.lock().unwrap().take() {
            self.website_texture.upload(wgpu_ctx, &paint);
            self.painted.store(true, Ordering::Release);
        }
        if let Err(err) = self.embedding_helper.chromium_sender().request_paint() {
            error!(
                "Failed to ask the website \"{}\" for a paint: {err}",
                self.spec.url
            );
        }
    }

    fn prepare_textures<'a>(
        &'a self,
        sources: &'a [&NodeTexture],
    ) -> Vec<(Option<&'a wgpu::TextureView>, RenderInfo)> {
        let mut source_info = sources
            .iter()
            .zip(self.source_transforms.lock().unwrap().iter())
            .map(|(node_texture, transform)| {
                (
                    node_texture.state().map(|t| t.view()),
                    RenderInfo::source_transform(transform),
                )
            })
            .collect();

        let website_info = (Some(self.website_texture.view()), RenderInfo::website());

        let mut result = Vec::new();
        match self.spec.embedding_method {
            WebEmbeddingMethod::NativeEmbeddingOverContent => {
                result.push(website_info);
                result.append(&mut source_info);
            }
            WebEmbeddingMethod::NativeEmbeddingUnderContent => {
                result.append(&mut source_info);
                result.push(website_info);
            }
            WebEmbeddingMethod::ChromiumEmbedding => {
                result.push(website_info);
            }
        };

        result
    }

    pub fn resolution(&self) -> Resolution {
        self.spec.resolution
    }
}

impl WebsiteTexture {
    fn new(ctx: &WgpuCtx, resolution: Resolution) -> Self {
        match ctx.mode {
            RenderingMode::GpuOptimized | RenderingMode::WebGl => {
                Self::Srgb(BgraSrgbTexture::new(ctx, resolution))
            }
            RenderingMode::CpuOptimized => Self::Linear(BgraLinearTexture::new(ctx, resolution)),
        }
    }
    fn view(&self) -> &wgpu::TextureView {
        match &self {
            WebsiteTexture::Srgb(texture) => texture.view(),
            WebsiteTexture::Linear(texture) => texture.view(),
        }
    }

    fn upload(&self, ctx: &WgpuCtx, data: &[u8]) {
        match &self {
            WebsiteTexture::Srgb(texture) => texture.upload(ctx, data),
            WebsiteTexture::Linear(texture) => texture.upload(ctx, data),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CreateWebRendererError {
    #[error(transparent)]
    CreateShaderFailed(#[from] CreateShaderError),

    #[error("Web rendering can not be used because it was disabled in the init request")]
    WebRendererDisabled,
}

#[derive(Debug, thiserror::Error)]
pub enum RenderWebsiteError {
    #[error("Failed to embed source on the website \"{0}\": {1}")]
    EmbeddingFailed(Arc<str>, #[source] EmbedError),
}
