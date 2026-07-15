use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use fdrive_core::engine::{io_err, Engine, Observation};
use fdrive_core::path::RelPath;
use fdrive_core::port::LocalTree;
use fdrive_core::scheduler::UploadStatus;
use fdrive_core::sdk::{Error as SdkError, FileInfo, FileType, Sdk};
use futures_util::TryStreamExt;
use tokio::sync::watch;

use crate::wire;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    Pinned,
    Unpinned,
    Unspecified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Dehydrated(Pin),
    Cached(Pin),
    Edited,
    New,
    Foreign,
}

pub struct PlaceholderTree {
    root: PathBuf,
    ledger: PathBuf,
    rt: tokio::runtime::Handle,
    suppressed: Mutex<BTreeMap<RelPath, usize>>,
}

impl PlaceholderTree {
    fn abs(&self, path: &RelPath) -> PathBuf {
        wire::abs_of(&self.root, path)
    }

    fn is_suppressed(&self, path: &RelPath) -> bool {
        self.suppressed
            .lock()
            .unwrap()
            .keys()
            .any(|p| p == path || path.is_descendant_of(p))
    }

    fn suppress<T>(&self, path: &RelPath, op: impl FnOnce() -> T) -> T {
        *self
            .suppressed
            .lock()
            .unwrap()
            .entry(path.clone())
            .or_insert(0) += 1;
        let result = op();
        let mut suppressed = self.suppressed.lock().unwrap();
        if let Some(n) = suppressed.get_mut(path) {
            *n -= 1;
            if *n == 0 {
                suppressed.remove(path);
            }
        }
        result
    }
}

impl LocalTree for PlaceholderTree {
    fn backing(&self, path: &RelPath) -> PathBuf {
        self.abs(path)
    }

    fn relocate(&self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        self.suppress(from, || fs::rename(self.abs(from), self.abs(to)))
    }

    fn settled(&self, target: &RelPath, mtime: Option<SystemTime>) {
        let abs = self.abs(target);
        let what = target.clone();
        self.rt.spawn_blocking(move || {
            if let Err(err) = wire::mark_in_sync_if_unmodified(&abs, &what, mtime) {
                log::debug!("mark in sync {what}: {err}");
            }
        });
    }

    fn ledger(&self) -> PathBuf {
        self.ledger.clone()
    }
}

pub struct Adapter {
    engine: Arc<Engine<PlaceholderTree>>,
    root: PathBuf,
    refreshing: Mutex<BTreeMap<RelPath, Instant>>,
    kept: Mutex<BTreeSet<RelPath>>,
    pinning: Mutex<BTreeSet<RelPath>>,
}

impl Adapter {
    pub fn new(
        sdk: Arc<Sdk>,
        rt: tokio::runtime::Handle,
        root: PathBuf,
        data: &Path,
    ) -> io::Result<Arc<Self>> {
        fs::create_dir_all(&root)?;
        let tree = PlaceholderTree {
            root: root.clone(),
            ledger: data.join("fdrive.db"),
            rt: rt.clone(),
            suppressed: Mutex::new(BTreeMap::new()),
        };
        Ok(Arc::new(Self {
            engine: Engine::spawn(sdk, rt, tree),
            root,
            refreshing: Mutex::new(BTreeMap::new()),
            kept: Mutex::new(BTreeSet::new()),
            pinning: Mutex::new(BTreeSet::new()),
        }))
    }

    pub async fn flush(&self, timeout: Duration) {
        self.engine.flush(timeout).await;
    }

    pub fn upload_status(&self) -> watch::Receiver<UploadStatus> {
        self.engine.upload_status()
    }

    pub fn connect(self: &Arc<Self>, root: &Path) -> io::Result<wire::Connection> {
        let fetch = self.clone();
        let populate = self.clone();
        let delete = self.clone();
        let rename = self.clone();
        wire::connect(
            root,
            wire::Callbacks {
                fetch: Box::new(move |path, expected, sink| fetch.fetch(path, expected, sink)),
                populate: Box::new(move |dir| populate.populate(dir)),
                delete: Box::new(move |path, is_dir| delete.on_delete(path, is_dir)),
                rename: Box::new(move |from, to, is_dir| rename.on_rename(from, to, is_dir)),
            },
        )
    }

