//! Regression coverage for the quarantine relapse-escalation ladder
//! (vibesql#6639, #9605): probation + doubling TTL.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::sweep_registry::test_support::{fixture_registry, insert_dead_running};
use serial_test::serial;
use tempfile::tempdir;

fn config(ttl: u64, ttl_max: u64) -> QuarantineConfig {
    QuarantineConfig {
        ttl: Duration::from_secs(ttl),
        ttl_max: Duration::from_secs(ttl_max),
        ..QuarantineConfig::default()
    }
}

/// Generation N serves `ttl * 2^(N-1)`, capped at `ttl_max`, saturating.
#[test]
fn escalation_doubles_per_generation_and_caps() {
    let c = config(3600, 86_400);
    assert_eq!(effective_quarantine_ttl_secs(&c, 0), 3600, "no marker = generation 1");
    assert_eq!(effective_quarantine_ttl_secs(&c, 1), 3600);
    assert_eq!(effective_quarantine_ttl_secs(&c, 2), 7200);
    assert_eq!(effective_quarantine_ttl_secs(&c, 3), 14_400);
    assert_eq!(effective_quarantine_ttl_secs(&c, 5), 57_600);
    assert_eq!(effective_quarantine_ttl_secs(&c, 6), 86_400, "3600 << 5 exceeds the cap");
    assert_eq!(effective_quarantine_ttl_secs(&c, u32::MAX), 86_400, "never overflows");
}

/// A ceiling misconfigured below the base TTL never shortens generation 1:
/// both the math and the resolver clamp it up to `ttl`.
#[test]
#[serial]
fn misconfigured_ceiling_clamps_up_to_base_ttl() {
    let c = config(3600, 60);
    assert_eq!(effective_quarantine_ttl_secs(&c, 1), 3600);
    assert_eq!(effective_quarantine_ttl_secs(&c, 4), 3600);

    let dir = tempdir().unwrap();
    std::env::set_var(QUARANTINE_TTL_MAX_ENV, "60");
    let resolved = resolve_quarantine_ttl_max(dir.path(), 3600);
    std::env::remove_var(QUARANTINE_TTL_MAX_ENV);
    assert_eq!(resolved, Duration::from_secs(3600));
}

/// `ttlMaxSecs` resolves env > config > default, and the default sits above
/// the default base TTL.
#[test]
#[serial]
fn ttl_max_resolves_env_over_config_over_default() {
    std::env::remove_var(QUARANTINE_TTL_MAX_ENV);
    std::env::remove_var(QUARANTINE_TTL_ENV);
    let dir = tempdir().unwrap();
    let base = resolve_quarantine_config(dir.path());
    assert_eq!(base.ttl_max, Duration::from_secs(DEFAULT_QUARANTINE_TTL_MAX_SECS));
    assert!(base.ttl_max > base.ttl);

    let loom = dir.path().join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    std::fs::write(
        loom.join("config.json"),
        r#"{"autonomous":{"workFinder":{"quarantine":{"ttlMaxSecs":7200}}}}"#,
    )
    .unwrap();
    assert_eq!(resolve_quarantine_config(dir.path()).ttl_max, Duration::from_secs(7200));

    std::env::set_var(QUARANTINE_TTL_MAX_ENV, "10800");
    let resolved = resolve_quarantine_config(dir.path());
    std::env::remove_var(QUARANTINE_TTL_MAX_ENV);
    assert_eq!(resolved.ttl_max, Duration::from_secs(10_800));
}

