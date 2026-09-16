//! Classifier-level regression coverage for the empty-pool redispatch storm
//! (#7860).
//!
//! In its own sibling module rather than `crash_signals.rs`'s `mod tests`:
//! that file sits just under the file-size ratchet's 1000-line threshold and
//! adding these cases inline would push it over (see
//! `.loom/docs/file-size-policy.md`). Mirrors the existing
//! `guards_union_tests.rs` precedent.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

/// A verbatim-shaped excerpt of the tail `spawn-claude.sh` leaves behind
/// when its token-selection step (exit 78) finds an unusable pool
/// (Issue #7860). Trimmed from a real production sweep log
/// (`sweep-issue-7815-1789576137`, 2026-09-16) — the load-bearing detail
/// is that the per-account diagnostic echoes each account's stored
/// `.bad_tokens` reason, whose prose matches `exhaustion_signatures`'
/// `rate-limited` regex even though NO account was ever selected.
fn empty_pool_token_selection_tail() -> String {
    "==== loom-daemon dispatch: 2026-09-16T16:29:10Z sweep_id=sweep-issue-7815-1 issue=7815 ====\n\
     spawn-claude: re-exec at nice -n 10 (issue #4233; LOOM_SWEEP_NICE=0 to disable)\n\
     spawn-claude: model=sonnet (from --model arg)\n\
     ERROR Token selection failed:\n\
     Resolved workspace: /home/ubuntu/GitHub/loom\n\
     error: All 15 tokens in /home/ubuntu/GitHub/loom/.loom/tokens are marked bad, empty, or .ranking-excluded.\n\
     \x20 - agent2-2amlogic: bad-marked [exhaustion, TTL] at 2026-09-16T14:15:43Z — \"exhausted: hit your org's monthly spend limit\"; clears in 4h31m\n\
     \x20 - agent3-2amlogic: bad-marked [exhaustion, TTL] at 2026-09-16T15:34:37Z — \"exhausted: hit your session limit\"; clears in 4h50m\n\
     \x20 - agent4-2amlogic: hard-excluded by .ranking status (exhausted)\n\
     ERROR Run 'loom-daemon tokens bootstrap' to populate <repo>/.loom/tokens/,\n"
        .to_string()
}

/// Issue #7860 (regression): the empty-pool token-selection tail above
/// DOES match an account-exhaustion signature — purely because the pool's
/// own per-account diagnostic is echoed into it. This asserts the
/// misattribution the fix has to work around still exists at the
/// `classify_account_exhaustion` layer (that classifier is deliberately
/// unchanged; the precedence fix lives in `classify_preflight_outcome`).
#[test]
fn empty_pool_tail_still_matches_an_exhaustion_signature() {
    let tail = empty_pool_token_selection_tail();
    assert_eq!(
        classify_account_exhaustion(&tail),
        Some("rate-limited"),
        "the echoed `.bad_tokens` reasons match the rate-limited regex — this is the \
         misattribution #7860 works around, not something the fix removes"
    );
    assert_eq!(
        classify_preflight_death(&tail),
        Some("preflight-token-selection-failed"),
        "precondition: the tail is ALSO an explicit token-selection preflight death"
    );
}

/// Issue #7860 (regression): a token-selection pre-flight death must feed
/// the #4386 workspace streak instead of being swallowed by the #4122
/// "exhaustion wins" precedence.
///
/// **Fails before the fix** (`classify_preflight_outcome` consulted
/// `classify_account_exhaustion` first and returned `Unknown`), which left
/// the streak neither incremented nor reset — so the dampener built for
/// "the whole pool is dead" never armed from an issue dispatch, and the
/// work finder re-offered the same backlog forever, burning a
/// `loom:issue` <-> `loom:building` label-flip pair per attempt
/// (kicad-tools#5333: 152 flips; rjwalters/loom#7815: 94).
#[test]
fn token_selection_preflight_death_outranks_exhaustion_precedence() {
    assert_eq!(
        classify_preflight_outcome(Some(&empty_pool_token_selection_tail())),
        PreflightOutcome::Preflight("preflight-token-selection-failed"),
    );
}

/// Issue #7860: the carve-out above is exactly one signature wide. Every
/// other exhaustion shape still yields to `classify_account_exhaustion`
/// (#4122) — those deaths really are attributable to a named spawn
/// account, so they must stay neutral for the pre-flight streak.
#[test]
fn genuine_exhaustion_still_wins_over_other_preflight_shapes() {
    // Mid-run exhaustion after the CLI started: unchanged `Unknown`.
    assert_eq!(
        classify_preflight_outcome(Some(
            "==== loom-daemon dispatch: now ====\n\
             spawn-claude: using OAuth account 'agent4-2amlogic' (mode=ranking)\n\
             # CLAUDE_CLI_START\n\
             Claude: You've hit your weekly limit — try again later\n"
        )),
        PreflightOutcome::Unknown,
    );
    // Exhaustion on a tail that never reached CLI start (so the
    // absence-based `preflight-no-cli-start` label would otherwise
    // match): still `Unknown`, exactly as before #7860.
    assert_eq!(
        classify_preflight_outcome(Some(
            "==== loom-daemon dispatch: now ====\n\
             spawn-claude: using OAuth account 'agent4-2amlogic' (mode=ranking)\n\
             RATE_LIMIT_ABORT\n"
        )),
        PreflightOutcome::Unknown,
    );
    // The other explicit pre-flight marker, with no exhaustion prose:
    // unchanged `Preflight`.
    assert_eq!(
        classify_preflight_outcome(Some(
            "==== loom-daemon dispatch: now ====\n# MCP_PREFLIGHT_FAILED\n"
        )),
        PreflightOutcome::Preflight("preflight-mcp-failed"),
    );
    // A healthy run is still definitively NonPreflight (resets the streak).
    assert_eq!(
        classify_preflight_outcome(Some(
            "==== loom-daemon dispatch: now ====\n# CLAUDE_CLI_START\nClaude: working\n"
        )),
        PreflightOutcome::NonPreflight,
    );
    // An unreadable log is still Unknown.
    assert_eq!(classify_preflight_outcome(None), PreflightOutcome::Unknown);
}
