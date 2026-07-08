use fsync_core::byte_stream;
use fsync_core::sdk::{Error, FileType, Sdk};
use futures_util::TryStreamExt;
use httpmock::prelude::*;

async fn authed_client(server: &MockServer) -> Sdk {
    let mut client = Sdk::new(&server.base_url()).unwrap();
    client.set_token("TOKEN".into());
    client
}

#[tokio::test]
async fn authenticate_reassembles_split_cookies() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/session/auth/")
            .query_param("label", "my-storage")
            .header("X-Requested-With", "SDKHttpRequest")
            .body_contains("user=alice")
            .body_contains("password=secret");
        then.status(302)
            .header("Set-Cookie", "auth=part1; Path=/; HttpOnly")
            .header("Set-Cookie", "auth1=part2; Path=/; HttpOnly");
    });

    let mut client = Sdk::new(&server.base_url()).unwrap();
    client
        .authenticate("alice", "secret", "my-storage")
        .await
        .unwrap();
    mock.assert();
    assert_eq!(client.token(), Some("part1part2"));
}

#[tokio::test]
async fn authenticate_rejects_bad_credentials() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/api/session/auth/");
        then.status(403);
    });

    let mut client = Sdk::new(&server.base_url()).unwrap();
    let err = client
        .authenticate("alice", "wrong", "s")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidCredentials));
}

#[tokio::test]
async fn authenticate_without_cookie_is_rejected() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/api/session/auth/");
        then.status(200);
    });

    let mut client = Sdk::new(&server.base_url()).unwrap();
    let err = client.authenticate("alice", "pw", "s").await.unwrap_err();
    assert!(matches!(err, Error::InvalidCredentials));
}

#[tokio::test]
async fn probe_accepts_filestash_and_returns_version() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/about");
        then.status(200)
            .header("X-Powered-By", "Filestash/v0.6.20260615 <https://filestash.app>");
    });

    let version = Sdk::builder(&server.base_url()).probe().await.unwrap();
    assert_eq!(version, "v0.6.20260615");
}

#[tokio::test]
async fn probe_rejects_a_random_website() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/about");
        then.status(200).header("X-Powered-By", "Express");
    });

    let err = Sdk::builder(&server.base_url()).probe().await.unwrap_err();
    assert!(matches!(err, Error::NotFilestash));
}

#[tokio::test]
async fn ls_parses_envelope() {
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

    let files = authed_client(&server).await.ls("/docs/").await.unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].name, "report.pdf");
    assert_eq!(files[0].kind, FileType::File);
    assert_eq!(files[0].size, Some(1024));
    assert!(files[0].mtime.is_some());
    assert_eq!(files[1].kind, FileType::Directory);
    assert_eq!(files[1].mtime, None, "time=0 means unknown");
}

#[tokio::test]
async fn ls_rejects_error_status_in_envelope() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api/files/ls");
        then.status(200)
            .json_body(serde_json::json!({"status": "error", "results": null}));
    });

    let err = authed_client(&server).await.ls("/").await.unwrap_err();
    assert!(matches!(err, Error::Api(_)));
}

#[tokio::test]
async fn stat_reads_head_headers() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method("HEAD")
            .path("/api/files/cat")
            .query_param("path", "/docs/report.pdf");
        then.status(200)
            .header("Content-Length", "2048")
            .header("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT");
    });

    let info = authed_client(&server)
        .await
        .stat("/docs/report.pdf")
        .await
        .unwrap();
    assert_eq!(info.name, "report.pdf");
    assert_eq!(info.kind, FileType::File);
    assert_eq!(info.size, Some(2048));
    assert!(info.mtime.is_some());
}

#[tokio::test]
async fn stat_detects_directories() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method("HEAD").path("/api/files/cat");
        then.status(200).header("Content-Type", "inode/directory");
    });

    let info = authed_client(&server).await.stat("/docs/").await.unwrap();
    assert_eq!(info.name, "docs");
    assert_eq!(info.kind, FileType::Directory);
}

#[tokio::test]
async fn cat_streams_content() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET)
            .path("/api/files/cat")
            .query_param("path", "/hello.txt");
        then.status(200).body("hello world");
    });

    let stream = authed_client(&server)
        .await
        .cat("/hello.txt")
        .await
        .unwrap();
    let chunks: Vec<_> = stream.try_collect().await.unwrap();
    assert_eq!(chunks.concat(), b"hello world");
}

#[tokio::test]
async fn save_posts_body() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/cat")
            .query_param("path", "/new.txt")
            .header("Authorization", "Bearer TOKEN")
            .body("content");
        then.status(200);
    });

    authed_client(&server)
        .await
        .save("/new.txt", byte_stream("content"))
        .await
        .unwrap();
    mock.assert();
}

#[tokio::test]
async fn mv_rm_mkdir_hit_expected_endpoints() {
    let server = MockServer::start();
    let mv = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/mv")
            .query_param("from", "/a.txt")
            .query_param("to", "/b.txt");
        then.status(200);
    });
    let rm = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/rm")
            .query_param("path", "/b.txt");
        then.status(200);
    });
    let mkdir = server.mock(|when, then| {
        when.method(POST)
            .path("/api/files/mkdir")
            .query_param("path", "/dir/");
        then.status(200);
    });

    let client = authed_client(&server).await;
    client.mv("/a.txt", "/b.txt").await.unwrap();
    client.rm("/b.txt").await.unwrap();
    client.mkdir("/dir/").await.unwrap();
    mv.assert();
    rm.assert();
    mkdir.assert();
}

#[tokio::test]
async fn http_errors_are_mapped() {
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

    let client = authed_client(&server).await;
    assert!(matches!(
        client.ls("/gone/").await.unwrap_err(),
        Error::NotFound
    ));
    assert!(matches!(
        client.ls("/secret/").await.unwrap_err(),
        Error::PermissionDenied
    ));
}

#[tokio::test]
async fn requests_without_token_fail_locally() {
    let server = MockServer::start();
    let client = Sdk::new(&server.base_url()).unwrap();
    assert!(matches!(
        client.ls("/").await.unwrap_err(),
        Error::NotAuthenticated
    ));
}
