use super::*;

fn parse(argv: &[&str]) -> Args {
    Args::try_parse_from([&["fdrive", "/tmp/mnt"], argv].concat()).unwrap()
}

#[test]
fn token_mode() {
    let s = setup(
        parse(&["--server", "localhost:8334", "--token", "t0k"]),
        None,
    )
    .unwrap();
    let creds = s.credentials.unwrap();
    assert_eq!(creds.url, "https://localhost:8334");
    assert_eq!(creds.token, "t0k");
    assert!(!s.prompt_login);
}

#[test]
fn password_mode() {
    let mut args = parse(&[
        "--server",
        "http://x/",
        "--user",
        "joe",
        "--storage",
        "docs",
    ]);
    args.password = Some("s3cret".into());
    let creds = setup(args, None).unwrap().credentials.unwrap();
    assert_eq!(creds.url, "http://x");
    assert_eq!(creds.user, "joe");
    assert_eq!(creds.password, "s3cret");
    assert_eq!(creds.storage, "docs");
    assert!(creds.token.is_empty());
}

#[test]
fn server_alone_prompts_login() {
    let s = setup(parse(&["--server", "http://x"]), None).unwrap();
    assert!(s.credentials.is_none());
    assert!(s.prompt_login);
    assert_eq!(s.prefill.url, "http://x");
}

#[test]
fn no_args_recalls_stored_session() {
    let stored = Credentials {
        url: "http://x".into(),
        token: "t0k".into(),
        ..Default::default()
    };
    let s = setup(parse(&[]), Some(stored)).unwrap();
    assert!(s.stored);
    assert_eq!(s.credentials.unwrap().token, "t0k");
    assert_eq!(s.mount, PathBuf::from("/tmp/mnt"));
}

#[test]
fn explicit_server_ignores_stored_session() {
    let stored = Credentials {
        url: "http://old".into(),
        token: "t0k".into(),
        ..Default::default()
    };
    let s = setup(parse(&["--server", "http://new"]), Some(stored)).unwrap();
    assert!(s.credentials.is_none());
    assert!(s.prompt_login);
}

#[test]
fn conflicting_and_orphan_flags_are_rejected() {
    let mut args = parse(&["--server", "http://x", "--user", "joe"]);
    args.token = Some("t0k".into());
    assert!(setup(args, None).is_err());
    assert!(setup(parse(&["--user", "joe"]), None).is_err());
    assert!(Args::try_parse_from(["fdrive"]).is_err());
}
