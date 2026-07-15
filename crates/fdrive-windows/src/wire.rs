pub mod shell;
pub mod viewer;
pub mod watcher;

use std::ffi::{c_void, OsStr};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fdrive_core::path::RelPath;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, NTSTATUS, STATUS_ACCESS_DENIED, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use windows::Win32::Storage::CloudFilters::{
    CfCloseHandle, CfConnectSyncRoot, CfConvertToPlaceholder, CfCreatePlaceholders,
    CfDehydratePlaceholder, CfDisconnectSyncRoot, CfExecute, CfGetPlaceholderStateFromAttributeTag,
    CfHydratePlaceholder, CfOpenFileWithOplock, CfSetInSyncState, CfSetPinState,
    CfUnregisterSyncRoot, CF_CALLBACK_DELETE_FLAG_IS_DIRECTORY, CF_CALLBACK_DELETE_FLAG_NONE,
    CF_CALLBACK_INFO, CF_CALLBACK_PARAMETERS, CF_CALLBACK_REGISTRATION,
    CF_CALLBACK_RENAME_FLAG_IS_DIRECTORY, CF_CALLBACK_RENAME_FLAG_NONE,
    CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE, CF_CALLBACK_TYPE_FETCH_DATA,
    CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS, CF_CALLBACK_TYPE_NONE, CF_CALLBACK_TYPE_NOTIFY_DELETE,
    CF_CALLBACK_TYPE_NOTIFY_RENAME, CF_CONNECTION_KEY, CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
    CF_CONVERT_FLAG_MARK_IN_SYNC, CF_CREATE_FLAG_NONE, CF_DEHYDRATE_FLAG_NONE, CF_FS_METADATA,
    CF_HYDRATE_FLAG_NONE, CF_IN_SYNC_STATE_IN_SYNC, CF_OPEN_FILE_FLAG_EXCLUSIVE,
    CF_OPEN_FILE_FLAG_WRITE_ACCESS, CF_OPERATION_ACK_DELETE_FLAG_NONE, CF_OPERATION_INFO,
    CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0, CF_OPERATION_PARAMETERS_0_0,
    CF_OPERATION_PARAMETERS_0_4, CF_OPERATION_PARAMETERS_0_6, CF_OPERATION_PARAMETERS_0_7,
    CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
    CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE, CF_OPERATION_TYPE, CF_OPERATION_TYPE_ACK_DELETE,
    CF_OPERATION_TYPE_ACK_RENAME, CF_OPERATION_TYPE_TRANSFER_DATA,
    CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS, CF_PIN_STATE_UNSPECIFIED,
    CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_INFO, CF_PLACEHOLDER_STATE,
    CF_PLACEHOLDER_STATE_IN_SYNC, CF_PLACEHOLDER_STATE_PARTIAL,
    CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK, CF_PLACEHOLDER_STATE_PLACEHOLDER,
    CF_SET_IN_SYNC_FLAG_NONE, CF_SET_PIN_FLAG_NONE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};

pub const PROVIDER_ID: GUID = GUID::from_u128(0x66c2_1b4f_91f5_4b30_9b6a_44f3_15fa_86d1);

pub type SinkFn<'a> = &'a mut dyn FnMut(u64, &[u8]) -> io::Result<()>;
pub type FetchFn = Box<dyn for<'a> Fn(&RelPath, i64, SinkFn<'a>) -> io::Result<u64> + Send + Sync>;
pub type PopulateFn = Box<dyn Fn(&RelPath) -> io::Result<()> + Send + Sync>;
pub type DeleteFn = Box<dyn Fn(&RelPath, bool) -> io::Result<()> + Send + Sync>;
pub type RenameFn = Box<dyn Fn(&RelPath, &RelPath, bool) -> io::Result<()> + Send + Sync>;

pub struct Callbacks {
    pub fetch: FetchFn,
    pub populate: PopulateFn,
    pub delete: DeleteFn,
    pub rename: RenameFn,
}

struct CallbackCtx {
    callbacks: Callbacks,
    root_novol: String,
}

pub struct Connection {
    key: CF_CONNECTION_KEY,
    _ctx: Box<CallbackCtx>,
}

