//! The check-then-launch race is closed by construction (Issue #8268, AC3).
//!
//! #8268's third acceptance criterion asks that the race be *either* accepted
//! as a documented best-effort limitation *or* closed with a claim primitive.
//! Loom closes it — so this file is the evidence, kept as a regression test
//! rather than a one-off manual check, because "exactly one winner" is the
//! single property the whole mechanism rests on. If a future refactor swaps
//! the `mkdir`-atomic entry for a read-then-write, every duplicate-suppression
//! guarantee in `verification-ownership.md` silently becomes false and nothing
//! else in the suite would notice.
//!
//! Deliberately an integration test, not a `#[cfg(test)]` unit: it needs real
//! OS threads contending on a real filesystem, which is the only way the
//! atomicity claim is actually exercised.

use std::sync::{Arc, Barrier};
use std::time::Duration;

use loom_daemon::inflight::{claim_in, fingerprint, list_in, ClaimOutcome, Registration};

/// How many threads pile onto one fingerprint at the same instant.
const RACERS: usize = 24;

fn temp_store(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "loom-inflight-it-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn simultaneous_claims_produce_exactly_one_winner() {
    let store = Arc::new(temp_store("race"));
    let stale = Duration::from_secs(3600);
    let barrier = Arc::new(Barrier::new(RACERS));
    let fp = fingerprint("the same suite", "/tree", "main");

    let handles: Vec<_> = (0..RACERS)
        .map(|i| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let fp = fp.clone();
            std::thread::spawn(move || {
                let reg = Registration {
                    fingerprint: fp,
                    command: "the same suite".to_string(),
                    tree: "/tree".to_string(),
                    branch: "main".to_string(),
                    // 0 = no liveness PID, so the age leg alone governs and a
                    // thread cannot be reaped mid-race for looking dead.
                    pid: 0,
                    agent: format!("agent-{i}"),
                    started_at: chrono::Utc::now(),
                };
                // Release every thread at the same instant — without this the
                // threads trivially serialize and the test proves nothing.
                barrier.wait();
                matches!(claim_in(&store, &reg, stale), ClaimOutcome::Claimed(_))
            })
        })
        .collect();

    let winners = handles
        .into_iter()
        .filter(|_| true)
        .map(|h| h.join().expect("racer thread panicked"))
        .filter(|won| *won)
        .count();

    assert_eq!(winners, 1, "expected exactly one claimant to win the race");
    assert_eq!(
        list_in(&store, stale).len(),
        1,
        "the race must leave exactly one registry entry"
    );

    let _ = std::fs::remove_dir_all(store.as_path());
}

#[test]
fn simultaneous_claims_on_distinct_trees_all_win() {
    // The dedup key includes the tree, so N builders in N worktrees running the
    // identical command are doing N different jobs and must NOT be deduplicated
    // — the failure mode opposite to the one above, and just as important.
    let store = Arc::new(temp_store("distinct"));
    let stale = Duration::from_secs(3600);
    let barrier = Arc::new(Barrier::new(RACERS));

    let handles: Vec<_> = (0..RACERS)
        .map(|i| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let tree = format!("/worktrees/issue-{i}");
                let reg = Registration {
                    fingerprint: fingerprint("cargo test", &tree, "main"),
                    command: "cargo test".to_string(),
                    tree,
                    branch: "main".to_string(),
                    pid: 0,
                    agent: format!("builder-{i}"),
                    started_at: chrono::Utc::now(),
                };
                barrier.wait();
                matches!(claim_in(&store, &reg, stale), ClaimOutcome::Claimed(_))
            })
        })
        .collect();

    let winners = handles
        .into_iter()
        .map(|h| h.join().expect("racer thread panicked"))
        .filter(|won| *won)
        .count();

    assert_eq!(winners, RACERS, "distinct trees must never deduplicate");

    let _ = std::fs::remove_dir_all(store.as_path());
}
