use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{
    Framerate, RendererId, Resolution, scene::ComponentId, state::node_texture::NodeTexture,
    transformations::web_renderer::chromium_context::ChromiumContext,
};
use crossbeam_channel::{Receiver, Sender};
use tracing::error;

use super::{
    WebRendererSpec, browser_client::BrowserClient, chromium_sender_thread::ChromiumSenderThread,
};

#[derive(Debug)]
pub(super) struct ChromiumSender {
    message_sender: Sender<ChromiumSenderMessage>,
    /// Used for synchronizing buffer map and unmap operations
    unmap_signal_receiver: Receiver<()>,
    /// Whether a paint is asked for that the browser has yet to take.
    paint_asked: Arc<AtomicBool>,
}

impl Drop for ChromiumSender {
    fn drop(&mut self) {
        if let Err(err) = self.message_sender.send(ChromiumSenderMessage::Quit) {
            error!("Failed to close ChromiumSenderThread: {err}");
        }
    }
}

impl ChromiumSender {
    pub fn new(
        chromium_context: Arc<ChromiumContext>,
        instance_id: &RendererId,
        spec: &WebRendererSpec,
        browser_client: BrowserClient,
        framerate: Framerate,
    ) -> Self {
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (unmap_signal_sender, unmap_signal_receiver) = crossbeam_channel::bounded(0);
        let paint_asked = Arc::new(AtomicBool::new(false));

        ChromiumSenderThread {
            chromium_context,
            url: spec.url.clone(),
            web_renderer_id: instance_id.clone(),
            browser_client,
            framerate,
            paint_asked: paint_asked.clone(),
            message_receiver,
            unmap_signal_sender,
        }
        .spawn();

        Self {
            message_sender,
            unmap_signal_receiver,
            paint_asked,
        }
    }

    pub fn embed_sources(
        &self,
        sources: &[&NodeTexture],
        children_ids: Vec<ComponentId>,
    ) -> Result<(), ChromiumSenderError> {
        let resolutions = sources.iter().map(|texture| texture.resolution()).collect();
        self.message_sender
            .send(ChromiumSenderMessage::EmbedSources {
                resolutions,
                children_ids,
            })
            .map_err(|_| ChromiumSenderError::MessageChannelDisconnected)
    }

    pub fn ensure_shared_memory(
        &self,
        sources: &[&NodeTexture],
    ) -> Result<(), ChromiumSenderError> {
        let resolutions = sources.iter().map(|texture| texture.resolution()).collect();
        self.message_sender
            .send(ChromiumSenderMessage::EnsureSharedMemory { resolutions })
            .map_err(|_| ChromiumSenderError::MessageChannelDisconnected)
    }

    pub fn update_shared_memory(
        &self,
        source_idx: usize,
        buffer: wgpu::Buffer,
        size: wgpu::Extent3d,
    ) -> Result<(), ChromiumSenderError> {
        let info = UpdateSharedMemoryInfo {
            source_idx,
            buffer,
            size,
        };

        self.message_sender
            .send(ChromiumSenderMessage::UpdateSharedMemory(info))
            .map_err(|_| ChromiumSenderError::MessageChannelDisconnected)?;

        // Wait until buffer unmap is possible
        self.unmap_signal_receiver
            .recv()
            .map_err(|_| ChromiumSenderError::MessageChannelDisconnected)
    }

    /// Asks the page for its next paint, unless it has yet to take the last
    /// ask: a browser starting, or gone, never piles them up.
    pub fn request_paint(&self) -> Result<(), ChromiumSenderError> {
        if self.paint_asked.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.message_sender
            .send(ChromiumSenderMessage::Paint)
            .map_err(|_| ChromiumSenderError::MessageChannelDisconnected)
    }

    pub fn request_frame_positions(
        &self,
        children_ids: Vec<ComponentId>,
    ) -> Result<(), ChromiumSenderError> {
        self.message_sender
            .send(ChromiumSenderMessage::GetFramePositions { children_ids })
            .map_err(|_| ChromiumSenderError::MessageChannelDisconnected)
    }
}

pub(super) enum ChromiumSenderMessage {
    EmbedSources {
        resolutions: Vec<Option<Resolution>>,
        children_ids: Vec<ComponentId>,
    },
    EnsureSharedMemory {
        resolutions: Vec<Option<Resolution>>,
    },
    UpdateSharedMemory(UpdateSharedMemoryInfo),
    GetFramePositions {
        children_ids: Vec<ComponentId>,
    },
    Paint,
    Quit,
}

pub(super) struct UpdateSharedMemoryInfo {
    pub source_idx: usize,
    pub buffer: wgpu::Buffer,
    pub size: wgpu::Extent3d,
}

#[derive(Debug, thiserror::Error)]
pub enum ChromiumSenderError {
    #[error("Chromium message channel is disconnected")]
    MessageChannelDisconnected,
}
