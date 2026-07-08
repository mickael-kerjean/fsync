#![windows_subsystem = "windows"]

mod args;
mod wire;
mod config;
mod adapter;
mod gui;
mod log;
mod webview;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fsync_core::config as session;
use fsync_core::path::RelPath;
use fsync_core::scheduler::UploadStatus;
use fsync_core::sdk::Sdk;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use crate::wire::{shell, viewer, watcher};
use crate::config::AppConfig;
use crate::adapter::Adapter;
use crate::gui::{Credentials, Status, Tray, TrayEvent, TrayState};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        log::error!("fatal: {err}");
        gui::alert(&err.to_string());
        std::process::exit(1);
    }
}

enum SessionEnd {
    Logout,
    Restart,
    Quit,
    Failed,
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let Some(setup) = args::init()? else {
        return Ok(());
    };
    let args::Setup {
        root,
        data,
        config_path,
        unregister,
        prefill_url,
        mut credentials,
        prompt_login,
    } = setup;
    log::init(&data)?;
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
    }));
    log::info!("fsync-windows {} starting", env!("CARGO_PKG_VERSION"));
    let config = AppConfig::load(&config_path)?;

    if unregister {
        shell::vacuum(&config.windows.provider_name, "");
        let _ = wire::unregister(&root);
        session::forget(&data);
        log::info!("unregistered {}", root.display());
        gui::info(&format!("Unregistered {}", root.display()));
        return Ok(());
    }

    let instance_lock = instance_lock(&data, &root)?;

    shell::ensure_autostart(&data.join("autostart.off"));

    let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let tray = gui::spawn(
        Arc::new(Mutex::new(TrayState {
            url: prefill_url,
            ..Default::default()
        })),
        events_tx.clone(),
        data.join("fsync.log"),
        data.join("autostart.off"),
    )?;
    if prompt_login {
        tray.prompt_login();
    }

    let restart = loop {
        set_tray(&tray, credentials.as_ref());
        let end = match &credentials {
            Some(creds) => session(creds, &config, &root, &data, &mut events, &tray)
                .await
                .unwrap_or_else(|err| {
                    log::error!("session: {err}");
                    SessionEnd::Failed
                }),
            None => match events.recv().await {
                None => SessionEnd::Quit,
                Some(TrayEvent::Login(creds)) => {
                    credentials = Some(creds);
                    continue;
                }
                Some(TrayEvent::Browse) => {
                    gui::open_folder(&root);
                    continue;
                }
                Some(TrayEvent::Restart) => SessionEnd::Restart,
                Some(TrayEvent::Quit) => SessionEnd::Quit,
                Some(TrayEvent::Logout) => continue,
            },
        };
        match end {
            SessionEnd::Quit => break false,
            SessionEnd::Restart => break true,
            SessionEnd::Logout | SessionEnd::Failed => credentials = None,
        }
    };

    if restart {
        drop(instance_lock);
        let exe = std::env::current_exe()?;
        let args: Vec<String> = std::env::args().skip(1).collect();
        log::info!("restarting");
        std::process::Command::new(exe).args(args).spawn()?;
    }
    Ok(())
}

fn instance_lock(data: &Path, root: &Path) -> Result<std::fs::File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(data.join("instance.lock"))
        .map_err(|_| {
            format!(
                "another instance is already running on {} — quit it first",
                root.display()
            )
        })
}

fn set_tray(tray: &Tray, credentials: Option<&Credentials>) {
    {
        let mut state = tray.state().lock().unwrap();
        if let Some(creds) = credentials {
            state.url = Some(creds.url.clone());
            state.user = creds.user.clone();
            state.storage = creds.storage.clone();
        }
    }
    tray.set_status(match credentials {
        Some(_) => Status::Syncing,
        None => Status::LoggedOut,
    });
}

