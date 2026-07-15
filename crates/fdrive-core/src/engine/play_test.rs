use httpmock::{Method, MockServer};

use super::testkit::*;
use crate::path::RelPath;

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
