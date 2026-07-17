use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::path::RelPath;

use super::{Ledger, Outcome};
use crate::model;
use crate::model::{Conflict, Fate, Observation, Operation, Plan};

const WINDOW_QUIET: Duration = Duration::from_millis(250);
const WINDOW_MAX: Duration = Duration::from_secs(2);
const OPEN_QUIET: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(10);
const RETRY_CAP: Duration = Duration::from_secs(300);

pub(super) struct State {
    pub(super) journal: Journal,
    pub(super) ledger: Ledger,
}

pub(super) struct Journal {
    window: Vec<(Instant, Operation)>,
    marks: BTreeSet<RelPath>,
    pending: BTreeMap<i64, Entry>,

    writing: BTreeMap<RelPath, usize>,
    inflight: BTreeSet<i64>,
}

pub(super) struct Entry {
    plan: Plan,
    attempts: u32,
    due: Instant,
}

pub(super) struct View {
    pub(super) fates: BTreeMap<RelPath, Fate>,
    pub(super) dirty: BTreeSet<RelPath>,
}

impl State {
    pub(super) fn open(file: &std::path::Path) -> Self {
        let ledger = match Ledger::open(file) {
            Ok(ledger) => ledger,
            Err(()) => {
                let _ = std::fs::remove_file(file);
                let mut ledger = Ledger::open(file).unwrap_or_default();
                ledger.unreadable = true;
                ledger
            }
        };
        let (recovered, plans) = ledger.journal_load();
        let now = Instant::now();
        let journal = Journal {
            marks: recovered.iter().flat_map(op_paths).cloned().collect(),
            window: recovered.into_iter().map(|op| (now, op)).collect(),
            pending: plans
                .into_iter()
                .map(|(seq, plan)| {
                    (
                        seq,
                        Entry {
                            plan,
                            attempts: 0,
                            due: now,
                        },
                    )
                })
                .collect(),
            writing: BTreeMap::new(),
            inflight: BTreeSet::new(),
        };
        Self { journal, ledger }
    }

    pub(super) fn record(&mut self, op: Operation) {
        let j = &mut self.journal;
        if let Some(last) = j.window.last_mut() {
            if last.1 == op {
                last.0 = Instant::now();
                return;
            }
        }
        match &op {
            Operation::Create(p) | Operation::Write(p) => {
                if j.marks.insert(p.clone()) {
                    self.ledger.mark(p);
                }
                self.ledger.dirty.insert(p.clone());
            }
            Operation::Rename(a, b) => {
                if self.ledger.dirty.remove(a) {
                    self.ledger.dirty.insert(b.clone());
                }
            }
            Operation::Delete(p) => {
                self.ledger.dirty.remove(p);
            }
        }
        j.window.push((Instant::now(), op));
    }

    pub(super) fn compact(
        &mut self,
        force: bool,
        ignore: &crate::config::Ignore,
        empty: impl Fn(&RelPath) -> bool,
    ) {
        let drained = self.drain(force, empty);
        if drained.is_empty() {
            return;
        }
        let seeds = self.journal.fold(&drained);
        let mut kept: Vec<RelPath> = Vec::new();
        let made: Vec<Plan> = model::coalesce(seeds.iter().map(|(_, i)| i), &drained, |p| {
            self.ledger.observations.get(p).copied()
        })
        .into_iter()
        .filter(|plan| match plan {
            Plan::Save { path, .. } if ignore.matches(path) => {
                kept.push(path.clone());
                false
            }
            _ => true,
        })
        .collect();
        log::info!(
            "journal [{}] -> [{}]",
            model::render(&drained),
            model::render(&made)
        );
        let unmark: Vec<RelPath> = drained
            .iter()
            .flat_map(op_paths)
            .filter(|p| !kept.contains(p))
            .cloned()
            .collect();
        let retired: Vec<i64> = seeds.iter().map(|(seq, _)| *seq).collect();
        let rows = self.ledger.journal_swap(&unmark, &retired, &made);
        for p in &unmark {
            self.journal.marks.remove(p);
        }
        self.journal.admit(rows);
        self.refresh();
    }

