use std::io;
use std::sync::Weak;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::engine::{Engine, Replayed};
use crate::port::LocalTree;

const CONCURRENCY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadStatus {
    Idle,
    Busy,
    Error,
}

pub(crate) enum Msg {
    Kick,
    Flush(oneshot::Sender<()>),
}

pub(crate) async fn run<T: LocalTree>(
    engine: Weak<Engine<T>>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    status: watch::Sender<UploadStatus>,
) {
    let mut running: JoinSet<(i64, io::Result<Replayed>)> = JoinSet::new();
    let mut flushes: Vec<oneshot::Sender<()>> = Vec::new();
    let mut failing = false;
    loop {
        let deadline = {
            let Some(engine) = engine.upgrade() else {
                return;
            };
            engine.journal_tick(false);
            while running.len() < CONCURRENCY {
                let Some((seq, intent)) = engine.next_runnable() else {
                    break;
                };
                let engine = engine.clone();
                running.spawn(async move {
                    let result = engine.replay(&intent).await;
                    (seq, result)
                });
            }
            let idle = engine.journal_idle() && running.is_empty();
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
            engine.journal_wait()
        };
        tokio::select! {
            msg = rx.recv() => match msg {
                None => break,
                Some(Msg::Kick) => {}
                Some(Msg::Flush(reply)) => {
                    if let Some(engine) = engine.upgrade() {
                        engine.expedite();
                    }
                    flushes.push(reply);
                }
            },
            Some(joined) = running.join_next(), if !running.is_empty() => {
                let Ok((seq, result)) = joined else {
                    log::error!("a replay task panicked");
                    continue;
                };
                let succeeded = result.is_ok();
                if let Some(engine) = engine.upgrade() {
                    if engine.finished(seq, result) {
                        failing = true;
                    } else if succeeded {
                        failing = false;
                    }
                }
            },
            _ = tokio::time::sleep_until(deadline.map(Instant::from_std).unwrap_or_else(Instant::now)), if deadline.is_some() => {}
        }
    }
}
