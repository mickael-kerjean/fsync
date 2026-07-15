use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use httpmock::{Method, MockServer};

use super::{Engine, Intent, Ledger, LocalTree, Observation, Resolution};
use crate::path::RelPath;
use crate::sdk::Sdk;

struct TempTree {
    dir: PathBuf,
    state: PathBuf,
    settled: Mutex<Vec<RelPath>>,
}

impl TempTree {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fdrive-engine-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self {
            state: dir.with_extension("ledger.json"),
            dir,
            settled: Mutex::new(Vec::new()),
        }
    }

    fn write(&self, path: &str, content: &[u8]) {
        let abs = self.dir.join(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, content).unwrap();
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.dir.join(path)).ok()
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
        let _ = fs::remove_file(&self.state);
    }
}

impl LocalTree for TempTree {
    fn backing(&self, path: &RelPath) -> PathBuf {
        self.dir.join(path.as_str())
    }

    fn relocate(&self, from: &RelPath, to: &RelPath) -> std::io::Result<()> {
        fs::rename(self.backing(from), self.backing(to))
    }

    fn settled(&self, target: &RelPath, _mtime: Option<SystemTime>) {
        self.settled.lock().unwrap().push(target.clone());
    }

    fn ledger(&self) -> PathBuf {
        self.state.clone()
    }
}

fn engine(server: &MockServer) -> Arc<Engine<TempTree>> {
    engine_with(server, TempTree::new())
}

fn engine_with(server: &MockServer, tree: TempTree) -> Arc<Engine<TempTree>> {
    let mut sdk = Sdk::new(&server.base_url()).unwrap();
    sdk.set_token("TOKEN".into());
    Engine::spawn(Arc::new(sdk), tokio::runtime::Handle::current(), tree)
}

async fn settle(engine: &Engine<TempTree>) {
    engine.flush(Duration::from_secs(10)).await;
}

const MTIME: &str = "Wed, 21 Oct 2015 07:28:00 GMT";

fn observed(size: u64) -> Observation {
    Observation::new(size, Some(httpdate::parse_http_date(MTIME).unwrap()))
}

#[tokio::test]
async fn unreadable_ledger_quarantines_instead_of_pruning() {
    let server = MockServer::start();
    let tree = TempTree::new();
    tree.write("only-copy.txt", b"bytes");
    fs::write(&tree.state, b"not json").unwrap();

    let engine = engine_with(&server, tree);
    let root = engine.tree().dir.clone();
    engine.prune(&root).unwrap();

    assert!(
        engine.tree().read("only-copy.txt").is_none(),
        "the cache was set aside"
    );
    let prefix = format!(
        "{}.unreadable-",
        root.file_name().unwrap().to_string_lossy()
    );
    let aside = fs::read_dir(root.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .expect("quarantine dir exists");
    assert!(aside.path().join("only-copy.txt").exists());
    fs::remove_dir_all(aside.path()).unwrap();

    engine.tree().write("fresh.txt", b"clean");
    engine.prune(&root).unwrap();
    assert!(
        engine.tree().read("fresh.txt").is_none(),
        "a second prune prunes normally instead of quarantining"
    );
    assert!(
        !fs::read_dir(root.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with(&prefix)),
        "no second quarantine"
    );
}

#[test]
fn observation_time_is_whole_seconds() {
    let fine = UNIX_EPOCH + std::time::Duration::from_millis(3_700);
    assert_eq!(
        Observation::new(5, Some(fine)),
        Observation { size: 5, time: 3 }
    );
    assert_eq!(Observation::new(0, None), Observation { size: 0, time: 0 });
}

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
    assert_eq!(window, vec![super::Operation::Write(RelPath::new("a/x"))]);
    assert!(pending.is_empty());
    let dirty: Vec<&str> = ledger.dirty.iter().map(|p| p.as_str()).collect();
    assert_eq!(dirty, ["a/x"]);
}

