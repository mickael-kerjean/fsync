use httpmock::{Method, MockServer};

use super::testkit::*;
use crate::engine::Observation;
use crate::path::RelPath;

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
