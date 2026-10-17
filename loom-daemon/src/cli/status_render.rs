//! Status-payload rendering shared by `loom-daemon status` and `fleet
//! status` (Issue #4712 — split out of `main.rs`): capacity resolution,
//! main-health-gate verdict classification, and the JSON/human table
//! builders. `cli::status` owns the IPC round-trip and the two command
//! entry points that call into this module.

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::Path;

mod capacity_line;
mod drain_render;
mod fleet_store_line;
mod forge_calls_render;
mod forge_events_line;
mod holds;
mod host_update_line;
mod last_tick_line;
mod model_class;
mod observability_line;
mod otlp_export_line;
mod peer_claims_line;
mod pending_restart_line;
mod session_containers_line;
mod task_liveness_line;
mod telemetry_banner;

use loom_daemon::daemon_install_state;
use loom_daemon::self_update;
use loom_daemon::status_section::{SectionSet, StatusSection as Section};
use loom_daemon::types::{DaemonStatusReport, SweepKind};
use loom_daemon::worktree_disk_status::{self, WorktreeDiskSummary};

/// The capacity figures the whole status view shares, resolved from a single
/// source so the summary, the healthy-tokens cap input, and the per-token table
/// never contradict each other (issue #3936).
///
/// Preference order:
/// 1. **fresh probe** — when a client-side `loom-daemon tokens check --json` succeeded
///    (the *same* data that renders the per-token table), the health counts are
///    derived from it via [`loom_daemon::capacity::summarize_probe`], applying
///    the near-ceiling threshold uniformly. This is the accurate *current*
///    capacity and matches the table row-for-row.
/// 2. **daemon ranking** — when no fresh probe is available but the daemon
///    reported a parsed `.loom/tokens/.ranking`, fall back to its snapshot.
/// 3. **raw pool** — no probe and no ranking: the pre-#3902 flat pool basis.
struct ResolvedCapacity {
    /// Where the figures came from — one of `"probe"`, `"ranking"`, `"pool"`.
    source: &'static str,
    /// Whether any account-health data (probe or ranking) was available.
    ranking_present: bool,
    total: usize,
    healthy: usize,
    exhausted: usize,
    /// Health-adjusted token axis (healthy accounts, or the raw pool as a
    /// fallback) — an *informational* account-health figure (drives spawn-time
    /// selection), not a cap input since #5270.
    token_axis_limit: usize,
    /// The effective dynamic cap. Always exactly `report.dynamic_cap` (#5270)
    /// — the token axis was removed from the admission formula entirely, so
    /// there is nothing left for a client-side probe to recompute here; using
    /// the daemon's own authoritative figure guarantees this can never drift
    /// from what the daemon actually admits against (the configured
    /// `maxConcurrent` since #12406),
    /// unlike the pre-#5270 client-side `token_axis_effective.min(...)`
    /// recomputation this superseded (which could disagree with the daemon
    /// when accounts were scarce).
    effective_cap: usize,
    /// Whether the token pool is genuinely starved (zero healthy accounts).
    /// Since #5270 this is NOT "tokens are the binding cap term" — the token
    /// axis was removed from `dynamic_cap` entirely — but zero healthy
    /// accounts still means every spawn will fail account selection, so
    /// #5305 restored this as a reachable starvation signal rather than the
    /// permanently-`false` placeholder #5304 left behind. Mirrors
    /// `report.capacity.token_bound`.
    token_bound: bool,
}

/// Resolve the shared capacity figures for a status render (#3936). Prefers the
/// fresh client-side probe over the daemon's possibly-stale ranking snapshot so
/// the summary count, the cap's healthy-tokens input, and the per-token table
/// all agree.
fn resolve_capacity(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
) -> ResolvedCapacity {
    // Tier 1: fresh probe — the same source as the per-token table.
    if let Some(usage) = token_usage {
        if let Some(accounts) = usage.get("accounts").and_then(serde_json::Value::as_array) {
            let pairs: Vec<(&str, Option<f64>)> = accounts
                .iter()
                .map(|a| {
                    let status = a
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let util_7d = a.get("7d_utilization").and_then(serde_json::Value::as_f64);
                    (status, util_7d)
                })
                .collect();
            let cap = loom_daemon::capacity::summarize_probe(pairs.iter().copied());
            let token_axis_limit = cap.healthy;
            // #5270: the token axis no longer participates in the cap on any
            // auth path — a fresh client-side probe changes the *reported*
            // account-health figures (total/healthy/exhausted/token_axis_limit
            // above), but must NOT re-derive its own `effective_cap` from them
            // the way the pre-#5270 formula did (that recomputation could
            // disagree with the daemon whenever accounts were scarce, which is
            // exactly the artificial cap this issue removes). The daemon's own
            // `dynamic_cap` (the configured `maxConcurrent` since #12406) is
            // the sole authority for the cap. `token_bound`, however, is a starvation
            // signal (zero healthy accounts), independent of the cap — #5305
            // restores it here too so a fresh probe reporting 0 healthy
            // accounts still surfaces the guidance.
            return ResolvedCapacity {
                source: "probe",
                ranking_present: true,
                total: cap.total,
                healthy: cap.healthy,
                exhausted: cap.exhausted,
                token_axis_limit,
                effective_cap: report.dynamic_cap,
                token_bound: cap.healthy == 0,
            };
        }
    }

    // Tier 2: daemon-reported ranking snapshot.
    if report.capacity.ranking_present {
        return ResolvedCapacity {
            source: "ranking",
            ranking_present: true,
            total: report.capacity.total_accounts,
            healthy: report.capacity.healthy_accounts,
            exhausted: report.capacity.exhausted_accounts,
            token_axis_limit: report.capacity.token_axis_limit,
            effective_cap: report.dynamic_cap,
            token_bound: report.capacity.token_bound,
        };
    }

    // Tier 3: no probe, no ranking — raw pool basis (pre-#3902 behavior).
    ResolvedCapacity {
        source: "pool",
        ranking_present: false,
        total: report.token_pool_size,
        healthy: 0,
        exhausted: 0,
        token_axis_limit: report.token_pool_size,
        effective_cap: report.dynamic_cap,
        token_bound: report.capacity.token_bound,
    }
}

/// Whether the daemon's own `.ranking` read is unambiguously starved (0
/// healthy accounts) while a fresher read (a client-side probe, or a
/// re-checked ranking `rc` resolved to) shows real capacity — #4344,
/// re-scoped by #5305 for #5270: the concurrency cap is no longer affected
/// either way, but the daemon's own spawn-time account **selection** is still
/// reading a stale, fully-exhausted `.ranking` file. Pure predicate, kept
/// separate from the println! block above it so it stays trivially
/// unit-testable (mirrors `ipc::dispatch_would_meet_or_exceed_headroom`'s
/// rationale).
#[must_use]
fn ranking_diverges_from_starvation(report: &DaemonStatusReport, rc: &ResolvedCapacity) -> bool {
    report.capacity.ranking_present
        && report.capacity.healthy_accounts == 0
        && rc.ranking_present
        && rc.healthy > 0
}

/// Marker-vs-non-autonomous-daemon mismatch (#4693, hardened by #5409's
/// AC2/AC3): `true` only when the autonomy-desired marker is present AND
/// this reachable daemon's own `work_finder_enabled` reads `Some(false)` — a
/// healthy, "protected" (crash-detectable) daemon that has nonetheless
/// silently stopped dispatching. `false` when work-finder IS on, when
/// `work_finder_enabled` is `null` (pre-#4693 daemon binary — never a false
/// positive), or when `protection` itself is `None` (no loom dir resolved).
///
/// Shared by construction — never independently recomputed — between the
/// `--json` payload's `protection.autonomy_mismatch` field
/// ([`build_status_json_value`]), the human-readable `WARNING:` block
/// ([`print_status_human`]), and `handle_status_command`'s exit-code decision
/// (#5409 AC2, `daemon_install_state::EXIT_AUTONOMY_MISMATCH`) so all three
/// can never disagree about what counts as a mismatch.
#[must_use]
pub(crate) fn autonomy_mismatch(
    protection: Option<&daemon_install_state::ProtectionReport>,
    report: &DaemonStatusReport,
) -> bool {
    protection.is_some_and(|p| p.marker_present) && report.work_finder_enabled == Some(false)
}

/// Build the combined status payload (daemon report + per-token usage) as a
/// [`serde_json::Value`] — the shared value builder behind both `loom-daemon
/// status --json` ([`print_status_json`]) and each fleet host's own
/// self-reported status, including the local host's row collected in-process
/// by `fleet status` (#4342, [`collect_local_fleet_report`]) — keeping the two
/// call sites' JSON shape identical by construction rather than by
/// convention.
/// The `role_agents` block of the `--json` status payload (#6102).
///
/// `max_concurrent` is `null` only for a pre-#6102 daemon that never sent one
/// — a consumer must read that as "unknown", not "unbounded".
/// `total_with_sweeps` is precomputed because the whole point of the block is
/// that this host's agent load was previously only obtainable by adding a
/// status field to a `pgrep` count.
fn role_agents_json(report: &DaemonStatusReport) -> serde_json::Value {
    serde_json::json!({
        "active": report.active_role_agents,
        "max_concurrent": report.role_agent_max_concurrent,
        "total_with_sweeps": report.in_flight.len() + report.active_role_agents,
        "bounded_by_work_finder_max_concurrent": false,
    })
}

pub(crate) fn build_status_json_value(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
    protection: Option<&daemon_install_state::ProtectionReport>,
    worktree_disk: Option<&[WorktreeDiskSummary]>,
) -> serde_json::Value {
    let all = SectionSet::all();
    build_status_json_value_for(
        report,
        token_usage,
        update,
        pipeline,
        protection,
        worktree_disk,
        &all,
    )
}

