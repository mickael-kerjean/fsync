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

pub(crate) struct Handle {
    queue: mpsc::UnboundedSender<Msg>,
    status: watch::Receiver<UploadStatus>,
}

impl Handle {
    pub(crate) fn kick(&self) {
        let _ = self.queue.send(Msg::Kick);
    }

    pub(crate) async fn flush(&self, timeout: Duration) {
        let (reply, done) = oneshot::channel();
        if self.queue.send(Msg::Flush(reply)).is_ok() {
            let _ = tokio::time::timeout(timeout, done).await;
        }
    }

    pub(crate) fn status(&self) -> watch::Receiver<UploadStatus> {
        self.status.clone()
    }
}

pub(crate) fn spawn<T: LocalTree>(rt: &tokio::runtime::Handle, engine: Weak<Engine<T>>) -> Handle {
    let (queue, rx) = mpsc::unbounded_channel();
    let (status_tx, status) = watch::channel(UploadStatus::Idle);
    rt.spawn(run(engine, rx, status_tx));
    Handle { queue, status }
}

async fn run<T: LocalTree>(
    engine: Weak<Engine<T>>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    status: watch::Sender<UploadStatus>,
) {
    let mut running: JoinSet<(i64, Outcome)> = JoinSet::new();
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
                running.spawn(async move {
                    let result = engine.replay(&plan).await;
                    (seq, result)
                });
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
            Some(joined) = running.join_next(), if !running.is_empty() => {
                let Ok((seq, outcome)) = joined else {
                    log::error!("a replay task panicked");
                    continue;
                };
                if let Some(engine) = engine.upgrade() {
                    failing = engine.settle(seq, outcome);
                }
            },
            _ = tokio::time::sleep_until(deadline.map(Instant::from_std).unwrap_or_else(Instant::now)), if deadline.is_some() => {}
        }
    }
}
