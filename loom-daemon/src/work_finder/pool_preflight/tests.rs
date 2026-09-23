//! Tests for the host-level token-pool exhaustion hold (Issue #7708).
//!
//! Hermetic by construction: every fixture workspace gets its **own**
//! repo-local `.loom/tokens/` holding at least one `.token` file, and
//! `tokens_pool::paths::resolve_tokens_dir` prefers a repo-local pool that
//! holds token files *unconditionally* — so the shared machine-level pool
//! (and therefore `LOOM_SHARED_TOKENS_DIR`, and therefore any cross-test
//! process-global env race) is never consulted. No `gh`, no network, no
//! daemon.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Result;
use serial_test::serial;
use tempfile::TempDir;

use crate::work_finder::{tick, TickReport, WorkDispatcher, WorkItem, WorkSource};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Create a workspace root whose repo-local pool holds `names.len()` accounts.
fn workspace_with_pool(names: &[&str]) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let pool = dir.path().join(".loom").join("tokens");
    fs::create_dir_all(&pool).unwrap();
    for name in names {
        fs::write(pool.join(format!("{name}.token")), "sk-ant-oat01-fake").unwrap();
    }
    dir
}

/// Bad-mark every named account with a live exhaustion-class `.bad_tokens`
/// entry — the exact shape of the incident (`exhausted: hit your session
/// limit`, a TTL cooldown, not a permanent auth mark).
fn bad_mark_all(root: &Path, names: &[&str]) {
    let pool = root.join(".loom").join("tokens");
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let body: String = names
        .iter()
        .map(|n| format!("{now} {n} exhausted: hit your session limit\n"))
        .collect();
    fs::write(pool.join(".bad_tokens"), body).unwrap();
}

/// Remove every bad-mark — the operator readmission / cooldown-expiry event.
fn readmit_all(root: &Path) {
    fs::remove_file(root.join(".loom").join("tokens").join(".bad_tokens")).unwrap();
}

// ---------------------------------------------------------------------------
// The pre-flight verdict itself
// ---------------------------------------------------------------------------

/// The core #7708 acceptance criterion at its smallest: a pool that HOLDS
/// accounts but has zero spawnable ones must hold dispatch.
#[test]
fn a_fully_bad_marked_pool_arms_the_hold() {
    let ws = workspace_with_pool(&["a", "b", "c"]);
    bad_mark_all(ws.path(), &["a", "b", "c"]);

    let host = PoolHoldState::new();
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));

    let holds = host.active_holds();
    assert_eq!(holds.len(), 1, "one hold, keyed by the resolved pool dir");
    assert_eq!(holds[0].dir, ws.path().join(".loom").join("tokens"));
    assert_eq!(holds[0].total, 3);
    assert!(
        !holds[0].wrapper_observed,
        "armed by the pre-flight read, not by an observed death"
    );
}

/// A healthy pool must never hold — the property that keeps this brake from
/// becoming a fleet-wide stall.
#[test]
fn a_healthy_pool_never_arms_the_hold() {
    let ws = workspace_with_pool(&["a", "b"]);
    let host = PoolHoldState::new();
    assert!(!host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.held_pool_count(), 0);
}

/// One usable account is enough. The hold is "zero spawnable", not "fewer
/// than we would like" — a partially-exhausted pool still dispatches, exactly
/// as it did before this change.
#[test]
fn one_remaining_spawnable_account_is_enough_to_stay_open() {
    let ws = workspace_with_pool(&["a", "b", "c"]);
    bad_mark_all(ws.path(), &["a", "b"]);

    let host = PoolHoldState::new();
    assert!(!host.observe_root(ws.path(), chrono::Utc::now()));
}

/// `.ranking`-hard-excluded accounts count as unspawnable too — the incident's
/// pool had both shapes at once (`agent2` bad-marked, `agent3` hard-excluded),
/// and a brake that saw only one of them would not have fired.
#[test]
fn ranking_hard_exclusions_count_toward_exhaustion() {
    let ws = workspace_with_pool(&["a", "b"]);
    let pool = ws.path().join(".loom").join("tokens");
    fs::write(pool.join(".ranking"), "a|exhausted\nb|exhausted\n").unwrap();

    let host = PoolHoldState::new();
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));
}

