use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::TryStreamExt;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::scheduler::{self, Msg, UploadStatus};
use crate::sdk::{Error as SdkError, FileInfo, Sdk};

pub enum Upload {
    Done,
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub size: u64,
    pub time: u64,
}

impl Observation {
    pub fn new(size: u64, mtime: Option<SystemTime>) -> Self {
        Self {
            size,
            time: secs(mtime),
        }
    }

    pub fn of(info: &FileInfo) -> Self {
        Self::new(info.size.unwrap_or(0), info.mtime)
    }

    pub fn of_local(md: &fs::Metadata) -> Self {
        Self::new(md.len(), md.modified().ok())
    }
}

fn secs(t: Option<SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub observations: BTreeMap<RelPath, Observation>,
    #[serde(default)]
    pub dirty: BTreeSet<RelPath>,
}

impl Ledger {
    pub fn forget(&mut self, path: &RelPath) {
        self.observations
            .retain(|p, _| p != path && !p.is_descendant_of(path));
        self.dirty
            .retain(|p| p != path && !p.is_descendant_of(path));
    }

    pub fn remap(&mut self, from: &RelPath, to: &RelPath) {
        let rebase =
            |p: &RelPath| RelPath::new(&p.as_str().replacen(from.as_str(), to.as_str(), 1));
        let moved: Vec<RelPath> = self
            .observations
            .keys()
            .filter(|p| *p == from || p.is_descendant_of(from))
            .cloned()
            .collect();
        for p in moved {
            let record = self.observations.remove(&p).unwrap();
            self.observations.insert(rebase(&p), record);
        }
        let moved: Vec<RelPath> = self
            .dirty
            .iter()
            .filter(|p| *p == from || p.is_descendant_of(from))
            .cloned()
            .collect();
        for p in moved {
            self.dirty.remove(&p);
            self.dirty.insert(rebase(&p));
        }
    }

    pub fn local_only(&self, path: &RelPath) -> bool {
        !self.observations.contains_key(path) && self.dirty.contains(path)
    }
}

fn load_ledger(ledger_file: &Path) -> Result<Ledger, ()> {
    match fs::read(ledger_file) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(ledger) => Ok(ledger),
            Err(err) => {
                log::error!("{} is unreadable: {err}", ledger_file.display());
                Err(())
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(err) => {
            log::error!("{} is unreadable: {err}", ledger_file.display());
            Err(())
        }
    }
}

fn store_ledger(ledger_file: &Path, ledger: &Ledger) {
    if let Ok(bytes) = serde_json::to_vec(ledger) {
        if let Err(err) = crate::write_atomic(ledger_file, &bytes) {
            log::error!("ledger save: {err}");
        }
    }
}

pub async fn save_with_parents(sdk: &Sdk, target: &RelPath, source: &Path) -> io::Result<()> {
    match sdk
        .save(&target.as_file(), crate::file_stream(source).await?)
        .await
    {
        Ok(()) => Ok(()),
        Err(SdkError::NotFound | SdkError::PermissionDenied) => {
            let mut ancestors = vec![];
            let mut cur = target.parent_or_root();
            while !cur.is_root() {
                ancestors.push(cur.clone());
                cur = cur.parent_or_root();
            }
            for dir in ancestors.iter().rev() {
                if let Err(err) = sdk.mkdir(&dir.as_dir()).await {
                    log::debug!("mkdirs {dir}: {err}");
                }
            }
            sdk.save(&target.as_file(), crate::file_stream(source).await?)
                .await
                .map_err(io_err)
        }
        Err(err) => Err(io_err(err)),
    }
}

fn part_file(abs: &Path) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut tmp = abs.as_os_str().to_owned();
    tmp.push(format!(".{}.part", COUNTER.fetch_add(1, Ordering::Relaxed)));
    PathBuf::from(tmp)
}

async fn download(sdk: &Sdk, path: &RelPath, tmp: &Path) -> io::Result<u64> {
    let mut stream = sdk.cat(&path.as_file()).await.map_err(io_err)?;
    let mut file = fs::File::create(tmp)?;
    let mut size: u64 = 0;
    while let Some(chunk) = stream.try_next().await? {
        io::Write::write_all(&mut file, &chunk)?;
        size += chunk.len() as u64;
    }
    Ok(size)
}

