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
    fn dispatch(&mut self, _issue: u32) -> Result<bool> {
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

    // Four independent daemons = four hosts, all sharing one pool directory.
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
    "LOOM_RUNTIME_BUILDER",
    "LOOM_CODEX_HOME",
    "CODEX_HOME",
    "LOOM_CODEX_PROFILE",
    "LOOM_SPAWN_NO_EXPORT",
    "LOOM_CODEX_NO_EXEC",
    "LOOM_CODEX_PROFILE_ROOT",
];

/// The second field holds the crate-wide `LOOM_CODEX_PROFILE_ROOT` lock (#9964).
struct PreferenceEnvGuard(
    Vec<(&'static str, Option<String>)>,
    #[allow(dead_code)] crate::tokens_pool::profile_root_env::ProfileRootLock,
);

impl PreferenceEnvGuard {
    fn new(profile_root: &Path) -> Self {
        let lock = crate::tokens_pool::profile_root_env::lock();
        let prior = PREFERENCE_ENV
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in PREFERENCE_ENV {
            std::env::remove_var(key);
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profile_root);
        Self(prior, lock)
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
/// that `runtime_preference::resolve_runtime("builder", ...)` needs to admit
/// a fallback tap for real, instead of failing closed on missing files. The
/// pre-flight probes the **builder** role (#13983), so `builder.json` is the
/// manifest read directly.
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

// ---------------------------------------------------------------------------
// #13983: the probe must not hold on a rejection only containment would lift
// ---------------------------------------------------------------------------

/// Rewrite the fixture's `builder.json` role manifest to require exactly
/// `worktreeIsolation` — the real `defaults/roles/builder.json` vs
/// `defaults/runtimes/codex.json` shape, whose `"partial"` leaves it unmet —
/// so a codex tap's admission refusal is `not-admitted(worktreeIsolation)`
/// and nothing else: the containment-eligible (#8787) rejection this section
/// exercises. With `capability` instead, the refusal names that capability
/// and is NOT containment-liftable — the control case.
fn require_capability(ws: &TempDir, capability: &str) {
    fs::write(
        ws.path().join(".loom/roles/builder.json"),
        format!(r#"{{"runtimeRequirements":["{capability}"]}}"#),
    )
    .unwrap();
}

/// The incident shape (#13983): on the one host that admits Codex, a
/// codex-only preference table made the probe — which resolves with
/// `NoContainment` — report the whole list unavailable, holding the repo
/// forever although the real step spawn carries a containment preparer
/// (#8787) and would admit the tap. The probe must not hold, and the
/// unheld root dispatches on every tick.
#[test]
#[serial]
fn a_containment_liftable_codex_only_list_does_not_hold_or_stop_dispatch() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    let ws = workspace_with_pool_and_preference(
        &["a"],
        &serde_json::json!({"runtimes": {"preference": ["codex"]}}),
    );
    require_capability(&ws, "worktreeIsolation");

    let host = PoolHoldState::new();
    assert!(
        !host.observe_root(ws.path(), chrono::Utc::now()),
        "a codex-only list whose refusal only containment would lift must not hold (#13983)"
    );
    assert_eq!(host.held_pool_count(), 0);
    // The Builder step is dispatched: an unheld root attempts a dispatch on
    // every tick, where an attempt is exactly the `loom:issue` →
    // `loom:building` label flip in production.
    assert_eq!(
        ticks_for_host(Some(&host), ws.path(), 13983, 3),
        3,
        "an unheld root dispatches on every tick"
    );
}

/// The carve-out composes with fall-through: a dry Claude tap above a
/// containment-liftable codex tap is not an exhausted list either — the real
/// launch skips the dry tap and contains onto codex.
#[test]
#[serial]
fn a_dry_tap_above_a_containment_liftable_codex_tap_does_not_hold() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    let ws = workspace_with_pool_and_preference(
        &["a"],
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
    );
    require_capability(&ws, "worktreeIsolation");
    bad_mark_all(ws.path(), &["a"]);

    let host = PoolHoldState::new();
    assert!(
        !host.observe_root(ws.path(), chrono::Utc::now()),
        "a dry claude tap above a liftable codex tap must not hold (#13983)"
    );
    assert_eq!(host.held_pool_count(), 0);
}

/// The carve-out is narrow: a codex-only list refused for a capability
/// containment can NEVER satisfy is a genuine whole-list unavailability and
/// still arms the hold.
#[test]
#[serial]
fn a_non_liftable_codex_refusal_still_arms_the_hold() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    let ws = workspace_with_pool_and_preference(
        &["a"],
        &serde_json::json!({"runtimes": {"preference": ["codex"]}}),
    );
    // The fixture's codex manifest has no `subagents` capability, so the
    // refusal names it — not the one requirement containment satisfies.
    require_capability(&ws, "subagents");

    let host = PoolHoldState::new();
    assert!(
        host.observe_root(ws.path(), chrono::Utc::now()),
        "a refusal naming a capability containment cannot satisfy still holds"
    );
    assert_eq!(host.held_pool_count(), 1);
}

/// AC 2: the halt is not keyed on `sweep-lifecycle`, the role no step
/// resolves since #11909. A repo whose `sweep-lifecycle` table is wholly
/// unavailable while the builder probe's own list resolves is not held: the
/// probe consults the launch's tables, not the retired sweep's.
#[test]
#[serial]
fn a_wholly_unavailable_sweep_lifecycle_table_does_not_key_the_halt() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let ws = workspace_with_pool_and_preference(
        &["a"],
        &serde_json::json!({"runtimes": {
            "rolePreference": {"sweep-lifecycle": ["claude"]},
            "preference": ["claude", "codex"],
        }}),
    );
    bad_mark_all(ws.path(), &["a"]);

    let host = PoolHoldState::new();
    assert!(
        !host.observe_root(ws.path(), chrono::Utc::now()),
        "a dead sweep-lifecycle table must not hold step dispatch (#13983 AC 2)"
    );
    assert_eq!(host.held_pool_count(), 0);
}

