//! Root-count-aware budget for the `DaemonStatus` build, and the client-side
//! IPC probe budget that has to wait on it (Issue #8163).
//!
//! # Why this module exists
//!
//! [`crate::ipc::build_daemon_status`] walks **every registered workspace
//! root** on every `status`/`health` round-trip. Its cost is therefore
//! `O(roots)`, but until #8163 every budget compared against it was a
//! *constant*: the `health` CLI bounded its round-trip at a fixed `10s`
//! escalated retry (`cli::health::ESCALATED_IPC_TIMEOUT`), and the slow-build
//! `WARN` named a fixed `5s` client timeout that no longer matched any code.
//!
//! On a host with several dozen registered workspaces that constant is
//! smaller than the build itself — measured `13.1s` / `14.3s` builds against
//! a `10s` client budget — so both the base attempt and the escalated retry
//! timed out, `health` reported `overall: "indeterminate-busy"` (exit `3`) on
//! a demonstrably healthy idle daemon, and every section downstream of
//! liveness came back `unknown`. The client also dropped the connection while
//! the daemon was still serialising, which is the `Broken pipe (os error 32)`
//! the same reports carry.
//!
//! The fix is to state the cost model **once**, here, and have both sides
//! derive from it:
//!
//! - the daemon compares each build against [`status_build_budget`] and names
//!   that budget in its log lines ([`record_status_build`]);
//! - the `health` client sizes its escalated retry with
//!   [`client_probe_budget`], reading the root count from the *local*
//!   workspace registry before the first round-trip (a cheap JSON read — the
//!   client cannot learn the root count from the daemon without completing
//!   the very round-trip it is trying to budget for).
//!
//! # Why the registry read is safe to do client-side
//!
//! [`registered_root_count`] reads the same host-level registry file the
//! daemon's own `effective_roots` walks
//! ([`crate::workspace_registry::default_registry_path`] — `$LOOM_REGISTRY_PATH`
//! or `~/.loom/workspaces.json`), so it is neither cwd-dependent nor a second
//! source of truth. An unreadable/absent registry falls back to `1`, which
//! reproduces the pre-#8163 budget exactly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The documented ceiling of registered workspace roots this budget is sized
/// for (Issue #8163 AC4). Before #8163 no such ceiling existed anywhere in
/// the codebase, so "the status build stays under the probe budget" was not a
/// falsifiable claim; `status_budget::tests` pins it at exactly this many
/// synthetic roots.
///
/// `64` is the next power of two above the "several dozen" the #8163 report
/// measured. It is a **budgeting** ceiling, not an enforced limit: a host may
/// register more, and [`status_build_budget`] keeps growing linearly past it —
/// only [`MAX_ROOT_SCALED_PROBE_TIMEOUT`] caps the client's willingness to
/// wait.
pub const DOCUMENTED_MAX_ROOTS: usize = 64;

/// The root-count-**independent** part of the status-build budget: the
/// registry load, the machine-level dynamic-cap inputs, and the serialisation
/// tail, none of which scale with the number of roots.
pub const STATUS_BUILD_FIXED_BUDGET: Duration = Duration::from_millis(500);

/// The per-registered-root allowance — one pass of `build_daemon_status`'s
/// per-repo loop (registry snapshot, per-root config reads, the
/// `role_shard::decide` walk, the token-pool/ranking `stat`s, the cached
/// stash summary, and the sweep-command `stat`).
///
/// Sized from the #8163 field measurement rather than a local micro-benchmark:
/// `13.1s`/`14.3s` across "several dozen" roots is `~250-350ms` per root on a
/// contended host. `200ms` is deliberately *below* that — the budget is what a
/// healthy host should fit in, and [`PROBE_BUDGET_SAFETY_FACTOR`] is what
/// covers a contended one — while still being ~200x the cost this loop has on
/// an uncontended host, so the budget is not a tripwire for ordinary jitter.
pub const STATUS_BUILD_PER_ROOT_BUDGET: Duration = Duration::from_millis(200);

/// Multiplier from the daemon-side build budget to the client-side probe
/// budget. The client waits for the build **plus** connect, framing,
/// serialisation and its own scheduling delay, all on a host that may be
/// contended — so it must never budget exactly the build's own target.
pub const PROBE_BUDGET_SAFETY_FACTOR: u32 = 2;