/// An ABSENT pool is a different condition with a different fix
/// (`loom-daemon tokens bootstrap`) and its own detection (#4642). It must
/// not arm THIS hold, whose operator message would send someone to
/// `tokens unblock` for a pool that does not exist.
#[test]
fn an_empty_pool_is_not_an_exhausted_pool() {
    let ws = tempfile::tempdir().unwrap();
    fs::create_dir_all(ws.path().join(".loom").join("tokens")).unwrap();

    let host = PoolHoldState::new();
    assert!(!host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.held_pool_count(), 0);
}

/// Self-heal within ONE tick of readmission, with no restart and no cached
/// verdict — the property `.loom/docs/token-pool.md` documents for #7607 and
/// the reason this brake is safe to arm aggressively.
#[test]
fn the_hold_clears_on_the_very_next_tick_after_readmission() {
    let ws = workspace_with_pool(&["a"]);
    bad_mark_all(ws.path(), &["a"]);

    let host = PoolHoldState::new();
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));

    readmit_all(ws.path());
    assert!(!host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.held_pool_count(), 0, "the hold is dropped, not just reported open");
}

/// The hold is keyed by resolved POOL directory, not workspace root: two
/// roots sharing one pool share one hold. This is what makes a workspace-level
/// check behave as the host-level brake the issue asks for.
#[test]
fn two_roots_resolving_to_the_same_pool_share_one_hold() {
    let ws = workspace_with_pool(&["a"]);
    bad_mark_all(ws.path(), &["a"]);
    // A nested root whose own repo-local pool is absent would fall through to
    // the shared pool, so instead assert the same root twice — the key is the
    // resolved dir, and two observations must not stack two holds.
    let host = PoolHoldState::new();
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.held_pool_count(), 1);
}

/// A hold refreshed by a second observation keeps its ORIGINAL `since` — an
/// operator reading the status surface needs "how long has this pool been
/// dead", not "when was it last re-observed".
#[test]
fn refreshing_a_hold_preserves_when_it_first_armed() {
    let ws = workspace_with_pool(&["a"]);
    bad_mark_all(ws.path(), &["a"]);

    let host = PoolHoldState::new();
    let first = chrono::Utc::now();
    host.observe_root(ws.path(), first);
    host.observe_root(ws.path(), first + chrono::Duration::seconds(600));

    assert_eq!(host.active_holds()[0].since, first);
}

// ---------------------------------------------------------------------------
// The post-mortem (wrapper-observed) hold
// ---------------------------------------------------------------------------

/// The divergence backstop: the wrapper proved a spawn cannot select an
/// account, while this daemon's own read of the same directory says the pool
/// is fine. The wrapper wins until the TTL — otherwise the very next tick
/// re-dispatches into the same dead pool, which is the #7708 storm.
#[test]
fn a_wrapper_observed_death_outranks_a_disagreeing_live_read() {
    let ws = workspace_with_pool(&["a"]);
    // Deliberately NOT bad-marked: the live read will report 1/1 spawnable.
    let host = PoolHoldState::new();
    assert!(!host.observe_root(ws.path(), chrono::Utc::now()));

    host.note_pool_dead(ws.path(), chrono::Utc::now());
    assert!(
        host.observe_root(ws.path(), chrono::Utc::now()),
        "a disagreeing live read must not clear a hold a real death armed"
    );
    assert!(host.active_holds()[0].wrapper_observed);
}

/// ...but it is BOUNDED. `pool_clear_estimate` never reports further than
/// 900s out, so the worst case is one doomed dispatch per host per TTL — not
/// a permanent stall that needs an operator to notice.
#[test]
fn a_wrapper_observed_hold_expires_on_its_own_ttl() {
    let ws = workspace_with_pool(&["a"]);
    let host = PoolHoldState::new();
    host.note_pool_dead(ws.path(), chrono::Utc::now());

    let ttl = host.active_holds()[0].next_clear_at;
    assert!(
        ttl <= chrono::Utc::now() + chrono::Duration::seconds(900),
        "the TTL must be capped, not open-ended"
    );
    assert!(
        !host.observe_root(ws.path(), ttl + chrono::Duration::seconds(1)),
        "past the TTL the live read decides again"
    );
    assert_eq!(host.held_pool_count(), 0);
}

