//! GitHub Actions CI telemetry poller (Issue #8824 — phase 1 of 3 of the
//! build/CI observability work under epic #8522).
//!
//! Captures every completed GitHub Actions **run** and **job** of one forge
//! org as first-class telemetry: `ci.run` / `ci.job` log records, the
//! `loom.ci.{run,job}.duration_ms` histograms (via `ci.duration`), and one
//! trace per run with one span per job. The records land in a local journal
//! (`.loom/logs/ci-telemetry.jsonl`) unconditionally, and the daemon's
//! existing observability backfill pass ([`export::backfill`]) offers them to
//! whichever exporter(s) `observability.*` configures — no new transport.
//!
//! # Surfaces
//!
//! - `loom-daemon ci-telemetry --once` — one poll cycle ([`poll::run_cycle`]).
//! - `loom-daemon ci-telemetry status` — ledger/watermark/health summary.
//! - A daemon-integrated periodic poller ([`spawn_task`]) when
//!   `autonomous.ciTelemetry.enabled=true` (default **false**, FLAGS-OFF).
//!
//! # "Never do the same job twice"
//!
//! The durable ledger ([`ledger`], `.loom/state/ci-telemetry/seen.jsonl`) is
//! the commit point: a unit's envelopes are appended and fsynced to the
//! ledger **before** they are written to the journal, and a unit becomes
//! "seen" only once its ledger line is durable. A crash after the commit but
//! before (or during) the journal write is repaired on the next cycle by
//! replaying only the envelopes the journal does not already hold — so every
//! run/job reaches the journal exactly once across restarts, re-polls and
//! re-runs. See `defaults/docs/ci-observability.md` for the full contract.
//!
//! # Multi-host posture
//!
//! One poller per org is the normal case. A per-host file lock
//! ([`state::CycleLock`]) serialises the CLI and the daemon poller on one
//! host; there is deliberately no cross-host lease (out of scope — one
//! mechanism per behaviour). Every record carries its stable GitHub identity
//! (`run_id` / `job_id`, and deterministic trace/span ids), so a second
//! poller on another host produces byte-identical identities a backend can
//! deduplicate on.

pub mod api;
pub mod export;
pub mod journal;
pub mod ledger;
pub mod poll;
pub mod records;
pub mod state;

use std::path::{Path, PathBuf};
use std::time::Duration;

/// `autonomous.ciTelemetry.enabled` env override.
pub const ENABLED_ENV: &str = "LOOM_CI_TELEMETRY_ENABLED";
/// `autonomous.ciTelemetry.org` env override.
pub const ORG_ENV: &str = "LOOM_CI_TELEMETRY_ORG";
/// `autonomous.ciTelemetry.intervalSecs` env override.
pub const INTERVAL_SECS_ENV: &str = "LOOM_CI_TELEMETRY_INTERVAL_SECS";
// `autonomous.ciTelemetry.excludedRepos` deliberately has NO env override:
// the ci-observability policy requires every exclusion to live in committed
// config with a recorded reason, never in a host-local tier (env included).
/// `autonomous.ciTelemetry.logCaptureEnabled` env override.
pub const LOG_CAPTURE_ENABLED_ENV: &str = "LOOM_CI_TELEMETRY_LOG_CAPTURE_ENABLED";

/// Default org when no tier sets one.
pub const DEFAULT_ORG: &str = "2amlogic";
/// Default poll cadence.
pub const DEFAULT_INTERVAL_SECS: u64 = 120;
/// How far back the very first poll of a repo (no watermark yet) looks.
pub const INITIAL_LOOKBACK_HOURS: i64 = 24;

/// Dotted path of the repo-exclusion key.
pub const EXCLUDED_REPOS_KEY: &str = "autonomous.ciTelemetry.excludedRepos";

/// One admitted `excludedRepos` entry: the repo (bare `name` or
/// `owner/name`, matched case-insensitively) and the reason recorded with it.
///
/// The ci-observability policy makes run/job records and metrics
/// unconditional; excluding a repo is a **policy exception** that must carry
/// its reason in committed config (see `defaults/docs/ci-observability.md`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RepoExclusion {
    pub repo: String,
    pub reason: String,
}

