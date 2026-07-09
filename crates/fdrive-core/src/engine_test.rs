use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use httpmock::{Method, MockServer};

use super::{Engine, Ledger, LocalTree, Observation, Upload};
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

fn mark_dirty(engine: &Engine<TempTree>, path: &RelPath) {
    engine.ledger().dirty_set(path);
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
async fn delete_propagates_and_forgets() {
    let server = MockServer::start();
    let rm = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/rm")
            .query_param("path", "/d/");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let engine = engine(&server);
    let dir = RelPath::new("d");
    let child = RelPath::new("d/f");
    engine.ledger().observe(&child, Observation::new(1, None));
    engine.ledger().dirty_set(&child);

    engine.delete(&dir, true).await.unwrap();
    rm.assert_hits(1);
    assert!(engine.ledger().observations.is_empty());
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn delete_tolerates_not_found() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), Observation::new(1, None));
    engine.delete(&path, false).await.unwrap();
    assert!(engine.ledger().observations.is_empty());
}

#[tokio::test]
async fn delete_of_a_local_only_file_skips_the_server() {
    let server = MockServer::start();
    let rm = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/rm");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.ledger().dirty.insert(path.clone());
    engine.delete(&path, false).await.unwrap();
    rm.assert_hits(0);
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn delete_is_vetoed_by_a_server_failure() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/rm");
        then.status(500);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), Observation::new(1, None));
    assert!(engine.delete(&path, false).await.is_err());
    assert!(engine.ledger().observations.contains_key(&path));
}

#[tokio::test]
async fn rename_propagates_and_remaps() {
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
    engine.ledger().dirty.insert(RelPath::new("a/y"));

    engine
        .ledger()
        .observations
        .insert(RelPath::new("z"), Observation::new(9, None));

    engine
        .rename(&RelPath::new("a"), &RelPath::new("z"), true)
        .await
        .unwrap();
    mv.assert_hits(1);
    let ledger = engine.ledger();
    let keys: Vec<&str> = ledger.observations.keys().map(|p| p.as_str()).collect();
    assert_eq!(keys, ["z/x"]);
    let dirty: Vec<&str> = ledger.dirty.iter().map(|p| p.as_str()).collect();
    assert_eq!(dirty, ["z/y"]);
}

#[tokio::test]
async fn rename_of_an_unuploaded_file_is_local() {
    let server = MockServer::start();
    let engine = engine(&server);
    let (from, to) = (RelPath::new("f"), RelPath::new("g"));
    engine.ledger().dirty.insert(from.clone());
    engine.rename(&from, &to, false).await.unwrap();
    assert!(engine.ledger().dirty.contains(&to));

    engine
        .ledger()
        .observations
        .insert(RelPath::new("clean"), Observation::new(1, None));
    assert!(engine
        .rename(&RelPath::new("clean"), &RelPath::new("other"), false)
        .await
        .is_err());
}

#[tokio::test]
async fn created_is_dirty_from_birth() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine
        .ledger()
        .observations
        .insert(path.clone(), Observation::new(1, None));
    engine.created(&path);
    let ledger = engine.ledger();
    assert!(ledger.local_only(&path), "stale observation must die");
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

#[tokio::test]
async fn released_pushes_only_debts() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"bytes");

    engine.released(&path);
    engine.flush(Duration::from_secs(5)).await;
    save.assert_hits(0);

    engine.modified(&path);
    engine.released(&path);
    engine.flush(Duration::from_secs(5)).await;
    save.assert_hits(1);
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
async fn upload_skips_clean_files() {
    let server = MockServer::start();
    let engine = engine(&server);
    assert!(matches!(
        engine.upload(&RelPath::new("f")).await.unwrap(),
        Upload::Done
    ));
    assert!(engine.tree().settled.lock().unwrap().is_empty());
}

