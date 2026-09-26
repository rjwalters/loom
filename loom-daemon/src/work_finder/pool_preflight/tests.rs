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

// ---------------------------------------------------------------------------
// Fleet broadcast of the hold (Issue #8001)
// ---------------------------------------------------------------------------
//
// The shape these mirror is #7477's: one process models N hosts, each host
// being one `PoolHoldState` + one `SweepRegistry` + one `PeerClaimView`. The
// room itself is modelled by hand-delivering the ad the publishing host put
// on its outbound channel into the receiving host's view — the same thing
// `safehouse::PeerClaimSink::on_event` does in production, minus the socket.

use std::sync::{Arc, Mutex};
use std::time::{Duration as StdDuration, Instant};

use crate::peer_claims::{ClaimAd, ClaimKind, PeerClaimView};
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
use crate::tokens_pool::paths::resolve_tokens_dir;
use crate::tokens_pool::select::pool_account_fingerprint;

/// One simulated fleet host: its own hold set, its own registry (with an
/// outbound peer-claim channel), and its own inbound peer-claim view.
struct FleetHost {
    holds: PoolHoldState,
    registry: SweepRegistry,
    view: Arc<Mutex<PeerClaimView>>,
    outbound: tokio::sync::mpsc::Receiver<ClaimAd>,
}

impl FleetHost {
    fn new(name: &str, root: &Path) -> Self {
        let mut registry = SweepRegistry::new(SweepRegistryConfig::new(root.to_path_buf()));
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        registry.set_peer_claim_publisher(tx);
        let view =
            Arc::new(Mutex::new(PeerClaimView::new(name.to_owned(), StdDuration::from_secs(120))));
        registry.set_peer_claims(Arc::clone(&view));
        Self {
            holds: PoolHoldState::new(),
            registry,
            view,
            outbound: rx,
        }
    }

    /// This host's full per-root verdict: its own pre-flight read, its own
    /// arm/clear broadcast, and any peer-reported hold folded in — exactly
    /// what `preflight_held_per_root` computes per root in production.
    fn preflight(&self, root: &Path, now: chrono::DateTime<chrono::Utc>) -> bool {
        let observation = self.holds.observe_root_edge(root, now);
        fold_peer_pool_hold(&self.holds, &self.registry, &observation)
    }

    /// Drain everything this host published and deliver it to `peer`'s view —
    /// the room, modelled.
    fn deliver_to(&mut self, peer: &FleetHost) {
        while let Ok(ad) = self.outbound.try_recv() {
            if ad.kind.is_pool_hold_lane() {
                let mut v = peer.view.lock().unwrap();
                v.observe_pool_hold_at(&ad, Instant::now());
            }
        }
    }

    /// Drop everything this host published without delivering it — a
    /// safehoused outage / dropped ad.
    fn drop_outbound(&mut self) {
        while self.outbound.try_recv().is_ok() {}
    }
}

fn fingerprint_of(root: &Path) -> String {
    pool_account_fingerprint(&resolve_tokens_dir(root)).expect("fixture pool has accounts")
}

/// **The #8001 acceptance criterion.** Host A discovers the pool is dead and
/// broadcasts. Host B — whose own `spawnable_pool_state` read still reports a
/// perfectly healthy pool, so its own pre-flight would NOT hold — holds
/// dispatch anyway, purely on the strength of the received ad.
#[test]
fn a_peer_holds_on_the_broadcast_without_rederiving_exhaustion_itself() {
    let now = chrono::Utc::now();
    // Host A's workspace: dead pool. Host B's: the SAME accounts, but its own
    // view of them is healthy (no `.bad_tokens`) — B's own read cannot catch
    // this, which is the whole point.
    let ws_a = workspace_with_pool(&["a", "b", "c"]);
    bad_mark_all(ws_a.path(), &["a", "b", "c"]);
    let ws_b = workspace_with_pool(&["a", "b", "c"]);

    assert_eq!(
        fingerprint_of(ws_a.path()),
        fingerprint_of(ws_b.path()),
        "the same account set must fingerprint identically across hosts"
    );

    let mut host_a = FleetHost::new("host-a", ws_a.path());
    let host_b = FleetHost::new("host-b", ws_b.path());

    assert!(host_a.preflight(ws_a.path(), now), "A's own read must hold");
    // Before the ad arrives B is free — this is the baseline the broadcast
    // has to change, and it proves B's OWN read says "healthy".
    assert!(
        !host_b.preflight(ws_b.path(), now),
        "B's own pre-flight read must report the pool healthy"
    );
    assert_eq!(
        host_b.holds.held_pool_count(),
        0,
        "B must hold nothing of its own — any hold it takes is the peer's"
    );

    host_a.deliver_to(&host_b);

    assert!(
        host_b.preflight(ws_b.path(), now),
        "B must hold on A's broadcast despite its own read saying healthy"
    );
    assert_eq!(
        host_b.holds.held_pool_count(),
        0,
        "the peer-sourced hold must not be forged into B's own local hold set"
    );
}

