//! Tranche-2 hyperparameter schema and validation: the `supervision`,
//! `headroom`, `process`, `observability` and `update` groups — the env-only
//! janitor/lifecycle knobs consolidated onto the `hyperparameters` surface.
//!
//! Verbatim move out of `hyperparams.rs` for the source-file ratchet (the
//! parent crossed the 1000-line threshold when this tranche landed); no
//! behavior change. The parent declares `mod tranche2;`, re-exports the
//! param structs, and its `validate_layer` dispatches to the validators here.

use serde::Serialize;
use serde_json::Value;

use super::{check_u64, Violation};

// Tranche-2 built-in defaults — the same single-source constants the parent
// imports for its `Default` impl; resolve_tranche2 uses them as the bottom
// of each field's precedence chain.
use crate::api_keys_pool::inflight::DEFAULT_STALE_SECS as DEFAULT_API_KEY_INFLIGHT_STALE_SECS;
use crate::disk_headroom::DEFAULT_PER_WORKTREE_GB;
use crate::epic_supervisor::{DEFAULT_INFLIGHT_TTL_SECS, DEFAULT_SUPERVISOR_INTERVAL_SECS};
use crate::inflight::DEFAULT_STALE_SECS as DEFAULT_SWEEP_INFLIGHT_STALE_SECS;
use crate::launchd_reload::{
    DEFAULT_BOOTOUT_SETTLE_SECS, DEFAULT_BOOTSTRAP_RETRY_ATTEMPTS, DEFAULT_BOOTSTRAP_RETRY_SECS,
};
use crate::observability::ops::disposition::DEFAULT_REFRESH_SECS as DEFAULT_DISPOSITION_REFRESH_SECS;
use crate::observability::ops::dwell::DEFAULT_STARVATION_SECS as DEFAULT_QUEUE_STARVATION_SECS;
use crate::ram_headroom::DEFAULT_PER_WORKTREE_RAM_GB;
use crate::restart_verify::{
    DEFAULT_POLL_INTERVAL_MS, DEFAULT_POLL_SECS, DEFAULT_RECOVERY_POLL_SECS,
};
use crate::self_update::{DEFAULT_STALE_WARN_COMMITS, DEFAULT_STALE_WARN_HOURS};
use crate::sweep_registry::reaper::{
    DEFAULT_REAPER_INTERVAL_SECS as DEFAULT_SWEEP_REAPER_INTERVAL_SECS, REAP_GH_TIMEOUT_SECS,
};
use crate::tokens_pool::bad_tokens::{
    DEFAULT_CLEANUP_MAX_AGE_SECS, DEFAULT_EXHAUSTION_COOLDOWN_SECS,
};
use crate::worktree_activity::DEFAULT_ACTIVITY_WINDOW_MINUTES;

