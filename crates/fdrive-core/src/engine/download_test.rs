use httpmock::{Method, MockServer};

use super::testkit::*;
use crate::engine::{Engine, Observation};
use crate::path::RelPath;
use crate::sdk::Sdk;
use std::sync::Arc;

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
async fn a_cached_file_opens_when_the_server_is_unreachable() {
    let sdk = Sdk::new("http://127.0.0.1:9").unwrap();
    let rt = tokio::runtime::Handle::current();
    let engine = Engine::start(Arc::new(sdk), rt, TempTree::new());
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