// ---------------------------------------------------------------------------
// The incident-shaped regression test (Issue #7708 acceptance criterion)
// ---------------------------------------------------------------------------

/// One ready issue, and nothing else to dispatch.
struct OneReadyIssue {
    issue: u32,
}

impl WorkSource for OneReadyIssue {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        Ok(vec![WorkItem::new(self.issue, vec!["loom:issue".to_string()])])
    }
}

/// Counts every dispatch attempt. A dispatch attempt is exactly the
/// `loom:issue -> loom:building` label flip plus the `loom:lease` record
/// comment in production, so "zero dispatches" here IS "zero label flips and
/// zero lease comments" — the AC's forge-side claim, asserted without a forge.
#[derive(Default)]
struct CountingDispatcher {
    attempts: usize,
}

impl WorkDispatcher for CountingDispatcher {
    fn in_flight(&self) -> HashSet<u32> {
        HashSet::new()
    }
    fn dispatch(&mut self, _issue: u32, _complexity: Option<&str>) -> Result<bool> {
        self.attempts += 1;
        Ok(true)
    }
}

/// Run `ticks` work-finder ticks for one host against `root` and return how
/// many dispatch attempts it made.
///
/// `host = Some(..)` is the post-fix path (the tick consults that host's own
/// pool hold). `host = None` reproduces the PRE-fix work finder exactly: the
/// `halted` slice carried the main-health / drain / breaker / #5030 terms and
/// nothing that could see `.bad_tokens`, so a dead pool was invisible to it.
fn ticks_for_host(host: Option<&PoolHoldState>, root: &Path, issue: u32, ticks: usize) -> usize {
    let mut source = OneReadyIssue { issue };
    let mut dispatcher = CountingDispatcher::default();
    for _ in 0..ticks {
        let halted = host.is_some_and(|h| h.observe_root(root, chrono::Utc::now()));
        let report: TickReport = tick(&mut source, &mut dispatcher, 4, halted).unwrap();
        assert_eq!(report.seen, 1, "the issue stays ready the whole time");
    }
    dispatcher.attempts
}

/// **The #7708 incident, reproduced.** Four hosts, one ready issue, a pool
/// with zero usable accounts. Before this change each host re-dispatched on
/// its own cadence (the #4485 ladder being per-issue and therefore incapable
/// of damping a pool-wide fault), producing 20 dispatches of one issue in
/// 4.3 h across four hosts — 39 permanent lease comments and 40 label flips
/// on a single public issue, for zero work.
///
/// With the pre-flight the correct number is not "≤ 1 per host per TTL" but
/// **zero**: no host ever reaches the dispatch path at all, because each one
/// reads the dead pool before flipping anything.
#[test]
fn four_hosts_one_ready_issue_and_a_dead_pool_dispatch_nothing() {
    let ws = workspace_with_pool(&["agent2", "agent3", "agent8", "robb", "rjwalters"]);
    bad_mark_all(ws.path(), &["agent2", "agent3", "agent8", "robb", "rjwalters"]);

    // Control — the PRE-fix work finder, whose `halted` slice had no term
    // that could see `.bad_tokens`. This is the storm: every tick of every
    // host dispatches into a pool that cannot spawn anything.
    let unbraked: usize = (0..4)
        .map(|_| ticks_for_host(None, ws.path(), 6704, 20))
        .sum();
    assert_eq!(
        unbraked, 80,
        "precondition: without the pre-flight, 4 hosts x 20 ticks = 80 doomed dispatches"
    );

    // Four independent daemons = four hosts. Real hosts each read their OWN
    // copy of the pool (see `hosts_with_separate_pool_dirs_hold_independently`);
    // one fixture directory stands in for four copies that have all reached
    // the same dead state — which each host observes on its own tick, with no
    // peer broadcast (#8001).
    let hosts = [
        PoolHoldState::new(),
        PoolHoldState::new(),
        PoolHoldState::new(),
        PoolHoldState::new(),
    ];

    let total: usize = hosts
        .iter()
        .map(|host| ticks_for_host(Some(host), ws.path(), 6704, 20))
        .sum();

    assert_eq!(
        total, 0,
        "80 ticks across 4 hosts into a 0/5-spawnable pool must produce ZERO dispatch \
         attempts — no label flip, no lease comment"
    );
    for host in &hosts {
        assert_eq!(host.held_pool_count(), 1, "every host holds the dead pool");
    }
}

