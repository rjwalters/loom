//! Status-payload rendering shared by `loom-daemon status` and `fleet
//! status` (Issue #4712 — split out of `main.rs`): capacity resolution,
//! main-health-gate verdict classification, and the JSON/human table
//! builders. `cli::status` owns the IPC round-trip and the two command
//! entry points that call into this module.

use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::Path;

use loom_daemon::daemon_install_state;
use loom_daemon::self_update;
use loom_daemon::types::{DaemonStatusReport, SweepKind};

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
    /// from what `min(disk, ram, configured_max)` actually evaluates to,
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
            // `dynamic_cap` — `min(disk, ram, configured_max)` — is the sole
            // authority for the cap. `token_bound`, however, is a starvation
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
pub(crate) fn build_status_json_value(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
    protection: Option<&daemon_install_state::ProtectionReport>,
) -> serde_json::Value {
    let rc = resolve_capacity(report, token_usage);
    serde_json::json!({
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
        // "Currently binding" vs "smallest ceiling" (#4031): the cap only binds
        // once in-flight reaches it. `false` ⇒ the limiter is work availability,
        // not any resource term, so scripted consumers don't misread the
        // token/CPU ceiling as a bottleneck at low occupancy.
        "capacity_bound": report.capacity_bound,
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
        "dynamic_cap": {
            "token_pool_size": report.token_pool_size,
            // The directory the daemon resolved for the pool above (#4292) —
            // `null` only from a pre-#4292 daemon binary that never computed
            // one. Lets an operator confirm at a glance which of the
            // per-repo/shared pools is actually in effect, independent of
            // whatever cwd `loom-daemon status` itself was run from.
            "token_pool_dir": report.token_pool_dir,
            "disk_headroom": report.disk_headroom,
            // RAM headroom (#5270) — the second machine-headroom cap term
            // alongside disk_headroom, since the token axis stopped bounding
            // admission on any auth path.
            "ram_headroom": report.ram_headroom,
            // Host CPU OBSERVATIONS (#3978/#4031), not cap terms: #4512 removed
            // the CPU headroom term from admission. Reported because observed
            // idle is the evidence for tuning `configured_max` on this machine.
            // The separate CPU saturation admission brake (#4903, retuned by
            // #5270) DOES gate admission — see the `admission_brake` block
            // below for its live held/threshold state.
            "logical_cpus": report.logical_cpus,
            "loadavg_1m": report.loadavg_1m,
            "cpu_idle_fraction": report.cpu_idle_fraction,
            "configured_max": report.configured_max,
            "per_token_concurrency": report.per_token_concurrency.max(1),
            "token_axis_effective": rc.token_axis_limit.saturating_mul(report.per_token_concurrency.max(1)),
            "effective": rc.effective_cap,
        },
        "capacity": {
            "source": rc.source,
            "ranking_present": rc.ranking_present,
            "total_accounts": rc.total,
            "healthy_accounts": rc.healthy,
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
        "drain": {
            "draining": report.draining,
            "deadline": report.drain_deadline,
            "note": report.drain_note,
        },
        // Per-repo breakdown across every registered managed workspace (#3930).
        "per_repo": report.per_repo.iter().map(|r| serde_json::json!({
            "root": r.root,
            "priority": r.priority,
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
            // Fleet-wide quarantine-stash visibility (#5692): per-repo
            // `refs/stash` counts, aggregated by
            // `quarantine_stash_status::collect_stash_summary`. Builds on
            // `check-quarantine-stashes.sh`'s (#5185) single-repo enumeration
            // rather than reinventing stash discovery.
            "stash": {
                "total_count": r.stash_total_count,
                "quarantine_count": r.stash_quarantine_count,
                "oldest_age_secs": r.stash_oldest_age_secs,
            },
        })).collect::<Vec<_>>(),
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
        "auto_update": {
            "enabled": report.auto_update_enabled,
            "last_check": report.auto_update_last_check,
            "last_roll": report.auto_update_last_roll,
            "consecutive_failures": report.auto_update_consecutive_failures,
            "backoff_secs": report.auto_update_backoff_secs,
            "terminal_reason": report.auto_update_terminal_reason,
            "note": report.auto_update_note,
        },
        // Host-distress circuit breaker (#4235) — `null` when no breaker is
        // registered (work-finder off / breaker disabled). Otherwise the current
        // phase (closed/open/cooldown), why it tripped, and the cool-down
        // release time so a scripted consumer sees a paused-dispatch host.
        "host_breaker": report.host_breaker.as_ref().map(|h| serde_json::json!({
            "enabled": h.enabled,
            "phase": h.phase,
            "suppressed": h.suppressed,
            "reason": h.reason,
            "tripped_at": h.tripped_at,
            "releases_at": h.releases_at,
            "last_load_per_core": h.last_load_per_core,
            "load_per_core_threshold": h.load_per_core_threshold,
            "sustain_ticks": h.sustain_ticks,
            "cooldown_secs": h.cooldown_secs,
        })),
        // Saturation admission brake (#4903) — `null` when no brake is
        // registered. `held` is the machine-readable form of "this host is
        // refusing new sweeps because it is already saturated", which
        // `capacity_bound` alone could never express.
        "admission_brake": report.admission_brake.as_ref().map(|b| serde_json::json!({
            "enabled": b.enabled,
            "held": b.held,
            "load_per_core": b.load_per_core,
            "load_per_core_threshold": b.load_per_core_threshold,
            "held_since": b.held_since,
            "held_ticks": b.held_ticks,
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
            "budget_probed_at": r.budget_probed_at,
        })),
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
    })
}

/// Emit the combined status (daemon report + per-token usage) as JSON.
pub(crate) fn print_status_json(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
    protection: Option<&daemon_install_state::ProtectionReport>,
) -> Result<()> {
    let combined = build_status_json_value(report, token_usage, update, pipeline, protection);
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

/// Render `verdict` as a short label for the per-repo table's 13-char `GATE`
/// column (#4012) — the short-form counterpart to [`format_gate_status`].
fn gate_status_short_label(verdict: &GateVerdict) -> &'static str {
    match verdict {
        GateVerdict::Disabled => "disabled",
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

/// The capacity-block line that **replaces** "the limiter is work availability"
/// while the saturation admission brake is holding (#4903).
///
/// `None` when no brake is registered, it is disabled, or it is not currently
/// holding — in which case the caller prints the generic line unchanged, so a
/// healthy host's output is byte-for-byte what it was before #4903.
///
/// Split out from [`print_status_human`] (same rationale as
/// [`render_in_flight_table`]) so the "a saturated host must not read as idle"
/// contract is unit-testable without capturing process stdout.
fn saturation_hold_note(report: &DaemonStatusReport, dispatch_cap: usize) -> Option<String> {
    let b = report.admission_brake.as_ref()?;
    if !(b.enabled && b.held) {
        return None;
    }
    let load = b
        .load_per_core
        .map_or_else(|| "n/a".to_string(), |l| format!("{l:.2}"));
    Some(format!(
        "  \u{26a0} ADMISSION BRAKE HOLDING: {} in flight, cap {dispatch_cap}, but this host is \
         saturated (load/core {load} \u{2265} {:.2}) — the limiter is the HOST, not work \
         availability. New sweeps are held until load recovers; in-flight sweeps are untouched. \
         Tune `autonomous.workFinder.maxConcurrent` for this machine's workload (#4903).",
        report.in_flight.len(),
        b.load_per_core_threshold,
    ))
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
/// capturing process stdout, mirroring [`render_admission_brake_line`].
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

/// The `Observability: …` telemetry-export line (Issue #5083) — always
/// rendered, because the whole point is that "healthy" must be *stated*, not
/// inferred from the absence of a warning.
///
/// Split out of [`print_status_human`] so every state is unit-testable without
/// capturing process stdout, mirroring [`render_admission_brake_line`]. `now`
/// is passed in (rather than read here) so the tests are deterministic.
///
/// `status = None` means a pre-#5083 daemon binary that never computed one —
/// reported as `unknown`, never silently as `disabled`, which would be an
/// invented fact about a daemon that said nothing.
fn render_observability_line(
    status: Option<&loom_daemon::types::ObservabilityExportStatus>,
    now: DateTime<Utc>,
) -> String {
    use loom_daemon::health::format_window;
    use loom_daemon::types::ObservabilityExportState as State;

    let Some(s) = status else {
        return "Observability: unknown (older daemon binary — restart to pick up #5083)"
            .to_string();
    };
    let host = s.host_id.as_deref().unwrap_or("unknown-host");
    let endpoint = s.endpoint.as_deref().unwrap_or("(no endpoint)");
    let uptime = s
        .uptime_secs(now)
        .map_or_else(|| "?".to_string(), format_window);
    let last_success = s
        .last_success_age_secs(now)
        .map_or_else(|| "never".to_string(), |age| format!("{} ago", format_window(age)));
    // Re-derived rather than trusting the daemon-stamped `state`, so a status
    // payload that sat in a pipe for a while still reads correctly across the
    // grace boundary — `classify` is the single shared rule (`types.rs`).
    match s.classify(now) {
        State::Disabled => {
            "Observability: disabled (no telemetry export — set observability.enabled=true to opt in)"
                .to_string()
        }
        // Distinct from `Disabled` (Issue #5337): `enabled: true` but a
        // required piece of config could not be resolved. `endpoint` reflects
        // whatever DID resolve rather than a blanket "(no endpoint)", and the
        // detail names the offending path plus the underlying error.
        State::Misconfigured => format!(
            "Observability: MISCONFIGURED — enabled but not exporting → {endpoint}{}",
            s.last_failure_detail
                .as_deref()
                .map_or_else(String::new, |d| format!(" ({d})")),
        ),
        State::Starting => format!(
            "Observability: starting — exporter up {uptime} as host_id={host}, no batch acked yet \
             (first flush due within {}s) → {endpoint}",
            s.flush_interval_secs.unwrap_or(0)
        ),
        State::NeverExported => format!(
            "Observability: NEVER EXPORTED — running {uptime} as host_id={host} and no batch has \
             EVER been acked; telemetry is not reaching {endpoint}{}",
            s.last_failure_detail
                .as_deref()
                .map_or_else(String::new, |d| format!(" (last error: {d})")),
        ),
        State::Healthy => format!(
            "Observability: OK — last export {last_success}, {} record(s) as host_id={host} → {endpoint}",
            s.records_exported
        ),
        State::HostIdMismatch => format!(
            "Observability: HOST-ID MISMATCH — telemetry is landing under host_id={}, not {host} \
             (last export {last_success}, {} record(s)) → {endpoint}",
            s.ingest_host_id.as_deref().unwrap_or("unknown"),
            s.records_exported
        ),
        State::Failing => format!(
            "Observability: FAILING — {} consecutive failed flush(es) as host_id={host}, last \
             success {last_success} → {endpoint}{}",
            s.consecutive_failures,
            s.last_failure_detail
                .as_deref()
                .map_or_else(String::new, |d| format!(" (last error: {d})")),
        ),
        // Only reachable from a NEWER daemon reporting a state this build does
        // not know. Say so plainly rather than collapsing it into one of the
        // known states — the same "degrade legibly, never mislabel" posture the
        // Safehouse block takes for an unknown state string (#4464).
        State::Unrecognized => format!(
            "Observability: unrecognized state from a newer daemon binary (host_id={host}) — \
             upgrade this client to read it"
        ),
    }
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
/// without capturing process stdout, mirroring [`render_observability_line`].
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

/// The standalone `Admission brake: …` status line (#4903), or `None` when no
/// brake is registered (older daemon / never registered) — which renders
/// nothing at all, the zero-behavior-change baseline the host breaker's line
/// already uses.
fn render_admission_brake_line(report: &DaemonStatusReport) -> Option<String> {
    let b = report.admission_brake.as_ref()?;
    let load = b
        .load_per_core
        .map_or_else(|| "n/a".to_string(), |l| format!("{l:.2}"));
    if !b.enabled {
        return Some("Admission brake: disabled".to_string());
    }
    if !b.held {
        return Some(format!(
            "Admission brake: OK (load/core {load}, hold \u{2265} {:.2})",
            b.load_per_core_threshold
        ));
    }
    let since = b.held_since.map_or_else(
        || "unknown".to_string(),
        |s| {
            let secs = (Utc::now() - s).num_seconds().max(0);
            format!("{secs}s ago ({s})")
        },
    );
    Some(format!(
        "Admission brake: HOLDING — host saturated (load/core {load} \u{2265} {:.2}); NEW sweep \
         admissions held since {since} ({} tick(s)); in-flight sweeps are untouched and will drain",
        b.load_per_core_threshold, b.held_ticks
    ))
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
            "  {:<30} {:>7} {:>8}  {:<20} {:<16} PHASE",
            "SWEEP", "ISSUE", "PID", "TOKEN", "REPO"
        );
        let _ = writeln!(out, "  {:-<91}", "");
        for s in &report.in_flight {
            let issue = match &s.kind {
                SweepKind::Issue(n) => format!("#{n}"),
                SweepKind::PrSet(_) => "prs".to_string(),
            };
            let repo = format_repo_column(s.repo.as_deref());
            let phase = s.latest_phase.as_deref().unwrap_or("-");
            let _ = writeln!(
                out,
                "  {:<30} {:>7} {:>8}  {:<20} {:<16} {}",
                s.sweep_id, issue, s.pid, s.token_name, repo, phase
            );
        }
    }
    out
}

/// Emit the combined status as a human-readable table.
pub(crate) fn print_status_human(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
    protection: Option<&daemon_install_state::ProtectionReport>,
) {
    println!("\n=== Loom Autonomous Daemon Status ===\n");

    println!("In-flight sweeps: {}", report.in_flight.len());
    print!("{}", render_in_flight_table(report));

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

    let factor = report.per_token_concurrency.max(1);
    // #4344: `rc` prefers a fresh client-side probe when one succeeded, which
    // can legitimately show a *different* (usually fresher) number than what
    // the running daemon actually used for its own dispatch decision this
    // tick. `report.dynamic_cap` / `report.capacity.token_axis_limit` are that
    // daemon-side truth — the number dispatch decisions are actually gated
    // on — so the headline always names the daemon's own cap; the probe's
    // number is shown as a labeled secondary line only when it disagrees.
    let dispatch_cap = report.dynamic_cap;
    let dispatch_token_axis = report.capacity.token_axis_limit;
    println!("\nDynamic concurrency cap: {dispatch_cap}  (the number dispatch uses)");
    println!(
        "  = min(disk headroom {}, ram headroom {}, configured max {})",
        report.disk_headroom, report.ram_headroom, report.configured_max
    );
    println!(
        "  (healthy {} × per-token {} = {} is informational only — the token axis no longer \
         bounds the cap, #5270)",
        dispatch_token_axis,
        factor,
        dispatch_token_axis.saturating_mul(factor),
    );
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
    // auth path: `dynamic_cap` is `min(disk, ram, configured_max)` with no
    // token term. #5304 hard-coded this to `false` to reflect that, which
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
    // #4903: while the saturation admission brake is holding, "the limiter is
    // work availability" is flatly wrong and is the exact misread the issue was
    // filed on — a worker at 12× overcommit rendered as an idle host with nine
    // free slots. The limiter is the HOST. Print the brake's diagnosis in place
    // of the generic line (the same suppression shape #4386 uses for the
    // pre-flight tripwire), so an operator reading the capacity block top-to-
    // bottom cannot miss it.
    let saturation_note: Option<String> = saturation_hold_note(report, dispatch_cap);
    if rc.ranking_present {
        let src = if rc.source == "probe" {
            "live probe: loom-daemon tokens check --json"
        } else {
            "from .loom/tokens/.ranking"
        };
        println!(
            "  {}/{} accounts healthy, {} exhausted/near-ceiling ({src})",
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
                // In-flight is below the cap: nothing is binding. Naming tokens
                // (or any resource) as "the bottleneck" here is the #4031
                // defect — at, say, 1 in-flight against a cap of 7 the limiter
                // is simply how much ready work exists. Suppress the
                // token-bound diagnosis.
                //
                // #4386: while the pre-flight tripwire is active, this bare
                // "work availability" line is actively misleading — every
                // dispatch IS starting, it just dies within ~1s at
                // claude-wrapper pre-flight, which reads as "no work" rather
                // than "everything is crashing." The warning printed above
                // already names the real cause, so suppress this line rather
                // than let it stand uncontested.
                if let Some(note) = &saturation_note {
                    // #4903: the host, not work availability, is the limiter.
                    println!("{note}");
                } else if !report.preflight_advisory_active {
                    println!(
                        "  not capacity-bound ({} in flight, cap {dispatch_cap} — the limiter is \
                         work availability, not disk/RAM/CPU)",
                        report.in_flight.len(),
                    );
                }
            } else if rc.token_bound {
                // #5305: `token_bound` means genuine starvation (zero healthy
                // accounts) — since #5270 the concurrency cap itself is
                // unaffected (it's `min(disk, ram, configured_max)`, no token
                // term), but with zero healthy accounts every spawn will
                // still fail account selection at dispatch time.
                println!(
                    "  token-bound: NO healthy accounts — the concurrency cap is unaffected \
                     (#5270), but every spawn will fail account selection until capacity \
                     returns. Add accounts (~/.claude-monitor/accounts.env + `loom-daemon tokens \
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
            if let Some(note) = &saturation_note {
                // #4903: same substitution as the ranking-present branch above.
                println!("{note}");
            } else if !report.preflight_advisory_active {
                // #4386: same suppression as the ranking-present branch above —
                // the warning printed at the top of `status` already names the
                // real cause while the tripwire is active.
                println!(
                    "  not capacity-bound ({} in flight, cap {dispatch_cap} — the limiter is work \
                     availability, not tokens/disk/CPU)",
                    report.in_flight.len(),
                );
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
        render_observability_line(report.observability_export.as_ref(), Utc::now())
    );

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
    } else if let Some(note) = &report.drain_note {
        println!("Drain: not draining (last: {note})");
    }

    // Host-distress circuit breaker (#4235): surface the phase, why it tripped,
    // and when the cool-down releases so an operator sees a paused-dispatch host
    // and can tell it apart from a main-health halt or a drain. A Closed breaker
    // prints a one-line "OK" with its configured thresholds; an absent breaker
    // (work-finder off / disabled) prints nothing.
    if let Some(hb) = &report.host_breaker {
        let load = hb
            .last_load_per_core
            .map_or_else(|| "n/a".to_string(), |l| format!("{l:.2}"));
        match hb.phase.as_str() {
            "closed" => {
                if hb.enabled {
                    println!(
                        "Host breaker: OK (closed; load/core {load}, trip ≥ {:.2} for {} tick(s), cooldown {}s)",
                        hb.load_per_core_threshold, hb.sustain_ticks, hb.cooldown_secs
                    );
                } else {
                    println!("Host breaker: disabled");
                }
            }
            "open" => {
                println!(
                    "Host breaker: OPEN — new dispatch paused, running work draining ({})",
                    hb.reason.as_deref().unwrap_or("sustained host distress")
                );
                if let Some(t) = hb.tripped_at {
                    println!("  tripped at: {t}");
                }
            }
            "cooldown" => {
                let releases = hb.releases_at.map_or_else(
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
                println!(
                    "Host breaker: COOLING DOWN — dispatch paused, releases {releases} (load/core {load})"
                );
            }
            other => println!("Host breaker: {other}"),
        }
    }

    // Saturation admission brake (#4903): a host that is holding new admissions
    // because it is already saturated must SAY so. Before this line, a worker at
    // 12× overcommit rendered as "3 sweeps, not capacity-bound" — visually
    // indistinguishable from an idle host with free slots. Printed immediately
    // after the host breaker so the two load-aware guards read together, and
    // always stating that in-flight work is untouched (the operator's next
    // question).
    if let Some(line) = render_admission_brake_line(report) {
        println!("{line}");
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

    // Per-repo breakdown across every registered managed workspace (#3930). In
    // the common single-workspace case this is one line for the daemon's own
    // workspace; with `loom-daemon workspace add <path>` it lists every managed
    // repo, its in-flight count, and its own gate state.
    println!(
        "\nManaged repos: {} (priority: lower = higher dispatch priority)",
        report.per_repo.len()
    );
    if report.per_repo.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:>4}  {:>9}  {:<13}  {:<5}  REPO", "PRIO", "IN-FLIGHT", "GATE", "ROLES");
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
            let gate = gate_status_short_label(&verdict);
            // Per-root role-runner enablement (#4377) — resolved from this
            // root's OWN config, so it can legitimately read "off" even while
            // the daemon's own workspace has the loops running.
            let roles = if r.role_runner_enabled { "on" } else { "off" };
            println!(
                "  {:>4}  {:>9}  {:<13}  {:<5}  {}{}",
                r.priority,
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
            if r.root_missing {
                println!(
                    "        root does not exist on disk — dispatch is skipped; \
                     run `loom-daemon workspace remove {}` if this is permanent",
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
            }
            // #4377: onIdle configured but the per-root gate is off is
            // exactly the silent no-op this issue fixes — call it out
            // explicitly rather than requiring the operator to cross-check
            // the ROLES column against a separate onIdle listing.
            if !r.role_runner_enabled && !r.role_runner_on_idle_roles.is_empty() {
                let list = r.role_runner_on_idle_roles.join(", ");
                println!(
                    "        role runner disabled for this root but onIdle=[{list}] is \
                     configured — these roles will never fire until \
                     autonomous.roleRunner.enabled=true is set in this root's own \
                     .loom/config.json (#4377)"
                );
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
        (Some(source), Some(true)) => println!(
            " — UPDATE AVAILABLE (source checkout HEAD is {source}); run \
             `./.loom/scripts/cli/loom-daemon-update.sh` to rebuild + provision + restart"
        ),
        (Some(source), Some(false)) => println!(" — up to date with source HEAD ({source})"),
        _ => println!(" (source checkout not found on this machine; staleness unknown)"),
    }

    // Autonomous self-update loop (#4055) — the daemon-side loop that acts on the
    // staleness above. Only rendered when enabled (opt-in); otherwise silent.
    if report.auto_update_enabled {
        print!("Auto-update loop: enabled");
        match &report.auto_update_last_check {
            Some(ts) => print!(" (last check {})", ts.format("%Y-%m-%dT%H:%M:%SZ")),
            None => print!(" (no check yet)"),
        }
        if let Some(ts) = &report.auto_update_last_roll {
            print!(", last roll {}", ts.format("%Y-%m-%dT%H:%M:%SZ"));
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
        if let Some(note) = &report.auto_update_note {
            println!("  last tick: {note}");
        }
    }

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
mod status_render_tests {
    use super::*;

    fn classify(
        enabled: Option<bool>,
        halted: bool,
        not_evaluated: bool,
        reason: Option<&str>,
        verdict_at: Option<DateTime<Utc>>,
    ) -> GateVerdict {
        // The 5-arg helper keeps the pre-#4259 test call sites unchanged: no
        // deferral, no tier label. Dedicated tests below exercise those paths
        // by calling `classify_gate_verdict` directly.
        classify_gate_verdict(enabled, halted, not_evaluated, false, reason, None, None, verdict_at)
    }

    #[test]
    fn format_gate_status_names_the_actual_not_evaluated_cause() {
        // Pre-#3974 this line asserted "workspace tree is dirty" for EVERY
        // skip, so a `git fetch` failure on a completely clean tree was
        // reported as a dirty tree. The cause is now passed through verbatim.
        let v = classify(
            Some(true),
            false,
            true,
            Some("git-failure: `git -C /repo fetch origin main` failed (exit 128)"),
            None,
        );
        let s = format_gate_status(&v);
        assert!(s.contains("git-failure"), "got: {s}");
        assert!(s.contains("exit 128"), "got: {s}");
        assert!(!s.contains("dirty"), "must not assume a dirty tree: {s}");
        assert!(s.contains("NOT evidence about"), "got: {s}");
        assert!(s.contains("NOT halted"), "an unevaluated gate does not halt: {s}");

        // A dirty tree still reads as a dirty tree — because the gate said so.
        let v = classify(Some(true), false, true, Some("dirty-tree: [ M src/main.rs]"), None);
        let s = format_gate_status(&v);
        assert!(s.contains("dirty-tree"), "got: {s}");
        assert!(s.contains("src/main.rs"), "got: {s}");
    }

    #[test]
    fn format_gate_status_covers_all_halted_and_not_evaluated_states() {
        let clear = classify(Some(true), false, false, None, Some(Utc::now()));
        assert!(format_gate_status(&clear).starts_with("clear (dispatch allowed"));

        let halted = classify(Some(true), true, false, None, None);
        let s = format_gate_status(&halted);
        assert!(s.starts_with("HALTED"), "got: {s}");
        assert!(s.contains("verified red"), "got: {s}");

        // Both at once: a prior verified-red halt persists while the next tick
        // cannot evaluate.
        let both = classify(Some(true), true, true, Some("timeout: gate command timed out"), None);
        let s = format_gate_status(&both);
        assert!(s.contains("HALTED"), "got: {s}");
        assert!(s.contains("NOT EVALUATED"), "got: {s}");
        assert!(s.contains("timeout"), "got: {s}");

        // A missing cause degrades gracefully rather than inventing one.
        let no_cause = classify(Some(true), false, true, None, None);
        let s = format_gate_status(&no_cause);
        assert!(s.contains("cause unrecorded"), "got: {s}");
    }

    /// #4012: the core regression this issue fixes — a fresh, enabled gate
    /// that has never completed an evaluation must render distinctly from a
    /// verified-green gate, even though both allow dispatch (`halted: false`,
    /// `not_evaluated: false` in both cases pre-#4012).
    #[test]
    fn format_gate_status_distinguishes_pending_from_clear() {
        let pending = classify(Some(true), false, false, None, None);
        assert_eq!(pending, GateVerdict::Pending);
        let s = format_gate_status(&pending);
        assert!(s.starts_with("pending"), "got: {s}");
        assert!(s.contains("dispatch allowed"), "got: {s}");
        assert!(!s.contains("clear"), "must not read as verified-green: {s}");

        let now = Utc::now();
        let clear = classify(Some(true), false, false, None, Some(now));
        assert_eq!(
            clear,
            GateVerdict::Clear {
                since: Some(now),
                tier: None
            }
        );
        let s = format_gate_status(&clear);
        assert!(s.starts_with("clear"), "got: {s}");
        assert!(s.contains(&now.to_rfc3339()), "clear must carry its own recency evidence: {s}");
        assert_ne!(
            format_gate_status(&pending),
            format_gate_status(&clear),
            "pending and clear must never render identically"
        );
    }

    /// #4259: a load-deferral is a distinct verdict — it must render as
    /// `deferred (…)`, never as `not evaluated (timeout …)` nor as a stale
    /// `clear`, and it must never halt dispatch.
    #[test]
    fn format_gate_status_deferred_is_distinct_and_never_halts() {
        let deferred = classify_gate_verdict(
            Some(true),
            false, // not halted
            false, // not unevaluated
            true,  // deferred
            None,
            Some("load 1.05/core for 14m — fast tier runs at the 30m bound"),
            None,
            None,
        );
        assert!(matches!(deferred, GateVerdict::Deferred { .. }));
        let s = format_gate_status(&deferred);
        assert!(s.starts_with("deferred"), "got: {s}");
        assert!(s.contains("load 1.05/core"), "carries the load reason: {s}");
        assert!(s.contains("NOT evidence about main"), "got: {s}");
        assert!(s.contains("NOT halted"), "a deferred gate does not halt: {s}");
        // Distinct from a timeout not-evaluated line for the same host stress.
        let timeout = classify(
            Some(true),
            false,
            true,
            Some("timeout: gate command timed out after 1200s"),
            None,
        );
        assert_ne!(
            format_gate_status(&deferred),
            format_gate_status(&timeout),
            "deferred (load) must never render identically to not-evaluated (timeout)"
        );
        assert_eq!(gate_status_short_label(&deferred), "deferred");
    }

    /// #4259: a fast-tier green must be labeled so it is never mistaken for a
    /// full-suite green.
    #[test]
    fn format_gate_status_fast_tier_clear_is_labeled() {
        let now = Utc::now();
        let full = classify_gate_verdict(
            Some(true),
            false,
            false,
            false,
            None,
            None,
            Some("full"),
            Some(now),
        );
        let fast = classify_gate_verdict(
            Some(true),
            false,
            false,
            false,
            None,
            None,
            Some("fast"),
            Some(now),
        );
        let full_s = format_gate_status(&full);
        let fast_s = format_gate_status(&fast);
        assert!(full_s.starts_with("clear"), "got: {full_s}");
        assert!(!full_s.contains("fast tier"), "full tier is unlabeled: {full_s}");
        assert!(fast_s.contains("fast tier"), "fast tier is labeled: {fast_s}");
        assert!(
            fast_s.contains("NOT a full-suite green"),
            "the fast-tier caveat is explicit: {fast_s}"
        );
        assert_eq!(gate_status_short_label(&full), "clear");
        assert_eq!(gate_status_short_label(&fast), "clear(fast)");
        // The short label still fits the 13-char table column.
        assert!(gate_status_short_label(&fast).len() <= 13);
    }

    /// #4012 AC2: the gate-disabled case must be distinguishable from both
    /// `pending` and `clear`.
    #[test]
    fn format_gate_status_distinguishes_disabled() {
        let disabled = classify(Some(false), false, false, None, None);
        assert_eq!(disabled, GateVerdict::Disabled);
        let s = format_gate_status(&disabled);
        assert!(s.starts_with("disabled"), "got: {s}");
        assert!(s.contains("dispatch allowed"), "got: {s}");

        let pending = classify(Some(true), false, false, None, None);
        assert_ne!(
            format_gate_status(&disabled),
            format_gate_status(&pending),
            "disabled and pending must never render identically"
        );

        // A disabled root that (implausibly) still carries a stale verdict
        // timestamp from before it was turned off still reports `Disabled` —
        // the enabled flag takes priority over verdict presence.
        let disabled_with_stale_verdict =
            classify(Some(false), false, false, None, Some(Utc::now()));
        assert_eq!(disabled_with_stale_verdict, GateVerdict::Disabled);
    }

    /// #4012 AC3: `pending` and `disabled` both still allow dispatch —
    /// observability-only, no new halt path.
    #[test]
    fn pending_and_disabled_never_halt() {
        for verdict in [
            classify(Some(true), false, false, None, None),
            classify(Some(false), false, false, None, None),
        ] {
            assert!(
                !matches!(verdict, GateVerdict::Halted { .. }),
                "{verdict:?} must never be classified as halted"
            );
            let s = format_gate_status(&verdict);
            assert!(s.contains("dispatch allowed"), "got: {s}");
        }
    }

    /// An older daemon that never populated `main_health_gate_enabled` (wire
    /// field absent ⇒ `None`, #4012) must not be misread as "disabled" — that
    /// is exactly the `bool::default() == false` trap the `Option<bool>` wire
    /// type exists to avoid.
    #[test]
    fn format_gate_status_legacy_none_enabled_is_not_disabled() {
        let v = classify(None, false, false, None, None);
        assert_ne!(v, GateVerdict::Disabled);
        // With no verdict either, it reads as pending (dispatch allowed) —
        // the conservative reading, never a fabricated "clear".
        assert_eq!(v, GateVerdict::Pending);
    }

    /// `halted`/`not_evaluated` always win over disabled/pending, matching the
    /// gate loop's own soft-fail contract (its disabled path always clears
    /// `halted` first) — this combination should only ever arise from a test
    /// poking the raw state directly, and the renderer must still surface it
    /// as halted rather than silently downgrading to "disabled".
    #[test]
    fn format_gate_status_halted_beats_disabled_and_pending() {
        let v = classify(Some(false), true, false, None, None);
        assert!(matches!(
            v,
            GateVerdict::Halted {
                not_evaluated: false,
                ..
            }
        ));
    }

    #[test]
    fn gate_status_short_label_fits_table_width_and_matches_long_form() {
        let cases = [
            classify(Some(false), false, false, None, None),
            classify(Some(true), false, false, None, None),
            classify(Some(true), false, false, None, Some(Utc::now())),
            classify(Some(true), false, true, Some("timeout"), None),
            classify(Some(true), true, false, None, None),
            classify(Some(true), true, true, Some("timeout"), None),
        ];
        for v in cases {
            let short = gate_status_short_label(&v);
            assert!(short.len() <= 13, "{short:?} exceeds the 13-char GATE column");
        }
        // The short label and long form must agree on the halted/not distinction.
        let halted = classify(Some(true), true, false, None, None);
        assert_eq!(gate_status_short_label(&halted), "HALTED");
        assert!(format_gate_status(&halted).starts_with("HALTED"));
    }
}

#[cfg(test)]
mod token_starvation_render_tests {
    //! #5305: PR #5304 removed the token axis from the admission cap (#5270)
    //! but, in the process, over-removed two *signals* rather than
    //! re-scoping them: `resolve_capacity`'s fresh-probe branch hardcoded
    //! `token_bound: false`, and the #4344 ranking-divergence detection
    //! (`ranking_diverges_from_starvation`, née `dispatch_starved_but_disagrees`)
    //! was hardcoded to `false` outright. Both must be reachable again —
    //! `token_bound` meaning genuine starvation (zero healthy accounts), not
    //! "tokens bind the cap."
    use super::{ranking_diverges_from_starvation, resolve_capacity};
    use crate::cli::status::status_client_tests::sample_report;
    use loom_daemon::types::CapacityReport;

    #[test]
    fn resolve_capacity_probe_tier_reports_token_bound_on_zero_healthy() {
        let report = sample_report();
        let usage = serde_json::json!({
            "accounts": [
                {"status": "exhausted", "7d_utilization": 0.99},
                {"status": "blocked", "7d_utilization": 0.99},
            ]
        });
        let rc = resolve_capacity(&report, Some(&usage));
        assert_eq!(rc.source, "probe");
        assert_eq!(rc.healthy, 0);
        assert!(
            rc.token_bound,
            "a fresh probe with 0 healthy accounts must report starvation, \
             not the #5304 hardcoded `false`"
        );
    }

    #[test]
    fn resolve_capacity_probe_tier_not_token_bound_when_any_account_healthy() {
        let report = sample_report();
        let usage = serde_json::json!({
            "accounts": [
                {"status": "available", "7d_utilization": 0.1},
                {"status": "exhausted", "7d_utilization": 0.99},
            ]
        });
        let rc = resolve_capacity(&report, Some(&usage));
        assert_eq!(rc.source, "probe");
        assert_eq!(rc.healthy, 1);
        assert!(!rc.token_bound, "one healthy account remains ⇒ not starved");
    }

    #[test]
    fn ranking_diverges_when_daemons_own_ranking_is_starved_but_probe_is_fresh() {
        let mut report = sample_report();
        report.capacity = CapacityReport {
            ranking_present: true,
            total_accounts: 3,
            healthy_accounts: 0,
            exhausted_accounts: 3,
            token_axis_limit: 0,
            token_bound: true,
        };
        let usage = serde_json::json!({
            "accounts": [
                {"status": "available", "7d_utilization": 0.1},
                {"status": "exhausted", "7d_utilization": 0.99},
                {"status": "exhausted", "7d_utilization": 0.99},
            ]
        });
        let rc = resolve_capacity(&report, Some(&usage));
        assert_eq!(rc.healthy, 1, "the fresher probe sees one healthy account");
        assert!(
            ranking_diverges_from_starvation(&report, &rc),
            "daemon's own ranking reads 0 healthy while a fresher probe disagrees"
        );
    }

    #[test]
    fn ranking_does_not_diverge_when_both_agree_or_daemon_has_capacity() {
        // Daemon's own ranking already shows healthy accounts ⇒ no divergence
        // to report, regardless of what a fresh probe shows.
        let mut report = sample_report();
        report.capacity = CapacityReport {
            ranking_present: true,
            total_accounts: 3,
            healthy_accounts: 2,
            exhausted_accounts: 1,
            token_axis_limit: 2,
            token_bound: false,
        };
        let usage = serde_json::json!({
            "accounts": [
                {"status": "available", "7d_utilization": 0.1},
                {"status": "available", "7d_utilization": 0.1},
            ]
        });
        let rc = resolve_capacity(&report, Some(&usage));
        assert!(!ranking_diverges_from_starvation(&report, &rc));

        // Both the daemon's own ranking AND the fresh probe agree on
        // starvation ⇒ no *disagreement* to headline (the plain "NO healthy
        // accounts" guidance branch handles this case instead).
        let mut starved_report = sample_report();
        starved_report.capacity = CapacityReport {
            ranking_present: true,
            total_accounts: 3,
            healthy_accounts: 0,
            exhausted_accounts: 3,
            token_axis_limit: 0,
            token_bound: true,
        };
        let starved_usage = serde_json::json!({
            "accounts": [
                {"status": "exhausted", "7d_utilization": 0.99},
                {"status": "exhausted", "7d_utilization": 0.99},
            ]
        });
        let rc = resolve_capacity(&starved_report, Some(&starved_usage));
        assert!(!ranking_diverges_from_starvation(&starved_report, &rc));
    }
}

#[cfg(test)]
mod in_flight_repo_column_tests {
    //! The in-flight table's `REPO` column (#4698): five different managed
    //! repos' small issue numbers (each repo's own `#3`/`#4`/`#6`) used to
    //! look like duplicate same-issue dispatches with no way to disambiguate
    //! them from `loom-daemon status` output alone, even though `SweepInfo`
    //! already carried the owning workspace root (`repo`, Issue #3929).
    use super::{format_repo_column, render_in_flight_table};
    use crate::cli::status::status_client_tests::sample_report;
    use chrono::Utc;
    use loom_daemon::types::{SweepInfo, SweepKind, SweepState};
    use std::path::PathBuf;

    /// A minimal in-flight entry for a given issue/repo pair, mirroring the
    /// `mk()` helper pattern used by `sweep_registry.rs`'s own tests.
    fn mk(issue: u32, repo: Option<&str>) -> SweepInfo {
        SweepInfo {
            pgid: None,
            sweep_id: format!("s{issue}"),
            kind: SweepKind::Issue(issue),
            pid: 4242,
            token_name: "agent-1.token".to_string(),
            runtime: "claude".to_string(),
            runtime_source: None,
            log_path: PathBuf::from(format!(".loom/logs/sweep-issue-{issue}.log")),
            idempotency_key: None,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: Some("builder".to_string()),
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: repo.map(str::to_string),
        }
    }

    #[test]
    fn format_repo_column_takes_path_basename() {
        assert_eq!(format_repo_column(Some("/repos/loom")), "loom");
        assert_eq!(format_repo_column(Some("/home/user/gf180-canary-3")), "gf180-canary-3");
    }

    #[test]
    fn format_repo_column_leaves_bare_value_as_is() {
        // A bare `owner/repo` slug (e.g. from `loom-daemon/src/ipc.rs`'s
        // `"rjwalters/vibesql"`) has no meaningful "directory" component
        // beyond its own last segment — `Path::file_name` already reduces it
        // to `vibesql`, matching the workspace-root basename convention.
        assert_eq!(format_repo_column(Some("rjwalters/vibesql")), "vibesql");
        assert_eq!(format_repo_column(Some("loom")), "loom");
    }

    #[test]
    fn format_repo_column_falls_back_to_dash_when_none() {
        assert_eq!(format_repo_column(None), "-");
    }

    #[test]
    fn table_header_advertises_repo_column() {
        let mut report = sample_report();
        report.in_flight = vec![mk(3, Some("/repos/loom"))];
        let table = render_in_flight_table(&report);
        assert!(
            table.lines().next().unwrap().contains("REPO"),
            "expected REPO in the table header, got: {table}"
        );
    }

    #[test]
    fn distinguishes_same_issue_number_across_repos() {
        // The #4698 scenario: two different repos each dispatching their own
        // issue #3 must no longer look like the same duplicate dispatch.
        let mut report = sample_report();
        report.in_flight = vec![
            mk(3, Some("/repos/loom")),
            mk(3, Some("/repos/gf180-canary-2")),
        ];
        let table = render_in_flight_table(&report);
        assert!(table.contains("loom"), "expected repo basename 'loom' in: {table}");
        assert!(
            table.contains("gf180-canary-2"),
            "expected repo basename 'gf180-canary-2' in: {table}"
        );
    }

    #[test]
    fn missing_repo_renders_dash_without_panicking_or_misaligning() {
        let mut report = sample_report();
        report.in_flight = vec![mk(4, None)];
        let table = render_in_flight_table(&report);
        let row = table.lines().nth(2).expect("header + separator + row");
        assert!(row.contains(" - "), "expected a lone '-' placeholder in row: {row}");
    }

    #[test]
    fn single_repo_fleet_stays_readable() {
        // AC3: with only one managed repo, every row just repeats the same
        // basename — no conditional collapsing needed, alignment still holds.
        let mut report = sample_report();
        report.in_flight = vec![mk(1, Some("/repos/loom")), mk(2, Some("/repos/loom"))];
        let table = render_in_flight_table(&report);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 4, "header + separator + 2 rows, got: {table}");
        for row in &lines[2..] {
            assert!(row.contains("loom"), "expected 'loom' in row: {row}");
        }
    }
}

#[cfg(test)]
mod status_protection_tests {
    //! Reachable-path watchdog-protection surfacing (#4354, AC4 of #4331).
    //!
    //! These tests pin the `--json` contract for the `protection` object and the
    //! deliberate separation from the unreachable path's `install_state` block
    //! (#4069): the reachable payload carries `protection` and never
    //! `install_state`, and `protection` is wire-compatible-nullable so a host
    //! where no loom dir resolves still emits a well-formed payload.
    use super::{
        build_is_stale, build_status_json_value, render_build_status_line,
        render_observability_line,
    };
    use crate::cli::status::status_client_tests::sample_report;
    use chrono::{DateTime, Utc};
    use loom_daemon::daemon_install_state::{ProtectionReport, ProtectionState, WatchdogJob};
    use std::path::PathBuf;

    fn protection(
        state: ProtectionState,
        marker_present: bool,
        watchdog_provisioned: Option<bool>,
    ) -> ProtectionReport {
        ProtectionReport {
            state,
            marker_present,
            marker_path: PathBuf::from("/home/u/.loom/autonomy-desired"),
            job: WatchdogJob::SystemdTimer {
                timer_unit: "loom-daemon-watchdog.timer".to_string(),
            },
            watchdog_provisioned,
            detail: "fixture detail".to_string(),
        }
    }

    /// The `update` argument's fixture — an "unknown" self-update status is
    /// enough for these tests (they assert only on the `protection` object).
    fn no_update() -> loom_daemon::self_update::SelfUpdateStatus {
        loom_daemon::self_update::SelfUpdateStatus {
            built_commit: "abc1234".to_string(),
            source_commit: None,
            update_available: None,
        }
    }

    #[test]
    fn protection_object_is_emitted_on_the_reachable_path() {
        let report = sample_report();
        let value = build_status_json_value(
            &report,
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::Protected, true, Some(true))),
        );
        let p = &value["protection"];
        assert_eq!(p["state"], "protected");
        assert_eq!(p["marker_present"], true);
        assert_eq!(p["marker_path"], "/home/u/.loom/autonomy-desired");
        assert_eq!(p["watchdog_job"], "loom-daemon-watchdog.timer");
        assert_eq!(p["watchdog_job_kind"], "systemd-timer");
        assert_eq!(p["watchdog_provisioned"], true);
        assert_eq!(p["detail"], "fixture detail");
    }

    #[test]
    fn protection_object_carries_both_facts_for_the_no_marker_verdict() {
        // `state` names the dominant fact (no marker), but a consumer must still
        // be able to read the watchdog fact independently (#4354 AC1: "both
        // should be reported").
        let value = build_status_json_value(
            &sample_report(),
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::NoMarker, false, Some(true))),
        );
        let p = &value["protection"];
        assert_eq!(p["state"], "no-marker");
        assert_eq!(p["marker_present"], false);
        assert_eq!(p["watchdog_provisioned"], true);
    }

    #[test]
    fn protection_watchdog_not_provisioned_and_unknown_serialize_distinctly() {
        let not_provisioned = build_status_json_value(
            &sample_report(),
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::WatchdogNotProvisioned, true, Some(false))),
        );
        assert_eq!(not_provisioned["protection"]["state"], "watchdog-not-provisioned");
        assert_eq!(not_provisioned["protection"]["watchdog_provisioned"], false);

        // An unanswerable probe is `unknown` with a NULL provisioning fact —
        // never `false` (AC4: degrade, never mis-report).
        let unknown = build_status_json_value(
            &sample_report(),
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::Unknown, true, None)),
        );
        assert_eq!(unknown["protection"]["state"], "unknown");
        assert!(unknown["protection"]["watchdog_provisioned"].is_null());
    }

    #[test]
    fn observability_host_id_mismatch_is_null_when_the_ids_agree() {
        // #4830: the common case — no exporter, or an exporter whose key is
        // bound to this very host — is a null field, so `status` is unchanged
        // for every daemon that is not actually misconfigured.
        let value = build_status_json_value(&sample_report(), None, &no_update(), None, None);
        assert!(value["observability_host_id_mismatch"].is_null());
    }

    #[test]
    fn observability_host_id_mismatch_names_both_identities_in_json() {
        let mut report = sample_report();
        report.observability_host_id_mismatch =
            Some(loom_daemon::types::ObservabilityHostIdMismatch {
                daemon_host_id: "robb-studio".to_string(),
                ingest_host_id: "robb-pro".to_string(),
                first_seen_at: chrono::Utc::now(),
            });
        let value = build_status_json_value(&report, None, &no_update(), None, None);
        let m = &value["observability_host_id_mismatch"];
        assert_eq!(m["daemon_host_id"], "robb-studio");
        assert_eq!(m["ingest_host_id"], "robb-pro");
        assert!(m["first_seen_at"].is_string());
    }

    // ===================================================================
    // Telemetry export liveness (#5083)
    // ===================================================================

    fn render_now() -> DateTime<Utc> {
        "2026-08-03T12:00:00Z".parse().unwrap()
    }

    /// A running HTTPS exporter, up four hours, that has never been touched by
    /// a flush attempt — the base the individual states mutate from.
    fn export_status(
        mutate: impl FnOnce(&mut loom_daemon::types::ObservabilityExportStatus),
    ) -> loom_daemon::types::ObservabilityExportStatus {
        let mut status = loom_daemon::types::ObservabilityExportStatus {
            state: loom_daemon::types::ObservabilityExportState::Starting,
            host_id: Some("robb-studio".to_string()),
            ingest_host_id: None,
            endpoint: Some("https://dashboard.example/ingest".to_string()),
            exporter: Some("https".to_string()),
            started_at: Some(render_now() - chrono::Duration::hours(4)),
            last_success_at: None,
            last_failure_at: None,
            last_failure_detail: None,
            records_exported: 0,
            consecutive_failures: 0,
            flush_interval_secs: Some(30),
        };
        mutate(&mut status);
        status
    }

    #[test]
    fn observability_line_states_health_positively_with_the_host_id() {
        // AC1: an operator can confirm telemetry is flowing, and under which
        // host_id, without reading logs.
        let status = export_status(|e| {
            e.last_success_at = Some(render_now() - chrono::Duration::seconds(12));
            e.records_exported = 3481;
        });
        let line = render_observability_line(Some(&status), render_now());
        assert!(line.starts_with("Observability: OK"), "{line}");
        assert!(line.contains("12s ago"), "{line}");
        assert!(line.contains("host_id=robb-studio"), "{line}");
        assert!(line.contains("3481 record(s)"), "{line}");
    }

    #[test]
    fn observability_line_distinguishes_disabled_from_healthy() {
        // AC2: a host with observability disabled must not read like a healthy
        // one (before #5083 both rendered as nothing at all).
        let disabled = render_observability_line(
            Some(&loom_daemon::types::ObservabilityExportStatus::disabled()),
            render_now(),
        );
        assert!(disabled.contains("disabled"), "{disabled}");
        assert!(!disabled.contains("OK"), "{disabled}");
    }

    #[test]
    fn observability_line_distinguishes_misconfigured_from_disabled() {
        // Issue #5337: `enabled: true` with an unreadable ingestKeyFile must
        // NOT read like the deliberate-off `disabled` state, and must name
        // the offending path + errno rather than reporting `endpoint: null`.
        let misconfigured = loom_daemon::types::ObservabilityExportStatus::misconfigured(
            Some("https://ingest.example.com/v1/telemetry".to_string()),
            "could not read ingest key file /etc/loom/ingest.key: No such file or directory (os error 2)"
                .to_string(),
        );
        let line = render_observability_line(Some(&misconfigured), render_now());
        assert!(line.contains("MISCONFIGURED"), "{line}");
        assert!(!line.contains("Observability: disabled"), "{line}");
        assert!(line.contains("https://ingest.example.com/v1/telemetry"), "{line}");
        assert!(
            line.contains("/etc/loom/ingest.key") && line.contains("os error 2"),
            "must name the offending path and errno: {line}"
        );

        let disabled = render_observability_line(
            Some(&loom_daemon::types::ObservabilityExportStatus::disabled()),
            render_now(),
        );
        assert!(!disabled.contains("MISCONFIGURED"), "{disabled}");
    }

    #[test]
    fn observability_line_surfaces_never_exported_as_a_problem() {
        // AC3: the silent failure mode — configured, running for hours, and
        // nothing has ever landed.
        let line = render_observability_line(Some(&export_status(|_| {})), render_now());
        assert!(line.contains("NEVER EXPORTED"), "{line}");
        assert!(line.contains("host_id=robb-studio"), "{line}");
        assert!(line.contains("dashboard.example"), "{line}");
    }

    #[test]
    fn observability_line_does_not_alarm_during_the_startup_grace_window() {
        // A daemon rolled 20 seconds ago must not read as broken.
        let status = export_status(|e| {
            e.started_at = Some(render_now() - chrono::Duration::seconds(20));
        });
        let line = render_observability_line(Some(&status), render_now());
        assert!(line.contains("starting"), "{line}");
        assert!(!line.contains("NEVER EXPORTED"), "{line}");
    }

    #[test]
    fn observability_line_reports_a_failing_exporter_with_its_error() {
        let status = export_status(|e| {
            e.last_success_at = Some(render_now() - chrono::Duration::hours(2));
            e.consecutive_failures = 4;
            e.last_failure_detail = Some("sink rejected batch: HTTP 401 — denied".to_string());
        });
        let line = render_observability_line(Some(&status), render_now());
        assert!(line.contains("FAILING"), "{line}");
        assert!(line.contains("HTTP 401"), "{line}");
        assert!(line.contains("2h ago"), "{line}");
    }

    #[test]
    fn observability_line_names_both_identities_on_a_mismatch() {
        let status = export_status(|e| {
            e.last_success_at = Some(render_now() - chrono::Duration::seconds(12));
            e.ingest_host_id = Some("robb-pro".to_string());
            e.records_exported = 77;
        });
        let line = render_observability_line(Some(&status), render_now());
        assert!(line.contains("HOST-ID MISMATCH"), "{line}");
        assert!(line.contains("robb-pro") && line.contains("robb-studio"), "{line}");
    }

    #[test]
    fn observability_line_from_an_older_daemon_is_unknown_not_disabled() {
        // A `None` field means the daemon could not answer — reporting it as
        // "disabled" would invent a fact about a daemon that said nothing.
        let line = render_observability_line(None, render_now());
        assert!(line.contains("unknown"), "{line}");
        assert!(line.contains("older daemon binary"), "{line}");
    }

    #[test]
    fn observability_export_is_machine_readable_in_json() {
        // AC5: a watch loop asserts on the state string directly.
        let mut report = sample_report();
        report.observability_export = Some(export_status(|e| {
            e.last_success_at = Some(chrono::Utc::now());
            e.records_exported = 12;
            e.state = loom_daemon::types::ObservabilityExportState::Healthy;
        }));
        let value = build_status_json_value(&report, None, &no_update(), None, None);
        let e = &value["observability_export"];
        assert_eq!(e["state"], "healthy");
        assert_eq!(e["host_id"], "robb-studio");
        assert_eq!(e["records_exported"], 12);
        assert_eq!(e["endpoint"], "https://dashboard.example/ingest");
        assert!(e["last_success_at"].is_string());
    }

    #[test]
    fn observability_export_is_null_for_a_pre_5083_daemon() {
        let value = build_status_json_value(&sample_report(), None, &no_update(), None, None);
        assert!(value["observability_export"].is_null());
    }

    // ===================================================================
    // Running-vs-disk build staleness (#5341)
    // ===================================================================

    #[test]
    fn build_is_stale_reports_true_on_a_genuine_mismatch() {
        // The `loom-worker-1` incident this issue exists for: a running
        // daemon process built from an OLDER commit than the on-disk binary.
        assert_eq!(build_is_stale(Some("5111b74a"), "3f5132a5"), Some(true));
    }

    #[test]
    fn build_is_stale_reports_false_on_a_match() {
        assert_eq!(build_is_stale(Some("5111b74a"), "5111b74a"), Some(false));
    }

    #[test]
    fn build_is_stale_is_none_when_the_daemon_never_reported_a_running_commit() {
        // A pre-#5341 daemon binary — never misread as "matches" or "stale".
        assert_eq!(build_is_stale(None, "3f5132a5"), None);
    }

    #[test]
    fn build_is_stale_is_none_for_an_unknown_or_empty_commit_on_either_side() {
        assert_eq!(build_is_stale(Some("unknown"), "3f5132a5"), None);
        assert_eq!(build_is_stale(Some("5111b74a"), "unknown"), None);
        assert_eq!(build_is_stale(Some(""), "3f5132a5"), None);
    }

    #[test]
    fn build_status_line_warns_and_recommends_a_restart_on_a_mismatch() {
        let line = render_build_status_line(
            Some("5111b74a"),
            Some("2026-08-03T02:09:51Z"),
            "3f5132a5",
            "2026-08-04T01:00:12Z",
        );
        assert!(line.starts_with("Build:         STALE"), "{line}");
        assert!(line.contains("running 5111b74a"), "{line}");
        assert!(line.contains("disk 3f5132a5"), "{line}");
        assert!(line.contains("restart to roll"), "{line}");
        assert!(
            line.contains("loom-daemon-stop.sh") && line.contains("loom-daemon-start.sh"),
            "{line}"
        );
    }

    #[test]
    fn build_status_line_is_quiet_on_a_match() {
        let line = render_build_status_line(
            Some("5111b74a"),
            Some("2026-08-03T02:09:51Z"),
            "5111b74a",
            "2026-08-03T02:09:51Z",
        );
        assert!(line.contains("running matches disk"), "{line}");
        assert!(!line.contains("STALE"), "{line}");
    }

    #[test]
    fn build_status_line_from_an_older_daemon_is_unknown_not_a_false_match() {
        // A `None` running commit means the daemon predates #5341 — reporting
        // it as "matches" would invent a fact about a daemon that said nothing.
        let line = render_build_status_line(None, None, "3f5132a5", "2026-08-04T01:00:12Z");
        assert!(line.contains("unknown"), "{line}");
        assert!(line.contains("older daemon binary"), "{line}");
    }

    #[test]
    fn build_status_line_degrades_legibly_for_a_tarball_build() {
        // No git info baked into either binary — never claim staleness from a
        // non-comparison.
        let line = render_build_status_line(Some("unknown"), None, "unknown", "unknown");
        assert!(line.contains("staleness unknown"), "{line}");
        assert!(!line.contains("STALE"), "{line}");
    }

    #[test]
    fn daemon_build_json_block_carries_the_running_and_disk_facts() {
        let mut report = sample_report();
        report.daemon_build_commit = Some("5111b74a".to_string());
        report.daemon_built_at_raw = Some("2026-08-03T02:09:51Z".to_string());
        let value = build_status_json_value(&report, None, &no_update(), None, None);
        let b = &value["daemon_build"];
        assert_eq!(b["running_commit"], "5111b74a");
        assert_eq!(b["running_built_at"], "2026-08-03T02:09:51Z");
        // The disk side is this TEST binary's own compile-time build — the
        // exact quantity `render_build_status_line` also compares against.
        assert_eq!(b["disk_commit"], loom_daemon::self_update::BUILT_COMMIT);
        assert_eq!(b["disk_built_at"], loom_daemon::self_update::BUILT_AT_RAW);
        assert_eq!(
            b["stale"],
            serde_json::json!(build_is_stale(
                Some("5111b74a"),
                loom_daemon::self_update::BUILT_COMMIT
            ))
        );
    }

    #[test]
    fn daemon_build_json_block_running_commit_is_null_for_a_pre_5341_daemon() {
        let mut report = sample_report();
        report.daemon_build_commit = None;
        report.daemon_built_at_raw = None;
        let value = build_status_json_value(&report, None, &no_update(), None, None);
        let b = &value["daemon_build"];
        assert!(b["running_commit"].is_null());
        assert!(b["running_built_at"].is_null());
        assert!(b["stale"].is_null());
    }

    #[test]
    fn protection_is_null_when_no_report_could_be_built() {
        // No loom dir resolvable ⇒ the field is present but null, so the payload
        // stays well-formed for consumers that always read it.
        let value = build_status_json_value(&sample_report(), None, &no_update(), None, None);
        assert!(value["protection"].is_null());
    }

    #[test]
    fn reachable_payload_never_carries_the_unreachable_install_state_block() {
        // #4069 regression guard: `install_state` (with its exit-code semantics)
        // belongs to the unreachable `Err` arm ONLY. Protection is a sibling
        // classification, so adding it must not leak `install_state` into the
        // reachable payload. This fixture (`NoMarker`, work_finder unset) is
        // not an `autonomy_mismatch` case, so it also exercises the ordinary
        // reachable-path exit-0 case (#5409's `EXIT_AUTONOMY_MISMATCH` fires
        // ONLY when `protection.autonomy_mismatch` is true — see the
        // `autonomy_mismatch_*` tests below for that decision, made in
        // `handle_status_command`, not in this JSON builder).
        let value = build_status_json_value(
            &sample_report(),
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::NoMarker, false, Some(false))),
        );
        assert!(
            value.get("install_state").is_none(),
            "install_state must stay on the unreachable path only"
        );
        assert!(value.get("protection").is_some());
    }

    // ---- marker-vs-non-autonomous-daemon mismatch (#4693, AC3) ----
    // The sibling of #4069's ExpectedButDead state, but for the REACHABLE
    // path: the daemon IS alive and answering, yet its work-finder loop is
    // OFF while the marker records an operator's autonomy-desired intent —
    // exactly the 2026-07-30 incident's end state (a "protected", healthy
    // daemon that had silently stopped dispatching).

    #[test]
    fn autonomy_mismatch_true_when_marker_present_and_work_finder_off() {
        let mut report = sample_report();
        report.work_finder_enabled = Some(false);
        let value = build_status_json_value(
            &report,
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::Protected, true, Some(true))),
        );
        assert_eq!(value["work_finder"]["enabled"], false);
        assert_eq!(value["protection"]["autonomy_mismatch"], true);
    }

    #[test]
    fn autonomy_mismatch_false_when_work_finder_is_on() {
        let mut report = sample_report();
        report.work_finder_enabled = Some(true);
        let value = build_status_json_value(
            &report,
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::Protected, true, Some(true))),
        );
        assert_eq!(value["work_finder"]["enabled"], true);
        assert_eq!(value["protection"]["autonomy_mismatch"], false);
    }

    #[test]
    fn autonomy_mismatch_false_when_marker_absent_even_if_work_finder_off() {
        // Work-finder off with NO marker is the ordinary, deliberate FLAGS-OFF
        // reliability-daemon case (#3911) -- not a mismatch.
        let mut report = sample_report();
        report.work_finder_enabled = Some(false);
        let value = build_status_json_value(
            &report,
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::NoMarker, false, Some(true))),
        );
        assert_eq!(value["protection"]["autonomy_mismatch"], false);
    }

    #[test]
    fn autonomy_mismatch_false_when_work_finder_enabled_is_unknown() {
        // A pre-#4693 daemon binary never computed `work_finder_enabled` —
        // `null` must degrade to "no claim", never a false positive.
        let mut report = sample_report();
        report.work_finder_enabled = None;
        let value = build_status_json_value(
            &report,
            None,
            &no_update(),
            None,
            Some(&protection(ProtectionState::Protected, true, Some(true))),
        );
        assert!(value["work_finder"]["enabled"].is_null());
        assert_eq!(value["protection"]["autonomy_mismatch"], false);
    }

    // ---- #5409 AC2: the shared `autonomy_mismatch()` predicate, exercised
    // directly (not just through the JSON payload it feeds) since it is now
    // ALSO the input to `handle_status_command`'s exit-code decision
    // (`EXIT_AUTONOMY_MISMATCH`) — the two consumers must never be able to
    // disagree, which sharing one function (rather than recomputing the
    // condition twice) makes structurally impossible.

    #[test]
    fn autonomy_mismatch_fn_true_when_marker_present_and_work_finder_off() {
        let mut report = sample_report();
        report.work_finder_enabled = Some(false);
        let p = protection(ProtectionState::Protected, true, Some(true));
        assert!(super::autonomy_mismatch(Some(&p), &report));
    }

    #[test]
    fn autonomy_mismatch_fn_false_when_protection_is_none() {
        // No loom dir resolvable ⇒ no marker fact available at all — never a
        // false positive just because work_finder happens to read off.
        let mut report = sample_report();
        report.work_finder_enabled = Some(false);
        assert!(!super::autonomy_mismatch(None, &report));
    }

    #[test]
    fn exit_autonomy_mismatch_is_distinct_from_every_unreachable_and_fleet_exit_code() {
        // #5409 AC2's own guardrail: this reachable-path exit code must not
        // collide with (a) the unreachable-path `InstallState` codes (1/3/4,
        // the SAME `status` command's OTHER branch) or (b) `fleet status`'s
        // `HealthReport::exit_code()` (0/1/2, a different command). A
        // collision would not break *this* command directly, but would make
        // "was this the mismatch, or was it something else" ambiguous for any
        // script that greps a bare exit code instead of `--json`.
        use loom_daemon::daemon_install_state::{
            EXIT_ALIVE_BUT_UNRESPONSIVE, EXIT_AUTONOMY_MISMATCH, EXIT_EXPECTED_BUT_DEAD,
            EXIT_NOT_EXPECTED,
        };
        assert_ne!(EXIT_AUTONOMY_MISMATCH, EXIT_NOT_EXPECTED);
        assert_ne!(EXIT_AUTONOMY_MISMATCH, EXIT_EXPECTED_BUT_DEAD);
        assert_ne!(EXIT_AUTONOMY_MISMATCH, EXIT_ALIVE_BUT_UNRESPONSIVE);
        assert_ne!(EXIT_AUTONOMY_MISMATCH, 0, "must not silently collapse to the OK exit code");
    }
}

