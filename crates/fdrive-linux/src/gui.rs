use std::path::{Path, PathBuf};

use gtk::prelude::*;
use libayatana_appindicator::{AppIndicator, AppIndicatorStatus};
use tokio::sync::mpsc::UnboundedSender;

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

pub use fdrive_core::sdk::normalize_server;

#[derive(Debug, Clone)]
pub enum TrayEvent {
    Login,
    Logout,
    Restart,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    LoggedOut,
    Ok,
    Syncing,
    Error,
}

impl Status {
    fn tip(self) -> &'static str {
        match self {
            Self::LoggedOut => "Filestash — not signed in",
            Self::Ok => "Filestash",
            Self::Syncing => "Filestash — syncing",
            Self::Error => "Filestash — sync error",
        }
    }

    fn icon_name(self) -> &'static str {
        match self {
            Self::LoggedOut => "icon-base",
            Self::Ok => "icon-ok",
            Self::Syncing => "icon-sync",
            Self::Error => "icon-error",
        }
    }
}

fn ensure_icons(dir: &Path) -> std::io::Result<()> {
    const ICONS: [(&str, &str); 4] = [
        (
            "icon-base.svg",
            include_str!("../../fdrive-core/icons/icon-base.svg"),
        ),
        (
            "icon-ok.svg",
            include_str!("../../fdrive-core/icons/icon-ok.svg"),
        ),
        (
            "icon-sync.svg",
            include_str!("../../fdrive-core/icons/icon-sync.svg"),
        ),
        (
            "icon-error.svg",
            include_str!("../../fdrive-core/icons/icon-error.svg"),
        ),
    ];
    std::fs::create_dir_all(dir)?;
    for (name, svg) in ICONS {
        std::fs::write(dir.join(name), update_svg(svg))?;
    }
    Ok(())
}

fn update_svg(svg: &str) -> String {
    use std::sync::LazyLock;
    static PAINT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(stroke|fill):#([0-9a-fA-F]{3,6})").unwrap());
    static WIDTH: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r#"stroke-width="([0-9.]+)"#).unwrap());

    let is_white = |hex: &str| matches!(hex.to_lowercase().as_str(), "fff" | "ffffff");
    let out = PAINT.replace_all(svg, |caps: &regex::Captures| {
        match (&caps[1], is_white(&caps[2])) {
            ("stroke", _) => "stroke:#ffffff".to_string(),
            ("fill", true) => caps[0].to_string(),
            ("fill", _) => "fill:none".to_string(),
            _ => unreachable!(),
        }
    });
    WIDTH
        .replace_all(&out, |caps: &regex::Captures| {
            let width: f32 = caps[1].parse().unwrap_or(0.0);
            format!(r#"stroke-width="{:.3}"#, width * 0.6)
        })
        .into_owned()
}

struct Ctx {
    events: UnboundedSender<TrayEvent>,
    mount: PathBuf,
}

enum TrayMsg {
    Set(Status, bool),
    Login(
        Credentials,
        tokio::sync::oneshot::Sender<Option<Credentials>>,
    ),
    Quit,
}

pub struct Tray {
    tx: gtk::glib::Sender<TrayMsg>,
}

impl Tray {
    pub async fn spawn(
        events: UnboundedSender<TrayEvent>,
        data_dir: PathBuf,
        mount: PathBuf,
    ) -> std::io::Result<Self> {
        let icon_dir = data_dir.join("icons");
        ensure_icons(&icon_dir)?;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("fdrive-tray".into())
            .spawn(move || tray_thread(ready_tx, Ctx { events, mount }, icon_dir))?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(tx)) => Ok(Self { tx }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(std::io::Error::other("tray thread did not start")),
        }
    }

    pub async fn set(&self, status: Status, signed_in: bool) {
        let _ = self.tx.send(TrayMsg::Set(status, signed_in));
    }

    pub async fn login(&self, prefill: Credentials) -> Option<Credentials> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.tx.send(TrayMsg::Login(prefill, reply_tx)).is_err() {
            return None;
        }
        reply_rx.await.ok().flatten()
    }

    pub async fn shutdown(self) {
        let _ = self.tx.send(TrayMsg::Quit);
    }
}

type Ready = Result<gtk::glib::Sender<TrayMsg>, std::io::Error>;

