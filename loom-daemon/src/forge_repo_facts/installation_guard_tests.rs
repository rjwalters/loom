//! Installation-snapshot guards (W8): who a refused listing is reported to
//! (a reader is withdrawn, only the writer reaches the host-wide breaker),
//! and the fail-private edges — a snapshot from the future, the TTL ceiling,
//! a refusal that must not demote a verified installation, a page failing
//! mid-listing, slug case, and a corrupt store entry.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::tests::{listed, page, writer, Listing};
use super::*;
use crate::forge_bucket_book::Resource;
use crate::forge_read_pool;
use crate::forge_repo_facts::state;
use crate::forge_repo_facts::test_support::Env;
use crate::rate_limit_breaker::{RateLimitBreakerConfig, SharedRateLimitBreaker};

fn fresh_breaker() -> Arc<SharedRateLimitBreaker> {
    Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled: true,
        ..RateLimitBreakerConfig::default()
    }))
}

/// A `limited` fake whose bucket resets half an hour from now.
fn limited(fake: &Listing) {
    fake.set("mode", "limited");
    fake.set("reset", &(chrono::Utc::now().timestamp() + 1800).to_string());
}

fn budget_probes(fake: &Listing) -> usize {
    fake.calls()
        .iter()
        .filter(|c| c.contains("rate_limit"))
        .count()
}

/// B2: an exhausted read-only reader App is that reader's problem. It is
/// withdrawn for its own `(app, owner)` bucket until the refusal's reset and
/// the host-wide breaker — which would stop every forge call on the host —
/// never hears of it.
#[test]
fn a_rate_limited_reader_is_withdrawn_and_never_trips_the_host_breaker() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    limited(&fake);
    // Unique to this test: the withdrawal tables are process-wide.
    let app = format!("w8-limited-reader-{}", std::process::id());
    let reader = Credential::reader(env.tmp.path().join("cfg-r"), &app, "w8-limited/x");
    let breaker = fresh_breaker();
    let answer = crate::rate_limit_breaker::with_test_global(breaker.clone(), || {
        lookup(&fake.gh, &reader, "w8-limited/x")
    });
    assert_eq!(answer, Answer::Unavailable);
    assert!(
        !breaker.is_suppressed(chrono::Utc::now()),
        "a reader's dry pool must not suppress the host's forge calls"
    );
    let soon = SystemTime::now() + Duration::from_secs(60);
    let core =
        |owner: &str| forge_read_pool::is_withdrawn_scoped_at(&app, owner, Resource::Core, soon);
    assert!(
        core("w8-limited") || forge_read_pool::is_withdrawn_at(&app, soon),
        "the refused reader is withdrawn"
    );
    assert!(!core("w8-other-owner"), "its other owners keep serving");
    let past_reset = SystemTime::now() + Duration::from_secs(1800 + 120);
    assert!(
        !forge_read_pool::is_withdrawn_scoped_at(&app, "w8-limited", Resource::Core, past_reset),
        "withdrawn until the refusal's own reset, not longer"
    );
    assert_eq!(budget_probes(&fake), 0, "{:?}", fake.calls());
}

/// ...and the lookup falls through to the writer's snapshot.
#[test]
fn a_rate_limited_reader_falls_through_to_the_writers_snapshot() {
    let env = Env::new(&[]);
    let reader_fake = Listing::new(&env.tmp.path().join("r"));
    let writer_fake = Listing::new(&env.tmp.path().join("w"));
    limited(&reader_fake);
    writer_fake.set("page1", &page(1, &[(2, "w8-fall/two", false)]));
    let app = format!("w8-fallthrough-reader-{}", std::process::id());
    let reader = Credential::reader(env.tmp.path().join("cfg-r"), &app, "w8-fall/two");
    let w = writer(&env);
    let breaker = fresh_breaker();
    crate::rate_limit_breaker::with_test_global(breaker.clone(), || {
        // The writer's snapshot is filed under its own key by its own stub.
        let _ = lookup(&writer_fake.gh, &w, "w8-fall/two");
        let both = [reader.clone(), w.clone()];
        assert_eq!(
            lookup_repo_with(&reader_fake.gh, &both, "w8-fall/two"),
            listed("w8-fall/two", 2, false)
        );
        // With no writer snapshot to fall to, the answer is private.
        assert_eq!(
            lookup_repo_with(&reader_fake.gh, std::slice::from_ref(&reader), "w8-fall/two"),
            Answer::Unavailable
        );
    });
    assert!(!breaker.is_suppressed(chrono::Utc::now()));
    assert_eq!(
        reader_fake.listing_calls(),
        1,
        "the reader backs off: {:?}",
        reader_fake.calls()
    );
}

