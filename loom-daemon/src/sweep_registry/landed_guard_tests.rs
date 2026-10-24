//! Issue #11429: the already-landed dispatch guard.

use super::*;
use crate::sweep_registry::test_support::{
    closed_guard_registry, fixture_registry, state_probe_json,
};
use serial_test::serial;
use tempfile::tempdir;

const REPO: &str = "rjwalters/loom";

fn outcome(
    issue: u32,
    sweep_id: &str,
    disposition: &str,
    emitted_at: DateTime<Utc>,
) -> TelemetryEnvelope {
    let mut record = serde_json::json!({
        "kind": "sweep.outcome",
        "repo": REPO,
        "issue": issue,
        "sweep_id": sweep_id,
        "total_duration_sec": 600,
        "result": "failure",
        "disposition": disposition,
    });
    if disposition == "landed" {
        record["pr_number"] = serde_json::json!(3079);
    }
    let mut envelope = TelemetryEnvelope::new("host-test", serde_json::from_value(record).unwrap());
    envelope.emitted_at = emitted_at;
    envelope
}

fn at(minute: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-11T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + chrono::Duration::minutes(minute)
}

#[test]
fn a_landing_inside_the_window_is_found_and_an_older_one_is_not() {
    let journal = vec![
        outcome(3079, "s-old", "landed", at(0)),
        outcome(3079, "s-fail", "substantive_failure", at(20)),
        outcome(1129, "s-other", "landed", at(25)),
    ];
    let window = LANDED_REDISPATCH_WINDOW;
    assert_eq!(
        recent_landing(&journal, REPO, 3079, at(5), window),
        Some(("s-old".to_string(), Some(3079), 300))
    );
    assert_eq!(recent_landing(&journal, REPO, 3079, at(31), window), None);
    assert_eq!(recent_landing(&journal, "o/other", 3079, at(5), window), None);
    assert_eq!(
        recent_landing(&journal, REPO, 1129, at(20), window).map(|l| l.2),
        Some(0),
        "an outcome stamped after `now` counts as age 0"
    );
}

/// A `landed` outcome in this host's journal refuses the dispatch, and the
/// refusal ends once the window has passed.
#[test]
#[serial]
fn dispatch_is_refused_after_a_landing_in_the_journal() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, spawn_log) = fixture_registry(dir.path());
    let landed_at = Utc::now() - chrono::Duration::minutes(4);
    sweep_outcomes::append_outcome_telemetry(
        &registry.config().resolve_outcome_telemetry_path(),
        &outcome(3079, "sweep-issue-3079-1", "landed", landed_at),
    )
    .unwrap();

    let err = registry
        .dispatch(&SweepKind::Issue(3079), None, None, None, None)
        .expect_err("a just-landed issue is not dispatched again");
    let refusal = err
        .downcast_ref::<AlreadyLandedDispatchError>()
        .expect("typed already_landed refusal");
    assert!(
        matches!(&refusal.evidence, LandedEvidence::JournalLanded { sweep_id, .. } if sweep_id == "sweep-issue-3079-1"),
        "{refusal:?}"
    );
    assert_eq!(refusal.detail(), "journal_landed");
    assert!(!spawn_log.exists(), "nothing was spawned");
    // #11853: the refusal arms a fleet-wide cooldown for the rest of the
    // window, so the work finder skips the issue instead of being refused
    // again on every tick.
    assert!(registry.landed_cooldown_active(3079));
    let remaining = registry
        .dispatch_backoff_remaining(3079, Utc::now())
        .unwrap();
    assert!(remaining <= LANDED_REDISPATCH_WINDOW, "{remaining:?}");
    assert!(remaining > Duration::from_secs(20 * 60), "{remaining:?}");

    let later = landed_at + chrono::Duration::minutes(31);
    assert!(registry.refuse_recent_landing(3079, later).is_ok());
    std::env::remove_var("LOOM_REPO");
    assert!(
        registry.refuse_recent_landing(3079, landed_at).is_ok(),
        "an unresolved repo fails open"
    );
}

/// The 2.5 read finds the issue closed: refused as `already_landed`. A PR
/// number is still refused, but not as a landing.
#[test]
#[serial]
fn a_closed_issue_is_refused_as_already_landed() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let (mut registry, gh_log) =
        closed_guard_registry(dir.path(), &state_probe_json("closed", false), 0);
    let err = registry
        .dispatch(&SweepKind::Issue(149), None, None, None, None)
        .expect_err("a closed issue is refused");
    let refusal = err
        .downcast_ref::<AlreadyLandedDispatchError>()
        .expect("typed already_landed refusal");
    assert_eq!(refusal.evidence, LandedEvidence::ForgeClosed);
    assert!(registry.landed_cooldown_active(149), "#11853: a closed issue cools down");
    assert!(
        registry
            .dispatch_backoff_remaining(149, Utc::now())
            .unwrap()
            > Duration::from_secs(5 * 3600)
    );
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(!calls.contains("issue edit"), "no label flip: {calls}");

    let dir = tempdir().unwrap();
    let (mut registry, _) = closed_guard_registry(dir.path(), &state_probe_json("open", true), 0);
    let err = registry
        .dispatch(&SweepKind::Issue(150), None, None, None, None)
        .expect_err("a PR number is refused");
    assert!(err.downcast_ref::<AlreadyLandedDispatchError>().is_none(), "{err:#}");
    assert!(err.to_string().contains("pull request"), "{err:#}");
}

/// An issue closed as not planned did not land: the REST read reports it as
/// `already_closed`.
#[test]
#[serial]
fn a_not_planned_close_is_refused_as_already_closed() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let probe = r#"{"state":"closed","state_reason":"not_planned","is_pr":false}"#;
    let (mut registry, _) = closed_guard_registry(dir.path(), probe, 0);
    let err = registry
        .dispatch(&SweepKind::Issue(151), None, None, None, None)
        .expect_err("a closed issue is refused");
    let refusal = err
        .downcast_ref::<AlreadyLandedDispatchError>()
        .expect("typed refusal");
    assert_eq!(refusal.evidence, LandedEvidence::ForgeClosedNotPlanned);
    assert!(err.to_string().contains("already_closed"), "{err:#}");
}
