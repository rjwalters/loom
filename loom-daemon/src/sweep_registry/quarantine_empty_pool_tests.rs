//! End-to-end regression coverage for the empty-pool redispatch storm
//! (#7860, and its pool-level death class + host hold, #7708).
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

// ===========================================================================
// `no-usable-account`: the pool-level death class and its brakes (Issue #7708)
// ===========================================================================

/// The log tail `spawn-claude.sh` leaves behind when its token-selection step
/// finds a pool in which every account is bad-marked or `.ranking`-excluded.
///
/// Load-bearing details, both of which reproduce the production shape:
/// - no `# CLAUDE_CLI_START` anywhere — the child died before the CLI existed,
///   which is why no account can be named;
/// - the per-account diagnostic echoes each account's stored `.bad_tokens`
///   reason, whose prose matches `exhaustion_signatures`' `rate-limited` row.
///   That echo is what made all 228 incident deaths journal as
///   `account-exhausted:rate-limited` with `token=unknown`.
fn no_usable_account_log(issue: u32, pool: &std::path::Path) -> String {
    format!(
        "==== loom-daemon dispatch: sweep-issue-{issue}-0 ====\n\
         spawn-claude: model=sonnet (from --model arg)\n\
         ERROR Token selection failed:\n\
         error: All 19 tokens in {} are marked bad, empty, or .ranking-excluded.\n\
         \x20 - agent16-2amlogic: bad-marked [exhaustion, TTL] — \"exhausted: hit your session limit\"; clears in 2h25m\n\
         \x20 - agent2-2amlogic: bad-marked [exhaustion, TTL] — \"exhausted: hit your monthly spend limit\"\n\
         ERROR Run 'loom-daemon tokens bootstrap' to populate <repo>/.loom/tokens/,\n",
        pool.display()
    )
}

/// Issue #7708 AC: *"A sweep log ending in the wrapper's 'no usable accounts
/// anywhere' block is classified `no-usable-account` … and does not increment
/// the #4485 ladder for the issue"*.
///
/// The per-issue ladder is the brake that was left holding this fault, and it
/// structurally cannot damp it: keyed per issue and capped at 900 s, ~10 ready
/// issues each behaving perfectly still aggregate to ~40 doomed spawns an hour
/// — the observed 228-in-4.3 h rate. Charging it here is worse than useless,
/// because it also *misreports* a pool-wide fault as that issue's dispatch
/// having failed. So this death arms neither arm of the ladder.
///
/// The `death_class` assertion is the other half of the same AC — *"existing
/// #4644 status wording still shows the death"*. `death_class` keeps its
/// `preflight-token-selection-failed` value from #4644/#7860; the NEW,
/// distinct class rides on `crash_classification`, which is where
/// `classify_crash`'s verdict is journaled. Both are asserted so a future
/// change cannot quietly trade one surface for the other.
#[test]
fn a_no_usable_account_death_is_exempt_from_the_per_issue_dispatch_ladder() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-9");
    let pool = dir.path().join(".loom").join("tokens");

    let sweep_id = insert_dead_running_with_log(
        &mut registry,
        6704,
        0,
        UNKNOWN_TOKEN_NAME,
        &no_usable_account_log(6704, &pool),
    );
    registry.reap_once();

    assert_eq!(
        registry.dispatch_failure_count(6704),
        0,
        "a pool-wide fault must never be charged to whichever issue happened to be dispatched \
         into it — before #7708 this armed the ladder on every one of the 228 incident deaths"
    );
    assert!(
        registry
            .dispatch_backoff_remaining(6704, Utc::now())
            .is_none(),
        "…and therefore no backoff window is in effect for the issue either"
    );
    assert!(
        registry.quarantined_issues_sorted().is_empty(),
        "the #4122 carve-out is untouched — a dead pool must never park an issue"
    );

    let records = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let record = records
        .iter()
        .find(|r| r.sweep_id == sweep_id)
        .expect("the death must be journaled");
    assert_eq!(
        record.crash_classification.as_deref(),
        Some("no-usable-account"),
        "the journaled class must name the POOL, not an account — `account-exhausted:rate-limited` \
         asserts a named account hit a limit about a death in which none was ever selected"
    );
    assert_eq!(
        record.death_class.as_deref(),
        Some("preflight-token-selection-failed"),
        "#4644/#7860's own surface is unchanged: the pre-flight death class still reports the \
         token-selection shape and still feeds the #4386 workspace streak"
    );
}