/// **The correctness risk the module doc names.** A peer resolving a
/// genuinely DIFFERENT (healthy) repo-local shadow pool must not be
/// suppressed by another host's hold — even though both hosts hold their
/// pools at the identical relative path `<root>/.loom/tokens`, which is
/// exactly why the broadcast is keyed by account set and not by directory.
#[test]
fn a_peer_with_a_different_healthy_shadow_pool_is_not_suppressed() {
    let now = chrono::Utc::now();
    let ws_a = workspace_with_pool(&["a", "b", "c"]);
    bad_mark_all(ws_a.path(), &["a", "b", "c"]);
    // B's repo-local shadow pool holds a DIFFERENT account set (#7527/#3938).
    let ws_b = workspace_with_pool(&["x", "y"]);

    assert_ne!(
        fingerprint_of(ws_a.path()),
        fingerprint_of(ws_b.path()),
        "different account sets must fingerprint differently"
    );
    assert_eq!(
        ws_a.path().join(".loom").join("tokens").file_name(),
        ws_b.path().join(".loom").join("tokens").file_name(),
        "both pools sit at the same relative path — a path key would collide"
    );

    let mut host_a = FleetHost::new("host-a", ws_a.path());
    let host_b = FleetHost::new("host-b", ws_b.path());

    assert!(host_a.preflight(ws_a.path(), now));
    host_a.deliver_to(&host_b);

    assert!(
        !host_b.preflight(ws_b.path(), now),
        "a host whose own pool is healthy and DIFFERENT must never be \
         suppressed by a peer's hold"
    );
}

/// Recovery propagates early: when A's pool recovers it broadcasts a clear,
/// and B resumes immediately rather than sitting out the remainder of the
/// advertised TTL.
#[test]
fn a_recovery_broadcast_releases_the_peer_early() {
    let now = chrono::Utc::now();
    let ws_a = workspace_with_pool(&["a", "b"]);
    bad_mark_all(ws_a.path(), &["a", "b"]);
    let ws_b = workspace_with_pool(&["a", "b"]);

    let mut host_a = FleetHost::new("host-a", ws_a.path());
    let host_b = FleetHost::new("host-b", ws_b.path());

    assert!(host_a.preflight(ws_a.path(), now));
    host_a.deliver_to(&host_b);
    assert!(host_b.preflight(ws_b.path(), now), "B holds on the arm ad");

    readmit_all(ws_a.path());
    assert!(!host_a.preflight(ws_a.path(), now), "A clears on its very next tick");
    host_a.deliver_to(&host_b);

    assert!(
        !host_b.preflight(ws_b.path(), now),
        "B must resume on A's clear ad, not wait out the advertised TTL"
    );
}

/// A second host holding the same pool keeps the hold alive after the first
/// one clears: a `PoolHoldCleared` speaks only for its own sender.
#[test]
fn one_peers_recovery_does_not_release_another_peers_hold() {
    let now = chrono::Utc::now();
    let pool_key = {
        let ws = workspace_with_pool(&["a", "b"]);
        fingerprint_of(ws.path())
    };
    let ws_c = workspace_with_pool(&["a", "b"]);
    let host_c = FleetHost::new("host-c", ws_c.path());

    {
        let mut v = host_c.view.lock().unwrap();
        for peer in ["host-a", "host-b"] {
            v.observe_pool_hold_at(
                &ClaimAd::pool_hold_armed(
                    "repo".into(),
                    peer.into(),
                    1,
                    "ts".into(),
                    pool_key.clone(),
                    900,
                ),
                Instant::now(),
            );
        }
        // Only host-a recovers.
        v.observe_pool_hold_at(
            &ClaimAd::pool_hold_cleared(
                "repo".into(),
                "host-a".into(),
                1,
                "ts".into(),
                pool_key.clone(),
            ),
            Instant::now(),
        );
    }

    assert!(
        host_c.preflight(ws_c.path(), now),
        "host-b still holds the pool — one peer's recovery must not speak for another's"
    );
}

