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
    /// fallback) — the "healthy tokens" input to the dynamic concurrency cap.
    token_axis_limit: usize,
    /// The effective dynamic cap consistent with `token_axis_limit`:
    /// `min(token_axis_limit, disk_headroom, configured_max)` (the CPU term
    /// added in #3978 was removed in #4512).
    effective_cap: usize,
    /// Whether the token axis is the binding (minimum) constraint.
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
            // The token axis of the cap is `healthy × per-token` (#3947); treat a
            // pre-#3947 wire `0` as the effective floor of 1.
            let factor = report.per_token_concurrency.max(1);
            let token_axis_effective = token_axis_limit.saturating_mul(factor);
            // #4512: the cap is min(token axis, disk, configured max). A
            // pre-#4512 daemon still SENDS a `cpu_headroom` field; serde ignores
            // it, and we deliberately do not reintroduce it into the client-side
            // recomputation — the daemon's own `dynamic_cap` (shown as the
            // headline below) remains the authority either way.
            let effective_cap = token_axis_effective
                .min(report.disk_headroom)
                .min(report.configured_max);
            let token_bound = token_axis_effective <= report.disk_headroom
                && token_axis_effective <= report.configured_max;
            return ResolvedCapacity {
                source: "probe",
                ranking_present: true,
                total: cap.total,
                healthy: cap.healthy,
                exhausted: cap.exhausted,
                token_axis_limit,
                effective_cap,
                token_bound,
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
        // Observability host-identity mismatch (#4830): non-null means this
        // host's ingest key is bound to a DIFFERENT host_id than the daemon
        // reports for itself, so its telemetry is being filed under the wrong
        // host. `null` in the common case (ids agree, or no exporter running).
        "observability_host_id_mismatch": report.observability_host_id_mismatch,
        "dynamic_cap": {
            "token_pool_size": report.token_pool_size,
            // The directory the daemon resolved for the pool above (#4292) —
            // `null` only from a pre-#4292 daemon binary that never computed
            // one. Lets an operator confirm at a glance which of the
            // per-repo/shared pools is actually in effect, independent of
            // whatever cwd `loom-daemon status` itself was run from.
            "token_pool_dir": report.token_pool_dir,
            "disk_headroom": report.disk_headroom,
            // Host CPU OBSERVATIONS (#3978/#4031), not cap terms: #4512 removed
            // the CPU headroom term from admission. Reported because observed
            // idle is the evidence for tuning `configured_max` on this machine.
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
            "autonomy_mismatch": p.marker_present && report.work_finder_enabled == Some(false),
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
    if report.preflight_advisory_active {
        if let Some(msg) = &report.preflight_advisory_message {
            println!("\n{msg}");
        }
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
        "  = min(healthy {} × per-token {} = {}, disk headroom {}, \
         configured max {})",
        dispatch_token_axis,
        factor,
        dispatch_token_axis.saturating_mul(factor),
        report.disk_headroom,
        report.configured_max
    );
    if rc.source == "probe" && rc.effective_cap != dispatch_cap {
        println!(
            "  fresh probe suggests: {} (healthy {} × per-token {} = {}) — not yet reflected in \
             dispatch; if this persists, refresh with `loom-daemon tokens check --ranking`.",
            rc.effective_cap,
            rc.token_axis_limit,
            factor,
            rc.token_axis_limit.saturating_mul(factor)
        );
    }
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
    // #4344: the daemon's own dispatch decision reads 0 healthy accounts while
    // a fresher probe (or the raw pool) shows real capacity — the exact
    // wedge this issue exists for. When this holds, promote the diagnosis to
    // the headline and suppress the misleading "limiter is work availability"
    // line below (the limiter is unmistakably the token term: `0 × per-token
    // = 0`).
    let dispatch_starved_but_disagrees = report.capacity.ranking_present
        && report.capacity.healthy_accounts == 0
        && rc.ranking_present
        && rc.healthy > 0;
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
            // Headline promotion (#4344, was a small-print "note" pre-fix):
            // the daemon's own dispatch decision is starved at 0 healthy
            // accounts while the number above disagrees — dispatch will not
            // resume until the ranking the daemon actually reads is fresh.
            let pool_display = report
                .token_pool_dir
                .as_ref()
                .map_or_else(|| "(unknown pool dir)".to_string(), |d| d.display().to_string());
            println!(
                "  \u{26a0} DISPATCH IS TOKEN-STARVED: the daemon's own ranking read shows \
                 0/{} healthy (dispatch cap {dispatch_cap}), disagreeing with the {} healthy \
                 shown above from {pool_display}. The token term is the limiter — refresh the \
                 ranking with `loom-daemon tokens check --ranking` (or wait for the next self-refresh).",
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
        // #4344: when the daemon's own dispatch decision is unambiguously
        // token-starved (see above), never print "the limiter is work
        // availability" — the headline diagnosis already named the real
        // limiter, and running the generic capacity_bound/token_bound chain
        // underneath it would contradict it (e.g. `capacity_bound` is
        // trivially true against a dispatch cap of 0).
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
                if !report.preflight_advisory_active {
                    println!(
                        "  not capacity-bound ({} in flight, cap {dispatch_cap} — the limiter is \
                         work availability, not tokens/disk/CPU)",
                        report.in_flight.len(),
                    );
                }
            } else if rc.token_bound {
                if rc.healthy == 0 {
                    println!(
                        "  token-bound: NO healthy accounts — new dispatch deferred until \
                         capacity returns. Add accounts (~/.claude-monitor/accounts.env + \
                         `loom-daemon tokens bootstrap`) or buy API credits, then `loom-daemon tokens check \
                         --ranking`."
                    );
                } else {
                    println!(
                        "  token-bound: tokens are the binding constraint on throughput. Add \
                         accounts or API credits to dispatch more concurrently."
                    );
                }
            } else {
                println!("  not token-bound (tokens are not the current bottleneck)");
            }
        }
    } else {
        println!(
            "  (no ranking — run `loom-daemon tokens check --ranking`; token pool size {} used as the \
             health basis)",
            report.token_pool_size
        );
        if !capacity_bound && !report.preflight_advisory_active {
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
                println!("               Re-arm with a supervised restart:");
                println!("                 ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh");
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
        if p.marker_present && report.work_finder_enabled == Some(false) {
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
    use super::build_status_json_value;
    use crate::cli::status::status_client_tests::sample_report;
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
        // reachable payload — nor gain an exit code of its own (the reachable
        // path always exits 0).
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
}
