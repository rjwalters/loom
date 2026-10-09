//! W4-A: reader withdrawal scoped to one `(app, owner, resource)` bucket.
//!
//! Every test uses its own numeric reader App id: the routing tables and the
//! bucket book are process-global, and tests run in parallel. The mode is
//! always passed explicitly (`*_in` / `*_at`), never set through
//! `LOOM_READ_ROUTING`, so no test mutates the process environment.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::forge_identity::{Identity, Roster};
use std::path::PathBuf;

fn t0() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_900_000_000)
}

fn no_probe(_: &str, _: &str, _: Resource) -> Option<SystemTime> {
    None
}

fn reader(app: &str) -> Identity {
    Identity {
        app_id: app.to_string(),
        slug: None,
        private_key_path: PathBuf::from(format!("/keys/{app}.pem")),
        owners: None,
    }
}

fn scoped(w: &Withdrawal) -> (&str, ResourceScope, SystemTime, ResetSource, bool) {
    match w {
        Withdrawal::Scoped {
            owner,
            scope,
            until,
            source,
            secondary,
        } => (owner.as_str(), *scope, *until, *source, *secondary),
        other => panic!("expected a scoped withdrawal, got {other:?}"),
    }
}

fn headers(reset: Option<i64>, retry_after: Option<u64>) -> RateLimitHeaders {
    RateLimitHeaders {
        resource: None,
        remaining: Some(0),
        used: Some(5000),
        reset_epoch: reset,
        limit: Some(5000),
        retry_after_secs: retry_after,
    }
}

// ---- scope ------------------------------------------------------------------

#[test]
fn a_core_rate_limit_withdraws_only_that_owners_core_bucket() {
    let now = t0();
    let reset = now + Duration::from_secs(1200);
    let app = "940001";
    let failure = Failure::RateLimited {
        resource: Resource::Core,
        reset: Some(reset),
        secondary: false,
        retry_after: None,
    };
    let w = withdraw_after_in(
        RoutingMode::Scoped,
        app,
        "Acme/widgets",
        failure,
        "test",
        now,
        &no_probe,
    );
    assert_eq!(scoped(&w), ("acme", ResourceScope::Core, reset, ResetSource::Header, false));
    let during = now + Duration::from_secs(600);
    assert!(forge_read_pool::is_withdrawn_scoped_at(app, "acme", Resource::Core, during));
    assert!(
        !forge_read_pool::is_withdrawn_scoped_at(app, "acme", Resource::Graphql, during),
        "the same owner's graphql pool keeps serving"
    );
    assert!(
        !forge_read_pool::is_withdrawn_scoped_at(app, "other", Resource::Core, during),
        "another owner's core pool keeps serving"
    );
    assert!(
        !forge_read_pool::is_withdrawn_scoped_at(app, "acme", Resource::Core, reset),
        "eligible again at the injected reset"
    );
    assert!(!forge_read_pool::is_withdrawn_at(app, during), "never App-wide");

    // Routing: the one-reader roster still serves the other owner's core and
    // this owner's graphql, and skips this owner's core.
    let roster = Roster {
        writer: None,
        readers: vec![reader(app)],
        legacy_logins: vec![],
    };
    let pick = |repo: &str, resource| {
        super::super::reader_for_resource_at(&roster, repo, resource, during, RoutingMode::Scoped)
            .map(|r| r.app_id.clone())
    };
    assert_eq!(pick("acme/widgets", Resource::Core), None);
    assert_eq!(pick("acme/widgets", Resource::Graphql).as_deref(), Some(app));
    assert_eq!(pick("other/widgets", Resource::Core).as_deref(), Some(app));
    // Legacy mode never consults the scoped table.
    assert_eq!(
        super::super::reader_for_resource_at(
            &roster,
            "acme/widgets",
            Resource::Core,
            during,
            RoutingMode::Legacy
        )
        .map(|r| r.app_id.clone())
        .as_deref(),
        Some(app)
    );
}

#[test]
fn a_header_reset_books_the_bucket_exhausted() {
    let now = SystemTime::now();
    let reset = now + Duration::from_secs(900);
    let app = "940002";
    let failure = Failure::RateLimited {
        resource: Resource::Graphql,
        reset: Some(reset),
        secondary: false,
        retry_after: None,
    };
    withdraw_after_in(RoutingMode::Scoped, app, "acme/x", failure, "test", now, &no_probe);
    let key = BucketKey::new("app-940002", "acme", Resource::Graphql);
    let r = forge_bucket_book::reading(&key, epoch_secs(now)).unwrap();
    assert_eq!(r.remaining, Some(0));
    assert_eq!(r.reset_epoch, epoch_secs(reset));
    assert_eq!(r.source, forge_bucket_book::Source::Refusal);
}