/// The raw `autonomous.ciTelemetry` block, before env/default resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CiTelemetryConfig {
    pub enabled: Option<bool>,
    pub org: Option<String>,
    pub interval_secs: Option<u64>,
    /// Admitted exclusions (committed tiers only, each with a reason).
    pub excluded_repos: Option<Vec<RepoExclusion>>,
    /// `excludedRepos` entries refused by the policy, each as a named
    /// reason. A refused entry excludes nothing — the repo is still polled.
    pub refused_exclusions: Vec<String>,
    pub log_capture_enabled: Option<bool>,
}

/// Split an `excludedRepos` value into admitted entries and named refusals.
/// Only `{ "repo": "<non-empty>", "reason": "<non-empty>" }` is admitted.
#[must_use]
pub fn parse_exclusions(value: &serde_json::Value) -> (Vec<RepoExclusion>, Vec<String>) {
    let Some(entries) = value.as_array() else {
        return (Vec::new(), vec![format!("{EXCLUDED_REPOS_KEY} is not an array: {value}")]);
    };
    let mut admitted = Vec::new();
    let mut refused = Vec::new();
    for entry in entries {
        let field = |name: &str| {
            entry
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        match (field("repo"), field("reason")) {
            (Some(repo), Some(reason)) => admitted.push(RepoExclusion { repo, reason }),
            (Some(repo), None) => refused
                .push(format!("{repo}: refused — an exclusion must record a non-empty \"reason\"")),
            _ => refused.push(format!(
                "{entry}: refused — an exclusion must be {{\"repo\": …, \"reason\": …}}"
            )),
        }
    }
    (admitted, refused)
}

/// Read `excludedRepos` from the **committed** tiers only (`.loom/config.json`
/// deep-merged with `.loom-project/project.json`). A value that a host-local
/// or shared-defaults tier adds or changes is refused by name.
fn read_exclusions(
    root: &Path,
    effective: &serde_json::Value,
) -> (Vec<RepoExclusion>, Vec<String>) {
    use crate::config_resolver::{
        deep_merge, get_path, soft_read_json_object, LEGACY_CONFIG_REL, PROJECT_CONFIG_REL,
    };
    let committed = deep_merge(
        &soft_read_json_object(&root.join(LEGACY_CONFIG_REL)),
        &soft_read_json_object(&root.join(PROJECT_CONFIG_REL)),
    );
    let committed_value = get_path(&committed, EXCLUDED_REPOS_KEY);
    let effective_value = get_path(effective, EXCLUDED_REPOS_KEY);
    let (admitted, mut refused) = committed_value.map(parse_exclusions).unwrap_or_default();
    if effective_value.is_some() && effective_value != committed_value {
        let source = crate::config_resolver::source_of(root, EXCLUDED_REPOS_KEY)
            .map_or_else(|| "a non-committed tier".to_string(), |p| p.display().to_string());
        refused.push(format!(
            "{EXCLUDED_REPOS_KEY} from {source}: refused — exclusions must live in committed config"
        ));
    }
    (admitted, refused)
}

/// Read `autonomous.ciTelemetry` from `root`'s effective config.
#[must_use]
pub fn read_config(root: &Path) -> CiTelemetryConfig {
    let config = crate::config_resolver::resolve_effective_config(root);
    let Some(block) = crate::config_resolver::get_path(&config, "autonomous.ciTelemetry") else {
        return CiTelemetryConfig::default();
    };
    let (admitted, refused_exclusions) = read_exclusions(root, &config);
    CiTelemetryConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        org: block
            .get("org")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0),
        excluded_repos: (!admitted.is_empty()).then_some(admitted),
        refused_exclusions,
        log_capture_enabled: block
            .get("logCaptureEnabled")
            .and_then(serde_json::Value::as_bool),
    }
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| {
        matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The fully resolved settings (**env > config > default**).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCiTelemetry {
    pub enabled: bool,
    pub org: String,
    pub interval_secs: u64,
    /// Admitted exclusions: repos (bare `name` or `owner/name`) never polled,
    /// each with its recorded reason. Config-only — no env override.
    pub excluded_repos: Vec<RepoExclusion>,
    /// Refused `excludedRepos` entries (named reasons); they exclude nothing.
    pub refused_exclusions: Vec<String>,
    /// Whether phase-2 job-log capture was *requested*. This phase refuses
    /// to honour it — see [`log_capture_gate`].
    pub log_capture_requested: bool,
}

/// Resolve every knob, **env > config > default**.
#[must_use]
pub fn resolve(config: &CiTelemetryConfig) -> ResolvedCiTelemetry {
    ResolvedCiTelemetry {
        enabled: env_bool(ENABLED_ENV).or(config.enabled).unwrap_or(false),
        org: env_nonempty(ORG_ENV)
            .or_else(|| config.org.clone())
            .unwrap_or_else(|| DEFAULT_ORG.to_string()),
        interval_secs: env_nonempty(INTERVAL_SECS_ENV)
            .and_then(|v| v.parse().ok())
            .filter(|v: &u64| *v > 0)
            .or(config.interval_secs)
            .unwrap_or(DEFAULT_INTERVAL_SECS),
        excluded_repos: config.excluded_repos.clone().unwrap_or_default(),
        refused_exclusions: config.refused_exclusions.clone(),
        log_capture_requested: env_bool(LOG_CAPTURE_ENABLED_ENV)
            .or(config.log_capture_enabled)
            .unwrap_or(false),
    }
}

/// Phase-1 job-log capture gate. Log download is phase 2 of this work
/// (non-goal of #8824): the config key exists so the follow-on can flip it,
/// but this build has **no** log-capture code path, so a request is refused
/// by name rather than half-honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogCaptureGate {
    /// Not requested (the default).
    Off,
    /// Requested via `logCaptureEnabled`, refused: not implemented in phase 1.
    RefusedNotImplemented,
}

