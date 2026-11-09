use std::os::raw::{c_int, c_void};

use crate::{
    browser::Browser,
    cef_ref::{CefRefCountable, CefRefData, CefStruct},
};

pub struct Resolution {
    pub width: usize,
    pub height: usize,
}

/// Handles browser render callbacks
pub trait RenderHandler {
    /// Specifies the render resolution
    fn resolution(&self, browser: &Browser) -> Resolution;

    /// Called every time new frame is rendered
    fn on_paint(&self, browser: &Browser, buffer: &[u8], resolution: Resolution);

    /// Called instead of [`RenderHandler::on_paint`] for each frame of a browser opened with
    /// [`WindowInfo::shared_texture_enabled`](crate::WindowInfo::shared_texture_enabled)
    fn on_accelerated_paint(&self, _browser: &Browser, _texture: SharedTexture<'_>) {}
}

/// A frame Chromium renders into a GPU buffer of its pool, which it reuses once
/// [`RenderHandler::on_accelerated_paint`] returns: the handler copies it first.
pub struct SharedTexture<'a>(&'a libcef_sys::cef_accelerated_paint_info_t);

/// Byte order of a frame's pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorType {
    Rgba8888,
    Bgra8888,
}

impl SharedTexture<'_> {
    pub fn format(&self) -> ColorType {
        match self.0.format {
            libcef_sys::cef_color_type_t_CEF_COLOR_TYPE_RGBA_8888 => ColorType::Rgba8888,
            libcef_sys::cef_color_type_t_CEF_COLOR_TYPE_BGRA_8888 => ColorType::Bgra8888,
            other => unreachable!("CEF has no color type {other}"),
        }
    }

    pub fn resolution(&self) -> Resolution {
        let size = self.0.extra.coded_size;
        Resolution {
            width: size.width as usize,
            height: size.height as usize,
        }
    }

    /// The buffer's planes, each a dma-buf.
    #[cfg(target_os = "linux")]
    pub fn planes(&self) -> impl Iterator<Item = Plane<'_>> {
        self.0.planes[..self.0.plane_count as usize]
            .iter()
            .map(|plane| Plane {
                fd: unsafe { std::os::fd::BorrowedFd::borrow_raw(plane.fd) },
                stride: plane.stride,
                offset: plane.offset,
                size: plane.size,
            })
    }

    /// The planes' DRM format modifier.
    #[cfg(target_os = "linux")]
    pub fn modifier(&self) -> u64 {
        self.0.modifier
    }

    /// The `IOSurfaceRef` holding the frame.
    #[cfg(target_os = "macos")]
    pub fn io_surface(&self) -> *mut c_void {
        self.0.shared_texture_io_surface
    }
}

#[cfg(target_os = "linux")]
pub struct Plane<'a> {
    pub fd: std::os::fd::BorrowedFd<'a>,
    pub stride: u32,
    pub offset: u64,
    pub size: u64,
}

pub(crate) struct RenderHandlerWrapper<R: RenderHandler>(pub R);

impl<R: RenderHandler> CefStruct for RenderHandlerWrapper<R> {
    type CefType = libcef_sys::cef_render_handler_t;

    fn new_cef_data() -> Self::CefType {
        libcef_sys::cef_render_handler_t {
            base: unsafe { std::mem::zeroed() },
            get_accessibility_handler: None,
            get_root_screen_rect: None,
            get_view_rect: Some(Self::view_rect),
            get_screen_point: None,
            get_screen_info: None,
            on_popup_show: None,
            on_popup_size: None,
            on_paint: Some(Self::on_paint),
            on_accelerated_paint: Some(Self::on_accelerated_paint),
            get_touch_handle_size: None,
            on_touch_handle_state_changed: None,
            start_dragging: None,
            update_drag_cursor: None,
            on_scroll_offset_changed: None,
            on_ime_composition_range_changed: None,
            on_text_selection_changed: None,
            on_virtual_keyboard_requested: None,
        }
    }

    fn base_from_cef_data(cef_data: &mut Self::CefType) -> &mut libcef_sys::cef_base_ref_counted_t {
        &mut cef_data.base
    }
}

impl CefRefCountable for libcef_sys::cef_render_handler_t {
    fn base_mut(&mut self) -> *mut libcef_sys::cef_base_ref_counted_t {
        &mut self.base
    }
}

impl<R: RenderHandler> RenderHandlerWrapper<R> {
    extern "C" fn view_rect(
        self_: *mut libcef_sys::cef_render_handler_t,
        browser: *mut libcef_sys::cef_browser_t,
        rect: *mut libcef_sys::cef_rect_t,
    ) {
        unsafe {
            let self_ref = CefRefData::<Self>::from_cef(self_);
            let browser = Browser::new(browser);
            let resolution = self_ref.0.resolution(&browser);
            let rect = &mut *rect;
            rect.width = resolution.width as i32;
            rect.height = resolution.height as i32;
        }
    }

    extern "C" fn on_paint(
        self_: *mut libcef_sys::cef_render_handler_t,
        browser: *mut libcef_sys::cef_browser_t,
        _type: libcef_sys::cef_paint_element_type_t,
        _dirty_rects_count: usize,
        _dirt_rects: *const libcef_sys::cef_rect_t,
        buffer: *const c_void,
        width: c_int,
        height: c_int,
    ) {
        unsafe {
            let self_ref = CefRefData::<Self>::from_cef(self_);
            let browser = Browser::new(browser);
            let buffer =
                std::slice::from_raw_parts(buffer as *const u8, (4 * width * height) as usize);
            self_ref.0.on_paint(
                &browser,
                buffer,
                Resolution {
                    width: width as usize,
                    height: height as usize,
                },
            );
        }
    }

    extern "C" fn on_accelerated_paint(
        self_: *mut libcef_sys::cef_render_handler_t,
        browser: *mut libcef_sys::cef_browser_t,
        _type: libcef_sys::cef_paint_element_type_t,
        _dirty_rects_count: usize,
        _dirty_rects: *const libcef_sys::cef_rect_t,
        info: *const libcef_sys::cef_accelerated_paint_info_t,
    ) {
        unsafe {
            let self_ref = CefRefData::<Self>::from_cef(self_);
            let browser = Browser::new(browser);
            self_ref
                .0
                .on_accelerated_paint(&browser, SharedTexture(&*info));
        }
    }
}
