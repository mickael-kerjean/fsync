use super::assemble_token;

fn cookies(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

#[test]
fn single_auth_cookie() {
    let token = assemble_token(&cookies(&[("lang", "en"), ("auth", "abc")]));
    assert_eq!(token, "abc");
}

#[test]
fn chunked_cookies_join_in_name_order() {
    let token = assemble_token(&cookies(&[
        ("auth2", "c"),
        ("auth", "a"),
        ("auth10", "d"),
        ("auth1", "b"),
    ]));
    assert_eq!(token, "abcd");
}

#[test]
fn unrelated_and_malformed_names_are_ignored() {
    let token = assemble_token(&cookies(&[
        ("authx", "nope"),
        ("author", "nope"),
        ("ssl", "nope"),
    ]));
    assert_eq!(token, "");
}