/// [`build_status_json_value`] keeping only the top-level keys of `sections`
/// (`status --json --section`, #10787). The host-local blocks after the
/// literal are not even probed unless selected; the full set keeps every key,
/// so the default payload is exactly [`build_status_json_value`]'s.
pub(crate) fn build_status_json_value_for(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
    protection: Option<&daemon_install_state::ProtectionReport>,
    worktree_disk: Option<&[WorktreeDiskSummary]>,
    sections: &SectionSet,
) -> serde_json::Value {
    let rc = resolve_capacity(report, token_usage);
    let mut value = serde_json::json!({
        "in_flight_count": report.in_flight.len(),
        "in_flight": report.in_flight,
        // Live-locked-but-unregistered sweeps (#4214): a live `owner_pid` lock
        // with no matching `in_flight` entry. Non-empty here means a sweep is
        // demonstrably alive (the lock proves it) but the in-memory registry
        // union above has lost track of it — read these as **alive**, not
        // dead, and reconcile rather than re-dispatching. Empty in the
        // overwhelmingly common case.
        "unregistered_locked_count": report.unregistered_locked.len(),
        "unregistered_locked": report.unregistered_locked.iter().map(|u| serde_json::json!({
            "root": u.root,
            "issue": u.issue,
            "owner_pid": u.owner_pid,
        })).collect::<Vec<_>>(),
        // Stale-untracked-sweep backstop (Issue #7529): in-flight sweeps
        // unreachable by either the startup-hang (#3887) or review-stall
        // (#3910) watchdog — see `DaemonStatusReport::stale_sweeps`'s doc.
        // Non-empty here is a hard finding: it means a sweep has been alive
        // and log-silent for far longer than any watchdog would tolerate.
        "stale_sweeps_count": report.stale_sweeps.len(),
        "stale_sweeps": report.stale_sweeps,
        // Backed-off worktree removals (Issue #7590): a removal that has
        // failed with a permission-class cause (or `REMOVAL_FAILURE_CAP`
        // consecutive times of any cause) and so is no longer retried every
        // reap tick. Non-empty here means a worktree cannot self-resolve
        // without operator intervention (typically a manual `sudo rm -rf`).
        "stuck_worktree_reclaims_count": report.stuck_worktree_reclaims.len(),
        "stuck_worktree_reclaims": report.stuck_worktree_reclaims,
        // Token pools holding ALL sweep dispatch because not one account in
        // them can spawn (#7708, surfaced by #7990). Non-empty here is the
        // machine-readable form of the "pool exhausted hold until T" lines
        // the human renderer prints; `loom-daemon health --json` carries the
        // same facts in its own `pool_hold` section.
        "pool_exhaustion_holds": report.pool_exhaustion_holds,
        // "Currently binding" vs "smallest ceiling" (#4031): the cap only binds
        // once in-flight reaches it. `false` ⇒ the limiter is work availability,
        // not any resource term, so scripted consumers don't misread the
        // token/CPU ceiling as a bottleneck at low occupancy.
        "capacity_bound": report.capacity_bound,
        // Role-runner agent load + its own ceiling (#6102). Reported next to
        // the sweep counts above because `capacity_bound` / `dynamic_cap` /
        // `configured_max` all describe SWEEP dispatch only: role agents are
        // spawned by the role runner's own interval/idle loops and never pass
        // through work-finder admission. A scripted consumer sizing a host
        // wants the sum, and before #6102 could only get the second term from
        // `pgrep`. Grouped into one object (rather than three sibling keys)
        // because this file's top-level `json!` literal is already at the
        // macro recursion limit.
        "role_agents": role_agents_json(report),
        // Claude-wrapper pre-flight-death workspace tripwire (#4386): `true`
        // means N consecutive dispatches, across different issues, died at
        // the wrapper's MCP-init pre-flight check before ever reaching
        // `# CLAUDE_CLI_START` — the classic stale-`.mcp.json` fleet-wide
        // silent-failure signature. `message` is `null` when not tripped.
        "preflight_advisory_active": report.preflight_advisory_active,
        "preflight_advisory_message": report.preflight_advisory_message,
        // Freshness signal for the advisory above (Issue #5029): the wall-clock
        // time of the most recent trip/clear transition, so a scripted
        // consumer (like the human-readable renderer below) can distinguish a
        // just-tripped warning from a stale one that predates a since-applied
        // fix. `null` before the first transition this daemon process has
        // observed.
        "preflight_advisory_changed_at": report.preflight_advisory_changed_at,
        // Observability host-identity mismatch (#4830): non-null means this
        // host's ingest key is bound to a DIFFERENT host_id than the daemon
        // reports for itself, so its telemetry is being filed under the wrong
        // host. `null` in the common case (ids agree, or no exporter running).
        "observability_host_id_mismatch": report.observability_host_id_mismatch,
        // Positive export-liveness signal (#5083). Unlike the anomaly-only
        // field above, this is non-null for any daemon of this vintage and
        // always carries a `state`, so a watch loop can assert health rather
        // than infer it from the absence of a warning:
        //   loom-daemon status --json \
        //     | jq -e '.observability_export.state == "healthy"'
        // States: disabled | starting | never_exported | healthy |
        // host_id_mismatch | failing. `null` only from a pre-#5083 daemon.
        "observability_export": report.observability_export,
        // Per-exporter cells (#8756): one entry per configured exporter,
        // keyed by exporter name — each sink's queue, counters and liveness
        // surfaced independently:
        //   loom-daemon status --json \
        //     | jq -e '.observability_exports.otlp.state == "healthy"'
        // `{}` when observability is off or from a pre-#8756 daemon (the
        // singular field above already distinguishes those states).
        "observability_exports": report.observability_exports,
        // OTLP-export health (#11353): `no_exporter` | `failing` | `ok` |
        // `exempt`, with an optional `detail` (the exemption reason).
        //   loom-daemon status --json | jq -e '.otlp_export.state == "ok"'
        "otlp_export": report.otlp_export,
        // Forge event-feed consumer state (ADR-0021, #8765). Non-null for any
        // daemon of this vintage and always carrying a `state`, so a watch
        // loop asserts rather than infers:
        //   loom-daemon status --json | jq -e '.forge_events.state == "healthy"'
        // States: disabled | misconfigured | connecting | failing |
        // auth_failed | host_mismatch | backoff | healthy. `null` only from a
        // pre-ADR-0021 daemon.
        "forge_events": report.forge_events,
        // Per-repo pressure-triggered deep-clean state (#5919): when the pass
        // last fired, what it reclaimed, and — for the common non-firing tick
        // — why it declined. Scripted consumers can assert reclamation is
        // alive without grepping the daemon log:
        //   loom-daemon status --json | jq -e '.deep_clean | length > 0'
        "deep_clean": report.deep_clean,
        "dynamic_cap": {
            "token_pool_size": report.token_pool_size,
            // The directory the daemon resolved for the pool above (#4292) —
            // `null` only from a pre-#4292 daemon binary that never computed
            // one. Lets an operator confirm at a glance which of the
            // per-repo/shared pools is actually in effect, independent of
            // whatever cwd `loom-daemon status` itself was run from.
            "token_pool_dir": report.token_pool_dir,
            // Host CPU OBSERVATIONS (#3978/#4031), not cap terms: the cap is
            // `configured_max` alone (#12406). Reported because observed idle
            // and load are the evidence for tuning it on this machine.
            "logical_cpus": report.logical_cpus,
            "loadavg_1m": report.loadavg_1m,
            "cpu_idle_fraction": report.cpu_idle_fraction,
            "configured_max": report.configured_max,
            "effective": rc.effective_cap,
        },
        "capacity": {
            "source": rc.source,
            "ranking_present": rc.ranking_present,
            "total_accounts": rc.total,
            "healthy_accounts": rc.healthy,
            // `class -> healthy` (#8058 Phase 3). `{}` on a pool with no
            // class-scoped `.bad_tokens` state, so every field above keeps
            // its exact pre-#8058 meaning and a consumer can read this one
            // unconditionally.
            "healthy_accounts_by_class": model_class::capacity_detail(report),
            // claude-monitor's predictive `class -> utilization` (issue
            // #8297); `{}` when `healthy_accounts_by_class` above is also
            // `{}` (same gate) or no fresh monitor sidecar exists. Never
            // consulted by the selector, never changes the counts above.
            "monitor_utilization_by_class": model_class::monitor_utilization_detail(report),
            "exhausted_accounts": rc.exhausted,
            "token_axis_limit": rc.token_axis_limit,
            "token_bound": rc.token_bound,
        },
        "main_health_gate": {
            "halted": report.main_health_gate_halted,
            // "Not evaluated" is distinct from "halted" (verified-red main) —
            // #3950 AC3. Both can be true at once: a prior halt from a
            // genuinely red run persists untouched while a later tick can't
            // even evaluate (dirty tree, timeout, missing tool, broken `git`).
            "not_evaluated": report.main_health_gate_not_evaluated,
            // Which failure class actually blocked evaluation (#3974 AC2).
            "not_evaluated_reason": report.main_health_gate_not_evaluated_reason,
            // Whether the gate is actually enabled for this root, and when its
            // last completed verdict landed (#4012) — the disambiguators
            // between "disabled", "pending" (enabled, no verdict yet), and
            // "clear" (verified green), all three of which pre-#4012 rendered
            // identically as `halted: false, not_evaluated: false`.
            "enabled": report.main_health_gate_enabled,
            "verdict_at": report.main_health_gate_verdict_at,
            // Load-aware deferral + tier label (#4259). `deferred` is a bounded
            // scheduling decision distinct from both `halted` and
            // `not_evaluated`; `verdict_tier` ("full"/"fast") keeps a fast-tier
            // green distinguishable from a full-suite green.
            "deferred": report.main_health_gate_deferred,
            "deferred_reason": report.main_health_gate_deferred_reason,
            "verdict_tier": report.main_health_gate_verdict_tier,
        },
        // Whether the autonomous work-finder loop is enabled for THIS running
        // daemon process (#4693), resolved daemon-side (env > config > default,
        // the same precedence `loom-daemon-start.sh` bakes into the plist/unit).
        // `null` only from a pre-#4693 daemon binary that never computed one —
        // never misread as `false`. See `protection.autonomy_mismatch` below
        // for the marker-vs-non-autonomous-daemon cross-check this feeds.
        "work_finder": {
            "enabled": report.work_finder_enabled,
        },
        // Host-wide `LOOM_ROLE_RUNNER` env override state (#6470), resolved
        // once for the whole report — `null` when unset (each root's own
        // config decides independently).
        "role_runner_host_env_override": report.role_runner_host_env_override,
        // Host-level role-runner sharding posture (#6374) — the fine half of
        // the same question `role_runner_host_env_override` answers coarsely:
        // the env override says *whether* this host runs role ticks at all,
        // the shard posture says *which workspaces'*. `null` from a pre-#6374
        // daemon binary, which a consumer must NOT read as "unsharded" — an
        // older daemon does not shard, but that is not something this payload
        // asserts.
        "role_runner_shard": report.role_runner_shard.as_ref().map(|s| serde_json::json!({
            "index": s.index,
            "count": s.count,
            "configured": s.configured,
            "summary": s.summary,
            // Roster section (Issue #7690 / #7691, #6704) — `null` when the
            // roster is disabled (the default) or has never completed a
            // heartbeat cycle. With `roster.enabled` it is also the source of
            // `index`/`count` above (`summary` says so), and `fence` names
            // the admission verdict.
            "roster": s.roster.as_ref().map(|r| serde_json::json!({
                "issue": r.issue,
                "live_count": r.live_count,
                "seen_count": r.seen_count,
                "generation": r.generation,
                "settled_secs": r.settled_secs,
                "fence": r.fence,
                "members": r.members.iter().map(|m| serde_json::json!({
                    "host": m.host,
                    "fresh": m.fresh,
                    "last_beat_secs_ago": m.last_beat_secs_ago,
                    "serves_count": m.serves_count,
                    "is_this_host": m.is_this_host,
                })).collect::<Vec<_>>(),
            })),
        })),
        // Startup forge-credential preflight (#4005) — resolved once at
        // daemon boot, before the daemon's first `gh` consumer. Never
        // contains a token value; `null` only from a pre-#4005 daemon binary
        // that never computed one.
        "credential_preflight": report.credential_preflight.as_ref().map(|c| serde_json::json!({
            "ok": c.ok,
            "mechanism": c.mechanism,
            "fingerprint": c.fingerprint,
            "message": c.message,
            "checked_at": c.checked_at,
        })),
        // Scheduled drain-and-restart state (#4090). `draining: false` in the
        // common case; `note` carries the last transition (timeout refusal /
        // abort) so a scripted consumer sees why a drain ended without a restart.
        // #8514 adds the live `roll` sub-object (null when no drain is active).
        "drain": drain_render::drain_json(report),
        // Per-repo breakdown across every registered managed workspace (#3930).
        "per_repo": report.per_repo.iter().map(|r| serde_json::json!({
            "root": r.root,
            "weight": r.weight,
            "in_flight_count": r.in_flight_count,
            "health_gate_halted": r.health_gate_halted,
            "health_gate_not_evaluated": r.health_gate_not_evaluated,
            "health_gate_not_evaluated_reason": r.health_gate_not_evaluated_reason,
            "health_gate_enabled": r.health_gate_enabled,
            "health_gate_verdict_at": r.health_gate_verdict_at,
            "health_gate_deferred": r.health_gate_deferred,
            "health_gate_deferred_reason": r.health_gate_deferred_reason,
            "health_gate_verdict_tier": r.health_gate_verdict_tier,
            // Per-root role-runner enablement (#4377) — resolved from THIS
            // root's own `.loom/config.json`, independent of the daemon
            // workspace's own master switch. `on_idle_roles` non-empty while
            // `enabled` is `false` is the exact silent-no-op this issue fixes.
            "role_runner_enabled": r.role_runner_enabled,
            "role_runner_roles": r.role_runner_roles,
            "role_runner_on_idle_roles": r.role_runner_on_idle_roles,
            // Per-role `onIdleMaxWait` age/promoted status (issue #7511) —
            // one entry per role that is both `onIdle` and has a configured
            // max-wait; empty when the key is unconfigured.
            "role_runner_on_idle_promotions": r.role_runner_on_idle_promotions.iter().map(|p| serde_json::json!({
                "role": p.role,
                "max_wait_secs": p.max_wait_secs,
                "age_secs": p.age_secs,
                "promoted": p.promoted,
            })).collect::<Vec<_>>(),
            // Which tier decided `role_runner_enabled` (#6470): `Some(v)`
            // only when the host-wide `LOOM_ROLE_RUNNER` env override (not
            // this root's own config) is the cause — see
            // `role_runner_host_env_override` below for the report-level
            // twin of this field.
            "role_runner_env_override": r.role_runner_env_override,
            // Which host carries THIS workspace's role slice (#6374).
            // `owned_here` is the actionable bit; `key`/`key_source` are here
            // so an operator diffing two hosts' `--json` output can catch the
            // one failure mode deterministic sharding cannot rule out
            // structurally — two hosts resolving *different keys* for the
            // same workspace, which would let both (or neither) own it.
            "role_runner_shard": r.role_runner_shard.as_ref().map(|s| serde_json::json!({
                "owned_here": s.owned_here,
                "key": s.key,
                "key_source": s.key_source,
                "owning_shard": s.owning_shard,
                "host_shard": s.host_shard,
                "shard_count": s.shard_count,
            })),
            // Fleet-wide quarantine-stash visibility (#5692): per-repo
            // `refs/stash` counts, aggregated by
            // `quarantine_stash_status::collect_stash_summary`. Builds on
            // `check-quarantine-stashes.sh`'s (#5185) single-repo enumeration
            // rather than reinventing stash discovery.
            "stash": {
                "total_count": r.stash_total_count,
                "quarantine_count": r.stash_quarantine_count,
                "oldest_age_secs": r.stash_oldest_age_secs,
                // #5512: of the non-quarantine entries, how many are NOT
                // presumed recoverable elsewhere (an operator pre-resync
                // stash, an agent's ad-hoc WIP, …) — see
                // `quarantine_stash_status::is_presumed_recoverable`.
                "non_quarantine_unrecoverable_count": r.stash_non_quarantine_unrecoverable_count,
                "non_quarantine_unrecoverable_oldest_age_secs": r.stash_non_quarantine_unrecoverable_oldest_age_secs,
            },
        })).collect::<Vec<_>>(),
        // Worktree footprint per managed repo (#5939) — how many worktrees the
        // repo carries, split by naming class, and their total on-disk size.
        // Collected client-side (a filesystem walk, never inside the IPC
        // handler — see `worktree_disk_status`), so `null` when the caller did
        // not collect it (e.g. the fleet-status local row), distinguishable
        // from "collected, and the repo has none". `total_bytes` is itself
        // `null` when the worktree root could not be read at all, so an
        // unmeasurable repo never reads as a genuine 0 bytes.
        "worktrees": worktree_disk.map(|summaries| summaries.iter().map(|w| serde_json::json!({
            "root": w.root,
            "total_count": w.total_count,
            "issue_count": w.issue_count,
            "pr_count": w.pr_count,
            "other_count": w.other_count,
            "total_bytes": w.total_bytes,
        })).collect::<Vec<_>>()),
        // Forge-side pipeline snapshot (#3977) — present only when `--pipeline`
        // was passed; `null` otherwise so a consumer can tell "not requested"
        // apart from "requested but empty".
        "pipeline": pipeline.map(|snapshots| snapshots.iter().map(|s| serde_json::json!({
            "root": s.root,
            "queued": s.queued,
            "building": s.building,
            "review_requested": s.review_requested,
            "changes_requested": s.changes_requested,
            "approved": s.approved,
            "merged_24h": s.merged_24h,
            "error": s.error,
        })).collect::<Vec<_>>()),
        "token_usage": token_usage,
        // Self-update staleness (#3968) — read-only, local-only comparison of
        // this binary's baked-in commit vs. the source checkout's HEAD.
        "self_update": {
            "built_commit": update.built_commit,
            "source_commit": update.source_commit,
            "update_available": update.update_available,
            // Staleness magnitude + warning (Issue #6261) — how far behind
            // the running binary is, not just whether it is behind at all.
            "commits_behind": update.commits_behind,
            "hours_behind": update.hours_behind,
            "staleness_warning": self_update::staleness_warning_default(
                update.commits_behind,
                update.hours_behind,
            ),
        },
        // Running-vs-disk build staleness (#5341) — distinct from
        // `self_update` above (which compares this CLI's own build against
        // the SOURCE checkout's HEAD): this compares the answering daemon
        // PROCESS's build (sourced over IPC, `running_*`) against the
        // ON-DISK build (`disk_*`, this CLI invocation's own compile-time
        // constants — a fresh exec always reads the disk binary). `stale` is
        // `null` when the comparison cannot be made (pre-#5341 daemon, or a
        // tarball build with no git info on either side).
        "daemon_build": {
            "running_commit": report.daemon_build_commit,
            "running_built_at": report.daemon_built_at_raw,
            "disk_commit": self_update::BUILT_COMMIT,
            "disk_built_at": self_update::BUILT_AT_RAW,
            "stale": build_is_stale(report.daemon_build_commit.as_deref(), self_update::BUILT_COMMIT),
        },
        // Autonomous self-update loop state (#4055) — daemon-side loop status
        // (distinct from the client-side `self_update` staleness read above).
        // Long-running task liveness (#10414): one entry per registered loop.
        "task_liveness": report.task_liveness,
        // Codex session containers (#10600); `null` without a session seat.
        "session_containers": report.session_containers,
        "auto_update": {
            "enabled": report.auto_update_enabled,
            "mode": report.auto_update_mode, // #10954
            "config_enabled": report.auto_update_config_enabled, // #11042
            "floor_not_rolling": report.auto_update_floor_not_rolling, // #11042
            "host": report.host_update, // #12420
            "last_check": report.auto_update_last_check,
            "last_roll": report.auto_update_last_roll,
            "consecutive_failures": report.auto_update_consecutive_failures,
            "backoff_secs": report.auto_update_backoff_secs,
            "terminal_reason": report.auto_update_terminal_reason,
            "note": report.auto_update_note,
            // Issue #7609: the release artifact the loop resolved for this
            // host's platform, next to the installed version above. `null`
            // when no artifact resolved (no Releases yet, an unreachable API,
            // artifact-fetch disabled on this host) — which is exactly when
            // the loop falls back to the source-staleness path.
            "artifact_available": report.auto_update_artifact_version.as_ref().map(|version| {
                serde_json::json!({
                    "version": version,
                    "published_at": report.auto_update_artifact_published_at,
                })
            }),
        },
        // Live idle-exit eligibility (#5565) — the SAME 0-in-flight /
        // no-active-role / no-lifecycle-activity-within-the-window (or
        // token-starvation) determination `autonomous.idleExit`'s tracker
        // uses. Lets the fleet cron idle-shutdown guard
        // (`fleet add-worker --idle-shutdown-minutes`) ask this running
        // daemon "are you eligible right now" instead of vetoing on bare
        // `loom-daemon` process presence, which under the fleet's own
        // `Restart=on-success` systemd supervision is essentially always
        // true. `eligible: true` is the ONLY condition under which the guard
        // may treat a live `loom-daemon` process as idle;
        // `enabled: false` (feature never spawned) MUST be read as
        // "cannot determine here", never as eligible. `null` only from a
        // pre-#5565 daemon binary that never computed one.
        "idle_exit": report.idle_exit.as_ref().map(|i| serde_json::json!({
            "enabled": i.enabled,
            "eligible": i.eligible,
            "trigger": i.trigger,
            "idle_minutes": i.idle_minutes,
            "in_flight_sweeps": i.in_flight_sweeps,
            "active_role_runs": i.active_role_runs,
            "healthy_tokens": i.healthy_tokens,
            "total_tokens": i.total_tokens,
            "idle_elapsed_secs": i.idle_elapsed_secs,
            "starved_elapsed_secs": i.starved_elapsed_secs,
            "starvation_enabled": i.starvation_enabled,
            "observed_at": i.observed_at,
        })),
        "rate_limit_breaker": report.rate_limit_breaker.as_ref().map(|r| serde_json::json!({
            "enabled": r.enabled,
            "phase": r.phase,
            "suppressed": r.suppressed,
            "source": r.source,
            "tripped_at": r.tripped_at,
            "cooldown_until": r.cooldown_until,
            "trips_total": r.trips_total,
            "core_remaining": r.core_remaining,
            "graphql_remaining": r.graphql_remaining,
            "core_used": r.core_used,
            "graphql_used": r.graphql_used,
            "budget_probed_at": r.budget_probed_at,
        })),
        // Per-caller forge call accounting (#9251); `null` from an older daemon.
        "forge_calls": report.forge_calls,
        // Live safehouse fleet-comms connection state (#4345) — `null` only
        // from a pre-#4345 daemon binary that never computed one. `state` is
        // one of "not_configured" / "unreachable" / "connected" /
        // "send_rejected" (#4464, carries `reason`).
        "safehouse": report.safehouse.as_ref().map(|s| serde_json::json!({
            "state": s.state,
            "socket": s.socket,
            "room": s.room,
            "reason": s.reason,
        })),
        // Peer-claim view + transport counters (Issue #5921) — `null` when no
        // safehouse peer-claim coordination has been established (mirrors
        // `safehouse`'s `None`-vs-empty-view contract). Non-null carries the
        // full set `loom-daemon peer-claims` also renders, so a scripted
        // consumer never needs the standalone subcommand just to get JSON.
        "peer_claims": report.peer_claims.as_ref().map(|p| serde_json::json!({
            "self_host": p.self_host,
            "ttl_secs": p.ttl_secs,
            "entries": p.entries.iter().map(|e| serde_json::json!({
                "repo": e.repo,
                "issue": e.issue,
                "host": e.host,
                "remaining_ttl_secs": e.remaining_ttl_secs,
            })).collect::<Vec<_>>(),
            "advertised": p.advertised,
            "received": p.received,
            "expired": p.expired,
            "dispatch_skipped": p.dispatch_skipped,
            // Resolved claims-room identity (Issue #6242) — `null` when
            // peer-claim coordination was never established (safehouse
            // disabled, or no socket ever resolved), never an empty string.
            "claims_room": p.claims_room,
        })),
        // Watchdog protection state (#4354) — client-side, host-local, read-only.
        // `state` is one of "protected" / "no-marker" /
        // "watchdog-not-provisioned" / "unknown". `marker_present` and
        // `watchdog_provisioned` carry the two underlying facts separately, so a
        // consumer can see BOTH even though `state` names only the dominant one
        // (a missing marker outranks the watchdog fact). `null` only when no loom
        // dir could be resolved at all.
        "protection": protection.map(|p| serde_json::json!({
            "state": p.state.as_str(),
            "marker_present": p.marker_present,
            "marker_path": p.marker_path.display().to_string(),
            "watchdog_job": p.job.identifier(),
            "watchdog_job_kind": p.job.kind_str(),
            // `null` ⇒ the provisioning probe could not answer (no
            // launchctl/systemctl, or an unreachable `systemctl --user` bus).
            "watchdog_provisioned": p.watchdog_provisioned,
            "detail": p.detail,
            // Marker-vs-non-autonomous-daemon mismatch (#4693, AC3): `true`
            // only when the marker is present AND this reachable daemon's own
            // `work_finder_enabled` reads `Some(false)` — a healthy,
            // "protected" (crash-detectable) daemon that has nonetheless
            // silently stopped dispatching. `false` when work-finder IS on, or
            // when `work_finder_enabled` is `null` (pre-#4693 daemon binary —
            // never a false positive).
            "autonomy_mismatch": autonomy_mismatch(Some(p), report),
        })),
    });
    // Restart-survivorship seed (#6262). Inserted after the literal rather than
    // added as another `json!` key: the macro above is already at the recursion
    // limit (see the `role_agents` comment), and a post-insert costs nothing and
    // cannot push it over.
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "journal_adopted_at_startup".to_string(),
            serde_json::json!(report.journal_adopted_at_startup),
        );
    }
    // Fleet-store sync (#9596) — client-side and host-local, like `protection`
    // above, and inserted only when this host actually has a snapshot, so a
    // host with no `fleet.repo` emits exactly the payload it always did.
    let fleet_store = sections
        .has(Section::FleetStore)
        .then(loom_daemon::fleet_sync::probe_status);
    if let Some(s) = fleet_store.flatten() {
        value["fleet_store"] = fleet_store_line::json(Some(&s));
    }
    // Pending-restart marker (#9597) — a restart-required `fleet-config
    // render` change this daemon's pid has not yet picked up. Same
    // client-side/host-local shape as the fleet-store block above, inserted
    // only when there is one to report.
    if sections.has(Section::PendingRestart) {
        let pending_restart = pending_restart_line::json(report.daemon_pid);
        if !pending_restart.is_null() {
            value["pending_restart"] = pending_restart;
        }
    }
    sections.retain_keys(&mut value);
    value
}

