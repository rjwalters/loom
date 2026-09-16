//! End-to-end regression coverage for the empty-pool redispatch storm (#7860).
//!
//! Lives in its own sibling module rather than inside `quarantine.rs`'s
//! `mod tests`: that file is over the file-size ratchet's threshold and is
//! therefore frozen at its current size (see `.loom/docs/file-size-policy.md`),
//! and this mirrors the existing `guards_union_tests.rs` precedent.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::sweep_registry::test_support::{
    backoff_registry, insert_dead_running_with_log, seed_token_pool,
};
use tempfile::tempdir;

/// Issue #7860 (end-to-end regression for the redispatch storm).
///
/// Reproduces the exact production shape observed on 2026-09-16: an
/// empty/unusable token pool, so `spawn-claude.sh` dies in its
/// token-selection step (exit 78) with `token=unknown`, leaving a log
/// tail that echoes every pooled account's stored `.bad_tokens` reason.
/// The `.ranking` snapshot still lists a healthy account (it was stale in
/// the real incident), so #4644's `pool_exhausted_now` force-trip cannot
/// paper over the streak — the ONLY path to the dampener is the #4386
/// streak itself.
///
/// **Before the fix**: `classify_preflight_outcome` matched the echoed
/// exhaustion prose first and returned `Unknown`, so the streak stayed at
/// 0 and the workspace advisory never tripped. Dispatch was never held,
/// the work finder re-offered the same candidates every tick, and each
/// attempt burned a `loom:issue` -> `loom:building` -> `loom:issue`
/// label-flip pair — the observed claim/yield churn (152 flips on
/// kicad-tools#5333; 94 on rjwalters/loom#7815).
///
/// **After the fix**: three such deaths arm the streak to the default
/// threshold and trip the advisory, which holds dispatch behind #5030's
/// one half-open probe per cooldown.
///
/// Crucially, the issues themselves stay clean: the #4122 carve-out is
/// untouched, so no issue's quarantine tally is charged and no issue is
/// parked `loom:blocked` for a fault that was never its own.
#[test]
fn empty_pool_token_selection_deaths_arm_the_preflight_streak() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-9");
    // A STALE ranking that still claims a healthy account, so the #4644
    // force-trip stays out of the way and only the streak can trip.
    std::fs::write(
        dir.path().join(".loom").join("tokens").join(".ranking"),
        "agent-9|available|0.10\n",
    )
    .unwrap();

    assert!(!registry.preflight_advisory().0, "precondition: advisory clear");

    for (seq, issue) in [7815_u32, 5333, 7795].into_iter().enumerate() {
        let log_body = format!(
            "==== loom-daemon dispatch: sweep-issue-{issue}-{seq} ====\n\
             spawn-claude: model=sonnet (from --model arg)\n\
             ERROR Token selection failed:\n\
             error: All 15 tokens in {}/.loom/tokens are marked bad, empty, or .ranking-excluded.\n\
             \x20 - agent2: bad-marked [exhaustion, TTL] — \"exhausted: hit your session limit\"; clears in 4h50m\n\
             \x20 - agent3: bad-marked [exhaustion, TTL] — \"exhausted: hit your org's monthly spend limit\"; clears in 4h31m\n",
            dir.path().display()
        );
        // `UNKNOWN_TOKEN_NAME`: no account was ever selected, which is
        // precisely why this death cannot be charged to one.
        insert_dead_running_with_log(
            &mut registry,
            issue,
            seq as u32,
            UNKNOWN_TOKEN_NAME,
            &log_body,
        );
        registry.reap_once();
    }

    assert_eq!(
        registry.preflight_death_streak(),
        3,
        "each empty-pool token-selection death must feed the #4386 streak (#7860)"
    );
    assert!(
        registry.preflight_advisory().0,
        "reaching the default threshold must trip the workspace advisory so dispatch is held \
         instead of re-offering the same backlog every tick (#7860)"
    );
    assert!(
        registry.quarantined_issues_sorted().is_empty(),
        "the #4122 carve-out is untouched — a dead pool must never park an issue"
    );
}