pub fn unregister(root: &Path) -> io::Result<()> {
    let root_w = wide(root.as_os_str());
    unsafe { CfUnregisterSyncRoot(PCWSTR(root_w.as_ptr())) }
        .map_err(|err| io::Error::other(format!("CfUnregisterSyncRoot: {err}")))
}

pub fn connect(root: &Path, callbacks: Callbacks) -> io::Result<Connection> {
    log::debug!("connect root={}", root.display());
    let abs = std::path::absolute(root)?;
    let ctx = Box::new(CallbackCtx {
        callbacks,
        root_novol: strip_volume(&abs.to_string_lossy()).to_string(),
    });
    let table = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(on_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(on_fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(on_notify_rename),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(on_notify_delete),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ];
    let root_w = wide(root.as_os_str());
    let key = unsafe {
        CfConnectSyncRoot(
            PCWSTR(root_w.as_ptr()),
            table.as_ptr(),
            Some(&*ctx as *const CallbackCtx as *const c_void),
            CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
        )
    }
    .map_err(|err| io::Error::other(format!("CfConnectSyncRoot: {err}")))?;
    Ok(Connection { key, _ctx: ctx })
}

impl Connection {
    pub fn disconnect(self) {
        if let Err(err) = unsafe { CfDisconnectSyncRoot(self.key) } {
            log::error!("CfDisconnectSyncRoot: {err}");
        }
    }
}

pub fn create_placeholder(
    root: &Path,
    path: &RelPath,
    size: u64,
    mtime: SystemTime,
) -> io::Result<()> {
    log::debug!("place path={path} size={size}");
    place(root, path, FILE_ATTRIBUTE_NORMAL.0, size, mtime)
}

pub fn create_dir_placeholder(root: &Path, path: &RelPath, mtime: SystemTime) -> io::Result<()> {
    log::debug!("place dir path={path}");
    place(root, path, FILE_ATTRIBUTE_DIRECTORY.0, 0, mtime)
}

fn place(root: &Path, path: &RelPath, attrs: u32, size: u64, mtime: SystemTime) -> io::Result<()> {
    let parent = path.parent_or_root();
    let mut parent_abs = root.to_path_buf();
    if !parent.is_root() {
        parent_abs.extend(parent.as_str().split('/'));
    }
    let parent_w = wide(parent_abs.as_os_str());
    let name_w = wide(OsStr::new(path.name()));
    let identity = path.as_str().as_bytes();
    let time = filetime(mtime);
    let mut info = CF_PLACEHOLDER_CREATE_INFO {
        RelativeFileName: PCWSTR(name_w.as_ptr()),
        FsMetadata: CF_FS_METADATA {
            BasicInfo: FILE_BASIC_INFO {
                CreationTime: time,
                LastAccessTime: time,
                LastWriteTime: time,
                ChangeTime: time,
                FileAttributes: attrs,
            },
            FileSize: size as i64,
        },
        FileIdentity: identity.as_ptr() as *const c_void,
        FileIdentityLength: identity.len() as u32,
        Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC,
        ..Default::default()
    };
    unsafe {
        CfCreatePlaceholders(
            PCWSTR(parent_w.as_ptr()),
            std::slice::from_mut(&mut info),
            CF_CREATE_FLAG_NONE,
            None,
        )
    }
    .map_err(|err| io::Error::other(format!("CfCreatePlaceholders {path}: {err}")))
}

pub fn mark_in_sync_if_unmodified(
    abs: &Path,
    path: &RelPath,
    expected: Option<SystemTime>,
) -> io::Result<()> {
    if let Some(expected) = expected {
        let now = std::fs::symlink_metadata(abs)?.modified().ok();
        if now != Some(expected) {
            return Err(io::Error::other(format!(
                "{path} modified since; leaving it out of sync"
            )));
        }
    }
    mark_in_sync(abs, path)
}

pub fn mark_in_sync(abs: &Path, path: &RelPath) -> io::Result<()> {
    let state = placeholder_state(abs)?;
    let abs_w = wide(abs.as_os_str());
    let flags = if state.placeholder {
        CF_OPEN_FILE_FLAG_WRITE_ACCESS
    } else {
        CF_OPEN_FILE_FLAG_EXCLUSIVE
    };
    let handle = unsafe { CfOpenFileWithOplock(PCWSTR(abs_w.as_ptr()), flags) }
        .map_err(|err| io::Error::other(format!("CfOpenFileWithOplock {path}: {err}")))?;
    let result = if state.placeholder {
        unsafe {
            CfSetInSyncState(
                handle,
                CF_IN_SYNC_STATE_IN_SYNC,
                CF_SET_IN_SYNC_FLAG_NONE,
                None,
            )
        }
        .map_err(|err| io::Error::other(format!("CfSetInSyncState {path}: {err}")))
    } else {
        log::debug!("convert to placeholder path={path}");
        let identity = path.as_str().as_bytes();
        unsafe {
            CfConvertToPlaceholder(
                handle,
                Some(identity.as_ptr() as *const c_void),
                identity.len() as u32,
                CF_CONVERT_FLAG_MARK_IN_SYNC,
                None,
                None,
            )
        }
        .map_err(|err| io::Error::other(format!("CfConvertToPlaceholder {path}: {err}")))
    };
    unsafe { CfCloseHandle(handle) };
    result
}

#[derive(Debug, Clone, Copy)]
pub struct PlaceholderState {
    pub placeholder: bool,
    pub in_sync: bool,
    pub partial: bool,
}

pub fn placeholder_state(abs: &Path) -> io::Result<PlaceholderState> {
    let abs_w = wide(abs.as_os_str());
    let handle = unsafe {
        CreateFileW(
            PCWSTR(abs_w.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|err| io::Error::other(format!("open {}: {err}", abs.display())))?;
    let state = state_of_handle(handle);
    unsafe {
        let _ = CloseHandle(handle);
    }
    state.map_err(|err| io::Error::other(format!("{}: {err}", abs.display())))
}

fn state_of_handle(handle: HANDLE) -> io::Result<PlaceholderState> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(|err| io::Error::other(format!("attributes: {err}")))?;
    let state =
        unsafe { CfGetPlaceholderStateFromAttributeTag(info.FileAttributes, info.ReparseTag) };
    let has = |bit| state & bit != CF_PLACEHOLDER_STATE(0);
    Ok(PlaceholderState {
        placeholder: has(CF_PLACEHOLDER_STATE_PLACEHOLDER),
        in_sync: has(CF_PLACEHOLDER_STATE_IN_SYNC),
        partial: has(CF_PLACEHOLDER_STATE_PARTIAL) || has(CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK),
    })
}

pub fn delete_if_clean(abs: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_DISPOSITION_INFO,
        FILE_READ_ATTRIBUTES, FILE_SHARE_MODE,
    };
    let abs_w = wide(abs.as_os_str());
    let handle = unsafe {
        CreateFileW(
            PCWSTR(abs_w.as_ptr()),
            DELETE.0 | FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|err| match err.code().0 as u32 {
        0x80070002 | 0x80070003 => io::Error::from(io::ErrorKind::NotFound),
        _ => io::Error::other(format!("open for delete {}: {err}", abs.display())),
    })?;
    let result = (|| {
        let state = state_of_handle(handle)?;
        if !state.placeholder || !state.in_sync {
            return Err(io::Error::other(format!(
                "{} is no longer a clean placeholder; not deleting",
                abs.display()
            )));
        }
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        unsafe {
            SetFileInformationByHandle(
                handle,
                FileDispositionInfo,
                &disposition as *const _ as *const c_void,
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        }
        .map_err(|err| io::Error::other(format!("delete {}: {err}", abs.display())))
    })();
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}

pub fn set_pinned(abs: &Path) -> io::Result<()> {
    use windows::Win32::Storage::CloudFilters::CF_PIN_STATE_PINNED;
    let abs_w = wide(abs.as_os_str());
    let handle =
        unsafe { CfOpenFileWithOplock(PCWSTR(abs_w.as_ptr()), CF_OPEN_FILE_FLAG_WRITE_ACCESS) }
            .map_err(|err| io::Error::other(format!("CfOpenFileWithOplock: {err}")))?;
    let result = unsafe { CfSetPinState(handle, CF_PIN_STATE_PINNED, CF_SET_PIN_FLAG_NONE, None) }
        .map_err(|err| io::Error::other(format!("CfSetPinState: {err}")));
    unsafe { CfCloseHandle(handle) };
    result
}

pub fn set_hydration(abs: &Path, wanted: bool) -> io::Result<()> {
    let abs_w = wide(abs.as_os_str());
    let handle =
        unsafe { CfOpenFileWithOplock(PCWSTR(abs_w.as_ptr()), CF_OPEN_FILE_FLAG_WRITE_ACCESS) }
            .map_err(|err| io::Error::other(format!("CfOpenFileWithOplock: {err}")))?;
    let result = if wanted {
        unsafe { CfHydratePlaceholder(handle, 0, -1, CF_HYDRATE_FLAG_NONE, None) }
            .map_err(|err| io::Error::other(format!("CfHydratePlaceholder: {err}")))
    } else {
        unsafe { CfDehydratePlaceholder(handle, 0, -1, CF_DEHYDRATE_FLAG_NONE, None) }
            .map_err(|err| io::Error::other(format!("CfDehydratePlaceholder: {err}")))
            .map(|()| {
                if let Err(err) = unsafe {
                    CfSetPinState(handle, CF_PIN_STATE_UNSPECIFIED, CF_SET_PIN_FLAG_NONE, None)
                } {
                    log::debug!("clear pin state: {err}");
                }
            })
    };
    unsafe { CfCloseHandle(handle) };
    result
}

unsafe extern "system" fn on_fetch_data(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let ctx = &*(info.CallbackContext as *const CallbackCtx);
    let fetch = (*params).Anonymous.FetchData;
    let Some(path) = ctx.rel_path(info.NormalizedPath) else {
        log::error!(
            "fetch callback outside the root: {:?}",
            pcwstr(info.NormalizedPath)
        );
        transfer(info, None, &fetch, STATUS_UNSUCCESSFUL);
        return;
    };
    log::debug!(
        "fetch path={path} offset={} length={}",
        fetch.RequiredFileOffset,
        fetch.RequiredLength
    );
    let mut sink = |offset: u64, bytes: &[u8]| transfer_chunk(info, offset, bytes);
    match catch_unwind(AssertUnwindSafe(|| {
        (ctx.callbacks.fetch)(&path, info.FileSize, &mut sink)
    })) {
        Ok(Ok(size)) => {
            log::info!("hydrated {path} ({size} bytes)");
        }
        Ok(Err(err)) => {
            if err.kind() == io::ErrorKind::NotFound {
                log::warn!("hydrate {path}: gone on the server");
            } else {
                log::error!("hydrate {path}: {err}");
            }
            transfer(info, None, &fetch, STATUS_UNSUCCESSFUL);
        }
        Err(payload) => {
            log::error!("panic fetching {path}: {}", panic_message(&payload));
            transfer(info, None, &fetch, STATUS_UNSUCCESSFUL);
        }
    }
}

unsafe extern "system" fn on_fetch_placeholders(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let ctx = &*(info.CallbackContext as *const CallbackCtx);
    let Some(path) = ctx.rel_path(info.NormalizedPath) else {
        log::error!(
            "fetch-placeholders outside the root: {:?}",
            pcwstr(info.NormalizedPath)
        );
        ack_placeholders(info, STATUS_UNSUCCESSFUL, false);
        return;
    };
    log::debug!("fetch-placeholders dir={path}");
    match catch_unwind(AssertUnwindSafe(|| (ctx.callbacks.populate)(&path))) {
        Ok(Ok(())) => ack_placeholders(info, STATUS_SUCCESS, true),
        Ok(Err(err)) => {
            log::error!("list {path}: {err}");
            ack_placeholders(info, STATUS_UNSUCCESSFUL, false);
        }
        Err(payload) => {
            log::error!("panic listing {path}: {}", panic_message(&payload));
            ack_placeholders(info, STATUS_UNSUCCESSFUL, false);
        }
    }
}

fn guarded(what: &str, f: impl FnOnce() -> io::Result<()>) -> NTSTATUS {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => STATUS_SUCCESS,
        Ok(Err(err)) => {
            log::warn!("{what}: {err}; denying");
            STATUS_ACCESS_DENIED
        }
        Err(payload) => {
            log::error!("panic in {what}: {}", panic_message(&payload));
            STATUS_ACCESS_DENIED
        }
    }
}

fn op_info(info: &CF_CALLBACK_INFO, kind: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
    CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: kind,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        RequestKey: info.RequestKey,
        ..Default::default()
    }
}

unsafe fn ack_placeholders(info: &CF_CALLBACK_INFO, status: NTSTATUS, complete: bool) {
    let op = op_info(info, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    let flags = if complete {
        CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION
    } else {
        CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferPlaceholders: CF_OPERATION_PARAMETERS_0_4 {
                Flags: flags,
                CompletionStatus: status,
                PlaceholderTotalCount: 0,
                PlaceholderArray: std::ptr::null_mut(),
                PlaceholderCount: 0,
                EntriesProcessed: 0,
            },
        },
    };
    if let Err(err) = CfExecute(&op, &mut params) {
        log::error!("CfExecute(TRANSFER_PLACEHOLDERS): {err}");
    }
}

unsafe extern "system" fn on_notify_delete(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let ctx = &*(info.CallbackContext as *const CallbackCtx);
    let delete = (*params).Anonymous.Delete;
    let is_dir =
        delete.Flags & CF_CALLBACK_DELETE_FLAG_IS_DIRECTORY != CF_CALLBACK_DELETE_FLAG_NONE;
    let status = match ctx.rel_path(info.NormalizedPath) {
        Some(path) => guarded(&format!("delete {path}"), || {
            (ctx.callbacks.delete)(&path, is_dir)
        }),
        None => STATUS_SUCCESS,
    };
    ack_delete(info, status);
}

unsafe fn ack_delete(info: &CF_CALLBACK_INFO, status: NTSTATUS) {
    let op = op_info(info, CF_OPERATION_TYPE_ACK_DELETE);
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckDelete: CF_OPERATION_PARAMETERS_0_7 {
                Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
                CompletionStatus: status,
            },
        },
    };
    if let Err(err) = CfExecute(&op, &mut params) {
        log::error!("CfExecute(ACK_DELETE): {err}");
    }
}

