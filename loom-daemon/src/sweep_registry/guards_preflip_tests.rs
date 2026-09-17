//! End-to-end coverage for [`SweepRegistry::classify_preflip_labels`] — the
//! `gh issue view --json labels` probe plus the [`preflip_labels`] verdict it
//! delegates to (Issues #4085, #5789, #7873).
//!
//! Sibling file rather than an inline `mod` in `guards.rs`: that file is over
//! the file-size ratchet threshold (`scripts/file-size-baseline.txt`), and
//! extracting a test module is the policy's own preferred remedy.
//!
//! The predicate's exhaustive label matrix lives in `preflip_labels.rs`'s own
//! unit tests (pure, no fixture). These tests are the wiring: that the probe
//! parses `gh` output into that predicate and preserves the fail-closed
//! `Unknown` leg.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use crate::sweep_registry::test_support::{
    collision_dispatch_registry, collision_registry, wait_for_contents, FIXTURE_CHILD_WAIT_MS,
};
use serial_test::serial;
use tempfile::tempdir;

/// Build a registry whose fake `gh issue view` reports exactly `labels`.
fn registry_reporting(dir: &Path, labels: &[&str]) -> SweepRegistry {
    let names: Vec<String> = labels
        .iter()
        .map(|l| format!(r#"{{"name":"{l}"}}"#))
        .collect();
    let stdout = format!(r#"{{"labels":[{}]}}"#, names.join(","));
    collision_registry(dir, &dir.join("gh.log"), &stdout, 0)
}

/// A pre-flip read showing `loom:building` already present is a collision.
#[test]
fn classify_preflip_labels_flags_prior_building_claim() {
    let dir = tempdir().unwrap();
    let registry = registry_reporting(dir.path(), &["loom:building", "loom:curated"]);
    match registry.classify_preflip_labels(42) {
        CollisionClass::Collision { labels } => {
            assert!(labels.iter().any(|l| l == "loom:building"));
        }
        other => panic!("expected Collision, got {other:?}"),
    }
}

/// Issue #7873: `loom:issue` merely *absent* — with no claim label anywhere —
/// is an issue that was never promoted, NOT a peer host's flip. Previously
/// every one of these was refused as a cross-host collision (observed on
/// #7743 `[loom:triage]`, #7812 `[]`, #7849 `[loom:triage]`).
#[test]
fn classify_preflip_labels_treats_unpromoted_issue_as_not_a_collision() {
    for labels in [
        &[][..],
        &["loom:curated"][..],
        &["loom:triage"][..],
        &["tier:goal-supporting"][..],
    ] {
        let dir = tempdir().unwrap();
        let registry = registry_reporting(dir.path(), labels);
        let class = registry.classify_preflip_labels(42);
        assert!(
            matches!(class, CollisionClass::NotYetApproved { .. }),
            "{labels:?} evidences no peer claim and must not refuse dispatch (#7873); got {class:?}"
        );
    }
}

/// The #5789 safety property #7873 must not weaken: every claim label still
/// collides, including when `loom:issue` was already removed alongside it.
#[test]
fn classify_preflip_labels_still_collides_on_every_claim_label() {
    for labels in [
        &["loom:building"][..],
        &["loom:curated", "loom:building"][..],
        &["loom:reviewing"][..],
        &["loom:treating"][..],
    ] {
        let dir = tempdir().unwrap();
        let registry = registry_reporting(dir.path(), labels);
        let class = registry.classify_preflip_labels(42);
        assert!(
            matches!(class, CollisionClass::Collision { .. }),
            "{labels:?} carries a claim label and must still be refused (#5789); got {class:?}"
        );
    }
}

/// `loom:issue` still present and no claim label ⇒ this host is the first
/// claimant: Clean, not a collision.
#[test]
fn classify_preflip_labels_clean_when_issue_label_present() {
    let dir = tempdir().unwrap();
    let registry = registry_reporting(dir.path(), &["loom:issue", "loom:curated"]);
    assert_eq!(registry.classify_preflip_labels(42), CollisionClass::Clean);
}

/// Fail-closed: a non-zero `gh` exit is `Unknown`, never a collision — an
/// unverifiable read must not inflate the baseline.
#[test]
fn classify_preflip_labels_fail_closed_on_gh_error() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh.log");
    let registry = collision_registry(dir.path(), &gh_log, "", 1);
    assert_eq!(registry.classify_preflip_labels(42), CollisionClass::Unknown);
}

/// Fail-closed: unparseable stdout (exit 0 but not the expected JSON) is
/// `Unknown`, never a collision.
#[test]
fn classify_preflip_labels_fail_closed_on_unparseable() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh.log");
    let registry = collision_registry(dir.path(), &gh_log, "not json at all", 0);
    assert_eq!(registry.classify_preflip_labels(42), CollisionClass::Unknown);
}

/// #7873 at the full `dispatch()` level — the behavior the issue reports: an
/// explicit `dispatch_sweep(kind={"Issue": N})` for an open, not-yet-promoted
/// issue must spawn its child (whose own pre-flight curates and promotes it),
/// not be refused with a `CollisionDispatchError` naming a peer that does not
/// exist. Both reported snapshots are covered: `[]` (#7812) and
/// `[loom:curated]`.
#[test]
#[serial]
fn dispatch_proceeds_for_unpromoted_issue_with_detection_enabled() {
    for (issue, preflip) in [
        (7812_u32, r#"{"labels":[]}"#),
        (7873, r#"{"labels":[{"name":"loom:curated"}]}"#),
    ] {
        let dir = tempdir().unwrap();
        let (mut registry, gh_log, spawn_log) = collision_dispatch_registry(dir.path(), preflip);
        registry.set_collision_detection(true);

        let outcome = registry
            .dispatch(&SweepKind::Issue(issue), None, None, None, None)
            .unwrap_or_else(|e| {
                panic!("{preflip} must not be refused as a collision (#7873): {e}")
            });
        assert!(outcome.was_new);
        assert_eq!(registry.collision_count(), 0, "{preflip} evidences no peer claim");

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit"),
            "an unpromoted issue must still reach the label flip; gh log: {gh_calls}"
        );
        assert!(
            wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
            "the child sweep must spawn so it can curate/promote the issue itself"
        );
    }
}

/// #7873: an unpromoted issue must not be *counted* as a collision either —
/// the cumulative cross-host baseline the work-finder summary line reports
/// stays zero, and the probe still runs (detection enabled).
#[test]
fn detect_and_record_collision_does_not_count_unpromoted_issue() {
    let dir = tempdir().unwrap();
    let mut registry = registry_reporting(dir.path(), &["loom:curated"]);
    registry.set_collision_detection(true);
    assert!(matches!(
        registry.detect_and_record_collision(42),
        Some(CollisionClass::NotYetApproved { .. })
    ));
    assert_eq!(registry.collision_count(), 0, "an unpromoted issue is not a collision");
}