#[cfg(test)]
mod admission_brake_render_tests {
    //! Saturation admission-brake surfacing (#4903, AC4).
    //!
    //! The reporting half of the issue: `loom-worker-1` sat at 12× overcommit
    //! and `loom-daemon status` said `capacity_bound: false` — "3 sweeps, cap
    //! 12, the limiter is work availability" — which is indistinguishable from
    //! an idle host with nine free slots. These tests pin that a holding host
    //! *says so*, on both the human and `--json` surfaces, and that a healthy
    //! host's output is unchanged.
    use super::{build_status_json_value, render_admission_brake_line, saturation_hold_note};
    use crate::cli::status::status_client_tests::sample_report;
    use chrono::Utc;
    use loom_daemon::self_update::SelfUpdateStatus;
    use loom_daemon::types::{AdmissionBrakeStatus, DaemonStatusReport};

    fn no_update() -> SelfUpdateStatus {
        SelfUpdateStatus {
            built_commit: "abc".to_string(),
            source_commit: None,
            update_available: None,
        }
    }

    fn brake(enabled: bool, held: bool, load: Option<f64>) -> AdmissionBrakeStatus {
        AdmissionBrakeStatus {
            enabled,
            held,
            load_per_core: load,
            load_per_core_threshold: 4.0,
            held_since: held.then(|| Utc::now() - chrono::Duration::seconds(120)),
            held_ticks: u32::from(held) * 2,
        }
    }

