use super::*;

#[test]
fn ls_serves_the_stale_listing_when_the_server_is_unreachable() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let data = std::env::temp_dir().join(format!("fdrive-stale-ls-{}", std::process::id()));
    fs::create_dir_all(&data).unwrap();
    let sdk = Sdk::new("http://127.0.0.1:9").unwrap();
    let adapter = Adapter::new(Arc::new(sdk), rt.handle().clone(), &data).unwrap();

    let dir = RelPath::new("d");
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(600))
        .unwrap();
    adapter.engine.tree().meta.lock().unwrap().insert(
        dir.clone(),
        (
            expired,
            vec![FileInfo {
                name: "a.txt".to_string(),
                kind: FileType::File,
                size: Some(1),
                mtime: None,
            }],
        ),
    );

    let listing = adapter.ls(&dir).unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "a.txt");
    assert!(adapter.ls(&RelPath::new("never-seen")).is_err());
    let _ = fs::remove_dir_all(&data);
}