// ---- reset source order and clamp -------------------------------------------

#[test]
fn the_reset_comes_from_the_header_then_the_probe_then_the_default() {
    let now = t0();
    let header_reset = now + Duration::from_secs(1500);
    let probe_reset = now + Duration::from_secs(2000);
    let probe = |_: &str, _: &str, _: Resource| Some(probe_reset);
    let plan = |reset, probe: ProbeReset<'_>| {
        let f = Failure::RateLimited {
            resource: Resource::Core,
            reset,
            secondary: false,
            retry_after: None,
        };
        plan_withdrawal("acme/x", f, now, RoutingMode::Scoped, "940003", probe)
    };
    let w = plan(Some(header_reset), &probe);
    assert_eq!(scoped(&w).2, header_reset, "the header beats the probe");
    assert_eq!(scoped(&w).3, ResetSource::Header);
    let w = plan(None, &probe);
    assert_eq!((scoped(&w).2, scoped(&w).3), (probe_reset, ResetSource::Probe));
    let w = plan(None, &no_probe);
    assert_eq!(
        (scoped(&w).2, scoped(&w).3),
        (now + Duration::from_secs(300), ResetSource::Default)
    );

    // Clamp: a reset already (nearly) past waits 30 s; one beyond the window
    // waits at most 3660 s.
    let w = plan(Some(now + Duration::from_secs(2)), &no_probe);
    assert_eq!(scoped(&w).2, now + Duration::from_secs(30));
    let w = plan(Some(now - Duration::from_secs(60)), &no_probe);
    assert_eq!(scoped(&w).2, now + Duration::from_secs(30));
    let w = plan(Some(now + Duration::from_secs(9000)), &no_probe);
    assert_eq!(scoped(&w).2, now + Duration::from_secs(3660));
}

#[test]
fn the_probe_is_consulted_only_when_the_refusal_had_no_reset() {
    let now = t0();
    let asked = std::cell::Cell::new(0);
    let probe = |app: &str, owner: &str, resource: Resource| {
        asked.set(asked.get() + 1);
        assert_eq!((app, owner, resource), ("940004", "Acme", Resource::Graphql));
        None
    };
    let f = Failure::rate_limited(Resource::Graphql);
    let _ = plan_withdrawal("Acme/x", f, now, RoutingMode::Scoped, "940004", &probe);
    assert_eq!(asked.get(), 1);
    let _ = plan_withdrawal(
        "Acme/x",
        f.with_reset(Some(now + Duration::from_secs(60))),
        now,
        RoutingMode::Scoped,
        "940004",
        &probe,
    );
    assert_eq!(asked.get(), 1, "a header reset needs no probe");
}

#[test]
fn the_probed_reset_is_believed_only_for_an_empty_bucket() {
    let now = SystemTime::now();
    let ws = tempfile::tempdir().unwrap();
    let reset = epoch_secs(now) + 1700;
    let key = |owner: &str| BucketKey::new("app-940005", owner, Resource::Core);
    let reading = |remaining| RateLimitHeaders {
        resource: Some("core".into()),
        remaining: Some(remaining),
        used: Some(5000 - remaining),
        reset_epoch: Some(reset),
        limit: Some(5000),
        retry_after_secs: None,
    };
    // No reader dir in the workspace: the probe is a no-op, and the book's
    // own reading decides.
    forge_bucket_book::observe_at(
        key("empty"),
        &reading(0),
        forge_bucket_book::Source::Probe,
        epoch_secs(now),
    );
    forge_bucket_book::observe_at(
        key("full"),
        &reading(4000),
        forge_bucket_book::Source::Probe,
        epoch_secs(now),
    );
    let got = |owner| probed_reset_with(ws.path(), None, "940005", owner, Resource::Core, now);
    assert_eq!(got("empty"), epoch_time(reset));
    assert_eq!(got("full"), None, "a bucket with budget did not refuse the call");
    assert_eq!(got("unknown"), None);
}

// ---- secondary and credential -----------------------------------------------