    fn report_with(brake: Option<AdmissionBrakeStatus>) -> DaemonStatusReport {
        let mut r = sample_report();
        r.admission_brake = brake;
        r
    }

    // ---- human surface -----------------------------------------------------

    #[test]
    fn a_holding_host_says_so_instead_of_looking_idle() {
        let note = saturation_hold_note(&report_with(Some(brake(true, true, Some(11.91)))), 12)
            .expect("a holding brake must produce the substitute line");
        assert!(note.contains("ADMISSION BRAKE HOLDING"), "got: {note}");
        assert!(note.contains("11.91"), "the measured load must be shown: {note}");
        assert!(
            note.contains("the limiter is the HOST, not work availability"),
            "the misleading diagnosis must be actively contradicted: {note}"
        );
        assert!(
            note.contains("in-flight sweeps are untouched"),
            "the operator's next question must be answered inline: {note}"
        );
    }

    #[test]
    fn a_healthy_host_keeps_the_generic_capacity_line() {
        // AC3's reporting half: not holding ⇒ no substitution, so the pre-#4903
        // "limiter is work availability" line still prints unchanged.
        assert!(
            saturation_hold_note(&report_with(Some(brake(true, false, Some(0.4)))), 12).is_none()
        );
        assert!(
            saturation_hold_note(&report_with(Some(brake(false, true, Some(11.9)))), 12).is_none()
        );
        assert!(saturation_hold_note(&report_with(None), 12).is_none());
    }

