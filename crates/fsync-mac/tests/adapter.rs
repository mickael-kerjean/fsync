use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use fsync_mac::{Adapter, EntryKind, FsError, SyncState};
use httpmock::prelude::*;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fsync-mac-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn adapter(server: &MockServer, data: &TempDir) -> Arc<Adapter> {
    Adapter::new(
        server.base_url(),
        false,
        "TOKEN".into(),
        data.0.to_str().unwrap().into(),
    )
    .unwrap()
}

#[test]
fn ls_maps_entries() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/api/files/ls")
            .query_param("path", "/docs/")
            .header("Authorization", "Bearer TOKEN");
        then.status(200).json_body(serde_json::json!({
            "status": "ok",
            "results": [
                {"name": "report.pdf", "size": 1024, "time": 1700000000000i64, "type": "file"},
                {"name": "archive", "size": 0, "time": 0, "type": "directory"},
            ]
        }));
    });

    let data = TempDir::new();
    let entries = adapter(&server, &data).ls("/docs/".into()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "report.pdf");
    assert_eq!(entries[0].kind, EntryKind::File);
    assert_eq!(entries[1].kind, EntryKind::Directory);
}

#[test]
fn ls_overlays_unpushed_creations() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api/files/ls");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok", "results": []}));
    });
    server.mock(|when, then| {
        when.method(POST).path("/api/files/cat");
        then.status(500);
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    adapter.created("/draft.txt".into(), None).unwrap();
    let entries = adapter.ls("/".into()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "draft.txt");
}

#[test]
fn fetch_streams_the_download() {
    let server = MockServer::start();
    let cat = server.mock(|when, then| {
        when.method(GET)
            .path("/api/files/cat")
            .query_param("path", "/hello.txt");
        then.status(200).body("hello world");
    });

    let data = TempDir::new();
    let dest = data.0.join("dest.tmp");
    adapter(&server, &data)
        .fetch("/hello.txt".into(), dest.to_str().unwrap().into())
        .unwrap();
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello world");
    cat.assert_hits(1);
}

#[test]
fn fetch_serves_the_dirty_copy_without_the_server() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/api/files/cat");
        then.status(500);
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    let src = data.0.join("edit.tmp");
    std::fs::write(&src, b"unpushed").unwrap();
    adapter
        .created("/note.txt".into(), Some(src.to_str().unwrap().into()))
        .unwrap();

    let dest = data.0.join("dest.tmp");
    adapter
        .fetch("/note.txt".into(), dest.to_str().unwrap().into())
        .unwrap();
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "unpushed");
}

#[test]
fn modified_uploads_and_drains_the_spool() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method("HEAD").path("/api/files/cat");
        then.status(200)
            .header("Content-Length", "3")
            .header("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT");
    });
    let save = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/cat")
            .query_param("path", "/note.txt")
            .body("edited");
        then.status(200);
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    let src = data.0.join("contents.tmp");
    std::fs::write(&src, b"edited").unwrap();
    adapter
        .modified("/note.txt".into(), src.to_str().unwrap().into())
        .unwrap();
    adapter.flush(5_000);
    save.assert_hits(1);
    assert!(
        !data.0.join("spool/note.txt").exists(),
        "committed edits leave the spool"
    );
    assert!(matches!(adapter.state(), SyncState::Idle));
}

#[test]
fn recover_requeues_spooled_edits_across_restarts() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method("HEAD").path("/api/files/cat");
        then.status(404);
    });
    let save = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/cat")
            .query_param("path", "/note.txt")
            .body("survives");
        then.status(200);
    });

    let data = TempDir::new();
    {
        let first = Adapter::new(
            "http://127.0.0.1:1".into(),
            false,
            "TOKEN".into(),
            data.0.to_str().unwrap().into(),
        )
        .unwrap();
        let src = data.0.join("contents.tmp");
        std::fs::write(&src, b"survives").unwrap();
        first
            .created("/note.txt".into(), Some(src.to_str().unwrap().into()))
            .unwrap();
    }

    let second = adapter(&server, &data);
    second.flush(5_000);
    save.assert_hits(1);
}

#[test]
fn delete_is_a_verdict() {
    let server = MockServer::start();
    let rm = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/rm")
            .query_param("path", "/docs/");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/rm")
            .query_param("path", "/locked.txt");
        then.status(500);
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    adapter.delete("/docs/".into()).unwrap();
    rm.assert_hits(1);
    assert!(adapter.delete("/locked.txt".into()).is_err());
}

#[test]
fn rename_is_a_verdict_and_moves_the_spool() {
    let server = MockServer::start();
    let mv = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/mv")
            .query_param("from", "/a.txt")
            .query_param("to", "/b.txt");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    adapter.rename("/a.txt".into(), "/b.txt".into()).unwrap();
    mv.assert_hits(1);
}

#[test]
fn errors_are_mapped() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/api/files/ls")
            .query_param("path", "/gone/");
        then.status(404);
    });

    let data = TempDir::new();
    assert!(matches!(
        adapter(&server, &data).ls("/gone/".into()).unwrap_err(),
        FsError::NotFound
    ));
}