/// Absolute ceiling on the root-scaled client probe budget. A corrupted
/// registry (or a genuinely enormous fleet) must never let a one-shot CLI
/// invocation hang for minutes — the same "bounded but generous" tradeoff
/// `cli::status`'s own `MAX_SCALED_STATUS_TIMEOUT` (30s) makes for host-load
/// scaling. Set above that because this budget covers a *known* `O(roots)`
/// cost rather than a speculative load multiplier: at
/// [`DOCUMENTED_MAX_ROOTS`] the scaled budget is already `26.6s`.
pub const MAX_ROOT_SCALED_PROBE_TIMEOUT: Duration = Duration::from_secs(45);

/// Floor under the slow-build `WARN` threshold (the pre-#8163 constant).
///
/// [`record_status_build`] warns at `min(budget, this)`, so a many-root host
/// whose budget is `8s` still gets a breakdown for a `1.2s` build — the
/// instrumentation #7513 added to find the dominant phase keeps working —
/// while the line itself now names the budget, so an operator can tell
/// "slower than usual but well inside budget" from "over budget, this is the
/// thing timing your probes out".
pub const STATUS_BUILD_SLOW_LOG_THRESHOLD: Duration = Duration::from_secs(1);

/// The wall-clock budget one [`crate::ipc::build_daemon_status`] over
/// `root_count` registered roots is expected to fit in.
#[must_use]
pub fn status_build_budget(root_count: usize) -> Duration {
    let roots = u32::try_from(root_count).unwrap_or(u32::MAX);
    STATUS_BUILD_FIXED_BUDGET.saturating_add(STATUS_BUILD_PER_ROOT_BUDGET.saturating_mul(roots))
}

/// The client-side IPC budget that covers [`status_build_budget`] plus
/// transport, capped at [`MAX_ROOT_SCALED_PROBE_TIMEOUT`].
///
/// Callers combine this with their own budget using `max`, never assignment,
/// so a wider operator override (`LOOM_DAEMON_IPC_TIMEOUT_MS`) or a
/// heavily load-scaled base is never *narrowed* by root scaling.
#[must_use]
pub fn client_probe_budget(root_count: usize) -> Duration {
    status_build_budget(root_count)
        .saturating_mul(PROBE_BUDGET_SAFETY_FACTOR)
        .min(MAX_ROOT_SCALED_PROBE_TIMEOUT)
}

/// How many roots [`crate::ipc::build_daemon_status`] would walk right now,
/// read from the host-level workspace registry.
///
/// Mirrors [`crate::workspace_registry::WorkspaceRegistry::effective_roots`]'s
/// own rule: an empty (or unreadable, or absent) registry means the daemon
/// walks exactly one root — its own fallback — so this returns `1` rather
/// than `0`, and the resulting budget is byte-for-byte the pre-#8163 one on a
/// single-workspace host.
#[must_use]
pub fn registered_root_count() -> usize {
    crate::workspace_registry::WorkspaceRegistry::load_default()
        .map(|reg| reg.workspaces.len())
        .unwrap_or(0)
        .max(1)
}

/// Raise `base` to at least [`client_probe_budget`] over `root_count`.
///
/// This is the **raise-only** combinator every client-side caller of
/// `Request::DaemonStatus` applies (Issue #8224 generalised it out of
/// `cli::health::resolve_retry_timeout`, #8163's first caller). `max`, never
/// assignment, is the whole contract: a wider operator override
/// (`LOOM_DAEMON_IPC_TIMEOUT_MS`), a heavily load-scaled base, or a caller's
/// own escalated floor must never be *narrowed* by root scaling, and a
/// single-workspace host must come out bit-for-bit unchanged (at
/// `root_count == 1` the budget is `1.4s`, under every caller's base).
///
/// `root_count` is passed in rather than read here so callers resolve it
/// exactly once via [`registered_root_count`] and can *report* the value they
/// budgeted from — `cli::status` returns it alongside the timeout, and the
/// dashboard's `fetch_report` names it in its timeout error. A hidden read
/// would make the two disagree the moment a workspace is registered mid-call.
#[must_use]
pub fn apply_client_probe_floor(base: Duration, root_count: usize) -> Duration {
    base.max(client_probe_budget(root_count))
}

