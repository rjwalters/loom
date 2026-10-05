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
        usd: None,
        rst: rem.map(|_| t + 600),
        op: None,
        pv: None,
        og: None,
        rp: None,
        ir: None,
    })
    .unwrap()
}

/// [`line`] carrying a #9777 call identity.
fn id_line(t: i64, caller: &str, o: Outcome, identity: &CallIdentity) -> String {
    serde_json::to_string(&SinkLine {
        t,
        c: caller.to_string(),
        p: Pool::Core,
        o,
        rem: None,
        usd: None,
        rst: None,
        op: Some(
            identity
                .operation
                .clone()
                .unwrap_or_else(|| UNKNOWN_OPERATION.to_string()),
        ),
        pv: identity.provider.clone(),
        og: identity.origin.clone(),
        rp: identity.repo.clone(),
        ir: identity.role.clone(),
    })
    .unwrap()
}

// ===== header parse (200 and 304 both carry the free budget reading) =====

#[test]
fn rate_limit_headers_parse_on_a_200() {
    let raw = "HTTP/2.0 200 OK\r\nEtag: W/\"a\"\r\nX-Ratelimit-Remaining: 4034\r\n\
               X-Ratelimit-Used: 966\r\nX-Ratelimit-Reset: 1785356436\r\n\
               X-Ratelimit-Resource: core\r\n\r\n[]";
    let r = parse_http_response(raw).unwrap();
    assert_eq!(r.ratelimit.resource.as_deref(), Some("core"));
    assert_eq!(r.ratelimit.remaining, Some(4034));
    assert_eq!(r.ratelimit.used, Some(966));
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
    // Own consumption: ok + error per pool (a 304 and a rate-limited call
    // cost nothing) — the denominator of #9855's external estimate.
    assert_eq!(agg.consumed_per_pool()[&Pool::Core], 1);
}