// ---------------------------------------------------------------------------
// Tap-less preference resolution is config, not exhaustion (#13428)
// ---------------------------------------------------------------------------

/// A `roleDraw` table for `builder` whose only provider is excluded: the
/// walk runs over ZERO taps, so `resolve_runtime` returns a
/// `Decision::Preference` whose resolution `is_empty_list()`.
fn tapless_config() -> serde_json::Value {
    serde_json::json!({"runtimes": {
        "excludeProviders": ["codex"],
        "roleDraw": {"builder": [{"attempt": 1, "draw": {"codex": 100}}]},
    }})
}

/// AC1: a tap-less root over a healthy pool must not hold or arm anything.
/// Reverting the `is_empty_list()` guard in `observe_root_edge` makes this
/// fail: the empty resolution has `chosen == None`, so it reads as exhausted.
#[test]
#[serial]
fn a_tapless_root_with_a_healthy_pool_does_not_hold() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    let ws = workspace_with_pool_and_preference(&["a", "b"], &tapless_config());

    let host = PoolHoldState::new();
    assert!(!host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.held_pool_count(), 0);
}

/// AC4: a tap-less root over a genuinely exhausted pool still holds — the
/// pool-state branch decides, exactly as with no preference configured.
#[test]
#[serial]
fn a_tapless_root_with_a_really_exhausted_pool_still_arms_the_hold() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    let ws = workspace_with_pool_and_preference(&["a"], &tapless_config());
    bad_mark_all(ws.path(), &["a"]);

    let host = PoolHoldState::new();
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.held_pool_count(), 1);
}

/// AC3: two roots sharing one HEALTHY pool, one tap-less and one with a
/// preference list. Neither may hold, and no arm/clear edge may appear across
/// repeated ticks. Before the fix the tap-less root read as exhausted and armed
/// the shared key, which the other root then cleared: the per-tick WARN /
/// recovered pair.
#[test]
#[serial]
fn a_tapless_root_does_not_flap_the_hold_shared_with_a_spawnable_root() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();

    // Neither root has a repo-local pool, so both resolve to the shared one.
    let shared = tempfile::tempdir().unwrap();
    fs::write(shared.path().join("a.token"), "sk-ant-oat01-fake").unwrap();
    let prior = std::env::var_os("LOOM_SHARED_TOKENS_DIR");
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", shared.path());

    let tapless = workspace_with_pool_and_preference(&[], &tapless_config());
    let spawnable = workspace_with_pool_and_preference(
        &[],
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
    );

    let host = PoolHoldState::new();
    let mut edges = Vec::new();
    for _ in 0..3 {
        for root in [tapless.path(), spawnable.path()] {
            let obs = host.observe_root_edge(root, chrono::Utc::now());
            edges.push((root == tapless.path(), obs.held, obs.edge));
        }
    }
    match prior {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }

    assert!(edges.iter().all(|(_, held, edge)| !held && edge.is_none()), "{edges:?}");
    assert_eq!(host.held_pool_count(), 0);
}

