//! The iOS adapter: like android, an online-first gateway backed by the core
//! engine. Document ids are server paths (directories end with '/'). The
//! Files app talks to the FileProvider extension, which owns the one Adapter
//! instance; the companion app only logs in and registers the domain.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use fdrive_core::engine::{Engine, Observation};
use fdrive_core::path::RelPath;
use fdrive_core::port::LocalTree;
use fdrive_core::sdk::{self, Sdk};
use tokio::runtime::Runtime;

uniffi::setup_scaffolding!();

const META_TTL: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FsError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("permission denied")]
    PermissionDenied,
    #[error("not found")]
    NotFound,
    #[error("network error: {msg}")]
    Network { msg: String },
    #[error("{msg}")]
    Other { msg: String },
}

impl From<sdk::Error> for FsError {
    fn from(err: sdk::Error) -> Self {
        match err {
            sdk::Error::InvalidCredentials => Self::InvalidCredentials,
            sdk::Error::NotAuthenticated => Self::NotAuthenticated,
            sdk::Error::PermissionDenied => Self::PermissionDenied,
            sdk::Error::NotFound => Self::NotFound,
            sdk::Error::Http(err) => Self::Network {
                msg: err.to_string(),
            },
            err => Self::Other {
                msg: err.to_string(),
            },
        }
    }
}

impl From<io::Error> for FsError {
    fn from(err: io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            _ => Self::Other {
                msg: err.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub mtime_ms: Option<i64>,
}

impl From<sdk::FileInfo> for Entry {
    fn from(info: sdk::FileInfo) -> Self {
        Self {
            name: info.name,
            kind: match info.kind {
                sdk::FileType::File => EntryKind::File,
                sdk::FileType::Directory => EntryKind::Directory,
            },
            size: info.size,
            mtime_ms: info
                .mtime
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64),
        }
    }
}

#[uniffi::export]
pub fn login(
    url: String,
    insecure: bool,
    user: String,
    password: String,
    storage: String,
) -> Result<String, FsError> {
    let rt = runtime()?;
    let sdk = rt.block_on(
        Sdk::builder(&url)
            .insecure(insecure)
            .login(&user, &password, &storage),
    )?;
    Ok(sdk.token().unwrap_or_default().to_string())
}

#[uniffi::export]
pub fn end_session(url: String, insecure: bool, token: String) {
    let Ok(rt) = runtime() else { return };
    let Ok(sdk) = Sdk::builder(&url).insecure(insecure).token(token) else {
        return;
    };
    let _ = rt.block_on(sdk.logout());
}

#[uniffi::export]
pub fn ping(url: String, insecure: bool, token: String) -> bool {
    let Ok(rt) = runtime() else { return false };
    let Ok(sdk) = Sdk::builder(&url).insecure(insecure).token(token) else {
        return false;
    };
    rt.block_on(sdk.ls("/")).is_ok()
}

struct IosTree {
    cache_dir: PathBuf,
    ledger: PathBuf,
    meta: Mutex<HashMap<RelPath, (Instant, Vec<sdk::FileInfo>)>>,
}

impl IosTree {
    fn invalidate(&self, dir: &RelPath) {
        self.meta.lock().unwrap().remove(dir);
    }
}

impl LocalTree for IosTree {
    fn backing(&self, path: &RelPath) -> PathBuf {
        self.cache_dir.join(path.as_str())
    }

    fn relocate(&self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        let to_abs = self.backing(to);
        if let Some(parent) = to_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(self.backing(from), to_abs)
    }

    fn settled(&self, target: &RelPath, _mtime: Option<std::time::SystemTime>) {
        self.invalidate(&target.parent_or_root());
    }

    fn ledger(&self) -> PathBuf {
        self.ledger.clone()
    }
}

fn rel(document_id: &str) -> RelPath {
    RelPath::new(document_id)
}

#[derive(uniffi::Object)]
pub struct Adapter {
    rt: Runtime,
    engine: Arc<Engine<IosTree>>,
}

#[uniffi::export]
impl Adapter {
    #[uniffi::constructor]
    pub fn new(
        url: String,
        insecure: bool,
        token: String,
        data_dir: String,
    ) -> Result<Arc<Self>, FsError> {
        let sdk = Sdk::builder(&url).insecure(insecure).token(token)?;
        let rt = runtime()?;
        let data = PathBuf::from(data_dir);
        let cache_dir = data.join("cache");
        fs::create_dir_all(&cache_dir)?;
        let tree = IosTree {
            ledger: data.join("fdrive.db"),
            cache_dir: cache_dir.clone(),
            meta: Mutex::new(HashMap::new()),
        };
        let engine = Engine::spawn(Arc::new(sdk), rt.handle().clone(), tree);
        engine.prune(&cache_dir)?;
        engine.recover();
        Ok(Arc::new(Self { rt, engine }))
    }