/// B2: the writer is what every other call runs on, so its refusal does
/// reach the breaker — with the refused response's head, so the reset comes
/// from that response and no ambient budget probe is needed.
#[test]
fn a_rate_limited_writer_trips_the_breaker_from_the_response_head() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    limited(&fake);
    let breaker = fresh_breaker();
    let answer = crate::rate_limit_breaker::with_test_global(breaker.clone(), || {
        lookup(&fake.gh, &writer(&env), "acme/pub")
    });
    assert_eq!(answer, Answer::Unavailable);
    let now = chrono::Utc::now();
    assert!(breaker.is_suppressed(now), "the writer's refusal trips the breaker");
    assert_eq!(budget_probes(&fake), 0, "the head was authoritative: {:?}", fake.calls());
    // While it cools, no further listing is attempted.
    crate::rate_limit_breaker::with_test_global(breaker, || {
        assert_eq!(lookup(&fake.gh, &writer(&env), "acme/pub"), Answer::Unavailable);
    });
    assert_eq!(fake.listing_calls(), 1);
}

/// With no reset in the refusal, the breaker's budget probe runs under the
/// WRITER's `GH_CONFIG_DIR` and `gh`: it measures the bucket that refused.
#[test]
fn the_writers_budget_probe_runs_under_the_writers_credential() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("mode", "limited_bare");
    let breaker = fresh_breaker();
    crate::rate_limit_breaker::with_test_global(breaker.clone(), || {
        assert_eq!(lookup(&fake.gh, &writer(&env), "acme/pub"), Answer::Unavailable);
    });
    assert!(breaker.is_suppressed(chrono::Utc::now()));
    let calls = fake.calls();
    let probe = calls
        .iter()
        .find(|c| c.contains("rate_limit"))
        .unwrap_or_else(|| panic!("no budget probe in {calls:?}"));
    assert!(probe.contains("cfg-writer"), "{probe}");
}

/// Hardening (a): a snapshot stamped later than now — the clock stepped
/// back — is no snapshot. It is refetched unconditionally, never served and
/// never revalidated against its validators.
#[test]
fn a_snapshot_from_the_future_is_treated_as_absent() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/pub", false)]));
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
    state::advance_test_clock(-7200);
    fake.set("notmodified", "");
    fake.set("page1", &page(1, &[(1, "acme/pub", true)]));
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, true));
    let calls = fake.calls();
    assert_eq!(fake.listing_calls(), 2, "{calls:?}");
    assert!(!calls[1].contains("If-None-Match"), "refetched whole: {calls:?}");
}

/// The same through the store (a damaged or future-dated file another
/// process left): with the forge unreachable the answer is private, not the
/// file's "public".
#[test]
fn a_future_dated_store_entry_is_never_served() {
    let env = Env::new(&[]);
    let store_dir = env.tmp.path().join("store");
    crate::forge_etag_store::set_test_daemon_store_dir(Some(store_dir));
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/pub", false)]));
    let cred = writer(&env);
    // Written by a process whose clock runs two hours ahead.
    state::advance_test_clock(7200);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
    // A second process on the true clock: empty memory, the same store.
    state::set_test_enabled(true);
    fake.set("mode", "fail");
    let answer = lookup(&fake.gh, &cred, "acme/pub");
    crate::forge_etag_store::set_test_daemon_store_dir(None);
    assert_eq!(answer, Answer::Unavailable);
    assert_eq!(fake.listing_calls(), 2, "{:?}", fake.calls());
}