unsafe extern "system" fn on_notify_rename(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let info = &*info;
    let ctx = &*(info.CallbackContext as *const CallbackCtx);
    let rename = (*params).Anonymous.Rename;
    let is_dir =
        rename.Flags & CF_CALLBACK_RENAME_FLAG_IS_DIRECTORY != CF_CALLBACK_RENAME_FLAG_NONE;
    let target_in_scope =
        rename.Flags & CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE != CF_CALLBACK_RENAME_FLAG_NONE;
    let from = ctx.rel_path(info.NormalizedPath);
    let status = match (&from, target_in_scope) {
        (Some(from), true) => match ctx.rel_path(rename.TargetPath) {
            Some(to) => guarded(&format!("rename {from}"), || {
                (ctx.callbacks.rename)(from, &to, is_dir)
            }),
            None => STATUS_ACCESS_DENIED,
        },
        (Some(from), false) if is_recycle_bin(rename.TargetPath) => {
            guarded(&format!("recycle {from}"), || {
                (ctx.callbacks.delete)(from, is_dir)
            })
        }
        (Some(_), false) => {
            log::info!("denying move out of the sync root (copy instead hydrates first)");
            STATUS_ACCESS_DENIED
        }
        (None, _) => STATUS_SUCCESS,
    };
    let op = op_info(info, CF_OPERATION_TYPE_ACK_RENAME);
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckRename: CF_OPERATION_PARAMETERS_0_6 {
                Flags: Default::default(),
                CompletionStatus: status,
            },
        },
    };
    if let Err(err) = CfExecute(&op, &mut params) {
        log::error!("CfExecute(ACK_RENAME): {err}");
    }
}

