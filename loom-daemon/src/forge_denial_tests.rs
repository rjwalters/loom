//! Tests for [`super`] (#10633).

use super::*;

fn headers(remaining: Option<u64>, retry_after: Option<u64>) -> RateLimitHeaders {
    RateLimitHeaders {
        remaining,
        retry_after_secs: retry_after,
        ..RateLimitHeaders::default()
    }
}

const NOT_ACCESSIBLE: &str = r#"{"message":"Resource not accessible by integration","documentation_url":"https://docs.github.com/rest/commits/statuses#get-the-combined-status-for-a-specific-reference","status":"403"}"#;

#[test]
fn a_403_resource_not_accessible_is_a_permission_denial() {
    assert_eq!(classify(Some(403), NOT_ACCESSIBLE, None), Some(Denial::Permission));
    // gh's stderr form, with no --include status.
    assert_eq!(
        classify(None, "gh: Resource not accessible by integration (HTTP 403)", None),
        Some(Denial::Permission)
    );
    assert_eq!(
        classify(Some(403), "Must have admin rights to Repository.", None),
        Some(Denial::Permission)
    );
}

#[test]
fn a_secondary_rate_limit_is_never_a_permission_denial() {
    let body = r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}"#;
    assert_eq!(classify(Some(403), body, None), Some(Denial::SecondaryRateLimit));
    // Retry-After alone marks it, whatever the body says.
    assert_eq!(
        classify(Some(403), NOT_ACCESSIBLE, Some(&headers(Some(10), Some(60)))),
        Some(Denial::SecondaryRateLimit)
    );
    assert_eq!(
        classify(Some(429), "", Some(&headers(None, Some(5)))),
        Some(Denial::SecondaryRateLimit)
    );
    assert!(Denial::SecondaryRateLimit.is_transient());
}

#[test]
fn an_exhausted_primary_pool_is_a_rate_limit() {
    assert_eq!(
        classify(
            Some(403),
            r#"{"message":"API rate limit exceeded for installation ID 1."}"#,
            None
        ),
        Some(Denial::RateLimit)
    );
    assert_eq!(
        classify(Some(403), "{}", Some(&headers(Some(0), None))),
        Some(Denial::RateLimit)
    );
    assert_eq!(classify(Some(429), "", None), Some(Denial::RateLimit));
    assert!(Denial::RateLimit.is_transient());
    assert!(!Denial::Permission.is_transient());
}

#[test]
fn a_403_refusing_the_action_itself_is_forbidden_not_permission() {
    assert_eq!(
        classify(Some(403), r#"{"message":"This workflow is already running"}"#, None),
        Some(Denial::Forbidden)
    );
    assert!(!Denial::Forbidden.is_transient());
    // No message at all: the conservative reading is a missing permission.
    assert_eq!(classify(Some(403), "", None), Some(Denial::Permission));
}

#[test]
fn a_401_is_a_credential_denial() {
    assert_eq!(
        classify(Some(401), r#"{"message":"Bad credentials"}"#, None),
        Some(Denial::Credential)
    );
    assert_eq!(classify(None, "gh: Bad credentials (HTTP 401)", None), Some(Denial::Credential));
}

#[test]
fn non_refusals_are_not_classified() {
    for status in [200, 304, 404, 422, 500, 502] {
        assert_eq!(classify(Some(status), NOT_ACCESSIBLE, None), None, "{status}");
    }
    assert_eq!(classify(None, "connection reset", None), None);
}

#[test]
fn the_needed_permission_is_named_per_endpoint() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        permission_for(&format!("repos/o/r/commits/{sha}/status?per_page=100"), false),
        Some("statuses:read")
    );
    assert_eq!(
        permission_for(&format!("repos/o/r/commits/{sha}/check-runs?per_page=100"), false),
        Some("checks:read")
    );
    assert_eq!(
        permission_for("repos/o/r/actions/runs/9/rerun-failed-jobs", true),
        Some("actions:write")
    );
    assert_eq!(permission_for("repos/o/r/actions/jobs/9/rerun", true), Some("actions:write"));
    assert_eq!(permission_for("repos/o/r/pulls/42", false), Some("pull_requests:read"));
    assert_eq!(permission_for("repos/o/r", false), None);
}

#[test]
fn describe_names_class_permission_and_message_on_one_line() {
    let line = describe(
        Denial::Permission,
        "repos/o/r/commits/abc/status",
        false,
        body_message(NOT_ACCESSIBLE).as_deref(),
    );
    assert_eq!(line, "permission (needs statuses:read): Resource not accessible by integration");
    assert_eq!(
        describe(Denial::SecondaryRateLimit, "repos/o/r/commits/abc/status", false, Some("a\nb")),
        "secondary-rate-limit: a b"
    );
    assert_eq!(body_message("not json"), None);
}
