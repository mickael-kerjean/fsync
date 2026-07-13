use std::cell::RefCell;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, COLOR_3DFACE, DEFAULT_GUI_FONT, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetDlgItem, GetMessageW,
    GetWindowTextLengthW, GetWindowTextW, IsDialogMessageW, LoadIconW, MessageBoxW, PeekMessageW,
    PostQuitMessage, PostThreadMessageW, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetWindowTextW, TrackPopupMenu, TranslateMessage, BS_DEFPUSHBUTTON, CW_USEDEFAULT,
    ES_AUTOHSCROLL, HICON, HMENU, IDI_APPLICATION, IMAGE_FLAGS, MB_ICONERROR, MB_ICONINFORMATION,
    MB_OK, MESSAGEBOX_STYLE, MF_CHECKED, MF_SEPARATOR, MF_STRING, MSG, PM_REMOVE, SW_SHOWNORMAL,
    TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

pub fn alert(message: &str) {
    message_box(message, MB_ICONERROR);
}

pub fn info(message: &str) {
    message_box(message, MB_ICONINFORMATION);
}

fn message_box(message: &str, icon: MESSAGEBOX_STYLE) {
    let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(None, PCWSTR(text.as_ptr()), w!("Filestash"), MB_OK | icon);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub url: String,
    pub token: String,
    pub user: String,
    pub password: String,
    pub storage: String,
    pub insecure: bool,
}

impl From<fdrive_core::config::Session> for Credentials {
    fn from(session: fdrive_core::config::Session) -> Self {
        Self {
            url: session.url,
            token: session.token,
            insecure: session.insecure,
            ..Default::default()
        }
    }
}

#[derive(Debug)]
pub enum TrayEvent {
    Browse,
    Login(Credentials),
    Logout,
    Restart,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Status {
    #[default]
    LoggedOut,
    Ok,
    Syncing,
    Error,
}

impl Status {
    fn icon_bytes(self) -> &'static [u8] {
        match self {
            Status::LoggedOut => include_bytes!(concat!(env!("OUT_DIR"), "/tray-unlogged.ico")),
            Status::Ok => include_bytes!(concat!(env!("OUT_DIR"), "/tray-ok.ico")),
            Status::Syncing => include_bytes!(concat!(env!("OUT_DIR"), "/tray-sync.ico")),
            Status::Error => include_bytes!(concat!(env!("OUT_DIR"), "/tray-error.ico")),
        }
    }

    fn tip(self) -> &'static str {
        match self {
            Status::LoggedOut => "Filestash — not signed in",
            Status::Ok => "Filestash",
            Status::Syncing => "Filestash — syncing",
            Status::Error => "Filestash — sync error",
        }
    }
}

#[derive(Default)]
pub struct TrayState {
    pub status: Status,
    pub url: Option<String>,
    pub user: String,
    pub storage: String,
}

pub struct Tray {
    state: Arc<Mutex<TrayState>>,
    thread: u32,
}

impl Tray {
    pub fn state(&self) -> &Arc<Mutex<TrayState>> {
        &self.state
    }

    pub fn set_status(&self, status: Status) {
        {
            let mut state = self.state.lock().unwrap();
            if state.status == status {
                return;
            }
            state.status = status;
        }
        unsafe {
            let _ = PostThreadMessageW(self.thread, WM_TRAY_REFRESH, WPARAM(0), LPARAM(0));
        }
    }

    pub fn prompt_login(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread, WM_TRAY_LOGIN, WPARAM(0), LPARAM(0));
        }
    }
}

const WM_TRAY: u32 = WM_APP + 1;
const WM_TRAY_REFRESH: u32 = WM_APP + 2;
const WM_TRAY_LOGIN: u32 = WM_APP + 3;
const CMD_BROWSE: usize = 1;
const CMD_LOGIN: usize = 2;
const CMD_LOGOUT: usize = 3;
const CMD_RESTART: usize = 4;
const CMD_QUIT: usize = 5;
const CMD_LOGS: usize = 6;
const CMD_AUTOSTART: usize = 7;