fn transfer_chunk(info: &CF_CALLBACK_INFO, offset: u64, bytes: &[u8]) -> io::Result<()> {
    let op = op_info(info, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_0 {
                Flags: Default::default(),
                CompletionStatus: STATUS_SUCCESS,
                Buffer: bytes.as_ptr() as *const c_void,
                Offset: offset as i64,
                Length: bytes.len() as i64,
            },
        },
    };
    unsafe { CfExecute(&op, &mut params) }.map_err(|err| {
        if err.code().0 == CLOUD_CANCELED {
            io::Error::other(format!("reader gave up: {err}"))
        } else {
            io::Error::other(format!("CfExecute(TRANSFER_DATA): {err}"))
        }
    })
}

unsafe fn transfer(
    info: &CF_CALLBACK_INFO,
    bytes: Option<&[u8]>,
    fetch: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS_0_1,
    status: NTSTATUS,
) {
    let op = op_info(info, CF_OPERATION_TYPE_TRANSFER_DATA);
    let (buffer, offset, length) = match bytes {
        Some(bytes) => (bytes.as_ptr() as *const c_void, 0, bytes.len() as i64),
        None => (
            std::ptr::null(),
            fetch.RequiredFileOffset,
            fetch.RequiredLength,
        ),
    };
    let mut params = CF_OPERATION_PARAMETERS {
        ParamSize: std::mem::size_of::<CF_OPERATION_PARAMETERS>() as u32,
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_0 {
                Flags: Default::default(),
                CompletionStatus: status,
                Buffer: buffer,
                Offset: offset,
                Length: length,
            },
        },
    };
    if let Err(err) = CfExecute(&op, &mut params) {
        if err.code().0 == CLOUD_CANCELED {
            log::debug!("CfExecute(TRANSFER_DATA): reader gave up: {err}");
        } else {
            log::error!("CfExecute(TRANSFER_DATA): {err}");
        }
    }
}