    pub fn ls(&self, path: String) -> Result<Vec<Entry>, FsError> {
        let dir = rel(&path);
        Ok(self.listing(&dir)?.into_iter().map(Entry::from).collect())
    }

    pub fn stat(&self, path: String) -> Result<Entry, FsError> {
        let rel = rel(&path);
        if let Some(md) = self.engine.dirty_metadata(&rel) {
            return Ok(Entry {
                name: rel.name().to_string(),
                kind: EntryKind::File,
                size: Some(md.len()),
                mtime_ms: md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64),
            });
        }
        self.listing(&rel.parent_or_root())?
            .into_iter()
            .find(|e| e.name == rel.name())
            .map(Entry::from)
            .ok_or(FsError::NotFound)
    }

    pub fn open(&self, path: String) -> Result<String, FsError> {
        let rel = rel(&path);
        let mut current = None;
        if let Ok(listing) = self.listing(&rel.parent_or_root()) {
            if let Some(entry) = listing.iter().find(|e| e.name == rel.name()) {
                let observation = Observation::of(entry);
                if self.engine.content_current(&rel, observation) {
                    return Ok(self.local(&rel));
                }
                current = Some(observation);
            }
        }
        self.rt.block_on(self.engine.hydrate(&rel, current))?;
        Ok(self.local(&rel))
    }

    pub fn create(&self, path: String) -> Result<String, FsError> {
        let rel = rel(&path);
        let abs = self.engine.tree().backing(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::File::create(&abs)?;
        self.engine.created(&rel);
        self.engine.tree().invalidate(&rel.parent_or_root());
        Ok(self.local(&rel))
    }

    pub fn saved(&self, path: String) {
        let rel = rel(&path);
        self.engine.modified(&rel);
        self.engine.released(&rel);
    }

    pub fn mkdir(&self, path: String) -> Result<(), FsError> {
        let rel = rel(&path);
        self.rt.block_on(self.engine.sdk().mkdir(&rel.as_dir()))?;
        self.engine.tree().invalidate(&rel.parent_or_root());
        Ok(())
    }

    pub fn delete(&self, path: String) -> Result<(), FsError> {
        let is_dir = path.ends_with('/');
        let rel = rel(&path);
        self.rt.block_on(self.engine.delete(&rel, is_dir))?;
        let abs = self.engine.tree().backing(&rel);
        let _ = if is_dir {
            fs::remove_dir_all(&abs)
        } else {
            fs::remove_file(&abs)
        };
        self.engine.tree().invalidate(&rel);
        self.engine.tree().invalidate(&rel.parent_or_root());
        Ok(())
    }

    pub fn rename(&self, from: String, to: String) -> Result<(), FsError> {
        let is_dir = from.ends_with('/');
        let (from, to) = (rel(&from), rel(&to));
        self.rt.block_on(self.engine.rename(&from, &to, is_dir))?;
        let from_abs = self.engine.tree().backing(&from);
        if from_abs.exists() {
            let _ = self.engine.tree().relocate(&from, &to);
        }
        self.engine.tree().invalidate(&from.parent_or_root());
        self.engine.tree().invalidate(&to.parent_or_root());
        Ok(())
    }

    pub fn thumbnail(&self, path: String) -> Result<Vec<u8>, FsError> {
        let rel = rel(&path);
        Ok(self
            .rt
            .block_on(self.engine.sdk().thumbnail(&rel.as_file()))?)
    }

    pub fn flush(&self, timeout_ms: u64) {
        self.rt
            .block_on(self.engine.flush(Duration::from_millis(timeout_ms)));
    }
}

impl Adapter {
    fn listing(&self, dir: &RelPath) -> Result<Vec<sdk::FileInfo>, FsError> {
        let cached = {
            let meta = self.engine.tree().meta.lock().unwrap();
            meta.get(dir)
                .filter(|(at, _)| at.elapsed() < META_TTL)
                .map(|(_, listing)| listing.clone())
        };
        let listing = match cached {
            Some(listing) => listing,
            None => {
                let fetched = self.rt.block_on(self.engine.sdk().ls(&dir.as_dir()))?;
                self.engine.listed(dir, &fetched);
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

    fn local(&self, path: &RelPath) -> String {
        self.engine
            .tree()
            .backing(path)
            .to_string_lossy()
            .into_owned()
    }
}

fn runtime() -> Result<Runtime, FsError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| FsError::Other { msg: e.to_string() })
}