async fn session(
    creds: &Credentials,
    config: &AppConfig,
    root: &Path,
    data: &Path,
    events: &mut UnboundedReceiver<TrayEvent>,
    tray: &Tray,
) -> Result<SessionEnd, Box<dyn std::error::Error>> {
    let builder = Sdk::builder(&creds.url).insecure(creds.insecure);
    let sdk = if creds.token.is_empty() {
        builder
            .login(&creds.user, &creds.password, &creds.storage)
            .await?
    } else {
        builder.token(creds.token.clone())?
    };
    session::remember(data, &creds.url, sdk.token().unwrap_or_default(), creds.insecure);
    let sdk = Arc::new(sdk);
    let adapter = Adapter::new(
        sdk.clone(),
        tokio::runtime::Handle::current(),
        root.to_path_buf(),
        data,
    )?;
    let mut upload_status = adapter.upload_status();

    let sync_root_id = register_sync_root(config, creds, root)?;
    let connection = adapter.connect(root)?;
    log::info!("sync root {} connected", root.display());

    let (changes_tx, mut changes) = tokio::sync::mpsc::unbounded_channel();
    watcher::spawn(root, changes_tx)?;
    let (views_tx, mut views) = tokio::sync::mpsc::unbounded_channel();
    viewer::spawn(root, views_tx)?;
    adapter.recover().await?;

    tray.set_status(Status::Ok);
    let refresh_every = Duration::from_secs(config.windows.refresh_secs.max(2));
    let mut refreshed: HashMap<RelPath, Instant> = HashMap::new();
    let mut sweep_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut sweep = tokio::time::interval(Duration::from_secs(30));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let end = loop {
        tokio::select! {
            event = events.recv() => {
                log::info!("session event: {event:?}");
                match event {
                    None | Some(TrayEvent::Quit) => break SessionEnd::Quit,
                    Some(TrayEvent::Logout) => break SessionEnd::Logout,
                    Some(TrayEvent::Restart) => break SessionEnd::Restart,
                    Some(TrayEvent::Browse) => gui::open_folder(root),
                    Some(TrayEvent::Login(_)) => {}
                }
            },
            Some(path) = changes.recv() => {
                adapter.on_change(&path).await;
            }
            Some((dir, newly)) = views.recv() => {
                let due = newly
                    || refreshed
                        .get(&dir)
                        .is_none_or(|at| at.elapsed() >= refresh_every);
                if due {
                    refreshed.insert(dir.clone(), Instant::now());
                    let adapter = adapter.clone();
                    tokio::spawn(async move {
                        if let Err(err) = adapter.refresh(&dir).await {
                            log::warn!("refresh {dir}: {err}");
                        }
                    });
                }
            }
            _ = upload_status.changed() => {
                tray.set_status(match *upload_status.borrow() {
                    UploadStatus::Idle => Status::Ok,
                    UploadStatus::Busy => Status::Syncing,
                    UploadStatus::Error => Status::Error,
                });
            }
            _ = sweep.tick() => {
                if sweep_task.as_ref().is_none_or(|task| task.is_finished()) {
                    let adapter = adapter.clone();
                    sweep_task = Some(tokio::spawn(async move {
                        if let Err(err) = adapter.recover().await {
                            log::error!("sweep: {err}");
                        }
                    }));
                }
            }
        }
    };

    log::info!("disconnecting");
    adapter.flush(Duration::from_secs(30)).await;
    if matches!(end, SessionEnd::Logout) {
        if let Err(err) = adapter.vacuum() {
            log::warn!("vacuum on logout: {err}");
        }
        connection.disconnect();
        match shell::unregister(&sync_root_id) {
            Ok(()) => log::info!("sync root unregistered"),
            Err(err) => log::warn!("unregister on logout: {err}"),
        }
        session::forget(data);
        let _ = sdk.logout().await;
    } else {
        connection.disconnect();
    }
    Ok(end)
}

fn register_sync_root(
    config: &AppConfig,
    creds: &Credentials,
    root: &Path,
) -> std::io::Result<String> {
    let account = match creds.user.is_empty() {
        true => host_of(&creds.url).to_string(),
        false => format!("{}@{}/{}", creds.user, host_of(&creds.url), creds.storage),
    };
    let id = shell::sync_root_id(&config.windows.provider_name, &account, root)?;
    shell::vacuum(&config.windows.provider_name, &id);
    shell::register(
        root,
        &shell::Registration {
            id: id.clone(),
            display_name: config.windows.provider_name.clone(),
            icon: config
                .windows
                .icon
                .clone()
                .unwrap_or_else(shell::default_icon),
            allow_pinning: config.windows.allow_pinning,
            provider_id: wire::PROVIDER_ID,
        },
    )?;
    Ok(id)
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}
