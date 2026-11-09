use std::os::raw::c_int;

use crate::{
    Frame,
    browser::Browser,
    cef_ref::{CefRefCountable, CefStruct},
};

/// Cancels every popup: a windowless browser has nowhere to show one, and a
/// popup sharing its client would paint over its view.
pub(crate) struct NoPopups;

impl CefStruct for NoPopups {
    type CefType = libcef_sys::cef_life_span_handler_t;

    fn new_cef_data() -> Self::CefType {
        libcef_sys::cef_life_span_handler_t {
            base: unsafe { std::mem::zeroed() },
            on_before_popup: Some(Self::on_before_popup),
            on_before_popup_aborted: None,
            on_before_dev_tools_popup: None,
            on_after_created: None,
            do_close: None,
            on_before_close: None,
        }
    }

    fn base_from_cef_data(cef_data: &mut Self::CefType) -> &mut libcef_sys::cef_base_ref_counted_t {
        &mut cef_data.base
    }
}

impl CefRefCountable for libcef_sys::cef_life_span_handler_t {
    fn base_mut(&mut self) -> *mut libcef_sys::cef_base_ref_counted_t {
        &mut self.base
    }
}

impl NoPopups {
    #[allow(clippy::too_many_arguments)]
    extern "C" fn on_before_popup(
        _self: *mut libcef_sys::cef_life_span_handler_t,
        browser: *mut libcef_sys::cef_browser_t,
        frame: *mut libcef_sys::cef_frame_t,
        _popup_id: c_int,
        _target_url: *const libcef_sys::cef_string_t,
        _target_frame_name: *const libcef_sys::cef_string_t,
        _target_disposition: libcef_sys::cef_window_open_disposition_t,
        _user_gesture: c_int,
        _popup_features: *const libcef_sys::cef_popup_features_t,
        _window_info: *mut libcef_sys::cef_window_info_t,
        _client: *mut *mut libcef_sys::cef_client_t,
        _settings: *mut libcef_sys::cef_browser_settings_t,
        _extra_info: *mut *mut libcef_sys::cef_dictionary_value_t,
        _no_javascript_access: *mut c_int,
    ) -> c_int {
        // The arguments' references are ours to release.
        drop((Browser::new(browser), Frame::new(frame)));
        true as c_int
    }
}
