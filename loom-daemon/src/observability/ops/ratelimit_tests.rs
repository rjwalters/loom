//! Tests for rate-limit trips, quota gauges and breaker skips (#10022).

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;

use chrono::DateTime;

use super::*;
use crate::observability::ops::capture::capture;
use crate::rate_limit_breaker::report::{
    report_failure, BreakerHandle, BudgetProbe, FailureContext, ProbeMode,
};
use crate::rate_limit_breaker::{RateLimitBreakerConfig, SharedRateLimitBreaker};
use crate::telemetry::ops::{
    MetricKind, MetricValue, OPS_METRIC_LABEL_KEYS, OPS_SPAN_ATTRIBUTE_KEYS,
};

/// The skip counters are process-global: serialise the tests that touch them.
static SKIP_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_785_352_000 + secs, 0).unwrap()
}

fn budget(core_used: Option<u64>, graphql_used: Option<u64>) -> BudgetSnapshot {
    BudgetSnapshot {
        core_remaining: 0,
        core_reset: t(1800),
        graphql_remaining: 4972,
        graphql_reset: t(2400),
        core_used,
        graphql_used,
        probed_at: t(0),
    }
}

fn ledger(core: u64, graphql: u64) -> BTreeMap<Pool, u64> {
    BTreeMap::from([(Pool::Core, core), (Pool::Graphql, graphql)])
}

fn breaker() -> SharedRateLimitBreaker {
    SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled: true,
        fallback_cooldown_secs: 900,
    })
}

const RL: &str = "HTTP 403: API rate limit exceeded for user ID 1";

#[test]
fn jobs_are_a_closed_set_and_sources_classify_into_it() {
    let labels: Vec<&str> = Job::ALL.iter().map(|j| j.as_str()).collect();
    assert_eq!(
        labels,
        [
            "work_finder",
            "claim_reconciliation",
            "role_runner",
            "epic_supervisor",
            "quarantine_reconciliation",
            "ci_telemetry",
            "outcome_journal",
            "star_liveness",
            "other"
        ]
    );
    for (source, job) in [
        ("work_finder", Job::WorkFinder),
        ("work_finder_starred_at", Job::WorkFinder),
        ("work_finder_main_red_ci", Job::WorkFinder),
        ("claim_reconciliation", Job::ClaimReconciliation),
        ("sweep_outcome_points_signal", Job::OutcomeJournal),
        ("ci_telemetry", Job::CiTelemetry),
        ("star_liveness", Job::StarLiveness),
        ("work_finderish", Job::Other),
        ("free text from somewhere", Job::Other),
    ] {
        assert_eq!(Job::from_source(source), job, "{source}");
    }
}

#[test]
fn a_trip_emits_one_span_with_source_cooldown_and_attribution() {
    let ((), captured) = capture(|| {
        crate::rate_limit_breaker::export_trip(
            "claim_reconciliation",
            t(0),
            Some(t(1800)),
            Some((&budget(Some(5000), Some(28)), true)),
            || Some(ledger(98, 5)),
        );
    });
    assert_eq!(captured.spans.len(), 1);
    assert!(captured.metrics.is_empty());
    let span = &captured.spans[0];
    assert_eq!(span.name.as_str(), "loom.ratelimit.trip");
    let a = &span.attributes;
    assert_eq!(a["loom.ratelimit.source"], "claim_reconciliation");
    assert_eq!(a["loom.ratelimit.cooldown_until"], "2026-07-29T19:36:40Z");
    assert_eq!(a["github.ratelimit.core.used"], "5000");
    assert_eq!(a["github.ratelimit.core.own"], "98");
    assert_eq!(a["github.ratelimit.core.external"], "4902");
    assert_eq!(a["github.ratelimit.graphql.used"], "28");
    assert_eq!(a["github.ratelimit.graphql.own"], "5");
    assert_eq!(a["github.ratelimit.graphql.external"], "23");
    assert_eq!(a["loom.daemon.version"], env!("CARGO_PKG_VERSION"));
    assert!(span.validate().is_ok() && span.context.sampled());
    assert!(a.keys().all(|k| {
        OPS_SPAN_ATTRIBUTE_KEYS.contains(&k.as_str())
            || crate::telemetry::trace::provenance::KEYS.contains(&k.as_str())
    }));
    assert_eq!(span.clone().bounded().attributes, span.attributes, "survives export policy");
}

/// A probe that always answers `reading` (the #8997 report path's seam).
struct FixedProbe(Option<BudgetSnapshot>);

impl BudgetProbe for FixedProbe {
    fn probe(&self, _ctx: &FailureContext, _now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        self.0
    }
}

