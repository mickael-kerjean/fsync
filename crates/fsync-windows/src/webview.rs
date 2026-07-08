use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::Path;
use std::rc::Rc;

use fsync_core::sdk::assemble_token;
use windows::core::{w, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    E_NOINTERFACE, E_OUTOFMEMORY, HWND, LPARAM, LRESULT, RECT, S_OK, WPARAM,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    PostMessageW, RegisterClassW, SetForegroundWindow, TranslateMessage, CW_USEDEFAULT, MSG,
    WM_CLOSE, WM_DESTROY, WNDCLASSW, WS_CAPTION, WS_SYSMENU, WS_VISIBLE,
};

const IID_UNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_ENV_COMPLETED: GUID = GUID::from_u128(0x4e8a3389_c9d8_4bd2_b6b5_124fee6cc14d);
const IID_CONTROLLER_COMPLETED: GUID = GUID::from_u128(0x6c4819f3_c9b7_4260_8127_c9f5bde7f68c);
const IID_SOURCE_CHANGED: GUID = GUID::from_u128(0x3c067f9f_5388_4772_8b48_79f7ef1ab37c);
const IID_GET_COOKIES: GUID = GUID::from_u128(0x5a4f5069_5c15_47c3_8646_f4de1c116670);
const IID_WEBVIEW2_2: GUID = GUID::from_u128(0x9e8f0cf8_e670_4b5e_b2bc_73e061e3184c);

pub fn login(base: &str, insecure: bool, data: &Path) -> Result<Option<String>, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let create = load_runtime()?;

        let instance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("fsync_webview"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            Default::default(),
            w!("fsync_webview"),
            w!("Filestash — Login"),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            680,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;
        let _ = SetForegroundWindow(hwnd);

        let state = Rc::new(State {
            base: base.to_owned(),
            hwnd,
            webview: Cell::new(std::ptr::null_mut()),
            controller: Cell::new(std::ptr::null_mut()),
            token: RefCell::new(None),
            error: RefCell::new(None),
            busy: Cell::new(false),
            done: Cell::new(false),
        });
        CURRENT.with_borrow_mut(|current| *current = Some(state.clone()));

        let user_data = data.join("webview");
        let _ = std::fs::create_dir_all(&user_data);
        let user_data = wide(&user_data.to_string_lossy());
        let options = insecure.then(Options::create);
        let on_env = {
            let state = state.clone();
            Completion::create(IID_ENV_COMPLETED, move |hr, env| on_environment(&state, hr, env))
        };
        let hr = create(
            true,
            0,
            PCWSTR(user_data.as_ptr()),
            options.unwrap_or(std::ptr::null_mut()),
            on_env,
        );
        com_release(on_env);
        if let Some(options) = options {
            com_release(options);
        }
        if hr.is_err() {
            let _ = DestroyWindow(hwnd);
            CURRENT.with_borrow_mut(|current| *current = None);
            return Err(format!("webview2: environment failed ({hr})"));
        }

        let mut msg = MSG::default();
        while !state.done.get() && GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let controller = state.controller.replace(std::ptr::null_mut());
        if !controller.is_null() {
            let _ = (vt::<ControllerVtbl>(controller).close)(controller);
            com_release(controller);
        }
        let webview = state.webview.replace(std::ptr::null_mut());
        if !webview.is_null() {
            com_release(webview);
        }
        CURRENT.with_borrow_mut(|current| *current = None);
        if let Some(error) = state.error.borrow_mut().take() {
            return Err(error);
        }
        let token = state.token.borrow_mut().take();
        Ok(token)
    }
}

