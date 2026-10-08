//! Which parts of the `DaemonStatus` build run for one request (Issue #10787).
//!
//! `Request::DaemonStatus` builds every section, exactly as before.
//! `Request::DaemonStatusSections` names the sections a caller wants, and
//! [`super::build_daemon_status_for`] skips the phases nothing requested reads
//! — the `O(roots)` per-root walk ([`roots_to_walk`]) and the machine-level
//! "tail" ([`machine_caps`] and the helpers after it), which between them are
//! where a full build spends its 10–21s on a busy dispatcher. The phase map
//! itself lives in [`crate::status_section::SectionSet`] so the CLI's
//! client-side collectors consult the same answers.
//!
//! A sibling of `ipc.rs` (an over-threshold file frozen by the file-size
//! ratchet), so the full-build entry point and the drain overlay live here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;

use super::{drain_roll, DrainState};
use crate::main_health_gate::WorkspaceHealthStates;
use crate::status_section::{SectionSet, StatusSection};
use crate::types::{CredentialPreflightReport, DaemonStatusReport, Request};
use crate::workspace_pool::WorkspacePool;
use crate::workspace_registry::WorkspaceRegistry;

/// The sections a status request asks for: every section for
/// `DaemonStatus`, the named ones for `DaemonStatusSections`, `None` for any
/// other request.
pub(super) fn requested_sections(request: &Request) -> Option<SectionSet> {
    match request {
        Request::DaemonStatus => Some(SectionSet::all()),
        Request::DaemonStatusSections { sections } => {
            Some(SectionSet::only(sections.iter().copied()))
        }
        _ => None,
    }
}

/// The roots the build walks: every registered root (an empty registry
/// yields `[fallback_root]`, #3930) when a requested section reads the
/// per-root sweep registries, none otherwise — no registry is provisioned,
/// locked or listed for a build that does not report on them.
pub(super) fn roots_to_walk(
    registry: &WorkspaceRegistry,
    fallback_root: &Path,
    sections: &SectionSet,
) -> Vec<PathBuf> {
    if sections.walks_roots() {
        registry.effective_roots(fallback_root)
    } else {
        Vec::new()
    }
}

/// The machine-level inputs of one status build — one token pool, one scratch
/// volume, one work-finder config — computed once from the daemon's primary
/// workspace (the same basis as pre-#3930). A field no requested section
/// reads is left at its default.
#[derive(Debug, Default)]
pub(super) struct MachineCaps {
    pub token_pool_size: usize,
    pub token_pool_dir: Option<PathBuf>,
    pub disk_headroom: usize,
    pub ram_headroom: usize,
    pub configured_max: usize,
    pub logical_cpus: usize,
    pub loadavg_1m: Option<f64>,
    pub cpu_idle_fraction: Option<f64>,
    pub dynamic_cap: usize,
    pub capacity_bound: bool,
    pub capacity: crate::types::CapacityReport,
    pub work_finder_enabled: Option<bool>,
    pub work_finder_interval_secs: Option<u64>,
}