fn handle(reading: Option<BudgetSnapshot>) -> BreakerHandle {
    BreakerHandle {
        breaker: Arc::new(breaker()),
        probe: Arc::new(FixedProbe(reading)),
        mode: ProbeMode::Inline,
    }
}

#[test]
fn a_reported_trip_exports_one_span_with_the_refined_cooldown() {
    let h = handle(Some(budget(Some(5000), Some(28))));
    let (transition, captured) = capture(|| {
        let first = report_failure(&h, RL, "claim_reconciliation", FailureContext::default(), t(0))
            .unwrap()
            .transition;
        // A re-trip while cooling is absorbed before any probe or export.
        let again = report_failure(&h, RL, "role_runner", FailureContext::default(), t(10));
        assert!(again.is_none());
        first
    });
    assert_eq!(transition.until, Some(t(1800)));
    assert_eq!(captured.spans.len(), 1);
    let a = &captured.spans[0].attributes;
    assert_eq!(a["loom.ratelimit.source"], "claim_reconciliation");
    assert_eq!(a["loom.ratelimit.cooldown_until"], "2026-07-29T19:36:40Z");
    assert_eq!(a["github.ratelimit.core.used"], "5000");
    assert_eq!(a["github.ratelimit.graphql.used"], "28");
}

#[test]
fn an_untrusted_reading_exports_the_trip_without_attribution() {
    // A primary rate-limit failure while the probe reads a non-exhausted
    // budget: the reading is untrusted (#8997, likely another credential).
    let mut reading = budget(Some(10), Some(28));
    reading.core_remaining = 4990;
    let h = handle(Some(reading));
    let (reported, captured) = capture(|| {
        report_failure(&h, RL, "work_finder", FailureContext::default(), t(0))
    });
    assert!(reported.is_some());
    assert_eq!(captured.spans.len(), 1);
    let a = &captured.spans[0].attributes;
    assert_eq!(a["loom.ratelimit.source"], "work_finder");
    assert!(a.contains_key("loom.ratelimit.cooldown_until"));
    assert!(!a.keys().any(|k| k.starts_with("github.ratelimit.")), "{a:?}");
}

#[test]
fn a_probe_without_a_reading_still_exports_the_trip() {
    let h = handle(None);
    let ((), captured) = capture(|| {
        report_failure(&h, RL, "work_finder", FailureContext::default(), t(0)).unwrap();
    });
    assert_eq!(captured.spans.len(), 1);
    let a = &captured.spans[0].attributes;
    assert!(!a.keys().any(|k| k.starts_with("github.ratelimit.")), "{a:?}");
}

#[test]
fn unknown_used_or_ledger_omits_attributes_instead_of_reporting_zero() {
    // Probe without `used`: nothing about either pool.
    let span = trip_span(
        Job::WorkFinder,
        t(0),
        None,
        &TripAttribution::from_budget(Some(&budget(None, None)), Some(&ledger(98, 5))),
    );
    assert!(!span
        .attributes
        .keys()
        .any(|k| k.starts_with("github.ratelimit.")));
    assert!(!span
        .attributes
        .contains_key("loom.ratelimit.cooldown_until"));
    // Ledger off: `used` only.
    let span = trip_span(
        Job::WorkFinder,
        t(0),
        None,
        &TripAttribution::from_budget(Some(&budget(Some(5000), None)), None),
    );
    let keys: Vec<&str> = span
        .attributes
        .keys()
        .map(String::as_str)
        .filter(|k| k.starts_with("github."))
        .collect();
    assert_eq!(keys, ["github.ratelimit.core.used"]);
    // No probe at all.
    assert_eq!(
        TripAttribution::from_budget(None, Some(&ledger(1, 1))),
        TripAttribution::default()
    );
}

#[test]
fn trip_ids_are_derived_from_job_and_instant() {
    let a = TripAttribution::default();
    let x = trip_span(Job::WorkFinder, t(0), None, &a);
    assert_eq!(x.context, trip_span(Job::WorkFinder, t(0), Some(t(60)), &a).context);
    assert_ne!(x.context.trace_id, trip_span(Job::RoleRunner, t(0), None, &a).context.trace_id);
    assert_ne!(x.context.trace_id, trip_span(Job::WorkFinder, t(1), None, &a).context.trace_id);
}

#[test]
fn the_log_line_rendering_is_unchanged() {
    let full = PoolShare {
        used: Some(5000),
        own: Some(98),
    };
    assert_eq!(full.log_text(), "used=5000 own≈98 external≈4902");
    let sink_off = PoolShare {
        used: Some(5000),
        own: None,
    };
    assert_eq!(sink_off.log_text(), "used=5000 own=? (sink off) external=?");
    assert_eq!(PoolShare::default().log_text(), "used=? (probe without used)");
}

