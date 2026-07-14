#![allow(clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fdrive_core::engine::{io_err, Engine, Observation};
use fdrive_core::path::RelPath;
use fdrive_core::port::LocalTree;
use fdrive_core::sdk::{self, FileInfo, FileType, Sdk};
use tokio::runtime::Runtime;

const META_TTL: Duration = Duration::from_secs(5);

pub struct MacTree {
    cache_dir: PathBuf,
    ledger: PathBuf,
    meta: Mutex<HashMap<RelPath, (Instant, Vec<FileInfo>)>>,
}

impl MacTree {
    fn invalidate(&self, dir: &RelPath) {
        self.meta.lock().unwrap().remove(dir);
    }

    fn drop(&self, dir: &RelPath, name: &str) {
        if let Some((_, listing)) = self.meta.lock().unwrap().get_mut(dir) {
            listing.retain(|e| e.name != name);
        }
    }
}

impl LocalTree for MacTree {
    fn backing(&self, path: &RelPath) -> PathBuf {
        self.cache_dir.join(path.as_str())
    }

    fn relocate(&self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        let to_backing = self.backing(to);
        ensure_parent(&to_backing)?;
        fs::rename(self.backing(from), to_backing)
    }

    fn settled(&self, target: &RelPath, _mtime: Option<SystemTime>) {
        self.invalidate(&target.parent_or_root());
    }

    fn ledger(&self) -> PathBuf {
        self.ledger.clone()
    }
}

pub struct Handle {
    rt: Runtime,
    engine: Arc<Engine<MacTree>>,
}

impl Handle {
    fn backing(&self, path: &RelPath) -> PathBuf {
        self.engine.tree().backing(path)
    }

    fn invalidate(&self, dir: &RelPath) {
        self.engine.tree().invalidate(dir);
    }

    fn ls(&self, dir: &RelPath) -> io::Result<Vec<FileInfo>> {
        let listing = match self.cached_listing(dir) {
            Some(listing) => listing,
            None => match self.rt.block_on(self.engine.sdk().ls(&dir.as_dir())) {
                Ok(fetched) => {
                    self.engine.listed(dir, &fetched);
                    self.engine
                        .tree()
                        .meta
                        .lock()
                        .unwrap()
                        .insert(dir.clone(), (Instant::now(), fetched.clone()));
                    fetched
                }
                Err(err @ (sdk::Error::NotFound | sdk::Error::PermissionDenied)) => {
                    return Err(io_err(err))
                }
                Err(err) => {
                    let meta = self.engine.tree().meta.lock().unwrap();
                    match meta.get(dir) {
                        Some((_, listing)) => {
                            log::debug!("ls {dir} unreachable, serving stale: {err}");
                            listing.clone()
                        }
                        None => return Err(io_err(err)),
                    }
                }
            },
        };
        Ok(self.engine.overlay(dir, listing))
    }

    fn cached_listing(&self, dir: &RelPath) -> Option<Vec<FileInfo>> {
        let meta = self.engine.tree().meta.lock().unwrap();
        let (at, listing) = meta.get(dir)?;
        (at.elapsed() < META_TTL).then(|| listing.clone())
    }

    fn attr(&self, path: &RelPath) -> io::Result<Option<(bool, u64, SystemTime)>> {
        if path.is_root() {
            return Ok(Some((true, 0, SystemTime::UNIX_EPOCH)));
        }
        if let Some(md) = self.engine.dirty_metadata(path) {
            return Ok(Some((
                false,
                md.len(),
                md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            )));
        }
        let listing = match self.ls(&path.parent_or_root()) {
            Ok(listing) => listing,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        Ok(listing.iter().find(|e| e.name == path.name()).map(|e| {
            (
                e.kind == FileType::Directory,
                e.size.unwrap_or(0),
                e.mtime.unwrap_or(SystemTime::UNIX_EPOCH),
            )
        }))
    }

    fn hydrate(&self, path: &RelPath) -> io::Result<()> {
        let mut current = None;
        if let Ok(listing) = self.ls(&path.parent_or_root()) {
            if let Some(entry) = listing.iter().find(|e| e.name == path.name()) {
                let observation = Observation::of(entry);
                if self.engine.content_current(path, observation) {
                    return Ok(());
                }
                current = Some(observation);
            }
        }
        self.rt.block_on(self.engine.hydrate(path, current))
    }

    fn read(&self, path: &RelPath, offset: u64, size: usize) -> io::Result<Vec<u8>> {
        self.hydrate(path)?;
        let mut file = fs::File::open(self.backing(path))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::with_capacity(size);
        file.take(size as u64).read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn write(&self, path: &RelPath, offset: u64, data: &[u8]) -> io::Result<usize> {
        // the C shim has no open hook, so the backing file must be
        // hydrated before the first write lands at an offset
        self.hydrate(path)?;
        let file_path = self.backing(path);
        ensure_parent(&file_path)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&file_path)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        self.engine.modified(path);
        Ok(data.len())
    }

    fn truncate(&self, path: &RelPath, size: u64) -> io::Result<()> {
        if size > 0 {
            self.hydrate(path)?;
        } else {
            self.rt.block_on(self.engine.overwriting(path));
        }
        let file_path = self.backing(path);
        ensure_parent(&file_path)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&file_path)?;
        file.set_len(size)?;
        self.engine.modified(path);
        Ok(())
    }

