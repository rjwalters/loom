//! Regression coverage for the quarantine relapse-escalation ladder
//! (vibesql#6639: probation + doubling TTL).
//!
//! Lives in its own sibling module rather than inside `quarantine.rs`'s
//! `mod tests`: that file is over the file-size ratchet's threshold and is
//! therefore frozen at its current size (see `.loom/docs/file-size-policy.md`),
//! and this mirrors the existing `quarantine_empty_pool_tests.rs` precedent.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::sweep_registry::test_support::{fixture_registry, insert_dead_running_at};
use tempfile::tempdir;

/// vibesql#6639: the flap itself. A quarantine that relapses after its TTL
/// release must re-quarantine on the FIRST further insta-crash (probation),
/// not after the full 3-strike runway — the old behavior cost three wasted
/// dispatches per hourly cycle and cycled forever on #6170/#6172/#6174.
/// Drives the real death-classification path (checkpoint-less fast deaths),
/// not just the tally entrypoints.
#[test]
fn relapse_after_ttl_release_requarantines_on_first_insta_crash() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    // Zero TTL: every reap_once ages the entry out immediately, so the
    // release step is exercisable without clock manipulation.
    registry.set_quarantine_config(QuarantineConfig {
        ttl: Duration::ZERO,
        ..QuarantineConfig::default()
    });

    // Generation 1: three real insta-crash deaths quarantine the issue.
    for seq in 0..3 {
        insert_dead_running_at(&mut registry, 61, seq, Utc::now());
        registry.reap_once();
    }
    assert!(registry.is_quarantined(61), "three strikes quarantine (gen 1)");

    // The next reap expires the zero-TTL entry: released, but the
    // generation marker (probation) must survive the release.
    registry.reap_once();
    assert!(!registry.is_quarantined(61), "zero-TTL entry releases on the next reap");
    assert_eq!(
        registry.quarantine_state.generations.get(&61),
        Some(&1),
        "probation generation survives the TTL release (vibesql#6639)"
    );

    // ONE further insta-crash re-quarantines immediately at generation 2.
    insert_dead_running_at(&mut registry, 61, 3, Utc::now());
    registry.reap_once();
    assert!(
        registry.is_quarantined(61),
        "a post-release relapse re-quarantines on the FIRST insta-crash, not the third"
    );
    assert_eq!(
        registry.quarantine_state.generations.get(&61),
        Some(&2),
        "the relapse increments the escalation generation"
    );
}

/// vibesql#6639: `quarantine_entries` surfaces the generation and reports
/// `ttl_remaining` against the ESCALATED TTL, so `loom-daemon quarantine
/// list` shows the pause an operator is actually waiting on.
#[test]
fn quarantine_entries_reflect_escalated_generation_ttl() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    registry.set_quarantine_config(QuarantineConfig {
        ttl: Duration::from_secs(3600),
        ..QuarantineConfig::default()
    });

    registry.quarantine_state.quarantined.insert(62, Utc::now());
    registry.quarantine_state.generations.insert(62, 2);
    registry.quarantine_state.insta_crash_counts.insert(62, 1);

    let entries = registry.quarantine_entries(Utc::now());
    let entry = entries
        .iter()
        .find(|e| e.issue == 62)
        .expect("entry present");
    assert_eq!(entry.generation, 2);
    assert_eq!(entry.insta_crash_count, 1);
    // Generation 2 serves 7200s; just-applied → ~all of it remains, and
    // crucially MORE than the base 3600s a flat-TTL rendering would show.
    assert!(
        entry.ttl_remaining_secs > 3600 && entry.ttl_remaining_secs <= 7200,
        "ttl_remaining reflects the escalated TTL, got {}",
        entry.ttl_remaining_secs
    );
}

/// vibesql#6639: a healthy outcome (progress / clean exit) resets the
/// probation — the issue proved the breakage transient, so the next
/// quarantine run starts from generation 1 with the full 3-strike runway.
#[test]
fn healthy_outcome_resets_quarantine_generation() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    registry.quarantine_state.generations.insert(63, 2);

    registry.record_terminal_outcome(63, false);

    assert!(
        !registry.quarantine_state.generations.contains_key(&63),
        "a healthy outcome clears the probation generation"
    );
    // And the restored runway: one insta-crash alone no longer quarantines.
    registry.record_terminal_outcome(63, true);
    assert_eq!(registry.insta_crash_count(63), 1);
    assert!(
        !registry.is_quarantined(63),
        "full threshold runway is restored after a healthy run"
    );
}

/// vibesql#6639: the operator clear is a vote of confidence — it resets
/// the escalation ladder too, so a wrongly-cleared issue serves the
/// generation-1 TTL, not the escalated pause it just escaped.
#[test]
fn clear_quarantine_resets_escalation_generation() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    registry.quarantine_state.quarantined.insert(64, Utc::now());
    registry.quarantine_state.generations.insert(64, 3);
    registry.quarantine_state.insta_crash_counts.insert(64, 1);

    assert!(registry.clear_quarantine(64));
    assert!(!registry.quarantine_state.generations.contains_key(&64));
    assert!(!registry.is_quarantined(64));
}