    #[test]
    fn brake_line_reports_held_ok_and_disabled_distinctly() {
        let holding =
            render_admission_brake_line(&report_with(Some(brake(true, true, Some(11.91)))))
                .expect("line");
        assert!(holding.starts_with("Admission brake: HOLDING"), "got: {holding}");
        assert!(holding.contains("2 tick(s)"), "got: {holding}");

        let ok = render_admission_brake_line(&report_with(Some(brake(true, false, Some(0.42)))))
            .expect("line");
        assert!(ok.starts_with("Admission brake: OK"), "got: {ok}");
        assert!(ok.contains("0.42"), "got: {ok}");

        let off = render_admission_brake_line(&report_with(Some(brake(false, false, None))))
            .expect("line");
        assert_eq!(off, "Admission brake: disabled");
    }

    #[test]
    fn absent_brake_renders_nothing_at_all() {
        // A pre-#4903 daemon reports `None`; the renderer must stay silent
        // rather than invent a "brake OK" claim it has no evidence for.
        assert!(render_admission_brake_line(&report_with(None)).is_none());
    }

    #[test]
    fn missing_load_reading_renders_as_na_not_zero() {
        let ok = render_admission_brake_line(&report_with(Some(brake(true, false, None))))
            .expect("line");
        assert!(ok.contains("n/a"), "absent evidence must not render as 0.00: {ok}");
    }