/// The deployment shape that decided #8001 (no peer broadcast): every host
/// resolves its pool to a directory on its **own** disk, so two hosts can hold
/// the same account names with different health. Host A's copy is fully
/// bad-marked; host B's copy of the *same* accounts still has one spawnable.
/// A must hold and B must keep dispatching — the hold's key is a host-local
/// path, and nothing A observed says anything about B's pool. A broadcast of
/// A's hold would suppress B here, which is the bug it would introduce.
#[test]
fn hosts_with_separate_pool_dirs_hold_independently() {
    let accounts = ["agent2", "agent3", "robb"];
    let host_a_ws = workspace_with_pool(&accounts);
    let host_b_ws = workspace_with_pool(&accounts);
    bad_mark_all(host_a_ws.path(), &accounts);
    bad_mark_all(host_b_ws.path(), &["agent2", "agent3"]);

    let host_a = PoolHoldState::new();
    let host_b = PoolHoldState::new();

    assert_eq!(ticks_for_host(Some(&host_a), host_a_ws.path(), 8001, 5), 0);
    assert_eq!(
        ticks_for_host(Some(&host_b), host_b_ws.path(), 8001, 5),
        5,
        "a host whose own pool still has a spawnable account must keep dispatching"
    );

    let a_holds = host_a.active_holds();
    assert_eq!(a_holds.len(), 1);
    assert_eq!(a_holds[0].dir, host_a_ws.path().join(".loom").join("tokens"));
    assert_eq!(host_b.held_pool_count(), 0, "host A's hold never reaches host B");
}

/// The other half of the same test: the instant the pool recovers, all four
/// hosts resume on their next tick. A brake that needs an operator to release
/// it would just be a different outage.
#[test]
fn all_four_hosts_resume_on_the_tick_after_the_pool_recovers() {
    let ws = workspace_with_pool(&["a", "b"]);
    bad_mark_all(ws.path(), &["a", "b"]);

    let hosts = [
        PoolHoldState::new(),
        PoolHoldState::new(),
        PoolHoldState::new(),
        PoolHoldState::new(),
    ];
    for host in &hosts {
        assert_eq!(ticks_for_host(Some(host), ws.path(), 6704, 5), 0);
    }

    readmit_all(ws.path());

    for host in &hosts {
        assert_eq!(
            ticks_for_host(Some(host), ws.path(), 6704, 1),
            1,
            "one tick, one dispatch — no restart, no manual release"
        );
        assert_eq!(host.held_pool_count(), 0);
    }
}

// ---------------------------------------------------------------------------
// #8554: preference-aware hold — arms on the WHOLE list, not just Claude
// ---------------------------------------------------------------------------

/// Every env var that can pin a runtime or repoint the codex account reader.
/// Cleared for the scope of a test and restored on drop, mirroring
/// `role_runner::runtime_preflight::tests::EnvGuard`.
const PREFERENCE_ENV: [&str; 8] = [
    "LOOM_RUNTIME",
    "LOOM_RUNTIME_SWEEP_LIFECYCLE",
    "LOOM_CODEX_HOME",
    "CODEX_HOME",
    "LOOM_CODEX_PROFILE",
    "LOOM_SPAWN_NO_EXPORT",
    "LOOM_CODEX_NO_EXEC",
    "LOOM_CODEX_PROFILE_ROOT",
];