    fn drain(&mut self, force: bool, empty: impl Fn(&RelPath) -> bool) -> Vec<Operation> {
        let stalled =
            |p: &RelPath| self.ledger.observations.get(p).is_some_and(|o| o.size > 0) && empty(p);
        let j = &mut self.journal;
        let now = Instant::now();
        let quiet = j
            .window
            .last()
            .is_some_and(|(at, _)| now - *at >= WINDOW_QUIET);
        let aged = j
            .window
            .first()
            .is_some_and(|(at, _)| now - *at >= WINDOW_MAX);
        if j.window.is_empty() || !(force || quiet || aged) {
            return Vec::new();
        }
        let inflight: Vec<Plan> = j
            .inflight
            .iter()
            .filter_map(|s| j.pending.get(s).map(|e| e.plan.clone()))
            .collect();
        let mut held: Vec<(Instant, Operation)> = Vec::new();
        let mut held_paths: Vec<RelPath> = Vec::new();
        let mut drained: Vec<Operation> = Vec::new();
        for (at, op) in std::mem::take(&mut j.window) {
            let age = now - at;
            let blocked = op_paths(&op).iter().any(|p| {
                let with_inflight = inflight.iter().any(|i| i.touches(p));
                let with_held = held_paths
                    .iter()
                    .any(|h| h == *p || p.is_descendant_of(h) || h.is_descendant_of(p));
                let still_open = !force && age < OPEN_QUIET && j.writing.contains_key(*p);
                let mid_rewrite = !force && age < WINDOW_MAX && stalled(p);
                with_inflight || with_held || still_open || mid_rewrite
            });
            if blocked {
                held_paths.extend(op_paths(&op).into_iter().cloned());
                held.push((at, op));
            } else {
                drained.push(op);
            }
        }
        j.window = held;
        drained
    }