    pub async fn recover(self: &Arc<Self>) -> io::Result<()> {
        self.engine.recover();
        let adapter = self.clone();
        for path in tokio::task::spawn_blocking(move || adapter.sweep()).await? {
            log::info!("recovered pending upload: {path}");
            self.engine.released(&path);
        }
        Ok(())
    }

    pub async fn resync(self: &Arc<Self>) -> io::Result<()> {
        log::info!("manual refresh: re-listing populated tree");
        let mut pending = vec![RelPath::root()];
        while let Some(dir) = pending.pop() {
            self.refresh(&dir).await?;
            let this = self.clone();
            let at = dir.clone();
            let mut children =
                tokio::task::spawn_blocking(move || this.populated_subdirs(&at)).await?;
            pending.append(&mut children);
        }
        log::info!("manual refresh: done");
        Ok(())
    }

    fn populated_subdirs(&self, dir: &RelPath) -> Vec<RelPath> {
        let mut dirs = Vec::new();
        let Ok(read) = fs::read_dir(self.abs(dir)) else {
            return dirs;
        };
        for entry in read.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(md) = entry.metadata() else { continue };
            if !md.is_dir() {
                continue;
            }
            match wire::placeholder_state(&entry.path()) {
                Ok(st) if st.placeholder && st.partial => continue,
                _ => dirs.push(dir.join(&name)),
            }
        }
        dirs
    }

    fn abs(&self, path: &RelPath) -> PathBuf {
        wire::abs_of(&self.root, path)
    }

    fn classify(&self, abs: &Path, path: &RelPath) -> io::Result<FileState> {
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
        let md = fs::symlink_metadata(abs)?;
        let ps = wire::placeholder_state(abs)?;
        if !ps.placeholder {
            return Ok(if self.engine.ledger().observations.contains_key(path) {
                FileState::Foreign
            } else {
                FileState::New
            });
        }
        if !ps.in_sync {
            return Ok(FileState::Edited);
        }
        let attrs = std::os::windows::fs::MetadataExt::file_attributes(&md);
        Ok(
            if attrs & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0 != 0 || ps.partial {
                FileState::Dehydrated(pin_of(&md))
            } else {
                FileState::Cached(pin_of(&md))
            },
        )
    }

    pub fn fetch(
        self: &Arc<Self>,
        path: &RelPath,
        expected: i64,
        sink: wire::SinkFn,
    ) -> io::Result<u64> {
        const ALIGN: usize = 4096;
        const FLUSH_AT: usize = 1 << 20;
        let sdk = self.engine.sdk().clone();
        let api = path.as_file();
        let info = self.engine.rt().block_on(sdk.stat(&api)).map_err(io_err)?;
        let size = info.size.unwrap_or(0);
        if size as i64 != expected {
            let mtime = info.mtime.unwrap_or_else(SystemTime::now);
            log::info!(
                "{path}: placeholder said {expected} bytes, server has {size}; failing this read and healing"
            );
            let this = self.clone();
            let what = path.clone();
            self.engine.rt().spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(err) = this.replace_placeholder(&what, size, mtime) {
                        log::warn!("heal {what}: {err}; will retry on the next read");
                    }
                })
                .await;
            });
            return Err(io::Error::other(format!(
                "{path}: size changed on the server; healing the placeholder"
            )));
        }
        let mut sent: u64 = 0;
        let mut buf: Vec<u8> = Vec::with_capacity(FLUSH_AT + ALIGN);
        self.engine.rt().block_on(async {
            let (_, mut stream) = sdk.cat(&api).await.map_err(io_err)?;
            while let Some(chunk) = stream.try_next().await? {
                buf.extend_from_slice(&chunk);
                if buf.len() >= FLUSH_AT {
                    let aligned = buf.len() & !(ALIGN - 1);
                    sink(sent, &buf[..aligned])?;
                    sent += aligned as u64;
                    buf.drain(..aligned);
                }
            }
            Ok::<(), io::Error>(())
        })?;
        if !buf.is_empty() {
            sink(sent, &buf)?;
            sent += buf.len() as u64;
        }
        if sent != size {
            return Err(io::Error::other(format!(
                "{path}: short download ({sent} of {size} bytes)"
            )));
        }
        self.engine.ledger().observe(path, Observation::of(&info));
        Ok(sent)
    }

    pub fn populate(&self, dir: &RelPath) -> io::Result<()> {
        let listing = match self
            .engine
            .rt()
            .block_on(self.engine.sdk().ls(&dir.as_dir()))
        {
            Ok(listing) => listing,
            Err(SdkError::NotFound) => return Ok(()),
            Err(err) => return Err(io_err(err)),
        };
        for entry in &listing {
            self.place(dir, entry);
        }
        Ok(())
    }

    pub fn on_delete(&self, path: &RelPath, is_dir: bool) -> io::Result<()> {
        if self.engine.tree().is_suppressed(path) {
            return Ok(());
        }
        self.engine.rt().block_on(self.engine.delete(path, is_dir))
    }

    pub fn on_rename(&self, from: &RelPath, to: &RelPath, is_dir: bool) -> io::Result<()> {
        if self.engine.tree().is_suppressed(from) {
            return Ok(());
        }
        self.engine
            .rt()
            .block_on(self.engine.rename(from, to, is_dir))
    }

    pub async fn on_change(self: &Arc<Self>, path: &RelPath) {
        let abs = self.abs(path);
        let Ok(md) = fs::symlink_metadata(&abs) else {
            return;
        };
        if md.is_dir() {
            let Ok(state) = wire::placeholder_state(&abs) else {
                return;
            };
            if !state.placeholder {
                match self.engine.sdk().mkdir(&path.as_dir()).await {
                    Ok(()) => log::info!("mkdir {path}"),
                    Err(err) => log::debug!("mkdir {path}: {err}"),
                }
                let what = path.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(err) = wire::mark_in_sync(&abs, &what) {
                        log::debug!("convert dir {what}: {err}");
                    }
                });
            } else if pin_of(&md) == Pin::Pinned {
                let this = self.clone();
                let what = path.clone();
                tokio::task::spawn_blocking(move || this.pin_subtree(&what));
            }
            return;
        }
        let Ok(fstate) = self.classify(&abs, path) else {
            return;
        };
        match fstate {
            FileState::Edited | FileState::New => self.engine.modified(path),
            FileState::Dehydrated(Pin::Pinned) => {
                let what = path.clone();
                tokio::task::spawn_blocking(move || match wire::set_hydration(&abs, true) {
                    Ok(()) => log::info!("hydrated {what} (pinned)"),
                    Err(err) => log::warn!("hydrate {what}: {err}"),
                });
            }
            FileState::Cached(Pin::Unpinned) => {
                let what = path.clone();
                tokio::task::spawn_blocking(move || match wire::set_hydration(&abs, false) {
                    Ok(()) => log::info!("dehydrated {what}"),
                    Err(err) => log::warn!("dehydrate {what}: {err}"),
                });
            }
            _ => {}
        }
    }

    pub async fn refresh(self: &Arc<Self>, dir: &RelPath) -> io::Result<()> {
        const STUCK: std::time::Duration = std::time::Duration::from_secs(120);
        let dir_abs = self.abs(dir);
        if !dir.is_root() {
            match wire::placeholder_state(&dir_abs) {
                Ok(state) if state.placeholder && state.partial => return Ok(()),
                Err(_) => return Ok(()),
                _ => {}
            }
        }
        {
            let mut refreshing = self.refreshing.lock().unwrap();
            match refreshing.get(dir) {
                Some(at) if at.elapsed() < STUCK => return Ok(()),
                _ => {}
            }
            refreshing.insert(dir.clone(), Instant::now());
        }
        let result = match self.engine.sdk().ls(&dir.as_dir()).await {
            Ok(listing) => {
                let this = self.clone();
                let dir2 = dir.clone();
                tokio::task::spawn_blocking(move || this.reconcile_local(&dir2, &dir_abs, listing))
                    .await
                    .map_err(io::Error::other)?
            }
            Err(SdkError::NotFound) => Ok(()),
            Err(err) => Err(io_err(err)),
        };
        self.refreshing.lock().unwrap().remove(dir);
        result
    }

    fn reconcile_local(
        &self,
        dir: &RelPath,
        dir_abs: &Path,
        listing: Vec<FileInfo>,
    ) -> io::Result<()> {
        self.engine.listed(dir, &listing);
        let listing = self.engine.overlay(dir, listing);
        let mut local: BTreeMap<String, fs::Metadata> = BTreeMap::new();
        for entry in fs::read_dir(dir_abs)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(md) = entry.metadata() {
                    local.insert(name.to_string(), md);
                }
            }
        }
        for entry in &listing {
            let child = dir.join(&entry.name);
            if child.parent_or_root() != *dir {
                continue;
            }
            match local.remove(&entry.name) {
                None => self.place(dir, entry),
                Some(md) => self.reconcile_entry(&child, entry, &md),
            }
        }
        for (name, md) in local {
            let child = dir.join(&name);
            if child.parent_or_root() != *dir {
                continue;
            }
            self.drop_stale(&child, md.is_dir());
        }
        Ok(())
    }

    fn place(&self, dir: &RelPath, entry: &FileInfo) {
        let child = dir.join(&entry.name);
        if child.parent_or_root() != *dir {
            log::warn!("skipping hostile name from the server in {dir}");
            return;
        }
        let mtime = entry.mtime.unwrap_or_else(SystemTime::now);
        let result = match entry.kind {
            FileType::Directory => wire::create_dir_placeholder(&self.root, &child, mtime),
            FileType::File => {
                wire::create_placeholder(&self.root, &child, entry.size.unwrap_or(0), mtime)
            }
        };
        if let Err(err) = result {
            log::debug!("place {child}: {err}");
        }
    }

    fn reconcile_entry(&self, path: &RelPath, remote: &FileInfo, md: &fs::Metadata) {
        let abs = self.abs(path);
        match (remote.kind, md.is_dir()) {
            (FileType::Directory, true) => {
                if matches!(wire::placeholder_state(&abs), Ok(st) if !st.placeholder) {
                    match wire::mark_in_sync(&abs, path) {
                        Ok(()) => log::info!("re-adopted directory {path}"),
                        Err(err) => log::debug!("adopt dir {path}: {err}"),
                    }
                }
            }
            (FileType::File, false) => match self.classify(&abs, path) {
                Ok(FileState::Cached(_) | FileState::Dehydrated(_)) => {
                    self.freshen(path, remote, md)
                }
                Ok(FileState::Foreign | FileState::New) => self.adopt(path, &abs, md),
                Ok(FileState::Edited) | Err(_) => {}
            },
            _ => {}
        }
    }

    fn adopt(&self, path: &RelPath, abs: &Path, md: &fs::Metadata) {
        if self.engine.ledger().dirty.contains(path) {
            return;
        }
        let observed = self.engine.ledger().observations.get(path).copied();
        match observed {
            Some(rec) if Observation::of_local(md) == rec => {
                match wire::mark_in_sync_if_unmodified(abs, path, md.modified().ok()) {
                    Ok(()) => log::info!("re-adopted {path}"),
                    Err(err) => log::debug!("adopt {path}: {err}"),
                }
            }
            Some(_) => {
                log::debug!("{path} no longer matches its observation; leaving it untouched");
            }
            None => self.engine.modified(path),
        }
    }

    fn freshen(&self, path: &RelPath, remote: &FileInfo, md: &fs::Metadata) {
        if self.engine.ledger().dirty.contains(path) {
            return;
        }
        let remote_rec = Observation::of(remote);
        let unchanged = match self.engine.ledger().observations.get(path).copied() {
            Some(rec) => rec == remote_rec,
            None => Observation::of_local(md) == remote_rec,
        };
        if unchanged {
            return;
        }
        match self.replace_placeholder(
            path,
            remote.size.unwrap_or(0),
            remote.mtime.unwrap_or_else(SystemTime::now),
        ) {
            Ok(()) => {}
            Err(err) => log::debug!("update {path}: {err}"),
        }
    }

    fn replace_placeholder(&self, path: &RelPath, size: u64, mtime: SystemTime) -> io::Result<()> {
        let abs = self.abs(path);
        let pinned = matches!(
            self.classify(&abs, path),
            Ok(FileState::Cached(Pin::Pinned) | FileState::Dehydrated(Pin::Pinned))
        );
        let result = self.engine.tree().suppress(path, || {
            match wire::delete_if_clean(&abs) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            wire::create_placeholder(&self.root, path, size, mtime)
        });
        if result.is_ok() {
            self.engine.ledger().unobserve(path);
            log::info!("rebuilt placeholder {path} ({size} bytes)");
            if pinned {
                if let Err(err) = wire::set_pinned(&abs) {
                    log::warn!("re-pin {path}: {err}");
                }
            }
        }
        result
    }

    fn drop_stale(&self, path: &RelPath, is_dir: bool) {
        let abs = self.abs(path);
        if is_dir {
            if !self.tree_is_clean(&abs, path) {
                if self.kept.lock().unwrap().insert(path.clone()) {
                    log::info!("{path} gone remotely but holds local edits; keeping");
                }
                return;
            }
        } else if !self.file_is_clean(&abs, path) {
            if matches!(self.classify(&abs, path), Ok(FileState::New))
                && !self.engine.ledger().dirty.contains(path)
            {
                log::info!("found new local file {path}");
                self.engine.modified(path);
            }
            return;
        }
        let removed = self.engine.tree().suppress(path, || {
            if is_dir {
                fs::remove_dir_all(&abs)
            } else {
                wire::delete_if_clean(&abs)
            }
        });
        match removed {
            Ok(()) => {
                log::info!("dropped {path} (gone remotely)");
                self.engine.ledger().forget(path);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.engine.ledger().forget(path);
            }
            Err(err) => log::warn!("drop {path}: {err}"),
        }
        self.kept.lock().unwrap().remove(path);
    }

    fn file_is_clean(&self, abs: &Path, path: &RelPath) -> bool {
        !self.engine.ledger().dirty.contains(path)
            && matches!(
                self.classify(abs, path),
                Ok(FileState::Cached(_) | FileState::Dehydrated(_))
            )
    }

    fn tree_is_clean(&self, abs: &Path, path: &RelPath) -> bool {
        let Ok(state) = wire::placeholder_state(abs) else {
            return false;
        };
        if !state.placeholder {
            return false;
        }
        if state.partial {
            return true;
        }
        let Ok(read) = fs::read_dir(abs) else {
            return false;
        };
        for entry in read.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                return false;
            };
            let child = path.join(&name);
            let child_abs = entry.path();
            let clean = match entry.metadata() {
                Ok(md) if md.is_dir() => self.tree_is_clean(&child_abs, &child),
                Ok(_) => self.file_is_clean(&child_abs, &child),
                Err(_) => false,
            };
            if !clean {
                return false;
            }
        }
        true
    }

    fn sweep(&self) -> Vec<RelPath> {
        let mut armed = Vec::new();
        let mut dehydrated = 0u32;
        let mut hydrated = 0u32;
        let mut pending = vec![(RelPath::root(), false)];
        while let Some((dir, inherited)) = pending.pop() {
            let Ok(read) = fs::read_dir(self.abs(&dir)) else {
                continue;
            };
            for entry in read.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let child = dir.join(&name);
                let abs = entry.path();
                let Ok(md) = entry.metadata() else { continue };
                let pin = match pin_of(&md) {
                    Pin::Unspecified if inherited => match wire::set_pinned(&abs) {
                        Ok(()) => Pin::Pinned,
                        Err(err) => {
                            log::debug!("pin {child}: {err}");
                            Pin::Unspecified
                        }
                    },
                    pin => pin,
                };
                if md.is_dir() {
                    match wire::placeholder_state(&abs) {
                        Ok(st) if st.placeholder && st.partial => {
                            if pin == Pin::Pinned {
                                pending.push((child, true));
                            }
                        }
                        _ => pending.push((child, pin == Pin::Pinned)),
                    }
                    continue;
                }
                match self.classify(&abs, &child) {
                    Ok(FileState::Edited) if !self.engine.ledger().dirty.contains(&child) => {
                        self.engine.modified(&child);
                        armed.push(child);
                    }
                    Ok(FileState::Dehydrated(Pin::Pinned)) => match wire::set_hydration(&abs, true)
                    {
                        Ok(()) => hydrated += 1,
                        Err(err) => log::debug!("hydrate {child}: {err}"),
                    },
                    Ok(FileState::Cached(Pin::Unpinned)) => {
                        match wire::set_hydration(&abs, false) {
                            Ok(()) => dehydrated += 1,
                            Err(err) => log::debug!("dehydrate {child}: {err}"),
                        }
                    }
                    _ => {}
                }
            }
        }
        if dehydrated > 0 || hydrated > 0 {
            log::info!("pin sweep: {hydrated} hydrated, {dehydrated} dehydrated");
        }
        armed
    }

    fn pin_subtree(&self, dir: &RelPath) {
        {
            let mut pinning = self.pinning.lock().unwrap();
            if pinning.iter().any(|p| p == dir || dir.is_descendant_of(p)) {
                return;
            }
            pinning.insert(dir.clone());
        }
        self.pin_walk(dir);
        self.pinning.lock().unwrap().remove(dir);
    }

    fn pin_walk(&self, dir: &RelPath) {
        let Ok(read) = fs::read_dir(self.abs(dir)) else {
            return;
        };
        for entry in read.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let child = dir.join(&name);
            let abs = entry.path();
            let Ok(md) = entry.metadata() else { continue };
            match pin_of(&md) {
                Pin::Unpinned => continue,
                Pin::Pinned => {}
                Pin::Unspecified => {
                    if let Err(err) = wire::set_pinned(&abs) {
                        log::debug!("pin {child}: {err}");
                        continue;
                    }
                }
            }
            if md.is_dir() {
                self.pin_walk(&child);
            } else if matches!(self.classify(&abs, &child), Ok(FileState::Dehydrated(_))) {
                match wire::set_hydration(&abs, true) {
                    Ok(()) => log::info!("hydrated {child} (pinned)"),
                    Err(err) => log::warn!("hydrate {child}: {err}"),
                }
            }
        }
    }

    pub fn vacuum(&self) -> io::Result<()> {
        let root = RelPath::root();
        let result = self
            .engine
            .tree()
            .suppress(&root, || self.vacuum_dir(&root));
        result.map(|_| ())
    }

    fn vacuum_dir(&self, dir: &RelPath) -> io::Result<bool> {
        let mut emptied = true;
        for entry in fs::read_dir(self.abs(dir))? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                emptied = false;
                continue;
            };
            let child = dir.join(&name);
            let abs = entry.path();
            let Ok(md) = entry.metadata() else {
                emptied = false;
                continue;
            };
            if md.is_dir() {
                let state = wire::placeholder_state(&abs).ok();
                let placeholder = state.is_some_and(|s| s.placeholder);
                if placeholder && state.is_some_and(|s| s.partial) {
                    if fs::remove_dir_all(&abs).is_err() {
                        emptied = false;
                    }
                } else if placeholder && self.vacuum_dir(&child).unwrap_or(false) {
                    if fs::remove_dir(&abs).is_err() {
                        emptied = false;
                    }
                } else {
                    let _ = self.vacuum_dir(&child);
                    emptied = false;
                }
            } else if self.file_is_clean(&abs, &child) {
                if wire::delete_if_clean(&abs).is_err() {
                    emptied = false;
                }
            } else {
                emptied = false;
            }
        }
        Ok(emptied)
    }
}

fn pin_of(md: &fs::Metadata) -> Pin {
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_PINNED, FILE_ATTRIBUTE_UNPINNED};
    let attrs = std::os::windows::fs::MetadataExt::file_attributes(md);
    if attrs & FILE_ATTRIBUTE_PINNED.0 != 0 {
        Pin::Pinned
    } else if attrs & FILE_ATTRIBUTE_UNPINNED.0 != 0 {
        Pin::Unpinned
    } else {
        Pin::Unspecified
    }
}
