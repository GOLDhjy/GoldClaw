use super::origin_allowed;

#[test]
fn origin_filter_accepts_localhost() {
    let allowed = vec![
        "http://127.0.0.1".to_string(),
        "http://localhost".to_string(),
    ];
    assert!(origin_allowed("http://localhost:3000", &allowed));
    assert!(!origin_allowed("https://example.com", &allowed));
}