    fn create(&self, path: &RelPath) -> io::Result<()> {
        let file_path = self.backing(path);
        ensure_parent(&file_path)?;
        fs::File::create(&file_path)?;
        self.engine.created(path);
        self.invalidate(&path.parent_or_root());
        Ok(())
    }

    fn mkdir(&self, path: &RelPath) -> io::Result<()> {
        self.rt
            .block_on(self.engine.sdk().mkdir(&path.as_dir()))
            .map_err(io_err)?;
        self.invalidate(&path.parent_or_root());
        Ok(())
    }

    fn delete(&self, path: &RelPath, is_dir: bool) -> io::Result<()> {
        self.rt.block_on(self.engine.delete(path, is_dir))?;
        remove_path(&self.backing(path))?;
        self.invalidate(path);
        self.engine.tree().drop(&path.parent_or_root(), path.name());
        Ok(())
    }

    fn rmdir(&self, path: &RelPath) -> io::Result<()> {
        match self.ls(path) {
            Ok(listing) if listing.is_empty() => self.delete(path, true),
            Ok(_) => Err(io::Error::from_raw_os_error(libc::ENOTEMPTY)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => self.delete(path, true),
            Err(err) => Err(err),
        }
    }

    fn rename(&self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        let is_dir = matches!(self.attr(from)?, Some((true, ..)));
        self.rt.block_on(self.engine.rename(from, to, is_dir))?;
        let from_backing = self.backing(from);
        if from_backing.exists() {
            let to_backing = self.backing(to);
            ensure_parent(&to_backing)?;
            remove_path(&to_backing)?;
            fs::rename(&from_backing, &to_backing)?;
        }
        self.invalidate(&from.parent_or_root());
        self.invalidate(&to.parent_or_root());
        Ok(())
    }
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent),
        None => Ok(()),
    }
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
    .or_else(|err| match err.kind() {
        io::ErrorKind::NotFound => Ok(()),
        _ => Err(err),
    })
}

fn cstr(ptr: *const c_char) -> String {
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn rel(ptr: *const c_char) -> RelPath {
    RelPath::new(&cstr(ptr))
}

fn mtime_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn errno(err: &io::Error) -> c_int {
    if let Some(code) = err.raw_os_error() {
        return -code;
    }
    -match err.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        _ => libc::EIO,
    }
}

fn done(r: io::Result<()>) -> c_int {
    match r {
        Ok(()) => 0,
        Err(err) => errno(&err),
    }
}

fn data_dir() -> PathBuf {
    match std::env::var("FILESTASH_DATA") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/Application Support/Filestash"),
    }
}

fn init_log() {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,fdrive_mac=debug,fdrive_core=debug"),
    )
    .write_style(env_logger::WriteStyle::Never)
    .format(|buf, record| {
        let message = record.args().to_string();
        let origin = match record.line() {
            Some(line) => format!("{}:{line}", record.target()),
            None => record.target().to_string(),
        };
        writeln!(
            buf,
            "time={} level={} origin={origin} message={message:?}",
            buf.timestamp_seconds(),
            record.level(),
        )
    })
    .try_init();
}

#[no_mangle]
pub unsafe extern "C" fn fsx_connect(
    url: *const c_char,
    token: *const c_char,
    insecure: c_int,
) -> *mut Handle {
    init_log();
    let (url, token) = (cstr(url), cstr(token));
    let Ok(rt) = Runtime::new() else {
        return std::ptr::null_mut();
    };
    let Ok(sdk) = Sdk::builder(&url).insecure(insecure != 0).token(token) else {
        return std::ptr::null_mut();
    };
    let data = data_dir();
    let cache_dir = data.join("cache");
    if fs::create_dir_all(&cache_dir).is_err() {
        return std::ptr::null_mut();
    }
    let tree = MacTree {
        cache_dir,
        ledger: data.join("fdrive.db"),
        meta: Mutex::new(HashMap::new()),
    };
    let engine = Engine::spawn(Arc::new(sdk), rt.handle().clone(), tree);
    if engine.prune(&engine.tree().cache_dir).is_err() {
        return std::ptr::null_mut();
    }
    engine.recover();
    Box::into_raw(Box::new(Handle { rt, engine }))
}