#[test]
fn journal_intents_survive_a_reopen() {
    let tree = TempTree::new();
    let file = tree.ledger();
    let intents = vec![
        Intent::Save {
            path: RelPath::new("b"),
            replaces: Some(Observation { size: 1, time: 2 }),
            reuses: Some(RelPath::new("a")),
        },
        Intent::Move {
            from: RelPath::new("x"),
            to: RelPath::new("y"),
            moves: Observation { size: 3, time: 4 },
        },
        Intent::Remove {
            path: RelPath::new("z"),
            removes: Observation { size: 5, time: 6 },
        },
    ];
    {
        let mut ledger = Ledger::open(&file).unwrap();
        let rows = ledger.journal_swap(&[], &[], &intents);
        assert_eq!(rows.len(), 3);
    }
    let ledger = Ledger::open(&file).unwrap();
    let loaded: Vec<Intent> = ledger
        .journal_load()
        .1
        .into_iter()
        .map(|(_, i)| i)
        .collect();
    assert_eq!(loaded, intents);
    assert!(
        ledger.dirty.contains(&RelPath::new("b")),
        "a pending save is a dirty path"
    );
}

#[test]
fn legacy_dirty_table_seeds_the_journal() {
    let tree = TempTree::new();
    let file = tree.ledger();
    {
        let db = rusqlite::Connection::open(&file).unwrap();
        db.execute_batch(
            "CREATE TABLE dirty(path TEXT PRIMARY KEY);
             INSERT INTO dirty(path) VALUES ('a/x');",
        )
        .unwrap();
    }
    let mut ledger = Ledger::open(&file).unwrap();
    assert!(ledger.dirty.contains(&RelPath::new("a/x")));
    ledger.journal_swap(&[RelPath::new("a/x")], &[], &[]);
    drop(ledger);
    let ledger = Ledger::open(&file).unwrap();
    assert!(ledger.dirty.is_empty(), "the seed happens only once");
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

#[tokio::test]
async fn modified_marks_the_debt_once() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.modified(&path);
    engine.modified(&path);
    assert!(engine.ledger().dirty.contains(&path));
}

#[tokio::test]
async fn created_keeps_the_lease() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), Observation::new(1, None));
    engine.created(&path);
    let ledger = engine.ledger();
    assert!(ledger.dirty.contains(&path));
    assert!(
        ledger.observations.contains_key(&path),
        "the observation is the lease the save will carry"
    );
}

#[tokio::test]
async fn a_new_file_saves_on_flush() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"hello");
    engine.created(&path);
    engine.modified(&path);

    settle(&engine).await;
    save.assert_hits(1);
    assert!(engine.ledger().dirty.is_empty());
    assert_eq!(*engine.tree().settled.lock().unwrap(), [path]);
}

#[tokio::test]
async fn a_save_carries_its_lease() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f")
            .header("If-Unmodified-Since", MTIME);
        then.status(200).header("Last-Modified", MTIME);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"hello");
    engine
        .ledger()
        .observations
        .insert(path.clone(), observed(5));
    engine.modified(&path);

    settle(&engine).await;
    save.assert_hits(1);
    assert_eq!(engine.ledger().observations[&path], observed(5));
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn a_vanished_file_settles_without_the_server() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("gone");
    engine.modified(&path);
    settle(&engine).await;
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn the_vim_dance_is_one_save() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/a");
        then.status(200).header("Last-Modified", MTIME);
    });
    let mv = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/mv");
        then.status(200);
    });
    let rm = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/rm");
        then.status(200);
    });
    let engine = engine(&server);
    let (a, backup) = (RelPath::new("a"), RelPath::new("a~"));
    engine.ledger().observations.insert(a.clone(), observed(2));
    engine.tree().write("a", b"v2");

    engine.rename(&a, &backup, false).await.unwrap();
    engine.created(&a);
    engine.modified(&a);
    engine.delete(&backup, false).await.unwrap();
    settle(&engine).await;

    save.assert_hits(1);
    mv.assert_hits(0);
    rm.assert_hits(0);
    assert!(engine.ledger().dirty.is_empty());
    assert!(engine.conflicts().is_empty());
}

#[tokio::test]
async fn the_backup_dance_moves_then_saves() {
    let server = MockServer::start();
    let stat = server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/x");
        then.status(200)
            .header("content-length", "5")
            .header("last-modified", MTIME);
    });
    let mv = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/mv")
            .query_param("from", "/x")
            .query_param("to", "/x_original");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/x");
        then.status(200).header("Last-Modified", MTIME);
    });
    let engine = engine(&server);
    let (x, tmp, orig) = (
        RelPath::new("x"),
        RelPath::new("x_tmp"),
        RelPath::new("x_original"),
    );
    engine.ledger().observations.insert(x.clone(), observed(5));
    engine.tree().write("x", b"newer");

    engine.created(&tmp);
    engine.modified(&tmp);
    engine.rename(&x, &orig, false).await.unwrap();
    engine.rename(&tmp, &x, false).await.unwrap();
    settle(&engine).await;

    stat.assert_hits(1);
    mv.assert_hits(1);
    save.assert_hits(1);
    let ledger = engine.ledger();
    let keys: Vec<&str> = ledger.observations.keys().map(|p| p.as_str()).collect();
    assert_eq!(keys, ["x", "x_original"]);
    drop(ledger);
    assert!(engine.conflicts().is_empty());
}

