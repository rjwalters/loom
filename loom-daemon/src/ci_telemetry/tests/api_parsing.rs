//! `Link` header and raw `gh api --include` parsing (Issue #8824), moved out
//! of `super` when #9088 needed its two lines: `tests.rs` is at the
//! file-size ratchet.

use crate::ci_telemetry::api::{normalise_api_path, parse_next_link, parse_raw};

#[test]
fn link_header_parsing_and_path_normalisation() {
    let link = r#"<https://api.github.com/organizations/9/repos?page=2>; rel="next", <https://api.github.com/organizations/9/repos?page=5>; rel="last""#;
    assert_eq!(parse_next_link(link).as_deref(), Some("organizations/9/repos?page=2"));
    assert_eq!(parse_next_link(r#"<https://x/y?page=1>; rel="prev""#), None);
    assert_eq!(normalise_api_path("https://ghe.example/api/v3/repos/o/r"), "repos/o/r");
    let raw = "HTTP/2.0 200 OK\r\nETag: W/\"e1\"\r\nLink: <https://api.github.com/repos/o/r/actions/runs?page=2>; rel=\"next\"\r\nX-RateLimit-Remaining: 42\r\n\r\n{\"workflow_runs\":[]}";
    let response = parse_raw(raw).unwrap();
    assert_eq!(response.etag.as_deref(), Some("W/\"e1\""));
    assert_eq!(response.next.as_deref(), Some("repos/o/r/actions/runs?page=2"));
    assert_eq!(response.ratelimit_remaining, Some(42));
    assert_eq!(response.body, "{\"workflow_runs\":[]}");
}