#[test]
fn a_gauge_tick_emits_remaining_used_and_reset_per_resource() {
    let points = quota_points(&budget(Some(5000), Some(28)), "app-12345");
    let mut seen: Vec<(&str, &str, MetricValue)> = points
        .iter()
        .map(|p| (p.name.as_str(), p.labels["resource"].as_str(), p.value))
        .collect();
    seen.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    assert_eq!(
        seen,
        [
            ("github.ratelimit.remaining", "core", MetricValue::Int(0)),
            ("github.ratelimit.remaining", "graphql", MetricValue::Int(4972)),
            ("github.ratelimit.reset", "core", MetricValue::Int(t(1800).timestamp())),
            ("github.ratelimit.reset", "graphql", MetricValue::Int(t(2400).timestamp())),
            ("github.ratelimit.used", "core", MetricValue::Int(5000)),
            ("github.ratelimit.used", "graphql", MetricValue::Int(28)),
        ]
    );
    for point in &points {
        assert_eq!(point.name.kind(), MetricKind::Gauge);
        assert_eq!(point.labels["account"], "app-12345");
        assert!(point
            .labels
            .keys()
            .all(|k| OPS_METRIC_LABEL_KEYS.contains(&k.as_str())));
    }
    // A probe without `used` drops only the `used` gauges.
    assert_eq!(quota_points(&budget(None, None), "unknown").len(), 4);
}

#[tokio::test]
async fn with_no_sink_a_gauge_tick_emits_and_probes_nothing() {
    let _guard = SKIP_LOCK.lock().await;
    if crate::observability::ops::global_ops_sink().is_some() {
        return; // another test in this binary registered the real sink
    }
    record(Path::new("/nonexistent")).await;
    assert!(ACCOUNT.lock().unwrap().is_none(), "no identity probe without a sink");
    // A skip outside a capture with no sink touches no counter.
    record_skip(Job::WorkFinder);
    assert!(drain_skip_points().is_empty());
}

#[test]
fn account_labels_never_carry_a_token_hash_or_path() {
    assert_eq!(app_account_label("123456"), "app-123456");
    assert_eq!(login_account_label("rjwalters"), "rjwalters");
    assert_eq!(login_account_label("loom-fleet-dispatch"), "loom-fleet-dispatch");
    let pat_like = format!("{}_{}", "ghp", "0123456789abcdefghijABCDEFGHIJ012345");
    let fine_like =
        format!("{}_{}", "github_pat", "11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz");
    let inst_like = format!("{}_{}", "ghs", "installationtoken0123456789");
    for hostile in [
        "",
        pat_like.as_str(),
        fine_like.as_str(),
        inst_like.as_str(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "/home/ubuntu/.loom/gh-config",
        "~/.config/gh/hosts.yml",
        "x-access-token",
        "-leadinghyphen",
        "has space",
    ] {
        assert_eq!(login_account_label(hostile), "unknown", "{hostile:?}");
    }
    for hostile in ["", "abc", "12/34", "ghs_123", "1234567890123456789012345"] {
        assert_eq!(app_account_label(hostile), "unknown", "{hostile:?}");
    }
}

#[test]
fn each_suppressed_skip_site_counts_once_per_pass_by_job() {
    let _guard = SKIP_LOCK.blocking_lock();
    let b = breaker();
    let ((), captured) = capture(|| {
        let _ = drain_skip_points();
        // Closed breaker: a pass runs, nothing is counted.
        assert!(!b.skip_if_suppressed("work_finder", t(0)));
        b.observe_failure(RL, "work_finder", None, t(0)).unwrap();
        for job in [
            "work_finder",
            "claim_reconciliation",
            "role_runner",
            "epic_supervisor",
        ] {
            assert!(b.skip_if_suppressed(job, t(1)));
        }
        assert!(b.skip_if_suppressed("work_finder", t(2)));
        // Polling the predicate itself never counts.
        assert!(b.is_suppressed(t(3)));
        crate::observability::ops::emit_metrics(drain_skip_points());
    });
    let counts: BTreeMap<&str, MetricValue> = captured
        .metrics
        .iter()
        .map(|p| {
            assert_eq!(p.name.as_str(), "github.ratelimit.breaker_skips");
            assert_eq!(p.name.kind(), MetricKind::DeltaCounter);
            (p.labels["reason"].as_str(), p.value)
        })
        .collect();
    assert_eq!(
        counts,
        BTreeMap::from([
            ("claim_reconciliation", MetricValue::Int(1)),
            ("epic_supervisor", MetricValue::Int(1)),
            ("role_runner", MetricValue::Int(1)),
            ("work_finder", MetricValue::Int(2)),
        ])
    );
    // Drained: the next flush is empty.
    assert!(drain_skip_points().is_empty());
}
