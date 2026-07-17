use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::path::RelPath;
use crate::sdk::FileInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub size: u64,
    pub time: u64,
}

impl Observation {
    pub fn new(size: u64, mtime: Option<SystemTime>) -> Self {
        Self {
            size,
            time: secs(mtime),
        }
    }

    pub fn of(info: &FileInfo) -> Self {
        Self::new(info.size.unwrap_or(0), info.mtime)
    }

    pub fn of_local(md: &std::fs::Metadata) -> Self {
        Self::new(md.len(), md.modified().ok())
    }
}

pub(crate) fn secs(t: Option<SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    Create(RelPath),
    Write(RelPath),
    Rename(RelPath, RelPath),
    Delete(RelPath),
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operation::Create(p) => write!(f, "c {p}"),
            Operation::Write(p) => write!(f, "w {p}"),
            Operation::Rename(a, b) => write!(f, "mv {a}->{b}"),
            Operation::Delete(p) => write!(f, "rm {p}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Save {
        path: RelPath,
        replaces: Option<Observation>,
        reuses: Option<RelPath>,
    },
    Move {
        from: RelPath,
        to: RelPath,
        moves: Observation,
    },
    Remove {
        path: RelPath,
        removes: Observation,
    },
}

impl Plan {
    pub(crate) fn paths(&self) -> Vec<&RelPath> {
        match self {
            Plan::Save { path, reuses, .. } => std::iter::once(path).chain(reuses).collect(),
            Plan::Move { from, to, .. } => vec![from, to],
            Plan::Remove { path, .. } => vec![path],
        }
    }

    pub(crate) fn touches(&self, p: &RelPath) -> bool {
        self.paths()
            .iter()
            .any(|q| p == *q || p.is_descendant_of(q) || q.is_descendant_of(p))
    }

    pub(crate) fn overlaps(&self, other: &Plan) -> bool {
        other.paths().iter().any(|p| self.touches(p))
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Plan::Save {
                path,
                replaces,
                reuses,
            } => {
                write!(f, "save {path}")?;
                if replaces.is_none() {
                    write!(f, " (new)")?;
                }
                if let Some(x) = reuses {
                    write!(f, " <{x}")?;
                }
                Ok(())
            }
            Plan::Move { from, to, .. } => write!(f, "move {from}->{to}"),
            Plan::Remove { path, .. } => write!(f, "remove {path}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Content {
    New(Option<RelPath>),
    Orig(RelPath),
    Gone,
}

pub(crate) fn coalesce<'a>(
    pending: impl Iterator<Item = &'a Plan>,
    ops: &[Operation],
    know: impl Fn(&RelPath) -> Option<Observation>,
) -> Vec<Plan> {
    let mut origs: Vec<RelPath> = Vec::new();
    let mut content: BTreeMap<RelPath, Content> = BTreeMap::new();
    for plan in pending {
        match plan {
            Plan::Save {
                path,
                replaces,
                reuses,
            } => {
                if replaces.is_some() && !origs.contains(path) {
                    origs.push(path.clone());
                }
                content.insert(path.clone(), Content::New(reuses.clone()));
            }
            Plan::Move { from, to, .. } => {
                if !origs.contains(from) {
                    origs.push(from.clone());
                }
                content.insert(to.clone(), Content::Orig(from.clone()));
                content.insert(from.clone(), Content::Gone);
            }
            Plan::Remove { path, .. } => {
                if !origs.contains(path) {
                    origs.push(path.clone());
                }
                content.insert(path.clone(), Content::Gone);
            }
        }
    }
    let known = |p: &RelPath| know(p).is_some();
    fn touch(
        origs: &mut Vec<RelPath>,
        content: &BTreeMap<RelPath, Content>,
        p: &RelPath,
        known: &impl Fn(&RelPath) -> bool,
    ) {
        if known(p) && !content.contains_key(p) && !origs.contains(p) {
            origs.push(p.clone());
        }
    }
    for op in ops {
        match op {
            Operation::Create(p) => {
                touch(&mut origs, &content, p, &known);
                content.insert(p.clone(), Content::New(None));
            }
            Operation::Write(p) => {
                touch(&mut origs, &content, p, &known);
                let from = match content.get(p) {
                    None => known(p).then(|| p.clone()),
                    Some(Content::Orig(x)) => Some(x.clone()),
                    Some(Content::New(x)) => x.clone(),
                    Some(Content::Gone) => None,
                };
                content.insert(p.clone(), Content::New(from));
            }
            Operation::Rename(a, b) => {
                touch(&mut origs, &content, a, &known);
                touch(&mut origs, &content, b, &known);
                let src = content.get(a).cloned().unwrap_or_else(|| {
                    if known(a) {
                        Content::Orig(a.clone())
                    } else {
                        Content::New(None)
                    }
                });
                content.insert(b.clone(), src);
                content.insert(a.clone(), Content::Gone);
            }
            Operation::Delete(p) => {
                touch(&mut origs, &content, p, &known);
                content.insert(p.clone(), Content::Gone);
            }
        }
    }
    let renames: Vec<(RelPath, RelPath)> = origs
        .iter()
        .filter_map(|orig| {
            let to = content
                .iter()
                .find(|(_, st)| **st == Content::Orig(orig.clone()))
                .map(|(p, _)| p.clone())?;
            (to != *orig).then(|| (orig.clone(), to))
        })
        .collect();
    let cyclic = cycle_members(&renames);
    let mut out = Vec::new();
    for (from, to) in &renames {
        match know(from) {
            Some(moves) if !cyclic.contains(from) => out.push(Plan::Move {
                from: from.clone(),
                to: to.clone(),
                moves,
            }),
            _ => out.push(Plan::Save {
                path: to.clone(),
                replaces: know(to),
                reuses: Some(from.clone()).filter(|f| known(f)),
            }),
        }
    }
    for (p, st) in &content {
        if let Content::New(from) = st {
            out.push(Plan::Save {
                path: p.clone(),
                replaces: know(p),
                reuses: from.clone().filter(|x| x != p && known(x)),
            });
        }
    }
    for orig in &origs {
        let survives = content
            .values()
            .any(|st| *st == Content::Orig(orig.clone()));
        if !survives && content.get(orig) == Some(&Content::Gone) {
            if let Some(removes) = know(orig) {
                out.push(Plan::Remove {
                    path: orig.clone(),
                    removes,
                });
            }
        }
    }
    out
}

fn cycle_members(renames: &[(RelPath, RelPath)]) -> BTreeSet<RelPath> {
    let map: BTreeMap<&RelPath, &RelPath> = renames.iter().map(|(f, t)| (f, t)).collect();
    let mut members = BTreeSet::new();
    for (from, to) in renames {
        let mut cur = to;
        for _ in 0..renames.len() {
            if cur == from {
                members.insert(from.clone());
                break;
            }
            match map.get(cur) {
                Some(next) => cur = next,
                None => break,
            }
        }
    }
    members
}

pub(crate) fn render<D: fmt::Display>(items: &[D]) -> String {
    items
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
#[path = "model_test.rs"]
mod tests;