    // ---- --json surface ----------------------------------------------------

    #[test]
    fn json_carries_the_brake_hold_state() {
        let value = build_status_json_value(
            &report_with(Some(brake(true, true, Some(11.91)))),
            None,
            &no_update(),
            None,
            None,
        );
        let b = &value["admission_brake"];
        assert_eq!(b["enabled"], true);
        assert_eq!(b["held"], true);
        assert_eq!(b["load_per_core_threshold"], 4.0);
        assert_eq!(b["held_ticks"], 2);
        assert!(b["held_since"].is_string());
    }

    #[test]
    fn json_brake_is_null_when_no_brake_is_registered() {
        let value = build_status_json_value(&report_with(None), None, &no_update(), None, None);
        assert!(value["admission_brake"].is_null());
    }

    #[test]
    fn admission_brake_field_survives_a_wire_round_trip_and_older_payloads() {
        // Forward-compat contract: an absent field (pre-#4903 daemon) parses as
        // `None`, never as a fabricated "not holding" brake.
        let report = report_with(Some(brake(true, true, Some(11.91))));
        let json = serde_json::to_string(&report).expect("serialize");
        let back: DaemonStatusReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.admission_brake, report.admission_brake);

        let mut stripped: serde_json::Value = serde_json::from_str(&json).expect("value");
        stripped
            .as_object_mut()
            .expect("object")
            .remove("admission_brake");
        let older: DaemonStatusReport =
            serde_json::from_value(stripped).expect("pre-#4903 payload must still parse");
        assert!(older.admission_brake.is_none());
    }
}

