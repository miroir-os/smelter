use std::sync::{Arc, Mutex};

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
    FrameData, PageView, SharedView, SourceTransforms, Views, WebEmbeddingMethod, WebRendererSpec,
    embedder::{EmbedError, EmbeddingHelper, RenderInfo},
    node::EmbeddingData,
    shader::WebRendererShader,
};

#[derive(Debug)]
pub struct WebRenderer {
    url: Arc<str>,
    embedding_method: WebEmbeddingMethod,
    view: SharedView,
    frame_data: FrameData,
    source_transforms: SourceTransforms,
    embedding_helper: EmbeddingHelper,

    /// Its latest paint, at its own size, which a resize reaches a paint or so late.
    website: Mutex<Option<Website>>,
    render_website_shader: WebRendererShader,
}

#[derive(Debug)]
struct Website {
    resolution: Resolution,
    texture: WebsiteTexture,
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
        let view = Arc::new(Mutex::new(Views::new(spec.view)));

        let client =
            BrowserClient::new(frame_data.clone(), source_transforms.clone(), view.clone());
        let chromium_sender = ChromiumSender::new(
            chromium_context,
            instance_id,
            &spec,
            client,
            view.clone(),
            ctx.framerate,
        );
        let embedding_helper = EmbeddingHelper::new(ctx, chromium_sender, spec.embedding_method);
        let render_website_shader = WebRendererShader::new(&ctx.wgpu_ctx)?;
        Ok(Self {
            url: spec.url,
            embedding_method: spec.embedding_method,
            view,
            frame_data,
            source_transforms,
            embedding_helper,
            website: Mutex::new(None),
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
                .map_err(|err| RenderWebsiteError::EmbeddingFailed(self.url.clone(), err))?;
        }

        if let Some(website) = &*self.website.lock().unwrap() {
            let target = target.ensure_size(ctx.wgpu_ctx, website.resolution);
            let render_textures = self.prepare_textures(sources, website.texture.view());

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
        if let Some((paint, resolution)) = self.frame_data.lock().unwrap().take() {
            let mut website = self.website.lock().unwrap();
            if website
                .as_ref()
                .is_none_or(|website| website.resolution != resolution)
            {
                *website = Some(Website::new(wgpu_ctx, resolution));
            }
            if let Some(website) = &*website {
                website.texture.upload(wgpu_ctx, &paint);
            }
        }
        if let Err(err) = self.embedding_helper.chromium_sender().request_paint() {
            error!(
                "Failed to ask the website \"{}\" for a paint: {err}",
                self.url
            );
        }
    }

    /// Lays the page out in `view` from a paint or so on, as it is: nothing reloads.
    pub fn resize(&self, view: PageView) {
        self.view.lock().unwrap().wanted = view;
    }

    fn prepare_textures<'a>(
        &'a self,
        sources: &'a [&NodeTexture],
        website: &'a wgpu::TextureView,
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

        let website_info = (Some(website), RenderInfo::website());

        let mut result = Vec::new();
        match self.embedding_method {
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

    /// Its latest paint's size, or the view's before its first.
    pub fn resolution(&self) -> Resolution {
        match &*self.website.lock().unwrap() {
            Some(website) => website.resolution,
            None => self.view.lock().unwrap().shown.paint_size(),
        }
    }
}

impl Website {
    fn new(ctx: &WgpuCtx, resolution: Resolution) -> Self {
        let texture = match ctx.mode {
            RenderingMode::GpuOptimized | RenderingMode::WebGl => {
                WebsiteTexture::Srgb(BgraSrgbTexture::new(ctx, resolution))
            }
            RenderingMode::CpuOptimized => {
                WebsiteTexture::Linear(BgraLinearTexture::new(ctx, resolution))
            }
        };
        Self {
            resolution,
            texture,
        }
    }
}

impl WebsiteTexture {
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
