//! Unit tests for [`crate::forge_call_stats`] (#9251).

#![allow(clippy::unwrap_used)]

use super::*;
use crate::forge_listing::parse_http_response;

fn line(t: i64, caller: &str, p: Pool, o: Outcome, rem: Option<u64>) -> String {
    serde_json::to_string(&SinkLine {
        t,
        c: caller.to_string(),
        p,
        o,
        rem,
        rst: rem.map(|_| t + 600),
    })
    .unwrap()
}

// ===== header parse (200 and 304 both carry the free budget reading) =====

#[test]
fn rate_limit_headers_parse_on_a_200() {
    let raw = "HTTP/2.0 200 OK\r\nEtag: W/\"a\"\r\nX-Ratelimit-Remaining: 4034\r\n\
               X-Ratelimit-Reset: 1785356436\r\nX-Ratelimit-Resource: core\r\n\r\n[]";
    let r = parse_http_response(raw).unwrap();
    assert_eq!(r.ratelimit.resource.as_deref(), Some("core"));
    assert_eq!(r.ratelimit.remaining, Some(4034));
    assert_eq!(r.ratelimit.reset_epoch, Some(1_785_356_436));
    assert_eq!(r.etag.as_deref(), Some("W/\"a\""));
}

#[test]
fn rate_limit_headers_parse_on_a_304() {
    let raw = "HTTP/2.0 304 Not Modified\r\nx-ratelimit-resource: graphql\r\n\
               x-ratelimit-remaining: 12\r\n\r\n";
    let r = parse_http_response(raw).unwrap();
    assert_eq!(r.status, 304);
    assert_eq!(r.ratelimit.resource.as_deref(), Some("graphql"));
    assert_eq!(r.ratelimit.remaining, Some(12));
    assert_eq!(r.ratelimit.reset_epoch, None);
}

// ===== classification =====

#[test]
fn pool_classification_from_the_resource_header() {
    assert_eq!(Pool::from_resource("core"), Pool::Core);
    assert_eq!(Pool::from_resource(" GraphQL "), Pool::Graphql);
    assert_eq!(Pool::from_resource("search"), Pool::Search);
    assert_eq!(Pool::from_resource("code_scanning_upload"), Pool::Other);
    // No headers at all (gh failed before a response): REST defaults to core.
    assert_eq!(classify(None, false, "boom").0, Pool::Core);
    let gql = parse_http_response("HTTP/2.0 200 OK\r\nX-Ratelimit-Resource: graphql\r\n\r\n{}");
    assert_eq!(classify(gql.as_ref(), true, "").0, Pool::Graphql);
}

#[test]
fn outcome_classification() {
    let ok = parse_http_response("HTTP/2.0 200 OK\r\n\r\n[]");
    let nm = parse_http_response("HTTP/2.0 304 Not Modified\r\n\r\n");
    let limited = parse_http_response("HTTP/2.0 429 Too Many\r\n\r\n");
    let exhausted =
        parse_http_response("HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\n\r\n{}");
    let forbidden = parse_http_response("HTTP/2.0 403 Forbidden\r\n\r\n{}");
    // gh exits non-zero on a 304: the status line, not the exit code, decides.
    assert_eq!(
        classify(nm.as_ref(), false, "gh: Not Modified (HTTP 304)").1,
        Outcome::NotModified
    );
    assert_eq!(classify(ok.as_ref(), true, "").1, Outcome::Ok);
    assert_eq!(classify(ok.as_ref(), false, "").1, Outcome::Error);
    assert_eq!(classify(limited.as_ref(), false, "").1, Outcome::RateLimited);
    assert_eq!(classify(exhausted.as_ref(), false, "").1, Outcome::RateLimited);
    assert_eq!(classify(forbidden.as_ref(), false, "").1, Outcome::Error);
    assert_eq!(
        classify(None, false, "gh: API rate limit exceeded for user ID 1 (HTTP 403)").1,
        Outcome::RateLimited
    );
    assert_eq!(classify(None, false, "gh: Not Found (HTTP 404)").1, Outcome::Error);
}

// ===== rolling window with an injected clock =====

