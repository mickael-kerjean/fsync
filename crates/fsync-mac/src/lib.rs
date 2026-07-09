#![allow(clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use fsync_core::byte_stream;
use fsync_core::sdk::{FileInfo, FileType, Sdk};
use futures_util::TryStreamExt;
use tokio::runtime::Runtime;

pub struct Handle {
    rt: Runtime,
    sdk: Sdk,
    cache: Mutex<HashMap<String, (bool, u64, i64)>>,
    writers: Mutex<HashMap<String, Vec<u8>>>,
}

fn cstr(ptr: *const c_char) -> String {
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

fn mtime_secs(t: Option<std::time::SystemTime>) -> i64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn dir_for_ls(path: &str) -> String {
    if path == "/" {
        "/".into()
    } else {
        format!("{}/", path.trim_end_matches('/'))
    }
}

fn parent_of(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => format!("{}/", &p[..i]),
    }
}

fn join_child(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

impl Handle {
    fn list_and_cache(&self, dir: &str) -> Result<Vec<FileInfo>, ()> {
        let entries = self.rt.block_on(self.sdk.ls(&dir_for_ls(dir))).map_err(|_| ())?;
        let mut cache = self.cache.lock().unwrap();
        for e in &entries {
            cache.insert(
                join_child(dir, &e.name),
                (e.kind == FileType::Directory, e.size.unwrap_or(0), mtime_secs(e.mtime)),
            );
        }
        Ok(entries)
    }

    fn fetch_all(&self, path: &str) -> Result<Vec<u8>, ()> {
        self.rt.block_on(async {
            let mut stream = self.sdk.cat(path).await.map_err(|_| ())?;
            let mut out = Vec::new();
            while let Some(chunk) = stream.try_next().await.map_err(|_| ())? {
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        })
    }

    fn invalidate(&self) {
        self.cache.lock().unwrap().clear();
    }

    fn is_cached_dir(&self, path: &str) -> bool {
        self.cache.lock().unwrap().get(path).is_some_and(|&(d, ..)| d)
    }
}

fn commit<E>(h: &Handle, r: Result<(), E>) -> c_int {
    match r {
        Ok(()) => {
            h.invalidate();
            0
        }
        Err(_) => -libc::EIO,
    }
}

#[no_mangle]
pub unsafe extern "C" fn fsx_connect(url: *const c_char, token: *const c_char, insecure: c_int) -> *mut Handle {
    let (url, token) = (cstr(url), cstr(token));
    let Ok(rt) = Runtime::new() else { return std::ptr::null_mut() };
    let Ok(sdk) = Sdk::builder(&url).insecure(insecure != 0).token(token) else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(Handle {
        rt,
        sdk,
        cache: Mutex::new(HashMap::new()),
        writers: Mutex::new(HashMap::new()),
    }))
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
    let path = cstr(path);
    let set = |d: bool, s: u64, m: i64| unsafe {
        *size_out = s;
        *is_dir_out = i32::from(d);
        *mtime_out = m;
    };
    if let Some(buf) = h.writers.lock().unwrap().get(&path) {
        set(false, buf.len() as u64, 0);
        return 0;
    }
    if path == "/" {
        set(true, 0, 0);
        return 0;
    }
    if let Some(&(d, s, m)) = h.cache.lock().unwrap().get(&path) {
        set(d, s, m);
        return 0;
    }
    if h.list_and_cache(&parent_of(&path)).is_ok() {
        if let Some(&(d, s, m)) = h.cache.lock().unwrap().get(&path) {
            set(d, s, m);
            return 0;
        }
    }
    -libc::ENOENT
}

pub type FillCb =
    extern "C" fn(ctx: *mut c_void, name: *const c_char, is_dir: c_int, size: u64, mtime: i64);

#[no_mangle]
pub unsafe extern "C" fn fsx_readdir(h: *mut Handle, path: *const c_char, fill: FillCb, ctx: *mut c_void) -> c_int {
    let h = unsafe { &*h };
    match h.list_and_cache(&cstr(path)) {
        Ok(entries) => {
            for e in entries {
                let Ok(name) = CString::new(e.name) else { continue };
                let is_dir = i32::from(e.kind == FileType::Directory);
                fill(ctx, name.as_ptr(), is_dir, e.size.unwrap_or(0), mtime_secs(e.mtime));
            }
            0
        }
        Err(_) => -libc::EIO,
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
    let Ok(data) = h.fetch_all(&cstr(path)) else { return -libc::EIO as isize };
    let off = offset.max(0) as usize;
    if off >= data.len() {
        return 0;
    }
    let end = (off + size).min(data.len());
    unsafe { std::ptr::copy_nonoverlapping(data[off..end].as_ptr(), buf as *mut u8, end - off) };
    (end - off) as isize
}

#[no_mangle]
pub unsafe extern "C" fn fsx_create(h: *mut Handle, path: *const c_char) -> c_int {
    let h = unsafe { &*h };
    let path = cstr(path);
    h.writers.lock().unwrap().insert(path.clone(), Vec::new());
    commit(h, h.rt.block_on(h.sdk.save(&path, byte_stream(Vec::<u8>::new()))))
}

fn writer_buf<'a>(h: &Handle, writers: &'a mut HashMap<String, Vec<u8>>, path: &str) -> &'a mut Vec<u8> {
    writers
        .entry(path.to_string())
        .or_insert_with(|| h.fetch_all(path).unwrap_or_default())
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
    let path = cstr(path);
    let off = offset.max(0) as usize;
    let src = unsafe { std::slice::from_raw_parts(buf as *const u8, size) };
    let mut writers = h.writers.lock().unwrap();
    let b = writer_buf(h, &mut writers, &path);
    if off + size > b.len() {
        b.resize(off + size, 0);
    }
    b[off..off + size].copy_from_slice(src);
    size as isize
}

