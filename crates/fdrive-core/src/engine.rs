use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot, watch};

use crate::path::RelPath;
use crate::port::LocalTree;
use crate::scheduler::{self, Msg, UploadStatus};
use crate::sdk::{Error as SdkError, FileInfo, Sdk};

#[path = "engine_ledger.rs"]
mod ledger;
pub use ledger::{Ledger, Observation};

#[path = "engine_download.rs"]
mod download;
pub use download::Download;

#[path = "engine_upload.rs"]
mod upload;

#[path = "engine_journal.rs"]
mod journal;
pub use journal::{Intent, Operation};

#[cfg(test)]
#[path = "engine_test.rs"]
mod tests;

const WINDOW_QUIET: Duration = Duration::from_millis(250);
const WINDOW_MAX: Duration = Duration::from_secs(2);
const OPEN_QUIET: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(10);
const RETRY_CAP: Duration = Duration::from_secs(300);

pub struct Engine<T: LocalTree> {
    sdk: Arc<Sdk>,
    rt: tokio::runtime::Handle,
    ledger: Mutex<Ledger>,
    tree: T,
    ignore: crate::config::Ignore,
    unreadable: AtomicBool,
    queue: mpsc::UnboundedSender<Msg>,
    status: watch::Receiver<UploadStatus>,
    hydrating: Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
    downloads: Mutex<HashMap<RelPath, Arc<Download>>>,
    uploading: Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
    journal: Mutex<Journal>,
    conflicts: Mutex<Vec<Conflict>>,
    conflicts_tx: watch::Sender<usize>,
    frozen: Mutex<BTreeSet<RelPath>>,
    weak: Weak<Engine<T>>,
}

struct Journal {
    window: Vec<(Instant, Operation)>,
    marks: BTreeSet<RelPath>,
    writing: BTreeMap<RelPath, usize>,
    pending: BTreeMap<i64, Pend>,
    inflight: BTreeSet<i64>,
}

struct Pend {
    intent: Intent,
    attempts: u32,
    due: Instant,
}

impl Journal {
    fn load(ledger: &Ledger) -> Self {
        let (recovered, intents) = ledger.journal_load();
        let now = Instant::now();
        Self {
            marks: recovered.iter().flat_map(op_paths).cloned().collect(),
            window: recovered.into_iter().map(|op| (now, op)).collect(),
            writing: BTreeMap::new(),
            pending: intents
                .into_iter()
                .map(|(seq, intent)| {
                    (
                        seq,
                        Pend {
                            intent,
                            attempts: 0,
                            due: now,
                        },
                    )
                })
                .collect(),
            inflight: BTreeSet::new(),
        }
    }

    fn drain(&mut self, force: bool, stalled: impl Fn(&RelPath) -> bool) -> Vec<Operation> {
        let now = Instant::now();
        let quiet = self
            .window
            .last()
            .is_some_and(|(at, _)| now - *at >= WINDOW_QUIET);
        let aged = self
            .window
            .first()
            .is_some_and(|(at, _)| now - *at >= WINDOW_MAX);
        if self.window.is_empty() || !(force || quiet || aged) {
            return Vec::new();
        }
        let inflight: Vec<Intent> = self
            .inflight
            .iter()
            .filter_map(|s| self.pending.get(s).map(|p| p.intent.clone()))
            .collect();
        let mut held: Vec<(Instant, Operation)> = Vec::new();
        let mut held_paths: Vec<RelPath> = Vec::new();
        let mut drained: Vec<Operation> = Vec::new();
        for (at, op) in std::mem::take(&mut self.window) {
            let age = now - at;
            let blocked = op_paths(&op).iter().any(|p| {
                inflight.iter().any(|i| i.touches(p))
                    || held_paths
                        .iter()
                        .any(|h| h == *p || p.is_descendant_of(h) || h.is_descendant_of(p))
                    || (!force && age < OPEN_QUIET && self.writing.contains_key(*p))
                    || (!force && age < WINDOW_MAX && stalled(p))
            });
            if blocked {
                held_paths.extend(op_paths(&op).into_iter().cloned());
                held.push((at, op));
            } else {
                drained.push(op);
            }
        }
        self.window = held;
        drained
    }

