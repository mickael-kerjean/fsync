use std::fs;
use std::io;
use std::path::Path;

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::{Error as SdkError, Sdk};

use super::{io_err, Engine};
use super::Observation;

pub enum Upload {
    Done,
    Retry,
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