/// Background janitor-loop cadences and staleness TTLs — the loops that
/// reap, reconcile, and expire stale state (tranche 2). Every field's
/// single-knob env var sits above the layer; each built-in default is the
/// constant its module used before consolidation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SupervisionParams {
    /// Seconds between epic-supervisor ticks. Source:
    /// `epic_supervisor::DEFAULT_SUPERVISOR_INTERVAL_SECS` (300). Env:
    /// `LOOM_EPIC_SUPERVISOR_INTERVAL_SECS`. Range `[30, 3600]`.
    pub epic_supervisor_interval_secs: u64,
    /// Seconds an epic-supervisor inflight record stays fresh. Source:
    /// `epic_supervisor::DEFAULT_INFLIGHT_TTL_SECS` (900). Env:
    /// `LOOM_EPIC_INFLIGHT_TTL_SECS`. Range `[60, 86400]`.
    pub epic_inflight_ttl_secs: u64,
    /// Seconds between sweep-registry reaper ticks. Source:
    /// `sweep_registry::reaper::DEFAULT_REAPER_INTERVAL_SECS` (30). Env:
    /// `LOOM_SWEEP_REAPER_INTERVAL_SECS`. Range `[5, 3600]`.
    pub sweep_reaper_interval_secs: u64,
    /// Whole-second budget on one reaper `gh api` call. Source:
    /// `sweep_registry::reaper::REAP_GH_TIMEOUT_SECS` (5). Env:
    /// `LOOM_REAP_GH_TIMEOUT_SECS`. Range `[1, 600]`.
    pub reap_gh_timeout_secs: u64,
    /// Seconds a sweep's inflight record stays fresh before it is stale.
    /// Source: `inflight::DEFAULT_STALE_SECS` (14400 = 4h). Env:
    /// `LOOM_INFLIGHT_STALE_SECS`. Range `[60, 604800]` (1 min–7 days).
    pub sweep_inflight_stale_secs: u64,
    /// Seconds an API-key pool inflight record stays fresh. Source:
    /// `api_keys_pool::inflight::DEFAULT_STALE_SECS` (14400 = 4h). Env:
    /// `LOOM_API_KEY_INFLIGHT_STALE_SECS`. Range `[60, 604800]`.
    pub api_key_inflight_stale_secs: u64,
    /// Seconds a rate-limited/exhausted token sits in cooldown before
    /// re-probing. Source: `tokens_pool::bad_tokens::DEFAULT_EXHAUSTION_COOLDOWN_SECS`
    /// (21600 = 6h). Env: `LOOM_TOKEN_EXHAUSTION_COOLDOWN_SECS`. Range
    /// `[60, 604800]`.
    pub token_exhaustion_cooldown_secs: u64,
    /// Seconds after which a cleaned-up bad-token record is itself deleted.
    /// Source: `tokens_pool::bad_tokens::DEFAULT_CLEANUP_MAX_AGE_SECS`
    /// (86400 = 24h). No single-knob env (promoted from a bare constant).
    /// Range `[3600, 2592000]` (1 h–30 days).
    pub bad_token_cleanup_max_age_secs: u64,
    /// Minutes of worktree inactivity the activity classifier treats as
    /// "idle". Source: `worktree_activity::DEFAULT_ACTIVITY_WINDOW_MINUTES`
    /// (30). Env: `LOOM_WORKTREE_ACTIVITY_WINDOW_MINUTES`. Range `[1, 1440]`.
    pub worktree_activity_window_minutes: u64,
}

/// Per-worktree resource budgets fed into the admission headroom math
/// (tranche 2). The dynamic concurrency cap stays additionally bounded by
/// real disk/RAM probes — these are the *accounting* shares, not probes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct HeadroomParams {
    /// Gigabytes reserved per live worktree when accounting free disk.
    /// Source: `disk_headroom::DEFAULT_PER_WORKTREE_GB` (2). Env:
    /// `LOOM_PER_WORKTREE_GB`. Range `[1, 1024]`.
    pub per_worktree_gb: u64,
    /// Gigabytes reserved per live worktree when accounting free RAM.
    /// Source: `ram_headroom::DEFAULT_PER_WORKTREE_RAM_GB` (2). Env:
    /// `LOOM_PER_WORKTREE_RAM_GB`. Range `[1, 1024]`.
    pub per_worktree_ram_gb: u64,
}

