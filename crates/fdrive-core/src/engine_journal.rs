use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::Observation;
use crate::path::RelPath;

// what the filesystem did, in order
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

// the net effect of a burst: what must eventually be true upstream
#[derive(Debug, Clone, PartialEq)]
pub enum Net {
    // `from` is the upstream content these bytes descend from, when the
    // burst itself reveals it (a rename folded into the write)
    Write {
        path: RelPath,
        from: Option<RelPath>,
    },
    Rename {
        from: RelPath,
        to: RelPath,
    },
    Delete(RelPath),
}

impl fmt::Display for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Net::Write { path, from: None } => write!(f, "w {path}"),
            Net::Write {
                path,
                from: Some(x),
            } => write!(f, "w {path}<{x}"),
            Net::Rename { from, to } => write!(f, "mv {from}->{to}"),
            Net::Delete(p) => write!(f, "rm {p}"),
        }
    }
}

// the promise to the server: each verb owns its lease
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Save {
        path: RelPath,
        // the server version these bytes replace; None = nothing should be there
        replaces: Option<Observation>,
        // server content the delta may build on (X-Copy-Source)
        reuses: Option<RelPath>,
    },
    Move {
        from: RelPath,
        to: RelPath,
        // the version being relocated
        moves: Observation,
    },
    Remove {
        path: RelPath,
        // only this version may die
        removes: Observation,
    },
}

impl Intent {
    // the hazard family: intents sharing any of these replay in seq order
    pub fn touches(&self, p: &RelPath) -> bool {
        let related = |q: &RelPath| p == q || p.is_descendant_of(q) || q.is_descendant_of(p);
        match self {
            Intent::Save { path, reuses, .. } => {
                related(path) || reuses.as_ref().is_some_and(related)
            }
            Intent::Move { from, to, .. } => related(from) || related(to),
            Intent::Remove { path, .. } => related(path),
        }
    }