/// #13539: two roots on ONE pool — A lists only `codex` and no Codex seat
/// exists, B lists `claude` with a spawnable account. Across repeated ticks A
/// stays held and B stays free, the hold never disappears between A's ticks
/// (it used to be cleared by B's, flapping the arm WARN / clear INFO per
/// tick), and nothing is ever offered for broadcast. When a Codex seat
/// appears A releases exactly once.
#[test]
#[serial]
fn a_per_workspace_preference_hold_survives_a_sibling_root_on_the_same_pool() {
    let profiles = tempfile::tempdir().unwrap();
    let _env = PreferenceEnvGuard::new(profiles.path());

    let shared = tempfile::tempdir().unwrap();
    fs::write(shared.path().join("a.token"), "sk-ant-oat01-fake").unwrap();
    let prior = std::env::var_os("LOOM_SHARED_TOKENS_DIR");
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", shared.path());

    let codex_only = workspace_with_pool_and_preference(
        &[],
        &serde_json::json!({"runtimes": {"preference": ["codex"]}}),
    );
    let claude = workspace_with_pool_and_preference(
        &[],
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
    );

    let host = PoolHoldState::new();
    let mut observed = Vec::new();
    for _ in 0..4 {
        for root in [codex_only.path(), claude.path()] {
            let obs = host.observe_root_edge(root, chrono::Utc::now());
            observed.push((root == codex_only.path(), obs.held, obs.edge));
            if root == codex_only.path() {
                assert_eq!(
                    host.held_pool_count(),
                    1,
                    "the sibling root's tick must not clear the codex-only hold"
                );
            }
        }
    }
    assert!(
        observed
            .iter()
            .all(|(is_codex_only, held, edge)| *is_codex_only == *held && edge.is_none()),
        "{observed:?}"
    );
    assert_eq!(host.active_holds().len(), 1);

    // A Codex seat logs in: the codex-only root releases, once.
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let obs = host.observe_root_edge(codex_only.path(), chrono::Utc::now());
    assert!(!obs.held && obs.edge.is_none());
    assert_eq!(host.held_pool_count(), 0);
    assert!(!host.observe_root(codex_only.path(), chrono::Utc::now()));

    match prior {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
}

// ---------------------------------------------------------------------------
// #8931: one `loom.pool.hold` span per hold, armed → cleared
// ---------------------------------------------------------------------------

/// A hold that arms and later clears emits exactly one `loom.pool.hold` span
/// starting at the arm instant and ending at the clear; the steady-state
/// ticks in between emit none.
#[test]
fn an_armed_then_cleared_hold_emits_one_hold_span() {
    let ws = workspace_with_pool(&["a", "b"]);
    bad_mark_all(ws.path(), &["a", "b"]);
    let state = PoolHoldState::new();
    let armed_at = chrono::Utc::now() - chrono::Duration::seconds(600);
    let ((), held) = crate::observability::ops::capture::capture(|| {
        assert!(state.observe_root(ws.path(), armed_at));
        assert!(state.observe_root(ws.path(), armed_at + chrono::Duration::seconds(60)));
    });
    assert!(held.spans.is_empty(), "no span while the hold is live");

    readmit_all(ws.path());
    let cleared_at = armed_at + chrono::Duration::seconds(300);
    let ((), cleared) = crate::observability::ops::capture::capture(|| {
        assert!(!state.observe_root(ws.path(), cleared_at));
    });
    assert_eq!(cleared.spans.len(), 1, "{:?}", cleared.spans);
    let span = &cleared.spans[0];
    assert_eq!(span.name.as_str(), "loom.pool.hold");
    assert_eq!((span.started_at, span.ended_at), (armed_at, cleared_at));
    assert_eq!(span.attributes["loom.pool.hold.post_mortem"], "false");
    assert_eq!(span.attributes["loom.pool.hold.accounts"], "2");
    assert!(cleared.metrics.is_empty());
}

// ---------------------------------------------------------------------------
// #9017 — the pool-hold / advisory-hold cause split
// ---------------------------------------------------------------------------

/// The #7708 pool hold and the #5030 pre-flight advisory fold into one bool;
/// `preflight_cause` names which one tripped. A pool hold outranks the
/// advisory (and its recovery probe) even when both are tripped, mirroring
/// `halt_cause::causes_per_root`'s precedence.
#[test]
fn preflight_cause_names_the_pool_hold_apart_from_the_advisory() {
    use crate::work_finder::halt_cause::HaltCause;

    // Healthy everywhere ⇒ not held, no cause.
    assert_eq!(preflight_cause(false, PreflightDispatchGate::Open), (false, None));
    // Pool held ⇒ token_pool regardless of the advisory gate's state.
    for gate in [
        PreflightDispatchGate::Open,
        PreflightDispatchGate::Held,
        PreflightDispatchGate::Probe,
    ] {
        assert_eq!(
            preflight_cause(true, gate),
            (true, Some(HaltCause::TokenPool)),
            "pool hold must outrank {gate:?}"
        );
    }
    // Advisory tripped, pool healthy ⇒ preflight_advisory.
    assert_eq!(
        preflight_cause(false, PreflightDispatchGate::Held),
        (true, Some(HaltCause::PreflightAdvisory))
    );
    // Probe granted, pool healthy ⇒ not held this tick, no cause — the
    // caller decides the probe dispatch, which this root's row must not
    // report as a hold.
    assert_eq!(preflight_cause(false, PreflightDispatchGate::Probe), (false, None));
}

// ---------------------------------------------------------------------------
// #13873: the fallback-aware exhaustion read
// ---------------------------------------------------------------------------

/// Bad-mark every named account for ONE model class (#8058's scoped shape,
/// #13872's live marks): the class is held, every other class stays usable.
/// `classes` as a slice keeps the every-class fixture (#14088 review) on the
/// same on-disk shape: one line per (account, class), exactly what repeated
/// production writes leave behind.
fn class_mark_all(root: &Path, names: &[&str], classes: &[&str]) {
    let pool = root.join(".loom").join("tokens");
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let body: String = names
        .iter()
        .flat_map(|n| {
            let now = now.as_str();
            classes.iter().map(move |c| {
                format!("{now} {n} exhausted: hit your {c} limit [model-class:{c}]\n")
            })
        })
        .collect();
    fs::write(pool.join(".bad_tokens"), body).unwrap();
}

/// #13873: a pool whose accounts are all bad-marked for one model class,
/// with `modelFallback.enabled`, must not hold dispatch — the launches it
/// would spawn fall a rung (`fable → opus`) and select an account the marks
/// leave usable. With the block absent, the same pool holds exactly as
/// before: an absent block reproduces today's behaviour byte-for-byte.
#[test]
fn a_class_marked_pool_holds_only_without_the_fallback_block() {
    let ws = workspace_with_pool(&["a", "b"]);
    class_mark_all(ws.path(), &["a", "b"], &["fable"]);

    // Block absent: the account-wide read treats class-scoped marks as
    // blocking and arms the hold (the 2026-11-13 fleet-idle state).
    let host = PoolHoldState::new();
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));
    assert_eq!(host.active_holds().len(), 1);

    // Enabled: the any-class read sees the opus capacity the marks leave,
    // so the hold never arms — dispatch proceeds and each launch resolves
    // its own rung at spawn time.
    fs::create_dir_all(ws.path().join(".loom")).unwrap();
    fs::write(
        ws.path().join(".loom/config.json"),
        r#"{"autonomous":{"modelFallback":{"enabled":true}}}"#,
    )
    .unwrap();
    let host = PoolHoldState::new();
    assert!(
        !host.observe_root(ws.path(), chrono::Utc::now()),
        "a usable lower rung must not hold dispatch"
    );
    assert!(host.active_holds().is_empty());

    // An account marked for *every* class is the same dead state even
    // though no mark is class-less (#14088 review): the any-class read must
    // count it for no class, so the hold arms exactly as the account-wide
    // read would.
    class_mark_all(ws.path(), &["a", "b"], &["fable", "claude-opus-5", "sonnet", "haiku"]);
    assert!(
        host.observe_root(ws.path(), chrono::Utc::now()),
        "every class marked on every account must arm the hold"
    );

    // ...and a class-LESS mark still holds everything, enabled or not: it
    // blocks every class outright, leaving no rung to fall to.
    bad_mark_all(ws.path(), &["a", "b"]);
    assert!(host.observe_root(ws.path(), chrono::Utc::now()));
}