/// Daemon process-lifecycle timings — restart verification, launchd
/// bootout/bootstrap, and client-side IPC budgets (tranche 2).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ProcessParams {
    /// Seconds between restart-verification polls. Source:
    /// `restart_verify::DEFAULT_POLL_SECS` (30). Env:
    /// `LOOM_DAEMON_RESTART_POLL_SECS`. Range `[5, 3600]`.
    pub restart_poll_secs: u64,
    /// Seconds between kickstart polls while a restart is recovering.
    /// Source: `restart_verify::DEFAULT_RECOVERY_POLL_SECS` (15). Env:
    /// `LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS`. Range `[1, 600]`.
    pub restart_kickstart_poll_secs: u64,
    /// Milliseconds between fast in-process restart polls. Source:
    /// `restart_verify::DEFAULT_POLL_INTERVAL_MS` (1000). Env:
    /// `LOOM_DAEMON_RESTART_POLL_INTERVAL`. Range `[50, 60000]`.
    pub restart_poll_interval_ms: u64,
    /// Seconds to let launchd settle after bootout before re-bootstrap.
    /// Source: `launchd_reload::DEFAULT_BOOTOUT_SETTLE_SECS` (5). Env:
    /// `LOOM_DAEMON_BOOTOUT_SETTLE_SECS`. Range `[1, 120]`.
    pub bootout_settle_secs: u64,
    /// launchd re-bootstrap attempts before giving up. Source:
    /// `launchd_reload::DEFAULT_BOOTSTRAP_RETRY_ATTEMPTS` (4). Env:
    /// `LOOM_DAEMON_BOOTSTRAP_RETRY_ATTEMPTS`. Range `[1, 20]`.
    pub bootstrap_retry_attempts: u64,
    /// Seconds between launchd re-bootstrap attempts. Source:
    /// `launchd_reload::DEFAULT_BOOTSTRAP_RETRY_SECS` (2). Env:
    /// `LOOM_DAEMON_BOOTSTRAP_RETRY_SECS`. Range `[1, 60]`.
    pub bootstrap_retry_secs: u64,
    /// Raise-only floor (milliseconds) on every client-side daemon IPC
    /// round-trip. Source: `cli::common::DEFAULT_IPC_TIMEOUT_MS_FLOOR`
    /// (30000). Env: `LOOM_DAEMON_IPC_TIMEOUT_MS` — like the env var, a
    /// configured value can only *raise* the floor, never lower it (see
    /// `cli::common::apply_ipc_timeout_env_floor`). Range `[1000, 3600000]`.
    pub ipc_timeout_ms: u64,
    /// Whole-second bound on the one `gh api` read the lease co-occupancy
    /// guard makes. Source: `cli::lease_co_occupancy::DEFAULT_TIMEOUT_SECS`
    /// (10). Env: `LOOM_WORKTREE_LEASE_GUARD_TIMEOUT`. Range `[1, 600]`.
    pub lease_guard_timeout_secs: u64,
}

/// Observability ops thresholds (tranche 2).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ObservabilityParams {
    /// Seconds between dispatch-disposition view refreshes. Source:
    /// `observability::ops::disposition::DEFAULT_REFRESH_SECS` (600). Env:
    /// `LOOM_DISPATCH_DISPOSITION_REFRESH_SECS`. Range `[30, 86400]`.
    pub dispatch_disposition_refresh_secs: u64,
    /// Seconds a sweep may sit unclaimed before the ops view flags queue
    /// starvation. Source: `observability::ops::dwell::DEFAULT_STARVATION_SECS`
    /// (21600 = 6h). Env: `LOOM_QUEUE_STARVATION_SECS`. Range
    /// `[300, 604800]`.
    pub queue_starvation_secs: u64,
}

/// Self-update staleness warning thresholds (tranche 2).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct UpdateParams {
    /// Commits behind that trips the stale-install warning. Source:
    /// `self_update::DEFAULT_STALE_WARN_COMMITS` (10). Env:
    /// `LOOM_SELF_UPDATE_STALE_WARN_COMMITS`. Range `[1, 100000]`.
    pub stale_warn_commits: u64,
    /// Hours since the last update that trips the same warning. Source:
    /// `self_update::DEFAULT_STALE_WARN_HOURS` (12). Env:
    /// `LOOM_SELF_UPDATE_STALE_WARN_HOURS`. Range `[1, 720]`.
    pub stale_warn_hours: u64,
}

const EPIC_SUPERVISOR_INTERVAL_SECS_RANGE: (u64, u64) = (30, 3600);
const EPIC_INFLIGHT_TTL_SECS_RANGE: (u64, u64) = (60, 86_400);
const SWEEP_REAPER_INTERVAL_SECS_RANGE: (u64, u64) = (5, 3600);
const REAP_GH_TIMEOUT_SECS_RANGE: (u64, u64) = (1, 600);
const INFLIGHT_STALE_SECS_RANGE: (u64, u64) = (60, 604_800);
const TOKEN_EXHAUSTION_COOLDOWN_SECS_RANGE: (u64, u64) = (60, 604_800);
const BAD_TOKEN_CLEANUP_MAX_AGE_SECS_RANGE: (u64, u64) = (3_600, 2_592_000);
const WORKTREE_ACTIVITY_WINDOW_MINUTES_RANGE: (u64, u64) = (1, 1440);
const PER_WORKTREE_GB_RANGE: (u64, u64) = (1, 1024);
const PER_WORKTREE_RAM_GB_RANGE: (u64, u64) = (1, 1024);
const RESTART_POLL_SECS_RANGE: (u64, u64) = (5, 3600);
const RESTART_KICKSTART_POLL_SECS_RANGE: (u64, u64) = (1, 600);
const RESTART_POLL_INTERVAL_MS_RANGE: (u64, u64) = (50, 60_000);
const BOOTOUT_SETTLE_SECS_RANGE: (u64, u64) = (1, 120);
const BOOTSTRAP_RETRY_ATTEMPTS_RANGE: (u64, u64) = (1, 20);
const BOOTSTRAP_RETRY_SECS_RANGE: (u64, u64) = (1, 60);
const IPC_TIMEOUT_MS_RANGE: (u64, u64) = (1_000, 3_600_000);
const LEASE_GUARD_TIMEOUT_SECS_RANGE: (u64, u64) = (1, 600);
const DISPATCH_DISPOSITION_REFRESH_SECS_RANGE: (u64, u64) = (30, 86_400);
const QUEUE_STARVATION_SECS_RANGE: (u64, u64) = (300, 604_800);
const STALE_WARN_COMMITS_RANGE: (u64, u64) = (1, 100_000);
const STALE_WARN_HOURS_RANGE: (u64, u64) = (1, 720);

