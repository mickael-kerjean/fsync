use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

fn tmp() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "fdrive-config-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn session_round_trip() {
    let data = tmp();
    assert!(super::recall(&data).is_none());
    super::remember(&data, "https://demo.filestash.app", "TOKEN", false);
    let session = super::recall(&data).unwrap();
    assert_eq!(session.url, "https://demo.filestash.app");
    assert_eq!(session.token, "TOKEN");
    assert!(!session.insecure);
    super::forget(&data);
    assert!(super::recall(&data).is_none());
    assert_eq!(
        super::recall_server(&data).as_deref(),
        Some("https://demo.filestash.app"),
        "logout keeps the server url for the next login"
    );
}

#[test]
fn forget_without_a_session_removes_an_empty_config() {
    let data = tmp();
    super::forget(&data);
    assert!(super::recall_server(&data).is_none());
    assert!(!data.join("fdrive.toml").exists(), "empty config is removed");
}

#[test]
fn login_and_logout_leave_the_rest_of_the_config_alone() {
    let data = tmp();
    let config = data.join("fdrive.toml");
    std::fs::write(&config, "[sync]\nignore = [\"node_modules\"]\n").unwrap();
    super::remember(&data, "https://x", "T", true);
    assert!(super::recall(&data).unwrap().insecure);
    super::forget(&data);
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("node_modules"), "foreign table survives: {text}");
    assert!(!text.contains("\"T\""), "token is gone: {text}");
    assert!(super::recall(&data).is_none(), "no usable session remains");
    assert_eq!(super::recall_server(&data).as_deref(), Some("https://x"));
}

#[test]
fn ignore_defaults_cover_the_usual_junk() {
    let data = tmp();
    let ignore = super::ignore(&data);
    assert!(ignore.matches(&crate::path::RelPath::new("a/node_modules/b/c.js")));
    assert!(ignore.matches(&crate::path::RelPath::new(".DS_Store")));
    assert!(!ignore.matches(&crate::path::RelPath::new("src/main.rs")));
    assert!(!ignore.matches(&crate::path::RelPath::new("")));
}

#[test]
fn ignore_is_configurable_and_an_empty_list_disables_it() {
    let data = tmp();
    std::fs::write(data.join("fdrive.toml"), "[sync]\nignore = [\"target\"]\n").unwrap();
    let ignore = super::ignore(&data);
    assert!(ignore.matches(&crate::path::RelPath::new("target/debug")));
    assert!(!ignore.matches(&crate::path::RelPath::new(".DS_Store")));

    std::fs::write(data.join("fdrive.toml"), "[sync]\nignore = []\n").unwrap();
    assert!(!super::ignore(&data).matches(&crate::path::RelPath::new(".DS_Store")));
}

#[test]
fn login_leaves_the_ignore_rules_alone() {
    let data = tmp();
    std::fs::write(data.join("fdrive.toml"), "[sync]\nignore = [\"target\"]\n").unwrap();
    super::remember(&data, "https://x", "T", false);
    assert!(super::ignore(&data).matches(&crate::path::RelPath::new("target")));
    assert_eq!(super::recall(&data).unwrap().token, "T");
}

#[test]
fn an_unchanged_login_does_not_rewrite_the_file() {
    let data = tmp();
    super::remember(&data, "https://x", "T", false);
    let config = data.join("fdrive.toml");
    let annotated = format!("# user note\n{}", std::fs::read_to_string(&config).unwrap());
    std::fs::write(&config, &annotated).unwrap();
    super::remember(&data, "https://x", "T", false);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), annotated);
}