const CLOUD_CANCELED: i32 = 0x8007018Eu32 as i32;

impl CallbackCtx {
    fn rel_path(&self, raw: PCWSTR) -> Option<RelPath> {
        let s = unsafe { pcwstr(raw) }?;
        rel_from_full(&self.root_novol, &s)
    }
}

fn strip_volume(s: &str) -> &str {
    let mut t = s;
    if let Some(rest) = t.strip_prefix(r"\\?\") {
        t = rest;
    }
    if t.len() >= 2 && t.as_bytes()[1] == b':' {
        t = &t[2..];
    }
    t.trim_end_matches('\\')
}

fn rel_from_full(root_novol: &str, s: &str) -> Option<RelPath> {
    let t = strip_volume(s);
    if t.len() < root_novol.len() || !t.is_char_boundary(root_novol.len()) {
        return None;
    }
    let (head, rest) = t.split_at(root_novol.len());
    if !head.eq_ignore_ascii_case(root_novol) {
        return None;
    }
    if rest.is_empty() {
        return Some(RelPath::root());
    }
    let rest = rest.strip_prefix('\\')?;
    Some(RelPath::new(&rest.replace('\\', "/")))
}

unsafe fn pcwstr(raw: PCWSTR) -> Option<String> {
    if raw.0.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *raw.0.add(len) != 0 {
        len += 1;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(
        raw.0, len,
    )))
}

pub fn abs_of(root: &Path, path: &RelPath) -> PathBuf {
    if path.is_root() {
        root.to_path_buf()
    } else {
        let mut abs = root.to_path_buf();
        abs.extend(path.as_str().split('/'));
        abs
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

unsafe fn is_recycle_bin(path: PCWSTR) -> bool {
    match pcwstr(path) {
        Some(s) => s.to_lowercase().contains(r"\$recycle.bin\"),
        None => false,
    }
}

fn filetime(t: SystemTime) -> i64 {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let since_unix = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    ((since_unix.as_secs() + EPOCH_DIFF_SECS) as i64) * 10_000_000
        + (since_unix.subsec_nanos() as i64) / 100
}

#[cfg(test)]
#[path = "wire_test.rs"]
mod tests;