#[test]
fn a_future_stamp_is_not_fresh_and_a_damaged_backoff_is_bounded() {
    let now = 1_000_000;
    let snap = |verified_at: i64, failed_until: Option<i64>| Snapshot {
        kind: Kind::Installation,
        total_count: 0,
        pages: Vec::new(),
        verified_at,
        failed_until,
    };
    assert!(snap(now - 10, None).fresh(now, 3600));
    assert!(snap(now, None).fresh(now, 3600));
    assert!(!snap(now + 1, None).fresh(now, 3600), "from the future");
    assert!(!snap(now - 3600, None).fresh(now, 3600), "expired");
    assert!(!snap(0, None).fresh(now, 3600), "never verified");
    assert!(snap(0, Some(now + SNAPSHOT_FAILURE_BACKOFF_SECS)).in_backoff(now));
    assert!(!snap(0, Some(now)).in_backoff(now));
    assert!(
        !snap(0, Some(now + SNAPSHOT_FAILURE_BACKOFF_SECS + 1)).in_backoff(now),
        "a backoff longer than one window is damage, not a backoff"
    );
}

/// Hardening (b): the TTL override cannot stretch past the ceiling.
#[test]
fn the_ttl_override_is_clamped() {
    let ttl_with = |value: &str| {
        let env = Env::new(&[("LOOM_INSTALLATION_SNAPSHOT_TTL_SECS", value)]);
        let ttl = ttl_secs();
        drop(env);
        ttl
    };
    assert_eq!(ttl_with("86400"), SNAPSHOT_TTL_MAX_SECS);
    assert_eq!(ttl_with("9223372036854775807"), SNAPSHOT_TTL_MAX_SECS);
    assert_eq!(ttl_with("120"), 120);
    assert_eq!(ttl_with("0"), SNAPSHOT_TTL_DEFAULT_SECS);
    assert_eq!(ttl_with("-5"), SNAPSHOT_TTL_DEFAULT_SECS);
    assert_eq!(ttl_with("soon"), SNAPSHOT_TTL_DEFAULT_SECS);
    const { assert!(SNAPSHOT_TTL_MAX_SECS <= 3600) };
    const { assert!(SNAPSHOT_TTL_DEFAULT_SECS <= SNAPSHOT_TTL_MAX_SECS) };
}

/// Hardening (c): a `403` that is neither a rate limit nor the forge's "not
/// an installation token" refusal does not turn a verified installation
/// into a user credential (whose per-repo reads could then answer). It is a
/// failed revalidation: private.
#[test]
fn an_unrelated_refusal_never_downgrades_a_verified_installation() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/pub", false)]));
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
    state::advance_test_clock(ttl_secs() + 1);
    fake.set("mode", "forbidden");
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), Answer::Unavailable, "not PerRepo");
    state::advance_test_clock(SNAPSHOT_FAILURE_BACKOFF_SECS + 1);
    assert_eq!(
        lookup(&fake.gh, &cred, "acme/pub"),
        Answer::Unavailable,
        "still an installation"
    );
    // It recovers as the installation it always was.
    state::advance_test_clock(SNAPSHOT_FAILURE_BACKOFF_SECS + 1);
    fake.set("mode", "ok");
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
    // The forge SAYING so is the one thing that does change its kind.
    state::advance_test_clock(ttl_secs() + 1);
    fake.set("mode", "user");
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), Answer::PerRepo);
}

/// A credential never seen listing keeps the pre-existing reading of a bare
/// refusal: it takes its per-repo reads.
#[test]
fn an_unrelated_refusal_on_an_unverified_credential_is_per_repo() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("mode", "forbidden");
    assert_eq!(lookup(&fake.gh, &writer(&env), "acme/pub"), Answer::PerRepo);
}

