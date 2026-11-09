use std::io;
use std::os::raw::c_int;

use crate::{
    cef_ref::{CefRc, CefRefCountable, CefRefData},
    cef_string::CefString,
    main_args::MainArgs,
    *,
};

/// Handles CEF initialization and deinitialization.
/// Used for interacting with CEF functions
pub struct Context {
    // We allow creating context only with `Context::new` and `Context::new_helper`.
    // This dissallows creating `Context` manually
    _priv: (),
    /// A macOS helper's seatbelt, held until the helper exits.
    #[cfg(target_os = "macos")]
    _sandbox: Option<Sandbox>,
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            libcef_sys::cef_shutdown();

            #[cfg(target_os = "macos")]
            libcef_sys::cef_unload_library();
        }
    }
}

impl Context {
    /// Creates new context for the main process
    pub fn new<A: App>(app: A, settings: Settings) -> Result<Self, ContextError> {
        #[cfg(target_os = "macos")]
        {
            let framework_path = std::env::current_exe()?
                .parent()
                .unwrap()
                .join("smelter.app")
                .join("Contents")
                .join("Frameworks")
                .join("Chromium Embedded Framework.framework")
                .join("Chromium Embedded Framework");

            Self::load_framework(framework_path)?;
        }
        Self::configure_api_version();

        let mut main_args = MainArgs::from_program_args();
        let settings = settings.into_raw();
        let app = CefRefData::new_ptr(AppWrapper::new(app));

        let init_result = unsafe {
            libcef_sys::cef_initialize(main_args.raw_mut(), &settings, app, std::ptr::null_mut())
        };

        if init_result != 1 {
            return Err(ContextError::FrameworkInitFailed);
        }

        Ok(Context {
            _priv: (),
            #[cfg(target_os = "macos")]
            _sandbox: None,
        })
    }

    /// Creates new context for a subprocess
    pub fn new_helper() -> Result<Self, ContextError> {
        #[cfg(target_os = "macos")]
        let sandbox = {
            let framework = std::env::current_exe()?
                .parent()
                .unwrap()
                .join("..")
                .join("..")
                .join("..")
                .join("Chromium Embedded Framework.framework");
            // Before the framework loads, as CEF's macOS sandbox requires.
            let sandbox =
                Sandbox::enter(&framework.join("Libraries").join("libcef_sandbox.dylib"))?;
            Self::load_framework(framework.join("Chromium Embedded Framework"))?;
            sandbox
        };
        Self::configure_api_version();

        Ok(Context {
            _priv: (),
            #[cfg(target_os = "macos")]
            _sandbox: Some(sandbox),
        })
    }

    fn configure_api_version() {
        unsafe { libcef_sys::cef_api_hash(libcef_sys::CEF_API_VERSION as i32, 0) };
    }

    #[cfg(target_os = "macos")]
    fn load_framework(framework_path: std::path::PathBuf) -> Result<(), ContextError> {
        use std::ffi::CString;
        let framework_path = CString::new(framework_path.display().to_string()).unwrap();
        let is_loaded = unsafe { libcef_sys::cef_load_library(framework_path.as_ptr()) };
        if is_loaded != 1 {
            return Err(ContextError::FrameworkNotLoaded);
        }

        Ok(())
    }

    /// Launches subprocess
    pub fn execute_process<A: App>(&self, app: A) -> i32 {
        let mut main_args = MainArgs::from_program_args();
        let app = CefRefData::new_ptr(AppWrapper::new(app));
        unsafe { libcef_sys::cef_execute_process(main_args.raw_mut(), app, std::ptr::null_mut()) }
    }

    pub fn start_browser<C: Client>(
        &self,
        client: C,
        window_info: WindowInfo,
        settings: BrowserSettings,
        profile: Profile,
        url: &str,
    ) -> Result<Browser, ContextError> {
        // Released here unless the browser takes it, a reference of its own.
        let request_context = match profile {
            Profile::Global => None,
            Profile::Ephemeral { proxy } => {
                let settings = libcef_sys::cef_request_context_settings_t {
                    size: std::mem::size_of::<libcef_sys::cef_request_context_settings_t>(),
                    cache_path: CefString::empty_raw(),
                    persist_session_cookies: false as c_int,
                    accept_language_list: CefString::empty_raw(),
                    cookieable_schemes_list: CefString::empty_raw(),
                    cookieable_schemes_exclude_defaults: false as c_int,
                };
                let context = unsafe {
                    libcef_sys::cef_request_context_create_context(&settings, std::ptr::null_mut())
                };
                if context.is_null() {
                    return Err(ContextError::StartBrowserFailed);
                }
                let context = CefRc::new(context);
                if let Some(server) = proxy {
                    unsafe { set_proxy(context.get_weak(), &server) }?;
                }
                Some(context)
            }
        };
        let client = CefRefData::new_ptr(ClientWrapper::new(client));
        let window_info = window_info.into_raw();
        let settings = settings.into_raw();
        let url = CefString::new_raw(url);
        let browser = unsafe {
            libcef_sys::cef_browser_host_create_browser_sync(
                &window_info,
                client,
                &url,
                &settings,
                std::ptr::null_mut(),
                request_context
                    .as_ref()
                    .map_or(std::ptr::null_mut(), CefRc::get),
            )
        };
        if browser.is_null() {
            return Err(ContextError::StartBrowserFailed);
        }

        Ok(Browser::new(browser))
    }

