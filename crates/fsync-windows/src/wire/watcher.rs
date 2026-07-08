use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use fsync_core::path::RelPath;
use tokio::sync::mpsc::UnboundedSender;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesW, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

pub fn spawn(root: &Path, changes: UnboundedSender<RelPath>) -> std::io::Result<()> {
    let root_w: Vec<u16> = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(root_w.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|err| std::io::Error::other(format!("watch {}: {err}", root.display())))?;

    let raw = handle.0 as isize;
    std::thread::Builder::new()
        .name("fsync-watcher".into())
        .spawn(move || watch_loop(HANDLE(raw as *mut c_void), changes))?;
    Ok(())
}

fn watch_loop(handle: HANDLE, changes: UnboundedSender<RelPath>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut returned = 0u32;
        let ok = unsafe {
            ReadDirectoryChangesW(
                handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                true,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES,
                Some(&mut returned),
                None,
                None,
            )
        };
        if let Err(err) = ok {
            log::error!("ReadDirectoryChangesW: {err}; local change detection stopped");
            return;
        }
        if returned == 0 {
            log::warn!("watch buffer overflow: some local changes may be missed");
            continue;
        }
        let mut offset = 0usize;
        loop {
            let info = unsafe { &*(buf.as_ptr().add(offset) as *const FILE_NOTIFY_INFORMATION) };
            let name = unsafe {
                std::slice::from_raw_parts(
                    info.FileName.as_ptr(),
                    (info.FileNameLength / 2) as usize,
                )
            };
            if info.Action != FILE_ACTION_RENAMED_OLD_NAME {
                if let Ok(name) = String::from_utf16(name) {
                    if changes
                        .send(RelPath::new(&name.replace('\\', "/")))
                        .is_err()
                    {
                        return;
                    }
                }
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
        }
    }
}