#[test]
fn the_installation_token_refusal_is_matched_in_body_or_stderr() {
    use super::failure::is_installation_token_refusal as refusal;
    assert!(refusal(
        r#"{"message":"You must authenticate with an installation access token in order to list repositories for an installation."}"#,
        ""
    ));
    assert!(refusal(
        "",
        "gh: You must AUTHENTICATE with an installation access token (HTTP 403)"
    ));
    assert!(refusal(r#"{"message":"Resource not accessible by personal access token"}"#, ""));
    assert!(!refusal(r#"{"message":"Resource not accessible by integration"}"#, ""));
    assert!(!refusal(r#"{"message":"Not Found"}"#, "gh: Not Found (HTTP 404)"));
    assert!(!refusal("", ""));
}

#[test]
fn the_response_head_stops_at_the_first_blank_line() {
    use super::failure::response_head;
    let raw = "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\n\r\n{\"a\":\"b\n\nc\"}";
    assert_eq!(response_head(raw), "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0");
    assert_eq!(response_head("HTTP/1.1 200 OK\nEtag: x\n\nbody"), "HTTP/1.1 200 OK\nEtag: x");
    assert_eq!(response_head("HTTP/1.1 304 Not Modified"), "HTTP/1.1 304 Not Modified");
}

/// Hardening (d): page 2 failing mid-listing fails the whole listing. Page
/// 1's rows — fetched fine, one of them public — are not served.
#[test]
fn a_page_failing_mid_listing_answers_nothing() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    let first: Vec<(u64, String, bool)> = (1..=100)
        .map(|i| (i, format!("acme/r{i}"), false))
        .collect();
    let rows: Vec<(u64, &str, bool)> = first.iter().map(|(i, n, p)| (*i, n.as_str(), *p)).collect();
    fake.set("page1", &page(101, &rows));
    fake.set("page2", &page(101, &[(101, "acme/last", false)]));
    fake.set("mode", "failpage2");
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/r1"), Answer::Unavailable, "page 1 is not served");
    assert_eq!(lookup(&fake.gh, &cred, "acme/last"), Answer::Unavailable);
    assert_eq!(fake.listing_calls(), 2, "page 1, page 2, then backoff: {:?}", fake.calls());
    // The same on a revalidation of a snapshot that was once whole.
    state::advance_test_clock(SNAPSHOT_FAILURE_BACKOFF_SECS + 1);
    fake.set("mode", "ok");
    assert_eq!(lookup(&fake.gh, &cred, "acme/r1"), listed("acme/r1", 1, false));
    state::advance_test_clock(ttl_secs() + 1);
    fake.set("mode", "failpage2");
    assert_eq!(lookup(&fake.gh, &cred, "acme/r1"), Answer::Unavailable);
}

/// Hardening (d): GitHub slugs are case-insensitive; the answer carries the
/// forge's spelling.
#[test]
fn the_slug_lookup_ignores_case() {
    let env = Env::new(&[]);
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(2, &[(1, "AcMe/Pub-Repo", false), (2, "acme/pub-repo-2", true)]));
    let creds = [writer(&env)];
    for slug in [
        "acme/pub-repo",
        "ACME/PUB-REPO",
        "AcMe/Pub-Repo",
        " acme/pub-repo ",
    ] {
        assert_eq!(
            lookup_repo_with(&fake.gh, &creds, slug),
            listed("AcMe/Pub-Repo", 1, false),
            "{slug:?}"
        );
    }
    // Case-insensitive is not prefix-matching.
    assert_eq!(lookup_repo_with(&fake.gh, &creds, "acme/pub"), Answer::Listed(None));
    assert_eq!(fake.listing_calls(), 1);
}

/// Hardening (d): a corrupt store entry is no snapshot. It is refetched
/// whole; with the forge unreachable the answer is private.
#[test]
fn a_corrupt_store_entry_is_refetched_or_private() {
    let env = Env::new(&[]);
    let store_dir = env.tmp.path().join("store");
    crate::forge_etag_store::set_test_daemon_store_dir(Some(store_dir.clone()));
    let fake = Listing::new(env.tmp.path());
    fake.set("page1", &page(1, &[(1, "acme/pub", false)]));
    let cred = writer(&env);
    assert_eq!(lookup(&fake.gh, &cred, "acme/pub"), listed("acme/pub", 1, false));
    let corrupt = || {
        let entry = std::fs::read_dir(&store_dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("instsnap-"))
            .expect("an instsnap- entry");
        std::fs::write(entry.path(), "{\"kind\":\"installation\",\"verified_at\":").unwrap();
        // A second process: empty memory, the same (damaged) store.
        state::set_test_enabled(true);
    };
    corrupt();
    fake.set("notmodified", "");
    fake.set("page1", &page(1, &[(1, "acme/pub", true)]));
    let refetched = lookup(&fake.gh, &cred, "acme/pub");
    let calls = fake.calls();
    corrupt();
    fake.set("mode", "fail");
    let unreachable = lookup(&fake.gh, &cred, "acme/pub");
    crate::forge_etag_store::set_test_daemon_store_dir(None);
    assert_eq!(refetched, listed("acme/pub", 1, true));
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(!calls[1].contains("If-None-Match"), "no validator survives: {calls:?}");
    assert_eq!(unreachable, Answer::Unavailable);
}
