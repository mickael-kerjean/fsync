use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot, watch};

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::scheduler::{self, Msg, UploadStatus};
use crate::sdk::{Error as SdkError, FileInfo, Sdk};

#[path = "engine_ledger.rs"]
mod ledger;
pub use ledger::{Ledger, Observation};

#[path = "engine_download.rs"]
mod download;
pub use download::Download;

#[path = "engine_upload.rs"]
mod upload;
pub(crate) use upload::Upload;

#[cfg(test)]
#[path = "engine_test.rs"]
mod tests;

pub struct Engine<T: LocalTree> {
    sdk: Arc<Sdk>,
    rt: tokio::runtime::Handle,
    ledger: Mutex<Ledger>,
    tree: T,
    ignore: crate::config::Ignore,
    unreadable: AtomicBool,
    queue: mpsc::UnboundedSender<Msg>,
    status: watch::Receiver<UploadStatus>,
    hydrating: Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
    downloads: Mutex<HashMap<RelPath, Arc<Download>>>,
    uploading: Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
    frozen: Mutex<BTreeSet<RelPath>>,
    weak: Weak<Engine<T>>,
}

pub(crate) fn gate(
    gates: &Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
    path: &RelPath,
) -> Arc<tokio::sync::Mutex<()>> {
    let mut gates = gates.lock().unwrap();
    gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    gates.entry(path.clone()).or_default().clone()
}

pub(crate) struct Frozen<'a> {
    set: &'a Mutex<BTreeSet<RelPath>>,
    paths: Vec<RelPath>,
}

impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        let mut set = self.set.lock().unwrap();
        for path in &self.paths {
            set.remove(path);
        }
    }
}

impl<T: LocalTree> Engine<T> {
    pub fn spawn(sdk: Arc<Sdk>, rt: tokio::runtime::Handle, tree: T) -> Arc<Self> {
        let ledger_file = tree.ledger();
        let ignore = crate::config::ignore(ledger_file.parent().unwrap_or(Path::new("")));
        let (ledger, unreadable) = match Ledger::open(&ledger_file) {
            Ok(ledger) => (ledger, false),
            Err(()) => {
                let _ = fs::remove_file(&ledger_file);
                (Ledger::open(&ledger_file).unwrap_or_default(), true)
            }
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
                ignore,
                unreadable: AtomicBool::new(unreadable),
                queue,
                status,
                hydrating: Mutex::new(HashMap::new()),
                downloads: Mutex::new(HashMap::new()),
                uploading: Mutex::new(HashMap::new()),
                frozen: Mutex::new(BTreeSet::new()),
                weak: weak.clone(),
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
        let extras: Vec<RelPath> = {
            let ledger = self.ledger();
            ledger
                .dirty
                .iter()
                .filter(|p| ledger.local_only(p) && p.parent_or_root() == *dir)
                .cloned()
                .collect()
        };
        for path in extras {
            let name = path.name();
            if !listing.iter().any(|e| e.name == name) {
                if let Ok(md) = fs::metadata(self.tree.backing(&path)) {
                    listing.push(FileInfo {
                        name: name.to_string(),
                        kind: crate::sdk::FileType::File,
                        size: Some(md.len()),
                        mtime: md.modified().ok(),
                    });
                }
            }
        }
        listing
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
            return Ok(());
        }
        let mut ledger = self.ledger();
        let gone: Vec<RelPath> = ledger
            .dirty
            .iter()
            .filter(|p| !self.tree.backing(p).is_file())
            .cloned()
            .collect();
        for path in &gone {
            ledger.dirty_clear(path);
        }
        let gone: Vec<RelPath> = ledger
            .observations
            .keys()
            .filter(|p| !ledger.dirty.contains(p))
            .cloned()
            .collect();
        for path in &gone {
            ledger.unobserve(path);
        }
        let keep: Vec<PathBuf> = ledger.dirty.iter().map(|p| self.tree.backing(p)).collect();
        drop(ledger);
        prune_dir(cache_root, &keep)?;
        Ok(())
    }

    pub fn modified(&self, path: &RelPath) {
        self.ledger().dirty_set(path);
        self.arm(path);
    }

    pub fn created(&self, path: &RelPath) {
        let mut ledger = self.ledger();
        ledger.unobserve(path);
        ledger.dirty_set(path);
    }

    pub fn released(&self, path: &RelPath) {
        if self.ledger().dirty.contains(path) {
            self.now(path);
        }
    }

    pub fn needs_baseline(&self, path: &RelPath) -> bool {
        let ledger = self.ledger();
        !ledger.observations.contains_key(path) && !ledger.dirty.contains(path)
    }

    pub async fn overwriting(&self, path: &RelPath) {
        if self.needs_baseline(path) {
            if let Ok(info) = self.sdk.stat(&path.as_file()).await {
                self.ledger().observe(path, Observation::of(&info));
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

    fn freeze(&self, paths: &[&RelPath]) -> Frozen<'_> {
        let paths: Vec<RelPath> = paths.iter().map(|p| (*p).clone()).collect();
        let mut set = self.frozen.lock().unwrap();
        for path in &paths {
            set.insert(path.clone());
        }
        Frozen {
            set: &self.frozen,
            paths,
        }
    }

    pub(crate) fn is_frozen(&self, path: &RelPath) -> bool {
        self.frozen
            .lock()
            .unwrap()
            .iter()
            .any(|p| path == p || path.is_descendant_of(p))
    }

    async fn drain(&self, path: &RelPath, subtree: bool) {
        let gates: Vec<Arc<tokio::sync::Mutex<()>>> = self
            .uploading
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| *p == path || (subtree && p.is_descendant_of(path)))
            .map(|(_, gate)| gate.clone())
            .collect();
        for gate in gates {
            let _gate = gate.lock().await;
        }
    }

    pub async fn delete(&self, path: &RelPath, is_dir: bool) -> io::Result<()> {
        let _frozen = self.freeze(&[path]);
        self.drain(path, is_dir).await;
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
        self.cancel(path);
        Ok(())
    }

    pub async fn rename(&self, from: &RelPath, to: &RelPath, is_dir: bool) -> io::Result<()> {
        let _frozen = self.freeze(&[from, to]);
        self.drain(from, is_dir).await;
        self.drain(to, false).await;
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
            ledger.unobserve(to);
            ledger.dirty_clear(to);
            ledger.remap(from, to);
        }
        self.cancel(from);
        let moved: Vec<RelPath> = self
            .ledger()
            .dirty
            .iter()
            .filter(|p| *p == to || p.is_descendant_of(to))
            .cloned()
            .collect();
        for path in moved {
            self.now(&path);
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
