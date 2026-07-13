use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::{Error as SdkError, Sdk};

use super::Observation;
use super::{io_err, Engine};

pub enum Upload {
    Done,
    Retry,
}

async fn save_with_parents(
    sdk: &Sdk,
    target: &RelPath,
    source: &Path,
    since: Option<SystemTime>,
) -> io::Result<Result<Option<SystemTime>, ()>> {
    let stream = crate::file_stream(source).await?;
    match sdk.save(&target.as_file(), stream, since).await {
        Ok(mtime) => Ok(Ok(mtime)),
        Err(SdkError::PreconditionFailed) => Ok(Err(())),
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
            let stream = crate::file_stream(source).await?;
            match sdk.save(&target.as_file(), stream, since).await {
                Ok(mtime) => Ok(Ok(mtime)),
                Err(SdkError::PreconditionFailed) => Ok(Err(())),
                Err(err) => Err(io_err(err)),
            }
        }
        Err(err) => Err(io_err(err)),
    }
}

impl<T: LocalTree> Engine<T> {
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
        let gate = super::gate(&self.uploading, path);
        let _gate = gate.lock().await;
        if self.is_frozen(path) {
            return Ok(Upload::Retry);
        }
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
                self.ledger().dirty_clear(path);
                return Ok(Upload::Done);
            }
            Err(err) => return Err(err),
        };
        let before = md.modified().ok();

        let recorded = self.ledger().observations.get(path).copied();
        let since = UNIX_EPOCH + Duration::from_secs(recorded.map_or(0, |rec| rec.time));
        let (target, mtime) = match save_with_parents(&self.sdk, path, &abs, Some(since)).await? {
            Ok(mtime) => (path.clone(), mtime),
            Err(()) => {
                let target = self.conflict_target(path).await;
                log::warn!("conflict on {path}: uploading as {target}");
                match save_with_parents(&self.sdk, &target, &abs, None).await? {
                    Ok(mtime) => (target, mtime),
                    Err(()) => return Err(io::Error::other("conflict copy was preempted")),
                }
            }
        };
        let uploaded = mtime.map(|mtime| Observation::new(md.len(), Some(mtime)));

        if target == *path {
            if let Some(rec) = uploaded {
                self.ledger().observe(path, rec);
            }
        }

        {
            let mut ledger = self.ledger();
            ledger.dirty_clear(path);
            ledger.dirty_clear(&target);
        }
        let after = fs::metadata(&abs).ok().and_then(|md| md.modified().ok());
        if after != before {
            self.ledger().dirty_set(path);
            return Ok(Upload::Retry);
        }

        if target != *path {
            if let Err(err) = self.tree.relocate(path, &target) {
                log::warn!("move conflicted copy {path} -> {target}: {err}");
            }
            let mut ledger = self.ledger();
            ledger.unobserve(path);
            if let Some(rec) = uploaded {
                ledger.observe(&target, rec);
            }
        }
        self.tree.settled(&target, after);
        log::info!("uploaded {target} ({} bytes)", md.len());
        Ok(Upload::Done)
    }
}
