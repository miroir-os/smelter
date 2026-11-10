use crate::Resolution;
use bytes::Bytes;
use nalgebra_glm::Mat4;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::error;

mod renderer;

pub mod process_helper;

mod transformation_matrices;
mod utils;

pub use renderer::*;

mod browser_client;
mod chromium_context;
mod chromium_sender;
mod chromium_sender_thread;
mod embedder;
mod node;
mod shader;
mod shared_memory;

pub const EMBED_SOURCE_FRAMES_MESSAGE: &str = "EMBED_SOURCE_FRAMES";
pub const UNEMBED_SOURCE_FRAMES_MESSAGE: &str = "UNEMBED_SOURCE_FRAMES";
pub const GET_FRAME_POSITIONS_MESSAGE: &str = "GET_FRAME_POSITIONS";

/// The page's latest paint and its size, until a tick takes it.
pub(super) type FrameData = Arc<Mutex<Option<(Bytes, Resolution)>>>;
pub(super) type SharedView = Arc<Mutex<Views>>;
pub(super) type SourceTransforms = Arc<Mutex<Vec<Mat4>>>;

pub(crate) use node::WebRendererNode;

pub use chromium_context::{ChromiumContext, ChromiumContextInitError, ChromiumOptions};

#[derive(Debug, Clone, PartialEq)]
pub struct WebRendererSpec {
    pub url: Arc<str>,
    pub view: PageView,
    pub embedding_method: WebEmbeddingMethod,
}

/// The size a page lays out at, in CSS pixels, and the device pixels each takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageView {
    pub layout: Resolution,
    pub scale_factor: f32,
}

impl PageView {
    /// The size of its paints, rounded up as CEF rounds it.
    pub fn paint_size(&self) -> Resolution {
        let pixels = |css: usize| (css as f32 * self.scale_factor).ceil() as usize;
        Resolution {
            width: pixels(self.layout.width),
            height: pixels(self.layout.height),
        }
    }
}

/// The view a page is asked for, and the one CEF has. CEF holds a resize until
/// a paint at its size, yet takes a screen info change sent meanwhile, which
/// can keep the page in an earlier view for good: CEF is handed the next view
/// only once it painted the last, on its UI thread, where it paints.
#[derive(Debug)]
pub(super) struct Views {
    pub wanted: PageView,
    pub shown: PageView,
    held: Option<Held>,
}

#[derive(Debug)]
struct Held {
    since: Instant,
    reported: bool,
}

/// Far longer than a page takes to lay out again.
const HELD_TOO_LONG: Duration = Duration::from_secs(5);

impl Views {
    pub(super) fn new(view: PageView) -> Self {
        Self {
            wanted: view,
            shown: view,
            held: None,
        }
    }

    /// Whether CEF is to take the wanted view now: none is in flight.
    pub(super) fn hand_over(&mut self, url: &str) -> bool {
        match &mut self.held {
            Some(held) => {
                if !held.reported && held.since.elapsed() > HELD_TOO_LONG {
                    held.reported = true;
                    error!("The website \"{url}\" never painted at its new size");
                }
                false
            }
            None if self.wanted == self.shown => false,
            None => {
                self.shown = self.wanted;
                self.held = Some(Held {
                    since: Instant::now(),
                    reported: false,
                });
                true
            }
        }
    }

    pub(super) fn painted(&mut self, size: Resolution) {
        if size == self.shown.paint_size() {
            self.held = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WebEmbeddingMethod {
    /// Send frames to chromium directly and render it on canvas
    ChromiumEmbedding,

    /// Render sources on top of the rendered website
    NativeEmbeddingOverContent,

    /// Render sources below the website.
    /// The website's background has to be transparent
    NativeEmbeddingUnderContent,
}
