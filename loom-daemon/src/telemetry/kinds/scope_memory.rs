//! `scope.memory` (#12241): one agent scope's memory limit, peak and OOM kills,
//! recorded when the scope ends.
//!
//! The per-agent-scope `MemoryMax` (#11094) decides whether a build is
//! OOM-killed, but none of its inputs or results were exported: diagnosing a
//! kill needed the host's kernel log and cgroup files. The daemon already
//! samples every `loom-agent-*.scope` each work-finder tick
//! (`crate::ram_peaks`); this record is its account of a scope that ended:
//! the applied limit and where it came from, the peak, the `memory.events`
//! `max` / `oom_kill` counters, and whether the peak was clipped at the limit.
//!
//! **OTLP only**, on the daemon's direct exporter. The scalars ride as
//! `loom.scope.*` attributes ([`SCOPE_MEMORY_LOG_ATTRIBUTE_KEYS`], which the
//! collector's log `keep_keys` must list; contract-tested). The body is the
//! record's JSON. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use crate::ram_peaks::LimitSource;
use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id`, `loom.repo`, `loom.repo.visibility`, `loom.role` and
/// `loom.issue` (already kept). The collector's `transform/privacy` log
/// `keep_keys` must list each.
pub const SCOPE_MEMORY_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.scope.unit",
    "loom.scope.workspace",
    "loom.scope.limit_bytes",
    "loom.scope.limit_source",
    "loom.scope.peak_bytes",
    "loom.scope.events_max",
    "loom.scope.oom_kills",
    "loom.scope.clipped",
];

/// The `scope.memory` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeMemoryRecord {
    /// The systemd scope unit (`loom-agent-….scope`).
    pub scope: String,
    /// The workspace's directory name (the peak-history key), or
    /// `(unattributed)` for a scope outside every workspace.
    pub workspace: String,
    /// `owner/repo` when the workspace's slug is already resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// `public` / `private`, alongside `repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_visibility: Option<String>,
    /// The role the scope ran (`builder`, `judge`, …; `sweep` for an issue
    /// sweep; `unknown` when nothing names it).
    pub role: String,
    /// The claimed issue, for an issue-lock scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    /// The scope's applied `MemoryMax` (`memory.max`) in bytes; absent = none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
    /// Where the limit came from.
    pub limit_source: LimitSource,
    /// The scope's `memory.peak` at its last sample.
    pub peak_bytes: u64,
    /// `memory.events` `max`: times usage hit the limit.
    pub events_max: u64,
    /// `memory.events` `oom_kill`: processes OOM-killed inside the scope.
    pub oom_kills: u64,
    /// The peak reached the limit, or the scope hit it at all: the peak is the
    /// cap, not the real need, and was not recorded in the peak history.
    pub clipped: bool,
    /// When the daemon saw the scope gone.
    pub observed_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_source_serializes_as_its_wire_spelling() {
        for s in [
            LimitSource::Env,
            LimitSource::Config,
            LimitSource::History,
            LimitSource::Floor,
            LimitSource::None,
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
    }

    #[test]
    fn collector_keeps_every_scope_memory_attribute() {
        const CONFIG: &str =
            include_str!("../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .expect("the transform/privacy log keep_keys line");
        for key in SCOPE_MEMORY_LOG_ATTRIBUTE_KEYS.iter().chain(&[
            "loom.repo",
            "loom.repo.visibility",
            "loom.role",
            "loom.issue",
        ]) {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
