use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

mod args;
mod log;

use fsync_core::config as session;
use fsync_core::scheduler::UploadStatus;
use fsync_core::sdk::Sdk;
use fsync_linux::adapter::Adapter;
use fsync_linux::wire::MountFs;
use fsync_linux::gui::{Credentials, Status, Tray, TrayEvent};
use fuser::{Config, MountOption};
use tokio::sync::mpsc::UnboundedReceiver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args::Setup {
        mount,
        data,
        prefill,
        mut credentials,
        stored,
        prompt_login,
    } = args::init()?;
    log::init(&data)?;
    let _instance_lock = instance_lock(&data)?;

    let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    if prompt_login {
        let _ = events_tx.send(TrayEvent::Login);
    }
    let tray = Tray::spawn(events_tx, data.clone(), mount.clone())
        .await
        .map_err(|err| format!("could not start the tray: {err}"))?;

    let mut last_status = Status::LoggedOut;
    let mut launching = credentials.is_some() && !stored;
    'app: loop {
        if let Some(creds) = credentials.as_ref() {
            tray.set(Status::Syncing, true).await;
            match run_session(creds, &mount, &data, &mut events, &tray).await {
                Ok(SessionEnd::Quit) => break 'app,
                Ok(SessionEnd::Logout) => {
                    credentials = None;
                    last_status = Status::LoggedOut;
                }
                Ok(SessionEnd::Restart) => {
                    last_status = Status::Syncing;
                }
                Err(err) if launching => {
                    log::error!("session: {err}");
                    tray.shutdown().await;
                    return Err(err);
                }
                Err(err) => {
                    log::error!("session: {err}");
                    credentials = None;
                    last_status = Status::Error;
                }
            }
            launching = false;
            continue;
        }
        tray.set(last_status, false).await;
        tokio::select! {
            event = events.recv() => match event {
                None | Some(TrayEvent::Quit) => break 'app,
                Some(TrayEvent::Login) => {
                    if let Some(creds) = tray.login(prefill.clone()).await {
                        credentials = Some(creds);
                        last_status = Status::Syncing;
                    }
                }
                Some(TrayEvent::Logout | TrayEvent::Restart) => {}
            },
            _ = tokio::signal::ctrl_c() => break 'app,
        }
    }
    tray.shutdown().await;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum SessionEnd {
    Logout,
    Restart,
    Quit,
}

async fn run_session(
    creds: &Credentials,
    mount: &Path,
    data: &Path,
    events: &mut UnboundedReceiver<TrayEvent>,
    tray: &Tray,
) -> Result<SessionEnd, Box<dyn std::error::Error>> {
    prepare_mount(mount)?;
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
    let adapter = Arc::new(Adapter::new(
        sdk.clone(),
        tokio::runtime::Handle::current(),
        data,
    )?);
    let mut mount_config = Config::default();
    mount_config.mount_options = vec![
        MountOption::FSName("filestash".to_string()),
        MountOption::DefaultPermissions,
    ];
    let filesystem = MountFs::new(adapter.clone());
    let session = fuser::spawn_mount2(filesystem, mount, &mount_config)?;
    let mut upload_status = adapter.upload_status();
    let mut unmounted = false;

    log::info!("mounted {}", mount.display());
    tray.set(Status::Ok, true).await;
    let end = loop {
        tokio::select! {
            event = events.recv() => match event {
                None | Some(TrayEvent::Quit) => break SessionEnd::Quit,
                Some(TrayEvent::Logout) => break SessionEnd::Logout,
                Some(TrayEvent::Restart) => break SessionEnd::Restart,
                Some(TrayEvent::Login) => {}
            },
            _ = tokio::signal::ctrl_c() => break SessionEnd::Quit,
            _ = upload_status.changed() => {
                let status = match *upload_status.borrow() {
                    UploadStatus::Idle => Status::Ok,
                    UploadStatus::Busy => Status::Syncing,
                    UploadStatus::Error => Status::Error,
                };
                tray.set(status, true).await;
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                if session.guard.is_finished() {
                    log::info!("unmounted externally, ending session");
                    unmounted = true;
                    break SessionEnd::Logout;
                }
            }
        }
    };

    log::info!("unmounting {}", mount.display());
    if unmounted {
        let _ = session.join();
    } else {
        session.umount_and_join()?;
    }
    adapter.flush(Duration::from_secs(30)).await;
    if matches!(end, SessionEnd::Logout) {
        adapter.vacuum()?;
        session::forget(data);
        let _ = sdk.logout().await;
    }
    Ok(end)
}

fn instance_lock(data: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("fsync.lock"))
        .map_err(|err| format!("fsync.lock: {err}"))?;
    match unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } {
        0 => Ok(file),
        _ => Err(format!(
            "another instance is already running on {} — quit it first",
            data.display()
        )),
    }
}

fn prepare_mount(mount: &Path) -> std::io::Result<()> {
    if let Err(err) = std::fs::symlink_metadata(mount) {
        if err.raw_os_error() == Some(libc::ENOTCONN) {
            log::warn!("stale mount at {}, detaching", mount.display());
            let _ = detach_mount(mount);
        }
    }
    std::fs::create_dir_all(mount)
}

fn detach_mount(mount: &Path) -> std::io::Result<std::process::ExitStatus> {
    std::process::Command::new("fusermount3")
        .arg("-uz")
        .arg(mount)
        .status()
}