    /// Runs Chromium's message loop. It's a blocking call
    pub fn run_message_loop(&self) {
        unsafe {
            libcef_sys::cef_run_message_loop();
        }
    }

    /// Does currently available message loop work. It has to be called periodically.
    /// Calling it too rarely leads to starvation and calling it too often leads to high CPU usage.
    /// In both cases it causes low performance.
    /// It's recommended to use [`Context::run_message_loop`] instead or use external message pump
    /// which is not implemented as of now.
    pub fn do_message_loop_work(&self) {
        // TODO: Implement external message pump
        unsafe {
            libcef_sys::cef_do_message_loop_work();
        }
    }

    pub fn currently_on_thread(&self, thread_id: ThreadId) -> bool {
        unsafe { libcef_sys::cef_currently_on(thread_id as u32) == 1 }
    }
}

/// What a browser shares with the other browsers: cookies, storage, caches,
/// service workers and permissions.
pub enum Profile {
    /// The context's own, in memory.
    Global,
    /// One of its own in memory, gone with the browser. Its connections go
    /// through `proxy`, such as `socks5://127.0.0.1:1080`, if any: loopback
    /// ones too.
    Ephemeral { proxy: Option<String> },
}

impl CefRefCountable for libcef_sys::cef_request_context_t {
    fn base_mut(&mut self) -> *mut libcef_sys::cef_base_ref_counted_t {
        &mut self.base.base
    }
}

/// Sets the proxy of `context`, which must be on the UI thread: an argument's
/// reference passes to the callee, so nothing here releases one.
unsafe fn set_proxy(
    context: *mut libcef_sys::cef_request_context_t,
    server: &str,
) -> Result<(), ContextError> {
    unsafe {
        let dictionary = libcef_sys::cef_dictionary_value_create();
        let set_string = (*dictionary).set_string.unwrap();
        for (key, value) in [
            ("mode", "fixed_servers"),
            ("server", server),
            // Chromium connects to loopback directly unless told otherwise.
            ("bypass_list", "<-loopback>"),
        ] {
            set_string(
                dictionary,
                CefString::new(key).raw(),
                CefString::new(value).raw(),
            );
        }
        let value = libcef_sys::cef_value_create();
        ((*value).set_dictionary.unwrap())(value, dictionary);
        let manager = context.cast::<libcef_sys::cef_preference_manager_t>();
        let mut error = CefString::empty_raw();
        let name = CefString::new("proxy");
        let set = ((*manager).set_preference.unwrap())(manager, name.raw(), value, &mut error);
        let outcome = match set {
            1 => Ok(()),
            _ => Err(ContextError::ProxyRefused(CefString::from_raw(&error))),
        };
        libcef_sys::cef_string_utf16_clear(&mut error);
        outcome
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("IO operation failed")]
    IOError(#[from] io::Error),

    #[error("Failed to load chromium framework lib")]
    FrameworkNotLoaded,

    #[error("Failed to init chromium framework")]
    FrameworkInitFailed,

    #[error("Failed to start browser session")]
    StartBrowserFailed,

    #[error("Chromium refused the proxy: {0}")]
    ProxyRefused(String),

    #[error("The macOS sandbox did not start: {0}")]
    SandboxNotEntered(String),
}

/// The seatbelt `libcef_sandbox` puts a macOS helper in, on the policy its browser process passes it.
#[cfg(target_os = "macos")]
struct Sandbox {
    handle: *mut std::ffi::c_void,
    destroy: unsafe extern "C" fn(*mut std::ffi::c_void),
}

#[cfg(target_os = "macos")]
impl Sandbox {
    fn enter(library: &std::path::Path) -> Result<Self, ContextError> {
        use std::ffi::{CStr, CString, c_char, c_int, c_void};
        type Initialize = unsafe extern "C" fn(c_int, *mut *mut c_char) -> *mut c_void;
        type Destroy = unsafe extern "C" fn(*mut c_void);
        let failure = || {
            let error = unsafe { libc::dlerror() };
            let reason = match error.is_null() {
                true => "no reason given".to_owned(),
                false => unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned(),
            };
            ContextError::SandboxNotEntered(reason)
        };
        let path = CString::new(library.display().to_string()).unwrap();
        let library = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if library.is_null() {
            return Err(failure());
        }
        let symbol = |name: &CStr| {
            let symbol = unsafe { libc::dlsym(library, name.as_ptr()) };
            if symbol.is_null() {
                Err(failure())
            } else {
                Ok(symbol)
            }
        };
        let initialize: Initialize =
            unsafe { std::mem::transmute(symbol(c"cef_sandbox_initialize")?) };
        let destroy: Destroy = unsafe { std::mem::transmute(symbol(c"cef_sandbox_destroy")?) };
        let mut main_args = MainArgs::from_program_args();
        let args = main_args.raw_mut();
        let handle = unsafe { initialize(args.argc, args.argv.cast()) };
        if handle.is_null() {
            return Err(ContextError::SandboxNotEntered(
                "cef_sandbox_initialize refused".to_owned(),
            ));
        }
        Ok(Sandbox { handle, destroy })
    }
}

// The handle is an opaque token, only ever handed back to `cef_sandbox_destroy`.
#[cfg(target_os = "macos")]
unsafe impl Send for Sandbox {}
#[cfg(target_os = "macos")]
unsafe impl Sync for Sandbox {}

#[cfg(target_os = "macos")]
impl Drop for Sandbox {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.handle) };
    }
}