#[test]
fn a_secondary_limit_withdraws_every_resource_for_retry_after_never_the_hourly_reset() {
    let now = t0();
    let stderr = "gh: You have exceeded a secondary rate limit. (HTTP 403)";
    let hourly = 1_900_000_000 + 3000;
    let classify = |h: &RateLimitHeaders| {
        classify_failure_in(RoutingMode::Scoped, stderr, Some(403), Some(h), Resource::Core)
            .unwrap()
    };
    let f = classify(&headers(Some(hourly), Some(120)));
    assert!(
        matches!(
            f,
            Failure::RateLimited {
                secondary: true,
                ..
            }
        ),
        "{f:?}"
    );
    let w = plan_withdrawal("acme/x", f, now, RoutingMode::Scoped, "940006", &no_probe);
    assert_eq!(
        scoped(&w),
        (
            "acme",
            ResourceScope::All,
            now + Duration::from_secs(120),
            ResetSource::Header,
            true
        )
    );

    let f = classify(&headers(Some(hourly), None));
    let w = plan_withdrawal("acme/x", f, now, RoutingMode::Scoped, "940006", &no_probe);
    assert_eq!(
        scoped(&w),
        (
            "acme",
            ResourceScope::All,
            now + Duration::from_secs(60),
            ResetSource::Default,
            true
        ),
        "never the hourly reset"
    );

    // A 429 (or 403) carrying Retry-After is secondary whatever the text.
    let f = classify_failure_in(
        RoutingMode::Scoped,
        "",
        Some(429),
        Some(&headers(None, Some(30))),
        Resource::Core,
    )
    .unwrap();
    assert!(
        matches!(
            f,
            Failure::RateLimited {
                secondary: true,
                ..
            }
        ),
        "{f:?}"
    );
}

#[test]
fn a_secondary_withdrawal_covers_every_resource_of_that_owner_only() {
    let now = t0();
    let app = "940007";
    let f = Failure::RateLimited {
        resource: Resource::Core,
        reset: None,
        secondary: true,
        retry_after: Some(Duration::from_secs(120)),
    };
    withdraw_after_in(RoutingMode::Scoped, app, "acme/x", f, "test", now, &no_probe);
    let during = now + Duration::from_secs(60);
    for r in [Resource::Core, Resource::Graphql, Resource::Search] {
        assert!(forge_read_pool::is_withdrawn_scoped_at(app, "acme", r, during), "{r:?}");
        assert!(!forge_read_pool::is_withdrawn_scoped_at(app, "other", r, during), "{r:?}");
    }
    assert!(!forge_read_pool::is_withdrawn_scoped_at(
        app,
        "acme",
        Resource::Core,
        now + Duration::from_secs(120)
    ));
}

#[test]
fn bad_credentials_withdraw_every_resource_for_five_minutes() {
    let now = t0();
    let f = classify_failure_in(
        RoutingMode::Scoped,
        "gh: Bad credentials (HTTP 401)",
        Some(401),
        None,
        Resource::Graphql,
    )
    .unwrap();
    assert_eq!(f, Failure::Credential);
    let w = plan_withdrawal("acme/x", f, now, RoutingMode::Scoped, "940008", &no_probe);
    assert_eq!(
        scoped(&w),
        (
            "acme",
            ResourceScope::All,
            now + Duration::from_secs(300),
            ResetSource::Default,
            false
        )
    );
}

#[test]
fn the_refused_pool_comes_from_the_response_else_the_static_pool() {
    let mut h = headers(Some(1_900_000_500), None);
    h.resource = Some("graphql".into());
    let f = classify_failure_in(
        RoutingMode::Scoped,
        "API rate limit exceeded",
        Some(403),
        Some(&h),
        Resource::Core,
    )
    .unwrap();
    assert_eq!(
        f,
        Failure::RateLimited {
            resource: Resource::Graphql,
            reset: epoch_time(1_900_000_500),
            secondary: false,
            retry_after: None,
        }
    );
    let f = classify_failure_in(RoutingMode::Scoped, "", Some(429), None, Resource::Search);
    assert_eq!(f, Some(Failure::rate_limited(Resource::Search)));
    // A 403 whose headers say the pool is empty is a rate limit, not coverage.
    let f = classify_failure_in(RoutingMode::Scoped, "", Some(403), Some(&h), Resource::Core);
    assert!(matches!(f, Some(Failure::RateLimited { .. })), "{f:?}");
}