struct State {
    base: String,
    hwnd: HWND,
    webview: Cell<*mut c_void>,
    controller: Cell<*mut c_void>,
    token: RefCell<Option<String>>,
    error: RefCell<Option<String>>,
    busy: Cell<bool>,
    done: Cell<bool>,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        CURRENT.with_borrow(|current| {
            if let Some(state) = current.as_ref().filter(|s| s.hwnd == hwnd) {
                state.done.set(true);
            }
        });
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn fail(state: &Rc<State>, message: String) {
    *state.error.borrow_mut() = Some(message);
    unsafe {
        let _ = PostMessageW(Some(state.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

unsafe fn on_environment(state: &Rc<State>, hr: HRESULT, env: *mut c_void) {
    if hr.is_err() || env.is_null() {
        return fail(state, format!("webview2: environment failed ({hr})"));
    }
    let on_controller = {
        let state = state.clone();
        Completion::create(IID_CONTROLLER_COMPLETED, move |hr, controller| {
            on_controller(&state, hr, controller)
        })
    };
    let hr = (vt::<EnvironmentVtbl>(env).create_controller)(env, state.hwnd, on_controller);
    com_release(on_controller);
    if hr.is_err() {
        fail(state, format!("webview2: controller failed ({hr})"));
    }
}

unsafe fn on_controller(state: &Rc<State>, hr: HRESULT, controller: *mut c_void) {
    if hr.is_err() || controller.is_null() {
        return fail(state, format!("webview2: controller failed ({hr})"));
    }
    com_add_ref(controller);
    state.controller.set(controller);
    let mut rect = RECT::default();
    let _ = GetClientRect(state.hwnd, &mut rect);
    let controller_vt = vt::<ControllerVtbl>(controller);
    let _ = (controller_vt.put_is_visible)(controller, 1);
    let _ = (controller_vt.put_bounds)(controller, rect);
    let mut webview = std::ptr::null_mut();
    if (controller_vt.get_core_webview2)(controller, &mut webview).is_err() || webview.is_null() {
        return fail(state, "webview2: no webview".into());
    }
    state.webview.set(webview);
    let on_source = {
        let state = state.clone();
        Event::create(IID_SOURCE_CHANGED, move |_, _| on_source_changed(&state))
    };
    let mut token = 0i64;
    let webview_vt = vt::<WebViewVtbl>(webview);
    let _ = (webview_vt.add_source_changed)(webview, on_source, &mut token);
    com_release(on_source);
    let url = wide(&format!("{}/login", state.base));
    let _ = (webview_vt.navigate)(webview, PCWSTR(url.as_ptr()));
}

unsafe fn on_source_changed(state: &Rc<State>) {
    let webview = state.webview.get();
    if webview.is_null() {
        return;
    }
    let mut source = PWSTR::null();
    if (vt::<WebViewVtbl>(webview).get_source)(webview, &mut source).is_err() {
        return;
    }
    let url = take_pwstr(source);
    let on_files = url::Url::parse(&url).is_ok_and(|u| u.path().starts_with("/files"));
    if !on_files || state.busy.replace(true) {
        return;
    }
    let mut webview2 = std::ptr::null_mut();
    if com_query(webview, &IID_WEBVIEW2_2, &mut webview2).is_err() || webview2.is_null() {
        state.busy.set(false);
        return;
    }
    let mut manager = std::ptr::null_mut();
    let hr = (vt::<WebView2Vtbl>(webview2).get_cookie_manager)(webview2, &mut manager);
    com_release(webview2);
    if hr.is_err() || manager.is_null() {
        state.busy.set(false);
        return;
    }
    let on_cookies = {
        let state = state.clone();
        Completion::create(IID_GET_COOKIES, move |hr, list| on_cookies(&state, hr, list))
    };
    let uri = wide(&format!("{}/api/", state.base));
    let _ = (vt::<CookieManagerVtbl>(manager).get_cookies)(manager, PCWSTR(uri.as_ptr()), on_cookies);
    com_release(on_cookies);
    com_release(manager);
}

unsafe fn on_cookies(state: &Rc<State>, hr: HRESULT, list: *mut c_void) {
    state.busy.set(false);
    if hr.is_err() || list.is_null() {
        return;
    }
    let list_vt = vt::<CookieListVtbl>(list);
    let mut count = 0u32;
    let _ = (list_vt.get_count)(list, &mut count);
    let mut cookies = Vec::new();
    for index in 0..count {
        let mut cookie = std::ptr::null_mut();
        if (list_vt.get_value_at_index)(list, index, &mut cookie).is_err() || cookie.is_null() {
            continue;
        }
        let cookie_vt = vt::<CookieVtbl>(cookie);
        let (mut name, mut value) = (PWSTR::null(), PWSTR::null());
        let _ = (cookie_vt.get_name)(cookie, &mut name);
        let _ = (cookie_vt.get_value)(cookie, &mut value);
        cookies.push((take_pwstr(name), take_pwstr(value)));
        com_release(cookie);
    }
    let token = assemble_token(&cookies);
    if !token.is_empty() {
        *state.token.borrow_mut() = Some(token);
        let _ = PostMessageW(Some(state.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

type CreateEnvironment =
    unsafe extern "system" fn(bool, i32, PCWSTR, *mut c_void, *mut c_void) -> HRESULT;

fn load_runtime() -> Result<CreateEnvironment, String> {
    const CLIENT: &str = r"Microsoft\EdgeUpdate\ClientState\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => "x86",
    };
    let candidates = [
        (windows_registry::CURRENT_USER, format!(r"SOFTWARE\{CLIENT}")),
        (windows_registry::LOCAL_MACHINE, format!(r"SOFTWARE\WOW6432Node\{CLIENT}")),
        (windows_registry::LOCAL_MACHINE, format!(r"SOFTWARE\{CLIENT}")),
    ];
    for (root, key) in candidates {
        let Ok(dir) = root.open(&key).and_then(|key| key.get_string("EBWebView")) else {
            continue;
        };
        let dll = Path::new(&dir)
            .join("EBWebView")
            .join(arch)
            .join("EmbeddedBrowserWebView.dll");
        if !dll.exists() {
            continue;
        }
        unsafe {
            let dll = wide(&dll.to_string_lossy());
            let Ok(library) = LoadLibraryW(PCWSTR(dll.as_ptr())) else {
                continue;
            };
            if let Some(create) = GetProcAddress(
                library,
                windows::core::s!("CreateWebViewEnvironmentWithOptionsInternal"),
            ) {
                return Ok(std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    CreateEnvironment,
                >(create));
            }
        }
    }
    Err("the WebView2 runtime is not installed".into())
}

unsafe fn vt<T>(ptr: *mut c_void) -> &'static T {
    &**(ptr as *const *const T)
}

#[repr(C)]
struct IUnknownVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

unsafe fn com_query(ptr: *mut c_void, iid: &GUID, out: *mut *mut c_void) -> HRESULT {
    (vt::<IUnknownVtbl>(ptr).query_interface)(ptr, iid, out)
}

unsafe fn com_add_ref(ptr: *mut c_void) {
    (vt::<IUnknownVtbl>(ptr).add_ref)(ptr);
}

unsafe fn com_release(ptr: *mut c_void) {
    (vt::<IUnknownVtbl>(ptr).release)(ptr);
}

#[repr(C)]
struct EnvironmentVtbl {
    base: IUnknownVtbl,
    create_controller: unsafe extern "system" fn(*mut c_void, HWND, *mut c_void) -> HRESULT,
}

#[repr(C)]
struct ControllerVtbl {
    base: IUnknownVtbl,
    get_is_visible: usize,
    put_is_visible: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
    get_bounds: usize,
    put_bounds: unsafe extern "system" fn(*mut c_void, RECT) -> HRESULT,
    get_zoom_factor: usize,
    put_zoom_factor: usize,
    add_zoom_factor_changed: usize,
    remove_zoom_factor_changed: usize,
    set_bounds_and_zoom_factor: usize,
    move_focus: usize,
    add_move_focus_requested: usize,
    remove_move_focus_requested: usize,
    add_got_focus: usize,
    remove_got_focus: usize,
    add_lost_focus: usize,
    remove_lost_focus: usize,
    add_accelerator_key_pressed: usize,
    remove_accelerator_key_pressed: usize,
    get_parent_window: usize,
    put_parent_window: usize,
    notify_parent_window_position_changed: usize,
    close: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    get_core_webview2: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct WebViewVtbl {
    base: IUnknownVtbl,
    get_settings: usize,
    get_source: unsafe extern "system" fn(*mut c_void, *mut PWSTR) -> HRESULT,
    navigate: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    navigate_to_string: usize,
    add_navigation_starting: usize,
    remove_navigation_starting: usize,
    add_content_loading: usize,
    remove_content_loading: usize,
    add_source_changed: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut i64) -> HRESULT,
}

#[repr(C)]
struct WebView2Vtbl {
    base: IUnknownVtbl,
    webview1: [usize; 58],
    add_web_resource_response_received: usize,
    remove_web_resource_response_received: usize,
    navigate_with_web_resource_request: usize,
    add_dom_content_loaded: usize,
    remove_dom_content_loaded: usize,
    get_cookie_manager: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct CookieManagerVtbl {
    base: IUnknownVtbl,
    create_cookie: usize,
    copy_cookie: usize,
    get_cookies: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT,
}

#[repr(C)]
struct CookieListVtbl {
    base: IUnknownVtbl,
    get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    get_value_at_index: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
}

#[repr(C)]
struct CookieVtbl {
    base: IUnknownVtbl,
    get_name: unsafe extern "system" fn(*mut c_void, *mut PWSTR) -> HRESULT,
    get_value: unsafe extern "system" fn(*mut c_void, *mut PWSTR) -> HRESULT,
}

type CompletionFn = Box<dyn FnOnce(HRESULT, *mut c_void)>;

#[repr(C)]
struct Completion {
    vtbl: &'static CompletionVtbl,
    refs: Cell<u32>,
    iid: GUID,
    invoke: RefCell<Option<CompletionFn>>,
}

#[repr(C)]
struct CompletionVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    invoke: unsafe extern "system" fn(*mut c_void, HRESULT, *mut c_void) -> HRESULT,
}

impl Completion {
    fn create(iid: GUID, invoke: impl FnOnce(HRESULT, *mut c_void) + 'static) -> *mut c_void {
        static VTBL: CompletionVtbl = CompletionVtbl {
            query_interface: qi::<Completion>,
            add_ref: add_ref::<Completion>,
            release: release::<Completion>,
            invoke: completion_invoke,
        };
        Box::into_raw(Box::new(Completion {
            vtbl: &VTBL,
            refs: Cell::new(1),
            iid,
            invoke: RefCell::new(Some(Box::new(invoke))),
        })) as *mut c_void
    }
}

unsafe extern "system" fn completion_invoke(
    this: *mut c_void,
    hr: HRESULT,
    arg: *mut c_void,
) -> HRESULT {
    let handler = &*(this as *const Completion);
    if let Some(invoke) = handler.invoke.borrow_mut().take() {
        invoke(hr, arg);
    }
    S_OK
}

#[repr(C)]
struct Event {
    vtbl: &'static EventVtbl,
    refs: Cell<u32>,
    iid: GUID,
    invoke: Box<dyn Fn(*mut c_void, *mut c_void)>,
}

#[repr(C)]
struct EventVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    invoke: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT,
}

impl Event {
    fn create(iid: GUID, invoke: impl Fn(*mut c_void, *mut c_void) + 'static) -> *mut c_void {
        static VTBL: EventVtbl = EventVtbl {
            query_interface: qi::<Event>,
            add_ref: add_ref::<Event>,
            release: release::<Event>,
            invoke: event_invoke,
        };
        Box::into_raw(Box::new(Event {
            vtbl: &VTBL,
            refs: Cell::new(1),
            iid,
            invoke: Box::new(invoke),
        })) as *mut c_void
    }
}

unsafe extern "system" fn event_invoke(
    this: *mut c_void,
    sender: *mut c_void,
    args: *mut c_void,
) -> HRESULT {
    let handler = &*(this as *const Event);
    (handler.invoke)(sender, args);
    S_OK
}

trait ComObject {
    fn refs(&self) -> &Cell<u32>;
    fn iid(&self) -> &GUID;
}

impl ComObject for Completion {
    fn refs(&self) -> &Cell<u32> {
        &self.refs
    }
    fn iid(&self) -> &GUID {
        &self.iid
    }
}

impl ComObject for Event {
    fn refs(&self) -> &Cell<u32> {
        &self.refs
    }
    fn iid(&self) -> &GUID {
        &self.iid
    }
}

unsafe extern "system" fn qi<T: ComObject>(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let object = &*(this as *const T);
    if *riid == IID_UNKNOWN || *riid == *object.iid() {
        object.refs().set(object.refs().get() + 1);
        *out = this;
        S_OK
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn add_ref<T: ComObject>(this: *mut c_void) -> u32 {
    let object = &*(this as *const T);
    object.refs().set(object.refs().get() + 1);
    object.refs().get()
}

unsafe extern "system" fn release<T: ComObject>(this: *mut c_void) -> u32 {
    let object = &*(this as *const T);
    let refs = object.refs().get() - 1;
    object.refs().set(refs);
    if refs == 0 {
        drop(Box::from_raw(this as *mut T));
    }
    refs
}

#[repr(C)]
struct Options {
    vtbl: &'static OptionsVtbl,
    refs: Cell<u32>,
    iid: GUID,
}

#[repr(C)]
struct OptionsVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_additional_browser_arguments:
        unsafe extern "system" fn(*mut c_void, *mut PWSTR) -> HRESULT,
    put_additional_browser_arguments: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    get_language: unsafe extern "system" fn(*mut c_void, *mut PWSTR) -> HRESULT,
    put_language: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    get_target_compatible_browser_version:
        unsafe extern "system" fn(*mut c_void, *mut PWSTR) -> HRESULT,
    put_target_compatible_browser_version:
        unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    get_allow_sso: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
    put_allow_sso: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
}

unsafe extern "system" fn options_browser_arguments(_: *mut c_void, out: *mut PWSTR) -> HRESULT {
    co_str("--ignore-certificate-errors", out)
}

unsafe extern "system" fn options_language(_: *mut c_void, out: *mut PWSTR) -> HRESULT {
    co_str("", out)
}

unsafe extern "system" fn options_browser_version(_: *mut c_void, out: *mut PWSTR) -> HRESULT {
    co_str("89.0.774.44", out)
}

unsafe extern "system" fn options_allow_sso(_: *mut c_void, out: *mut i32) -> HRESULT {
    *out = 0;
    S_OK
}

unsafe extern "system" fn options_put_str(_: *mut c_void, _: PCWSTR) -> HRESULT {
    S_OK
}

unsafe extern "system" fn options_put_bool(_: *mut c_void, _: i32) -> HRESULT {
    S_OK
}

impl Options {
    fn create() -> *mut c_void {
        const IID_OPTIONS: GUID = GUID::from_u128(0x2fde08a8_1e9a_4766_8c05_95a9ceb9d1c5);
        static VTBL: OptionsVtbl = OptionsVtbl {
            query_interface: qi::<Options>,
            add_ref: add_ref::<Options>,
            release: release::<Options>,
            get_additional_browser_arguments: options_browser_arguments,
            put_additional_browser_arguments: options_put_str,
            get_language: options_language,
            put_language: options_put_str,
            get_target_compatible_browser_version: options_browser_version,
            put_target_compatible_browser_version: options_put_str,
            get_allow_sso: options_allow_sso,
            put_allow_sso: options_put_bool,
        };
        Box::into_raw(Box::new(Options {
            vtbl: &VTBL,
            refs: Cell::new(1),
            iid: IID_OPTIONS,
        })) as *mut c_void
    }
}

impl ComObject for Options {
    fn refs(&self) -> &Cell<u32> {
        &self.refs
    }
    fn iid(&self) -> &GUID {
        &self.iid
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn co_str(text: &str, out: *mut PWSTR) -> HRESULT {
    let utf16 = wide(text);
    let ptr = CoTaskMemAlloc(utf16.len() * 2) as *mut u16;
    if ptr.is_null() {
        return E_OUTOFMEMORY;
    }
    std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
    *out = PWSTR(ptr);
    S_OK
}

unsafe fn take_pwstr(ptr: PWSTR) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let text = ptr.to_string().unwrap_or_default();
    CoTaskMemFree(Some(ptr.as_ptr() as *const c_void));
    text
}
