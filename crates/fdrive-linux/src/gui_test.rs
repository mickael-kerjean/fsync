use super::normalize_server;

#[test]
fn bare_host_defaults_to_https() {
    assert_eq!(
        normalize_server("files.example.com"),
        "https://files.example.com"
    );
}

#[test]
fn scheme_and_port_are_kept() {
    assert_eq!(
        normalize_server("http://localhost:8334"),
        "http://localhost:8334"
    );
}

#[test]
fn whitespace_and_trailing_slashes_are_trimmed() {
    assert_eq!(
        normalize_server(" http://localhost:8334// "),
        "http://localhost:8334"
    );
}