#[tokio::test]
async fn upload_forgets_a_vanished_file() {
    let server = MockServer::start();
    let engine = engine(&server);
    let path = RelPath::new("gone");
    mark_dirty(&engine, &path);
    assert!(matches!(engine.upload(&path).await.unwrap(), Upload::Done));
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn upload_new_file() {
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
    mark_dirty(&engine, &path);

    assert!(matches!(engine.upload(&path).await.unwrap(), Upload::Done));
    save.assert_hits(1);
    assert!(engine.ledger().dirty.is_empty());
    assert_eq!(*engine.tree().settled.lock().unwrap(), [path]);
}

#[tokio::test]
async fn upload_overwrites_when_the_observation_matches() {
    let server = MockServer::start();
    let mtime = "Wed, 21 Oct 2015 07:28:00 GMT";
    let stat = server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .header("content-length", "5")
            .header("last-modified", mtime);
    });
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"hello");
    let observation = Observation::new(5, Some(httpdate::parse_http_date(mtime).unwrap()));
    engine
        .ledger()
        .observations
        .insert(path.clone(), observation);
    mark_dirty(&engine, &path);

    assert!(matches!(engine.upload(&path).await.unwrap(), Upload::Done));
    save.assert_hits(1);
    assert!(stat.hits() >= 2);
    assert_eq!(engine.ledger().observations[&path], observation);
    assert!(engine.ledger().dirty.is_empty());
}

#[tokio::test]
async fn upload_conflict_keeps_both_versions() {
    let server = MockServer::start();
    let stat = server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200)
            .header("content-length", "3")
            .header("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT");
    });
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f (conflicted copy)");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"ours");
    engine
        .ledger()
        .observations
        .insert(path.clone(), Observation::new(1, None));
    mark_dirty(&engine, &path);

    assert!(matches!(engine.upload(&path).await.unwrap(), Upload::Done));
    save.assert_hits(1);
    stat.assert_hits(1);
    assert_eq!(engine.tree().read("f"), None);
    assert_eq!(
        engine.tree().read("f (conflicted copy)").as_deref(),
        Some(b"ours".as_slice())
    );
    assert!(engine.ledger().dirty.is_empty());
    assert_eq!(
        *engine.tree().settled.lock().unwrap(),
        [RelPath::new("f (conflicted copy)")]
    );
}

#[tokio::test]
async fn upload_unseen_collision_diverts_to_a_conflict_copy() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200).header("content-length", "3");
    });
    let save = server.mock(|when, then| {
        when.method(Method::POST)
            .path("/api/files/cat")
            .query_param("path", "/f (conflicted copy)");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"new");
    mark_dirty(&engine, &path);

    assert!(matches!(engine.upload(&path).await.unwrap(), Upload::Done));
    save.assert_hits(1);
}

#[tokio::test]
async fn upload_conflict_never_clobbers_a_local_only_file() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/f");
        then.status(200).header("content-length", "3");
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
    mark_dirty(&engine, &path);

    assert!(matches!(engine.upload(&path).await.unwrap(), Upload::Done));
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
async fn upload_skips_ignored_paths_but_keeps_them_visible() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(200);
    });
    let engine = engine(&server);
    let path = RelPath::new("web/node_modules/left-pad/index.js");
    engine.tree().write(path.as_str(), b"junk");
    engine.modified(&path);

    assert!(matches!(engine.upload(&path).await.unwrap(), Upload::Done));
    save.assert_hits(0);
    assert!(
        engine.ledger().dirty.contains(&path),
        "still dirty so overlay keeps showing it"
    );
    let listing = engine.overlay(&RelPath::new("web/node_modules/left-pad"), vec![]);
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "index.js");
}

#[tokio::test]
async fn upload_recreates_missing_parents_and_stays_dirty_on_failure() {
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
    mark_dirty(&engine, &path);

    assert!(engine.upload(&path).await.is_err());
    mkdir.assert_hits(2);
    save.assert_hits(2);
    assert!(engine.ledger().dirty.contains(&path));
}
