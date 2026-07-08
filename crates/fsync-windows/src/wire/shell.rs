use std::io;
use std::path::Path;

use windows::core::{GUID, HSTRING, PWSTR};
use windows::Storage::Provider::{
    StorageProviderHardlinkPolicy, StorageProviderHydrationPolicy,
    StorageProviderHydrationPolicyModifier, StorageProviderInSyncPolicy,
    StorageProviderPopulationPolicy, StorageProviderProtectionMode, StorageProviderSyncRootInfo,
    StorageProviderSyncRootManager,
};
use windows::Storage::StorageFolder;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub struct Registration {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub allow_pinning: bool,
    pub provider_id: GUID,
}

pub fn path_tag(root: &Path) -> io::Result<String> {
    let abs = std::path::absolute(root)?;
    let mut tag: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in abs.to_string_lossy().to_lowercase().as_bytes() {
        tag ^= u64::from(*byte);
        tag = tag.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{:08x}", tag as u32))
}

pub fn sync_root_id(provider: &str, account: &str, root: &Path) -> io::Result<String> {
    let sid = current_user_sid()?;
    let clean = |s: &str| s.replace('!', "_");
    Ok(format!(
        "{}!{sid}!{}.{}",
        clean(provider),
        clean(account),
        path_tag(root)?
    ))
}

pub fn vacuum(provider: &str, keep_id: &str) {
    const SYNC_ROOTS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
    let Ok(key) = windows_registry::LOCAL_MACHINE.open(SYNC_ROOTS) else {
        return;
    };
    let Ok(ids) = key.keys() else { return };
    let prefix = format!("{}!", provider.replace('!', "_"));
    let stale: Vec<String> = ids
        .filter(|id| id.starts_with(&prefix) && id != keep_id)
        .collect();
    for id in stale {
        match unregister(&id) {
            Ok(()) => log::info!("swept stale sync root {id}"),
            Err(err) => log::warn!("sweep {id}: {err}"),
        }
    }
}

pub fn register(root: &Path, reg: &Registration) -> io::Result<()> {
    let abs = std::path::absolute(root)?;
    log::debug!(
        "shell register id={} root={} icon={}",
        reg.id,
        abs.display(),
        reg.icon
    );
    (|| {
        let folder =
            StorageFolder::GetFolderFromPathAsync(&HSTRING::from(abs.as_os_str()))?.join()?;
        let info = StorageProviderSyncRootInfo::new()?;
        info.SetId(&HSTRING::from(&reg.id))?;
        info.SetPath(&folder)?;
        info.SetDisplayNameResource(&HSTRING::from(&reg.display_name))?;
        info.SetIconResource(&HSTRING::from(&reg.icon))?;
        info.SetProviderId(reg.provider_id)?;
        info.SetVersion(&HSTRING::from(env!("CARGO_PKG_VERSION")))?;
        info.SetHydrationPolicy(StorageProviderHydrationPolicy::Full)?;
        info.SetHydrationPolicyModifier(StorageProviderHydrationPolicyModifier::default())?;
        info.SetPopulationPolicy(StorageProviderPopulationPolicy::Full)?;
        info.SetInSyncPolicy(StorageProviderInSyncPolicy::FileLastWriteTime)?;
        info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)?;
        info.SetProtectionMode(StorageProviderProtectionMode::Unknown)?;
        info.SetAllowPinning(reg.allow_pinning)?;
        info.SetShowSiblingsAsGroup(false)?;
        StorageProviderSyncRootManager::Register(&info)
    })()
    .map_err(win_err)
}

pub fn unregister(id: &str) -> io::Result<()> {
    StorageProviderSyncRootManager::Unregister(&HSTRING::from(id)).map_err(win_err)
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Filestash";

pub fn autostart_enabled() -> bool {
    windows_registry::CURRENT_USER
        .open(RUN_KEY)
        .and_then(|key| key.get_string(RUN_VALUE))
        .is_ok()
}

pub fn ensure_autostart(opt_out: &Path) {
    if !opt_out.exists() {
        if let Err(err) = set_autostart(true) {
            log::warn!("autostart: {err}");
        }
    }
}

pub fn set_autostart(enabled: bool) -> io::Result<()> {
    let key = windows_registry::CURRENT_USER
        .create(RUN_KEY)
        .map_err(|err| io::Error::other(format!("{err}")))?;
    if enabled {
        let exe = std::env::current_exe()?;
        key.set_string(RUN_VALUE, format!("\"{}\"", exe.display()))
            .map_err(|err| io::Error::other(format!("{err}")))?;
    } else if autostart_enabled() {
        key.remove_value(RUN_VALUE)
            .map_err(|err| io::Error::other(format!("{err}")))?;
    }
    Ok(())
}

pub fn default_icon() -> String {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    format!("{system_root}\\system32\\imageres.dll,-3")
}

fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(win_err)?;
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buf = vec![0u8; needed as usize];
        let got = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        got.map_err(win_err)?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut wide = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut wide).map_err(win_err)?;
        let sid = wide.to_string().map_err(io::Error::other);
        let _ = LocalFree(Some(HLOCAL(wide.as_ptr() as _)));
        sid
    }
}

fn win_err(err: windows::core::Error) -> io::Error {
    io::Error::other(format!("{err}"))
}