/// Compute [`MachineCaps`] for `sections` (#10787). The full set computes
/// everything, exactly as the inline code did before; a scoped build skips
/// each input nothing requested reads. That matters as much as skipping the
/// per-root walk: the headroom reads fork `df` / `vm_stat` / `sysctl`, and on
/// a saturated dispatcher this "tail" was measured at 2.7–6.1s of an 8–13s
/// build.
pub(super) fn machine_caps(
    workspace_root: &Path,
    workspace_registry: &WorkspaceRegistry,
    in_flight: usize,
    sections: &SectionSet,
) -> MachineCaps {
    let mut caps = MachineCaps::default();
    if sections.needs_token_pool() {
        // Registry-aware anchoring (#4292, trip-wire 1): `workspace_root` is the
        // daemon's own seeded default (its cwd at startup, or `LOOM_WORKSPACE`),
        // which for a machine-level daemon started under systemd with a bare cwd
        // (e.g. `$HOME`) is not itself a real repo checkout. The caller already
        // loaded `workspace_registry`, so this reuses it rather than a second
        // registry read.
        let tokens_dir = crate::tokens_pool::paths::resolve_tokens_dir_anchored(
            workspace_root,
            workspace_registry,
        );
        let token_pool_size = crate::tokens::token_pool_size_at_dir(&tokens_dir);
        // Token-capacity backpressure (#3902): back the token axis off from the
        // flat pool count toward the count of *healthy* accounts read from the
        // rotation ranking. When no ranking exists, `token_axis_limit` == the
        // raw pool size, so the figure is byte-for-byte the pre-#3902 value.
        let ranking = crate::capacity::read_ranking_at(&tokens_dir);
        let token_axis_limit = ranking.as_ref().map_or(token_pool_size, |r| r.available);
        // The token axis no longer bounds the concurrency cap (#5270) —
        // `token_bound` here does NOT mean "tokens are the binding cap term"; it
        // means genuine starvation (zero healthy accounts to select from at
        // spawn time). `token_axis_limit` remains on the report as an
        // informational account-health figure (it still drives spawn-time
        // *selection*), but it does not gate admission any more (#5305:
        // restoring this as a reachable zero-healthy check, rather than a
        // hardcoded `false`, so `status_render.rs`'s add-accounts guidance
        // branch can fire again).
        caps.capacity = crate::types::CapacityReport {
            ranking_present: ranking.is_some(),
            total_accounts: ranking.as_ref().map_or(token_pool_size, |r| r.total),
            healthy_accounts: ranking.as_ref().map_or(token_pool_size, |r| r.available),
            exhausted_accounts: ranking
                .as_ref()
                .map_or(0, crate::capacity::RankingSnapshot::unhealthy),
            token_axis_limit,
            token_bound: token_axis_limit == 0,
        };
        caps.token_pool_size = token_pool_size;
        // Exposed on the report (#4292) so a client reading `status` from any
        // cwd sees exactly which directory the daemon used rather than
        // silently re-resolving a possibly-different one.
        caps.token_pool_dir = Some(tokens_dir);
    }
    if sections.needs_work_finder_config() {
        let wf_config = crate::work_finder::read_work_finder_config(workspace_root);
        caps.configured_max = crate::work_finder::resolve_max_concurrent_with_config(&wf_config);
        // Whether the work-finder loop is enabled for THIS running daemon
        // process (#4693) — read from this process's own env/config.
        caps.work_finder_enabled = Some(crate::work_finder::resolve_enabled(&wf_config));
        // The tick interval THIS process resolved (#4824).
        caps.work_finder_interval_secs =
            Some(crate::work_finder::resolve_interval_with_config(&wf_config).as_secs());
    }
    if sections.needs_host_headroom() {
        caps.disk_headroom = crate::disk_headroom::disk_headroom_limit(workspace_root);
        // RAM headroom (#5270): the second "dumb mode" machine-headroom axis,
        // folded into `dynamic_cap` alongside disk headroom.
        caps.ram_headroom = crate::ram_headroom::ram_headroom_limit();
        // `needs_host_headroom` implies `needs_work_finder_config`, so
        // `configured_max` is resolved here.
        caps.dynamic_cap = crate::work_finder::resolve_dynamic_max_concurrent(
            caps.disk_headroom,
            caps.ram_headroom,
            caps.configured_max,
        );
        // "Currently binding" vs "smallest ceiling" (#4031): the dynamic cap is
        // the minimum of several ceilings, but a ceiling only *binds* once
        // in-flight occupancy reaches it. Below the cap the limiter is work
        // availability, not any resource term.
        caps.capacity_bound = in_flight >= caps.dynamic_cap;
    }
    if sections.needs_cpu_sample() {
        // Host CPU **observations** (#3978, measured-idle signal #4031). Since
        // #4512 these no longer feed the cap — they are reported so an operator
        // can see whether this machine's `maxConcurrent` leaves it idle or
        // saturated. Never blocks: the idle fraction is the memoized sample
        // (`status_off_runtime::serve` pre-warms it), plus a fast fresh
        // loadavg read.
        caps.logical_cpus = crate::cpu_headroom::logical_cpu_count();
        caps.loadavg_1m = crate::cpu_headroom::read_loadavg_1m();
        caps.cpu_idle_fraction = crate::cpu_headroom::cached_cpu_idle_fraction();
    }
    caps
}