pub fn io_err(err: SdkError) -> io::Error {
    match err {
        SdkError::NotFound => io::ErrorKind::NotFound.into(),
        SdkError::PermissionDenied => io::ErrorKind::PermissionDenied.into(),
        err => io::Error::other(err),
    }
}

fn prune_dir(dir: &Path, keep: &[PathBuf]) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if keep.iter().any(|k| k == &path) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if keep.iter().any(|k| k.starts_with(&path)) {
                prune_dir(&path, keep)?;
            } else {
                fs::remove_dir_all(&path)?;
            }
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "engine_test.rs"]
mod tests;

pub struct Engine<T: LocalTree> {
    sdk: Arc<Sdk>,
    rt: tokio::runtime::Handle,
    ledger: Mutex<Ledger>,
    tree: T,
    ledger_file: PathBuf,
    ignore: crate::config::Ignore,
    unreadable: AtomicBool,
    queue: mpsc::UnboundedSender<Msg>,
    status: watch::Receiver<UploadStatus>,
}

impl<T: LocalTree> Engine<T> {
    pub fn spawn(sdk: Arc<Sdk>, rt: tokio::runtime::Handle, tree: T) -> Arc<Self> {
        let ledger_file = tree.ledger();
        let ignore = crate::config::ignore(ledger_file.parent().unwrap_or(Path::new("")));
        let (ledger, unreadable) = match load_ledger(&ledger_file) {
            Ok(ledger) => (ledger, false),
            Err(()) => (Ledger::default(), true),
        };
        let (queue, rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(UploadStatus::Idle);
        Arc::new_cyclic(|weak| {
            rt.spawn(scheduler::run(weak.clone(), rx, status_tx));
            Self {
                sdk,
                rt: rt.clone(),
                ledger: Mutex::new(ledger),
                tree,
                ledger_file,
                ignore,
                unreadable: AtomicBool::new(unreadable),
                queue,
                status,
            }
        })
    }

    fn arm(&self, path: &RelPath) {
        let _ = self.queue.send(Msg::Arm(path.clone()));
    }

    fn now(&self, path: &RelPath) {
        let _ = self.queue.send(Msg::Now(path.clone()));
    }

    fn cancel(&self, path: &RelPath) {
        let _ = self.queue.send(Msg::Cancel(path.clone()));
    }

    pub async fn flush(&self, timeout: Duration) {
        let (reply, done) = oneshot::channel();
        if self.queue.send(Msg::Flush(reply)).is_ok() {
            let _ = tokio::time::timeout(timeout, done).await;
        }
    }

    pub fn upload_status(&self) -> watch::Receiver<UploadStatus> {
        self.status.clone()
    }

    pub fn recover(&self) {
        for path in self.ledger().dirty.iter() {
            if self.ignore.matches(path) {
                continue;
            }
            log::info!("recovered pending upload: {path}");
            self.now(path);
        }
    }

    pub fn overlay(&self, dir: &RelPath, mut listing: Vec<FileInfo>) -> Vec<FileInfo> {
        let ledger = self.ledger();
        for path in ledger.dirty.iter() {
            if ledger.local_only(path) && path.parent_or_root() == *dir {
                let name = path.name();
                if !listing.iter().any(|e| e.name == name) {
                    if let Ok(md) = fs::metadata(self.tree.backing(path)) {
                        listing.push(FileInfo {
                            name: name.to_string(),
                            kind: crate::sdk::FileType::File,
                            size: Some(md.len()),
                            mtime: md.modified().ok(),
                        });
                    }
                }
            }
        }
        listing
    }

    pub async fn hydrate(&self, path: &RelPath) -> io::Result<()> {
        let (observed, dirty) = {
            let ledger = self.ledger();
            (
                ledger.observations.get(path).copied(),
                ledger.dirty.contains(path),
            )
        };
        if dirty {
            return Ok(());
        }
        let current = match self.sdk.stat(&path.as_file()).await {
            Ok(info) => Observation::of(&info),
            Err(err) => return Err(io_err(err)),
        };
        let abs = self.tree.backing(path);
        if observed == Some(current) && abs.is_file() {
            return Ok(());
        }
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = part_file(&abs);
        let size = match download(&self.sdk, path, &tmp).await {
            Ok(size) => size,
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }
        };
        if self.ledger().dirty.contains(path) {
            let _ = fs::remove_file(&tmp);
            return Ok(());
        }
        fs::rename(&tmp, &abs)?;
        let observed = self
            .sdk
            .stat(&path.as_file())
            .await
            .ok()
            .map(|info| Observation::of(&info));
        {
            let mut ledger = self.ledger();
            match observed {
                Some(obs) => {
                    ledger.observations.insert(path.clone(), obs);
                }
                None => {
                    ledger.observations.remove(path);
                }
            }
            ledger.dirty.remove(path);
        }
        self.persist();
        log::info!("cached {path} ({size} bytes)");
        Ok(())
    }

