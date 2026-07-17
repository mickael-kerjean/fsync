use httpmock::{Method, MockServer};

use super::testkit::*;
use crate::engine::{Observation, Resolution};
use crate::path::RelPath;

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
