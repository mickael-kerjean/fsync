#[test]
fn percent_decoding() {
    assert_eq!(super::percent_decode("My%20Docs"), "My Docs");
    assert_eq!(super::percent_decode("plain"), "plain");
    assert_eq!(super::percent_decode("bad%2"), "bad%2");
    assert_eq!(super::percent_decode("caf%C3%A9"), "café");
}
