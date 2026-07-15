use std::collections::HashMap;
use std::io;
use std::sync::Weak;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use super::{Engine, Outcome};
use crate::port::LocalTree;

const CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStatus {
    Idle,
    Busy,
    Error,
}

enum Msg {
    Kick,
    Flush(oneshot::Sender<()>),
}

pub(super) struct Handle {
    queue: mpsc::UnboundedSender<Msg>,
    status: watch::Receiver<UploadStatus>,
}

impl Handle {
    pub(super) fn kick(&self) {
        let _ = self.queue.send(Msg::Kick);
    }

    pub(super) async fn flush(&self, timeout: Duration) {
        let (reply, done) = oneshot::channel();
        if self.queue.send(Msg::Flush(reply)).is_ok() {
            let _ = tokio::time::timeout(timeout, done).await;
        }
    }

    pub(super) fn status(&self) -> watch::Receiver<UploadStatus> {
        self.status.clone()
    }
}

pub(super) struct Driver {
    rx: mpsc::UnboundedReceiver<Msg>,
    status: watch::Sender<UploadStatus>,
}

impl Driver {
    pub(super) fn spawn<T: LocalTree>(self, rt: &tokio::runtime::Handle, engine: Weak<Engine<T>>) {
        rt.spawn(run(engine, self.rx, self.status));
    }
}

pub(super) fn prepare() -> (Handle, Driver) {
    let (queue, rx) = mpsc::unbounded_channel();
    let (status_tx, status) = watch::channel(UploadStatus::Idle);
    (Handle { queue, status }, Driver { rx, status: status_tx })
}

async fn run<T: LocalTree>(
    engine: Weak<Engine<T>>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    status: watch::Sender<UploadStatus>,
) {
    let mut running: JoinSet<(i64, Outcome)> = JoinSet::new();
    let mut spawned: HashMap<tokio::task::Id, i64> = HashMap::new();
    let mut flushes: Vec<oneshot::Sender<()>> = Vec::new();
    let mut failing = false;
    loop {
        let deadline = {
            let Some(engine) = engine.upgrade() else {
                return;
            };
            engine.compact(false);
            while running.len() < CONCURRENCY {
                let Some((seq, plan)) = engine.next() else {
                    break;
                };
                let engine = engine.clone();
                let handle = running.spawn(async move {
                    let result = engine.replay(&plan).await;
                    (seq, result)
                });
                spawned.insert(handle.id(), seq);
            }
            let idle = engine.idle() && running.is_empty();
            if idle {
                for reply in flushes.drain(..) {
                    let _ = reply.send(());
                }
            }
            let _ = status.send(match (failing, idle) {
                (true, _) => UploadStatus::Error,
                (false, true) => UploadStatus::Idle,
                (false, false) => UploadStatus::Busy,
            });
            engine.wait()
        };
        tokio::select! {
            msg = rx.recv() => match msg {
                None => break,
                Some(Msg::Kick) => {}
                Some(Msg::Flush(reply)) => {
                    if let Some(engine) = engine.upgrade() {
                        engine.rush();
                    }
                    flushes.push(reply);
                }
            },
            Some(joined) = running.join_next_with_id(), if !running.is_empty() => {
                let (seq, outcome) = match joined {
                    Ok((id, (seq, outcome))) => {
                        spawned.remove(&id);
                        (seq, outcome)
                    }
                    Err(err) => {
                        let Some(seq) = spawned.remove(&err.id()) else {
                            continue;
                        };
                        (seq, Outcome::Failed(io::Error::other("replay panicked")))
                    }
                };
                if let Some(engine) = engine.upgrade() {
                    failing = engine.settle(seq, outcome);
                }
            },
            _ = tokio::time::sleep_until(deadline.map(Instant::from_std).unwrap_or_else(Instant::now)), if deadline.is_some() => {}
        }
    }
}