/// Issue #7708 AC: the same death *"arms the host hold"*.
///
/// This is the post-mortem arming path — the backstop for the case where the
/// wrapper and the daemon disagree about the pool's health (the divergence at
/// the heart of the incident: `.ranking` reported six accounts `available`
/// while all six carried live `.bad_tokens` cooldowns). The pre-flight in
/// `work_finder::pool_preflight` reads the pool directly and normally stops
/// the dispatch before it happens; when it nonetheless does happen, the
/// wrapper's verdict wins and holds the whole host, bounding the damage to one
/// doomed dispatch per host per TTL rather than one per tick.
///
/// `#[serial]` because the hold set consulted here is the process-global one —
/// this host's — by construction.
#[test]
#[serial_test::serial]
fn a_no_usable_account_death_arms_the_host_level_pool_hold() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-9");
    let pool = dir.path().join(".loom").join("tokens");

    crate::work_finder::pool_preflight::PoolHoldState::global().clear_all();
    insert_dead_running_with_log(
        &mut registry,
        6704,
        0,
        UNKNOWN_TOKEN_NAME,
        &no_usable_account_log(6704, &pool),
    );
    registry.reap_once();

    let holds = crate::work_finder::pool_preflight::active_holds();
    let hold = holds
        .iter()
        .find(|h| h.dir == pool)
        .expect("the death must arm a hold on the pool the sweep resolved");
    assert!(
        hold.wrapper_observed,
        "armed by an observed death, so it outranks this daemon's own disagreeing read"
    );
    assert!(
        hold.next_clear_at > Utc::now(),
        "the hold must carry a forward clear estimate for the status surface"
    );
    crate::work_finder::pool_preflight::PoolHoldState::global().clear_all();
}

/// Issue #7708 (the #4122 contract, asserted from the reaper rather than the
/// classifier): a genuine MID-RUN exhaustion — the CLI started, ran, and then
/// hit a limit — is untouched by any of this. It still marks the spawn account
/// bad, still leaves the issue's tally alone, and must NOT arm the host pool
/// hold, because the pool is fine: one account died and rotation is the cure.
///
/// This is the specific way the #7708 change could have silently broken
/// account rotation while passing everything else, so it is pinned here.
#[test]
#[serial_test::serial]
fn a_mid_run_exhaustion_still_charges_the_account_and_never_holds_the_pool() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    seed_token_pool(dir.path(), "agent-9");
    let pool = dir.path().join(".loom").join("tokens");

    crate::work_finder::pool_preflight::PoolHoldState::global().clear_all();
    let sweep_id = insert_dead_running_with_log(
        &mut registry,
        6705,
        0,
        "agent-9",
        "==== loom-daemon dispatch: sweep-issue-6705-0 ====\n\
         spawn-claude: using OAuth account 'agent-9' (mode=ranking)\n\
         # CLAUDE_CLI_START\n\
         Claude: You've hit your weekly limit — try again later\n",
    );
    registry.reap_once();

    let records = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let record = records
        .iter()
        .find(|r| r.sweep_id == sweep_id)
        .expect("the death must be journaled");
    assert_eq!(
        record.crash_classification.as_deref(),
        Some("account-exhausted:rate-limited"),
        "reaching the CLI proves an account WAS selected — this stays an account death (#4122)"
    );
    assert!(
        bad_tokens::is_bad(dir.path(), "agent-9"),
        "the spawn account must still be marked bad, which is how the pool heals itself"
    );
    // Scoped to THIS test's pool rather than asserting the global set is
    // empty: the hold set is process-global by design, so a concurrently
    // running test with its own tempdir pool may legitimately hold its own.
    assert!(
        !crate::work_finder::pool_preflight::active_holds()
            .iter()
            .any(|h| h.dir == pool),
        "one exhausted account is NOT an exhausted pool — holding the host here would turn \
         routine rotation into a fleet-wide stall"
    );
    crate::work_finder::pool_preflight::PoolHoldState::global().clear_all();
}