#[cfg(test)]
mod preflight_advisory_render_tests {
    //! Issue #5029: the pre-flight-death advisory (#4386) has no freshness
    //! signal and is scoped to a single workspace with no way to tell which
    //! one from the rendered text alone. These tests pin the display-only fix
    //! — an "as of" freshness suffix on the human line, the timestamp on the
    //! `--json` surface, and forward/backward wire compatibility — without
    //! touching the trip/clear decision logic itself.
    use super::{build_status_json_value, render_preflight_advisory_line};
    use crate::cli::status::status_client_tests::sample_report;
    use chrono::Utc;
    use loom_daemon::self_update::SelfUpdateStatus;
    use loom_daemon::types::DaemonStatusReport;

    fn no_update() -> SelfUpdateStatus {
        SelfUpdateStatus {
            built_commit: "abc".to_string(),
            source_commit: None,
            update_available: None,
        }
    }

    fn report_with(
        active: bool,
        message: Option<&str>,
        changed_at: Option<chrono::DateTime<Utc>>,
    ) -> DaemonStatusReport {
        let mut r = sample_report();
        r.preflight_advisory_active = active;
        r.preflight_advisory_message = message.map(ToString::to_string);
        r.preflight_advisory_changed_at = changed_at;
        r
    }

    // ---- human surface -----------------------------------------------------

