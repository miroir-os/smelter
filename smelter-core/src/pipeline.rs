use std::{path::Path, sync::Arc, time::Duration};

use ::rtmp::TlsConfig;
use smelter_render::{
    Framerate, RenderingMode, WgpuCtx, WgpuFeatures, web_renderer::ChromiumContext,
};
use tokio::runtime::Runtime;

use crate::{
    event::EventEmitter,
    graphics_context::GraphicsContext,
    pipeline::{
        moq::MoqPipelineState,
        rtmp::RtmpPipelineState,
        webrtc::{WebrtcSettingEngineCtx, WhipWhepPipelineState},
    },
    queue::QueueContext,
    stats::StatsSender,
};

use crate::prelude::*;

mod decoder;
mod encoder;

mod ffmpeg_utils;

#[cfg(feature = "decklink")]
mod decklink;

#[cfg(target_os = "linux")]
mod v4l2;

mod channel;
mod hls;
mod moq;
mod mp4;
mod rtmp;
mod rtp;
mod webrtc;

mod input;
mod instance;
mod output;

pub(crate) mod utils;

pub use instance::Pipeline;
pub(crate) use moq::SelfSignedTlsError;

#[cfg(target_os = "linux")]
pub use v4l2::{V4l2DeviceInfo, V4l2FormatInfo, V4l2ResolutionInfo, list_v4l2_devices};

// Constructible parts for callers that drive inputs/outputs without a
// running `Pipeline` (see `PipelineCtx::new_detached`).
pub use channel::{EncodedDataOutput, RawDataInput, RawDataOutput};
#[cfg(feature = "decklink")]
pub use decklink::DeckLink;
pub use input::{Input, PipelineInput};
pub use mp4::{Mp4Input, Mp4Output};
pub use output::{Output, OutputAudio, OutputVideo};
pub use rtmp::RtmpClientOutput;
#[cfg(target_os = "linux")]
pub use v4l2::V4l2Input;

#[derive(Debug)]
pub struct PipelineOptions {
    pub stream_fallback_timeout: Duration,
    pub default_buffer_duration: Duration,

    pub load_system_fonts: bool,
    pub run_late_scheduled_events: bool,
    pub never_drop_output_frames: bool,
    pub ahead_of_time_processing: bool,
    pub side_channel_socket_dir: Option<Arc<Path>>,

    pub output_framerate: Framerate,
    pub mixing_sample_rate: u32,

    pub download_root: Arc<Path>,

    pub rendering_mode: RenderingMode,
    pub max_layouts_count: usize,
    pub wgpu_options: PipelineWgpuOptions,
    pub tokio_rt: Option<Arc<Runtime>>,

    /// required for web rendering support
    pub chromium_context: Option<Arc<ChromiumContext>>,

    pub whip_whep_server: PipelineWhipWhepServerOptions,
    pub webrtc_stun_servers: Arc<Vec<String>>,
    pub webrtc_udp_port_strategy: Option<WebrtcUdpPortStrategy>,
    pub webrtc_nat_1to1_ips: Arc<Vec<String>>,

    pub rtmp_server: PipelineRtmpServerOptions,
    pub moq_server: PipelineMoqServerOptions,
}

#[derive(Debug)]
pub enum PipelineWgpuOptions {
    Context(GraphicsContext),
    Options {
        device_id: Option<u32>,
        driver_name: Option<String>,
        features: WgpuFeatures,
        force_gpu: bool,
    },
}

#[derive(Debug)]
pub enum PipelineWhipWhepServerOptions {
    Enable { port: u16 },
    Disable,
}

#[derive(Debug)]
pub enum PipelineRtmpServerOptions {
    Enable {
        port: u16,
        tls_config: Option<TlsConfig>,
    },
    Disable,
}

#[derive(Debug)]
pub enum PipelineMoqServerOptions {
    Enable {
        port: u16,
        tls_config: Option<moq_native::ServerTlsConfig>,
    },
    Disable,
}

pub const DEFAULT_BUFFER_DURATION: Duration = Duration::from_millis(16 * 5); // about 5 frames at 60 fps

#[derive(Clone)]
pub struct PipelineCtx {
    pub queue_ctx: QueueContext,
    pub default_buffer_duration: Duration,

    pub mixing_sample_rate: u32,
    pub output_framerate: Framerate,

    pub download_dir: Arc<Path>,
    pub graphics_context: GraphicsContext,
    pub wgpu_ctx: Arc<WgpuCtx>,
    pub event_emitter: Arc<EventEmitter>,
    pub stats_sender: StatsSender,
    pub webrtc_stun_servers: Arc<Vec<String>>,
    pub webrtc_setting_engine: WebrtcSettingEngineCtx,
    tokio_rt: Arc<Runtime>,
    whip_whep_state: Option<Arc<WhipWhepPipelineState>>,
    rtmp_state: Option<Arc<RtmpPipelineState>>,
    moq_state: Option<Arc<MoqPipelineState>>,
}

impl PipelineCtx {
    /// Minimal context for constructing inputs and outputs without a running
    /// [`Pipeline`]. No protocol servers are started; the caller owns the
    /// event emitter and drains the returned [`crate::stats::StatsMonitor`].
    pub fn new_detached(
        queue_ctx: QueueContext,
        output_framerate: Framerate,
        mixing_sample_rate: u32,
        download_root: Arc<Path>,
        graphics_context: GraphicsContext,
        wgpu_ctx: Arc<WgpuCtx>,
        event_emitter: Arc<EventEmitter>,
        tokio_rt: Option<Arc<Runtime>>,
    ) -> Result<(Arc<Self>, crate::stats::StatsMonitor), InitPipelineError> {
        let download_dir = download_root.join(format!("smelter-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&download_dir).map_err(InitPipelineError::CreateDownloadDir)?;
        let tokio_rt = match tokio_rt {
            Some(tokio_rt) => tokio_rt,
            None => Arc::new(Runtime::new().map_err(InitPipelineError::CreateTokioRuntime)?),
        };
        let (stats_monitor, stats_sender) = crate::stats::StatsMonitor::new();
        let webrtc_setting_engine = WebrtcSettingEngineCtx::new(Arc::new(vec![]), None, &tokio_rt)?;
        Ok((
            Arc::new(Self {
                queue_ctx,
                default_buffer_duration: DEFAULT_BUFFER_DURATION,
                mixing_sample_rate,
                output_framerate,
                download_dir: download_dir.into(),
                graphics_context,
                wgpu_ctx,
                event_emitter,
                stats_sender,
                webrtc_stun_servers: Arc::new(vec![]),
                webrtc_setting_engine,
                tokio_rt,
                whip_whep_state: None,
                rtmp_state: None,
                moq_state: None,
            }),
            stats_monitor,
        ))
    }
}

impl std::fmt::Debug for PipelineCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineCtx")
            .field("mixing_sample_rate", &self.mixing_sample_rate)
            .field("output_framerate", &self.output_framerate)
            .field("download_dir", &self.download_dir)
            .field("event_emitter", &self.event_emitter)
            .finish()
    }
}