/// Fail-open: a dropped ad (safehoused outage, saturated channel) leaves the
/// peer exactly where it was before #8001 — free to dispatch, and still
/// protected by its own pre-flight on its own next tick once its own view of
/// the pool catches up.
#[test]
fn a_dropped_ad_degrades_to_the_local_only_preflight() {
    let now = chrono::Utc::now();
    let ws_a = workspace_with_pool(&["a", "b"]);
    bad_mark_all(ws_a.path(), &["a", "b"]);
    let ws_b = workspace_with_pool(&["a", "b"]);

    let mut host_a = FleetHost::new("host-a", ws_a.path());
    let host_b = FleetHost::new("host-b", ws_b.path());

    assert!(host_a.preflight(ws_a.path(), now));
    host_a.drop_outbound(); // the ad never reaches the room

    assert!(!host_b.preflight(ws_b.path(), now), "a dropped ad must not hold B — fail-open");

    // B's own read catching up still holds it, unchanged from pre-#8001.
    bad_mark_all(ws_b.path(), &["a", "b"]);
    assert!(host_b.preflight(ws_b.path(), now));
}

/// The arm edge is published exactly once per outage, not once per tick —
/// what keeps a multi-hour outage from flooding the shared room.
#[test]
fn the_arm_edge_is_broadcast_once_per_outage() {
    let now = chrono::Utc::now();
    let ws = workspace_with_pool(&["a"]);
    bad_mark_all(ws.path(), &["a"]);
    let mut host = FleetHost::new("host-a", ws.path());

    for _ in 0..5 {
        assert!(host.preflight(ws.path(), now));
    }

    let mut ads = Vec::new();
    while let Ok(ad) = host.outbound.try_recv() {
        ads.push(ad);
    }
    assert_eq!(ads.len(), 1, "five held ticks must publish one arm ad");
    assert_eq!(ads[0].kind, ClaimKind::PoolHoldArmed);
    assert_eq!(ads[0].pool_key.as_deref(), Some(fingerprint_of(ws.path()).as_str()));
    assert!(ads[0].remaining_secs.is_some_and(|s| s > 0 && s <= 900));

    readmit_all(ws.path());
    for _ in 0..3 {
        assert!(!host.preflight(ws.path(), now));
    }
    let mut ads = Vec::new();
    while let Ok(ad) = host.outbound.try_recv() {
        ads.push(ad);
    }
    assert_eq!(ads.len(), 1, "three recovered ticks must publish one clear ad");
    assert_eq!(ads[0].kind, ClaimKind::PoolHoldCleared);
}

/// A wrapper-confirmed death (the post-mortem path the reaper drives) is the
/// strongest evidence available, so it broadcasts too — including when it
/// merely *upgrades* an existing pre-flight hold.
#[test]
fn a_post_mortem_hold_broadcasts_its_arming_edge() {
    let now = chrono::Utc::now();
    let ws = workspace_with_pool(&["a"]);
    let host = PoolHoldState::new();

    // Healthy live read, wrapper says otherwise: arms, and must advertise.
    let observation = host.note_pool_dead(ws.path(), now);
    assert!(observation.held);
    assert_eq!(observation.pool_key.as_deref(), Some(fingerprint_of(ws.path()).as_str()));
    assert!(matches!(observation.edge, Some(PoolHoldEdge::Armed { .. })));

    // Re-confirming an already-post-mortem hold is not a fresh edge.
    let again = host.note_pool_dead(ws.path(), now);
    assert!(again.edge.is_none(), "a repeat post-mortem must not re-broadcast");
}

/// An upgrade from a pre-flight hold to a wrapper-confirmed one IS a fresh
/// edge: a peer that missed (or has since expired) the original arm ad gets a
/// new window backed by the strongest evidence this daemon ever gets.
#[test]
fn a_preflight_hold_upgraded_by_the_wrapper_rebroadcasts() {
    let now = chrono::Utc::now();
    let ws = workspace_with_pool(&["a"]);
    bad_mark_all(ws.path(), &["a"]);
    let host = PoolHoldState::new();

    assert!(host.observe_root(ws.path(), now));
    let upgraded = host.note_pool_dead(ws.path(), now);
    assert!(
        matches!(upgraded.edge, Some(PoolHoldEdge::Armed { .. })),
        "false -> true on wrapper_observed is a fresh, stronger edge"
    );
}

/// An empty pool (#4642's ABSENT-pool condition) has no identity, so it is
/// never advertised and no peer hold can be about it.
#[test]
fn an_empty_pool_has_no_broadcast_identity() {
    let now = chrono::Utc::now();
    let ws = tempfile::tempdir().unwrap();
    fs::create_dir_all(ws.path().join(".loom").join("tokens")).unwrap();

    let host = PoolHoldState::new();
    let observation = host.observe_root_edge(ws.path(), now);
    assert!(!observation.held);
    assert!(observation.pool_key.is_none());
    assert!(observation.edge.is_none());
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