#[tokio::test]
async fn a_file_open_for_writing_holds_its_save() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.write_opened(&path);
    engine.tree().write("f", b"half-written");
    engine.created(&path);
    engine.modified(&path);

    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    save.assert_hits(0);

    engine.write_closed(&path);
    settle(&engine).await;
    save.assert_hits(1);
}

#[tokio::test]
async fn an_emptied_file_waits_for_its_rewrite() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200).header("Last-Modified", MTIME);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), observed(5));

    engine.tree().write("f", b"");
    engine.modified(&path);
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    save.assert_hits(0);

    engine.tree().write("f", b"the real bytes");
    engine.modified(&path);
    settle(&engine).await;
    save.assert_hits(1);
}

#[tokio::test]
async fn a_file_deleted_in_the_window_never_touches_the_server() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(200);
    });
    let rm = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/rm");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("db-journal");
    engine.tree().write("db-journal", b"tmp");
    engine.created(&path);
    engine.modified(&path);
    engine.released(&path);
    engine.delete(&path, false).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    save.assert_hits(0);
    rm.assert_hits(0);
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn a_remove_carries_its_lease() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .header("content-length", "5")
            .header("last-modified", MTIME);
    });
    let rm = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/rm")
            .query_param("path", "/f");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), observed(5));

    engine.delete(&path, false).await.unwrap();
    settle(&engine).await;
    rm.assert_hits(1);
    assert!(engine.ledger().observations.is_empty());
}

#[tokio::test]
async fn a_remove_never_deletes_an_unseen_version() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .header("content-length", "9")
            .header("last-modified", MTIME);
    });
    let rm = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/rm");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), observed(5));

    engine.delete(&path, false).await.unwrap();
    settle(&engine).await;
    rm.assert_hits(0);
    let conflicts = engine.conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].found, Some(observed(9)));
    assert_eq!(
        engine.ledger().observations[&path],
        observed(9),
        "their version is now the one we know"
    );
}

#[tokio::test]
async fn a_local_only_delete_is_vacuous() {
    let server = MockServer::start();
    let rm = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/rm");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.ledger().dirty.insert(path.clone());
    engine.delete(&path, false).await.unwrap();
    settle(&engine).await;
    rm.assert_hits(0);
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn a_failed_remove_stays_owed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD).path("/api/files/cat");
        then.status(500);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), observed(5));
    engine.delete(&path, false).await.unwrap();
    engine.flush(Duration::from_millis(1200)).await;
    assert!(engine.ledger().observations.contains_key(&path));
    assert_eq!(engine.journal.lock().unwrap().pending.len(), 1);
}

#[tokio::test]
async fn a_move_follows_a_vanished_source_with_a_save() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD).path("/api/files/cat");
        then.status(404);
    });
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/b");
        then.status(200);
    });
    let engine = engine(&server);
    let (a, b) = (RelPath::new("a"), RelPath::new("b"));
    engine.ledger().observations.insert(a.clone(), observed(5));
    engine.tree().write("b", b"moved");

    engine.rename(&a, &b, false).await.unwrap();
    settle(&engine).await;
    save.assert_hits(1);
    assert!(
        engine.conflicts().is_empty(),
        "we held the bytes, nothing was lost"
    );
}

#[tokio::test]
async fn dir_delete_drains_the_journal_first() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/d/f");
        then.status(200);
    });
    let rm = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/rm")
            .query_param("path", "/d/");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let engine = engine(&server);
    let (dir, child) = (RelPath::new("d"), RelPath::new("d/f"));
    engine.tree().write("d/f", b"x");
    engine.created(&child);
    engine.modified(&child);

    engine.delete(&dir, true).await.unwrap();
    save.assert_hits(1);
    rm.assert_hits(1);
    assert!(engine.ledger().observations.is_empty());
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn dir_rename_propagates_and_remaps() {
    let server = MockServer::start();
    let mv = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/mv")
            .query_param("from", "/a/")
            .query_param("to", "/z/");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let engine = engine(&server);
    engine
        .ledger()
        .observations
        .insert(RelPath::new("a/x"), Observation::new(1, None));

    engine
        .rename(&RelPath::new("a"), &RelPath::new("z"), true)
        .await
        .unwrap();
    mv.assert_hits(1);
    let ledger = engine.ledger();
    let keys: Vec<&str> = ledger.observations.keys().map(|p| p.as_str()).collect();
    assert_eq!(keys, ["z/x"]);
}