    fn fold(&mut self, drained: &[Operation]) -> Vec<(i64, Intent)> {
        let mut seeds: Vec<(i64, Intent)> = Vec::new();
        loop {
            let next = self.pending.iter().find(|(seq, pend)| {
                !self.inflight.contains(seq)
                    && !seeds.iter().any(|(s, _)| s == *seq)
                    && (drained
                        .iter()
                        .flat_map(op_paths)
                        .any(|p| pend.intent.touches(p))
                        || seeds.iter().any(|(_, i)| i.overlaps(&pend.intent)))
            });
            match next {
                Some((seq, _)) => {
                    let seq = *seq;
                    seeds.push((seq, self.pending.remove(&seq).unwrap().intent));
                }
                None => return seeds,
            }
        }
    }

    fn admit(&mut self, rows: Vec<(i64, Intent)>) {
        let now = Instant::now();
        for (seq, intent) in rows {
            self.pending.insert(
                seq,
                Pend {
                    intent,
                    attempts: 0,
                    due: now,
                },
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Fate {
    Gone,
    Arrived { from: RelPath, was: Observation },
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub seq: i64,
    pub op: Operation,
    pub expected: Option<Observation>,
    pub found: Option<Observation>,
    pub ours: Option<RelPath>,
    pub at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Resolution {
    Ours,
    Theirs,
    Both,
}

impl Conflict {
    pub(crate) fn new(
        op: Operation,
        expected: Option<Observation>,
        found: Option<Observation>,
        ours: Option<RelPath>,
    ) -> Self {
        Self {
            seq: 0,
            op,
            expected,
            found,
            ours,
            at: SystemTime::now(),
        }
    }

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

pub(crate) fn gate(
    gates: &Mutex<HashMap<RelPath, Arc<tokio::sync::Mutex<()>>>>,
    path: &RelPath,
) -> Arc<tokio::sync::Mutex<()>> {
    let mut gates = gates.lock().unwrap();
    gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    gates.entry(path.clone()).or_default().clone()
}

pub(crate) struct Frozen<'a> {
    set: &'a Mutex<BTreeSet<RelPath>>,
    paths: Vec<RelPath>,
}

impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        let mut set = self.set.lock().unwrap();
        for path in &self.paths {
            set.remove(path);
        }
    }
}

pub(crate) enum Replayed {
    Done,
    Busy,
}

fn op_paths(op: &Operation) -> Vec<&RelPath> {
    match op {
        Operation::Create(p) | Operation::Write(p) | Operation::Delete(p) => vec![p],
        Operation::Rename(a, b) => vec![a, b],
    }
}

impl<T: LocalTree> Engine<T> {
    pub fn spawn(sdk: Arc<Sdk>, rt: tokio::runtime::Handle, tree: T) -> Arc<Self> {
        let ledger_file = tree.ledger();
        let ignore = crate::config::ignore(ledger_file.parent().unwrap_or(Path::new("")));
        let (ledger, unreadable) = match Ledger::open(&ledger_file) {
            Ok(ledger) => (ledger, false),
            Err(()) => {
                let _ = fs::remove_file(&ledger_file);
                (Ledger::open(&ledger_file).unwrap_or_default(), true)
            }
        };
        let journal = Journal::load(&ledger);
        let conflicts = ledger.conflicts_load();
        let (queue, rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(UploadStatus::Idle);
        let (conflicts_tx, _) = watch::channel(conflicts.len());
        Arc::new_cyclic(|weak| {
            rt.spawn(scheduler::run(weak.clone(), rx, status_tx));
            Self {
                sdk,
                rt: rt.clone(),
                ledger: Mutex::new(ledger),
                tree,
                ignore,
                unreadable: AtomicBool::new(unreadable),
                queue,
                status,
                hydrating: Mutex::new(HashMap::new()),
                downloads: Mutex::new(HashMap::new()),
                uploading: Mutex::new(HashMap::new()),
                journal: Mutex::new(journal),
                conflicts: Mutex::new(conflicts),
                conflicts_tx,
                frozen: Mutex::new(BTreeSet::new()),
                weak: weak.clone(),
            }
        })
    }

    fn kick(&self) {
        let _ = self.queue.send(Msg::Kick);
    }

    pub async fn flush(&self, timeout: Duration) {
        let (reply, done) = oneshot::channel();
        if self.queue.send(Msg::Flush(reply)).is_ok() {
            let _ = tokio::time::timeout(timeout, done).await;
        }
    }

    pub fn upload_status(&self) -> watch::Receiver<UploadStatus> {
        self.status.clone()
    }

    pub fn recover(&self) {
        let pending = self.journal.lock().unwrap().pending.len();
        if pending > 0 {
            log::info!("recovered {pending} pending intents");
        }
        self.kick();
        self.pin_sweep();
    }

    pub fn pin(&self, path: &RelPath) {
        self.ledger().pin_set(path);
        log::info!("pinned {path}");
        self.pin_sweep();
    }

    pub fn unpin(&self, path: &RelPath) {
        self.ledger().pin_clear(path);
        log::info!("unpinned {path}");
    }

    pub fn pinned(&self, path: &RelPath) -> bool {
        self.ledger()
            .pins
            .iter()
            .any(|p| path == p || path.is_descendant_of(p))
    }

    fn pin_sweep(&self) {
        let Some(engine) = self.weak.upgrade() else {
            return;
        };
        self.rt.spawn(async move {
            let roots: Vec<RelPath> = engine.ledger().pins.iter().cloned().collect();
            for root in roots {
                engine.hydrate_subtree(&root).await;
            }
        });
    }

    async fn hydrate_subtree(&self, root: &RelPath) {
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            let listing = match self.sdk.ls(&dir.as_dir()).await {
                Ok(listing) => listing,
                Err(_) if dir == *root => {
                    if let Err(err) = self.hydrate(root, None).await {
                        log::debug!("pin {root}: {err}");
                    }
                    return;
                }
                Err(err) => {
                    log::debug!("pin {dir}: {err}");
                    continue;
                }
            };
            self.listed(&dir, &listing);
            for entry in listing {
                let child = dir.join(&entry.name);
                if child.parent_or_root() != dir {
                    continue;
                }
                match entry.kind {
                    crate::sdk::FileType::Directory => dirs.push(child),
                    crate::sdk::FileType::File => {
                        let hint = Observation::of(&entry);
                        if self.content_current(&child, hint) {
                            continue;
                        }
                        if let Err(err) = self.hydrate(&child, Some(hint)).await {
                            log::debug!("pin {child}: {err}");
                        }
                    }
                }
            }
        }
    }

    fn record(&self, op: Operation) {
        let mark = {
            let mut j = self.journal.lock().unwrap();
            if let Some(last) = j.window.last_mut() {
                if last.1 == op {
                    last.0 = Instant::now();
                    return;
                }
            }
            let mark = match &op {
                Operation::Create(p) | Operation::Write(p) if !j.marks.contains(p) => {
                    j.marks.insert(p.clone());
                    Some(p.clone())
                }
                _ => None,
            };
            j.window.push((Instant::now(), op.clone()));
            mark
        };
        {
            let mut ledger = self.ledger();
            if let Some(path) = &mark {
                ledger.mark(path);
            }
            match &op {
                Operation::Create(p) | Operation::Write(p) => {
                    ledger.dirty.insert(p.clone());
                }
                Operation::Rename(a, b) => {
                    if ledger.dirty.remove(a) {
                        ledger.dirty.insert(b.clone());
                    }
                }
                Operation::Delete(p) => {
                    ledger.dirty.remove(p);
                }
            }
        }
        self.kick();
    }

    pub fn modified(&self, path: &RelPath) {
        self.record(Operation::Write(path.clone()));
    }

    pub fn created(&self, path: &RelPath) {
        self.record(Operation::Create(path.clone()));
    }

    pub fn released(&self, path: &RelPath) {
        if self.ledger().dirty.contains(path) {
            self.kick();
        }
    }

    pub fn write_opened(&self, path: &RelPath) {
        let mut j = self.journal.lock().unwrap();
        *j.writing.entry(path.clone()).or_insert(0) += 1;
    }

    pub fn write_closed(&self, path: &RelPath) {
        let mut j = self.journal.lock().unwrap();
        if let Some(n) = j.writing.get_mut(path) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                j.writing.remove(path);
            }
        }
        drop(j);
        self.kick();
    }

    pub async fn delete(&self, path: &RelPath, is_dir: bool) -> io::Result<()> {
        if is_dir {
            self.flush(Duration::from_secs(60)).await;
            let _frozen = self.freeze(&[path]);
            self.drain(path, true).await;
            match self.sdk.rm(&path.as_dir()).await {
                Ok(()) | Err(SdkError::NotFound) => {}
                Err(err) => return Err(io_err(err)),
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
            self.drain(from, true).await;
            self.drain(to, false).await;
            match self.sdk.mv(&from.as_dir(), &to.as_dir()).await {
                Ok(()) | Err(SdkError::NotFound) => {}
                Err(err) => return Err(io_err(err)),
            }
            self.ledger().remap(from, to);
            log::info!("renamed {from}/ -> {to}/");
            return Ok(());
        }
        self.record(Operation::Rename(from.clone(), to.clone()));
        Ok(())
    }

    pub(crate) fn journal_tick(&self, force: bool) {
        let (drained, seeds) = {
            let mut j = self.journal.lock().unwrap();
            let drained = j.drain(force, |p| {
                self.ledger()
                    .observations
                    .get(p)
                    .is_some_and(|o| o.size > 0)
                    && fs::metadata(self.tree.backing(p)).is_ok_and(|md| md.len() == 0)
            });
            if drained.is_empty() {
                return;
            }
            let seeds = j.fold(&drained);
            (drained, seeds)
        };
        let mut kept: Vec<RelPath> = Vec::new();
        let made: Vec<Intent> = {
            let ledger = self.ledger();
            journal::coalesce(journal::seed(seeds.iter().map(|(_, i)| i)), &drained, |p| {
                ledger.observations.get(p).copied()
            })
        }
        .into_iter()
        .filter(|intent| match intent {
            Intent::Save { path, .. } if self.ignore.matches(path) => {
                kept.push(path.clone());
                false
            }
            _ => true,
        })
        .collect();
        log::info!(
            "journal [{}] -> [{}]",
            journal::render(&drained),
            journal::render(&made)
        );
        let unmark: Vec<RelPath> = drained
            .iter()
            .flat_map(op_paths)
            .filter(|p| !kept.contains(p))
            .cloned()
            .collect();
        let retired: Vec<i64> = seeds.iter().map(|(seq, _)| *seq).collect();
        let rows = self.ledger().journal_swap(&unmark, &retired, &made);
        {
            let mut j = self.journal.lock().unwrap();
            for p in &unmark {
                j.marks.remove(p);
            }
            j.admit(rows);
        }
        self.refresh();
    }

    pub(crate) fn next_runnable(&self) -> Option<(i64, Intent)> {
        let mut j = self.journal.lock().unwrap();
        let now = Instant::now();
        let mut pick: Option<i64> = None;
        'candidates: for (seq, pend) in &j.pending {
            if j.inflight.contains(seq) || pend.due > now {
                continue;
            }
            for (_, earlier) in j.pending.range(..seq) {
                if earlier.intent.overlaps(&pend.intent) {
                    continue 'candidates;
                }
            }
            pick = Some(*seq);
            break;
        }
        let seq = pick?;
        j.inflight.insert(seq);
        Some((seq, j.pending.get(&seq).unwrap().intent.clone()))
    }

    pub(crate) fn finished(&self, seq: i64, result: io::Result<Replayed>) -> bool {
        let mut j = self.journal.lock().unwrap();
        j.inflight.remove(&seq);
        match result {
            Ok(Replayed::Done) => {
                j.pending.remove(&seq);
                drop(j);
                self.ledger().journal_retire(seq);
                self.refresh();
                false
            }
            Ok(Replayed::Busy) => {
                if let Some(pend) = j.pending.get_mut(&seq) {
                    pend.due = Instant::now() + Duration::from_secs(1);
                }
                false
            }
            Err(err) => {
                if let Some(pend) = j.pending.get_mut(&seq) {
                    pend.attempts += 1;
                    let n = pend.attempts;
                    if n == 1 {
                        log::error!("replay {}: {err}", pend.intent);
                    } else {
                        log::warn!("replay {} (attempt {n}): {err}", pend.intent);
                    }
                    pend.due = Instant::now()
                        + RETRY.saturating_mul(1u32 << (n - 1).min(5)).min(RETRY_CAP);
                }
                true
            }
        }
    }

    pub(crate) fn journal_idle(&self) -> bool {
        let j = self.journal.lock().unwrap();
        j.window.is_empty() && j.pending.is_empty()
    }

    pub(crate) fn journal_wait(&self) -> Option<Instant> {
        let j = self.journal.lock().unwrap();
        let window = j.window.last().map(|(at, _)| *at + WINDOW_QUIET);
        let due = j
            .pending
            .iter()
            .filter(|(seq, _)| !j.inflight.contains(seq))
            .map(|(_, p)| p.due)
            .min();
        match (window, due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(crate) fn expedite(&self) {
        self.journal_tick(true);
        let mut j = self.journal.lock().unwrap();
        let now = Instant::now();
        for pend in j.pending.values_mut() {
            pend.due = now;
        }
    }

    pub(crate) async fn replay(&self, intent: &Intent) -> io::Result<Replayed> {
        match intent {
            Intent::Save {
                path,
                replaces,
                reuses,
            } => self.replay_save(path, *replaces, reuses.as_ref()).await,
            Intent::Move { from, to, moves } => self.replay_move(from, to, *moves).await,
            Intent::Remove { path, removes } => self.replay_remove(path, *removes).await,
        }
    }

    async fn replay_move(
        &self,
        from: &RelPath,
        to: &RelPath,
        moves: Observation,
    ) -> io::Result<Replayed> {
        if self.is_frozen(from) || self.is_frozen(to) {
            return Ok(Replayed::Busy);
        }
        match self.sdk.stat(&from.as_file()).await {
            Ok(info) if Observation::of(&info) == moves => {
                match self.sdk.mv(&from.as_file(), &to.as_file()).await {
                    Ok(()) => {
                        self.ledger().remap(from, to);
                        log::info!("moved {from} -> {to}");
                        Ok(Replayed::Done)
                    }
                    Err(SdkError::NotFound) => {
                        self.vanished_move(from, to).await;
                        Ok(Replayed::Done)
                    }
                    Err(err) => Err(io_err(err)),
                }
            }
            Ok(info) => {
                let found = Observation::of(&info);
                self.ledger().observe(from, found);
                self.conflicted(Conflict::new(
                    Operation::Rename(from.clone(), to.clone()),
                    Some(moves),
                    Some(found),
                    None,
                ));
                Ok(Replayed::Done)
            }
            Err(SdkError::NotFound) => {
                self.vanished_move(from, to).await;
                Ok(Replayed::Done)
            }
            Err(err) => Err(io_err(err)),
        }
    }

    async fn vanished_move(&self, from: &RelPath, to: &RelPath) {
        self.ledger().unobserve(from);
        if self.tree.backing(to).is_file() {
            self.record(Operation::Write(to.clone()));
            return;
        }
        self.conflicted(Conflict::new(
            Operation::Rename(from.clone(), to.clone()),
            None,
            None,
            None,
        ));
    }

    async fn replay_remove(&self, path: &RelPath, removes: Observation) -> io::Result<Replayed> {
        if self.is_frozen(path) {
            return Ok(Replayed::Busy);
        }
        match self.sdk.stat(&path.as_file()).await {
            Err(SdkError::NotFound) => {
                self.ledger().forget(path);
                Ok(Replayed::Done)
            }
            Ok(info) if Observation::of(&info) == removes => {
                match self.sdk.rm(&path.as_file()).await {
                    Ok(()) | Err(SdkError::NotFound) => {
                        self.ledger().forget(path);
                        log::info!("removed {path}");
                        Ok(Replayed::Done)
                    }
                    Err(err) => Err(io_err(err)),
                }
            }
            Ok(info) => {
                let found = Observation::of(&info);
                self.ledger().observe(path, found);
                self.conflicted(Conflict::new(
                    Operation::Delete(path.clone()),
                    Some(removes),
                    Some(found),
                    None,
                ));
                Ok(Replayed::Done)
            }
            Err(err) => Err(io_err(err)),
        }
    }

    pub(crate) fn conflicted(&self, mut c: Conflict) {
        log::warn!("conflict on {}", c.op);
        c.seq = self.ledger().conflict_add(&c);
        self.conflicts.lock().unwrap().push(c);
        self.conflicts_tx.send_modify(|n| *n += 1);
    }

    pub fn conflicts(&self) -> Vec<Conflict> {
        self.conflicts.lock().unwrap().clone()
    }

    pub fn conflict_watch(&self) -> watch::Receiver<usize> {
        self.conflicts_tx.subscribe()
    }

    pub fn resolve(&self, seq: i64, resolution: Resolution) -> io::Result<()> {
        let conflict = {
            let mut conflicts = self.conflicts.lock().unwrap();
            let idx = conflicts
                .iter()
                .position(|c| c.seq == seq)
                .ok_or(io::ErrorKind::NotFound)?;
            conflicts.remove(idx)
        };
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

    fn view(&self) -> (BTreeMap<RelPath, Fate>, BTreeSet<RelPath>) {
        let (pending, window, mut dirty): (Vec<Intent>, Vec<Operation>, BTreeSet<RelPath>) = {
            let j = self.journal.lock().unwrap();
            (
                j.pending.values().map(|p| p.intent.clone()).collect(),
                j.window.iter().map(|(_, op)| op.clone()).collect(),
                j.marks.clone(),
            )
        };
        let mut fates: BTreeMap<RelPath, Fate> = BTreeMap::new();
        let arrive = |fates: &mut BTreeMap<RelPath, Fate>,
                      from: &RelPath,
                      to: &RelPath,
                      was: Option<Observation>| {
            let (root, was) = match fates.get(from) {
                Some(Fate::Arrived { from: root, was }) => (root.clone(), Some(*was)),
                _ => (from.clone(), was),
            };
            fates.insert(from.clone(), Fate::Gone);
            if let Some(was) = was {
                fates.insert(to.clone(), Fate::Arrived { from: root, was });
            }
        };
        for intent in &pending {
            match intent {
                Intent::Save { path, .. } => {
                    dirty.insert(path.clone());
                }
                Intent::Move { from, to, moves } => {
                    arrive(&mut fates, from, to, Some(*moves));
                }
                Intent::Remove { path, .. } => {
                    fates.insert(path.clone(), Fate::Gone);
                }
            }
        }
        for op in &window {
            match op {
                Operation::Create(p) | Operation::Write(p) => {
                    fates.remove(p);
                    dirty.insert(p.clone());
                }
                Operation::Rename(from, to) => {
                    let was = self.ledger().observations.get(from).copied();
                    arrive(&mut fates, from, to, was);
                    if dirty.remove(from) {
                        dirty.insert(to.clone());
                    }
                }
                Operation::Delete(p) => {
                    fates.insert(p.clone(), Fate::Gone);
                    dirty.remove(p);
                }
            }
        }
        (fates, dirty)
    }

    pub fn fates(&self) -> BTreeMap<RelPath, Fate> {
        self.view().0
    }

    fn refresh(&self) {
        let dirty = self.view().1;
        self.ledger().dirty = dirty;
    }

    pub(crate) fn upstream_of(&self, path: &RelPath) -> Option<RelPath> {
        match self.fates().get(path) {
            Some(Fate::Arrived { from, .. }) => Some(from.clone()),
            _ => None,
        }
    }

    pub fn listed(&self, dir: &RelPath, entries: &[FileInfo]) {
        let fates = self.fates();
        let mut ledger = self.ledger();
        for e in entries {
            if e.kind != crate::sdk::FileType::File {
                continue;
            }
            let path = dir.join(&e.name);
            if ledger.dirty.contains(&path) || fates.contains_key(&path) {
                continue;
            }
            let obs = Observation::of(e);
            if ledger.observations.get(&path) != Some(&obs) {
                ledger.observe(&path, obs);
            }
        }
    }

    pub fn overlay(&self, dir: &RelPath, mut listing: Vec<FileInfo>) -> Vec<FileInfo> {
        let fates = self.fates();
        listing.retain(|e| {
            let path = dir.join(&e.name);
            !matches!(fates.get(&path), Some(Fate::Gone))
        });
        for (path, fate) in &fates {
            let Fate::Arrived { was, .. } = fate else {
                continue;
            };
            if path.parent_or_root() != *dir {
                continue;
            }
            let name = path.name();
            if listing.iter().any(|e| e.name == name) {
                continue;
            }
            let (size, mtime) = match fs::metadata(self.tree.backing(path)) {
                Ok(md) => (md.len(), md.modified().ok()),
                Err(_) => (was.size, Some(UNIX_EPOCH + Duration::from_secs(was.time))),
            };
            listing.push(FileInfo {
                name: name.to_string(),
                kind: crate::sdk::FileType::File,
                size: Some(size),
                mtime,
            });
        }
        let extras: Vec<RelPath> = {
            let ledger = self.ledger();
            ledger
                .dirty
                .iter()
                .filter(|p| ledger.local_only(p) && p.parent_or_root() == *dir)
                .cloned()
                .collect()
        };
        for path in extras {
            let name = path.name();
            if !listing.iter().any(|e| e.name == name) {
                if let Ok(md) = fs::metadata(self.tree.backing(&path)) {
                    listing.push(FileInfo {
                        name: name.to_string(),
                        kind: crate::sdk::FileType::File,
                        size: Some(md.len()),
                        mtime: md.modified().ok(),
                    });
                }
            }
        }
        listing
    }

    pub fn prune(&self, cache_root: &Path) -> io::Result<()> {
        if self.unreadable.swap(false, Ordering::SeqCst) {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let aside = cache_root.with_file_name(format!(
                "{}.unreadable-{stamp}",
                cache_root.file_name().unwrap_or_default().to_string_lossy()
            ));
            log::error!(
                "the ledger was unreadable; moving the cache to {} instead of pruning it",
                aside.display()
            );
            fs::rename(cache_root, &aside)?;
            fs::create_dir_all(cache_root)?;
            return Ok(());
        }
        let owed: BTreeSet<RelPath> = {
            let j = self.journal.lock().unwrap();
            j.pending
                .values()
                .flat_map(|p| p.intent.paths().into_iter().cloned().collect::<Vec<_>>())
                .collect()
        };
        let mut ledger = self.ledger();
        let pins = ledger.pins.clone();
        let pinned = |p: &RelPath| pins.iter().any(|r| p == r || p.is_descendant_of(r));
        let gone: Vec<RelPath> = ledger
            .observations
            .keys()
            .filter(|p| !ledger.dirty.contains(p) && !owed.contains(*p) && !pinned(p))
            .cloned()
            .collect();
        for path in &gone {
            ledger.unobserve(path);
        }
        let keep: Vec<PathBuf> = ledger
            .dirty
            .iter()
            .chain(owed.iter())
            .chain(pins.iter())
            .map(|p| self.tree.backing(p))
            .collect();
        drop(ledger);
        prune_dir(cache_root, &keep)?;
        Ok(())
    }

    pub fn needs_baseline(&self, path: &RelPath) -> bool {
        let ledger = self.ledger();
        !ledger.observations.contains_key(path) && !ledger.dirty.contains(path)
    }

    pub async fn overwriting(&self, path: &RelPath) {
        if self.needs_baseline(path) {
            if let Ok(info) = self.sdk.stat(&path.as_file()).await {
                self.ledger().observe(path, Observation::of(&info));
            }
        }
    }

    pub fn content_current(&self, path: &RelPath, current: Observation) -> bool {
        let (observed, dirty) = {
            let ledger = self.ledger();
            (
                ledger.observations.get(path).copied(),
                ledger.dirty.contains(path),
            )
        };
        dirty || (observed == Some(current) && self.tree.backing(path).is_file())
    }

    pub fn dirty_metadata(&self, path: &RelPath) -> Option<fs::Metadata> {
        if !self.ledger().dirty.contains(path) {
            return None;
        }
        fs::metadata(self.tree.backing(path)).ok()
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

    async fn drain(&self, path: &RelPath, subtree: bool) {
        let gates: Vec<Arc<tokio::sync::Mutex<()>>> = self
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
        &self.rt
    }

    pub fn tree(&self) -> &T {
        &self.tree
    }

    pub fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap()
    }
}

pub fn io_err(err: SdkError) -> io::Error {
    match err {
        SdkError::NotFound => io::ErrorKind::NotFound.into(),
        SdkError::PermissionDenied => io::ErrorKind::PermissionDenied.into(),
        err => io::Error::other(err),
    }
}

fn prune_dir(dir: &Path, keep: &[PathBuf]) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if keep.iter().any(|k| path.starts_with(k)) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            if keep.iter().any(|k| k.starts_with(&path)) {
                prune_dir(&path, keep)?;
            } else {
                fs::remove_dir_all(&path)?;
            }
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