/// The flap itself, driven through the real death-classification path
/// (checkpoint-less fast deaths reaped by `reap_once`): after a TTL release,
/// the FIRST further insta-crash re-quarantines at generation 2 with a doubled
/// TTL — not after three more wasted dispatches.
#[test]
fn relapse_after_ttl_release_requarantines_on_first_insta_crash() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    for seq in 0..3 {
        insert_dead_running(&mut registry, 61, seq);
        registry.reap_once();
    }
    assert!(registry.is_quarantined(61), "three strikes quarantine (generation 1)");
    assert_eq!(registry.quarantine_generation(61), 1);

    // Age the entry past its 1h TTL; the next reap releases it.
    registry
        .quarantined
        .insert(61, Utc::now() - chrono::Duration::seconds(3601));
    registry.reap_once();
    assert!(!registry.is_quarantined(61), "TTL release");
    assert_eq!(registry.insta_crash_count(61), 0, "the tally resets on release");
    assert_eq!(
        registry.quarantine_generation(61),
        1,
        "the generation marker (probation) survives the TTL release"
    );

    insert_dead_running(&mut registry, 61, 3);
    registry.reap_once();
    assert!(
        registry.is_quarantined(61),
        "a post-release relapse re-quarantines on the FIRST insta-crash"
    );
    assert_eq!(registry.quarantine_generation(61), 2);
    assert_eq!(registry.effective_quarantine_ttl(61), Duration::from_secs(7200));

    // A generation-2 entry is NOT released at the base 1h TTL...
    registry
        .quarantined
        .insert(61, Utc::now() - chrono::Duration::seconds(3601));
    registry.reap_once();
    assert!(registry.is_quarantined(61), "generation 2 serves the doubled TTL");
    // ...only once the escalated 2h TTL has elapsed.
    registry
        .quarantined
        .insert(61, Utc::now() - chrono::Duration::seconds(7201));
    registry.reap_once();
    assert!(!registry.is_quarantined(61));
}

/// `quarantine_entries` (the `quarantine list` surface) carries the generation
/// and reports `ttl_remaining_secs` against the ESCALATED TTL.
#[test]
fn quarantine_entries_report_generation_and_escalated_ttl() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    registry.set_quarantine_config(config(3600, 86_400));
    let now = Utc::now();
    registry.quarantined.insert(62, now);
    registry.quarantine_generations.insert(62, 3);
    registry.insta_crash_counts.insert(62, 1);

    let entries = registry.quarantine_entries(now);
    let entry = entries
        .iter()
        .find(|e| e.issue == 62)
        .expect("entry present");
    assert_eq!(entry.generation, 3);
    assert_eq!(entry.insta_crash_count, 1);
    assert_eq!(entry.ttl_remaining_secs, 14_400, "generation 3 serves 4x the base TTL");
}

/// A healthy outcome (progress / clean exit) ends probation and resets the
/// ladder: the next quarantine needs the full threshold again.
#[test]
fn healthy_outcome_resets_the_ladder() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    registry.quarantine_generations.insert(63, 2);

    registry.record_terminal_outcome(63, false);
    assert_eq!(registry.quarantine_generation(63), 0, "healthy run clears the marker");

    registry.record_terminal_outcome(63, true);
    assert_eq!(registry.insta_crash_count(63), 1);
    assert!(!registry.is_quarantined(63), "full threshold runway restored");
    registry.record_terminal_outcome(63, true);
    registry.record_terminal_outcome(63, true);
    assert!(registry.is_quarantined(63));
    assert_eq!(registry.quarantine_generation(63), 1, "ladder restarts at generation 1");
}

/// The operator's `quarantine clear` resets the ladder too.
#[test]
fn operator_clear_resets_the_ladder() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    registry.quarantined.insert(64, Utc::now());
    registry.quarantine_generations.insert(64, 3);
    registry.insta_crash_counts.insert(64, 1);

    assert!(registry.clear_quarantine(64));
    assert!(!registry.is_quarantined(64));
    assert_eq!(registry.quarantine_generation(64), 0);
}

/// The forge-comment note explains the escalation only from generation 2.
#[test]
fn escalation_note_only_for_relapsed_generations() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    registry.set_quarantine_config(config(3600, 86_400));
    registry.quarantine_generations.insert(65, 1);
    assert!(registry.quarantine_escalation_note(65).is_empty());
    registry.quarantine_generations.insert(65, 2);
    let note = registry.quarantine_escalation_note(65);
    assert!(note.contains("generation 2"), "{note}");
    assert!(note.contains("7200s"), "{note}");
    assert!(note.contains("quarantine clear 65"), "{note}");
}