pub(super) fn validate_supervision(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("supervision", "must be an object"));
        return;
    }
    const KEYS: &[&str] = &[
        "epicSupervisorIntervalSecs",
        "epicInflightTtlSecs",
        "sweepReaperIntervalSecs",
        "reapGhTimeoutSecs",
        "sweepInflightStaleSecs",
        "apiKeyInflightStaleSecs",
        "tokenExhaustionCooldownSecs",
        "badTokenCleanupMaxAgeSecs",
        "worktreeActivityWindowMinutes",
    ];
    for (key, _) in group.as_object().into_iter().flatten() {
        if !KEYS.contains(&key.as_str()) {
            violations.push(Violation::new(format!("supervision.{key}"), "unknown key"));
        }
    }
    check_u64(
        group,
        "supervision",
        "epicSupervisorIntervalSecs",
        EPIC_SUPERVISOR_INTERVAL_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "supervision",
        "epicInflightTtlSecs",
        EPIC_INFLIGHT_TTL_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "supervision",
        "sweepReaperIntervalSecs",
        SWEEP_REAPER_INTERVAL_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "supervision",
        "reapGhTimeoutSecs",
        REAP_GH_TIMEOUT_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "supervision",
        "sweepInflightStaleSecs",
        INFLIGHT_STALE_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "supervision",
        "apiKeyInflightStaleSecs",
        INFLIGHT_STALE_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "supervision",
        "tokenExhaustionCooldownSecs",
        TOKEN_EXHAUSTION_COOLDOWN_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "supervision",
        "badTokenCleanupMaxAgeSecs",
        BAD_TOKEN_CLEANUP_MAX_AGE_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "supervision",
        "worktreeActivityWindowMinutes",
        WORKTREE_ACTIVITY_WINDOW_MINUTES_RANGE,
        violations,
    );
}

pub(super) fn validate_headroom(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("headroom", "must be an object"));
        return;
    }
    for (key, _) in group.as_object().into_iter().flatten() {
        if !matches!(key.as_str(), "perWorktreeGb" | "perWorktreeRamGb") {
            violations.push(Violation::new(format!("headroom.{key}"), "unknown key"));
        }
    }
    check_u64(group, "headroom", "perWorktreeGb", PER_WORKTREE_GB_RANGE, violations);
    check_u64(group, "headroom", "perWorktreeRamGb", PER_WORKTREE_RAM_GB_RANGE, violations);
}

