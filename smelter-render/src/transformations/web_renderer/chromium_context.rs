use core::fmt;
use rand::Rng;
use rand::distr::Alphanumeric;
use std::sync::Arc;
use tracing::info;

use crate::{transformations::web_renderer::utils, types::Framerate};
use crossbeam_channel::RecvError;

pub struct ChromiumContext {
    instance_id: String,
    context: libcef::Context,
    page_proxy: Option<String>,
}

#[derive(Debug, Default)]
pub struct ChromiumOptions {
    pub enable_gpu: bool,
    /// The base64 SHA-256 hashes of the public keys pages are served locally
    /// with: trusted self-signed, while every other certificate is verified.
    pub trusted_certificate_keys: Vec<String>,
    /// The proxy every page of a pipeline connects through, such as
    /// `socks5://127.0.0.1:1080`, loopback connections too: one refusing local
    /// addresses keeps pages off them. Standalone browsers connect directly.
    pub page_proxy: Option<String>,
}

impl ChromiumContext {
    pub fn new(options: ChromiumOptions) -> Result<Arc<Self>, ChromiumContextInitError> {
        let instance_id = generate_random_id(30);

        info!("Init chromium context");
        let app = ChromiumApp {
            show_fps: false,
            enable_gpu: options.enable_gpu,
            trusted_certificate_keys: options.trusted_certificate_keys.join(","),
        };
        let settings = libcef::Settings {
            root_cache_path: utils::get_smelter_instance_tmp_path(&instance_id).join("cef_cache"),
            windowless_rendering_enabled: true,
            log_severity: libcef::LogSeverity::Info,
            ..Default::default()
        };

        let context = libcef::Context::new(app, settings)
            .map_err(ChromiumContextInitError::ContextFailure)?;
        Ok(Arc::new(Self {
            instance_id,
            context,
            page_proxy: options.page_proxy,
        }))
    }

    /// A page of a pipeline paints when asked, as its outputs render at
    /// `framerate`, in a profile of its own, through the page proxy: nothing
    /// it keeps reaches another page, nor outlives it.
    pub(super) fn start_browser(
        &self,
        url: &str,
        state: super::browser_client::BrowserClient,
        framerate: Framerate,
    ) -> Result<libcef::Browser, ChromiumContextInitError> {
        let window_info = libcef::WindowInfo {
            windowless_rendering_enabled: true,
            external_begin_frame_enabled: true,
        };
        let settings = libcef::BrowserSettings {
            windowless_frame_rate: framerate.num.div_ceil(framerate.den) as i32,
            background_color: 0,
        };
        let profile = libcef::Profile::Ephemeral {
            proxy: self.page_proxy.clone(),
        };
        self.open_browser(url, state, window_info, settings, profile)
    }

    /// Opens a browser on this context outside any pipeline, in the context's
    /// own profile: `client`'s render handler receives its paints.
    pub fn start_standalone_browser<C: libcef::Client + Send>(
        &self,
        url: &str,
        client: C,
        window_info: libcef::WindowInfo,
        settings: libcef::BrowserSettings,
    ) -> Result<libcef::Browser, ChromiumContextInitError> {
        self.open_browser(url, client, window_info, settings, libcef::Profile::Global)
    }

    fn open_browser<C: libcef::Client + Send>(
        &self,
        url: &str,
        client: C,
        window_info: libcef::WindowInfo,
        settings: libcef::BrowserSettings,
        profile: libcef::Profile,
    ) -> Result<libcef::Browser, ChromiumContextInitError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let task = libcef::Task::new(move || {
            let result = self
                .context
                .start_browser(client, window_info, settings, profile, url);
            tx.send(result).unwrap();
        });

        task.run(libcef::ThreadId::UI);
        rx.recv()?.map_err(ChromiumContextInitError::ContextFailure)
    }

    pub fn run_event_loop(&self) -> Result<(), ChromiumEventLoopError> {
        if !self.context.currently_on_thread(libcef::ThreadId::UI) {
            return Err(ChromiumEventLoopError::WrongThread);
        }

        self.context.run_message_loop();
        Ok(())
    }

    pub fn run_event_loop_single_iter(&self) -> Result<(), ChromiumEventLoopError> {
        if !self.context.currently_on_thread(libcef::ThreadId::UI) {
            return Err(ChromiumEventLoopError::WrongThread);
        }

        self.context.do_message_loop_work();
        Ok(())
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChromiumContextInitError {
    #[error("Chromium context failed: {0}")]
    ContextFailure(#[from] libcef::ContextError),

    #[error("Thread communication failed.")]
    ThreadNoResponse(#[from] RecvError),
}

#[derive(Debug, thiserror::Error)]
pub enum ChromiumEventLoopError {
    #[error("Event loop must run on the main thread")]
    WrongThread,
}

struct ChromiumApp {
    show_fps: bool,
    enable_gpu: bool,
    /// Comma-separated.
    trusted_certificate_keys: String,
}

impl libcef::App for ChromiumApp {
    type RenderProcessHandlerType = ();

    fn on_before_command_line_processing(
        &mut self,
        process_type: String,
        command_line: &mut libcef::CommandLine,
    ) {
        // Execute only on the main process
        if !process_type.is_empty() {
            return;
        }

        // OSR will not work without this on MacOS
        #[cfg(target_os = "macos")]
        command_line.append_switch("use-mock-keychain");

        if self.show_fps {
            command_line.append_switch("show-fps-counter")
        }
        if !self.enable_gpu {
            command_line.append_switch("disable-gpu");
            // TODO: This is probably only needed in docker container
            command_line.append_switch("disable-software-rasterizer");
        }
        // Headless Chromium on Linux finds no GL display of its own and silently
        // composites in software: ANGLE over the driver's EGL reaches the GPU.
        #[cfg(target_os = "linux")]
        if self.enable_gpu {
            command_line.append_switch_with_value("use-gl", "angle");
            command_line.append_switch_with_value("use-angle", "gl-egl");
        }

        command_line.append_switch("disable-dev-shm-usage");
        // Local pages are served over self-signed TLS, for HTTP/2.
        if !self.trusted_certificate_keys.is_empty() {
            command_line.append_switch_with_value(
                "ignore-certificate-errors-spki-list",
                &self.trusted_certificate_keys,
            );
        }
        command_line.append_switch("disable-gpu-shader-disk-cache");
        command_line.append_switch_with_value("autoplay-policy", "no-user-gesture-required");
        // Past a few GPU process crashes, which a page can cause, Chromium ends
        // its embedder's process.
        command_line.append_switch("disable-gpu-process-crash-limit");
        // No page is heard: its sound would play on the machine's own output.
        command_line.append_switch("mute-audio");
        // WebRTC's UDP goes around the network service, whose checks keep pages
        // off local addresses: with no proxy, it opens none.
        command_line
            .append_switch_with_value("force-webrtc-ip-handling-policy", "disable_non_proxied_udp");
    }
}

impl fmt::Debug for ChromiumContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromiumContext")
            .field("instance_id", &self.instance_id)
            .field("page_proxy", &self.page_proxy)
            .finish()
    }
}

pub(crate) fn generate_random_id(length: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect::<String>()
}