#[test]
fn coverage_and_non_credential_failures_are_unchanged() {
    for mode in [RoutingMode::Scoped, RoutingMode::Legacy] {
        let c = |s, h| classify_failure_in(mode, s, h, None, Resource::Core);
        assert_eq!(c("gh: Not Found (HTTP 404)", Some(404)), Some(Failure::Coverage));
        assert_eq!(
            c("Resource not accessible by integration (HTTP 403)", Some(403)),
            Some(Failure::Coverage)
        );
        assert_eq!(c("GraphQL: Could not resolve to a Repository", None), Some(Failure::Coverage));
        assert_eq!(c("Server Error (HTTP 502)", Some(502)), None);
        let w = plan_withdrawal("acme/x", Failure::Coverage, t0(), mode, "940009", &no_probe);
        assert_eq!(
            w,
            Withdrawal::Repo {
                until: t0() + super::super::REPO_WITHDRAWAL
            }
        );
    }
}

// ---- extend, never shorten ----------------------------------------------------

#[test]
fn a_later_shorter_withdrawal_never_shortens_a_held_one() {
    let now = t0();
    let app = "940010";
    let long = now + Duration::from_secs(1800);
    let short = now + Duration::from_secs(60);
    let held = forge_read_pool::withdraw_scoped_until(app, "Acme", ResourceScope::Core, long);
    assert_eq!(held, long);
    let held = forge_read_pool::withdraw_scoped_until(app, "acme", ResourceScope::Core, short);
    assert_eq!(held, long, "the shorter request returns the held end");
    assert!(forge_read_pool::is_withdrawn_scoped_at(
        app,
        "ACME",
        Resource::Core,
        now + Duration::from_secs(900)
    ));
    let longer = now + Duration::from_secs(3000);
    assert_eq!(
        forge_read_pool::withdraw_scoped_until(app, "acme", ResourceScope::Core, longer),
        longer,
        "a longer one extends"
    );
    let live = forge_read_pool::live_scoped_withdrawals(now);
    assert!(live.contains(&(app.to_string(), "acme".to_string(), ResourceScope::Core, longer)));
}

// ---- legacy -------------------------------------------------------------------

/// The pre-W4-A classifier's answers, recorded: `true` = `Failure::App`,
/// `false` = `Failure::Coverage`, `None` = not the credential's.
const PRE_W4A_CLASSIFICATIONS: &[(&str, Option<u16>, Option<bool>)] = &[
    ("gh: API rate limit exceeded for installation (HTTP 403)", Some(403), Some(true)),
    ("secondary rate limit", Some(403), Some(true)),
    ("You have exceeded a secondary rate limit", None, Some(true)),
    ("", Some(429), Some(true)),
    ("HTTP 429: too many", None, Some(true)),
    ("Bad credentials (HTTP 401)", Some(401), Some(true)),
    ("", Some(401), Some(true)),
    ("HTTP 401", None, Some(true)),
    ("gh: Not Found (HTTP 404)", Some(404), Some(false)),
    ("", Some(403), Some(false)),
    ("Resource not accessible by integration (HTTP 403)", Some(403), Some(false)),
    ("Could not resolve to a Repository", None, Some(false)),
    ("Server Error (HTTP 502)", Some(502), None),
    ("connection reset", None, None),
];

#[test]
fn legacy_mode_restores_the_pre_change_classifier_and_app_wide_withdrawal() {
    let now = t0();
    for &(stderr, status, want) in PRE_W4A_CLASSIFICATIONS {
        // Legacy ignores headers entirely, as the old classifier did: a 403
        // whose headers say "empty" is still coverage there.
        let h = headers(Some(1_900_001_000), Some(5));
        let got =
            classify_failure_in(RoutingMode::Legacy, stderr, status, Some(&h), Resource::Core);
        assert_eq!(got.map(|f| f.is_app_wide()), want, "{stderr:?} {status:?}");
        if let Some(f) = got.filter(Failure::is_app_wide) {
            // The pre-change TTL: the default window, every caller that had no
            // reset of its own.
            let w = plan_withdrawal("acme/x", f, now, RoutingMode::Legacy, "940011", &no_probe);
            assert_eq!(
                w,
                Withdrawal::AppWide {
                    until: now + forge_read_pool::DEFAULT_WITHDRAWAL
                },
                "{stderr:?}"
            );
        }
    }
    // A caller that parsed the reset itself (ci_telemetry, the ETA fetch)
    // withdrew App-wide until it, unclamped.
    let reset = now + Duration::from_secs(5000);
    let f = Failure::rate_limited(Resource::Core).with_reset(Some(reset));
    assert_eq!(
        plan_withdrawal("acme/x", f, now, RoutingMode::Legacy, "940011", &no_probe),
        Withdrawal::AppWide { until: reset }
    );
    // And the probe is never run in legacy mode.
    let probe =
        |_: &str, _: &str, _: Resource| -> Option<SystemTime> { panic!("legacy mode probed") };
    let _ = plan_withdrawal(
        "acme/x",
        Failure::rate_limited(Resource::Core),
        now,
        RoutingMode::Legacy,
        "940011",
        &probe,
    );
}