    #[test]
    fn active_advisory_renders_with_a_freshness_suffix() {
        let ts = Utc::now() - chrono::Duration::seconds(42);
        let report = report_with(
            true,
            Some("WARNING: last 3 dispatches died at claude-wrapper pre-flight (x) — check .mcp.json [workspace: /repos/loom]"),
            Some(ts),
        );
        let line = render_preflight_advisory_line(&report).expect("active advisory renders a line");
        assert!(line.contains("WARNING: last 3 dispatches"), "got: {line}");
        assert!(
            line.contains("workspace: /repos/loom"),
            "the rendered line must name the scoped workspace: {line}"
        );
        assert!(
            line.contains("as of") && line.contains("ago"),
            "the rendered line must carry a freshness indicator: {line}"
        );
    }

    #[test]
    fn active_advisory_without_a_timestamp_renders_the_message_unchanged() {
        // Forward-compat: an older daemon binary that never populated the new
        // field must still render the bare message, not a "None"/error text.
        let report = report_with(true, Some("WARNING: still dying"), None);
        let line = render_preflight_advisory_line(&report).expect("line");
        assert_eq!(line, "WARNING: still dying");
    }

    #[test]
    fn inactive_advisory_renders_nothing() {
        assert!(render_preflight_advisory_line(&report_with(false, None, None)).is_none());
        // Defensive: `active` true with no message must not panic or fabricate
        // a line — should not happen in practice, but stay silent rather than
        // render a garbage string.
        assert!(
            render_preflight_advisory_line(&report_with(true, None, Some(Utc::now()))).is_none()
        );
    }