#[tokio::test]
async fn rename_of_an_unuploaded_file_stays_local() {
    let server = MockServer::start();
    let engine = engine(&server);
    let (from, to) = (RelPath::new("f"), RelPath::new("g"));
    engine.tree().write("f", b"bytes");
    engine.modified(&from);
    engine.rename(&from, &to, false).await.unwrap();
    assert!(engine.ledger().dirty.contains(&to));
    assert!(!engine.ledger().dirty.contains(&from));
}

#[tokio::test]
async fn a_save_conflict_keeps_both_versions() {
    let server = MockServer::start();
    let reject = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(412);
    });
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f (conflicted copy)");
        then.status(200).header("Last-Modified", MTIME);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"ours");
    engine
        .ledger()
        .observations
        .insert(path.clone(), Observation::new(1, None));
    engine.modified(&path);

    settle(&engine).await;
    save.assert_hits(1);
    reject.assert_hits(1);
    assert_eq!(engine.tree().read("f"), None);
    assert_eq!(
        engine.tree().read("f (conflicted copy)").as_deref(),
        Some(b"ours".as_slice())
    );
    assert!(engine.ledger().dirty.is_empty());
    let conflicts = engine.conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].ours, Some(RelPath::new("f (conflicted copy)")));
}

#[tokio::test]
async fn resolving_theirs_removes_our_copy() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(412);
    });
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f (conflicted copy)");
        then.status(200).header("Last-Modified", MTIME);
    });
    let rm = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/rm")
            .query_param("path", "/f (conflicted copy)");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"ours");
    engine
        .ledger()
        .observations
        .insert(path.clone(), Observation::new(1, None));
    engine.modified(&path);
    settle(&engine).await;

    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f (conflicted copy)");
        then.status(200)
            .header("content-length", "4")
            .header("last-modified", MTIME);
    });
    let conflict = &engine.conflicts()[0];
    engine.resolve(conflict.seq, Resolution::Theirs).unwrap();
    settle(&engine).await;
    rm.assert_hits(1);
    assert!(engine.conflicts().is_empty());
    assert_eq!(engine.tree().read("f (conflicted copy)"), None);
}

#[tokio::test]
async fn conflicts_never_clobber_a_local_copy() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(412);
    });
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f (conflicted copy 2)");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"ours");
    engine.tree().write("f (conflicted copy)", b"precious");
    engine
        .ledger()
        .observations
        .insert(path.clone(), Observation::new(1, None));
    engine.modified(&path);

    settle(&engine).await;
    save.assert_hits(1);
    assert_eq!(
        engine.tree().read("f (conflicted copy)").as_deref(),
        Some(b"precious".as_slice())
    );
    assert_eq!(
        engine.tree().read("f (conflicted copy 2)").as_deref(),
        Some(b"ours".as_slice())
    );
}

#[tokio::test]
async fn overlay_masks_the_pending_world() {
    let server = MockServer::start();
    let engine = engine(&server);
    let (a, b, doomed) = (RelPath::new("a"), RelPath::new("b"), RelPath::new("doomed"));
    engine.ledger().observations.insert(a.clone(), observed(5));
    engine
        .ledger()
        .observations
        .insert(doomed.clone(), observed(3));

    engine.rename(&a, &b, false).await.unwrap();
    engine.delete(&doomed, false).await.unwrap();

    let listing = engine.overlay(
        &RelPath::new(""),
        vec![
            crate::sdk::FileInfo {
                name: "a".into(),
                kind: crate::sdk::FileType::File,
                size: Some(5),
                mtime: None,
            },
            crate::sdk::FileInfo {
                name: "doomed".into(),
                kind: crate::sdk::FileType::File,
                size: Some(3),
                mtime: None,
            },
        ],
    );
    let names: Vec<&str> = listing.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["b"], "a moved to b, doomed is gone");
    assert_eq!(listing[0].size, Some(5), "b borrows a's metadata");
}

