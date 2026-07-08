//! The macOS adapter: like windows, the system owns the user's replica
//! (FileProvider materializes and dehydrates files on its own); the only
//! local state this crate keeps is a spool of unpushed edits. Content
//! travels down through fetch (streamed straight to the URL the system
//! consumes) and up through created/modified, which copy the system's
//! temp file into the spool and hand the debt to the engine's scheduler.
//! Deletes and renames are verdicts: the server call happens first and a
//! failure vetoes the operation in Finder.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use fsync_core::engine::{io_err, Engine};
use fsync_core::port::LocalTree;
use fsync_core::path::RelPath;
use fsync_core::scheduler::UploadStatus;
use fsync_core::sdk::{self, Sdk};
use futures_util::TryStreamExt;
use tokio::runtime::Runtime;

uniffi::setup_scaffolding!();

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SyncState {
    Idle,
    Busy,
    Error,
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

struct SpoolTree {
    spool_dir: PathBuf,
    ledger: PathBuf,
}

impl LocalTree for SpoolTree {
    fn backing(&self, path: &RelPath) -> PathBuf {
        self.spool_dir.join(path.as_str())
    }

    fn relocate(&self, from: &RelPath, to: &RelPath) -> io::Result<()> {
        let to_abs = self.backing(to);
        if let Some(parent) = to_abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(self.backing(from), to_abs)
    }

    fn settled(&self, target: &RelPath, _mtime: Option<std::time::SystemTime>) {
        let _ = fs::remove_file(self.backing(target));
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
    engine: Arc<Engine<SpoolTree>>,
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
        let spool_dir = data.join("spool");
        fs::create_dir_all(&spool_dir)?;
        let tree = SpoolTree {
            ledger: data.join("fsync.json"),
            spool_dir: spool_dir.clone(),
        };
        let engine = Engine::spawn(Arc::new(sdk), rt.handle().clone(), tree);
        engine.prune(&spool_dir)?;
        engine.recover();
        Ok(Arc::new(Self { rt, engine }))
    }

    pub fn ls(&self, path: String) -> Result<Vec<Entry>, FsError> {
        let dir = rel(&path);
        let listing = self.rt.block_on(self.engine.sdk().ls(&dir.as_dir()))?;
        Ok(self
            .engine
            .overlay(&dir, listing)
            .into_iter()
            .map(Entry::from)
            .collect())
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
        Ok(self
            .rt
            .block_on(self.engine.sdk().stat(&rel.as_file()))?
            .into())
    }

    pub fn fetch(&self, path: String, dest_path: String) -> Result<(), FsError> {
        let rel = rel(&path);
        if self.engine.dirty_metadata(&rel).is_some() {
            fs::copy(self.engine.tree().backing(&rel), &dest_path)?;
            return Ok(());
        }
        self.rt.block_on(async {
            let mut stream = self.engine.sdk().cat(&rel.as_file()).await.map_err(io_err)?;
            let mut file = fs::File::create(&dest_path)?;
            while let Some(chunk) = stream.try_next().await? {
                file.write_all(&chunk)?;
            }
            file.flush()?;
            Ok::<(), io::Error>(())
        })?;
        Ok(())
    }

    pub fn created(&self, path: String, contents_path: Option<String>) -> Result<(), FsError> {
        let rel = rel(&path);
        let abs = self.engine.tree().backing(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        match contents_path {
            Some(src) => {
                fs::copy(&src, &abs)?;
            }
            None => {
                fs::File::create(&abs)?;
            }
        }
        self.engine.created(&rel);
        self.engine.released(&rel);
        Ok(())
    }

    pub fn modified(&self, path: String, contents_path: String) -> Result<(), FsError> {
        let rel = rel(&path);
        self.rt.block_on(self.engine.overwriting(&rel));
        let abs = self.engine.tree().backing(&rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&contents_path, &abs)?;
        self.engine.modified(&rel);
        self.engine.released(&rel);
        Ok(())
    }

    pub fn mkdir(&self, path: String) -> Result<(), FsError> {
        let rel = rel(&path);
        self.rt
            .block_on(self.engine.sdk().mkdir(&rel.as_dir()))
            .map_err(FsError::from)
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
        Ok(())
    }

    pub fn thumbnail(&self, path: String) -> Result<Vec<u8>, FsError> {
        let rel = rel(&path);
        Ok(self
            .rt
            .block_on(self.engine.sdk().thumbnail(&rel.as_file()))?)
    }

    pub fn recover(&self) {
        self.engine.recover();
    }

    pub fn state(&self) -> SyncState {
        match *self.engine.upload_status().borrow() {
            UploadStatus::Idle => SyncState::Idle,
            UploadStatus::Busy => SyncState::Busy,
            UploadStatus::Error => SyncState::Error,
        }
    }

    pub fn flush(&self, timeout_ms: u64) {
        self.rt
            .block_on(self.engine.flush(Duration::from_millis(timeout_ms)));
    }

    pub fn vacuum(&self) -> Result<(), FsError> {
        self.engine
            .prune(&self.engine.tree().spool_dir)
            .map_err(FsError::from)
    }
}

fn runtime() -> Result<Runtime, FsError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| FsError::Other { msg: e.to_string() })
}
