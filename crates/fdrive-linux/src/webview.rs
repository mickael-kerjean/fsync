use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::rc::Rc;

use gtk::glib::gobject_ffi;
use gtk::glib::translate::FromGlibPtrNone;
use gtk::prelude::*;

pub fn login(base: &str, insecure: bool) -> Result<Option<String>, String> {
    let wk = WebKit::load()?;
    let dialog = gtk::Dialog::new();
    dialog.set_title("Filestash");
    dialog.set_default_size(460, 640);

    let view = unsafe { (wk.web_view_new)() };
    let widget = unsafe { gtk::Widget::from_glib_none(view as *mut gtk::ffi::GtkWidget) };
    dialog.content_area().pack_start(&widget, true, true, 0);
    if insecure {
        const TLS_ERRORS_POLICY_IGNORE: c_int = 0;
        unsafe {
            let context = (wk.web_view_get_context)(view);
            (wk.web_context_set_tls_errors_policy)(context, TLS_ERRORS_POLICY_IGNORE);
        }
    }

    let state = Rc::new(State {
        wk,
        base: base.to_owned(),
        dialog: dialog.clone(),
        view,
        token: RefCell::new(None),
        busy: Cell::new(false),
    });
    unsafe {
        gobject_ffi::g_signal_connect_data(
            view as *mut gobject_ffi::GObject,
            c"notify::uri".as_ptr(),
            Some(std::mem::transmute::<
                unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
                unsafe extern "C" fn(),
            >(on_uri)),
            Rc::into_raw(state.clone()) as *mut c_void,
            Some(drop_state),
            0,
        );
        let uri = CString::new(format!("{base}/login")).map_err(|e| e.to_string())?;
        (wk.web_view_load_uri)(view, uri.as_ptr());
    }
    dialog.show_all();
    dialog.run();
    let token = state.token.borrow().clone();
    unsafe {
        dialog.destroy();
    }
    Ok(token)
}

struct State {
    wk: WebKit,
    base: String,
    dialog: gtk::Dialog,
    view: *mut c_void,
    token: RefCell<Option<String>>,
    busy: Cell<bool>,
}

unsafe extern "C" fn drop_state(data: *mut c_void, _closure: *mut gobject_ffi::GClosure) {
    drop(Rc::from_raw(data as *const State));
}

unsafe extern "C" fn on_uri(_view: *mut c_void, _pspec: *mut c_void, data: *mut c_void) {
    let state = &*(data as *const State);
    let uri = (state.wk.web_view_get_uri)(state.view);
    if uri.is_null() {
        return;
    }
    let Ok(uri) = CStr::from_ptr(uri).to_str() else {
        return;
    };
    let on_files = url::Url::parse(uri).is_ok_and(|u| u.path().starts_with("/files"));
    if !on_files || state.busy.replace(true) {
        return;
    }
    let manager =
        (state.wk.web_context_get_cookie_manager)((state.wk.web_view_get_context)(state.view));
    let api = CString::new(format!("{}/api/", state.base)).unwrap();
    Rc::increment_strong_count(data as *const State);
    (state.wk.cookie_manager_get_cookies)(
        manager,
        api.as_ptr(),
        std::ptr::null_mut(),
        on_cookies,
        data,
    );
}

unsafe extern "C" fn on_cookies(manager: *mut c_void, result: *mut c_void, data: *mut c_void) {
    let state = &*(data as *const State);
    let list = (state.wk.cookie_manager_get_cookies_finish)(manager, result, std::ptr::null_mut());
    let mut cookies = Vec::new();
    let mut node = list;
    while !node.is_null() {
        let cookie = (*node).data;
        let text = |ptr: *const c_char| match ptr.is_null() {
            true => String::new(),
            false => CStr::from_ptr(ptr).to_string_lossy().into_owned(),
        };
        cookies.push((
            text((state.wk.soup_cookie_get_name)(cookie)),
            text((state.wk.soup_cookie_get_value)(cookie)),
        ));
        node = (*node).next;
    }
    if !list.is_null() {
        gtk::glib::ffi::g_list_free_full(list, Some(state.wk.soup_cookie_free));
    }
    let token = assemble_token(&cookies);
    state.busy.set(false);
    if !token.is_empty() {
        *state.token.borrow_mut() = Some(token);
        state.dialog.response(gtk::ResponseType::Accept);
    }
    Rc::decrement_strong_count(data as *const State);
}

pub use fdrive_core::sdk::assemble_token;

#[derive(Clone, Copy)]
struct WebKit {
    web_view_new: unsafe extern "C" fn() -> *mut c_void,
    web_view_load_uri: unsafe extern "C" fn(*mut c_void, *const c_char),
    web_view_get_uri: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    web_view_get_context: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    web_context_get_cookie_manager: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    web_context_set_tls_errors_policy: unsafe extern "C" fn(*mut c_void, c_int),
    cookie_manager_get_cookies: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_void,
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
        *mut c_void,
    ),
    cookie_manager_get_cookies_finish: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *mut *mut c_void,
    ) -> *mut gtk::glib::ffi::GList,
    soup_cookie_get_name: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    soup_cookie_get_value: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    soup_cookie_free: unsafe extern "C" fn(*mut c_void),
}

impl WebKit {
    fn load() -> Result<Self, String> {
        const FLAVORS: [(&CStr, &CStr); 2] = [
            (c"libwebkit2gtk-4.1.so.0", c"libsoup-3.0.so.0"),
            (c"libwebkit2gtk-4.0.so.37", c"libsoup-2.4.so.1"),
        ];
        for (webkit_so, soup_so) in FLAVORS {
            unsafe {
                let webkit = libc::dlopen(webkit_so.as_ptr(), libc::RTLD_NOW);
                if webkit.is_null() {
                    continue;
                }
                let soup = libc::dlopen(soup_so.as_ptr(), libc::RTLD_NOW);
                if soup.is_null() {
                    continue;
                }
                return Ok(Self {
                    web_view_new: sym(webkit, c"webkit_web_view_new")?,
                    web_view_load_uri: sym(webkit, c"webkit_web_view_load_uri")?,
                    web_view_get_uri: sym(webkit, c"webkit_web_view_get_uri")?,
                    web_view_get_context: sym(webkit, c"webkit_web_view_get_context")?,
                    web_context_get_cookie_manager: sym(
                        webkit,
                        c"webkit_web_context_get_cookie_manager",
                    )?,
                    web_context_set_tls_errors_policy: sym(
                        webkit,
                        c"webkit_web_context_set_tls_errors_policy",
                    )?,
                    cookie_manager_get_cookies: sym(webkit, c"webkit_cookie_manager_get_cookies")?,
                    cookie_manager_get_cookies_finish: sym(
                        webkit,
                        c"webkit_cookie_manager_get_cookies_finish",
                    )?,
                    soup_cookie_get_name: sym(soup, c"soup_cookie_get_name")?,
                    soup_cookie_get_value: sym(soup, c"soup_cookie_get_value")?,
                    soup_cookie_free: sym(soup, c"soup_cookie_free")?,
                });
            }
        }
        Err("webkit2gtk is not installed".into())
    }
}

unsafe fn sym<T>(library: *mut c_void, name: &CStr) -> Result<T, String> {
    let ptr = libc::dlsym(library, name.as_ptr());
    if ptr.is_null() {
        return Err(format!("webkit2gtk: missing {}", name.to_string_lossy()));
    }
    Ok(std::mem::transmute_copy::<*mut c_void, T>(&ptr))
}

#[cfg(test)]
#[path = "webview_test.rs"]
mod tests;
