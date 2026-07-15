use std::time::Duration;

use httpmock::{Method, MockServer};

use super::testkit::*;
use crate::path::RelPath;

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
