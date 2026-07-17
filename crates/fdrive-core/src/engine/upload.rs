use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::{Error as SdkError, Sdk};

use super::{Engine, Outcome};
use crate::model::{Conflict, Observation, Operation};

enum Saved {
    Done(Option<SystemTime>),
    Conflict,
}

pub(super) fn signature(data: &[u8]) -> Vec<u8> {
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

    pub(super) async fn replay_save(
        &self,
        path: &RelPath,
        replaces: Option<Observation>,
        reuses: Option<&RelPath>,
    ) -> Outcome {
        let gate = self.transfers.upload_gate(path);
        let _gate = gate.lock().await;
        if self.is_frozen(path) {
            return Outcome::Busy;
        }
        let abs = self.tree.backing(path);
        let md = match fs::symlink_metadata(&abs) {
            Ok(md) if md.is_file() => md,
            Ok(_) => {
                log::warn!("upload {path}: the cache entry is not a regular file anymore");
                return Outcome::Saved {
                    obs: None,
                    sig: None,
                    reedited: false,
                };
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Outcome::Saved {
                    obs: None,
                    sig: None,
                    reedited: false,
                };
            }
            Err(err) => return Outcome::Failed(err),
        };
        let before = md.modified().ok();
        let since = UNIX_EPOCH + Duration::from_secs(replaces.map_or(0, |r| r.time));

        let attempt = match self.try_delta(path, &abs, replaces, reuses).await {
            Some(saved) => saved,
            None => match upload_full(&self.sdk, path, &abs, Some(since)).await {
                Ok(saved) => saved,
                Err(err) => return Outcome::Failed(err),
            },
        };
        match attempt {
            Saved::Conflict => self.divert(path, replaces, &abs, md.len()).await,
            Saved::Done(mtime) => {
                let obs = mtime.map(|m| Observation::new(md.len(), Some(m)));
                let sig = fs::read(&abs).ok().map(|d| signature(&d));
                let after = fs::metadata(&abs).ok().and_then(|md| md.modified().ok());
                self.tree.settled(path, after);
                log::info!("uploaded {path} ({} bytes)", md.len());
                Outcome::Saved {
                    obs,
                    sig,
                    reedited: after != before,
                }
            }
        }
    }

    async fn try_delta(
        &self,
        path: &RelPath,
        abs: &Path,
        replaces: Option<Observation>,
        reuses: Option<&RelPath>,
    ) -> Option<Saved> {
        let source = reuses.unwrap_or(path);
        let sig = self.ledger().sign_get(source)?;
        let time = match reuses {
            Some(base) => self.ledger().observations.get(base).map(|o| o.time),
            None => replaces.map(|r| r.time),
        }?;
        let since = UNIX_EPOCH + Duration::from_secs(time);
        upload_delta(&self.sdk, path, abs, sig, reuses, since).await
    }

    async fn divert(
        &self,
        path: &RelPath,
        replaces: Option<Observation>,
        abs: &Path,
        len: u64,
    ) -> Outcome {
        let theirs = match self.sdk.stat(&path.as_file()).await {
            Ok(info) => Some(Observation::of(&info)),
            Err(_) => None,
        };
        let copy = self.conflict_target(path).await;
        log::warn!("conflict on {path}: uploading as {copy}");
        let mtime = match upload_full(&self.sdk, &copy, abs, None).await {
            Ok(Saved::Done(mtime)) => mtime,
            Ok(Saved::Conflict) => {
                return Outcome::Failed(io::Error::other("conflict copy was preempted"))
            }
            Err(err) => return Outcome::Failed(err),
        };
        let after = fs::metadata(abs).ok().and_then(|md| md.modified().ok());
        if let Err(err) = self.tree.relocate(path, &copy) {
            log::warn!("move conflicted copy {path} -> {copy}: {err}");
        }
        let sig = fs::read(self.tree.backing(&copy))
            .ok()
            .map(|d| signature(&d));
        self.tree.settled(&copy, after);
        log::info!("uploaded {copy} ({len} bytes)");
        Outcome::Diverted {
            theirs,
            copy: copy.clone(),
            obs: mtime.map(|m| Observation::new(len, Some(m))),
            sig,
            conflict: Conflict::new(Operation::Write(path.clone()), replaces, theirs, Some(copy)),
        }
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
                Err(err) => Err(err.into()),
            }
        }
        Err(err) => Err(err.into()),
    }
}
