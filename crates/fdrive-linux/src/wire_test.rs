use super::*;

fn table() -> InodeTable {
    InodeTable {
        paths: HashMap::from([(ROOT, RelPath::root())]),
        inos: HashMap::from([(RelPath::root(), ROOT)]),
        lookups: HashMap::new(),
        next_ino: 2,
    }
}

#[test]
fn an_inode_is_freed_once_the_kernel_forgets_every_lookup() {
    let mut t = table();
    let path = RelPath::new("a/b");
    let ino = t.ino(&path);
    t.bump(ino);
    t.bump(ino);
    assert_eq!(t.paths.get(&ino), Some(&path));

    t.forget(ino, 1);
    assert_eq!(t.paths.get(&ino), Some(&path), "still referenced once");

    t.forget(ino, 1);
    assert!(!t.paths.contains_key(&ino), "dropped after the last forget");
    assert!(!t.inos.contains_key(&path));
    assert!(!t.lookups.contains_key(&ino));
}

#[test]
fn forgetting_an_unlooked_or_root_inode_is_a_noop() {
    let mut t = table();
    let ino = t.ino(&RelPath::new("listed-only")); // readdir-style, never bumped
    t.forget(ino, 1);
    assert!(
        t.paths.contains_key(&ino),
        "no lookup count, nothing to forget"
    );
    t.bump(ROOT);
    t.forget(ROOT, 1);
    assert_eq!(
        t.paths.get(&ROOT),
        Some(&RelPath::root()),
        "root is never freed"
    );
}
