use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures_util::TryStreamExt;
use tokio::sync::watch;

use crate::path::RelPath;
use crate::port::LocalTree;

use super::{io_err, Engine, Observation};

fn part_file(abs: &Path) -> PathBuf {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut tmp = abs.as_os_str().to_owned();
    tmp.push(format!(".{}.part", COUNTER.fetch_add(1, Ordering::Relaxed)));
    PathBuf::from(tmp)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStatus {
    Running,
    Done,
    Failed,
}

pub struct Download {
    file: fs::File,
    state: watch::Receiver<(u64, DownloadStatus)>,
}

impl Download {
    pub async fn read(&self, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        let end = offset + size as u64;
        let mut state = self.state.clone();
        loop {
            let (written, status) = *state.borrow_and_update();
            match status {
                DownloadStatus::Failed => return Err(io::Error::other("download failed")),
                DownloadStatus::Done => break,
                DownloadStatus::Running if written >= end => break,
                DownloadStatus::Running => {
                    if state.changed().await.is_err() {
                        return Err(io::Error::other("download aborted"));
                    }
                }
            }
        }
        let mut buf = vec![0u8; size as usize];
        let mut filled = 0;
        while filled < buf.len() {
            let n = pread(&self.file, &mut buf[filled..], offset + filled as u64)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        Ok(buf)
    }

    pub async fn done(&self) -> io::Result<()> {
        let mut state = self.state.clone();
        loop {
            let status = state.borrow_and_update().1;
            match status {
                DownloadStatus::Done => return Ok(()),
                DownloadStatus::Failed => return Err(io::Error::other("download failed")),
                DownloadStatus::Running => {
                    if state.changed().await.is_err() {
                        return Err(io::Error::other("download aborted"));
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
fn pread(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn pread(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

impl<T: LocalTree> Engine<T> {
    pub async fn hydrate(&self, path: &RelPath) -> io::Result<()> {
        self.hydrate_start(path).await?;
        let download = self.downloads.lock().unwrap().get(path).cloned();
        match download {
            Some(download) => download.done().await,
            None => Ok(()),
        }
    }

    pub async fn hydrate_start(&self, path: &RelPath) -> io::Result<()> {
        let gate = self.hydrating.lock().unwrap().entry(path.clone()).or_default().clone();
        let _gate = gate.lock().await;
        let result = self.fetch_start(path).await;
        let mut hydrating = self.hydrating.lock().unwrap();
        if hydrating.get(path).is_some_and(|e| Arc::strong_count(e) <= 2) {
            hydrating.remove(path);
        }
        result
    }

    pub fn download(&self, path: &RelPath) -> Option<Arc<Download>> {
        if self.ledger().dirty.contains(path) {
            return None;
        }
        self.downloads.lock().unwrap().get(path).cloned()
    }

    async fn fetch_start(&self, path: &RelPath) -> io::Result<()> {
        if self.downloads.lock().unwrap().contains_key(path) {
            return Ok(());
        }
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
        fs::File::create(&tmp)?;
        let file = fs::File::open(&tmp)?;
        let (tx, state) = watch::channel((0u64, DownloadStatus::Running));
        self.downloads
            .lock()
            .unwrap()
            .insert(path.clone(), Arc::new(Download { file, state }));
        let engine = self.weak.upgrade().expect("engine is alive");
        self.rt.spawn(engine.stream(path.clone(), tmp, tx));
        Ok(())
    }

    async fn stream(
        self: Arc<Self>,
        path: RelPath,
        tmp: PathBuf,
        tx: watch::Sender<(u64, DownloadStatus)>,
    ) {
        let fail = |err: &dyn std::fmt::Display| {
            log::warn!("hydrate {path}: {err}");
            let _ = fs::remove_file(&tmp);
            self.downloads.lock().unwrap().remove(&path);
            tx.send_modify(|s| s.1 = DownloadStatus::Failed);
        };
        let downloaded = async {
            let mut stream = self.sdk.cat(&path.as_file()).await.map_err(io_err)?;
            let mut file = fs::File::options().append(true).open(&tmp)?;
            let mut size: u64 = 0;
            while let Some(chunk) = stream.try_next().await? {
                io::Write::write_all(&mut file, &chunk)?;
                size += chunk.len() as u64;
                tx.send_modify(|s| s.0 = size);
            }
            Ok::<u64, io::Error>(size)
        }
        .await;
        let size = match downloaded {
            Ok(size) => size,
            Err(err) => return fail(&err),
        };
        if self.ledger().dirty.contains(&path) {
            return fail(&"superseded by a local edit");
        }
        if let Err(err) = fs::rename(&tmp, self.tree.backing(&path)) {
            return fail(&err);
        }
        let observed = self
            .sdk
            .stat(&path.as_file())
            .await
            .ok()
            .map(|info| Observation::of(&info));
        {
            let mut ledger = self.ledger();
            match observed {
                Some(obs) => ledger.observe(&path, obs),
                None => ledger.unobserve(&path),
            }
            ledger.dirty_clear(&path);
        }
        self.downloads.lock().unwrap().remove(&path);
        tx.send_modify(|s| s.1 = DownloadStatus::Done);
        log::info!("cached {path} ({size} bytes)");
    }
}