struct PreferenceEnvGuard(Vec<(&'static str, Option<String>)>);

impl PreferenceEnvGuard {
    fn new(profile_root: &Path) -> Self {
        let prior = PREFERENCE_ENV
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in PREFERENCE_ENV {
            std::env::remove_var(key);
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profile_root);
        Self(prior)
    }
}

impl Drop for PreferenceEnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A workspace with BOTH a Claude token pool (`workspace_with_pool`) and the
/// on-disk role/runtime manifests plus an executable `codex` adapter stub
/// that `runtime_preference::resolve_runtime("sweep-lifecycle", ...)` needs
/// to admit a fallback tap for real, instead of failing closed on missing
/// files. `sweep-lifecycle`'s role-manifest lookup substitutes `builder.json`
/// (`runtime_admission`'s own fallback), so that is the file written here.
fn workspace_with_pool_and_preference(names: &[&str], config: &serde_json::Value) -> TempDir {
    let dir = workspace_with_pool(names);
    for sub in ["roles", "runtimes", "scripts"] {
        fs::create_dir_all(dir.path().join(".loom").join(sub)).unwrap();
    }
    fs::write(dir.path().join(".loom/config.json"), config.to_string()).unwrap();
    fs::write(
        dir.path().join(".loom/roles/builder.json"),
        r#"{"runtimeRequirements":["mcp"]}"#,
    )
    .unwrap();
    fs::write(
        dir.path().join(".loom/runtimes/codex.json"),
        r#"{"runtime":"codex","accountProvider":"codex","capabilities":{"mcp":"yes"}}"#,
    )
    .unwrap();
    // Both adapters, so the `claude` tap is genuinely ADMITTED and its skip
    // (when it is skipped) is `unavailable(claude_tokens: …)` — the #7708
    // pool-exhaustion shape this hold exists for — and never the materially
    // different `not-admitted(...)` a missing adapter would produce.
    for runtime in ["codex", "claude"] {
        let adapter = dir.path().join(format!(".loom/scripts/spawn-{runtime}.sh"));
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// The core #8554 acceptance criterion: the Claude pool is fully bad-marked
/// (the exact #7708 shape), but `runtimes.preference` names a codex fallback
/// with a healthy account — the hold must NOT arm; dispatch belongs on codex
/// instead of holding every workspace resolving to this pool.
#[test]
#[serial]
fn a_spawnable_lower_tap_keeps_the_hold_from_arming() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let ws = workspace_with_pool_and_preference(
        &["a"],
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
    );
    bad_mark_all(ws.path(), &["a"]);

    let host = PoolHoldState::new();
    assert!(
        !host.observe_root(ws.path(), chrono::Utc::now()),
        "a spawnable fallback tap must keep the hold from arming"
    );
    assert_eq!(host.held_pool_count(), 0);
}

/// The other half: every tap in the list is unavailable too (here, zero
/// codex accounts) — the hold still arms, exactly as the Claude-only check
/// did before #8554.
#[test]
#[serial]
fn a_wholly_unavailable_preference_list_still_arms_the_hold() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    // No codex profile directories created: the codex pool is provisioned
    // (manifest + adapter exist) but has zero enabled accounts.
    let ws = workspace_with_pool_and_preference(
        &["a"],
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
    );
    bad_mark_all(ws.path(), &["a"]);

    let host = PoolHoldState::new();
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.held_pool_count(), 1);
}

/// A healthy Claude pool with a preference list configured never holds —
/// tier 0 serves the work, exactly as the no-preference case does.
#[test]
#[serial]
fn a_healthy_claude_pool_never_arms_even_with_a_preference_list() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    let ws = workspace_with_pool_and_preference(
        &["a", "b"],
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
    );

    let host = PoolHoldState::new();
    assert!(!host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.held_pool_count(), 0);
}

/// An operator pin (`LOOM_RUNTIME=claude`) disables fall-through at the hold
/// too: a dry Claude pool still arms the hold even with a spawnable codex
/// fallback configured, because the pin is a deliberate act that a silent
/// route-around would defeat.
#[test]
#[serial]
fn an_operator_pin_disables_fall_through_at_the_hold() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let ws = workspace_with_pool_and_preference(
        &["a"],
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
    );
    bad_mark_all(ws.path(), &["a"]);
    std::env::set_var("LOOM_RUNTIME", "claude");

    let host = PoolHoldState::new();
    assert!(
        host.observe_root(ws.path(), chrono::Utc::now()),
        "a pin must keep the hold on Claude even with a spawnable fallback"
    );
}
