use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use fsync_core::engine::{io_err, Engine, Observation};
use fsync_core::port::LocalTree;
use fsync_core::path::RelPath;
use fsync_core::scheduler::UploadStatus;
use fsync_core::sdk::{FileInfo, FileType, Sdk};
use tokio::sync::watch;

use crate::xattr::XattrDb;

const META_TTL: Duration = Duration::from_secs(2);

pub struct CacheTree {
    cache_dir: PathBuf,
    ledger: PathBuf,
    meta: Mutex<HashMap<RelPath, (Instant, Vec<FileInfo>)>>,
}

impl CacheTree {
    fn invalidate(&self, dir: &RelPath) {
        self.meta.lock().unwrap().remove(dir);
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
}

impl Adapter {
    pub fn new(sdk: Arc<Sdk>, rt: tokio::runtime::Handle, data_dir: &Path) -> io::Result<Self> {
        let cache_dir = data_dir.join("cache");
        fs::create_dir_all(&cache_dir)?;
        let tree = CacheTree {
            cache_dir,
            ledger: data_dir.join("fsync.json"),
            meta: Mutex::new(HashMap::new()),
        };
        let adapter = Self {
            engine: Engine::spawn(sdk, rt, tree),
            xattrs: XattrDb::open(data_dir.join("xattr.json")),
        };
        adapter.prune()?;
        adapter.engine.recover();
        Ok(adapter)
    }

    pub fn released(&self, path: &RelPath) {
        self.engine.released(path);
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
            None => {
                let fetched = self
                    .engine
                    .rt()
                    .block_on(self.engine.sdk().ls(&dir.as_dir()))
                    .map_err(io_err)?;
                self.engine
                    .tree()
                    .meta
                    .lock()
                    .unwrap()
                    .insert(dir.clone(), (Instant::now(), fetched.clone()));
                fetched
            }
        };
        Ok(self.engine.overlay(dir, listing))
    }

    fn cached_listing(&self, dir: &RelPath) -> Option<Vec<FileInfo>> {
        let meta = self.engine.tree().meta.lock().unwrap();
        let (at, listing) = meta.get(dir)?;
        (at.elapsed() < META_TTL).then(|| listing.clone())
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

    pub fn hydrate(&self, path: &RelPath) -> io::Result<()> {
        if let Ok(listing) = self.ls(&path.parent_or_root()) {
            if let Some(entry) = listing.iter().find(|e| e.name == path.name()) {
                if self.engine.content_current(path, Observation::of(entry)) {
                    return Ok(());
                }
            }
        }
        self.engine.rt().block_on(self.engine.hydrate(path))
    }

    pub fn read(&self, path: &RelPath, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        let mut file = fs::File::open(self.backing(path))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::with_capacity(size as usize);
        file.take(size as u64).read_to_end(&mut buf)?;
        Ok(buf)
    }

    pub fn write(&self, path: &RelPath, offset: u64, data: &[u8]) -> io::Result<u32> {
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
        Ok(data.len() as u32)
    }

    pub fn truncate(&self, path: &RelPath, size: u64) -> io::Result<()> {
        if size > 0 {
            self.hydrate(path)?;
        } else {
            self.engine.rt().block_on(self.engine.overwriting(path));
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

    pub fn create(&self, path: &RelPath) -> io::Result<()> {
        let file_path = self.backing(path);
        ensure_parent(&file_path)?;
        fs::File::create(&file_path)?;
        self.engine.created(path);
        self.invalidate(&path.parent_or_root());
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

    pub fn delete(&self, path: &RelPath) -> io::Result<()> {
        let is_dir = matches!(self.attr(path)?, Some((true, ..)));
        self.engine
            .rt()
            .block_on(self.engine.delete(path, is_dir))?;
        remove_path(&self.backing(path))?;
        self.xattrs.forget(path);
        self.invalidate(path);
        self.invalidate(&path.parent_or_root());
        Ok(())
    }

    pub fn rmdir(&self, path: &RelPath) -> io::Result<()> {
        match self.ls(path) {
            Ok(listing) if listing.is_empty() => self.delete(path),
            Ok(_) => Err(io::Error::from_raw_os_error(libc::ENOTEMPTY)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => self.delete(path),
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
