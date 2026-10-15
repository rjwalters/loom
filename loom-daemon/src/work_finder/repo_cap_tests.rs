//! Coverage for the per-repo dispatch cap and track affinity (issue #9090).
//!
//! Its own file rather than part of `work_finder::tests` for the file-size
//! ratchet reason `prless_retry_tests` documents (`tests.rs` is well over the
//! threshold and frozen), so these carry their own two-field fakes.

use super::super::*;

// ===================================================================
// Per-repo cap — admission
// ===================================================================

// ===================================================================
// `None` is a no-op (the upgrade path)
// ===================================================================

// ===================================================================
// Track affinity — ordering, subordinate to the cap
// ===================================================================

/// The comparator itself is untouched — `shape_queue` is a partition over an
/// already-sorted list, so a sorted list with affinity off comes back
/// unchanged, and with affinity on it is a permutation of the same set.
#[test]
fn shape_queue_never_adds_or_drops_a_candidate() {
    let cand = |idx: usize, number: u32| PriorityCandidate {
        workspace_idx: idx,
        workspace_weight: 0,
        number,
        ..PriorityCandidate::default()
    };
    let candidates = vec![cand(0, 1), cand(1, 2), cand(0, 3)];
    let mut report = TickReport::default();

    let off = super::shape_queue(candidates.clone(), &RepoCap::disabled(), &mut report);
    assert_eq!(off, candidates, "disabled is an exact no-op");

    // Workspace 1 is hot; workspace 0 is cold.
    let cap = RepoCap::new(Some(1), vec![0, 1]);
    let on = super::shape_queue(candidates.clone(), &cap, &mut report);
    assert_eq!(on.len(), candidates.len());
    assert_eq!(on.first().map(|c| c.number), Some(2), "the hot repo floats to the front");
    for c in &candidates {
        assert!(on.contains(c), "#{} must survive the partition", c.number);
    }
}

// ===================================================================
// Cross-repo interleaving is already emergent (#4123), not a new mechanism
// ===================================================================

// ===================================================================
// Config resolution
// ===================================================================

/// The config parse mirrors `maxConcurrent`'s soft-fail contract: absent,
/// zero, negative and non-integer all resolve to "uncapped".
#[test]
fn config_parse_soft_fails_to_uncapped() {
    let wf = |body: &str| serde_json::from_str::<serde_json::Value>(body).unwrap();
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrentPerRepo":2}"#))), Some(2));
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrentPerRepo":0}"#))), None);
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrentPerRepo":-1}"#))), None);
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrentPerRepo":"2"}"#))), None);
    assert_eq!(super::parse_config(Some(&wf(r#"{"maxConcurrent":4}"#))), None);
    assert_eq!(super::parse_config(None), None);
}

/// `read_work_finder_config` carries the key through, and an `autonomous` block
/// without it stays uncapped (the upgrade path).
#[test]
fn read_work_finder_config_reads_the_per_repo_key() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    let write = |body: &str| std::fs::write(dir.path().join(".loom/config.json"), body).unwrap();

    write(r#"{"autonomous":{"workFinder":{"maxConcurrent":6,"maxConcurrentPerRepo":3}}}"#);
    assert_eq!(read_work_finder_config(dir.path()).max_concurrent_per_repo, Some(3));

    write(r#"{"autonomous":{"workFinder":{"maxConcurrent":6}}}"#);
    assert_eq!(read_work_finder_config(dir.path()).max_concurrent_per_repo, None);
}

/// Precedence is env > config > none, and a zero/garbage env value falls
/// through to config rather than disabling the configured cap.
#[test]
#[serial_test::serial]
fn env_overrides_config_for_the_per_repo_cap() {
    let env = super::WORK_FINDER_MAX_CONCURRENT_PER_REPO_ENV;
    let config = WorkFinderConfig {
        max_concurrent_per_repo: Some(2),
        ..Default::default()
    };
    std::env::remove_var(env);
    assert_eq!(super::resolve(&config), Some(2));
    assert_eq!(super::resolve(&WorkFinderConfig::default()), None);

    std::env::set_var(env, "5");
    assert_eq!(super::resolve(&config), Some(5));
    std::env::set_var(env, "0");
    assert_eq!(super::resolve(&config), Some(2), "zero is absent, not a cap of 0");
    std::env::set_var(env, "nonsense");
    assert_eq!(super::resolve(&config), Some(2));
    std::env::remove_var(env);
}

// ===================================================================
// Edge cases
// ===================================================================

/// A short occupancy seed (fewer entries than workspaces — a caller bug, or a
/// workspace added mid-tick) must not panic and must fail OPEN: a workspace
/// with no seeded entry counts as empty, never as capped.
#[test]
fn a_missing_occupancy_entry_fails_open() {
    let mut cap = RepoCap::new(Some(1), vec![1]);
    let cand = PriorityCandidate {
        workspace_idx: 7,
        workspace_weight: 0,
        number: 1,
        ..PriorityCandidate::default()
    };
    let mut report = TickReport::default();
    assert!(!cap.defer(&cand, &mut report), "an unseeded workspace is not at any cap");
    cap.admit(7); // must not panic
    assert_eq!(report.deferred_repo_cap, 0);
}

// ===================================================================
// Dispatch plan (Issue #9288) — the recorded order is the real order
// ===================================================================