#[no_mangle]
pub unsafe extern "C" fn fsx_destroy(h: *mut Handle) {
    let h = unsafe { &*h };
    h.rt.block_on(h.engine.flush(Duration::from_secs(30)));
}

#[no_mangle]
pub unsafe extern "C" fn fsx_getattr(
    h: *mut Handle,
    path: *const c_char,
    size_out: *mut u64,
    is_dir_out: *mut c_int,
    mtime_out: *mut i64,
) -> c_int {
    let h = unsafe { &*h };
    match h.attr(&rel(path)) {
        Ok(Some((is_dir, size, mtime))) => {
            unsafe {
                *size_out = size;
                *is_dir_out = i32::from(is_dir);
                *mtime_out = mtime_secs(mtime);
            }
            0
        }
        Ok(None) => -libc::ENOENT,
        Err(err) => errno(&err),
    }
}

pub type FillCb =
    extern "C" fn(ctx: *mut c_void, name: *const c_char, is_dir: c_int, size: u64, mtime: i64);

#[no_mangle]
pub unsafe extern "C" fn fsx_readdir(
    h: *mut Handle,
    path: *const c_char,
    fill: FillCb,
    ctx: *mut c_void,
) -> c_int {
    let h = unsafe { &*h };
    match h.ls(&rel(path)) {
        Ok(entries) => {
            for e in entries {
                let Ok(name) = CString::new(e.name) else {
                    continue;
                };
                let is_dir = i32::from(e.kind == FileType::Directory);
                let mtime = e.mtime.map(mtime_secs).unwrap_or(0);
                fill(ctx, name.as_ptr(), is_dir, e.size.unwrap_or(0), mtime);
            }
            0
        }
        Err(err) => errno(&err),
    }
}

#[no_mangle]
pub unsafe extern "C" fn fsx_read(
    h: *mut Handle,
    path: *const c_char,
    buf: *mut c_char,
    size: usize,
    offset: i64,
) -> isize {
    let h = unsafe { &*h };
    match h.read(&rel(path), offset.max(0) as u64, size) {
        Ok(data) => {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len()) };
            data.len() as isize
        }
        Err(err) => errno(&err) as isize,
    }
}

#[no_mangle]
pub unsafe extern "C" fn fsx_create(h: *mut Handle, path: *const c_char) -> c_int {
    let h = unsafe { &*h };
    done(h.create(&rel(path)))
}

#[no_mangle]
pub unsafe extern "C" fn fsx_write(
    h: *mut Handle,
    path: *const c_char,
    buf: *const c_char,
    size: usize,
    offset: i64,
) -> isize {
    let h = unsafe { &*h };
    let src = unsafe { std::slice::from_raw_parts(buf as *const u8, size) };
    match h.write(&rel(path), offset.max(0) as u64, src) {
        Ok(written) => written as isize,
        Err(err) => errno(&err) as isize,
    }
}

#[no_mangle]
pub unsafe extern "C" fn fsx_truncate(h: *mut Handle, path: *const c_char, size: i64) -> c_int {
    let h = unsafe { &*h };
    done(h.truncate(&rel(path), size.max(0) as u64))
}

#[no_mangle]
pub unsafe extern "C" fn fsx_release(h: *mut Handle, path: *const c_char) -> c_int {
    let h = unsafe { &*h };
    h.engine.released(&rel(path));
    0
}

#[no_mangle]
pub unsafe extern "C" fn fsx_mkdir(h: *mut Handle, path: *const c_char) -> c_int {
    let h = unsafe { &*h };
    done(h.mkdir(&rel(path)))
}

#[no_mangle]
pub unsafe extern "C" fn fsx_rm(h: *mut Handle, path: *const c_char, is_dir: c_int) -> c_int {
    let h = unsafe { &*h };
    let path = rel(path);
    done(if is_dir != 0 {
        h.rmdir(&path)
    } else {
        h.delete(&path, false)
    })
}

#[no_mangle]
pub unsafe extern "C" fn fsx_rename(
    h: *mut Handle,
    from: *const c_char,
    to: *const c_char,
) -> c_int {
    let h = unsafe { &*h };
    done(h.rename(&rel(from), &rel(to)))
}