#[test]
fn window_counts_only_lines_inside_the_window() {
    let now = 1_800_000_000;
    let lines = [
        line(now - WINDOW_SECS - 1, "work_finder", Pool::Core, Outcome::Ok, None),
        line(now - WINDOW_SECS, "work_finder", Pool::Core, Outcome::NotModified, None),
        line(now - 10, "work_finder", Pool::Core, Outcome::NotModified, Some(4000)),
        line(now - 5, "work_finder", Pool::Core, Outcome::Ok, Some(3999)),
        line(now - 5, "pipeline_snapshot", Pool::Graphql, Outcome::RateLimited, None),
        "not json".to_string(),
    ];
    let agg = aggregate_lines(lines.iter().map(String::as_str), now - WINDOW_SECS);
    let rows = agg.rows();
    assert_eq!(rows.len(), 2);
    let wf = rows.iter().find(|r| r.caller == "work_finder").unwrap();
    assert_eq!((wf.ok, wf.not_modified, wf.rate_limited, wf.error), (1, 2, 0, 0));
    assert_eq!(wf.pool, "core");
    let ps = rows
        .iter()
        .find(|r| r.caller == "pipeline_snapshot")
        .unwrap();
    assert_eq!((ps.pool.as_str(), ps.rate_limited), ("graphql", 1));
    // The newest header reading wins.
    assert_eq!(agg.latest[&Pool::Core].remaining, 3999);
}

#[test]
fn sink_round_trips_prunes_old_hours_and_feeds_status() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let private = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir.path(), private).unwrap();
    }
    let now = Utc::now().timestamp();
    let hour = now.div_euclid(3600);
    // A stale file far outside retention, and one inside the window.
    std::fs::write(sink_file(dir.path(), hour - RETAIN_HOURS - 2), "x\n").unwrap();
    let old = line(now - 7200, "role_collision", Pool::Core, Outcome::Ok, None);
    std::fs::write(sink_file(dir.path(), hour - 2), format!("{old}\n")).unwrap();

    set_test_sink_dir(Some(dir.path().to_path_buf()));
    let headers = RateLimitHeaders {
        resource: Some("core".into()),
        remaining: Some(4321),
        reset_epoch: Some(now + 900),
    };
    record("test_sink_caller", Pool::Core, Outcome::NotModified, Some(&headers));
    record("test_sink_caller", Pool::Core, Outcome::Ok, None);
    let report = status_report(Utc::now(), None);
    set_test_sink_dir(None);

    assert!(
        !sink_file(dir.path(), hour - RETAIN_HOURS - 2).exists(),
        "creating a new hour's file prunes files past retention"
    );
    let window = report.host_window.expect("sink enabled on this thread");
    let row = window
        .iter()
        .find(|r| r.caller == "test_sink_caller")
        .unwrap();
    assert_eq!((row.ok, row.not_modified), (1, 1));
    assert!(window.iter().all(|r| r.caller != "role_collision"), "2h-old line is outside");
    let core = report.budget.iter().find(|b| b.pool == "core").unwrap();
    assert_eq!(core.source, "headers");
    // Since-start totals are process-wide; this test's unique caller is there.
    assert!(report
        .since_start
        .iter()
        .any(|r| r.caller == "test_sink_caller"));
}

#[test]
fn no_sink_means_no_host_window() {
    // Test threads default to no sink: nothing is written to a real host dir.
    assert!(status_report(Utc::now(), None).host_window.is_none());
}

#[test]
fn a_newer_breaker_probe_overrides_an_older_header_reading() {
    let probed_at = Utc::now() + chrono::Duration::hours(1);
    let snap = crate::rate_limit_breaker::RateLimitSnapshot {
        enabled: true,
        phase: crate::rate_limit_breaker::BreakerPhase::Closed,
        suppressed: false,
        source: None,
        tripped_at: None,
        cooldown_until: None,
        trips_total: 1,
        core_remaining: Some(7),
        graphql_remaining: Some(8),
        budget_probed_at: Some(probed_at),
    };
    let report = status_report(Utc::now(), Some(&snap));
    let gql = report.budget.iter().find(|b| b.pool == "graphql").unwrap();
    assert_eq!((gql.remaining, gql.source.as_str()), (8, "breaker_probe"));
    let core = report.budget.iter().find(|b| b.pool == "core").unwrap();
    assert_eq!((core.remaining, core.observed_at), (7, probed_at));
}