    // ---- --json surface ----------------------------------------------------

    #[test]
    fn json_carries_the_advisory_timestamp() {
        let ts = Utc::now() - chrono::Duration::seconds(7);
        let value = build_status_json_value(
            &report_with(true, Some("WARNING: x"), Some(ts)),
            None,
            &no_update(),
            None,
            None,
        );
        assert_eq!(value["preflight_advisory_active"], true);
        assert!(value["preflight_advisory_changed_at"].is_string());
    }

    #[test]
    fn json_advisory_timestamp_is_null_when_absent() {
        let value = build_status_json_value(
            &report_with(false, None, None),
            None,
            &no_update(),
            None,
            None,
        );
        assert!(value["preflight_advisory_changed_at"].is_null());
    }

    #[test]
    fn advisory_timestamp_survives_a_wire_round_trip_and_older_payloads() {
        let ts = Utc::now() - chrono::Duration::seconds(5);
        let report = report_with(true, Some("WARNING: x"), Some(ts));
        let json = serde_json::to_string(&report).expect("serialize");
        let back: DaemonStatusReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.preflight_advisory_changed_at, report.preflight_advisory_changed_at);

        // Forward-compat: an absent field (pre-#5029 daemon) parses as `None`,
        // never a fabricated/default timestamp.
        let mut stripped: serde_json::Value = serde_json::from_str(&json).expect("value");
        stripped
            .as_object_mut()
            .expect("object")
            .remove("preflight_advisory_changed_at");
        let older: DaemonStatusReport =
            serde_json::from_value(stripped).expect("pre-#5029 payload must still parse");
        assert!(older.preflight_advisory_changed_at.is_none());
    }
}

#[cfg(test)]
mod stash_status_render_tests {
    //! Fleet-wide quarantine-stash visibility (#5692): pins the
    //! `per_repo[].stash` `--json` contract and the compact human-age
    //! formatter that renders alongside it.
    use super::{build_status_json_value, format_stash_age};
    use crate::cli::status::status_client_tests::sample_report;

    fn no_update() -> loom_daemon::self_update::SelfUpdateStatus {
        loom_daemon::self_update::SelfUpdateStatus {
            built_commit: "abc1234".to_string(),
            source_commit: None,
            update_available: None,
        }
    }

    fn repo_with_stashes(
        total: usize,
        quarantine: usize,
        oldest_age_secs: Option<u64>,
    ) -> loom_daemon::types::RepoStatus {
        loom_daemon::types::RepoStatus {
            root: std::path::PathBuf::from("/repos/loom"),
            priority: 100,
            in_flight_count: 0,
            health_gate_halted: false,
            quarantined_issues: vec![],
            health_gate_not_evaluated: false,
            health_gate_not_evaluated_reason: None,
            health_gate_enabled: None,
            health_gate_verdict_at: None,
            root_missing: false,
            health_gate_deferred: false,
            health_gate_deferred_reason: None,
            health_gate_verdict_tier: None,
            role_runner_enabled: false,
            role_runner_roles: vec![],
            role_runner_on_idle_roles: vec![],
            token_pool_dir: None,
            ranking_present: false,
            ranking_age_secs: None,
            stash_total_count: total,
            stash_quarantine_count: quarantine,
            stash_oldest_age_secs: oldest_age_secs,
        }
    }

    #[test]
    fn per_repo_json_carries_stash_counts_and_oldest_age() {
        let mut report = sample_report();
        report.per_repo = vec![repo_with_stashes(5, 2, Some(3600))];
        let value = build_status_json_value(&report, None, &no_update(), None, None);
        let stash = &value["per_repo"][0]["stash"];
        assert_eq!(stash["total_count"], 5);
        assert_eq!(stash["quarantine_count"], 2);
        assert_eq!(stash["oldest_age_secs"], 3600);
    }

    #[test]
    fn per_repo_json_stash_oldest_age_is_null_with_zero_stashes() {
        let mut report = sample_report();
        report.per_repo = vec![repo_with_stashes(0, 0, None)];
        let value = build_status_json_value(&report, None, &no_update(), None, None);
        let stash = &value["per_repo"][0]["stash"];
        assert_eq!(stash["total_count"], 0);
        assert_eq!(stash["quarantine_count"], 0);
        assert!(stash["oldest_age_secs"].is_null());
    }

    #[test]
    fn format_stash_age_prefers_the_largest_whole_unit() {
        assert_eq!(format_stash_age(30), "30s");
        assert_eq!(format_stash_age(90), "1m");
        assert_eq!(format_stash_age(3600), "1h");
        assert_eq!(format_stash_age(90_000), "1d");
        // #5690's fleet audit: "12 days" of accumulation is the motivating
        // scale for this surface.
        assert_eq!(format_stash_age(12 * 86_400), "12d");
    }
}
