use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use fdrive_core::path::RelPath;
use fdrive_core::sdk::Sdk;
use fdrive_linux::adapter::Adapter;
use httpmock::prelude::*;
use tokio::runtime::Runtime;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fdrive-linux-test-{}-{}",
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

fn adapter(server: &MockServer, data: &TempDir, rt: &Runtime) -> Adapter {
    let mut sdk = Sdk::new(&server.base_url()).unwrap();
    sdk.set_token("TOKEN".into());
    Adapter::new(Arc::new(sdk), rt.handle().clone(), &data.0).unwrap()
}

fn ls_mock<'a>(server: &'a MockServer, path: &str, entries: &str) -> httpmock::Mock<'a> {
    let body = format!("{{\"status\": \"ok\", \"results\": [{entries}]}}");
    server.mock(move |when, then| {
        when.method(GET)
            .path("/api/files/ls")
            .query_param("path", path);
        then.status(200)
            .json_body_obj(&serde_json::from_str::<serde_json::Value>(&body).unwrap());
    })
}

#[test]
fn rmdir_vetoes_a_directory_that_is_not_empty_on_the_server() {
    let server = MockServer::start();
    ls_mock(
        &server,
        "/d/",
        r#"{"name": "precious.txt", "size": 3, "time": 0, "type": "file"}"#,
    );
    let rm = server.mock(|when, then| {
        when.method(POST).path("/api/files/rm");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let (rt, data) = (Runtime::new().unwrap(), TempDir::new());
    let adapter = adapter(&server, &data, &rt);

    let err = adapter.rmdir(&RelPath::new("d")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOTEMPTY));
    rm.assert_hits(0);
}

#[test]
fn rmdir_of_an_empty_directory_deletes_it() {
    let server = MockServer::start();
    ls_mock(&server, "/d/", "");
    let rm = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/rm")
            .query_param("path", "/d/");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    let (rt, data) = (Runtime::new().unwrap(), TempDir::new());
    let adapter = adapter(&server, &data, &rt);

    adapter.rmdir(&RelPath::new("d")).unwrap();
    rm.assert_hits(1);
}

#[test]
fn a_delete_storm_lists_each_directory_once() {
    let server = MockServer::start();
    let ls = ls_mock(
        &server,
        "/d/",
        r#"{"name": "a.txt", "size": 1, "time": 0, "type": "file"},
           {"name": "b.txt", "size": 1, "time": 0, "type": "file"},
           {"name": "c.txt", "size": 1, "time": 0, "type": "file"}"#,
    );
    let rm = server.mock(|when, then| {
        when.method(POST).path("/api/files/rm");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}));
    });
    // the removes verify their lease before deleting
    server.mock(|when, then| {
        when.method(httpmock::Method::HEAD).path("/api/files/cat");
        then.status(200).header("content-length", "1");
    });
    let (rt, data) = (Runtime::new().unwrap(), TempDir::new());
    let adapter = adapter(&server, &data, &rt);

    adapter.ls(&RelPath::new("d")).unwrap();
    for name in ["d/a.txt", "d/b.txt", "d/c.txt"] {
        adapter.delete(&RelPath::new(name), false).unwrap();
    }
    adapter.rmdir(&RelPath::new("d")).unwrap();
    ls.assert_hits(1);
    rm.assert_hits(4);
}

#[test]
fn rmdir_of_a_directory_gone_from_the_server_succeeds() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api/files/ls");
        then.status(404);
    });
    server.mock(|when, then| {
        when.method(POST).path("/api/files/rm");
        then.status(404);
    });
    let (rt, data) = (Runtime::new().unwrap(), TempDir::new());
    let adapter = adapter(&server, &data, &rt);

    adapter.rmdir(&RelPath::new("d")).unwrap();
}