/// The per-phase wall-clock breakdown #7513 accumulates inside
/// [`crate::ipc::build_daemon_status`], passed here so the logging policy
/// (and the budget it is compared against) lives in one place instead of
/// inline in an over-threshold file.
#[derive(Debug, Default)]
pub struct StatusBuildPhases {
    /// Loading the workspace registry (once, not per root).
    pub registry_load: Duration,
    /// Sum of every per-root phase below — the `O(roots)` part.
    pub per_repo_loop_total: Duration,
    /// Registry snapshot + the live/stale/quarantine derivations from it.
    pub registry_lock: Duration,
    /// Per-root `.loom/config.json` role-runner resolution.
    pub role_runner_config: Duration,
    /// Per-root `role_shard::decide`.
    pub role_shard: Duration,
    /// Per-root token-pool dir + `.ranking` state.
    pub token_pool: Duration,
    /// Per-root quarantine-stash summary (a cache lookup since #7526).
    pub stash_git_shellout: Duration,
    /// Per-root sweep-command `stat`.
    pub sweep_command_check: Duration,
    /// Everything after the loop: machine-level caps, report assembly.
    pub tail: Duration,
    /// The single slowest root and its own loop-body time.
    pub slowest_root: Option<(PathBuf, Duration)>,
}

/// Set once the first [`record_status_build`] call has emitted the
/// startup-cost `INFO` line (Issue #8163 AC3).
static STARTUP_COST_LOGGED: AtomicBool = AtomicBool::new(false);

/// Record one completed [`crate::ipc::build_daemon_status`]: emit the
/// one-time per-root cost line at `INFO` and, when the build is slow, the
/// #7513 phase breakdown at `WARN` — now naming the budget it was compared
/// against (Issue #8163 AC3).
///
/// The `INFO` line fires on the **first** build this process performs rather
/// than at literal process start, because "cost per root" is a measurement:
/// there is nothing to report until a build has happened. It carries the
/// budget model alongside the measurement so an operator reading a single log
/// line can tell whether this host is within the shape the client budgets
/// for, without cross-referencing source.
pub fn record_status_build(total: Duration, root_count: usize, phases: &StatusBuildPhases) {
    let budget = status_build_budget(root_count);
    if !STARTUP_COST_LOGGED.swap(true, Ordering::Relaxed) {
        let per_root = total
            .checked_div(u32::try_from(root_count).unwrap_or(u32::MAX).max(1))
            .unwrap_or_default();
        log::info!(
            "build_daemon_status: first build took {total:?} across {root_count} root(s) = \
             {per_root:?}/root; budget for this host is {budget:?} \
             ({STATUS_BUILD_FIXED_BUDGET:?} fixed + {STATUS_BUILD_PER_ROOT_BUDGET:?}/root), and \
             the health/status client sizes its escalated IPC retry at \
             {probe:?}. Documented ceiling is {DOCUMENTED_MAX_ROOTS} roots \
             ({ceiling:?} build budget, {ceiling_probe:?} probe budget).",
            probe = client_probe_budget(root_count),
            ceiling = status_build_budget(DOCUMENTED_MAX_ROOTS),
            ceiling_probe = client_probe_budget(DOCUMENTED_MAX_ROOTS),
        );
    }
    if total < budget.min(STATUS_BUILD_SLOW_LOG_THRESHOLD) {
        return;
    }
    let verdict = if total >= budget {
        "OVER BUDGET"
    } else {
        "within budget"
    };
    let slowest_root_desc = phases
        .slowest_root
        .as_ref()
        .map_or_else(|| "n/a".to_string(), |(root, d)| format!("{} ({d:?})", root.display()));
    log::warn!(
        "build_daemon_status: slow build took {total:?} across {root_count} roots — {verdict} \
         (budget {budget:?} = {STATUS_BUILD_FIXED_BUDGET:?} + \
         {STATUS_BUILD_PER_ROOT_BUDGET:?}/root; the health client's escalated IPC retry is \
         sized at {probe:?} from the same model) — phase breakdown: \
         registry_load={registry_load:?}, per_repo_loop_total={loop_total:?} \
         [registry_lock/list={registry_lock:?}, role_runner_config={role_runner_config:?}, \
         role_shard={role_shard:?}, token_pool={token_pool:?}, \
         stash_git_shellout={stash:?}, sweep_command_check={sweep_cmd:?}], tail={tail:?}; \
         slowest single root: {slowest_root_desc}",
        probe = client_probe_budget(root_count),
        registry_load = phases.registry_load,
        loop_total = phases.per_repo_loop_total,
        registry_lock = phases.registry_lock,
        role_runner_config = phases.role_runner_config,
        role_shard = phases.role_shard,
        token_pool = phases.token_pool,
        stash = phases.stash_git_shellout,
        sweep_cmd = phases.sweep_command_check,
        tail = phases.tail,
    );
}

#[cfg(test)]
mod tests;
