use std::time::Duration;

use httpmock::{Method, MockServer};

use super::testkit::*;
use crate::engine::{Engine, Observation};
use crate::path::RelPath;
use crate::sdk::Sdk;
use std::sync::Arc;

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
async fn an_offline_dir_rename_is_refused_before_touching_anything() {
    let sdk = Sdk::new("http://127.0.0.1:9").unwrap();
    let rt = tokio::runtime::Handle::current();
    let engine = Engine::start(Arc::new(sdk), rt, TempTree::new());
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
    assert!(engine.idle(), "nothing was queued to replay later");
}

#[tokio::test]
async fn a_failed_save_lands_when_the_server_recovers() {
    let server = MockServer::start();
    let mut broken = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(500);
    });
    let engine = engine(&server);
    let path = RelPath::new("f");
    engine.tree().write("f", b"precious");
    engine.created(&path);
    engine.modified(&path);
    engine.flush(Duration::from_millis(1200)).await;
    assert!(
        engine.ledger().dirty.contains(&path),
        "the debt survives the outage"
    );

    broken.delete();
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(200);
    });
    settle(&engine).await;
    save.assert_hits(1);
    assert!(engine.ledger().dirty.is_empty());
}
