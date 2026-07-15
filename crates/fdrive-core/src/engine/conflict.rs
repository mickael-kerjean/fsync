use std::fs;
use std::io;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use tokio::sync::watch;

use crate::path::RelPath;
use crate::port::LocalTree;

use super::Engine;
use crate::model::{Conflict, Observation, Operation, Resolution};

impl Conflict {
    pub(crate) fn what(&self) -> (&'static str, &RelPath, Option<&RelPath>) {
        match &self.op {
            Operation::Create(p) | Operation::Write(p) => ("w", p, None),
            Operation::Rename(a, b) => ("mv", a, Some(b)),
            Operation::Delete(p) => ("rm", p, None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_row(
        seq: i64,
        op: &str,
        path: RelPath,
        dest: Option<RelPath>,
        expected: Option<Observation>,
        found: Option<Observation>,
        ours: Option<RelPath>,
        at: u64,
    ) -> Option<Self> {
        let op = match (op, dest) {
            ("w", _) => Operation::Write(path),
            ("mv", Some(dest)) => Operation::Rename(path, dest),
            ("rm", _) => Operation::Delete(path),
            _ => return None,
        };
        Some(Self {
            seq,
            op,
            expected,
            found,
            ours,
            at: UNIX_EPOCH + Duration::from_secs(at),
        })
    }
}

pub(super) struct Conflicts {
    list: Mutex<Vec<Conflict>>,
    tx: watch::Sender<usize>,
}

impl Conflicts {
    pub(super) fn load(list: Vec<Conflict>) -> Self {
        let (tx, _) = watch::channel(list.len());
        Self {
            list: Mutex::new(list),
            tx,
        }
    }

    fn add(&self, c: Conflict) {
        self.list.lock().unwrap().push(c);
        self.tx.send_modify(|n| *n += 1);
    }

    fn take(&self, seq: i64) -> Option<Conflict> {
        let mut list = self.list.lock().unwrap();
        let idx = list.iter().position(|c| c.seq == seq)?;
        let c = list.remove(idx);
        drop(list);
        self.tx.send_modify(|n| *n += 1);
        Some(c)
    }

    fn all(&self) -> Vec<Conflict> {
        self.list.lock().unwrap().clone()
    }

    fn watch(&self) -> watch::Receiver<usize> {
        self.tx.subscribe()
    }
}

impl<T: LocalTree> Engine<T> {
    pub(crate) fn conflicted(&self, mut c: Conflict) {
        log::warn!("conflict on {}", c.op);
        c.seq = self.ledger().conflict_add(&c);
        self.conflicts.add(c);
    }

    pub fn conflicts(&self) -> Vec<Conflict> {
        self.conflicts.all()
    }

    pub fn conflict_watch(&self) -> watch::Receiver<usize> {
        self.conflicts.watch()
    }

    pub fn resolve(&self, seq: i64, resolution: Resolution) -> io::Result<()> {
        let conflict = self.conflicts.take(seq).ok_or(io::ErrorKind::NotFound)?;
        self.ledger().conflict_retire(seq);
        match resolution {
            Resolution::Both => {}
            Resolution::Theirs => {
                if let Some(ours) = &conflict.ours {
                    let _ = fs::remove_file(self.tree.backing(ours));
                    self.record(Operation::Delete(ours.clone()));
                }
            }
            Resolution::Ours => {
                if let (Some(found), Operation::Write(path) | Operation::Delete(path)) =
                    (conflict.found, &conflict.op)
                {
                    self.ledger().observe(path, found);
                }
                match (&conflict.ours, &conflict.op) {
                    (Some(ours), Operation::Write(path)) => {
                        self.tree.relocate(ours, path)?;
                        self.record(Operation::Rename(ours.clone(), path.clone()));
                    }
                    _ => {
                        self.record(conflict.op.clone());
                    }
                }
            }
        }
        Ok(())
    }
}