    pub fn prune(&self, cache_root: &Path) -> io::Result<()> {
        if self.unreadable.swap(false, Ordering::SeqCst) {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let aside = cache_root.with_file_name(format!(
                "{}.unreadable-{stamp}",
                cache_root.file_name().unwrap_or_default().to_string_lossy()
            ));
            log::error!(
                "the ledger was unreadable; moving the cache to {} instead of pruning it",
                aside.display()
            );
            fs::rename(cache_root, &aside)?;
            fs::create_dir_all(cache_root)?;
            self.persist();
            return Ok(());
        }
        let mut ledger = self.ledger();
        let Ledger {
            observations,
            dirty,
        } = &mut *ledger;
        dirty.retain(|path| self.tree.backing(path).is_file());
        observations.retain(|path, _| dirty.contains(path));
        let keep: Vec<PathBuf> = dirty.iter().map(|p| self.tree.backing(p)).collect();
        drop(ledger);
        prune_dir(cache_root, &keep)?;
        self.persist();
        Ok(())
    }

    pub fn modified(&self, path: &RelPath) {
        if self.ledger().dirty.insert(path.clone()) {
            self.persist();
        }
        self.arm(path);
    }

    pub fn created(&self, path: &RelPath) {
        {
            let mut ledger = self.ledger();
            ledger.observations.remove(path);
            ledger.dirty.insert(path.clone());
        }
        self.persist();
    }

    pub fn released(&self, path: &RelPath) {
        if self.ledger().dirty.contains(path) {
            self.now(path);
        }
    }

    pub async fn overwriting(&self, path: &RelPath) {
        let unobserved = {
            let ledger = self.ledger();
            !ledger.observations.contains_key(path) && !ledger.dirty.contains(path)
        };
        if unobserved {
            if let Ok(info) = self.sdk.stat(&path.as_file()).await {
                self.ledger()
                    .observations
                    .insert(path.clone(), Observation::of(&info));
            }
        }
    }

    pub fn content_current(&self, path: &RelPath, current: Observation) -> bool {
        let (observed, dirty) = {
            let ledger = self.ledger();
            (
                ledger.observations.get(path).copied(),
                ledger.dirty.contains(path),
            )
        };
        dirty || (observed == Some(current) && self.tree.backing(path).is_file())
    }

    pub fn dirty_metadata(&self, path: &RelPath) -> Option<fs::Metadata> {
        if !self.ledger().dirty.contains(path) {
            return None;
        }
        fs::metadata(self.tree.backing(path)).ok()
    }

    pub async fn delete(&self, path: &RelPath, is_dir: bool) -> io::Result<()> {
        let local_only = !is_dir && self.ledger().local_only(path);
        if !local_only {
            let api = if is_dir {
                path.as_dir()
            } else {
                path.as_file()
            };
            match self.sdk.rm(&api).await {
                Ok(()) | Err(SdkError::NotFound) => {}
                Err(err) => return Err(io_err(err)),
            }
        }
        log::info!("deleted {path}");
        self.ledger().forget(path);
        self.persist();
        self.cancel(path);
        Ok(())
    }