/// Emit the combined status (daemon report + per-token usage) as JSON.
pub(crate) fn print_status_json(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
    protection: Option<&daemon_install_state::ProtectionReport>,
    worktree_disk: Option<&[WorktreeDiskSummary]>,
    sections: &SectionSet,
) -> Result<()> {
    let combined = build_status_json_value_for(
        report,
        token_usage,
        update,
        pipeline,
        protection,
        worktree_disk,
        sections,
    );
    println!("{}", serde_json::to_string_pretty(&combined)?);
    Ok(())
}

/// The reportable main-health-gate condition for one workspace root (#4012).
///
/// Pre-#4012, `loom-daemon status` derived its summary from just the
/// `halted`/`not_evaluated` boolean pair — and `(false, false)` meant any of
/// three genuinely different things: the gate is disabled, the gate is
/// enabled but has not completed its first evaluation this process
/// ("pending"), or the gate's last completed run verified `main` green
/// ("clear"). Two booleans cannot encode three states, so this enum widens
/// the reporting boundary rather than reusing the same pair for a fourth
/// meaning. [`classify_gate_verdict`] builds one from the raw wire-report
/// ingredients; [`format_gate_status`] (long form) and
/// [`gate_status_short_label`] (13-char table column) both render it, so the
/// top-level summary and the per-repo table can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateVerdict {
    /// The gate is not enabled for this root — or is enabled but has no
    /// usable `buildGate` block, which the gate loop treats identically
    /// (always-green, never runs). Dispatch is allowed; nothing will ever
    /// evaluate this root until it is turned on (and configured).
    Disabled,
    /// The gate is enabled but has not completed a first evaluation yet this
    /// daemon process. Dispatch is allowed — this is NOT evidence that `main`
    /// is healthy, only that nothing has said otherwise yet.
    Pending,
    /// The gate's most recent completed run verified `main` green. `since`
    /// is the wall-clock time of that verdict, when known (#4012 AC4) — a
    /// `clear` reading with no `since` predates the daemon populating it.
    /// `tier` (#4259) labels which stage set produced it (`"fast"` ⇒ a
    /// compile+smoke subset, NOT a full-suite green); `None` for a full-tier
    /// verdict or a pre-#4259 payload.
    Clear {
        since: Option<DateTime<Utc>>,
        tier: Option<String>,
    },
    /// The most recent tick DEFERRED for host load (#4259): the host was
    /// saturated and the bounded max-defer window had not yet elapsed, so no
    /// command ran. NOT evidence about `main` either way; dispatch is NOT
    /// halted by this. Distinct from `NotEvaluated` — the gate chose not to run,
    /// it did not run and fail to conclude.
    Deferred { reason: Option<String> },
    /// The most recent tick could not produce a verdict at all (dirty tree,
    /// timeout, missing tool, broken `git`, …) — NOT evidence about `main`
    /// either way; dispatch is NOT halted by this.
    NotEvaluated { reason: Option<String> },
    /// A completed run verified `main` red — dispatch is paused (in-flight
    /// sweeps keep running). `not_evaluated` records whether a *later* tick
    /// also failed to produce a verdict (#3950 AC3): the two can co-occur,
    /// since an unevaluated tick leaves the prior halt untouched.
    Halted {
        not_evaluated: bool,
        reason: Option<String>,
    },
}

/// Classify the reportable gate condition from a [`DaemonStatusReport`] /
/// [`crate::types::RepoStatus`]'s raw fields (#4012).
///
/// `enabled` is `Some(false)` only when the daemon positively resolved this
/// root's gate as off (or effectively off — enabled but no usable
/// `buildGate` block, via [`main_health_gate::effective_enabled`]); `None`
/// means an older daemon that never reported the flag at all, which must NOT
/// be misread as "disabled" (a bare `bool`'s wire default would do exactly
/// that — see the `Option<bool>` rationale on
/// [`DaemonStatusReport::main_health_gate_enabled`]). `halted` and
/// `not_evaluated` take priority over disabled/pending so a genuinely active
/// halt is never hidden behind either newer state — a case that in practice
/// only arises from a test poking the raw state directly, since the gate
/// loop's own disabled path always clears `halted` first.
// A pure classifier that maps the raw, independent gate status fields (each
// carried separately on `DaemonStatusReport` / `RepoStatus` with its own
// `#[serde(default)]`) onto one verdict. The argument count tracks the field
// count 1:1 by design; grouping them into a struct here would just move the
// same primitives around without adding meaning.
#[allow(clippy::too_many_arguments)]
fn classify_gate_verdict(
    enabled: Option<bool>,
    halted: bool,
    not_evaluated: bool,
    deferred: bool,
    reason: Option<&str>,
    deferred_reason: Option<&str>,
    verdict_tier: Option<&str>,
    verdict_at: Option<DateTime<Utc>>,
) -> GateVerdict {
    if halted {
        return GateVerdict::Halted {
            not_evaluated,
            reason: reason.map(str::to_string),
        };
    }
    // A load-deferral (#4259) is a current-tick scheduling decision; surface it
    // ahead of `not_evaluated` (a deferred tick clears the unevaluated flag, so
    // in practice they do not co-occur) and ahead of the disabled/pending/clear
    // readings, so the operator sees "the host is too busy to run the gate right
    // now" rather than a stale green.
    if deferred {
        return GateVerdict::Deferred {
            reason: deferred_reason.map(str::to_string),
        };
    }
    if not_evaluated {
        return GateVerdict::NotEvaluated {
            reason: reason.map(str::to_string),
        };
    }
    if enabled == Some(false) {
        return GateVerdict::Disabled;
    }
    if verdict_at.is_none() {
        return GateVerdict::Pending;
    }
    GateVerdict::Clear {
        since: verdict_at,
        tier: verdict_tier.map(str::to_string),
    }
}

/// Render the main-health gate summary line for `loom-daemon status`.
///
/// Before #3974 this line asserted "workspace tree is dirty" for every skip,
/// which reported a clean tree as dirty whenever the real cause was a
/// timeout, a missing build tool, or a broken `git`; before #4012 `clear` and
/// "the gate has never run" were the same string.
fn format_gate_status(verdict: &GateVerdict) -> String {
    match verdict {
        GateVerdict::Disabled => "disabled (gate not enabled; dispatch allowed)".to_string(),
        GateVerdict::Pending => {
            "pending (no verdict yet this process — dispatch allowed)".to_string()
        }
        GateVerdict::Clear { since, tier } => {
            // #4259: a fast-tier green covers only the compile+smoke subset, so
            // it must never read as an unqualified "clear".
            let tier_suffix = match tier.as_deref() {
                Some("fast") => " [fast tier — compile+smoke only, NOT a full-suite green]",
                _ => "",
            };
            match since {
                Some(t) => format!(
                    "clear (dispatch allowed; last verified green at {}){tier_suffix}",
                    t.to_rfc3339()
                ),
                None => format!("clear (dispatch allowed){tier_suffix}"),
            }
        }
        GateVerdict::Deferred { reason } => {
            let detail = reason
                .clone()
                .unwrap_or_else(|| "host saturated".to_string());
            format!(
                "deferred ({detail}) — the host is too busy to run the gate right now, which is \
                 NOT evidence about main; dispatch is NOT halted by this. The fast tier runs at \
                 the max-defer bound so a permanently-loaded host still reaches a verdict"
            )
        }
        GateVerdict::NotEvaluated { reason } => {
            let cause = reason
                .clone()
                .unwrap_or_else(|| "cause unrecorded".to_string());
            format!(
                "not evaluated ({cause}) — the gate could not run, which is NOT evidence about \
                 main; dispatch is NOT halted by this"
            )
        }
        GateVerdict::Halted {
            not_evaluated,
            reason,
        } => {
            if *not_evaluated {
                let cause = reason
                    .clone()
                    .unwrap_or_else(|| "cause unrecorded".to_string());
                format!(
                    "HALTED (main verified red — new dispatch paused) + NOT EVALUATED ({cause}) — \
                     the gate cannot currently confirm main is still red, or check for recovery"
                )
            } else {
                "HALTED (main verified red — new dispatch paused; in-flight sweeps keep running)"
                    .to_string()
            }
        }
    }
}