/// Claude-wrapper pre-flight-death tripwire (#4386) as `(active, message,
/// changed_at)`, read from the fallback/default workspace's own registry —
/// mirrors the top-level `main_health_gate_*` fields' fallback-root scoping.
/// Not read (and that registry not locked) unless `preflight_advisory` is
/// requested.
pub(super) fn preflight_advisory(
    workspace_pool: &WorkspacePool,
    fallback_root: &Path,
    sections: &SectionSet,
) -> (bool, Option<String>, Option<chrono::DateTime<Utc>>) {
    if !sections.has(StatusSection::PreflightAdvisory) {
        return (false, None, None);
    }
    let registry = workspace_pool.get_or_provision(fallback_root);
    let sr = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (active, message) = sr.preflight_advisory();
    (active, message, sr.preflight_advisory_changed_at())
}

/// Host-level role-runner sharding posture (#6374) for the `role_runner`
/// section.
pub(super) fn host_shard_posture(
    fallback_root: &Path,
    sections: &SectionSet,
) -> Option<crate::types::RoleRunnerShardPosture> {
    sections.when(StatusSection::RoleRunner, || {
        // `decide(...).posture` rather than `resolve_posture(...)` so that
        // with roster mode on (#7691) the header reports the ring the
        // fence actually produced — "shard 1 of 3 (index from roster,
        // count from roster)" — instead of a static posture no tick uses.
        // With the roster off (the default) `decide`'s posture IS
        // `resolve_posture`'s, so this is byte-identical to pre-#7691.
        let decision = crate::role_shard::decide(fallback_root);
        let posture = decision.posture;
        crate::types::RoleRunnerShardPosture {
            index: posture.index(),
            count: posture.count(),
            summary: posture.describe(),
            configured: posture.is_configured(),
            roster: super::roster_status::roster_status(&decision.roster),
        }
    })
}

/// Codex session containers (#10600) for the `session_containers` section:
/// the published snapshot plus on-disk hold/removal records.
pub(super) fn session_containers(
    sections: &SectionSet,
) -> Option<crate::session_status::SessionContainersReport> {
    sections
        .when(StatusSection::SessionContainers, crate::session_status::report)
        .flatten()
}

/// Forge call accounting (#9251) for the `forge_calls` section: the
/// host-wide last-hour window from the per-host sink + this process's
/// totals; a local read, no forge call.
pub(super) fn forge_calls(sections: &SectionSet) -> Option<Box<crate::types::ForgeCallsStatus>> {
    sections.when(StatusSection::ForgeCalls, || {
        Box::new(crate::forge_call_stats::status_report(
            Utc::now(),
            crate::rate_limit_breaker::global_snapshot().as_ref(),
        ))
    })
}

/// Build the full autonomous-mode operability snapshot — every section.
/// See [`super::build_daemon_status_for`] for the build itself.
#[must_use]
pub fn build_daemon_status(
    workspace_pool: &Arc<WorkspacePool>,
    health_states: &WorkspaceHealthStates,
    fallback_root: &Path,
    credential_preflight: &CredentialPreflightReport,
) -> DaemonStatusReport {
    super::build_daemon_status_for(
        workspace_pool,
        health_states,
        fallback_root,
        credential_preflight,
        &SectionSet::all(),
    )
}

/// Like [`super::build_daemon_status_for`] but overlays the live
/// drain-and-restart state (Issue #4090) so `loom-daemon status` can surface
/// `DRAINING (n remaining, deadline …)`. The IPC status handler calls this;
/// the base builder stays drain-agnostic for its existing tests.
#[must_use]
pub fn build_daemon_status_with_drain(
    workspace_pool: &Arc<WorkspacePool>,
    health_states: &WorkspaceHealthStates,
    fallback_root: &Path,
    credential_preflight: &CredentialPreflightReport,
    drain: &DrainState,
    sections: &SectionSet,
) -> DaemonStatusReport {
    let mut report = super::build_daemon_status_for(
        workspace_pool,
        health_states,
        fallback_root,
        credential_preflight,
        sections,
    );
    let snap = drain.snapshot();
    report.draining = drain.is_draining();
    report.drain_deadline = snap.deadline;
    // #8514: the live roll projection — "roll pending since T, dispatch paused
    // for D, N in flight" — computed against the same in-flight list this
    // report already carries, so the two can never disagree. (`drain` is one
    // of the sections that walks the roots, so that list is populated
    // whenever the drain section is served.)
    report.drain_roll = drain_roll::roll_status(&snap, report.in_flight.len(), Utc::now());
    report.drain_paused_by_day = drain.paused_by_day(Utc::now()); // #8652
    report.drain_note = snap.note;
    report
}

#[cfg(test)]
mod tests;
