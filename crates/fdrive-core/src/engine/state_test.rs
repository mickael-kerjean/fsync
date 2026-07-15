use std::time::Duration;

use httpmock::{Method, MockServer};

use super::testkit::*;
use crate::path::RelPath;

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
    assert_eq!(engine.state.lock().unwrap().journal.pending.len(), 1);
}