#[no_mangle]
pub unsafe extern "C" fn fsx_truncate(h: *mut Handle, path: *const c_char, size: i64) -> c_int {
    let h = unsafe { &*h };
    let path = cstr(path);
    let mut writers = h.writers.lock().unwrap();
    writer_buf(h, &mut writers, &path).resize(size.max(0) as usize, 0);
    0
}

#[no_mangle]
pub unsafe extern "C" fn fsx_release(h: *mut Handle, path: *const c_char) -> c_int {
    let h = unsafe { &*h };
    let path = cstr(path);
    let Some(data) = h.writers.lock().unwrap().remove(&path) else {
        return 0;
    };
    commit(h, h.rt.block_on(h.sdk.save(&path, byte_stream(data))))
}

#[no_mangle]
pub unsafe extern "C" fn fsx_mkdir(h: *mut Handle, path: *const c_char) -> c_int {
    let h = unsafe { &*h };
    commit(h, h.rt.block_on(h.sdk.mkdir(&dir_for_ls(&cstr(path)))))
}

#[no_mangle]
pub unsafe extern "C" fn fsx_rm(h: *mut Handle, path: *const c_char, is_dir: c_int) -> c_int {
    let h = unsafe { &*h };
    let path = cstr(path);
    let target = if is_dir != 0 { dir_for_ls(&path) } else { path };
    commit(h, h.rt.block_on(h.sdk.rm(&target)))
}

#[no_mangle]
pub unsafe extern "C" fn fsx_rename(h: *mut Handle, from: *const c_char, to: *const c_char) -> c_int {
    let h = unsafe { &*h };
    let (from, to) = (cstr(from), cstr(to));
    let (f, t) = if h.is_cached_dir(&from) {
        (dir_for_ls(&from), dir_for_ls(&to))
    } else {
        (from, to)
    };
    commit(h, h.rt.block_on(h.sdk.mv(&f, &t)))
}