#[test]
fn legacy_mode_applies_an_app_wide_withdrawal_and_no_scoped_one() {
    let now = SystemTime::now();
    let app = "940012";
    withdraw_after_in(
        RoutingMode::Legacy,
        app,
        "acme/x",
        Failure::Credential,
        "test",
        now,
        &no_probe,
    );
    assert!(forge_read_pool::is_withdrawn_at(app, now + Duration::from_secs(10)));
    assert!(!forge_read_pool::is_withdrawn_scoped_at(
        app,
        "acme",
        Resource::Core,
        now + Duration::from_secs(10)
    ));
}

#[test]
fn the_kill_switch_reads_legacy_in_any_case_and_defaults_to_scoped() {
    assert_eq!(RoutingMode::parse(Some("legacy")), RoutingMode::Legacy);
    assert_eq!(RoutingMode::parse(Some(" LEGACY\n")), RoutingMode::Legacy);
    assert_eq!(RoutingMode::parse(Some("v2")), RoutingMode::Scoped);
    assert_eq!(RoutingMode::parse(Some("")), RoutingMode::Scoped);
    assert_eq!(RoutingMode::parse(None), RoutingMode::Scoped);
}

// ---- call sites pass the parsed headers ---------------------------------------

#[test]
fn ci_telemetry_rate_limits_carry_their_reset_into_the_withdrawal() {
    use crate::ci_telemetry::api::{api_failure, ApiError};
    let now = t0();
    let reset = 1_900_000_000 + 1400;
    let r = Err(ApiError::RateLimited {
        retry_after_secs: None,
        reset_epoch: Some(reset),
        detail: "API rate limit exceeded".into(),
    });
    let f = api_failure(&r).unwrap();
    let w = plan_withdrawal("acme/x", f, now, RoutingMode::Scoped, "940013", &no_probe);
    assert_eq!(
        scoped(&w),
        (
            "acme",
            ResourceScope::Core,
            epoch_time(reset).unwrap(),
            ResetSource::Header,
            false
        )
    );
    let r = Err(ApiError::RateLimited {
        retry_after_secs: Some(90),
        reset_epoch: Some(reset),
        detail: "secondary rate limit".into(),
    });
    let w = plan_withdrawal(
        "acme/x",
        api_failure(&r).unwrap(),
        now,
        RoutingMode::Scoped,
        "940013",
        &no_probe,
    );
    assert_eq!(scoped(&w).1, ResourceScope::All);
    assert_eq!(scoped(&w).2, now + Duration::from_secs(90));
}

#[test]
fn fetch_conditional_failures_carry_their_reset_into_the_withdrawal() {
    use std::os::unix::process::ExitStatusExt;
    let now = t0();
    let reset = 1_900_000_000 + 2100;
    let mut h = headers(Some(reset), None);
    h.resource = Some("core".into());
    let result: crate::forge_etag_store::FetchResult = (
        std::process::ExitStatus::from_raw(1 << 8),
        Some(crate::forge_listing::HttpResponse {
            status: 403,
            etag: None,
            body: String::new(),
            ratelimit: h,
            next_page: false,
        }),
        "gh: API rate limit exceeded for installation ID 1. (HTTP 403)".into(),
    );
    let f = crate::forge_etag_store::reader_failure(&result).unwrap();
    let w = plan_withdrawal("acme/x", f, now, RoutingMode::Scoped, "940014", &no_probe);
    assert_eq!(
        scoped(&w),
        (
            "acme",
            ResourceScope::Core,
            epoch_time(reset).unwrap(),
            ResetSource::Header,
            false
        ),
        "the response's x-ratelimit-reset, not the flat default"
    );
}
