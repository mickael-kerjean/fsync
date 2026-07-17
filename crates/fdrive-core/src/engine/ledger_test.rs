use super::testkit::*;
use crate::engine::{Ledger, Observation, Plan};
use crate::path::RelPath;
use crate::port::LocalTree;

#[test]
fn forget_drops_the_subtree_and_nothing_else() {
    let mut ledger = Ledger::default();
    for p in ["a", "a/b", "ab", "c"] {
        ledger
            .observations
            .insert(RelPath::new(p), Observation::new(0, None));
        ledger.dirty.insert(RelPath::new(p));
    }
    ledger.forget(&RelPath::new("a"));
    let keys: Vec<&str> = ledger.observations.keys().map(|p| p.as_str()).collect();
    assert_eq!(keys, ["ab", "c"]);
    let dirty: Vec<&str> = ledger.dirty.iter().map(|p| p.as_str()).collect();
    assert_eq!(dirty, ["ab", "c"]);
}

#[test]
fn remap_moves_the_subtree_bookkeeping() {
    let mut ledger = Ledger::default();
    ledger
        .observations
        .insert(RelPath::new("a"), Observation::new(1, None));
    ledger
        .observations
        .insert(RelPath::new("a/x"), Observation::new(2, None));
    ledger
        .observations
        .insert(RelPath::new("b"), Observation::new(3, None));
    ledger.dirty.insert(RelPath::new("a/x"));
    ledger.dirty.insert(RelPath::new("b"));
    ledger.remap(&RelPath::new("a"), &RelPath::new("z"));
    let keys: Vec<&str> = ledger.observations.keys().map(|p| p.as_str()).collect();
    assert_eq!(keys, ["b", "z", "z/x"]);
    assert_eq!(
        ledger.observations[&RelPath::new("z/x")],
        Observation::new(2, None)
    );
    let dirty: Vec<&str> = ledger.dirty.iter().map(|p| p.as_str()).collect();
    assert_eq!(dirty, ["b", "z/x"]);
}

#[test]
fn journal_marks_survive_a_reopen_as_window_writes() {
    let tree = TempTree::new();
    let file = tree.ledger();
    {
        let mut ledger = Ledger::open(&file).unwrap();
        ledger.mark(&RelPath::new("a/x"));
        ledger.mark(&RelPath::new("b"));
        ledger.journal_swap(&[RelPath::new("b")], &[], &[]);
    }
    let ledger = Ledger::open(&file).unwrap();
    let (window, pending) = ledger.journal_load();
    assert_eq!(
        window,
        vec![crate::engine::Operation::Write(RelPath::new("a/x"))]
    );
    assert!(pending.is_empty());
    let dirty: Vec<&str> = ledger.dirty.iter().map(|p| p.as_str()).collect();
    assert_eq!(dirty, ["a/x"]);
}

#[test]
fn journal_intents_survive_a_reopen() {
    let tree = TempTree::new();
    let file = tree.ledger();
    let plans = vec![
        Plan::Save {
            path: RelPath::new("b"),
            replaces: Some(Observation { size: 1, time: 2 }),
            reuses: Some(RelPath::new("a")),
        },
        Plan::Move {
            from: RelPath::new("x"),
            to: RelPath::new("y"),
            moves: Observation { size: 3, time: 4 },
        },
        Plan::Remove {
            path: RelPath::new("z"),
            removes: Observation { size: 5, time: 6 },
        },
    ];
    {
        let mut ledger = Ledger::open(&file).unwrap();
        let rows = ledger.journal_swap(&[], &[], &plans);
        assert_eq!(rows.len(), 3);
    }
    let ledger = Ledger::open(&file).unwrap();
    let loaded: Vec<Plan> = ledger
        .journal_load()
        .1
        .into_iter()
        .map(|(_, i)| i)
        .collect();
    assert_eq!(loaded, plans);
    assert!(
        ledger.dirty.contains(&RelPath::new("b")),
        "a pending save is a dirty path"
    );
}

#[test]
fn local_only_means_dirty_with_no_record() {
    let mut ledger = Ledger::default();
    let path = RelPath::new("f");
    assert!(!ledger.local_only(&path));
    ledger.dirty.insert(path.clone());
    assert!(ledger.local_only(&path));
    ledger
        .observations
        .insert(path.clone(), Observation::new(0, None));
    assert!(!ledger.local_only(&path));
}

#[test]
fn pins_survive_a_reopen() {
    let tree = TempTree::new();
    let file = tree.ledger();
    {
        let mut ledger = Ledger::open(&file).unwrap();
        ledger.pin_set(&RelPath::new("keep"));
        ledger.pin_set(&RelPath::new("gone"));
        ledger.pin_clear(&RelPath::new("gone"));
    }
    let ledger = Ledger::open(&file).unwrap();
    let pins: Vec<&str> = ledger.pins.iter().map(|p| p.as_str()).collect();
    assert_eq!(pins, ["keep"]);
}