fn tray_thread(ready: std::sync::mpsc::Sender<Ready>, ctx: Ctx, icon_dir: PathBuf) {
    if gtk::init().is_err() {
        let _ = ready.send(Err(std::io::Error::other(
            "could not connect GTK to the desktop session",
        )));
        return;
    }
    gtk::glib::log_set_writer_func(|level, fields| {
        let field = |key| {
            fields
                .iter()
                .find(|f| f.key() == key)
                .and_then(|f| f.value_str())
        };
        let benign = field("GLIB_DOMAIN") == Some("libayatana-appindicator")
            || field("MESSAGE").is_some_and(|m| m.contains("thaw_toplevel_updates"));
        if benign {
            gtk::glib::LogWriterOutput::Handled
        } else {
            gtk::glib::log_writer_default(level, fields)
        }
    });
    let (tx, rx) = gtk::glib::MainContext::channel(gtk::glib::PRIORITY_DEFAULT);
    let mut indicator = AppIndicator::with_path(
        "filestash",
        Status::LoggedOut.icon_name(),
        icon_dir.to_str().unwrap_or("."),
    );
    indicator.set_title(Status::LoggedOut.tip());
    indicator.set_status(AppIndicatorStatus::Active);
    let mut menu = build_menu(false, &ctx);
    indicator.set_menu(&mut menu);
    let _ = ready.send(Ok(tx));

    let mut signed_in = false;
    rx.attach(None, move |msg| match msg {
        TrayMsg::Set(status, signed) => {
            indicator.set_icon(status.icon_name());
            indicator.set_title(status.tip());
            if signed != signed_in {
                signed_in = signed;
                let mut menu = build_menu(signed_in, &ctx);
                indicator.set_menu(&mut menu);
            }
            gtk::glib::Continue(true)
        }
        TrayMsg::Login(prefill, reply) => {
            let _ = reply.send(show_login(prefill));
            gtk::glib::Continue(true)
        }
        TrayMsg::Quit => {
            gtk::main_quit();
            gtk::glib::Continue(false)
        }
    });
    gtk::main();
}

fn show_login(prefill: Credentials) -> Option<Credentials> {
    let dialog = gtk::Dialog::new();
    dialog.set_title("Filestash");
    dialog.set_default_size(320, -1);
    dialog.set_border_width(12);
    dialog.add_button("Login", gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);

    let grid = gtk::Grid::new();
    grid.set_row_spacing(8);
    grid.set_column_spacing(8);
    grid.set_margin_bottom(12);
    let label = gtk::Label::new(Some("Server"));
    label.set_halign(gtk::Align::Start);
    let server = gtk::Entry::new();
    server.set_hexpand(true);
    server.set_activates_default(true);
    server.set_text(&prefill.url);
    grid.attach(&label, 0, 0, 1, 1);
    grid.attach(&server, 1, 0, 1, 1);
    server.grab_focus();
    dialog.content_area().add(&grid);
    dialog.show_all();

    let accepted = dialog.run() == gtk::ResponseType::Accept;
    let raw = server.text();
    unsafe {
        dialog.destroy();
    }
    if !accepted || raw.trim().is_empty() {
        return None;
    }
    let url = normalize_server(&raw);
    if let Err(err) = fdrive_core::sdk::Sdk::builder(&url)
        .insecure(prefill.insecure)
        .probe_blocking()
    {
        alert(&format!(
            "{url} does not look like a Filestash server.\n\n{err}"
        ));
        return None;
    }
    match crate::webview::login(&url, prefill.insecure) {
        Ok(Some(token)) => Some(Credentials {
            url,
            token,
            insecure: prefill.insecure,
            ..Default::default()
        }),
        Ok(None) => None,
        Err(err) => {
            alert(&format!(
                "{err}\n\nInstall webkit2gtk, or use --token / --user from the command line."
            ));
            None
        }
    }
}

fn alert(message: &str) {
    let dialog = gtk::MessageDialog::new(
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Error,
        gtk::ButtonsType::Close,
        message,
    );
    dialog.set_title("Filestash");
    dialog.run();
    unsafe {
        dialog.destroy();
    }
}

fn build_menu(signed_in: bool, ctx: &Ctx) -> gtk::Menu {
    let menu = gtk::Menu::new();
    let item = |label: &str, event: TrayEvent| {
        let item = gtk::MenuItem::with_label(label);
        let events = ctx.events.clone();
        item.connect_activate(move |_| {
            let _ = events.send(event.clone());
        });
        item
    };
    if signed_in {
        if file_manager().is_some() {
            let browse = gtk::MenuItem::with_label("Browse");
            let mount = ctx.mount.clone();
            browse.connect_activate(move |_| open_folder(&mount));
            menu.append(&browse);
        }
        menu.append(&item("Logout", TrayEvent::Logout));
        menu.append(&item("Restart", TrayEvent::Restart));
    } else {
        menu.append(&item("Login…", TrayEvent::Login));
    }
    menu.append(&gtk::SeparatorMenuItem::new());
    menu.append(&item("Quit", TrayEvent::Quit));
    menu.show_all();
    menu
}

fn file_manager() -> Option<gtk::gio::AppInfo> {
    use gtk::gio;
    use gtk::gio::prelude::AppInfoExt;
    let is_manager = |app: &gio::AppInfo| {
        app.id()
            .and_then(|id| gio::DesktopAppInfo::new(&id))
            .and_then(|info| info.categories())
            .is_some_and(|categories| categories.split(';').any(|c| c == "FileManager"))
    };
    gio::AppInfo::default_for_type("inode/directory", false)
        .filter(is_manager)
        .or_else(|| {
            gio::AppInfo::all_for_type("inode/directory")
                .into_iter()
                .find(is_manager)
        })
}

fn open_folder(path: &Path) {
    use gtk::gio;
    use gtk::gio::prelude::AppInfoExt;
    if let Some(app) = file_manager() {
        let _ = app.launch(&[gio::File::for_path(path)], None::<&gio::AppLaunchContext>);
    }
}

pub fn default_data() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("filestash")
}

#[cfg(test)]
#[path = "gui_test.rs"]
mod tests;