/// Render `verdict` as a short label for the per-repo table's 13-char
/// `HEALTH-GATE` column (#4012) — the short-form counterpart to
/// [`format_gate_status`].
///
/// The off-state reads `"off"`, not `"disabled"` (#8288): in a column an
/// operator scans for "why is nothing dispatching into this repo?", the word
/// "disabled" was read as *dispatch* being disabled for the workspace, when
/// it only ever meant the main-health gate feature itself is off (the
/// default) and dispatch is entirely unaffected. `"off"` pairs with the
/// adjacent `ROLES on/off` column's vocabulary — a feature switch, not a
/// verdict — and stays visually distinct from `"HALTED"` (gate on, main
/// verified red, dispatch actually paused) and `"pending"` (gate on, no
/// verdict yet).
fn gate_status_short_label(verdict: &GateVerdict) -> &'static str {
    match verdict {
        GateVerdict::Disabled => "off",
        GateVerdict::Pending => "pending",
        GateVerdict::Clear { tier: Some(t), .. } if t == "fast" => "clear(fast)",
        GateVerdict::Clear { .. } => "clear",
        GateVerdict::Deferred { .. } => "deferred",
        GateVerdict::NotEvaluated { .. } => "not-evaluated",
        GateVerdict::Halted {
            not_evaluated: true,
            ..
        } => "HALTED+UNEVAL",
        GateVerdict::Halted {
            not_evaluated: false,
            ..
        } => "HALTED",
    }
}

/// Render a stash age (seconds) in a compact, human-scaled form for the
/// per-repo "stashes: … oldest …" line (#5692) — days when at least a day
/// old (the common case for an accumulating quarantine backlog, per #5690's
/// "oldest 2026-07-27" fleet audit), otherwise hours, otherwise minutes.
#[must_use]
fn format_stash_age(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if secs >= DAY {
        format!("{}d", secs / DAY)
    } else if secs >= HOUR {
        format!("{}h", secs / HOUR)
    } else if secs >= MINUTE {
        format!("{}m", secs / MINUTE)
    } else {
        format!("{secs}s")
    }
}

/// Render the per-repo table's `HEALTH-GATE` column value (Issue #5682): the
/// distinguishing `"no-sweep"` label when `sweep_command_missing` is `true`,
/// overriding whatever the main-health gate itself reports — a root missing
/// `.claude/commands/loom/sweep.md` is refused by `dispatch()`
/// unconditionally, so the gate's own verdict (which would otherwise render
/// identically to a healthy idle repo, e.g. `HEALTH-GATE off` / `ROLES on`) is
/// irrelevant to why nothing dispatches into it. Falls through to
/// [`gate_status_short_label`] in the common case.
fn gate_column_label(sweep_command_missing: bool, verdict: &GateVerdict) -> &'static str {
    if sweep_command_missing {
        "no-sweep"
    } else {
        gate_status_short_label(verdict)
    }
}

/// Render `SweepInfo.repo` for the in-flight table's `REPO` column: the
/// basename of the workspace-root path (e.g. `/repos/loom` -> `loom`), or
/// the value itself when it has no path separator (an `owner/repo` slug
/// such as `rjwalters/vibesql` from some call sites is left as-is since
/// `Path::file_name` already returns the last component in that case).
/// Falls back to `"-"` when `repo` is `None`, matching the existing `phase`
/// fallback convention in this table (#4698).
fn format_repo_column(repo: Option<&str>) -> &str {
    match repo {
        Some(r) => Path::new(r)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(r),
        None => "-",
    }
}

/// Render the "as of" freshness suffix for the pre-flight advisory line
/// (Issue #5029), e.g. `" (as of 12s ago, 2026-08-03T12:00:00Z)"` — empty
/// string when no transition timestamp is available (an older daemon binary,
/// or a state this process has not transitioned through yet), so pre-#5029
/// output is unaffected byte-for-byte.
fn format_preflight_advisory_freshness(changed_at: Option<DateTime<Utc>>) -> String {
    match changed_at {
        Some(ts) => {
            let secs = (Utc::now() - ts).num_seconds().max(0);
            format!(" (as of {secs}s ago, {ts})")
        }
        None => String::new(),
    }
}

/// The claude-wrapper pre-flight-death tripwire warning line (#4386), with an
/// "as of" freshness suffix appended (Issue #5029) — split out from
/// [`print_status_human`] so the freshness text is unit-testable without
/// capturing process stdout.
/// `None` when the advisory is not currently active (nothing to print) or the
/// active flag is set with no message (defensive — should not happen in
/// practice).
fn render_preflight_advisory_line(report: &DaemonStatusReport) -> Option<String> {
    if !report.preflight_advisory_active {
        return None;
    }
    let msg = report.preflight_advisory_message.as_ref()?;
    // Issue #5029: append the freshness indicator so a stale (already-cleared
    // elsewhere, or long-since-tripped) warning is visibly distinguishable
    // from a just-tripped one, rather than reading as a frozen count with no
    // notion of time. The message itself already names the specific
    // workspace it is scoped to (`preflight_advisory_message`).
    Some(format!(
        "{msg}{}",
        format_preflight_advisory_freshness(report.preflight_advisory_changed_at)
    ))
}

/// The `Build: …` running-vs-disk build-staleness line (Issue #5341) — always
/// rendered, unflagged, in the same block as `Protection:` / `Observability:`.
///
/// Before this line existed, `loom-daemon status` / `--version` reported ONLY
/// the build baked into whichever binary answered the CLI invocation — and
/// because every CLI invocation freshly execs the binary on disk, that is
/// always the ON-DISK build, never the long-running daemon PROCESS's build.
/// A daemon process that predates a since-rebuilt disk binary therefore
/// misreported itself as current, and every routine "is this daemon stale?"
/// check silently passed (the `loom-worker-1` incident this issue exists for:
/// process start time ~25h old, disk binary mtime ~1h old, the two facts never
/// compared anywhere `status` looked).
///
/// - `running_commit` / `running_built_at` — the answering daemon PROCESS's own
///   build, sourced over IPC from
///   [`DaemonStatusReport::daemon_build_commit`] /
///   [`DaemonStatusReport::daemon_built_at_raw`] (never read from disk).
/// - `disk_commit` / `disk_built_at` — THIS CLI invocation's own compile-time
///   [`self_update::BUILT_COMMIT`] / [`self_update::BUILT_AT_RAW`], which —
///   because `status` always execs the on-disk binary fresh — IS the disk
///   build.
///
/// Split out from [`print_status_human`] so every branch is unit-testable
/// without capturing process stdout, mirroring [`observability_line::render`].
fn render_build_status_line(
    running_commit: Option<&str>,
    running_built_at: Option<&str>,
    disk_commit: &str,
    disk_built_at: &str,
) -> String {
    let Some(running_commit) = running_commit else {
        return "Build:         unknown (older daemon binary — restart to pick up #5341)"
            .to_string();
    };
    let running_built_at = running_built_at.unwrap_or("unknown");

    match build_is_stale(Some(running_commit), disk_commit) {
        None => format!(
            "Build:         running {running_commit} (built {running_built_at}), disk \
             {disk_commit} (built {disk_built_at}) — staleness unknown (a build lacks git info)"
        ),
        Some(false) => format!(
            "Build:         {running_commit} (built {running_built_at}) — running matches disk"
        ),
        Some(true) => format!(
            "Build:         STALE — running {running_commit} (built {running_built_at}), disk \
             {disk_commit} (built {disk_built_at}) — restart to roll: \
             ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh"
        ),
    }
}

/// Whether the running daemon PROCESS's build commit differs from the on-disk
/// build's (Issue #5341), or `None` when the comparison cannot be made:
/// `running_commit` is `None` (a pre-#5341 daemon binary that never reported
/// one), or either side is empty/`"unknown"` (a tarball build with no git
/// info baked in). Shared by [`render_build_status_line`] and
/// [`build_status_json_value`]'s `daemon_build.stale` field so the two
/// surfaces can never disagree.
fn build_is_stale(running_commit: Option<&str>, disk_commit: &str) -> Option<bool> {
    const UNKNOWN: &str = "unknown";
    let comparable = |c: &str| !c.is_empty() && c != UNKNOWN;
    let running = running_commit?;
    if !comparable(running) || !comparable(disk_commit) {
        return None;
    }
    Some(running != disk_commit)
}

/// The `artifact reclaim (deep)` block printed under the dynamic-cap section
/// (#5919) — the answer to "is this host reclaiming its own build artifacts,
/// or is its disk headroom quietly shrinking toward the next incident?".
///
/// Pure (returns lines rather than printing) so the wording of every state —
/// fired, never-fired-but-healthy, and never-evaluated — is unit-testable.
///
/// Three shapes, deliberately distinct:
/// - **no entries** — the reaper has not completed a post-startup tick (it
///   skips its first) or is disabled entirely. Says so; it is NOT the same as
///   "nothing needed reclaiming".
/// - **entry, never fired** — the healthy steady state. Renders the evaluation
///   verdict (`"118G free >= 20G floor — no disk pressure"`) so the silence is
///   attributable.
/// - **entry that fired** — names the time and exactly what went.
fn deep_clean_lines(report: &DaemonStatusReport) -> Vec<String> {
    if report.deep_clean.is_empty() {
        return vec![
            "  artifact reclaim (deep): not evaluated yet — the worktree reaper skips its \
             first tick after startup (or is disabled)"
                .to_string(),
        ];
    }

    let mut lines = vec!["  artifact reclaim (deep, per repo):".to_string()];
    for repo in &report.deep_clean {
        let fired = match (&repo.last_fired_at, &repo.last_reclaimed) {
            (Some(at), Some(what)) => {
                format!("last fired {} — reclaimed {what}", at.to_rfc3339())
            }
            (Some(at), None) => format!("last fired {}", at.to_rfc3339()),
            // Not "never": the cooldown/firing record is process state, so this
            // is scoped to this daemon's lifetime and must not read as a claim
            // about the host's whole history.
            (None, _) => "no pass has fired since this daemon started".to_string(),
        };
        lines.push(format!("    {}: {fired}", repo.root.display()));
        if let Some(reason) = &repo.last_reason {
            let when = repo
                .last_evaluated_at
                .map_or_else(|| "never".to_string(), |at| at.to_rfc3339());
            lines.push(format!("      last evaluated {when}: {reason}"));
        }
    }
    lines
}

/// The `Worktree footprint` section (Issue #5939) — how much of this host's
/// disk each managed repo's `.loom/worktrees` is holding, split by naming
/// class.
///
/// Returned as lines rather than printed so it is unit-testable as a pure
/// function (the same split [`render_in_flight_table`] already uses). `None` when the caller did
/// not collect a census at all — an absent measurement renders nothing, never
/// a misleading `0 worktrees`.
///
/// # Why this is next to the cap line
///
/// Disk no longer bounds the cap (`dynamic_cap` is the configured
/// `maxConcurrent` since #12406), so a filling disk shows up here and in the
/// reclamation lines rather than as a lower cap. Free space alone never said
/// *why* it is short: a host carrying
/// 39 GB of long-merged `pr-*` worktrees rendered identically to a host that
/// was genuinely full, and telling the two apart required `du`. The
/// per-class split is the diagnosis — `pr-* 110` next to `issue-* 14` names
/// the class that is not being reclaimed, in the same glance.
fn worktree_disk_lines(worktree_disk: Option<&[WorktreeDiskSummary]>) -> Option<Vec<String>> {
    let summaries = worktree_disk?;
    let mut lines = Vec::new();

    let total_count: usize = summaries.iter().map(|w| w.total_count).sum();
    let issue_count: usize = summaries.iter().map(|w| w.issue_count).sum();
    let pr_count: usize = summaries.iter().map(|w| w.pr_count).sum();
    let other_count: usize = summaries.iter().map(|w| w.other_count).sum();
    // A repo whose worktree root could not be read contributes nothing to the
    // byte total; `measured` records whether ANY repo produced a figure, so a
    // fleet-wide measurement failure prints "unknown" rather than "0 B".
    let measured = summaries.iter().any(|w| w.total_bytes.is_some());
    let total_bytes: u64 = summaries.iter().filter_map(|w| w.total_bytes).sum();
    let size = if measured {
        worktree_disk_status::format_bytes(total_bytes)
    } else {
        "unknown".to_string()
    };

    lines.push(format!(
        "Worktree footprint: {total_count} worktree(s), {size} on disk \
         (issue-* {issue_count}, pr-* {pr_count}, other {other_count})"
    ));

    if summaries.is_empty() {
        lines.push("  (no managed repos)".to_string());
        return Some(lines);
    }

    lines.push(format!(
        "  {:>5}  {:>9}  {:>8}  {:>5}  {:>5}  REPO",
        "COUNT", "SIZE", "ISSUE-*", "PR-*", "OTHER"
    ));
    lines.push(format!("  {:-<52}", ""));
    for w in summaries {
        let size = w
            .total_bytes
            .map_or_else(|| "unknown".to_string(), worktree_disk_status::format_bytes);
        lines.push(format!(
            "  {:>5}  {:>9}  {:>8}  {:>5}  {:>5}  {}",
            w.total_count,
            size,
            w.issue_count,
            w.pr_count,
            w.other_count,
            w.root.display()
        ));
    }

    // #5939's generalized failure mode: a worktree naming class the reaper's
    // filters do not recognize is reclaimed by nothing at all. `pr-*` was that
    // class until this issue; naming the residual `other` bucket makes the
    // next such class visible the day it appears instead of after it has
    // eaten a disk.
    if other_count > 0 {
        lines.push(format!(
            "  note: {other_count} worktree(s) match neither `issue-<N>` nor `pr-<N>` — the \
             periodic reaper does not reclaim these; remove them by hand or with \
             `loom-daemon clean --aggressive`"
        ));
    }

    Some(lines)
}