struct Ctx {
    state: Arc<Mutex<TrayState>>,
    events: tokio::sync::mpsc::UnboundedSender<TrayEvent>,
    log_path: PathBuf,
    autostart_opt_out: PathBuf,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
    static DIALOG: RefCell<DialogState> = const { RefCell::new(DialogState::Closed) };
}

enum DialogState {
    Closed,
    Open,
    Submitted,
    Cancelled,
}

pub fn spawn(
    state: Arc<Mutex<TrayState>>,
    events: tokio::sync::mpsc::UnboundedSender<TrayEvent>,
    log_path: PathBuf,
    autostart_opt_out: PathBuf,
) -> std::io::Result<Tray> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread_state = state.clone();
    std::thread::Builder::new()
        .name("fdrive-tray".into())
        .spawn(move || {
            let _ = ready_tx.send(unsafe { GetCurrentThreadId() });
            if let Err(err) = tray_thread(thread_state, events, log_path, autostart_opt_out) {
                log::error!("tray: {err}");
            }
        })?;
    let thread = ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| std::io::Error::other("tray thread did not start"))?;
    Ok(Tray { state, thread })
}

fn tray_thread(
    state: Arc<Mutex<TrayState>>,
    events: tokio::sync::mpsc::UnboundedSender<TrayEvent>,
    log_path: PathBuf,
    autostart_opt_out: PathBuf,
) -> windows::core::Result<()> {
    CTX.with_borrow_mut(|ctx| {
        *ctx = Some(Ctx {
            state,
            events,
            log_path,
            autostart_opt_out,
        })
    });
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(tray_wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("fdrive_tray"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            Default::default(),
            w!("fdrive_tray"),
            w!("Filestash"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        let mut data = icon_data(hwnd);
        if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
            log::error!("Shell_NotifyIconW failed; no tray icon");
        }
        log::info!("tray icon up");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            if msg.hwnd.is_invalid() && msg.message == WM_TRAY_REFRESH {
                data = icon_data(hwnd);
                let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
                continue;
            }
            if msg.hwnd.is_invalid() && msg.message == WM_TRAY_LOGIN {
                prompt_login();
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
    Ok(())
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let status =
        CTX.with_borrow(|ctx| ctx.as_ref().expect("tray ctx").state.lock().unwrap().status);
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: status_icon(status),
        ..Default::default()
    };
    let tip: Vec<u16> = status.tip().encode_utf16().take(127).collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);
    data
}

fn status_icon(status: Status) -> HICON {
    thread_local! {
        static CACHE: RefCell<std::collections::HashMap<Status, HICON>> =
            RefCell::new(std::collections::HashMap::new());
    }
    CACHE.with_borrow_mut(|cache| {
        *cache.entry(status).or_insert_with(|| {
            icon_from_ico(status.icon_bytes())
                .unwrap_or_else(|| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() })
        })
    })
}

fn icon_from_ico(bytes: &[u8]) -> Option<HICON> {
    let count = u16::from_le_bytes([*bytes.get(4)?, *bytes.get(5)?]) as usize;
    let mut best: Option<(u32, usize, usize)> = None;
    for i in 0..count {
        let entry = bytes.get(6 + i * 16..6 + i * 16 + 16)?;
        let width = if entry[0] == 0 {
            256
        } else {
            u32::from(entry[0])
        };
        let size = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        let fit = width.abs_diff(32);
        if best.is_none_or(|(best_fit, ..)| fit < best_fit) {
            best = Some((fit, offset, size));
        }
    }
    let (_, offset, size) = best?;
    let frame = bytes.get(offset..offset + size)?;
    unsafe { CreateIconFromResourceEx(frame, true, 0x0003_0000, 32, 32, IMAGE_FLAGS(0)).ok() }
}

unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY => {
            let mouse = lparam.0 as u32;
            if mouse == WM_RBUTTONUP || mouse == WM_LBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn prompt_login() {
    let prefill = CTX.with_borrow(|ctx| {
        let ctx = ctx.as_ref().expect("tray ctx");
        let state = ctx.state.lock().unwrap();
        Credentials {
            url: state.url.clone().unwrap_or_default(),
            ..Default::default()
        }
    });
    if let Some(credentials) = login_dialog(prefill) {
        CTX.with_borrow(|ctx| {
            let _ = ctx
                .as_ref()
                .expect("tray ctx")
                .events
                .send(TrayEvent::Login(credentials));
        });
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let logged_in = CTX.with_borrow(|ctx| {
        let ctx = ctx.as_ref().expect("tray ctx");
        let state = ctx.state.lock().unwrap();
        state.status != Status::LoggedOut
    });
    let Ok(menu) = CreatePopupMenu() else { return };
    let autostart = if crate::wire::shell::autostart_enabled() {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    if logged_in {
        let _ = AppendMenuW(menu, MF_STRING, CMD_BROWSE, w!("Browse"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_LOGS, w!("Logs"));
        let _ = AppendMenuW(menu, autostart, CMD_AUTOSTART, w!("Autostart"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_LOGOUT, w!("Logout"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_RESTART, w!("Restart"));
    } else {
        let _ = AppendMenuW(menu, MF_STRING, CMD_LOGIN, w!("Login"));
        let _ = AppendMenuW(menu, autostart, CMD_AUTOSTART, w!("Autostart"));
    }
    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit"));

    let mut point = Default::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(hwnd);
    let picked = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_BOTTOMALIGN,
        point.x,
        point.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);

    let send = |event: TrayEvent| {
        CTX.with_borrow(|ctx| {
            let _ = ctx.as_ref().expect("tray ctx").events.send(event);
        })
    };
    match picked.0 as usize {
        CMD_BROWSE => send(TrayEvent::Browse),
        CMD_LOGIN => prompt_login(),
        CMD_LOGOUT => send(TrayEvent::Logout),
        CMD_LOGS => CTX.with_borrow(|ctx| {
            let wide = wide_path(&ctx.as_ref().expect("tray ctx").log_path);
            unsafe {
                ShellExecuteW(
                    None,
                    w!("open"),
                    w!("notepad.exe"),
                    PCWSTR(wide.as_ptr()),
                    None,
                    SW_SHOWNORMAL,
                );
            }
        }),
        CMD_AUTOSTART => CTX.with_borrow(|ctx| {
            let opt_out = &ctx.as_ref().expect("tray ctx").autostart_opt_out;
            let result = if crate::wire::shell::autostart_enabled() {
                std::fs::write(opt_out, []).and_then(|()| crate::wire::shell::set_autostart(false))
            } else {
                let _ = std::fs::remove_file(opt_out);
                crate::wire::shell::set_autostart(true)
            };
            if let Err(err) = result {
                log::error!("autostart: {err}");
            }
        }),
        CMD_RESTART => send(TrayEvent::Restart),
        CMD_QUIT => send(TrayEvent::Quit),
        _ => {}
    }
}

fn wide_path(path: &std::path::Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub fn open_folder(path: &std::path::Path) {
    let wide = wide_path(path);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

const ID_SERVER: i32 = 101;
const ID_OK: i32 = 1;
const ID_CANCEL: i32 = 2;

fn login_dialog(prefill: Credentials) -> Option<Credentials> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(login_wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("fdrive_login"),
            hIcon: LoadIconW(Some(instance.into()), PCWSTR(1 as _)).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_3DFACE.0 + 1) as _),
            ..Default::default()
        };
        RegisterClassW(&class);

        let hwnd = CreateWindowExW(
            Default::default(),
            w!("fdrive_login"),
            w!("Filestash — Login"),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            360,
            135,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;

        let font = GetStockObject(DEFAULT_GUI_FONT);
        let child =
            |class: PCWSTR, text: PCWSTR, style: u32, x: i32, y: i32, w: i32, h: i32, id: i32| {
                if let Ok(ctl) = CreateWindowExW(
                    Default::default(),
                    class,
                    text,
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                    x,
                    y,
                    w,
                    h,
                    Some(hwnd),
                    Some(HMENU(id as _)),
                    Some(instance.into()),
                    None,
                ) {
                    SendMessageW(
                        ctl,
                        WM_SETFONT,
                        Some(WPARAM(font.0 as usize)),
                        Some(LPARAM(1)),
                    );
                }
            };
        child(w!("STATIC"), w!("Server"), 0, 12, 18, 80, 20, 0);
        child(
            w!("EDIT"),
            PCWSTR::null(),
            WS_BORDER.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32,
            100,
            15,
            230,
            22,
            ID_SERVER,
        );
        child(
            w!("BUTTON"),
            w!("Login"),
            WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
            150,
            55,
            85,
            26,
            ID_OK,
        );
        child(
            w!("BUTTON"),
            w!("Cancel"),
            WS_TABSTOP.0,
            245,
            55,
            85,
            26,
            ID_CANCEL,
        );
        set_text(hwnd, ID_SERVER, &prefill.url);
        let _ = SetForegroundWindow(hwnd);
        if let Ok(first) = GetDlgItem(Some(hwnd), ID_SERVER) {
            let _ = SetFocus(Some(first));
        }

        DIALOG.with_borrow_mut(|d| *d = DialogState::Open);
        let mut msg = MSG::default();
        loop {
            let submitted = DIALOG.with_borrow(|d| match d {
                DialogState::Open => None,
                DialogState::Submitted => Some(true),
                DialogState::Cancelled | DialogState::Closed => Some(false),
            });
            if let Some(submitted) = submitted {
                let raw = get_text(hwnd, ID_SERVER);
                DIALOG.with_borrow_mut(|d| *d = DialogState::Closed);
                let _ = DestroyWindow(hwnd);
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if !submitted || raw.trim().is_empty() {
                    return None;
                }
                let url = fdrive_core::sdk::normalize_server(&raw);
                if let Err(err) = fdrive_core::sdk::Sdk::builder(&url)
                    .insecure(prefill.insecure)
                    .probe_blocking()
                {
                    alert(&format!(
                        "{url} does not look like a Filestash server.\n\n{err}"
                    ));
                    return None;
                }
                let data = CTX.with_borrow(|ctx| {
                    ctx.as_ref()
                        .expect("tray ctx")
                        .log_path
                        .parent()
                        .expect("data dir")
                        .to_path_buf()
                });
                return match crate::webview::login(&url, prefill.insecure, &data) {
                    Ok(Some(token)) => Some(Credentials {
                        url,
                        token,
                        insecure: prefill.insecure,
                        ..Default::default()
                    }),
                    Ok(None) => None,
                    Err(err) => {
                        alert(&format!(
                            "{err}\n\nInstall the WebView2 runtime, or use --token / --user from the command line."
                        ));
                        None
                    }
                };
            }
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                return None;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

unsafe extern "system" fn login_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match (wparam.0 & 0xffff) as i32 {
                ID_OK => DIALOG.with_borrow_mut(|d| *d = DialogState::Submitted),
                ID_CANCEL => DIALOG.with_borrow_mut(|d| *d = DialogState::Cancelled),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            DIALOG.with_borrow_mut(|d| *d = DialogState::Cancelled);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn set_text(hwnd: HWND, id: i32, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if let Ok(ctl) = GetDlgItem(Some(hwnd), id) {
            let _ = SetWindowTextW(ctl, PCWSTR(wide.as_ptr()));
        }
    }
}

fn get_text(hwnd: HWND, id: i32) -> String {
    unsafe {
        let Ok(ctl) = GetDlgItem(Some(hwnd), id) else {
            return String::new();
        };
        let len = GetWindowTextLengthW(ctl);
        let mut buf = vec![0u16; len as usize + 1];
        let got = GetWindowTextW(ctl, &mut buf);
        String::from_utf16_lossy(&buf[..got as usize])
    }
}