pub(super) fn validate_process(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("process", "must be an object"));
        return;
    }
    const KEYS: &[&str] = &[
        "restartPollSecs",
        "restartKickstartPollSecs",
        "restartPollIntervalMs",
        "bootoutSettleSecs",
        "bootstrapRetryAttempts",
        "bootstrapRetrySecs",
        "ipcTimeoutMs",
        "leaseGuardTimeoutSecs",
    ];
    for (key, _) in group.as_object().into_iter().flatten() {
        if !KEYS.contains(&key.as_str()) {
            violations.push(Violation::new(format!("process.{key}"), "unknown key"));
        }
    }
    check_u64(group, "process", "restartPollSecs", RESTART_POLL_SECS_RANGE, violations);
    check_u64(
        group,
        "process",
        "restartKickstartPollSecs",
        RESTART_KICKSTART_POLL_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "process",
        "restartPollIntervalMs",
        RESTART_POLL_INTERVAL_MS_RANGE,
        violations,
    );
    check_u64(group, "process", "bootoutSettleSecs", BOOTOUT_SETTLE_SECS_RANGE, violations);
    check_u64(
        group,
        "process",
        "bootstrapRetryAttempts",
        BOOTSTRAP_RETRY_ATTEMPTS_RANGE,
        violations,
    );
    check_u64(group, "process", "bootstrapRetrySecs", BOOTSTRAP_RETRY_SECS_RANGE, violations);
    check_u64(group, "process", "ipcTimeoutMs", IPC_TIMEOUT_MS_RANGE, violations);
    check_u64(
        group,
        "process",
        "leaseGuardTimeoutSecs",
        LEASE_GUARD_TIMEOUT_SECS_RANGE,
        violations,
    );
}

pub(super) fn validate_observability(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("observability", "must be an object"));
        return;
    }
    for (key, _) in group.as_object().into_iter().flatten() {
        if !matches!(key.as_str(), "dispatchDispositionRefreshSecs" | "queueStarvationSecs") {
            violations.push(Violation::new(format!("observability.{key}"), "unknown key"));
        }
    }
    check_u64(
        group,
        "observability",
        "dispatchDispositionRefreshSecs",
        DISPATCH_DISPOSITION_REFRESH_SECS_RANGE,
        violations,
    );
    check_u64(
        group,
        "observability",
        "queueStarvationSecs",
        QUEUE_STARVATION_SECS_RANGE,
        violations,
    );
}

pub(super) fn validate_update(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("update", "must be an object"));
        return;
    }
    for (key, _) in group.as_object().into_iter().flatten() {
        if !matches!(key.as_str(), "staleWarnCommits" | "staleWarnHours") {
            violations.push(Violation::new(format!("update.{key}"), "unknown key"));
        }
    }
    check_u64(group, "update", "staleWarnCommits", STALE_WARN_COMMITS_RANGE, violations);
    check_u64(group, "update", "staleWarnHours", STALE_WARN_HOURS_RANGE, violations);
}

