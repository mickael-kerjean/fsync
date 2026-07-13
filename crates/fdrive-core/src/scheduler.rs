use std::collections::{BTreeMap, BTreeSet};
use std::sync::Weak;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant};

use crate::engine::{Engine, Upload};
use crate::path::RelPath;
use crate::port::LocalTree;

const QUIET: Duration = Duration::from_secs(5);
const GRACE: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_secs(10);
const RETRY_CAP: Duration = Duration::from_secs(300);
const CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStatus {
    Idle,
    Busy,
    Error,
}

pub(crate) enum Msg {
    Arm(RelPath),
    Now(RelPath),
    Cancel(RelPath),
    Flush(oneshot::Sender<()>),
}

pub(crate) async fn run<T: LocalTree>(
    engine: Weak<Engine<T>>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    status: watch::Sender<UploadStatus>,
) {
    let mut pending: BTreeMap<RelPath, Instant> = BTreeMap::new();
    let mut flushes: Vec<oneshot::Sender<()>> = Vec::new();
    let mut attempts: BTreeMap<RelPath, u32> = BTreeMap::new();
    let mut running: JoinSet<(RelPath, std::io::Result<Upload>)> = JoinSet::new();
    let mut inflight: BTreeSet<RelPath> = BTreeSet::new();
    let mut failing = false;
    loop {
        let now = Instant::now();
        while running.len() < CONCURRENCY {
            let due = pending
                .iter()
                .find(|(path, at)| **at <= now && !inflight.contains(*path))
                .map(|(path, _)| path.clone());
            let Some(path) = due else { break };
            pending.remove(&path);
            let Some(engine) = engine.upgrade() else {
                return;
            };
            inflight.insert(path.clone());
            running.spawn(async move {
                let result = engine.upload(&path).await;
                (path, result)
            });
        }
        if pending.is_empty() && running.is_empty() {
            for reply in flushes.drain(..) {
                let _ = reply.send(());
            }
        }
        let _ = status.send(match (failing, pending.is_empty() && running.is_empty()) {
            (true, _) => UploadStatus::Error,
            (false, true) => UploadStatus::Idle,
            (false, false) => UploadStatus::Busy,
        });
        let next = pending
            .iter()
            .filter(|(path, _)| !inflight.contains(*path))
            .map(|(_, at)| *at)
            .min();
        tokio::select! {
            msg = rx.recv() => match msg {
                None => break,
                Some(Msg::Arm(path)) => {
                    pending.insert(path, Instant::now() + QUIET);
                }
                Some(Msg::Now(path)) => {
                    pending.insert(path, Instant::now() + GRACE);
                }
                Some(Msg::Cancel(path)) => {
                    pending.retain(|p, _| p != &path && !p.is_descendant_of(&path));
                    attempts.retain(|p, _| p != &path && !p.is_descendant_of(&path));
                }
                Some(Msg::Flush(reply)) => {
                    let now = Instant::now();
                    let dirty: Vec<RelPath> = match engine.upgrade() {
                        Some(engine) => engine.ledger().dirty.iter().cloned().collect(),
                        None => break,
                    };
                    for path in dirty {
                        pending.insert(path, now);
                    }
                    if pending.is_empty() && running.is_empty() {
                        let _ = reply.send(());
                    } else {
                        flushes.push(reply);
                    }
                }
            },
            Some(joined) = running.join_next(), if !running.is_empty() => {
                let Ok((path, result)) = joined else {
                    log::error!("an upload task panicked");
                    continue;
                };
                inflight.remove(&path);
                match result {
                    Ok(Upload::Done) => {
                        attempts.remove(&path);
                        failing = false;
                    }
                    Ok(Upload::Retry) => {
                        pending.entry(path).or_insert(Instant::now() + QUIET);
                    }
                    Err(err) => {
                        let n = attempts.entry(path.clone()).or_insert(0);
                        *n += 1;
                        if *n == 1 {
                            log::error!("upload {path}: {err}");
                        } else {
                            log::warn!("upload {path} (attempt {n}): {err}");
                        }
                        let delay = RETRY
                            .saturating_mul(1u32 << (*n - 1).min(5))
                            .min(RETRY_CAP);
                        failing = true;
                        pending.entry(path).or_insert(Instant::now() + delay);
                    }
                }
            },
            _ = tokio::time::sleep_until(next.unwrap_or_else(Instant::now)), if next.is_some() => {}
        }
    }
}