#[test]
fn a_newer_line_without_used_keeps_the_last_known_used() {
    // A pre-#9855 binary shares the sink: its lines carry no `usd`. The
    // newest reading must not erase the last known `used`.
    let now = 1_800_000_000;
    let with_used =
        format!(r#"{{"t":{},"c":"a","p":"core","o":"ok","rem":4000,"usd":1000}}"#, now - 10);
    let without_used = format!(r#"{{"t":{},"c":"a","p":"core","o":"ok","rem":3900}}"#, now - 5);
    let agg =
        aggregate_lines([with_used, without_used].iter().map(String::as_str), now - WINDOW_SECS);
    let reading = &agg.latest[&Pool::Core];
    assert_eq!((reading.remaining, reading.used), (3900, Some(1000)));
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
        used: Some(679),
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
    // The reading carries the pool-wide spend (#9855) and the status carries
    // this host's own share, so used − own is the external estimate.
    assert_eq!(core.used, Some(679));
    let own = report.own_window.expect("sink enabled on this thread");
    let own_core = own.iter().find(|r| r.pool == "core").unwrap();
    assert_eq!(own_core.consumed, 1, "the 304 costs nothing, the Ok costs one");
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
        core_used: Some(4993),
        graphql_used: Some(4992),
        budget_probed_at: Some(probed_at),
    };
    let report = status_report(Utc::now(), Some(&snap));
    let gql = report.budget.iter().find(|b| b.pool == "graphql").unwrap();
    assert_eq!((gql.remaining, gql.source.as_str()), (8, "breaker_probe"));
    let core = report.budget.iter().find(|b| b.pool == "core").unwrap();
    assert_eq!((core.remaining, core.observed_at), (7, probed_at));
    // The probe's pool-wide spend rides along (#9855).
    assert_eq!((core.used, gql.used), (Some(4993), Some(4992)));
}

#[test]
fn readings_of_two_independent_identities_stay_separate() {
    let with_role = |t: i64, role: Option<&str>, rem: u64| {
        let mut l: SinkLine =
            serde_json::from_str(&line(t, "c", Pool::Core, Outcome::Ok, Some(rem))).unwrap();
        l.ir = role.map(str::to_string);
        l
    };
    let mut agg = Aggregate::default();
    // Reader exhausted; the writer answers more recently with budget left; a
    // later line names no identity at all.
    agg.add(&with_role(100, Some("reader"), 0));
    agg.add(&with_role(200, Some("writer"), 4000));
    agg.add(&with_role(300, None, 4999));
    let reader = agg.latest_by_role[&(Pool::Core, "reader".to_string())];
    let writer = agg.latest_by_role[&(Pool::Core, "writer".to_string())];
    assert_eq!((reader.remaining, writer.remaining), (0, 4000));
    // The unattributed line updates the pool-wide reading only.
    assert_eq!(agg.latest_by_role.len(), 2);
    assert_eq!(agg.latest[&Pool::Core].remaining, 4999);
}

// ===== #9777 call identity =====
//
// These two tests are the `test_path` evidence the forge operation inventory's
// `telemetry.call-accounting` and `quota.rate-limit-reading` rows point at
// (`defaults/forge/operations/fleet-delivery.toml`). The validator checks that
// this file actually contains a test named after each row's `test_id`, so
// renaming one of these functions without updating the manifest fails
// `forge-inventory validate` rather than silently voiding the claim.

/// Covers inventory row `telemetry.call-accounting` — identity-keyed
/// accounting, including the `cross-origin-identity` high-risk case: two forges
/// with the same `owner/repo` slug must not aggregate into one row.
#[test]
fn telemetry_call_accounting() {
    let now = 1_800_000_000;
    let gh = CallIdentity::operation("issue.list")
        .with_provider("github")
        .with_origin("github.com")
        .with_repo("acme/app");
    let gitea = CallIdentity::operation("issue.list")
        .with_provider("gitea")
        .with_origin("gitea.example.com")
        .with_repo("acme/app");
    let lines = [
        id_line(now - 10, "work_finder", Outcome::Ok, &gh),
        id_line(now - 9, "work_finder", Outcome::NotModified, &gh),
        id_line(now - 8, "work_finder", Outcome::Ok, &gitea),
        // An un-migrated caller: visible as `unknown`, never dropped.
        id_line(now - 7, "legacy", Outcome::Error, &CallIdentity::default()),
    ];
    let rows = aggregate_lines(lines.iter().map(String::as_str), now - WINDOW_SECS).identity_rows();

    // Same operation, same slug, two origins => two rows, not one.
    let gh_row = rows
        .iter()
        .find(|r| r.origin.as_deref() == Some("github.com"))
        .expect("github row");
    let gitea_row = rows
        .iter()
        .find(|r| r.origin.as_deref() == Some("gitea.example.com"))
        .expect("gitea row");
    assert_eq!((gh_row.ok, gh_row.not_modified), (1, 1));
    assert_eq!((gitea_row.ok, gitea_row.not_modified), (1, 0));
    assert_eq!(gh_row.operation, gitea_row.operation);
    assert_eq!(gh_row.repo, gitea_row.repo);
    assert_ne!(gh_row.provider, gitea_row.provider);

    // The unmapped call is accounted for under `unknown`, not discarded.
    let unknown = rows
        .iter()
        .find(|r| r.operation == UNKNOWN_OPERATION)
        .expect("an unmapped caller stays visible");
    assert_eq!((unknown.error, unknown.provider.clone()), (1, None));

    // A qualified key keeps two same-numbered artifacts on two forges apart.
    assert_ne!(gh.qualified_key(Some(12)), gitea.qualified_key(Some(12)));
    assert_eq!(gh.qualified_key(Some(12)), "github:github.com/acme/app#12");
}

/// Covers inventory row `quota.rate-limit-reading` — the free per-pool budget
/// headers reach the status report, and the identity layer never records a
/// credential-shaped value even when a caller hands it one.
#[test]
fn quota_rate_limit_reading() {
    // The budget reading rides along on an ordinary response, including a 304.
    let r = parse_http_response(
        "HTTP/2.0 304 Not Modified\r\nx-ratelimit-resource: core\r\n\
         x-ratelimit-remaining: 57\r\nx-ratelimit-reset: 1785356436\r\n\r\n",
    )
    .unwrap();
    assert_eq!(r.ratelimit.remaining, Some(57));
    assert_eq!(r.ratelimit.reset_epoch, Some(1_785_356_436));
    assert_eq!(classify(Some(&r), false, "").1, Outcome::NotModified);

    // Bounded: an over-long or multi-line value is truncated to one line.
    let long = sanitize(&format!("gitea.example.com{}", "x".repeat(500))).unwrap();
    assert!(long.len() <= 96, "identity fields are length-capped");
    assert_eq!(sanitize("a\nb\tc").as_deref(), Some("a b c"));
    assert_eq!(sanitize("   ").as_deref(), None);

    // Never a credential: a header or token shape records NOTHING rather than
    // a secret, and the drop is observable through the public constructor.
    //
    // The full-length PAT shape is ASSEMBLED rather than written as a literal.
    // `loom-daemon secret-scan` (the PreToolUse guard, .githooks/, and CI's
    // Secret Scan gate all run it) correctly flags a literal one even inside a
    // test, and the right answer to that is to not commit the literal — not to
    // add a fingerprint to the allow list, which would spend a real policy
    // exemption on a string this test can just as well build at runtime.
    let pat = format!("ghp{}{}", "_", "0123456789abcdefghijklmnopqrstuvwxyz");
    for secret in [
        "Authorization: Bearer abc",
        pat.as_str(),
        "github_pat_11ABCDE",
        "https://user:hunter2@gitea.example.com",
        "access_token=abc123",
    ] {
        assert_eq!(sanitize(secret), None, "must not record {secret:?}");
        assert!(
            CallIdentity::default().with_origin(secret).is_empty(),
            "a credential-shaped origin leaves the identity empty"
        );
        // …and the sanitizer's own normalization cannot smuggle it past the
        // check (PR #9832 review). Folding a control character to a space
        // SPLITS a marker (`ghp\0_…` -> `ghp _…`); deleting a non-graphic
        // character JOINS one (`ghpé_…` -> `ghp_…`). Injecting each kind at
        // every position inside the marker must still be refused.
        for (i, _) in secret.char_indices().take(12) {
            for injected in ['\u{0}', '\u{7}', '\u{e9}'] {
                let mut probe = secret.to_string();
                probe.insert(i, injected);
                assert_eq!(
                    sanitize(&probe),
                    None,
                    "{injected:?} at byte {i} must not make {secret:?} recordable"
                );
            }
        }
    }
    // A legitimate host is kept.
    assert_eq!(
        CallIdentity::default()
            .with_origin("gitea.example.com")
            .origin
            .as_deref(),
        Some("gitea.example.com")
    );
}

/// #10210: only a pool read at zero whose reset is still ahead (or, with no
/// reset, whose reading is fresh) is an exhausted pool.
#[test]
fn exhausted_in_keeps_only_live_zero_readings() {
    let now = 1_000_000;
    let reading = |remaining, reset_epoch, observed_at| Reading {
        remaining,
        used: None,
        reset_epoch,
        observed_at,
    };
    let mut latest = BTreeMap::new();
    latest.insert(Pool::Core, reading(0, Some(now + 600), now - 10));
    latest.insert(Pool::Graphql, reading(0, Some(now - 1), now - 10));
    latest.insert(Pool::Search, reading(5, Some(now + 600), now - 10));
    latest.insert(Pool::Other, reading(0, None, now - 10));
    let exhausted = exhausted_in(&latest, now);
    assert_eq!(
        exhausted,
        vec![(Pool::Core, epoch(now + 600)), (Pool::Other, None)],
        "graphql already reset; search has budget"
    );
    latest.insert(Pool::Other, reading(0, None, now - WINDOW_SECS - 1));
    assert_eq!(exhausted_in(&latest, now).len(), 1, "a stale unreset reading is no stall");
}
