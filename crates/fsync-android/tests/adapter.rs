use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use fsync_android::{Adapter, EntryKind, FsError};
use httpmock::prelude::*;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fsync-android-test-{}-{}",
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
fn login_returns_reassembled_token() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/session/auth/")
            .query_param("label", "my-storage")
            .body_contains("user=alice")
            .body_contains("password=secret");
        then.status(302)
            .header("Set-Cookie", "auth=part1; Path=/; HttpOnly")
            .header("Set-Cookie", "auth1=part2; Path=/; HttpOnly");
    });

    let token = fsync_android::login(
        server.base_url(),
        false,
        "alice".into(),
        "secret".into(),
        "my-storage".into(),
    )
    .unwrap();
    mock.assert();
    assert_eq!(token, "part1part2");
}

#[test]
fn login_rejects_bad_credentials() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/api/session/auth/");
        then.status(403);
    });

    let err = fsync_android::login(
        server.base_url(),
        false,
        "alice".into(),
        "wrong".into(),
        "s".into(),
    )
    .unwrap_err();
    assert!(matches!(err, FsError::InvalidCredentials));
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
    assert_eq!(entries[0].size, Some(1024));
    assert_eq!(entries[0].mtime_ms, Some(1700000000000));
    assert_eq!(entries[1].kind, EntryKind::Directory);
    assert_eq!(entries[1].mtime_ms, None, "time=0 means unknown");
}

#[test]
fn ls_overlays_local_only_files() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api/files/ls");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok", "results": []}));
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    adapter.create("/draft.txt".into()).unwrap();
    let entries = adapter.ls("/".into()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "draft.txt");
}

#[test]
fn open_caches_the_download() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method("HEAD")
            .path("/api/files/cat")
            .query_param("path", "/hello.txt");
        then.status(200)
            .header("Content-Length", "11")
            .header("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT");
    });
    let cat = server.mock(|when, then| {
        when.method(GET)
            .path("/api/files/cat")
            .query_param("path", "/hello.txt");
        then.status(200).body("hello world");
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    let local = adapter.open("/hello.txt".into()).unwrap();
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "hello world");
    let again = adapter.open("/hello.txt".into()).unwrap();
    assert_eq!(again, local);
    cat.assert_hits(1);
}

#[test]
fn saved_uploads_the_edit() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/cat")
            .query_param("path", "/note.txt");
        then.status(200);
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    let local = adapter.create("/note.txt".into()).unwrap();
    std::fs::write(&local, b"jotted").unwrap();
    adapter.saved("/note.txt".into());
    adapter.flush(5_000);
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
fn errors_are_mapped() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/api/files/ls")
            .query_param("path", "/gone/");
        then.status(404);
    });
    server.mock(|when, then| {
        when.method(GET)
            .path("/api/files/ls")
            .query_param("path", "/secret/");
        then.status(403);
    });

    let data = TempDir::new();
    let adapter = adapter(&server, &data);
    assert!(matches!(
        adapter.ls("/gone/".into()).unwrap_err(),
        FsError::NotFound
    ));
    assert!(matches!(
        adapter.ls("/secret/".into()).unwrap_err(),
        FsError::PermissionDenied
    ));
}

#[test]
#[ignore]
fn against_real_server() {
    let env = |name: &str| std::env::var(name).expect(name);
    let token = fsync_android::login(
        env("FILESTASH_URL"),
        false,
        env("FILESTASH_USER"),
        env("FILESTASH_PASSWORD"),
        env("FILESTASH_STORAGE"),
    )
    .unwrap();
    assert!(!token.is_empty());
    let data = TempDir::new();
    let adapter = Adapter::new(
        env("FILESTASH_URL"),
        false,
        token,
        data.0.to_str().unwrap().into(),
    )
    .unwrap();
    let entries = adapter.ls("/".into()).unwrap();
    println!("ls / -> {} entries", entries.len());
    if let Some(file) = entries.iter().find(|e| e.kind == EntryKind::File) {
        let local = adapter.open(format!("/{}", file.name)).unwrap();
        println!("cached {} at {local}", file.name);
    }
}