    pub(super) fn next(&mut self) -> Option<(i64, Plan)> {
        let j = &mut self.journal;
        let now = Instant::now();
        let mut pick: Option<i64> = None;
        'candidates: for (seq, entry) in &j.pending {
            if j.inflight.contains(seq) || entry.due > now {
                continue;
            }
            for (_, earlier) in j.pending.range(..seq) {
                if earlier.plan.overlaps(&entry.plan) {
                    continue 'candidates;
                }
            }
            pick = Some(*seq);
            break;
        }
        let seq = pick?;
        j.inflight.insert(seq);
        Some((seq, j.pending.get(&seq).unwrap().plan.clone()))
    }

    pub(super) fn settle(&mut self, seq: i64, outcome: Outcome) -> (bool, Option<Conflict>) {
        self.journal.inflight.remove(&seq);
        let Some(plan) = self.journal.pending.get(&seq).map(|e| e.plan.clone()) else {
            return (false, None);
        };
        match outcome {
            Outcome::Saved { obs, sig, reedited } => {
                if let Plan::Save { path, .. } = &plan {
                    if let Some(obs) = obs {
                        self.ledger.observe(path, obs);
                    }
                    if let Some(sig) = sig {
                        self.ledger.sign_set(path, &sig);
                    }
                    if reedited {
                        self.record(Operation::Write(path.clone()));
                    }
                }
                self.retire(seq);
                (false, None)
            }
            Outcome::Diverted {
                theirs,
                copy,
                obs,
                sig,
                conflict,
            } => {
                if let Plan::Save { path, .. } = &plan {
                    if let Some(theirs) = theirs {
                        self.ledger.observe(path, theirs);
                    }
                }
                if let Some(obs) = obs {
                    self.ledger.observe(&copy, obs);
                }
                if let Some(sig) = sig {
                    self.ledger.sign_set(&copy, &sig);
                }
                self.retire(seq);
                (false, Some(conflict))
            }
            Outcome::Moved => {
                if let Plan::Move { from, to, .. } = &plan {
                    self.ledger.remap(from, to);
                    log::info!("moved {from} -> {to}");
                }
                self.retire(seq);
                (false, None)
            }
            Outcome::MoveLost {
                theirs,
                resurrect,
                conflict,
            } => {
                if let Plan::Move { from, .. } = &plan {
                    match theirs {
                        Some(theirs) => self.ledger.observe(from, theirs),
                        None => self.ledger.unobserve(from),
                    }
                }
                if let Some(to) = resurrect {
                    self.record(Operation::Write(to));
                }
                self.retire(seq);
                (false, conflict)
            }
            Outcome::Removed => {
                if let Plan::Remove { path, .. } = &plan {
                    self.ledger.forget(path);
                    log::info!("removed {path}");
                }
                self.retire(seq);
                (false, None)
            }
            Outcome::RemoveLost { theirs, conflict } => {
                if let Plan::Remove { path, .. } = &plan {
                    self.ledger.observe(path, theirs);
                }
                self.retire(seq);
                (false, Some(conflict))
            }
            Outcome::Busy => {
                if let Some(entry) = self.journal.pending.get_mut(&seq) {
                    entry.due = Instant::now() + Duration::from_secs(1);
                }
                (false, None)
            }
            Outcome::Failed(err) => {
                if let Some(entry) = self.journal.pending.get_mut(&seq) {
                    entry.attempts += 1;
                    let n = entry.attempts;
                    if n == 1 {
                        log::error!("replay {}: {err}", entry.plan);
                    } else {
                        log::warn!("replay {} (attempt {n}): {err}", entry.plan);
                    }
                    entry.due = Instant::now()
                        + RETRY.saturating_mul(1u32 << (n - 1).min(5)).min(RETRY_CAP);
                }
                (true, None)
            }
        }
    }

    fn retire(&mut self, seq: i64) {
        self.journal.pending.remove(&seq);
        self.ledger.journal_retire(seq);
        self.refresh();
    }

    pub(super) fn pending(&self) -> usize {
        self.journal.pending.len()
    }

    pub(super) fn idle(&self) -> bool {
        self.journal.window.is_empty() && self.journal.pending.is_empty()
    }

    pub(super) fn wait(&self) -> Option<Instant> {
        let j = &self.journal;
        let window = j.window.last().map(|(at, _)| *at + WINDOW_QUIET);
        let due = j
            .pending
            .iter()
            .filter(|(seq, _)| !j.inflight.contains(seq))
            .map(|(_, e)| e.due)
            .min();
        match (window, due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(super) fn rush(&mut self) {
        let now = Instant::now();
        for entry in self.journal.pending.values_mut() {
            entry.due = now;
        }
    }

    pub(super) fn write_opened(&mut self, path: &RelPath) {
        *self.journal.writing.entry(path.clone()).or_insert(0) += 1;
    }

    pub(super) fn write_closed(&mut self, path: &RelPath) {
        if let Some(n) = self.journal.writing.get_mut(path) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.journal.writing.remove(path);
            }
        }
    }

    pub(super) fn owed(&self) -> BTreeSet<RelPath> {
        self.journal
            .pending
            .values()
            .flat_map(|e| e.plan.paths().into_iter().cloned().collect::<Vec<_>>())
            .collect()
    }

    pub(super) fn view(&self) -> View {
        let mut fates: BTreeMap<RelPath, Fate> = BTreeMap::new();
        let mut dirty = self.journal.marks.clone();
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
        for entry in self.journal.pending.values() {
            match &entry.plan {
                Plan::Save { path, .. } => {
                    dirty.insert(path.clone());
                }
                Plan::Move { from, to, moves } => {
                    arrive(&mut fates, from, to, Some(*moves));
                }
                Plan::Remove { path, .. } => {
                    fates.insert(path.clone(), Fate::Gone);
                }
            }
        }
        for (_, op) in &self.journal.window {
            match op {
                Operation::Create(p) | Operation::Write(p) => {
                    fates.remove(p);
                    dirty.insert(p.clone());
                }
                Operation::Rename(from, to) => {
                    let was = self.ledger.observations.get(from).copied();
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
        View { fates, dirty }
    }

    pub(super) fn refresh(&mut self) {
        self.ledger.dirty = self.view().dirty;
    }
}

impl Journal {
    fn fold(&mut self, drained: &[Operation]) -> Vec<(i64, Plan)> {
        let mut seeds: Vec<(i64, Plan)> = Vec::new();
        loop {
            let next = self.pending.iter().find(|(seq, entry)| {
                !self.inflight.contains(seq)
                    && !seeds.iter().any(|(s, _)| s == *seq)
                    && (drained
                        .iter()
                        .flat_map(op_paths)
                        .any(|p| entry.plan.touches(p))
                        || seeds.iter().any(|(_, i)| i.overlaps(&entry.plan)))
            });
            match next {
                Some((seq, _)) => {
                    let seq = *seq;
                    seeds.push((seq, self.pending.remove(&seq).unwrap().plan));
                }
                None => return seeds,
            }
        }
    }

    fn admit(&mut self, rows: Vec<(i64, Plan)>) {
        let now = Instant::now();
        for (seq, plan) in rows {
            self.pending.insert(
                seq,
                Entry {
                    plan,
                    attempts: 0,
                    due: now,
                },
            );
        }
    }
}

fn op_paths(op: &Operation) -> Vec<&RelPath> {
    match op {
        Operation::Create(p) | Operation::Write(p) | Operation::Delete(p) => vec![p],
        Operation::Rename(a, b) => vec![a, b],
    }
}

pub struct LedgerGuard<'a>(pub(super) std::sync::MutexGuard<'a, State>);

impl std::ops::Deref for LedgerGuard<'_> {
    type Target = Ledger;
    fn deref(&self) -> &Ledger {
        &self.0.ledger
    }
}

impl std::ops::DerefMut for LedgerGuard<'_> {
    fn deref_mut(&mut self) -> &mut Ledger {
        &mut self.0.ledger
    }
}
