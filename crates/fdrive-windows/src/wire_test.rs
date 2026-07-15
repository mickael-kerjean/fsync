use super::rel_from_full;

#[test]
fn rel_path_from_callback_paths() {
    let root = r"\Users\micka\Filestash";
    assert_eq!(
        rel_from_full(root, r"\Users\micka\Filestash")
            .unwrap()
            .as_str(),
        ""
    );
    assert_eq!(
        rel_from_full(root, r"\users\MICKA\Filestash\a\b.txt")
            .unwrap()
            .as_str(),
        "a/b.txt"
    );
    assert_eq!(
        rel_from_full(root, r"C:\Users\micka\Filestash\a")
            .unwrap()
            .as_str(),
        "a"
    );
    assert!(rel_from_full(root, r"\Users\micka\FilestashOther\x").is_none());
    assert!(rel_from_full(root, r"\Users\micka\Elsewhere").is_none());
}
