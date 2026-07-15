use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::Observation;
use crate::path::RelPath;

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
pub enum Intent {
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

impl Intent {
    pub fn paths(&self) -> Vec<&RelPath> {
        match self {
            Intent::Save { path, reuses, .. } => std::iter::once(path).chain(reuses).collect(),
            Intent::Move { from, to, .. } => vec![from, to],
            Intent::Remove { path, .. } => vec![path],
        }
    }

    pub fn touches(&self, p: &RelPath) -> bool {
        self.paths()
            .iter()
            .any(|q| p == *q || p.is_descendant_of(q) || q.is_descendant_of(p))
    }

    pub fn overlaps(&self, other: &Intent) -> bool {
        other.paths().iter().any(|p| self.touches(p))
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Intent::Save {
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
            Intent::Move { from, to, .. } => write!(f, "move {from}->{to}"),
            Intent::Remove { path, .. } => write!(f, "remove {path}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    New(Option<RelPath>),
    Orig(RelPath),
    Gone,
}

#[derive(Default)]
pub struct Slate {
    origs: Vec<RelPath>,
    content: BTreeMap<RelPath, State>,
}

pub fn seed<'a>(pending: impl Iterator<Item = &'a Intent>) -> Slate {
    let mut slate = Slate::default();
    for intent in pending {
        match intent {
            Intent::Save {
                path,
                replaces,
                reuses,
            } => {
                if replaces.is_some() && !slate.origs.contains(path) {
                    slate.origs.push(path.clone());
                }
                slate
                    .content
                    .insert(path.clone(), State::New(reuses.clone()));
            }
            Intent::Move { from, to, .. } => {
                if !slate.origs.contains(from) {
                    slate.origs.push(from.clone());
                }
                slate.content.insert(to.clone(), State::Orig(from.clone()));
                slate.content.insert(from.clone(), State::Gone);
            }
            Intent::Remove { path, .. } => {
                if !slate.origs.contains(path) {
                    slate.origs.push(path.clone());
                }
                slate.content.insert(path.clone(), State::Gone);
            }
        }
    }
    slate
}

pub fn coalesce(
    mut slate: Slate,
    ops: &[Operation],
    know: impl Fn(&RelPath) -> Option<Observation>,
) -> Vec<Intent> {
    let known = |p: &RelPath| know(p).is_some();
    fn touch(slate: &mut Slate, p: &RelPath, known: &impl Fn(&RelPath) -> bool) {
        if known(p) && !slate.content.contains_key(p) && !slate.origs.contains(p) {
            slate.origs.push(p.clone());
        }
    }
    for op in ops {
        match op {
            Operation::Create(p) => {
                touch(&mut slate, p, &known);
                slate.content.insert(p.clone(), State::New(None));
            }
            Operation::Write(p) => {
                touch(&mut slate, p, &known);
                let from = match slate.content.get(p) {
                    None => known(p).then(|| p.clone()),
                    Some(State::Orig(x)) => Some(x.clone()),
                    Some(State::New(x)) => x.clone(),
                    Some(State::Gone) => None,
                };
                slate.content.insert(p.clone(), State::New(from));
            }
            Operation::Rename(a, b) => {
                touch(&mut slate, a, &known);
                touch(&mut slate, b, &known);
                let src = slate.content.get(a).cloned().unwrap_or_else(|| {
                    if known(a) {
                        State::Orig(a.clone())
                    } else {
                        State::New(None)
                    }
                });
                slate.content.insert(b.clone(), src);
                slate.content.insert(a.clone(), State::Gone);
            }
            Operation::Delete(p) => {
                touch(&mut slate, p, &known);
                slate.content.insert(p.clone(), State::Gone);
            }
        }
    }
    let renames: Vec<(RelPath, RelPath)> = slate
        .origs
        .iter()
        .filter_map(|orig| {
            let to = slate
                .content
                .iter()
                .find(|(_, st)| **st == State::Orig(orig.clone()))
                .map(|(p, _)| p.clone())?;
            (to != *orig).then(|| (orig.clone(), to))
        })
        .collect();
    let cyclic = cycle_members(&renames);
    let mut out = Vec::new();
    for (from, to) in &renames {
        match know(from) {
            Some(moves) if !cyclic.contains(from) => out.push(Intent::Move {
                from: from.clone(),
                to: to.clone(),
                moves,
            }),
            _ => out.push(Intent::Save {
                path: to.clone(),
                replaces: know(to),
                reuses: Some(from.clone()).filter(|f| known(f)),
            }),
        }
    }
    for (p, st) in &slate.content {
        if let State::New(from) = st {
            out.push(Intent::Save {
                path: p.clone(),
                replaces: know(p),
                reuses: from.clone().filter(|x| x != p && known(x)),
            });
        }
    }
    for orig in &slate.origs {
        let survives = slate
            .content
            .values()
            .any(|st| *st == State::Orig(orig.clone()));
        if !survives && slate.content.get(orig) == Some(&State::Gone) {
            if let Some(removes) = know(orig) {
                out.push(Intent::Remove {
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

pub fn render<D: fmt::Display>(items: &[D]) -> String {
    items
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::new(s)
    }

    fn obs(v: u64) -> Observation {
        Observation { size: v, time: v }
    }

    fn fold(ops: &[Operation], known: &[&str]) -> Vec<Intent> {
        let known: Vec<RelPath> = known.iter().map(|s| p(s)).collect();
        coalesce(Slate::default(), ops, |q| {
            known.contains(q).then(|| obs(q.as_str().len() as u64))
        })
    }

    fn save(path: &str, replaces: Option<&str>, reuses: Option<&str>) -> Intent {
        Intent::Save {
            path: p(path),
            replaces: replaces.map(|r| obs(r.len() as u64)),
            reuses: reuses.map(p),
        }
    }

    fn mv(from: &str, to: &str) -> Intent {
        Intent::Move {
            from: p(from),
            to: p(to),
            moves: obs(from.len() as u64),
        }
    }

    fn rm(path: &str) -> Intent {
        Intent::Remove {
            path: p(path),
            removes: obs(path.len() as u64),
        }
    }

    #[test]
    fn vim_dance_is_one_save() {
        let ops = [
            Operation::Rename(p("a"), p("a~")),
            Operation::Create(p("a")),
            Operation::Write(p("a")),
            Operation::Delete(p("a~")),
        ];
        assert_eq!(fold(&ops, &["a"]), vec![save("a", Some("a"), None)]);
    }

    #[test]
    fn replacefile_dance_is_one_save() {
        let ops = [
            Operation::Create(p("t.tmp")),
            Operation::Write(p("t.tmp")),
            Operation::Rename(p("a"), p("a~RF.TMP")),
            Operation::Rename(p("t.tmp"), p("a")),
            Operation::Delete(p("a~RF.TMP")),
        ];
        assert_eq!(fold(&ops, &["a"]), vec![save("a", Some("a"), None)]);
    }

    #[test]
    fn exiftool_keeps_its_backup() {
        let ops = [
            Operation::Create(p("x_tmp")),
            Operation::Write(p("x_tmp")),
            Operation::Rename(p("x"), p("x_original")),
            Operation::Rename(p("x_tmp"), p("x")),
        ];
        assert_eq!(
            fold(&ops, &["x"]),
            vec![mv("x", "x_original"), save("x", Some("x"), None)]
        );
    }

    #[test]
    fn rename_then_edit_saves_with_provenance_then_removes() {
        let ops = [Operation::Rename(p("a"), p("b")), Operation::Write(p("b"))];
        assert_eq!(
            fold(&ops, &["a"]),
            vec![save("b", None, Some("a")), rm("a")]
        );
    }

    #[test]
    fn temp_file_that_dies_is_nothing() {
        let ops = [
            Operation::Create(p("t.swp")),
            Operation::Write(p("t.swp")),
            Operation::Delete(p("t.swp")),
        ];
        assert_eq!(fold(&ops, &[]), vec![]);
    }

    #[test]
    fn deleted_original_is_a_remove_even_when_edited_first() {
        let ops = [Operation::Write(p("a")), Operation::Delete(p("a"))];
        assert_eq!(fold(&ops, &["a"]), vec![rm("a")]);
    }

    #[test]
    fn rename_chain_folds() {
        let ops = [
            Operation::Rename(p("a"), p("b")),
            Operation::Rename(p("b"), p("c")),
        ];
        assert_eq!(fold(&ops, &["a"]), vec![mv("a", "c")]);
    }

    #[test]
    fn clobbering_chain_tombstones_the_vacated_name() {
        let ops = [
            Operation::Rename(p("c"), p("a")),
            Operation::Rename(p("a"), p("b")),
        ];
        assert_eq!(fold(&ops, &["a", "c"]), vec![mv("c", "b"), rm("a")]);
    }

    #[test]
    fn plain_ops_pass_through() {
        let ops = [Operation::Rename(p("a"), p("b")), Operation::Delete(p("x"))];
        assert_eq!(fold(&ops, &["a", "x"]), vec![mv("a", "b"), rm("x")]);
    }

    #[test]
    fn edit_survives_a_following_dance() {
        let ops = [
            Operation::Write(p("a")),
            Operation::Rename(p("a"), p("a~")),
            Operation::Create(p("a")),
            Operation::Write(p("a")),
            Operation::Delete(p("a~")),
        ];
        assert_eq!(fold(&ops, &["a"]), vec![save("a", Some("a"), None)]);
    }

    #[test]
    fn unobserved_paths_never_earn_tombstones() {
        let ops = [
            Operation::Rename(p("a"), p("a~")),
            Operation::Delete(p("a~")),
        ];
        assert_eq!(fold(&ops, &["a"]), vec![rm("a")]);
    }

    #[test]
    fn swap_degrades_to_saves() {
        let ops = [
            Operation::Rename(p("a"), p("t")),
            Operation::Rename(p("b"), p("a")),
            Operation::Rename(p("t"), p("b")),
        ];
        assert_eq!(
            fold(&ops, &["a", "b"]),
            vec![
                save("b", Some("b"), Some("a")),
                save("a", Some("a"), Some("b"))
            ]
        );
    }

    #[test]
    fn pending_intents_fold_with_the_next_burst() {
        let pending = [save("b", None, Some("a")), rm("a")];
        let ops = [Operation::Delete(p("b"))];
        let folded = coalesce(seed(pending.iter()), &ops, |q| {
            (q == &p("a")).then(|| obs(1))
        });
        assert_eq!(
            folded,
            vec![Intent::Remove {
                path: p("a"),
                removes: obs(1),
            }]
        );
    }

    #[test]
    fn pending_save_supersedes_on_reedit() {
        let pending = [save("a", Some("a"), None)];
        let ops = [Operation::Write(p("a"))];
        let folded = coalesce(seed(pending.iter()), &ops, |q| {
            (q == &p("a")).then(|| obs(1))
        });
        assert_eq!(
            folded,
            vec![Intent::Save {
                path: p("a"),
                replaces: Some(obs(1)),
                reuses: None,
            }]
        );
    }

    #[test]
    fn hazard_overlap_includes_reuses() {
        let save = save("b", None, Some("a"));
        let remove = rm("a");
        assert!(save.overlaps(&remove));
    }
}
