use std::collections::HashMap;
use std::fs;
use std::io::{self};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use fdrive_core::engine::{io_err, Engine, Observation};
use fdrive_core::path::RelPath;
use fdrive_core::port::LocalTree;
use fdrive_core::scheduler::UploadStatus;
use fdrive_core::sdk::{self, FileInfo, FileType, Sdk};
use tokio::sync::watch;

use crate::xattr::XattrDb;

struct Handle {
    path: RelPath,
    file: Option<Arc<fs::File>>,
    writable: bool,
}

const META_TTL: Duration = Duration::from_secs(5);
const PIN_XATTR: &str = "user.fdrive.pin";

pub struct CacheTree {
    cache_dir: PathBuf,
    ledger: PathBuf,
    meta: Mutex<HashMap<RelPath, (Instant, Vec<FileInfo>)>>,
}

impl CacheTree {
    fn invalidate(&self, dir: &RelPath) {
        self.meta.lock().unwrap().remove(dir);
    }

    fn drop(&self, dir: &RelPath, name: &str) {
        if let Some((_, listing)) = self.meta.lock().unwrap().get_mut(dir) {
            listing.retain(|e| e.name != name);
        }
    }
}

impl LocalTree for CacheTree {
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

pub struct Adapter {
    engine: Arc<Engine<CacheTree>>,
    xattrs: XattrDb,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
}

impl Adapter {
    pub fn new(sdk: Arc<Sdk>, rt: tokio::runtime::Handle, data_dir: &Path) -> io::Result<Self> {
        let cache_dir = data_dir.join("cache");
        fs::create_dir_all(&cache_dir)?;
        let tree = CacheTree {
            cache_dir,
            ledger: data_dir.join("fdrive.db"),
            meta: Mutex::new(HashMap::new()),
        };
        let adapter = Self {
            engine: Engine::spawn(sdk, rt, tree),
            xattrs: XattrDb::open(data_dir.join("xattr.json")),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        };
        adapter.prune()?;
        adapter.engine.recover();
        Ok(adapter)
    }

    pub async fn flush(&self, timeout: Duration) {
        self.engine.flush(timeout).await;
    }

    pub fn upload_status(&self) -> watch::Receiver<UploadStatus> {
        self.engine.upload_status()
    }

    pub fn rt(&self) -> &tokio::runtime::Handle {
        self.engine.rt()
    }

    pub fn xattrs(&self) -> &XattrDb {
        &self.xattrs
    }

    pub fn xattr_set(
        &self,
        path: &RelPath,
        name: &str,
        value: &[u8],
        flags: i32,
    ) -> Result<(), fuser::Errno> {
        if name == PIN_XATTR {
            match value {
                b"always" => self.engine.pin(path),
                b"auto" => self.engine.unpin(path),
                _ => return Err(fuser::Errno::EINVAL),
            }
            return Ok(());
        }
        self.xattrs.set(path, name, value, flags)
    }

    pub fn xattr_get(&self, path: &RelPath, name: &str) -> Option<Vec<u8>> {
        if name == PIN_XATTR {
            return self.engine.pinned(path).then(|| b"always".to_vec());
        }
        self.xattrs.get(path, name)
    }

    pub fn xattr_remove(&self, path: &RelPath, name: &str) -> Result<(), fuser::Errno> {
        if name == PIN_XATTR {
            self.engine.unpin(path);
            return Ok(());
        }
        self.xattrs.remove(path, name)
    }

    fn prune(&self) -> io::Result<()> {
        self.engine.prune(&self.engine.tree().cache_dir)
    }

    fn backing(&self, path: &RelPath) -> PathBuf {
        self.engine.tree().backing(path)
    }

    fn invalidate(&self, dir: &RelPath) {
        self.engine.tree().invalidate(dir);
    }

