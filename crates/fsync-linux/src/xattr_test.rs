use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use fsync_core::path::RelPath;
use fuser::Errno;

use super::XattrDb;

struct TempFile(PathBuf);

impl TempFile {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        Self(std::env::temp_dir().join(format!(
            "fsync-xattr-test-{}-{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn assert_err(result: Result<(), Errno>, want: Errno) {
    assert_eq!(format!("{:?}", result.unwrap_err()), format!("{want:?}"));
}

#[test]
fn set_get_roundtrip() {
    let file = TempFile::new();
    let db = XattrDb::open(file.0.clone());
    let path = RelPath::new("f");
    db.set(&path, "user.tag", b"blue", 0).unwrap();
    assert_eq!(
        db.get(&path, "user.tag").as_deref(),
        Some(b"blue".as_slice())
    );
    assert_eq!(db.get(&path, "user.other"), None);
    assert_eq!(db.get(&RelPath::new("g"), "user.tag"), None);
}

#[test]
fn create_and_replace_flags() {
    let file = TempFile::new();
    let db = XattrDb::open(file.0.clone());
    let path = RelPath::new("f");
    assert_err(
        db.set(&path, "user.a", b"x", libc::XATTR_REPLACE),
        Errno::ENODATA,
    );
    db.set(&path, "user.a", b"x", libc::XATTR_CREATE).unwrap();
    assert_err(
        db.set(&path, "user.a", b"y", libc::XATTR_CREATE),
        Errno::EEXIST,
    );
    db.set(&path, "user.a", b"y", libc::XATTR_REPLACE).unwrap();
    assert_eq!(db.get(&path, "user.a").as_deref(), Some(b"y".as_slice()));
}

#[test]
fn list_is_null_terminated_names() {
    let file = TempFile::new();
    let db = XattrDb::open(file.0.clone());
    let path = RelPath::new("f");
    assert_eq!(db.list(&path), b"");
    db.set(&path, "user.b", b"", 0).unwrap();
    db.set(&path, "user.a", b"1", 0).unwrap();
    assert_eq!(db.list(&path), b"user.a\0user.b\0");
}

#[test]
fn remove_reports_missing() {
    let file = TempFile::new();
    let db = XattrDb::open(file.0.clone());
    let path = RelPath::new("f");
    assert_err(db.remove(&path, "user.a"), Errno::ENODATA);
    db.set(&path, "user.a", b"x", 0).unwrap();
    db.remove(&path, "user.a").unwrap();
    assert_eq!(db.get(&path, "user.a"), None);
    assert_err(db.remove(&path, "user.a"), Errno::ENODATA);
}

#[test]
fn forget_drops_the_subtree_and_nothing_else() {
    let file = TempFile::new();
    let db = XattrDb::open(file.0.clone());
    for p in ["a", "a/b", "ab"] {
        db.set(&RelPath::new(p), "user.k", b"v", 0).unwrap();
    }
    db.forget(&RelPath::new("a"));
    assert_eq!(db.get(&RelPath::new("a"), "user.k"), None);
    assert_eq!(db.get(&RelPath::new("a/b"), "user.k"), None);
    assert!(db.get(&RelPath::new("ab"), "user.k").is_some());
}

#[test]
fn remap_moves_the_subtree_and_drops_the_replaced_target() {
    let file = TempFile::new();
    let db = XattrDb::open(file.0.clone());
    db.set(&RelPath::new("a/x"), "user.k", b"kept", 0).unwrap();
    db.set(&RelPath::new("z"), "user.k", b"replaced", 0)
        .unwrap();
    db.remap(&RelPath::new("a"), &RelPath::new("z"));
    assert_eq!(db.get(&RelPath::new("a/x"), "user.k"), None);
    assert_eq!(
        db.get(&RelPath::new("z/x"), "user.k").as_deref(),
        Some(b"kept".as_slice())
    );
    assert_eq!(db.get(&RelPath::new("z"), "user.k"), None);
}

#[test]
fn survives_a_reopen() {
    let file = TempFile::new();
    let path = RelPath::new("f");
    XattrDb::open(file.0.clone())
        .set(&path, "user.tag", b"blue", 0)
        .unwrap();
    let db = XattrDb::open(file.0.clone());
    assert_eq!(
        db.get(&path, "user.tag").as_deref(),
        Some(b"blue".as_slice())
    );
}