/// Render the `CTR` (containment) column for one in-flight sweep (issue
/// #7430, epic #6896 Phase 3): `-` for bare-metal dispatch (the overwhelming
/// default today — containment is opt-in, see `runtimes.containment.enabled`
/// in `spawn-claude.sh`), or a compact `cpu=<v>,mem=<v>` / `unbounded`
/// summary for a containerized one.
///
/// The prefix names the containment SHAPE when the marker records one (issue
/// #8403): `claude-ephemeral(...)` for `spawn-claude.sh`'s per-sweep
/// container, `native-ephemeral(...)` for a Pi/OpenCode worker's. A
/// pre-#8403 marker carries no `containment=` token and renders under the
/// generic `container(...)` prefix it always did.
///
/// This is a CLIENT-SIDE, render-time read of the sweep's own per-sweep log
/// (`loom_daemon::sweep_registry::containment_signal::detect_containment`) —
/// deliberately NOT a `SweepInfo` field. `log_path` is always an absolute,
/// workspace-rooted path (`SweepRegistryConfig::logs_dir()` joins the
/// workspace root), so this works from any cwd the CLI happens to run from,
/// exactly like a fresh `loom-daemon status` invocation always can read it
/// (same host, same filesystem the daemon itself wrote it to). Best-effort:
/// a missing/unreadable log (e.g. a very recent dispatch, or a rotated log)
/// degrades to `-`, never an error, since `detect_containment` never fails.
fn format_containment_column(s: &loom_daemon::types::SweepInfo) -> String {
    use loom_daemon::sweep_registry::containment_signal::detect_containment;

    let header_anchor = format!("sweep_id={}", s.sweep_id);
    let signal = detect_containment(&s.log_path, &header_anchor);
    let Some(kind) = signal.label() else {
        return "-".to_string();
    };
    match (signal.cpus.as_deref(), signal.memory.as_deref()) {
        (None, None) => format!("{kind}(unbounded)"),
        (Some(cpu), None) => format!("{kind}(cpu={cpu})"),
        (None, Some(mem)) => format!("{kind}(mem={mem})"),
        (Some(cpu), Some(mem)) => format!("{kind}(cpu={cpu},mem={mem})"),
    }
}

/// Render the in-flight-sweeps table body (header + separator + one row per
/// sweep, or the `(none)` placeholder) as a `String` — split out from
/// [`print_status_human`] so the `REPO` column (#4698) is unit-testable
/// without capturing process stdout.
fn render_in_flight_table(report: &DaemonStatusReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if report.in_flight.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let _ = writeln!(
            out,
            "  {:<30} {:>7} {:>8}  {:<20} {:<16} {:<8} PHASE",
            "SWEEP", "ISSUE", "PID", "TOKEN", "REPO", "CTR"
        );
        let _ = writeln!(out, "  {:-<100}", "");
        for s in &report.in_flight {
            let issue = match &s.kind {
                SweepKind::Issue(n) | SweepKind::Step { item: n, .. } => format!("#{n}"),
                SweepKind::PrSet(_) => "prs".to_string(),
            };
            let repo = format_repo_column(s.repo.as_deref());
            // A step has no sweep phases; its role is its phase (#11898).
            let fallback = s
                .kind
                .role()
                .map_or("-", loom_daemon::step_role::StepRole::as_str);
            let mut phase = s.latest_phase.as_deref().unwrap_or(fallback).to_string();
            if s.overflow {
                // #9244: the host's single over-limit starred sweep.
                phase.push_str(" [overflow]");
            }
            let ctr = format_containment_column(s);
            let _ = writeln!(
                out,
                "  {:<30} {:>7} {:>8}  {:<20} {:<16} {:<8} {}",
                s.sweep_id, issue, s.pid, s.token_name, repo, ctr, phase
            );
        }
    }
    out
}

/// Render the concurrent role-agent line (#6102) that follows the in-flight
/// sweep table.
///
/// Printed here, immediately under the sweeps, because "total agent load on
/// this host" is one question and it had two answers in two places (the status
/// report for sweeps, `pgrep` for role agents). The line always names that
/// `maxConcurrent` does **not** cover this count — that misreading is the
/// entire subject of #6102, and it cost a Mac Studio an overnight hard halt at
/// a 1m load average of 126–136.
///
/// A daemon that reports no ceiling (`None` — pre-#6102 wire payload) is
/// rendered as `unknown`, never as "unbounded": an older daemon genuinely has
/// no ceiling, but this client cannot tell that from a field it never sent.
fn render_role_agent_line(report: &DaemonStatusReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let ceiling = report
        .role_agent_max_concurrent
        .map_or_else(|| "unknown".to_string(), |c| c.to_string());
    let _ = writeln!(
        out,
        "\nRole-runner agents in flight: {} (ceiling {ceiling})",
        report.active_role_agents
    );
    let _ = writeln!(
        out,
        "  (a SEPARATE ceiling from the sweep cap below — autonomous.workFinder.maxConcurrent \
         bounds sweep dispatch only; tune these agents with \
         autonomous.roleRunner.maxConcurrent / LOOM_ROLE_RUNNER_MAX_CONCURRENT, #6102)"
    );
    let _ = writeln!(
        out,
        "  total agents on this host = {} sweep(s) + {} role agent(s) = {}",
        report.in_flight.len(),
        report.active_role_agents,
        report.in_flight.len() + report.active_role_agents
    );
    out
}

/// Render the host-level role runner header line (#6470), printed above the
/// per-root "Managed repos" table so the `LOOM_ROLE_RUNNER` host master
/// switch state is visible without scanning every row's ROLES column (or
/// worse, every root's own `.loom/config.json`).
///
/// Branches on [`DaemonStatusReport::role_runner_host_env_override`]:
/// * `Some(true)` / `Some(false)` — the env override is set and decides for
///   every registered root identically, overriding each root's own config.
/// * `None` — no host override; each root's own
///   `autonomous.roleRunner.enabled` decides independently (the pre-#6470
///   behavior), so the line instead summarizes how many of the registered
///   roots currently resolve enabled.
#[must_use]
fn render_role_runner_host_header_line(report: &DaemonStatusReport) -> String {
    match report.role_runner_host_env_override {
        Some(true) => format!(
            "\nRole runner (host): ON — env {}=<truthy> overrides every root's own config",
            loom_daemon::role_runner::ROLE_RUNNER_ENABLE_ENV
        ),
        Some(false) => format!(
            "\nRole runner (host): OFF — env {}=<falsy> overrides every root's own config \
             (a root's own autonomous.roleRunner.enabled=true is ignored while this is set)",
            loom_daemon::role_runner::ROLE_RUNNER_ENABLE_ENV
        ),
        None => {
            let enabled_count = report
                .per_repo
                .iter()
                .filter(|r| r.role_runner_enabled)
                .count();
            format!(
                "\nRole runner (host): no env override — resolved per-root config ({} of {} \
                 registered root(s) enabled)",
                enabled_count,
                report.per_repo.len()
            )
        }
    }
}

/// Render the host-level role-runner **sharding** posture line (#6374),
/// printed directly under [`render_role_runner_host_header_line`].
///
/// `None` — print nothing — in exactly two cases, which must not be conflated:
/// * The daemon reported no posture at all (pre-#6374 binary). Printing
///   "unsharded" would be a claim this client cannot substantiate, the same
///   `unknown`-vs-`unbounded` distinction [`render_role_agent_line`] makes.
/// * The posture is off *because nothing is configured* — the overwhelmingly
///   common single-host install, which should not grow a line about a feature
///   it does not use.
///
/// Every other unsharded reason (a malformed knob, an out-of-range index, the
/// refused tracked-config index) DOES print: those are misconfigurations the
/// operator asked for and did not get, and silence is precisely how the
/// pre-#6374 `LOOM_ROLE_RUNNER=0` mitigation became invisible.
#[must_use]
fn render_role_runner_shard_header_line(report: &DaemonStatusReport) -> Option<String> {
    let shard = report.role_runner_shard.as_ref()?;
    if !shard.configured {
        return None;
    }
    Some(format!("Role runner (sharding): {}", shard.summary))
}

/// Render the roster section (Issue #7690, Phase A of #6704), printed
/// directly under [`render_role_runner_shard_header_line`] — the issue, the
/// live/seen member counts, the current generation and how long it has been
/// settled, and one line per member. `None` when the daemon reported no
/// roster (disabled — the default — or no heartbeat cycle has completed yet).
///
/// An EXPIRED member is rendered, never dropped: silence about a dead host is
/// exactly how the pre-#6374 `LOOM_ROLE_RUNNER=0` mitigation became invisible
/// (the same rationale [`render_role_runner_shard_header_line`]'s doc comment
/// gives for a misconfigured-but-configured shard).
#[must_use]
fn render_roster_lines(report: &DaemonStatusReport) -> Vec<String> {
    let Some(roster) = report
        .role_runner_shard
        .as_ref()
        .and_then(|s| s.roster.as_ref())
    else {
        return Vec::new();
    };
    let gen_str = roster
        .generation
        .map(|g| g.to_rfc3339())
        .unwrap_or_else(|| "(none)".to_string());
    let settled_str = roster
        .settled_secs
        .map(|s| format!(" (settled {}m)", s / 60))
        .unwrap_or_default();
    let mut lines = vec![format!(
        "  Roster: issue {} · {} live / {} seen · gen {gen_str}{settled_str}",
        roster.issue, roster.live_count, roster.seen_count,
    )];
    // The fence verdict (#7691), directly under the roster header: a host
    // that is YIELDING runs no role ticks at all, and that must never be
    // invisible — it is the one state where "nothing is happening" is correct
    // by design rather than a bug.
    if let Some(fence) = &roster.fence {
        lines.push(format!("  Fence: {fence}"));
    }
    for m in &roster.members {
        let freshness = if m.fresh { "fresh  " } else { "EXPIRED" };
        let this_host = if m.is_this_host {
            "   ← this host"
        } else {
            ""
        };
        lines.push(format!(
            "    {:<14} {freshness} (last beat {}s ago)   serves {}{this_host}",
            m.host, m.last_beat_secs_ago, m.serves_count,
        ));
    }
    lines
}

/// Render the per-root "which host carries this workspace's role slice" line
/// (#6374) — `None` on an unsharded host, where every workspace is carried
/// here and the line would be pure noise on every row.
///
/// The **not-owned** case is the load-bearing one: without it, a workspace
/// whose slice belongs to a peer is indistinguishable in `status` from one
/// that is silently getting zero ticks because of a bug. It names the owning
/// shard and the key (plus the tier the key came from) so an operator
/// comparing two hosts' output can spot the one failure this design cannot
/// prevent structurally — the two hosts resolving *different keys* for the
/// same workspace.
#[must_use]
fn render_role_runner_shard_repo_line(r: &loom_daemon::types::RepoStatus) -> Option<String> {
    let shard = r.role_runner_shard.as_ref()?;
    let (owner, count) = (shard.owning_shard?, shard.shard_count?);
    let verdict = if shard.owned_here {
        "role slice OWNED by this host".to_string()
    } else {
        format!(
            "role slice owned by shard {owner} of {count}, NOT this host — this root's role \
             ticks run on the peer holding {}={owner} (#6374)",
            loom_daemon::role_shard::SHARD_INDEX_ENV
        )
    };
    Some(format!(
        "        {verdict} [shard {owner}/{count}, key={:?} via {}]",
        shard.key, shard.key_source
    ))
}

/// Render the per-root "role runner disabled but onIdle configured" line
/// (#4377), naming the TRUE cause (#6470) — `None` when the condition does
/// not apply (enabled, or no `onIdle` roles configured for this root).
///
/// [`crate::types::RepoStatus::role_runner_env_override`] disambiguates the
/// two structurally different reasons `role_runner_enabled` can be `false`:
/// * `Some(false)` — the host-wide `LOOM_ROLE_RUNNER` env override is the
///   cause, not this root's own config (which may already say
///   `enabled: true`) — telling the reader to edit it would be actively
///   wrong, so this branch points at the host-level header line instead.
/// * `None` (or, structurally impossible here, `Some(true)`) — this root's
///   own `autonomous.roleRunner.enabled` (or the built-in default) decided,
///   so the original #4377 config-edit instruction is still correct.
#[must_use]
fn render_role_runner_disabled_line(r: &loom_daemon::types::RepoStatus) -> Option<String> {
    if r.role_runner_enabled || r.role_runner_on_idle_roles.is_empty() {
        return None;
    }
    let list = r.role_runner_on_idle_roles.join(", ");
    if r.role_runner_env_override == Some(false) {
        Some(format!(
            "        role runner disabled for this root but onIdle=[{list}] is configured — \
             disabled by the host-wide env override {} (this root's own .loom/config.json is \
             overridden, editing it will not help) — see the \"Role runner (host)\" line above \
             (#6470)",
            loom_daemon::role_runner::ROLE_RUNNER_ENABLE_ENV
        ))
    } else {
        Some(format!(
            "        role runner disabled for this root but onIdle=[{list}] is configured — \
             these roles will never fire until autonomous.roleRunner.enabled=true is set in \
             this root's own .loom/config.json (#4377)"
        ))
    }
}

/// Render the per-root `onIdleMaxWait` age/promoted lines (issue #7511) —
/// one line per role that is both `onIdle` and has a configured max-wait,
/// e.g. `hermit: 31h since last tick, PROMOTED (max wait 24h)`. Empty
/// (`vec![]`, no lines printed) when the repo has no such roles configured —
/// this is purely additive surfacing of
/// [`loom_daemon::types::RepoStatus::role_runner_on_idle_promotions`], never
/// a gate.
#[must_use]
fn render_role_runner_on_idle_promotion_lines(r: &loom_daemon::types::RepoStatus) -> Vec<String> {
    r.role_runner_on_idle_promotions
        .iter()
        .map(|p| {
            let age = match p.age_secs {
                Some(secs) => format!("{} since last tick", format_stash_age(secs)),
                None => "never ticked".to_string(),
            };
            let verdict = if p.promoted {
                "PROMOTED"
            } else {
                "not yet due"
            };
            format!(
                "        {}: {age}, {verdict} (onIdleMaxWait={}, #7511)",
                p.role,
                format_stash_age(p.max_wait_secs)
            )
        })
        .collect()
}