    pub fn ls(&self, dir: &RelPath) -> io::Result<Vec<FileInfo>> {
        let listing = match self.cached_listing(dir) {
            Some(listing) => listing,
            None => match self
                .engine
                .rt()
                .block_on(self.engine.sdk().ls(&dir.as_dir()))
            {
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

    fn entry(&self, path: &RelPath) -> io::Result<Option<FileInfo>> {
        let parent = path.parent_or_root();
        match self.ls(&parent) {
            Ok(listing) => Ok(listing.iter().find(|e| e.name == path.name()).cloned()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn attr(&self, path: &RelPath) -> io::Result<Option<(bool, u64, SystemTime)>> {
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
        Ok(self.entry(path)?.map(|e| {
            (
                e.kind == FileType::Directory,
                e.size.unwrap_or(0),
                e.mtime.unwrap_or(SystemTime::UNIX_EPOCH),
            )
        }))
    }

    pub fn hydrate(&self, path: &RelPath) -> io::Result<()> {
        let current = self.remote(path);
        if current.is_some_and(|current| self.engine.content_current(path, current)) {
            return Ok(());
        }
        self.engine
            .rt()
            .block_on(self.engine.hydrate(path, current))
    }

    pub fn hydrate_start(&self, path: &RelPath) -> io::Result<()> {
        let current = self.remote(path);
        if current.is_some_and(|current| self.engine.content_current(path, current)) {
            return Ok(());
        }
        self.engine
            .rt()
            .block_on(self.engine.hydrate_start(path, current))
    }

    fn remote(&self, path: &RelPath) -> Option<Observation> {
        self.entry(path).ok().flatten().map(|e| Observation::of(&e))
    }

    pub fn opened(&self, path: &RelPath, writable: bool) -> u64 {
        if writable {
            self.engine.write_opened(path);
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.backing(path))
            .ok()
            .map(Arc::new);
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(
            fh,
            Handle {
                path: path.clone(),
                file,
                writable,
            },
        );
        fh
    }

    pub fn closed(&self, fh: u64) {
        if let Some(handle) = self.handles.lock().unwrap().remove(&fh) {
            if handle.writable {
                self.engine.write_closed(&handle.path);
            }
            self.engine.released(&handle.path);
        }
    }

    fn handle_file(&self, fh: u64) -> Option<Arc<fs::File>> {
        self.handles.lock().unwrap().get(&fh)?.file.clone()
    }

    pub fn read(&self, fh: u64, path: &RelPath, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        if let Some(download) = self.engine.download(path) {
            return self.engine.rt().block_on(download.read(offset, size));
        }
        let mut buf = vec![0u8; size as usize];
        let filled = match self.handle_file(fh) {
            Some(file) => fill_at(&file, &mut buf, offset)?,
            None => fill_at(&fs::File::open(self.backing(path))?, &mut buf, offset)?,
        };
        buf.truncate(filled);
        Ok(buf)
    }

    pub fn write(&self, fh: u64, path: &RelPath, offset: u64, data: &[u8]) -> io::Result<u32> {
        self.engine.modified(path);
        match self.handle_file(fh) {
            Some(file) => file.write_all_at(data, offset)?,
            None => {
                let file_path = self.backing(path);
                ensure_parent(&file_path)?;
                fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&file_path)?
                    .write_all_at(data, offset)?;
            }
        }
        Ok(data.len() as u32)
    }

    pub fn truncate(&self, path: &RelPath, size: u64) -> io::Result<()> {
        if size > 0 {
            self.hydrate(path)?;
        } else if self.engine.needs_baseline(path) {
            self.engine.rt().block_on(self.engine.overwriting(path));
        }
        let file_path = self.backing(path);
        ensure_parent(&file_path)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&file_path)?;
        self.engine.modified(path);
        file.set_len(size)?;
        Ok(())
    }

    pub fn create(&self, path: &RelPath) -> io::Result<()> {
        let file_path = self.backing(path);
        ensure_parent(&file_path)?;
        self.engine.created(path);
        fs::File::create(&file_path)?;
        Ok(())
    }

    pub fn mkdir(&self, path: &RelPath) -> io::Result<()> {
        self.engine
            .rt()
            .block_on(self.engine.sdk().mkdir(&path.as_dir()))
            .map_err(io_err)?;
        self.invalidate(&path.parent_or_root());
        Ok(())
    }

    pub fn delete(&self, path: &RelPath, is_dir: bool) -> io::Result<()> {
        self.engine
            .rt()
            .block_on(self.engine.delete(path, is_dir))?;
        remove_path(&self.backing(path))?;
        self.xattrs.forget(path);
        self.invalidate(path);
        self.engine.tree().drop(&path.parent_or_root(), path.name());
        Ok(())
    }

    pub fn rmdir(&self, path: &RelPath) -> io::Result<()> {
        match self.ls(path) {
            Ok(listing) if listing.is_empty() => self.delete(path, true),
            Ok(_) => Err(io::Error::from_raw_os_error(libc::ENOTEMPTY)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => self.delete(path, true),
            Err(err) => Err(err),
        }
    }

    pub fn rename(&self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        let is_dir = matches!(self.attr(from)?, Some((true, ..)));
        self.engine
            .rt()
            .block_on(self.engine.rename(from, to, is_dir))?;
        let from_backing = self.backing(from);
        if from_backing.exists() {
            let to_backing = self.backing(to);
            ensure_parent(&to_backing)?;
            remove_path(&to_backing)?;
            fs::rename(&from_backing, &to_backing)?;
        }
        self.xattrs.remap(from, to);
        self.invalidate(&from.parent_or_root());
        self.invalidate(&to.parent_or_root());
        Ok(())
    }

    pub fn vacuum(&self) -> io::Result<()> {
        self.engine.tree().meta.lock().unwrap().clear();
        self.prune()
    }
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent),
        None => Ok(()),
    }
}

fn fill_at(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read_at(&mut buf[filled..], offset + filled as u64)? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_serves_the_stale_listing_when_the_server_is_unreachable() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let data = std::env::temp_dir().join(format!("fdrive-stale-ls-{}", std::process::id()));
        fs::create_dir_all(&data).unwrap();
        let sdk = Sdk::new("http://127.0.0.1:9").unwrap();
        let adapter = Adapter::new(Arc::new(sdk), rt.handle().clone(), &data).unwrap();

        let dir = RelPath::new("d");
        let expired = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .unwrap();
        adapter.engine.tree().meta.lock().unwrap().insert(
            dir.clone(),
            (
                expired,
                vec![FileInfo {
                    name: "a.txt".to_string(),
                    kind: FileType::File,
                    size: Some(1),
                    mtime: None,
                }],
            ),
        );

        let listing = adapter.ls(&dir).unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].name, "a.txt");
        assert!(adapter.ls(&RelPath::new("never-seen")).is_err());
        let _ = fs::remove_dir_all(&data);
    }
}
