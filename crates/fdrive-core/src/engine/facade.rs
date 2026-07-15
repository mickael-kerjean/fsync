use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::model::{Operation, Plan};
use crate::path::RelPath;
use crate::port::LocalTree;
use crate::sdk::{Error as SdkError, Sdk};

use super::conflict::Conflicts;
use super::gates::Transfers;
use super::spawner::Spawner;
use super::state::{LedgerGuard, State};
use super::{scheduler, Engine, Frozen, Outcome, UploadStatus};

impl<T: LocalTree> Engine<T> {
    pub fn start(sdk: Arc<Sdk>, rt: tokio::runtime::Handle, tree: T) -> Arc<Self> {
        let ledger_file = tree.ledger();
        let ignore = crate::config::ignore(ledger_file.parent().unwrap_or(Path::new("")));
        let state = State::open(&ledger_file);
        let conflicts = Conflicts::load(state.ledger.conflicts_load());
        Arc::new_cyclic(|weak| Self {
            tree,
            sdk,
            ignore,
            state: Mutex::new(state),
            transfers: Transfers::default(),
            frozen: Mutex::new(BTreeSet::new()),
            conflicts,
            scheduler: scheduler::spawn(&rt, weak.clone()),
            spawner: Spawner {
                rt,
                weak: weak.clone(),
            },
        })
    }

    pub fn created(&self, path: &RelPath) {
        self.record(Operation::Create(path.clone()));
    }

    pub fn modified(&self, path: &RelPath) {
        self.record(Operation::Write(path.clone()));
    }

    pub async fn delete(&self, path: &RelPath, is_dir: bool) -> io::Result<()> {
        if is_dir {
            self.flush(Duration::from_secs(60)).await;
            let _frozen = self.freeze(&[path]);
            self.wait_uploads(path, true).await;
            match self.sdk.rm(&path.as_dir()).await {
                Ok(()) | Err(SdkError::NotFound) => {}
                Err(err) => return Err(err.into()),
            }
            self.ledger().forget(path);
            log::info!("deleted {path}/");
            return Ok(());
        }
        self.record(Operation::Delete(path.clone()));
        Ok(())
    }

    pub async fn rename(&self, from: &RelPath, to: &RelPath, is_dir: bool) -> io::Result<()> {
        if is_dir {
            self.flush(Duration::from_secs(60)).await;
            let _frozen = self.freeze(&[from, to]);
            self.wait_uploads(from, true).await;
            self.wait_uploads(to, false).await;
            match self.sdk.mv(&from.as_dir(), &to.as_dir()).await {
                Ok(()) | Err(SdkError::NotFound) => {}
                Err(err) => return Err(err.into()),
            }
            self.ledger().remap(from, to);
            log::info!("renamed {from}/ -> {to}/");
            return Ok(());
        }
        self.record(Operation::Rename(from.clone(), to.clone()));
        Ok(())
    }

    pub async fn flush(&self, timeout: Duration) {
        self.scheduler.flush(timeout).await;
    }

    pub fn released(&self, path: &RelPath) {
        if self.ledger().dirty.contains(path) {
            self.kick();
        }
    }

    pub fn write_opened(&self, path: &RelPath) {
        self.state().write_opened(path);
    }

    pub fn write_closed(&self, path: &RelPath) {
        self.state().write_closed(path);
        self.kick();
    }

    pub fn upload_status(&self) -> watch::Receiver<UploadStatus> {
        self.scheduler.status()
    }

    pub fn recover(&self) {
        let pending = self.state().journal.pending.len();
        if pending > 0 {
            log::info!("recovered {pending} pending plans");
        }
        self.kick();
        self.pin_sweep();
    }

    pub(crate) fn compact(&self, force: bool) {
        self.state().compact(force, &self.ignore, |p| {
            fs::metadata(self.tree.backing(p)).is_ok_and(|md| md.len() == 0)
        });
    }

    pub(crate) fn next(&self) -> Option<(i64, Plan)> {
        self.state().next()
    }

    pub(crate) fn settle(&self, seq: i64, outcome: Outcome) -> bool {
        let (failing, conflict) = self.state().settle(seq, outcome);
        if let Some(c) = conflict {
            self.conflicted(c);
        }
        failing
    }

    pub(crate) fn idle(&self) -> bool {
        self.state().idle()
    }

    pub(crate) fn wait(&self) -> Option<Instant> {
        self.state().wait()
    }

    pub(crate) fn rush(&self) {
        self.compact(true);
        self.state().rush();
    }

    pub(super) fn record(&self, op: Operation) {
        self.state().record(op);
        self.kick();
    }

    pub(super) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn kick(&self) {
        self.scheduler.kick();
    }

    fn freeze(&self, paths: &[&RelPath]) -> Frozen<'_> {
        let paths: Vec<RelPath> = paths.iter().map(|p| (*p).clone()).collect();
        let mut set = self.frozen.lock().unwrap();
        for path in &paths {
            set.insert(path.clone());
        }
        Frozen {
            set: &self.frozen,
            paths,
        }
    }

    pub(crate) fn is_frozen(&self, path: &RelPath) -> bool {
        self.frozen
            .lock()
            .unwrap()
            .iter()
            .any(|p| path == p || path.is_descendant_of(p))
    }

    async fn wait_uploads(&self, path: &RelPath, subtree: bool) {
        let gates: Vec<Arc<tokio::sync::Mutex<()>>> = self
            .transfers
            .uploading
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| *p == path || (subtree && p.is_descendant_of(path)))
            .map(|(_, gate)| gate.clone())
            .collect();
        for gate in gates {
            let _gate = gate.lock().await;
        }
    }

    pub fn sdk(&self) -> &Arc<Sdk> {
        &self.sdk
    }

    pub fn rt(&self) -> &tokio::runtime::Handle {
        &self.spawner.rt
    }

    pub fn tree(&self) -> &T {
        &self.tree
    }

    pub fn ledger(&self) -> LedgerGuard<'_> {
        LedgerGuard(self.state())
    }
}