/// Resolve the five tranche-2 groups down the layer (vector > block; no
/// legacy tier for these fields) — the bulk of the parent's
/// `resolve_effective` arms, moved here for the source-file ratchet.
pub(super) fn resolve_tranche2(
    vector: Option<&Value>,
    block: &Value,
    sources: &mut std::collections::BTreeMap<String, super::Source>,
) -> (
    SupervisionParams,
    HeadroomParams,
    ProcessParams,
    ObservabilityParams,
    UpdateParams,
) {
    use super::pick_u64;
    // Tranche-2 groups have no legacy config tier (these knobs were
    // env-only before consolidation), so `legacy` is always `None`
    // here: the chain is vector > block > default.
    let supervision = SupervisionParams {
        epic_supervisor_interval_secs: pick_u64(
            vector,
            block,
            "supervision",
            "epicSupervisorIntervalSecs",
            None,
            DEFAULT_SUPERVISOR_INTERVAL_SECS,
            sources,
        ),
        epic_inflight_ttl_secs: pick_u64(
            vector,
            block,
            "supervision",
            "epicInflightTtlSecs",
            None,
            DEFAULT_INFLIGHT_TTL_SECS,
            sources,
        ),
        sweep_reaper_interval_secs: pick_u64(
            vector,
            block,
            "supervision",
            "sweepReaperIntervalSecs",
            None,
            DEFAULT_SWEEP_REAPER_INTERVAL_SECS,
            sources,
        ),
        reap_gh_timeout_secs: pick_u64(
            vector,
            block,
            "supervision",
            "reapGhTimeoutSecs",
            None,
            REAP_GH_TIMEOUT_SECS,
            sources,
        ),
        sweep_inflight_stale_secs: pick_u64(
            vector,
            block,
            "supervision",
            "sweepInflightStaleSecs",
            None,
            DEFAULT_SWEEP_INFLIGHT_STALE_SECS,
            sources,
        ),
        api_key_inflight_stale_secs: pick_u64(
            vector,
            block,
            "supervision",
            "apiKeyInflightStaleSecs",
            None,
            DEFAULT_API_KEY_INFLIGHT_STALE_SECS,
            sources,
        ),
        token_exhaustion_cooldown_secs: pick_u64(
            vector,
            block,
            "supervision",
            "tokenExhaustionCooldownSecs",
            None,
            DEFAULT_EXHAUSTION_COOLDOWN_SECS as u64,
            sources,
        ),
        bad_token_cleanup_max_age_secs: pick_u64(
            vector,
            block,
            "supervision",
            "badTokenCleanupMaxAgeSecs",
            None,
            DEFAULT_CLEANUP_MAX_AGE_SECS as u64,
            sources,
        ),
        worktree_activity_window_minutes: pick_u64(
            vector,
            block,
            "supervision",
            "worktreeActivityWindowMinutes",
            None,
            DEFAULT_ACTIVITY_WINDOW_MINUTES,
            sources,
        ),
    };
    let headroom = HeadroomParams {
        per_worktree_gb: pick_u64(
            vector,
            block,
            "headroom",
            "perWorktreeGb",
            None,
            DEFAULT_PER_WORKTREE_GB,
            sources,
        ),
        per_worktree_ram_gb: pick_u64(
            vector,
            block,
            "headroom",
            "perWorktreeRamGb",
            None,
            DEFAULT_PER_WORKTREE_RAM_GB,
            sources,
        ),
    };
    let process = ProcessParams {
        restart_poll_secs: pick_u64(
            vector,
            block,
            "process",
            "restartPollSecs",
            None,
            DEFAULT_POLL_SECS,
            sources,
        ),
        restart_kickstart_poll_secs: pick_u64(
            vector,
            block,
            "process",
            "restartKickstartPollSecs",
            None,
            DEFAULT_RECOVERY_POLL_SECS,
            sources,
        ),
        restart_poll_interval_ms: pick_u64(
            vector,
            block,
            "process",
            "restartPollIntervalMs",
            None,
            DEFAULT_POLL_INTERVAL_MS,
            sources,
        ),
        bootout_settle_secs: pick_u64(
            vector,
            block,
            "process",
            "bootoutSettleSecs",
            None,
            DEFAULT_BOOTOUT_SETTLE_SECS,
            sources,
        ),
        bootstrap_retry_attempts: pick_u64(
            vector,
            block,
            "process",
            "bootstrapRetryAttempts",
            None,
            DEFAULT_BOOTSTRAP_RETRY_ATTEMPTS as u64,
            sources,
        ),
        bootstrap_retry_secs: pick_u64(
            vector,
            block,
            "process",
            "bootstrapRetrySecs",
            None,
            DEFAULT_BOOTSTRAP_RETRY_SECS,
            sources,
        ),
        ipc_timeout_ms: pick_u64(
            vector,
            block,
            "process",
            "ipcTimeoutMs",
            None,
            30_000, // cli::common::DEFAULT_IPC_TIMEOUT_MS_FLOOR (drift-guard test there)
            sources,
        ),
        lease_guard_timeout_secs: pick_u64(
            vector,
            block,
            "process",
            "leaseGuardTimeoutSecs",
            None,
            10, // cli::lease_co_occupancy::DEFAULT_TIMEOUT_SECS (drift-guard test there)
            sources,
        ),
    };
    let observability = ObservabilityParams {
        dispatch_disposition_refresh_secs: pick_u64(
            vector,
            block,
            "observability",
            "dispatchDispositionRefreshSecs",
            None,
            DEFAULT_DISPOSITION_REFRESH_SECS as u64,
            sources,
        ),
        queue_starvation_secs: pick_u64(
            vector,
            block,
            "observability",
            "queueStarvationSecs",
            None,
            DEFAULT_QUEUE_STARVATION_SECS as u64,
            sources,
        ),
    };
    let update = UpdateParams {
        stale_warn_commits: pick_u64(
            vector,
            block,
            "update",
            "staleWarnCommits",
            None,
            DEFAULT_STALE_WARN_COMMITS as u64,
            sources,
        ),
        stale_warn_hours: pick_u64(
            vector,
            block,
            "update",
            "staleWarnHours",
            None,
            DEFAULT_STALE_WARN_HOURS as u64,
            sources,
        ),
    };

    (supervision, headroom, process, observability, update)
}