impl LogCaptureGate {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LogCaptureGate::Off => "off",
            LogCaptureGate::RefusedNotImplemented => {
                "requested but refused: job-log capture is not implemented in phase 1 (#8824)"
            }
        }
    }
}

/// Evaluate the log-capture gate for `resolved`.
#[must_use]
pub fn log_capture_gate(resolved: &ResolvedCiTelemetry) -> LogCaptureGate {
    if resolved.log_capture_requested {
        LogCaptureGate::RefusedNotImplemented
    } else {
        LogCaptureGate::Off
    }
}

/// `<root>/.loom/state/ci-telemetry/` — the ledger, status, discovery cache,
/// export cursor and lock all live here.
#[must_use]
pub fn state_dir(root: &Path) -> PathBuf {
    root.join(".loom").join("state").join("ci-telemetry")
}

/// `<root>/.loom/logs/ci-telemetry.jsonl` — the local journal.
#[must_use]
pub fn journal_path(root: &Path) -> PathBuf {
    root.join(".loom").join("logs").join("ci-telemetry.jsonl")
}

/// Spawn the daemon-integrated periodic poller, or return `None` when
/// `autonomous.ciTelemetry.enabled` resolves false (the default — zero side
/// effects, no task, no file I/O).
#[must_use]
pub fn spawn_task(root: PathBuf) -> Option<tokio::task::JoinHandle<()>> {
    let resolved = resolve(&read_config(&root));
    if !resolved.enabled {
        log::debug!("ci_telemetry: disabled (set autonomous.ciTelemetry.enabled=true to opt in)");
        return None;
    }
    if log_capture_gate(&resolved) == LogCaptureGate::RefusedNotImplemented {
        log::warn!(
            "ci_telemetry: logCaptureEnabled={}",
            LogCaptureGate::RefusedNotImplemented.as_str()
        );
    }
    for refusal in &resolved.refused_exclusions {
        log::warn!("ci_telemetry: excludedRepos entry {refusal} (the repo is still polled)");
    }
    let interval = Duration::from_secs(resolved.interval_secs);
    log::info!(
        "ci_telemetry: enabled (org={}, interval={}s, excluded={:?})",
        resolved.org,
        interval.as_secs(),
        resolved.excluded_repos
    );
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let root = root.clone();
            let resolved = resolved.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let api = api::GhCliApi::from_env();
                poll::run_cycle(&poll::CycleContext::new(&root, &resolved), &api)
            })
            .await;
            match outcome {
                Ok(Ok(report)) => log::info!("ci_telemetry: {}", report.summary()),
                Ok(Err(error)) => log::warn!("ci_telemetry: cycle failed: {error}"),
                Err(error) => log::warn!("ci_telemetry: cycle task panicked: {error}"),
            }
        }
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