#[tokio::test]
async fn overlay_keeps_local_only_files_visible() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("web/node_modules/left-pad/index.js");
    engine.tree().write(path.as_str(), b"junk");
    engine.modified(&path);

    settle(&engine).await;
    assert!(
        engine.ledger().dirty.contains(&path),
        "ignored paths stay dirty so the overlay keeps showing them"
    );
    let listing = engine.overlay(&RelPath::new("web/node_modules/left-pad"), vec![]);
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "index.js");
}

#[tokio::test]
async fn a_failed_save_keeps_the_debt() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(403);
    });
    let mkdir = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/mkdir");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("a/b/f");
    engine.tree().write("a/b/f", b"deep");
    engine.modified(&path);

    engine.flush(Duration::from_millis(1200)).await;
    mkdir.assert_hits(2);
    save.assert_hits(2);
    assert!(engine.ledger().dirty.contains(&path));
}

#[tokio::test]
async fn concurrent_hydrates_download_once() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .header("content-length", "5")
            .header("last-modified", MTIME);
    });
    let cat = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .body("hello")
            .header("last-modified", MTIME)
            .delay(std::time::Duration::from_millis(200));
    });
    let engine = engine(&server);
    let path = RelPath::new("f");

    let (a, b) = tokio::join!(engine.hydrate(&path, None), engine.hydrate(&path, None));
    a.unwrap();
    b.unwrap();
    cat.assert_hits(1);
    assert_eq!(engine.tree().read("f").unwrap(), b"hello");
}

#[tokio::test]
async fn reads_are_served_while_the_download_is_in_flight() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .header("content-length", "11")
            .header("last-modified", MTIME);
    });
    server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .body("hello world")
            .delay(std::time::Duration::from_millis(300));
    });
    let engine = engine(&server);
    let path = RelPath::new("f");

    engine.hydrate_start(&path, None).await.unwrap();
    assert!(
        engine.tree().read("f").is_none(),
        "hydrate_start returned before the file was cached"
    );
    let download = engine.download(&path).expect("download is in flight");
    assert_eq!(download.read(0, 5).await.unwrap(), b"hello");
    download.done().await.unwrap();
    assert_eq!(engine.tree().read("f").unwrap(), b"hello world");
}

#[tokio::test]
async fn a_renamed_file_hydrates_from_its_old_name() {
    let server = MockServer::start();
    let cat = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/files/cat")
            .query_param("path", "/a");
        then.status(200)
            .body("hello")
            .header("last-modified", MTIME);
    });
    let engine = engine(&server);
    let (a, b) = (RelPath::new("a"), RelPath::new("b"));
    engine.ledger().observations.insert(a.clone(), observed(5));

    engine.rename(&a, &b, false).await.unwrap();
    engine.hydrate(&b, Some(observed(5))).await.unwrap();
    cat.assert_hits(1);
    assert_eq!(engine.tree().read("b").unwrap(), b"hello");
}

#[tokio::test]
async fn a_deleted_file_stops_hydrating() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), observed(5));
    engine.delete(&path, false).await.unwrap();
    assert!(engine.hydrate(&path, None).await.is_err());
}

#[tokio::test]
async fn an_offline_dir_rename_is_refused_before_touching_anything() {
    let sdk = Sdk::new("http://127.0.0.1:9").unwrap();
    let rt = tokio::runtime::Handle::current();
    let engine = Engine::spawn(Arc::new(sdk), rt, TempTree::new());
    engine
        .ledger()
        .observations
        .insert(RelPath::new("a/x"), Observation::new(1, None));

    let refused = engine
        .rename(&RelPath::new("a"), &RelPath::new("z"), true)
        .await;
    assert!(refused.is_err(), "the plane rename fails loudly");
    assert!(
        engine
            .ledger()
            .observations
            .contains_key(&RelPath::new("a/x")),
        "nothing was remapped"
    );
    assert!(engine.fates().is_empty(), "nothing is pending");
    assert!(engine.journal_idle(), "nothing was queued to replay later");
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

#[tokio::test]
async fn a_pin_hydrates_the_subtree() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/files/ls")
            .query_param("path", "/d/");
        then.status(200).json_body(serde_json::json!({
            "status": "ok",
            "results": [{"name": "f.txt", "size": 5, "time": 0, "type": "file"}]
        }));
    });
    let cat = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/files/cat")
            .query_param("path", "/d/f.txt");
        then.status(200).body("hello");
    });
    let engine = engine(&server);
    engine.ledger().pin_set(&RelPath::new("d"));
    engine.hydrate_subtree(&RelPath::new("d")).await;
    cat.assert_hits(1);
    assert_eq!(engine.tree().read("d/f.txt").unwrap(), b"hello");
    assert!(
        engine
            .ledger()
            .observations
            .contains_key(&RelPath::new("d/f.txt")),
        "the walk observed what it listed"
    );

    engine.hydrate_subtree(&RelPath::new("d")).await;
    cat.assert_hits(1);
}

