use super::*;
use std::time::Duration;

fn p(s: &str) -> RelPath {
    RelPath::new(s)
}

fn obs(v: u64) -> Observation {
    Observation { size: v, time: v }
}

fn fold(ops: &[Operation], known: &[&str]) -> Vec<Plan> {
    let known: Vec<RelPath> = known.iter().map(|s| p(s)).collect();
    coalesce([].iter(), ops, |q| {
        known.contains(q).then(|| obs(q.as_str().len() as u64))
    })
}

fn save(path: &str, replaces: Option<&str>, reuses: Option<&str>) -> Plan {
    Plan::Save {
        path: p(path),
        replaces: replaces.map(|r| obs(r.len() as u64)),
        reuses: reuses.map(p),
    }
}

fn mv(from: &str, to: &str) -> Plan {
    Plan::Move {
        from: p(from),
        to: p(to),
        moves: obs(from.len() as u64),
    }
}

fn rm(path: &str) -> Plan {
    Plan::Remove {
        path: p(path),
        removes: obs(path.len() as u64),
    }
}

#[test]
fn observation_time_is_whole_seconds() {
    let fine = UNIX_EPOCH + Duration::from_millis(3_700);
    assert_eq!(
        Observation::new(5, Some(fine)),
        Observation { size: 5, time: 3 }
    );
    assert_eq!(Observation::new(0, None), Observation { size: 0, time: 0 });
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
    let folded = coalesce(pending.iter(), &ops, |q| (q == &p("a")).then(|| obs(1)));
    assert_eq!(
        folded,
        vec![Plan::Remove {
            path: p("a"),
            removes: obs(1),
        }]
    );
}

#[test]
fn pending_save_supersedes_on_reedit() {
    let pending = [save("a", Some("a"), None)];
    let ops = [Operation::Write(p("a"))];
    let folded = coalesce(pending.iter(), &ops, |q| (q == &p("a")).then(|| obs(1)));
    assert_eq!(
        folded,
        vec![Plan::Save {
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
