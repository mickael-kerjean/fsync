use super::RelPath;

#[test]
fn normalizes() {
    assert_eq!(RelPath::new("/docs//a.txt/").as_str(), "docs/a.txt");
    assert_eq!(RelPath::new("docs/a.txt").name(), "a.txt");
    assert_eq!(
        RelPath::new("docs/sub/a.txt").parent(),
        Some(RelPath::new("docs/sub"))
    );
    assert_eq!(RelPath::new("a.txt").parent(), None);
}

#[test]
fn never_escapes_the_root() {
    assert_eq!(RelPath::new("../../etc/passwd").as_str(), "etc/passwd");
    assert_eq!(RelPath::new("a/../b").as_str(), "a/b");
    assert_eq!(RelPath::new("./a").as_str(), "a");
    assert_eq!(RelPath::new("docs").join("../evil").as_str(), "docs/evil");
}

#[test]
fn descendants() {
    let docs = RelPath::new("docs");
    assert!(RelPath::new("docs/a.txt").is_descendant_of(&docs));
    assert!(RelPath::new("docs/sub/a.txt").is_descendant_of(&docs));
    assert!(!RelPath::new("docs").is_descendant_of(&docs));
    assert!(!RelPath::new("docs2/a.txt").is_descendant_of(&docs));
}

#[test]
fn api_paths() {
    assert_eq!(RelPath::new("docs/a.txt").as_file(), "/docs/a.txt");
    assert_eq!(RelPath::new("docs").as_dir(), "/docs/");
    assert_eq!(RelPath::root().as_dir(), "/");
}
