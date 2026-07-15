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
async fn listed_observes_only_what_the_replica_mirrors() {
    let server = MockServer::start();
    let engine = engine(&server);
    let dir = RelPath::new("");
    let mtime = httpdate::parse_http_date(MTIME).unwrap();
    let entry = |name: &str, size: u64| crate::sdk::FileInfo {
        name: name.into(),
        kind: crate::sdk::FileType::File,
        size: Some(size),
        mtime: Some(mtime),
    };

    let mirrored = RelPath::new("mirrored");
    engine.tree().write("mirrored", b"12345");
    backdate(&engine.tree().dir.join("mirrored"), mtime);
    let phantom = RelPath::new("phantom");

    engine.listed(&dir, &[entry("mirrored", 5), entry("phantom", 9)]);
    assert_eq!(
        engine.observed(&mirrored),
        Some(observed(5)),
        "local bytes match the listing, so they agreed"
    );
    assert_eq!(
        engine.observed(&phantom),
        None,
        "nothing local, nothing agreed"
    );

    engine.listed(&dir, &[entry("mirrored", 23)]);
    assert_eq!(
        engine.observed(&mirrored),
        Some(observed(5)),
        "the server moved on; the baseline must keep the last agreement so freshen sees the drift"
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