    pub fn overlaps(&self, other: &Intent) -> bool {
        match other {
            Intent::Save { path, reuses, .. } => {
                self.touches(path) || reuses.as_ref().is_some_and(|r| self.touches(r))
            }
            Intent::Move { from, to, .. } => self.touches(from) || self.touches(to),
            Intent::Remove { path, .. } => self.touches(path),
        }
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

// where each pre-existing piece of content stands mid-burst
#[derive(Debug, Clone, PartialEq)]
enum State {
    // fresh bytes; the upstream path they descend from, when known
    New(Option<RelPath>),
    // untouched content that entered the burst living at the named path
    Orig(RelPath),
    Gone,
}

// the working state a burst coalesces onto; seeded from pending intents so
// consecutive bursts fold together instead of queueing
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

// fold a burst onto the slate and emit its net effect; `known` answers
// "does this path exist upstream" so only real content earns a tombstone
pub fn coalesce(mut slate: Slate, ops: &[Operation], known: impl Fn(&RelPath) -> bool) -> Vec<Net> {
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
    let mut net = Vec::new();
    // renames first: they must land before writes clobber their sources
    for orig in &slate.origs {
        let survives = slate
            .content
            .iter()
            .find(|(_, st)| **st == State::Orig(orig.clone()))
            .map(|(p, _)| p.clone());
        if let Some(to) = survives {
            if to != *orig {
                net.push(Net::Rename {
                    from: orig.clone(),
                    to,
                });
            }
        }
    }
    // then writes, so deletes of their ancestors wait behind them
    for (p, st) in &slate.content {
        if let State::New(from) = st {
            net.push(Net::Write {
                path: p.clone(),
                from: from.clone().filter(|x| x != p),
            });
        }
    }
    // deletes last: content that ended nowhere, at a name that ended vacant
    // (a name that ended occupied dies by overwrite, no tombstone needed)
    for orig in &slate.origs {
        let survives = slate
            .content
            .values()
            .any(|st| *st == State::Orig(orig.clone()));
        if !survives && slate.content.get(orig) == Some(&State::Gone) {
            net.push(Net::Delete(orig.clone()));
        }
    }
    net
}

// attach leases: an op becomes an intent only against a version we've seen
pub fn intents(net: &[Net], know: impl Fn(&RelPath) -> Option<Observation>) -> Vec<Intent> {
    let renames: Vec<(&RelPath, &RelPath)> = net
        .iter()
        .filter_map(|n| match n {
            Net::Rename { from, to } => Some((from, to)),
            _ => None,
        })
        .collect();
    let cyclic = cycle_members(&renames);
    let mut out = Vec::new();
    for n in net {
        match n {
            Net::Rename { from, to } => match know(from) {
                // a rename cycle (swap) cannot be ordered; each member
                // degrades to a save of its local bytes, delta'd off the
                // content that used to live there
                Some(moves) if !cyclic.contains(from) => out.push(Intent::Move {
                    from: from.clone(),
                    to: to.clone(),
                    moves,
                }),
                _ => out.push(Intent::Save {
                    path: to.clone(),
                    replaces: know(to),
                    reuses: Some(from.clone()).filter(|f| know(f).is_some()),
                }),
            },
            Net::Write { path, from } => out.push(Intent::Save {
                path: path.clone(),
                replaces: know(path),
                reuses: from.clone().filter(|f| know(f).is_some()),
            }),
            Net::Delete(path) => {
                // no observation = nothing of ours upstream = vacuous
                if let Some(removes) = know(path) {
                    out.push(Intent::Remove {
                        path: path.clone(),
                        removes,
                    });
                }
            }
        }
    }
    out
}

fn cycle_members(renames: &[(&RelPath, &RelPath)]) -> BTreeSet<RelPath> {
    let map: BTreeMap<&RelPath, &RelPath> = renames.iter().copied().collect();
    let mut members = BTreeSet::new();
    for (from, to) in renames {
        let mut cur = *to;
        for _ in 0..renames.len() {
            if cur == *from {
                members.insert((*from).clone());
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

    fn net(ops: &[Operation], known: &[&str]) -> Vec<Net> {
        let known: Vec<RelPath> = known.iter().map(|s| p(s)).collect();
        coalesce(Slate::default(), ops, |q| known.contains(q))
    }

    fn w(path: &str) -> Net {
        Net::Write {
            path: p(path),
            from: None,
        }
    }

    fn wf(path: &str, from: &str) -> Net {
        Net::Write {
            path: p(path),
            from: Some(p(from)),
        }
    }

    fn mv(from: &str, to: &str) -> Net {
        Net::Rename {
            from: p(from),
            to: p(to),
        }
    }

    fn rm(path: &str) -> Net {
        Net::Delete(p(path))
    }

    #[test]
    fn vim_dance_is_a_write() {
        let ops = [
            Operation::Rename(p("a"), p("a~")),
            Operation::Create(p("a")),
            Operation::Write(p("a")),
            Operation::Delete(p("a~")),
        ];
        assert_eq!(net(&ops, &["a"]), vec![w("a")]);
    }

    #[test]
    fn replacefile_dance_is_a_write() {
        let ops = [
            Operation::Create(p("t.tmp")),
            Operation::Write(p("t.tmp")),
            Operation::Rename(p("a"), p("a~RF.TMP")),
            Operation::Rename(p("t.tmp"), p("a")),
            Operation::Delete(p("a~RF.TMP")),
        ];
        assert_eq!(net(&ops, &["a"]), vec![w("a")]);
    }

    #[test]
    fn exiftool_keeps_its_backup() {
        let ops = [
            Operation::Create(p("x_tmp")),
            Operation::Write(p("x_tmp")),
            Operation::Rename(p("x"), p("x_original")),
            Operation::Rename(p("x_tmp"), p("x")),
        ];
        assert_eq!(net(&ops, &["x"]), vec![mv("x", "x_original"), w("x")]);
    }

    #[test]
    fn rename_then_edit_carries_provenance() {
        let ops = [Operation::Rename(p("a"), p("b")), Operation::Write(p("b"))];
        assert_eq!(net(&ops, &["a"]), vec![wf("b", "a"), rm("a")]);
    }

    #[test]
    fn temp_file_that_dies_is_nothing() {
        let ops = [
            Operation::Create(p("t.swp")),
            Operation::Write(p("t.swp")),
            Operation::Delete(p("t.swp")),
        ];
        assert_eq!(net(&ops, &[]), vec![]);
    }

    #[test]
    fn deleted_original_is_a_delete_even_when_edited_first() {
        let ops = [Operation::Write(p("a")), Operation::Delete(p("a"))];
        assert_eq!(net(&ops, &["a"]), vec![rm("a")]);
    }

    #[test]
    fn rename_chain_folds() {
        let ops = [
            Operation::Rename(p("a"), p("b")),
            Operation::Rename(p("b"), p("c")),
        ];
        assert_eq!(net(&ops, &["a"]), vec![mv("a", "c")]);
    }

    #[test]
    fn clobbering_chain_tombstones_the_vacated_name() {
        // c's content lands on a (clobbering it), then moves on to b:
        // upstream must end with c at b, and a gone
        let ops = [
            Operation::Rename(p("c"), p("a")),
            Operation::Rename(p("a"), p("b")),
        ];
        assert_eq!(net(&ops, &["a", "c"]), vec![mv("c", "b"), rm("a")]);
    }

    #[test]
    fn plain_ops_pass_through() {
        let ops = [Operation::Rename(p("a"), p("b")), Operation::Delete(p("x"))];
        assert_eq!(net(&ops, &["a", "x"]), vec![mv("a", "b"), rm("x")]);
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
        assert_eq!(net(&ops, &["a"]), vec![w("a")]);
    }

    #[test]
    fn unobserved_paths_never_earn_tombstones() {
        let ops = [
            Operation::Rename(p("a"), p("a~")),
            Operation::Delete(p("a~")),
        ];
        // a's content died at a~, which the server never had: rm a is the truth
        assert_eq!(net(&ops, &["a"]), vec![rm("a")]);
    }

    #[test]
    fn intents_carry_leases_and_drop_vacuous_removes() {
        let know = |q: &RelPath| (q == &p("a")).then(|| obs(3));
        let made = intents(&[wf("b", "a"), rm("a"), rm("ghost")], know);
        assert_eq!(
            made,
            vec![
                Intent::Save {
                    path: p("b"),
                    replaces: None,
                    reuses: Some(p("a")),
                },
                Intent::Remove {
                    path: p("a"),
                    removes: obs(3),
                },
            ]
        );
    }

    #[test]
    fn swap_degrades_to_saves() {
        let know = |q: &RelPath| (q == &p("a") || q == &p("b")).then(|| obs(1));
        let made = intents(&[mv("a", "b"), mv("b", "a")], know);
        assert_eq!(
            made,
            vec![
                Intent::Save {
                    path: p("b"),
                    replaces: Some(obs(1)),
                    reuses: Some(p("a")),
                },
                Intent::Save {
                    path: p("a"),
                    replaces: Some(obs(1)),
                    reuses: Some(p("b")),
                },
            ]
        );
    }

    #[test]
    fn pending_intents_fold_with_the_next_burst() {
        // pending: save b (was mv a->b + edit); burst: rm b
        // net truth: a is gone, b never reached the server
        let pending = [
            Intent::Save {
                path: p("b"),
                replaces: None,
                reuses: Some(p("a")),
            },
            Intent::Remove {
                path: p("a"),
                removes: obs(1),
            },
        ];
        let ops = [Operation::Delete(p("b"))];
        let folded = coalesce(seed(pending.iter()), &ops, |q| q == &p("a"));
        assert_eq!(folded, vec![rm("a")]);
    }

    #[test]
    fn pending_save_supersedes_on_reedit() {
        let pending = [Intent::Save {
            path: p("a"),
            replaces: Some(obs(1)),
            reuses: None,
        }];
        let ops = [Operation::Write(p("a"))];
        let folded = coalesce(seed(pending.iter()), &ops, |q| q == &p("a"));
        assert_eq!(folded, vec![w("a")]);
    }

    #[test]
    fn hazard_overlap_includes_reuses() {
        let save = Intent::Save {
            path: p("b"),
            replaces: None,
            reuses: Some(p("a")),
        };
        let remove = Intent::Remove {
            path: p("a"),
            removes: obs(1),
        };
        assert!(save.overlaps(&remove));
    }
}