    pub async fn rename(&self, from: &RelPath, to: &RelPath, is_dir: bool) -> io::Result<()> {
        if !self.ledger().local_only(from) {
            let (api_from, api_to) = if is_dir {
                (from.as_dir(), to.as_dir())
            } else {
                (from.as_file(), to.as_file())
            };
            match self.sdk.mv(&api_from, &api_to).await {
                Ok(()) => {}
                Err(SdkError::NotFound) if self.ledger().dirty.contains(from) => {}
                Err(err) => return Err(io_err(err)),
            }
        }
        log::info!("renamed {from} -> {to}");
        {
            let mut ledger = self.ledger();
            ledger.observations.remove(to);
            ledger.dirty.remove(to);
            ledger.remap(from, to);
        }
        self.persist();
        self.cancel(from);
        let moved: Vec<RelPath> = self
            .ledger()
            .dirty
            .iter()
            .filter(|p| *p == to || p.is_descendant_of(to))
            .cloned()
            .collect();
        for path in moved {
            self.arm(&path);
        }
        Ok(())
    }

    pub fn sdk(&self) -> &Arc<Sdk> {
        &self.sdk
    }

    pub fn rt(&self) -> &tokio::runtime::Handle {
        &self.rt
    }

    pub fn tree(&self) -> &T {
        &self.tree
    }

    pub fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap()
    }

    pub fn persist(&self) {
        store_ledger(&self.ledger_file, &self.ledger());
    }

    async fn conflict_target(&self, path: &RelPath) -> RelPath {
        let (stem, ext) = match path.name().rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
            _ => (path.name().to_string(), String::new()),
        };
        let dir = path.parent_or_root();
        for n in 0..10 {
            let name = match n {
                0 => format!("{stem} (conflicted copy){ext}"),
                n => format!("{stem} (conflicted copy {}){ext}", n + 1),
            };
            let candidate = dir.join(&name);
            if self.sdk.stat(&candidate.as_file()).await.is_err()
                && !self.tree.backing(&candidate).exists()
            {
                return candidate;
            }
        }
        dir.join(&format!("{stem} (conflicted copy){ext}"))
    }

    pub(crate) async fn upload(&self, path: &RelPath) -> io::Result<Upload> {
        if !self.ledger().dirty.contains(path) {
            return Ok(Upload::Done);
        }
        if self.ignore.matches(path) {
            log::debug!("{path} is ignored");
            return Ok(Upload::Done);
        }
        let abs = self.tree.backing(path);
        let md = match fs::metadata(&abs) {
            Ok(md) => md,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.ledger().dirty.remove(path);
                return Ok(Upload::Done);
            }
            Err(err) => return Err(err),
        };
        let before = md.modified().ok();

        let recorded = self.ledger().observations.get(path).copied();
        let server = self
            .sdk
            .stat(&path.as_file())
            .await
            .ok()
            .map(|i| Observation::of(&i));
        let target = match (recorded, server) {
            (Some(rec), Some(now)) if rec != now => self.conflict_target(path).await,
            (None, Some(_)) => self.conflict_target(path).await,
            _ => path.clone(),
        };
        if target != *path {
            log::warn!("conflict on {path}: uploading as {target}");
        }

        if !self.ledger().dirty.contains(path) {
            return Ok(Upload::Done);
        }
        save_with_parents(&self.sdk, &target, &abs).await?;
        let uploaded = self
            .sdk
            .stat(&target.as_file())
            .await
            .ok()
            .map(|info| Observation::of(&info));

        if target == *path {
            if let Some(rec) = uploaded {
                self.ledger().observations.insert(path.clone(), rec);
            }
        }

        {
            let mut ledger = self.ledger();
            ledger.dirty.remove(path);
            ledger.dirty.remove(&target);
        }
        let after = fs::metadata(&abs).ok().and_then(|md| md.modified().ok());
        if after != before {
            self.ledger().dirty.insert(path.clone());
            self.persist();
            return Ok(Upload::Retry);
        }

        if target != *path {
            if let Err(err) = self.tree.relocate(path, &target) {
                log::warn!("move conflicted copy {path} -> {target}: {err}");
            }
            let mut ledger = self.ledger();
            ledger.observations.remove(path);
            if let Some(rec) = uploaded {
                ledger.observations.insert(target.clone(), rec);
            }
        }
        self.persist();
        self.tree.settled(&target, after);
        log::info!("uploaded {target} ({} bytes)", md.len());
        Ok(Upload::Done)
    }
}
