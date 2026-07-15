use httpmock::{Method, MockServer};

use super::testkit::*;
use crate::path::RelPath;

#[tokio::test]
async fn the_scheduler_replays_concurrently() {
    let server = MockServer::start();
    let save = server.mock(|when, then| {
        when.method(Method::POST).path("/api/files/cat");
        then.status(200)
            .json_body(serde_json::json!({"status": "ok"}))
            .delay(std::time::Duration::from_millis(200));
    });
    let engine = engine(&server);
    let started = std::time::Instant::now();
    for name in ["a", "b", "c", "d"] {
        let path = RelPath::new(name);
        engine.tree().write(name, b"x");
        engine.created(&path);
        engine.modified(&path);
    }
    settle(&engine).await;
    save.assert_hits(4);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(700),
        "4 saves at 200ms each finished in {:?}, so they overlapped",
        started.elapsed()
    );
}
