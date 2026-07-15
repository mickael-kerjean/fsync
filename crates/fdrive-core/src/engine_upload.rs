use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::{Error as SdkError, Sdk};

use super::{io_err, Conflict, Engine, Observation, Operation, Replayed};

enum Saved {
    Done(Option<SystemTime>),
    Conflict,
}

pub(crate) fn signature(data: &[u8]) -> Vec<u8> {
    fast_rsync::Signature::calculate(
        data,
        fast_rsync::SignatureOptions {
            block_size: 2048,
            crypto_hash_size: 16,
        },
    )
    .into_serialized()
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

    pub(crate) async fn replay_save(
        &self,
        path: &RelPath,
        replaces: Option<Observation>,
        reuses: Option<&RelPath>,
    ) -> io::Result<Replayed> {
        let gate = super::gate(&self.uploading, path);
        let _gate = gate.lock().await;
        if self.is_frozen(path) {
            return Ok(Replayed::Busy);
        }
        let abs = self.tree.backing(path);
        let md = match fs::metadata(&abs) {
            Ok(md) => md,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Replayed::Done);
            }
            Err(err) => return Err(err),
        };
        let before = md.modified().ok();
        let since = UNIX_EPOCH + Duration::from_secs(replaces.map_or(0, |r| r.time));

        let delta = {
            let source = reuses.unwrap_or(path);
            let sig = self.ledger().sign_get(source);
            let time = match reuses {
                Some(base) => self.ledger().observations.get(base).map(|o| o.time),
                None => replaces.map(|r| r.time),
            };
            match (sig, time) {
                (Some(sig), Some(time)) => {
                    let base_since = UNIX_EPOCH + Duration::from_secs(time);
                    upload_delta(&self.sdk, path, &abs, sig, reuses, base_since).await
                }
                _ => None,
            }
        };
        let attempt = match delta {
            Some(saved) => saved,
            None => upload_full(&self.sdk, path, &abs, Some(since)).await?,
        };
        let (target, mtime) = match attempt {
            Saved::Done(mtime) => (path.clone(), mtime),
            Saved::Conflict => {
                let found = match self.sdk.stat(&path.as_file()).await {
                    Ok(info) => Some(Observation::of(&info)),
                    Err(_) => None,
                };
                let target = self.conflict_target(path).await;
                log::warn!("conflict on {path}: uploading as {target}");
                match upload_full(&self.sdk, &target, &abs, None).await? {
                    Saved::Done(mtime) => {
                        self.conflicted(Conflict::new(
                            Operation::Write(path.clone()),
                            replaces,
                            found,
                            Some(target.clone()),
                        ));
                        if let Some(found) = found {
                            self.ledger().observe(path, found);
                        }
                        (target, mtime)
                    }
                    Saved::Conflict => return Err(io::Error::other("conflict copy was preempted")),
                }
            }
        };
        let uploaded = mtime.map(|mtime| Observation::new(md.len(), Some(mtime)));

        if target == *path {
            if let Some(rec) = uploaded {
                self.ledger().observe(path, rec);
            }
        }
        let after = fs::metadata(&abs).ok().and_then(|md| md.modified().ok());
        if after != before && target == *path {
            self.record(Operation::Write(path.clone()));
        }

        if target != *path {
            if let Err(err) = self.tree.relocate(path, &target) {
                log::warn!("move conflicted copy {path} -> {target}: {err}");
            }
            if let Some(rec) = uploaded {
                self.ledger().observe(&target, rec);
            }
        }
        if uploaded.is_some() {
            if let Ok(data) = fs::read(self.tree.backing(&target)) {
                self.ledger().sign_set(&target, &signature(&data));
            }
        }
        self.tree.settled(&target, after);
        log::info!("uploaded {target} ({} bytes)", md.len());
        Ok(Replayed::Done)
    }
}

async fn upload_delta(
    sdk: &Sdk,
    target: &RelPath,
    source: &Path,
    sig: Vec<u8>,
    base: Option<&RelPath>,
    since: SystemTime,
) -> Option<Saved> {
    if !sdk.delta_supported().await {
        return None;
    }
    let sig = fast_rsync::Signature::deserialize(sig).ok()?;
    let data = fs::read(source).ok()?;
    let mut body = vec![1u8];
    fast_rsync::diff(&sig.index(), &data, &mut body).ok()?;
    if body.len() >= data.len() {
        return None;
    }
    use sha2::Digest;
    body.extend_from_slice(&sha2::Sha256::digest(&data));
    let (sent, size) = (body.len(), data.len());
    match sdk
        .save_delta(&target.as_file(), body, since, base.map(|b| b.as_file()))
        .await
    {
        Ok(mtime) => {
            log::info!("delta {target} ({sent} bytes for {size})");
            Some(Saved::Done(mtime))
        }
        Err(SdkError::PreconditionFailed) => Some(Saved::Conflict),
        Err(err) => {
            log::debug!("delta {target}: {err}");
            None
        }
    }
}

async fn upload_full(
    sdk: &Sdk,
    target: &RelPath,
    source: &Path,
    since: Option<SystemTime>,
) -> io::Result<Saved> {
    let stream = crate::file_stream(source).await?;
    match sdk.save(&target.as_file(), stream, since).await {
        Ok(mtime) => Ok(Saved::Done(mtime)),
        Err(SdkError::PreconditionFailed) => Ok(Saved::Conflict),
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
                Ok(mtime) => Ok(Saved::Done(mtime)),
                Err(SdkError::PreconditionFailed) => Ok(Saved::Conflict),
                Err(err) => Err(io_err(err)),
            }
        }
        Err(err) => Err(io_err(err)),
    }
}