/// Render the restart-survivorship seed line (#6262).
///
/// Printed only when non-zero, on purpose: `0` is both the "idle host at
/// startup" case and the "every claim lock survived and `reconstruct()`
/// recovered every sweep" case — the healthy shape either way — and a line that
/// appears on every status invocation to say "nothing happened" is noise an
/// operator learns to skip past.
///
/// A **non-zero** count is the signal worth a line: the daemon started with
/// sweeps still running whose claim locks did NOT survive, so the lock-based
/// `reconstruct()` could not see them and the machine-journal safety net had to
/// seed them into capacity accounting instead. That is exactly the condition
/// that, before #6262, let the work finder refill to cap on top of the
/// survivors (the 2026-08-14 "28 running vs cap 12" incident).
fn render_journal_adoption_line(report: &DaemonStatusReport) -> String {
    use std::fmt::Write as _;
    if report.journal_adopted_at_startup == 0 {
        return String::new();
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\nAdopted {} surviving sweep(s) from the machine journal at startup (#6262)",
        report.journal_adopted_at_startup
    );
    let _ = writeln!(
        out,
        "  (they were still running across the restart but their claim locks were gone, so \
         reconstruct() could not see them; they DO occupy capacity — this daemon will not \
         dispatch on top of them)"
    );
    out
}

/// Emit the combined status as a human-readable table.
pub(crate) fn print_status_human(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
    protection: Option<&daemon_install_state::ProtectionReport>,
    worktree_disk: Option<&[WorktreeDiskSummary]>,
) {
    println!("\n=== Loom Autonomous Daemon Status ===\n");

    println!("In-flight sweeps: {}", report.in_flight.len());
    print!("{}", render_in_flight_table(report));
    print!("{}", render_role_agent_line(report));
    print!("{}", render_journal_adoption_line(report));

    // Live-locked-but-unregistered sweeps (#4214): a sweep whose per-issue lock
    // has a live `owner_pid` but no matching in-flight entry above. Non-empty
    // means the daemon's in-memory registry union has lost track of a sweep
    // that is demonstrably still alive (the lock proves it) — a monitor should
    // read this as ALIVE, not dead, and an operator should reconcile rather
    // than re-dispatch (re-dispatch is blocked by the lock anyway, #4146).
    if !report.unregistered_locked.is_empty() {
        println!(
            "\nWARNING: {} live-locked sweep(s) missing from the in-flight registry \
             (alive, not dead — reconcile, don't re-dispatch):",
            report.unregistered_locked.len()
        );
        for u in &report.unregistered_locked {
            println!("  issue #{} (pid {}) in {}", u.issue, u.owner_pid, u.root.display());
        }
    }

    // Stale-untracked-sweep backstop (Issue #7529): a sweep that has been
    // alive and log-silent well past every existing watchdog timeout, but
    // that neither the startup-hang (#3887) nor the review-stall (#3910)
    // watchdog can ever reach because this daemon instance holds no retained
    // process handle for it (a restart-survived, re-admitted entry). This is
    // a hard finding, not merely informational — see `assess_stale_sweeps` in
    // `loom-daemon health` for the same signal rolled up into a verdict.
    if !report.stale_sweeps.is_empty() {
        println!(
            "\nWARNING: {} stale untracked sweep(s) with zero watchdog coverage (#7529):",
            report.stale_sweeps.len()
        );
        for s in &report.stale_sweeps {
            let idle = s
                .log_idle_secs
                .map_or_else(|| "unreadable/missing".to_string(), |secs| format!("{secs}s"));
            println!(
                "  issue #{} (pid {}, alive {}s, log idle {idle}) in {}",
                s.issue,
                s.pid,
                s.elapsed_secs,
                s.root.display()
            );
        }
    }

    // Stalled activity the daemon has stopped retrying on its own: backed-off
    // worktree removals (#7590) and held token pools (#7708). Both are built
    // in `holds`, which `loom-daemon health`'s own `health::holds` module
    // mirrors — see there for why a pool hold is never folded into the token
    // capacity figures printed further below.
    for line in holds::render_lines(report) {
        println!("{line}");
    }

    // Claude-wrapper pre-flight-death tripwire (#4386): printed prominently,
    // ahead of the capacity section, so a fleet-wide spawn failure is visible
    // even to an operator skimming just the top of `status`. Printed FIRST so
    // the "not capacity-bound … the limiter is work availability" line further
    // below is never the only diagnosis shown while this is tripped — see the
    // guard on that line.
    if let Some(line) = render_preflight_advisory_line(report) {
        println!("\n{line}");
    }

    // Observability host-identity mismatch (#4830): printed alongside the
    // tripwire above because it has the same character — a silent,
    // config-level fault whose only symptom is that data goes somewhere
    // unexpected. `loom-daemon health` reports the same condition as an
    // `observability DEGRADED` section.
    if let Some(mismatch) = &report.observability_host_id_mismatch {
        println!(
            "\nWARNING: telemetry is being filed under host_id {} — this host's ingest key is \
             bound to that id, not to {}. Install the key provisioned for {}, or set \
             $LOOM_HOST_ID to match the key's binding, then restart the daemon.",
            mismatch.ingest_host_id, mismatch.daemon_host_id, mismatch.daemon_host_id
        );
    }

    // Capacity figures resolved from a single source (fresh probe when
    // available, else the daemon's ranking snapshot) so the cap's healthy-tokens
    // input, the Token-capacity summary, and the Per-token table all agree (#3936).
    let rc = resolve_capacity(report, token_usage);

    // #4344: `rc` prefers a fresh client-side probe when one succeeded, which
    // can legitimately show a *different* (usually fresher) number than what
    // the running daemon actually used for its own dispatch decision this
    // tick. `report.dynamic_cap` is that daemon-side truth — the number
    // dispatch decisions are actually gated on — so the headline always names
    // the daemon's own cap; the probe's number is shown as a labeled
    // secondary line only when it disagrees.
    let dispatch_cap = report.dynamic_cap;
    println!("\nConcurrency cap: {dispatch_cap}  (maxConcurrent; nothing else limits it, #12406)");
    // Host CPU OBSERVATION (#3978 AC4; measured-idle signal #4031) — since
    // #4512 this is deliberately NOT a cap term: it is the evidence an operator
    // uses to decide whether this machine's `maxConcurrent` should go up (host
    // mostly idle) or not (host saturated). `logical_cpus == 0` means an older
    // daemon (pre-#3978) never sent these fields — nothing to show.
    if report.logical_cpus > 0 {
        let basis = match (report.cpu_idle_fraction, report.loadavg_1m) {
            (Some(idle), load) => {
                let consumed = report.logical_cpus as f64 * (1.0 - idle.clamp(0.0, 1.0));
                format!(
                    "{} logical cores, {:.0}% idle measured (≈{:.1} cores consumed){}",
                    report.logical_cpus,
                    idle * 100.0,
                    consumed,
                    load.map_or_else(String::new, |l| format!(", 1m loadavg {l:.2}"))
                )
            }
            (None, Some(load)) => format!(
                "{} logical cores, 1m loadavg {load:.2} (no idle sample yet)",
                report.logical_cpus
            ),
            (None, None) => {
                format!("{} logical cores, no CPU signal on this platform", report.logical_cpus)
            }
        };
        println!("  host cpu (observed, not a cap term since #4512): {basis}");
    }

    // Artifact reclamation (#5919): "how much disk is free" and "here is why it
    // is or is not being actively reclaimed" are the same question for an
    // operator watching a host's disk fill. (Disk no longer lowers the cap,
    // #12406.)
    for line in deep_clean_lines(report) {
        println!("{line}");
    }

    // Worktree footprint (#5939) — printed under the cap line because it
    // explains where a host's disk went. Without it, a host silently carrying
    // 110 merged-PR worktrees looked exactly like a host that was genuinely
    // out of space.
    if let Some(lines) = worktree_disk_lines(worktree_disk) {
        println!();
        for line in lines {
            println!("{line}");
        }
    }

    // Token-capacity backpressure section (#3902, source-unified in #3936).
    println!("\nToken capacity:");
    // Name the resolved pool directory (#4292) — the same one the per-token
    // usage table below was probed against — so a mismatch between "where I
    // ran this command from" and "where the daemon's pool actually lives" is
    // visible instead of silent. `None` only from a pre-#4292 daemon binary.
    match &report.token_pool_dir {
        Some(dir) => println!("  pool: {}", dir.display()),
        None => println!("  pool: (unknown — daemon predates #4292)"),
    }
    // "Currently binding" vs "smallest ceiling" (#4031): the dynamic cap is the
    // minimum of several ceilings, but a ceiling only *binds* once in-flight
    // occupancy reaches it. Below the cap the limiter is work availability, not
    // any resource term — so the token-bound diagnosis below is gated on this.
    // #4344: this must be checked against the daemon's *actual* dispatch cap
    // (`dispatch_cap`), not `rc.effective_cap` — the latter is recomputed from
    // a fresh client-side probe when one succeeds and can disagree with what
    // the daemon itself used, which previously let "not capacity-bound" print
    // even while the daemon's real (lower) cap was already saturated.
    let capacity_bound = report.in_flight.len() >= dispatch_cap;
    // #4344, re-scoped by #5270/#5305: this used to mean "the token term is
    // starving *dispatch itself*" — true pre-#5270, when the token axis was
    // part of `dynamic_cap`. Since #5270 that is no longer possible on any
    // auth path: `dynamic_cap` has no token term (and since #12406 it is the
    // configured `maxConcurrent` alone). #5304 hard-coded this to `false` to reflect that, which
    // also silenced the underlying ranking-divergence detection entirely
    // (#4344's original point) — the daemon's *own* `.ranking` read can still
    // disagree with a fresher probe/ranking even though it no longer affects
    // the cap. #5305 restores the detection: it now means the daemon's own
    // spawn-time account **selection** will read 0/N healthy from a stale
    // `.ranking`, while a fresher read (probe or re-check) shows real
    // capacity — connects to #5269/#5283's per-repo ranking-staleness
    // surfacing on `loom-daemon health`, this is the `status`-side
    // counterpart.
    let dispatch_starved_but_disagrees = ranking_diverges_from_starvation(report, &rc);
    if rc.ranking_present {
        let src = if rc.source == "probe" {
            "live probe: loom-daemon tokens check --json"
        } else {
            "from .loom/tokens/.ranking"
        };
        // Per-model-class breakdown (#8058 Phase 3), spliced into the same
        // line rather than added below it so the two numbers are read
        // together. Empty — the pool has no class-scoped `.bad_tokens` state,
        // which is every pool that has never recorded one — leaves this line
        // byte-identical to its pre-#8058 form. Read from the pool directory
        // the DAEMON resolved (#4292), the same rule the healthy counts above
        // follow, so both describe one pool.
        let class_suffix = model_class::capacity_suffix(report);
        println!(
            "  {}/{} accounts healthy{class_suffix}, {} exhausted/near-ceiling ({src})",
            rc.healthy, rc.total, rc.exhausted
        );
        if dispatch_starved_but_disagrees {
            // Headline promotion (#4344, re-scoped by #5305 for #5270): the
            // daemon's own `.ranking` read is starved at 0 healthy accounts
            // while the number above disagrees. The dynamic concurrency cap
            // itself is unaffected (#5270 — no token term on any auth path),
            // but every spawn this daemon dispatches still picks its account
            // from that same stale `.ranking` file, so account selection will
            // keep failing until it is refreshed.
            let pool_display = report
                .token_pool_dir
                .as_ref()
                .map_or_else(|| "(unknown pool dir)".to_string(), |d| d.display().to_string());
            println!(
                "  \u{26a0} SPAWN SELECTION IS TOKEN-STARVED: the daemon's own ranking read shows \
                 0/{} healthy, disagreeing with the {} healthy shown above from {pool_display}. \
                 The concurrency cap ({dispatch_cap}) is unaffected (#5270), but every spawn still \
                 picks its account from that stale ranking and will fail selection — refresh it \
                 with `loom-daemon tokens check --ranking` (or wait for the next self-refresh).",
                report.capacity.total_accounts, rc.healthy,
            );
        } else if rc.source == "probe"
            && report.capacity.ranking_present
            && report.capacity.healthy_accounts != rc.healthy
        {
            // Non-zero disagreement (the daemon still has *some* healthy
            // accounts, just a different count than the fresh probe) stays a
            // small-print note — dispatch is not silently starved here, just
            // running on slightly stale data.
            println!(
                "  note: daemon dispatch cap still uses a stale .ranking ({} healthy); \
                 refresh it with `loom-daemon tokens check --ranking`.",
                report.capacity.healthy_accounts
            );
        }
        // #4344: when the daemon's own ranking read is unambiguously
        // starved (see above), never print "the limiter is work
        // availability" — the headline diagnosis already named the real
        // problem, and running the generic capacity_bound/token_bound chain
        // underneath it would contradict it.
        if !dispatch_starved_but_disagrees {
            if !capacity_bound {
                // #4031: below the cap nothing is binding, so never name a
                // resource. #9131: `below_cap_line` names any active dispatch
                // hold (drain, breaker, brake, red main, pool) before blaming
                // work availability; #4386 pre-flight suppression lives there too.
                if let Some(line) = capacity_line::below_cap_line(report, dispatch_cap, "disk/CPU")
                {
                    println!("{line}");
                }
            } else if rc.token_bound {
                // #5305: `token_bound` means genuine starvation (zero healthy
                // accounts) — since #5270 the concurrency cap itself is
                // unaffected (it is the configured `maxConcurrent`, #12406,
                // with no token term), but with zero healthy accounts every spawn will
                // still fail account selection at dispatch time.
                println!(
                    "  token-bound: NO healthy accounts — the concurrency cap is unaffected \
                     (#5270), but every spawn will fail account selection until capacity \
                     returns. Add accounts (~/.llm-monitor/accounts.env + `loom-daemon tokens \
                     bootstrap`) or buy API credits, then `loom-daemon tokens check --ranking`."
                );
            } else {
                println!("  not token-bound (healthy accounts available for selection)");
            }
        }
    } else {
        println!(
            "  (no ranking — run `loom-daemon tokens check --ranking`; token pool size {} used as the \
             health basis)",
            report.token_pool_size
        );
        if !capacity_bound {
            // #9131: same line choice as the ranking-present branch above.
            if let Some(line) =
                capacity_line::below_cap_line(report, dispatch_cap, "tokens/disk/CPU")
            {
                println!("{line}");
            }
        }
    }

    // "Halted" (a completed gate run found main verified-red) and "not
    // evaluated" (the gate could not run this tick) are distinct states that can
    // co-occur (#3950 AC3): a prior halt persists untouched while an
    // environmental failure blocks the *next* evaluation. The not-evaluated
    // cause is reported verbatim from the gate (#3974 AC2) — pre-#3974 this
    // line hard-coded "workspace tree is dirty" for every skip, which
    // misreported timeouts / missing tools / broken `git` as a dirty tree.
    let verdict = classify_gate_verdict(
        report.main_health_gate_enabled,
        report.main_health_gate_halted,
        report.main_health_gate_not_evaluated,
        report.main_health_gate_deferred,
        report.main_health_gate_not_evaluated_reason.as_deref(),
        report.main_health_gate_deferred_reason.as_deref(),
        report.main_health_gate_verdict_tier.as_deref(),
        report.main_health_gate_verdict_at,
    );
    let gate = format_gate_status(&verdict);
    println!("\nMain-health gate: {gate}");

    // Startup forge-credential preflight (#4005) — resolved once at daemon
    // boot, before the daemon's first `gh` consumer, so a headless/SSH-only
    // start with no usable credential is visible here rather than only as
    // silent per-tick 401s in the logs. `None` only from a pre-#4005 daemon
    // binary that never computed one.
    match &report.credential_preflight {
        Some(c) if c.ok => {
            println!(
                "Forge credential: OK — {} ({})",
                c.mechanism,
                c.fingerprint.as_deref().unwrap_or("no fingerprint")
            );
        }
        Some(c) => println!("Forge credential: DEGRADED — {}", c.message),
        None => {
            println!("Forge credential: unknown (older daemon binary — restart to pick up #4005)")
        }
    }

    // Live safehouse fleet-comms connection state (#4345): before this,
    // "not configured", "configured but unreachable", and "connected" all
    // looked identical — silence. See `.loom/docs/safehouse.md`.
    match &report.safehouse {
        Some(s) if s.state == "connected" => {
            println!(
                "Safehouse:     connected (room: {}, socket: {})",
                s.room.as_deref().unwrap_or("(default — sole joined room)"),
                s.socket
                    .as_ref()
                    .map_or_else(|| "?".to_string(), |p| p.display().to_string())
            );
        }
        Some(s) if s.state == "unreachable" => {
            println!(
                "Safehouse:     configured, unreachable (socket: {})",
                s.socket
                    .as_ref()
                    .map_or_else(|| "unresolved".to_string(), |p| p.display().to_string())
            );
        }
        // #4464: handshake succeeds but every send is rejected — the socket is
        // reachable, so "unreachable" would point the operator at the wrong
        // fix. Surface the rejection reason directly (canonically a missing
        // `safehouse.room` on a multi-room host).
        Some(s) if s.state == "send_rejected" => {
            println!(
                "Safehouse:     connected, sends rejected: {} (socket: {})",
                s.reason.as_deref().unwrap_or("unknown reason"),
                s.socket
                    .as_ref()
                    .map_or_else(|| "?".to_string(), |p| p.display().to_string())
            );
        }
        Some(s) if s.state == "not_configured" => println!("Safehouse:     not configured"),
        // #4464: an unknown state string (version skew with a newer daemon)
        // degrades legibly — print the raw state rather than mislabeling it
        // "not configured" (the old fallthrough, which hid real states).
        Some(s) => println!("Safehouse:     {} (socket: {})", s.state, {
            s.socket
                .as_ref()
                .map_or_else(|| "?".to_string(), |p| p.display().to_string())
        }),
        None => {
            println!("Safehouse:     unknown (older daemon binary — restart to pick up #4345)")
        }
    }

    // Peer-claim view + transport counters (Issue #5921): before this, the
    // ONLY signal an operator had for #4028/#4431's soft cross-host claim
    // mechanism was a `debug!`-only re-advertisement log line — a duplicate
    // dispatch (e.g. #5789) could not be root-caused without attaching a
    // debugger. `None` when no safehouse peer-claim coordination has been
    // established (mirrors the Safehouse block's `None` case above).
    match &report.peer_claims {
        Some(pc) if pc.entries.is_empty() => {
            println!(
                "Peer claims:   none live (self_host: {}, ttl: {}s, room: {}, advertised={} \
                 rejected={} received={} expired={} dispatch_skipped={})",
                pc.self_host,
                pc.ttl_secs,
                pc.claims_room.as_deref().unwrap_or("none"),
                pc.advertised,
                pc.advertise_rejected,
                pc.received,
                pc.expired,
                pc.dispatch_skipped
            );
            peer_claims_line::print_coordination_verdict(&pc.coordination);
        }
        Some(pc) => {
            println!(
                "Peer claims:   {} live (self_host: {}, ttl: {}s, room: {}, advertised={} \
                 rejected={} received={} expired={} dispatch_skipped={})",
                pc.entries.len(),
                pc.self_host,
                pc.ttl_secs,
                pc.claims_room.as_deref().unwrap_or("none"),
                pc.advertised,
                pc.advertise_rejected,
                pc.received,
                pc.expired,
                pc.dispatch_skipped
            );
            peer_claims_line::print_coordination_verdict(&pc.coordination);
            for e in &pc.entries {
                println!(
                    "               issue #{} ({}) claimed by {} — {}s remaining",
                    e.issue, e.repo, e.host, e.remaining_ttl_secs
                );
            }
        }
        None => {
            println!(
                "Peer claims:   not configured (safehouse.enabled false, or older daemon \
                 binary — restart to pick up #5921)"
            );
        }
    }

    // Running-vs-disk build staleness (#5341): the answering daemon PROCESS's
    // own build, sourced over IPC, compared against THIS CLI invocation's own
    // compile-time build (which — because `status` always execs the on-disk
    // binary fresh — IS the disk build). Unflagged, in the same block as
    // Safehouse/Observability/Protection, so a stale-but-still-answering
    // daemon is visible without an operator having to compare process start
    // time against binary mtime by hand.
    println!(
        "{}",
        render_build_status_line(
            report.daemon_build_commit.as_deref(),
            report.daemon_built_at_raw.as_deref(),
            self_update::BUILT_COMMIT,
            self_update::BUILT_AT_RAW,
        )
    );

    // Telemetry export liveness (#5083): the positive counterpart to the
    // host-id mismatch WARNING printed further up. Before this, "exporting
    // fine", "observability disabled", and "configured but silently never
    // exported" were all rendered as the same thing — nothing — and telling
    // them apart meant grepping `daemon.log` for the *absence* of a warning.
    // Same shape as the Safehouse block above, for the same reason.
    println!(
        "{}",
        observability_line::render(report.observability_export.as_ref(), Utc::now())
    );
    // OTLP-export health (#11353): `no_exporter` / `failing` / `ok` / `exempt`.
    println!("{}", otlp_export_line::render(report.otlp_export.as_ref()));

    // The loud telemetry banner (#9950): the one-line render above is the
    // data; when telemetry is demonstrably broken the common-tool rule says
    // the status surface must COMPLAIN — what is broken, what it means
    // (records accumulating locally), and what to fix — on every invocation,
    // for as long as it lasts. The 2026-10-01 store-tunnel outage ran ~15h on
    // accurate-but-ignorable one-liners. The host-id mismatch has its own
    // WARNING block further up and is not duplicated here.
    if let Some(banner) = telemetry_banner::render(report.observability_export.as_ref(), Utc::now())
    {
        println!("\n{banner}");
    }

    // Forge event-feed consumer (ADR-0021, #8765). Same block, same reason:
    // "off", "never provisioned", "wrong key", "wrong host" and "quiet feed"
    // are five different answers that would otherwise all render as nothing.
    println!("{}", forge_events_line::render_block(report.forge_events.as_ref(), Utc::now()));
    // Fleet-store sync (#9596): what the last startup/timer pass found — the
    // drift the timer detects but deliberately does not write. Nothing at all
    // on a host with no `fleet.repo`.
    fleet_store_line::print(loom_daemon::fleet_sync::probe_status().as_ref());
    // Pending-restart marker (#9597): a restart-required `fleet-config
    // render` change this daemon's pid has not yet picked up. Nothing at all
    // once the daemon that saw the drift has restarted (or if none ever did).
    pending_restart_line::print(report.daemon_pid);

    // Watchdog protection state (#4354): this daemon is answering, so it is
    // alive — but is anything positioned to notice when it *stops* being? Before
    // this line an operator had to read `daemon-watchdog.log` or poke
    // `launchctl`/`systemctl` by hand to find out. Client-side, read-only, and
    // never fatal: `unknown` when the provisioning probe cannot answer, and this
    // line is simply omitted when no loom dir resolved at all.
    if let Some(p) = protection {
        println!("Protection:    {}", p.state.description());
        println!("               {}", p.detail);
        match p.state {
            daemon_install_state::ProtectionState::NoMarker => {
                // The #4331 state. A supervised daemon self-heals the marker at
                // startup, so seeing this on a supervised host means the marker
                // went away *after* boot — a restart re-arms it.
                println!(
                    "               Crash protection is DISARMED — the watchdog will log \
                     \"nothing to check\"."
                );
                // #5409 AC3: name a flag up front. If the still-installed
                // plist/unit had a loop ON before the marker was removed, a
                // bare re-start here now REFUSES (AC1) rather than silently
                // downgrading it — pass --work-finder / --health-gate to
                // restate the desired autonomy, or --from-config to drive it.
                println!(
                    "               Re-arm with a supervised restart (state the desired \
                     autonomy explicitly):"
                );
                println!("                 ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh --work-finder");
            }
            daemon_install_state::ProtectionState::WatchdogNotProvisioned => {
                println!("               Nothing is scheduled to detect a future daemon death.");
                println!(
                    "               Re-provision it (the start script installs the watchdog job):"
                );
                println!("                 ./.loom/scripts/cli/loom-daemon-start.sh");
            }
            // Protected / Unknown: no remediation — `unknown` is a probe
            // limitation on this host, not evidence of a fault, so steering the
            // operator into a restart for it would be the #4213 ghost-chase.
            _ => {}
        }

        // Marker-vs-non-autonomous-daemon mismatch (#4693): the sibling of the
        // #4069 ExpectedButDead state above, but for the REACHABLE case — the
        // daemon IS alive and answering (unlike ExpectedButDead, which only
        // fires when IPC is unreachable), yet its work-finder loop is OFF
        // while the marker records an operator's autonomy-desired intent. This
        // is exactly the 2026-07-30 incident's end state: `loom-daemon status`
        // reported a perfectly healthy, PROTECTED daemon (marker present,
        // watchdog provisioned) while dispatch had silently stopped, because
        // "protected" only ever asked "would we notice a DEATH", never "is
        // this live daemon actually dispatching". `report.work_finder_enabled`
        // is `None` only for a pre-#4693 daemon binary that never computed it
        // — that degrades to no claim, never a false positive.
        if autonomy_mismatch(Some(p), report) {
            println!();
            println!(
                "WARNING: autonomy-desired marker present, but the work finder is OFF on this"
            );
            println!("         running, reachable daemon (LOOM_WORK_FINDER=0/unset) — autonomous");
            println!("         dispatch is NOT happening, even though the daemon looks healthy.");
            println!(
                "         This is the #4693 silent-downgrade scenario (a plain \
                 loom-daemon-start.sh"
            );
            println!("         run can re-render FLAGS-OFF over a previously-autonomous host).");
            // #5409 AC2: this is no longer a cosmetic-only WARNING — it makes
            // this `status` invocation exit non-OK (see EXIT_AUTONOMY_MISMATCH
            // / `--json`'s `protection.autonomy_mismatch`), so a caller
            // scripting against the exit code (not just grepping this text)
            // also sees the mismatch.
            println!(
                "         This mismatch makes `loom-daemon status` exit non-zero \
                 (see --json"
            );
            println!("         protection.autonomy_mismatch).");
            println!("         Re-enable with:");
            println!("           ./.loom/scripts/cli/loom-daemon-start.sh --work-finder");
            println!("         or drive from config:");
            println!("           ./.loom/scripts/cli/loom-daemon-start.sh --from-config");
        }
    }

    // Scheduled drain-and-restart (#4090): a drain that quietly hangs is worse
    // than no drain, so surface DRAINING with the remaining count + deadline.
    // A `drain_note` (timeout refusal / abort) persists after a drain ends so
    // the operator sees WHY the daemon is still up rather than restarted.
    if report.draining {
        let deadline = report.drain_deadline.map_or_else(
            || "no deadline".to_string(),
            |d| {
                let secs = (d - Utc::now()).num_seconds();
                if secs >= 0 {
                    format!("deadline in {secs}s ({d})")
                } else {
                    format!("deadline passed {}s ago ({d})", -secs)
                }
            },
        );
        println!("Drain: DRAINING ({} sweep(s) remaining, {deadline})", report.in_flight.len());
        // #8514: the live roll state — how long dispatch has actually been
        // paused, against what budget, for which artifact. A host idling behind
        // a roll is now visible from one `status`, not only from a note that
        // happened to be written at the last transition.
        if let Some(line) = drain_render::roll_line(report) {
            println!("{line}");
        }
        // #6007: while a drain is ACTIVE the note is where a retained ("pending")
        // roll explains itself — a roll that already survived a deadline refusal
        // and re-armed must not read identically to a first-attempt drain.
        if let Some(note) = &report.drain_note {
            println!("       {note}");
        }
    } else if let Some(note) = &report.drain_note {
        println!("Drain: not draining (last: {note})");
    }
    if let Some(line) = drain_render::paused_by_day_line(report) {
        println!("{line}"); // #8652
    }

    // Live idle-exit eligibility (#5565): a one-line summary for an operator
    // eyeballing `status` on a fleet host, matching the same determination
    // the cron idle-shutdown guard now queries via `--json`. Silent when the
    // feature was never enabled this process (nothing to report).
    if let Some(ie) = &report.idle_exit {
        if ie.enabled {
            if ie.eligible {
                println!(
                    "Idle exit: ELIGIBLE ({}, idle {}m \u{2265} {}m)",
                    ie.trigger.as_deref().unwrap_or("idle"),
                    ie.idle_elapsed_secs / 60,
                    ie.idle_minutes
                );
            } else {
                println!(
                    "Idle exit: not eligible (in-flight {}, active roles {}, idle {}m / {}m)",
                    ie.in_flight_sweeps,
                    ie.active_role_runs,
                    ie.idle_elapsed_secs / 60,
                    ie.idle_minutes
                );
            }
        }
    }

    // GitHub rate-limit circuit breaker (#4429): one line while Closed, a
    // fuller block while cooling (the operator's first question is "when does
    // polling resume").
    if let Some(rl) = &report.rate_limit_breaker {
        if !rl.enabled {
            println!("GitHub rate limit: breaker disabled");
        } else if rl.suppressed {
            let releases = rl.cooldown_until.map_or_else(
                || "unknown".to_string(),
                |r| {
                    let secs = (r - Utc::now()).num_seconds();
                    if secs >= 0 {
                        format!("in {secs}s ({r})")
                    } else {
                        format!("overdue by {}s ({r})", -secs)
                    }
                },
            );
            let source = rl.source.as_deref().unwrap_or("unknown");
            println!(
                "GitHub rate limit: COOLDOWN — forge polling paused (tripped by {source}), \
                 resumes {releases}"
            );
            if let (Some(core), Some(gql)) = (rl.core_remaining, rl.graphql_remaining) {
                println!("  last probed budget: core {core} remaining, graphql {gql} remaining");
            }
        } else if rl.trips_total > 0 {
            println!(
                "GitHub rate limit: OK (breaker closed; {} trip(s) this daemon lifetime)",
                rl.trips_total
            );
        } else {
            println!("GitHub rate limit: OK (breaker closed)");
        }
    }

    // Per-caller forge call accounting (#9251), right under the breaker.
    for line in forge_calls_render::render_forge_calls_lines(report, Utc::now()) {
        println!("{line}");
    }

    // Host-level role runner header (#6470): a single line naming the
    // `LOOM_ROLE_RUNNER` env master-switch state, printed above the per-root
    // table so a reader does not have to scan every ROLES column (or worse,
    // every root's own `.loom/config.json`) to learn a host-wide env
    // override is in play.
    println!("{}", render_role_runner_host_header_line(report));

    // Host-level sharding posture (#6374), printed immediately below the
    // env-override line: the two answer the coarse and fine halves of the
    // same question ("does this host run role ticks at all?" then "which
    // workspaces' role ticks does it run?"), so reading them apart would be
    // misleading. Suppressed entirely when the daemon never reported one
    // (pre-#6374 binary) or when nothing is configured — an unsharded
    // single-host install should not grow a line about a feature it does not
    // use.
    if let Some(line) = render_role_runner_shard_header_line(report) {
        println!("{line}");
    }
    // Roster section (Issue #7690, Phase A of #6704) — printed immediately
    // below the sharding header, when the daemon has one to show. `None`
    // (nothing printed) whenever the roster is disabled (the default) or has
    // never completed a heartbeat cycle, so a default install's `status`
    // output is byte-identical to pre-#7690.
    for line in render_roster_lines(report) {
        println!("{line}");
    }

    // Per-repo breakdown across every registered managed workspace (#3930). In
    // the common single-workspace case this is one line for the daemon's own
    // workspace; with `loom-daemon workspace add <path>` it lists every managed
    // repo, its in-flight count, and its own gate state.
    println!(
        "\nManaged repos: {} (weight: relative share of repo draws, higher = more)",
        report.per_repo.len()
    );
    if report.per_repo.is_empty() {
        println!("  (none)");
    } else {
        // The gate column is named `HEALTH-GATE`, not `GATE` (#8288): the bare
        // noun invited the reading "this repo's dispatch gate", so an `off`
        // there looked like dispatch being switched off for the workspace.
        // Naming the feature it reports on — the main-health gate — keeps the
        // column's own value about that feature. `HEALTH-GATE` is 11 chars, so
        // it still fits the existing 13-wide column and the row width (and thus
        // the separator below) is unchanged.
        println!(
            "  {:>4}  {:>9}  {:<13}  {:<5}  REPO",
            "WGHT", "IN-FLIGHT", "HEALTH-GATE", "ROLES"
        );
        println!("  {:-<68}", "");
        for r in &report.per_repo {
            // Same classification as the top-level summary above, condensed
            // for the table column (#3950 AC3, widened #4012).
            let verdict = classify_gate_verdict(
                r.health_gate_enabled,
                r.health_gate_halted,
                r.health_gate_not_evaluated,
                r.health_gate_deferred,
                r.health_gate_not_evaluated_reason.as_deref(),
                r.health_gate_deferred_reason.as_deref(),
                r.health_gate_verdict_tier.as_deref(),
                r.health_gate_verdict_at,
            );
            let gate = gate_column_label(r.sweep_command_missing, &verdict);
            // Per-root role-runner enablement (#4377) — resolved from this
            // root's OWN config, so it can legitimately read "off" even while
            // the daemon's own workspace has the loops running.
            let roles = if r.role_runner_enabled { "on" } else { "off" };
            println!(
                "  {:>4}  {:>9}  {:<13}  {:<5}  {}{}",
                r.weight,
                r.in_flight_count,
                gate,
                roles,
                r.root.display(),
                if r.root_missing {
                    "  [MISSING ROOT]"
                } else {
                    ""
                }
            );
            // Issue #4326: a dangling registry entry (root deleted without
            // `workspace remove`) — the work-finder already warns-and-skips
            // it on dispatch; this is the operator-facing pointer to clean it
            // up (or, if the root is only transiently unavailable, e.g. an
            // unmounted volume, to leave it registered).
            if let Some(mark) = &r.maintain_only {
                println!("        {}: kept current, never dispatched into (#11186)", mark.label());
            }
            if r.root_missing {
                println!(
                    "        root does not exist on disk — dispatch is skipped; \
                     run `loom-daemon workspace remove {}` if this is permanent",
                    r.root.display()
                );
            }
            // Issue #5682: name the reason behind the `HEALTH-GATE no-sweep` override
            // above so an operator reading past the table sees the fix, not
            // just the symptom.
            if r.sweep_command_missing {
                println!(
                    "        no /loom:sweep command installed \
                     (.claude/commands/loom/sweep.md missing) — dispatch is refused every \
                     tick; run `loom-daemon init {}` to fix",
                    r.root.display()
                );
            }
            // Name the failure class behind a not-evaluated repo (#3974 AC2) so
            // the operator can tell "dirty tree" from "cargo not on PATH".
            if let Some(reason) = &r.health_gate_not_evaluated_reason {
                println!("        gate not evaluated — {reason}");
            }
            // Load-aware deferral (#4259): name why the gate is deferring so a
            // repo whose gate is not producing verdicts under host load is
            // explained (distinct from the not-evaluated line above).
            if let Some(reason) = &r.health_gate_deferred_reason {
                println!("        gate deferred — {reason}");
            }
            // Insta-crash quarantine (#3939): list the issues this repo is
            // currently refusing to re-dispatch so a stalled-but-nonempty backlog
            // is explained. Auto-releases on a TTL (or `loom:blocked` removal).
            if !r.quarantined_issues.is_empty() {
                let list = r
                    .quarantined_issues
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("        quarantined (insta-crash, #3939): {list}");
            }
            // Fleet-wide quarantine-stash visibility (#5692): surface this
            // repo's `refs/stash` counts so an operator does not have to SSH
            // in and run `git stash list` per repo to notice accumulation
            // (#5690's fleet audit: 148 stashes across three hosts, found only
            // by hand). Silent when the repo has no stashes at all.
            if r.stash_total_count > 0 {
                let age = r
                    .stash_oldest_age_secs
                    .map(format_stash_age)
                    .unwrap_or_else(|| "unknown".to_string());
                println!(
                    "        stashes: {} total ({} loom-quarantine:), oldest {age} old",
                    r.stash_total_count, r.stash_quarantine_count
                );
                // #5512: of the non-quarantine entries, how many are NOT
                // presumed recoverable elsewhere (an operator pre-resync
                // stash, an agent's ad-hoc WIP, …) — the population that hid
                // the #6129 quiesce work behind a half-applied pop until it
                // broke `spawn-claude.sh` (#6162). Silent when there are
                // none.
                if r.stash_non_quarantine_unrecoverable_count > 0 {
                    let other_age = r
                        .stash_non_quarantine_unrecoverable_oldest_age_secs
                        .map(format_stash_age)
                        .unwrap_or_else(|| "unknown".to_string());
                    println!(
                        "          of which {} non-quarantine stash(es) are not presumed \
                         recoverable elsewhere, oldest {other_age} old",
                        r.stash_non_quarantine_unrecoverable_count
                    );
                }
            }
            // #4377/#6470: onIdle configured but the per-root gate is off is
            // exactly the silent no-op #4377 fixes — call it out explicitly
            // rather than requiring the operator to cross-check the ROLES
            // column against a separate onIdle listing. #6470: the message
            // must name the TRUE cause (see `render_role_runner_disabled_line`).
            if let Some(line) = render_role_runner_disabled_line(r) {
                println!("{line}");
            }
            // #6374: which host carries this workspace's role slice. Only
            // printed on a sharded host — see
            // `render_role_runner_shard_repo_line`.
            if let Some(line) = render_role_runner_shard_repo_line(r) {
                println!("{line}");
            }
            // #7511: age-since-last-tick + promoted state for every role
            // that is both `onIdle` and has a configured `onIdleMaxWait` —
            // see `render_role_runner_on_idle_promotion_lines`.
            for line in render_role_runner_on_idle_promotion_lines(r) {
                println!("{line}");
            }
        }
    }

    // Forge-side pipeline snapshot (#3977) — opt-in via `--pipeline`. Rendered
    // in the same order as the "Managed repos" table above (both iterate
    // `report.per_repo`), so the two tables line up row-for-row.
    if let Some(snapshots) = pipeline {
        println!("\nForge pipeline (per repo, --pipeline):");
        if snapshots.is_empty() {
            println!("  (none)");
        } else {
            println!(
                "  {:>6}  {:>8}  {:>6}  {:>7}  {:>5}  {:>9}  REPO",
                "QUEUED", "BUILDING", "REVIEW", "CHNG-RQ", "PR", "MERGED24H"
            );
            println!("  {:-<75}", "");
            for s in snapshots {
                use loom_daemon::pipeline_snapshot::format_count;
                println!(
                    "  {:>6}  {:>8}  {:>6}  {:>7}  {:>5}  {:>9}  {}",
                    format_count(s.queued),
                    format_count(s.building),
                    format_count(s.review_requested),
                    format_count(s.changes_requested),
                    format_count(s.approved),
                    format_count(s.merged_24h),
                    s.root.display()
                );
                if let Some(err) = &s.error {
                    println!("        forge query failed for one or more metrics ({err}) — unreachable fields shown as ?");
                }
            }
        }
    }

    println!("\nPer-token usage:");
    match token_usage {
        Some(value) => print_token_usage_table(value),
        None => println!(
            "  (unavailable — `loom-daemon tokens check --json` failed or the token pool is not bootstrapped)"
        ),
    }

    // Self-update staleness (#3968) — read-only, local-only. Never implies an
    // auto-restart; run `.loom/scripts/cli/loom-daemon-update.sh` to act on it.
    print!("\nSelf-update: built from {}", update.built_commit);
    match (update.source_commit.as_deref(), update.update_available) {
        (Some(source), Some(true)) => {
            // Staleness magnitude (Issue #6261) — how far behind, not just
            // whether. Prints "?" for a side that could not be computed
            // (e.g. `built_commit` unreachable in this checkout's history)
            // rather than silently dropping the whole line.
            let commits_str = update
                .commits_behind
                .map_or_else(|| "?".to_string(), |c| c.to_string());
            let hours_str = update
                .hours_behind
                .map_or_else(|| "?".to_string(), |h| h.to_string());
            println!(
                " — UPDATE AVAILABLE (source checkout HEAD is {source}, {commits_str} commit(s) \
                 / {hours_str}h behind); run `./.loom/scripts/cli/loom-daemon-update.sh` to \
                 rebuild + provision + restart"
            );
            // Issue #6261: the 2026-08-14 incident's diagnostic gap — a
            // staleness surface that WARNS once the magnitude crosses a
            // threshold, instead of only ever printing the same quiet
            // "UPDATE AVAILABLE" hint whether it has been 5 minutes or 20
            // hours.
            if let Some(warning) =
                self_update::staleness_warning_default(update.commits_behind, update.hours_behind)
            {
                println!("  WARNING: {warning}");
            }
        }
        (Some(source), Some(false)) => println!(" — up to date with source HEAD ({source})"),
        _ => println!(" (source checkout not found on this machine; staleness unknown)"),
    }

    // Autonomous self-update loop (#4055) — the daemon-side loop that acts on the
    // staleness above. Only rendered when enabled (opt-in); otherwise silent.
    if report.auto_update_enabled {
        // #10954: the mode says why the loop runs (e.g. for the fleet floor alone).
        print!("Auto-update loop: {}", report.auto_update_mode.as_deref().unwrap_or("enabled"));
        match &report.auto_update_last_check {
            Some(ts) => print!(" (last check {})", ts.format("%Y-%m-%dT%H:%M:%SZ")),
            None => print!(" (no check yet)"),
        }
        if let Some(ts) = &report.auto_update_last_roll {
            print!(", last roll {}", ts.format("%Y-%m-%dT%H:%M:%SZ"));
        }
        // Issue #7609: what the artifact-first tick most recently saw as
        // available to roll onto — the fleet-visible answer to "is a newer
        // signed binary published for this platform?".
        if let Some(version) = &report.auto_update_artifact_version {
            print!(", artifact available {version}");
            if let Some(published) = &report.auto_update_artifact_published_at {
                print!(" (published {published})");
            }
        }
        if let Some(reason) = &report.auto_update_terminal_reason {
            print!(" — TERMINAL: {reason}");
        } else if let Some(secs) = report.auto_update_backoff_secs {
            print!(
                " — backing off {secs}s after {} consecutive failure(s)",
                report.auto_update_consecutive_failures
            );
        }
        println!();
        if let Some(why) = report.auto_update_floor_not_rolling {
            println!("  Below the fleet floor, NOT rolling ({}): {}", why.as_str(), why.describe());
        }
        last_tick_line::print(report);
    }
    host_update_line::print(report); // #12420
    task_liveness_line::print(&report.task_liveness);
    session_containers_line::print(report.session_containers.as_ref());

    println!();
}

/// Render the `loom-daemon tokens check --json` report (`{ "accounts": [ { name,
/// status, 5h_utilization, 7d_utilization, 7d_reset } ] }`) as a small table.
/// Falls back to pretty-printed JSON if the shape is unexpected.
fn print_token_usage_table(value: &serde_json::Value) {
    let Some(accounts) = value.get("accounts").and_then(serde_json::Value::as_array) else {
        // Unexpected shape — surface the raw JSON rather than dropping it.
        if let Ok(pretty) = serde_json::to_string_pretty(value) {
            for line in pretty.lines() {
                println!("  {line}");
            }
        }
        return;
    };

    if accounts.is_empty() {
        println!("  (no accounts probed)");
        return;
    }

    println!("  {:<22} {:<14} {:>8} {:>8}", "ACCOUNT", "STATUS", "5h", "7d");
    println!("  {:-<54}", "");
    for acct in accounts {
        let name = acct
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let raw_status = acct
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let util_7d = acct
            .get("7d_utilization")
            .and_then(serde_json::Value::as_f64);
        // Apply the same near-ceiling override the summary uses so a 99%-7d
        // `available` row never renders `available` here (#3936).
        let status = loom_daemon::capacity::effective_probe_status(raw_status, util_7d);
        let fmt_pct = |key: &str| {
            acct.get(key)
                .and_then(serde_json::Value::as_f64)
                .map_or_else(|| "-".to_string(), |u| format!("{:.0}%", u * 100.0))
        };
        println!(
            "  {:<22} {:<14} {:>8} {:>8}",
            name,
            status,
            fmt_pct("5h_utilization"),
            fmt_pct("7d_utilization")
        );
    }
}

#[cfg(test)]
mod status_render_tests;

#[cfg(test)]
mod token_starvation_render_tests;

#[cfg(test)]
mod in_flight_repo_column_tests;

#[cfg(test)]
mod containment_column_tests;

#[cfg(test)]
mod status_protection_tests;

#[cfg(test)]
mod role_agent_render_tests;

#[cfg(test)]
mod journal_adoption_render_tests;

#[cfg(test)]
mod stash_status_render_tests;

#[cfg(test)]
mod worktree_footprint_render_tests;

#[cfg(test)]
mod role_runner_diagnostic_source_render_tests;

#[cfg(test)]
mod role_runner_shard_render_tests;

#[cfg(test)]
mod roster_render_tests;

#[cfg(test)]
mod preflight_advisory_render_tests;
