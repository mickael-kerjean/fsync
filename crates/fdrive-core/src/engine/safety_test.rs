use std::fs;
use std::sync::Arc;
use std::time::Duration;

use httpmock::{Method, Mock, MockServer};

use crate::engine::{Engine, Observation};
use crate::path::RelPath;
use crate::sdk::Sdk;

use super::testkit::{engine, engine_with, observed, settle, TempTree, MTIME};

fn tripwires(server: &MockServer) -> (Mock<'_>, Mock<'_>, Mock<'_>) {
    let rm = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/rm");
        then.status(200).body(r#"{"status":"ok"}"#);
    });
    let mv = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/mv");
        then.status(200).body(r#"{"status":"ok"}"#);
    });
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(200).body(r#"{"status":"ok"}"#);
    });
    (rm, mv, save)
}

#[tokio::test]
async fn a_wiped_db_with_a_leftover_cache_mutates_nothing() {
    let server = MockServer::start();
    let (rm, mv, save) = tripwires(&server);
    let tree = TempTree::new();
    tree.write("was-dirty-before-the-wipe.txt", b"leftover");
    let engine = engine_with(&server, tree);

    settle(&engine).await;

    rm.assert_hits(0);
    mv.assert_hits(0);
    save.assert_hits(0);
}

#[tokio::test]
async fn a_cache_deleted_under_pending_edits_mutates_nothing() {
    let server = MockServer::start();
    let (rm, mv, save) = tripwires(&server);
    let engine = engine(&server);
    let path = RelPath::new("doc.txt");
    engine.created(&path);
    engine.modified(&path);

    settle(&engine).await;

    rm.assert_hits(0);
    mv.assert_hits(0);
    save.assert_hits(0);
    assert!(engine.ledger().dirty.is_empty(), "the debt is written off");
}

#[tokio::test]
async fn garbage_plans_in_the_db_never_reach_the_server() {
    let owner = TempTree::new();
    {
        let db = rusqlite::Connection::open(&owner.state).unwrap();
        db.execute_batch(
            "CREATE TABLE journal(seq INTEGER PRIMARY KEY, op TEXT NOT NULL, path TEXT NOT NULL, dest TEXT, base TEXT, size INTEGER, time INTEGER);
             INSERT INTO journal(op, path, size, time) VALUES ('r', 'ghost/../../../etc/passwd', 5, 5);
             INSERT INTO journal(op, path, dest, size, time) VALUES ('m', 'no/where', 'else/where', 1, 1);",
        )
        .unwrap();
    }
    let server = MockServer::start();
    let (rm, mv, save) = tripwires(&server);
    let engine = engine_with(&server, TempTree::reopen(&owner));

    settle(&engine).await;

    rm.assert_hits(0);
    mv.assert_hits(0);
    save.assert_hits(0);
    assert!(engine.idle(), "the garbage is retired, not retried");
}

#[tokio::test]
async fn a_stale_save_landing_on_a_file_turned_directory_uploads_nothing() {
    let server = MockServer::start();
    let mut down = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(500);
    });
    let owner = TempTree::new();
    let path = RelPath::new("doc.txt");
    {
        let crashed = engine_with(&server, TempTree::reopen(&owner));
        crashed.created(&path);
        crashed.modified(&path);
        owner.write("doc.txt", b"unsent edit");
        crashed.flush(Duration::from_secs(1)).await;
    }
    down.delete();
    fs::remove_file(owner.dir.join("doc.txt")).unwrap();
    fs::create_dir(owner.dir.join("doc.txt")).unwrap();
    owner.write("doc.txt/nested", b"the name is a folder now");

    let (rm, mv, save) = tripwires(&server);
    let engine = engine_with(&server, TempTree::reopen(&owner));
    settle(&engine).await;

    rm.assert_hits(0);
    mv.assert_hits(0);
    save.assert_hits(0);
    assert!(engine.idle(), "the stale save is retired, not retried");
}

#[tokio::test]
async fn a_crash_with_a_pending_remove_replays_it_exactly_once() {
    let server = MockServer::start();
    let mut down = server.mock(|when, then| {
        when.method(Method::HEAD).path("/api/files/cat");
        then.status(500);
    });
    let owner = TempTree::new();
    let path = RelPath::new("doomed.txt");
    {
        let crashed = engine_with(&server, TempTree::reopen(&owner));
        crashed.ledger().observe(&path, observed(5));
        crashed.delete(&path, false).await.unwrap();
        crashed.flush(Duration::from_secs(1)).await;
    }
    down.delete();

    let (rm, mv, save) = tripwires(&server);
    server.mock(|when, then| {
        when.method(Method::HEAD)
            .path("/api/files/cat")
            .query_param("path", "/doomed.txt");
        then.status(200)
            .header("content-length", "5")
            .header("last-modified", MTIME);
    });
    let engine = engine_with(&server, TempTree::reopen(&owner));
    settle(&engine).await;

    rm.assert_hits(1);
    mv.assert_hits(0);
    save.assert_hits(0);
}

#[tokio::test]
async fn a_dead_server_leaves_no_mark_anywhere() {
    let sdk = Sdk::new("http://127.0.0.1:9").unwrap();
    let rt = tokio::runtime::Handle::current();
    let engine = Engine::start(Arc::new(sdk), rt, TempTree::new());
    let path = RelPath::new("doc.txt");
    engine.ledger().observe(&path, Observation::new(1, None));
    engine.created(&path);
    engine.modified(&path);
    engine.delete(&path, false).await.unwrap();

    engine.flush(Duration::from_secs(1)).await;

    assert!(
        !engine.idle(),
        "the debt is still owed, nothing was dropped"
    );
}