#[tokio::test]
async fn prune_spares_pinned_content() {
    let server = MockServer::start();
    let engine = engine(&server);
    let root = engine.tree().dir.clone();
    let path = RelPath::new("d/f.txt");
    engine.tree().write("d/f.txt", b"hello");
    engine.ledger().observe(&path, Observation::new(5, None));
    engine.ledger().pin_set(&RelPath::new("d"));

    engine.prune(&root).unwrap();
    assert_eq!(engine.tree().read("d/f.txt").unwrap(), b"hello");
    assert!(engine.ledger().observations.contains_key(&path));

    engine.unpin(&RelPath::new("d"));
    engine.prune(&root).unwrap();
    assert!(engine.tree().read("d/f.txt").is_none());
    assert!(!engine.ledger().observations.contains_key(&path));
}

#[tokio::test]
async fn the_scheduler_replays_concurrently() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}))
            .delay(std::time::Duration::from_millis(200));
    });
    let engine = engine(&server);
    let started = std::time::Instant::now();
    for name in ["a", "b", "c", "d"] {
        let path = RelPath::new(name);
        engine.tree().write(name, b"x");
        engine.created(&path);
        engine.modified(&path);
    }
    settle(&engine).await;
    save.assert_hits(4);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(700),
        "4 saves at 200ms each finished in {:?}, so they overlapped",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_cached_file_opens_when_the_server_is_unreachable() {
    let sdk = Sdk::new("http://127.0.0.1:9").unwrap();
    let rt = tokio::runtime::Handle::current();
    let engine = Engine::spawn(Arc::new(sdk), rt, TempTree::new());
    let path = RelPath::new("f");
    engine.tree().write("f", b"cached");
    engine.ledger().observe(&path, Observation::new(6, None));

    engine.hydrate(&path, None).await.unwrap();
    assert_eq!(engine.tree().read("f").unwrap(), b"cached");
    assert!(
        engine
            .hydrate(&RelPath::new("never-cached"), None)
            .await
            .is_err(),
        "a file we never saw still fails honestly"
    );
}

#[tokio::test]
async fn a_fresh_listing_hint_makes_a_cold_open_one_request() {
    let server = MockServer::start();
    let cat = server.mock(|when, then| {
        when.method(Method::GET)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .body("hello")
            .header("last-modified", MTIME);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    let hint = observed(5);

    engine.hydrate(&path, Some(hint)).await.unwrap();
    cat.assert_hits(1);
    assert_eq!(engine.tree().read("f").unwrap(), b"hello");
    assert_eq!(
        engine.ledger().observations.get(&path).copied(),
        Some(hint),
        "the observation comes from the cat response headers"
    );
}

#[tokio::test]
async fn content_current_is_the_freshness_rule() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("f");
    let version = Observation::new(5, None);

    assert!(!engine.content_current(&path, version), "nothing local yet");
    engine.ledger().observations.insert(path.clone(), version);
    assert!(
        !engine.content_current(&path, version),
        "observed but no bytes"
    );
    engine.tree().write("f", b"bytes");
    assert!(engine.content_current(&path, version));
    assert!(
        !engine.content_current(&path, Observation::new(9, None)),
        "server moved"
    );
    engine.ledger().dirty.insert(path.clone());
    assert!(
        engine.content_current(&path, Observation::new(9, None)),
        "dirty always wins"
    );
}

#[tokio::test]
async fn overwriting_observes_the_replaced_version() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200).header("content-length", "7");
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.overwriting(&path).await;
    assert_eq!(
        engine.ledger().observations.get(&path),
        Some(&Observation::new(7, None))
    );

    let dirty = RelPath::new("g");
    engine.ledger().dirty.insert(dirty.clone());
    engine.overwriting(&dirty).await;
    assert!(!engine.ledger().observations.contains_key(&dirty));
}
