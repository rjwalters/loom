//! `backlog.level` (#11819): the backlog level the daemon handed one Curator
//! dispatch.
//!
//! A repo that sets `curator.backlogTarget` gets a keep-or-close admission
//! step in its Curator, and how strict that step is depends on the ratio of
//! the repo's open issues to that target. The daemon computes the level at
//! dispatch (`role_runner::backlog`) and passes it as `--backlog-level`; this
//! record is the daemon's own account of that decision, one per dispatch where
//! a target is set. Nothing is written for a repo with no target, and the
//! Curator agent writes no telemetry of its own: its closes are recorded from
//! GitHub's `closed` events.
//!
//! **OTLP only**, on the daemon's direct exporter. The scalars ride as
//! `loom.backlog.*` attributes ([`BACKLOG_LEVEL_LOG_ATTRIBUTE_KEYS`], which the
//! collector's log `keep_keys` must list; contract-tested). The body is the
//! record's JSON. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id`, `loom.repo`, `loom.role` and `loom.sweep_id` (already
/// kept). The collector's `transform/privacy` log `keep_keys` must list each.
pub const BACKLOG_LEVEL_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.backlog.level",
    "loom.backlog.open",
    "loom.backlog.target",
    "loom.backlog.count_failed",
];

/// How strict the Curator's admission step is, from `r = open / target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BacklogLevel {
    /// `r < 0.5`: today's behavior; the step is a no-op.
    Lenient,
    /// `0.5 <= r <= 1.0`, and the fallback when the count fails.
    Normal,
    /// `r > 1.0`: close unless a keep rule holds.
    Strict,
}

impl BacklogLevel {
    /// The level for `open` issues against a positive `target`, in integer
    /// arithmetic so the boundaries are exact: `r < 0.5` is `2*open < target`,
    /// `r <= 1.0` is `open <= target`.
    #[must_use]
    pub fn for_counts(open: u64, target: u64) -> Self {
        if open.saturating_mul(2) < target {
            Self::Lenient
        } else if open <= target {
            Self::Normal
        } else {
            Self::Strict
        }
    }

    /// The wire / flag spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lenient => "lenient",
            Self::Normal => "normal",
            Self::Strict => "strict",
        }
    }
}

/// The `backlog.level` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacklogLevelRecord {
    /// `owner/repo`, lowercased.
    pub repo: String,
    /// Open issues (PRs excluded) at dispatch; absent when the count failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<u64>,
    /// The resolved `curator.backlogTarget`.
    pub target: u64,
    /// The level passed to the Curator.
    pub level: BacklogLevel,
    /// The dispatch's id (`role-curator-<instant>`).
    pub dispatch_id: String,
    /// Why the count failed, when it did (the level is then `normal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count_error: Option<String>,
    /// When the daemon computed the level.
    pub observed_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_boundaries_are_exact() {
        assert_eq!(BacklogLevel::for_counts(49, 100), BacklogLevel::Lenient);
        assert_eq!(BacklogLevel::for_counts(50, 100), BacklogLevel::Normal);
        assert_eq!(BacklogLevel::for_counts(100, 100), BacklogLevel::Normal);
        assert_eq!(BacklogLevel::for_counts(101, 100), BacklogLevel::Strict);
        assert_eq!(BacklogLevel::for_counts(0, 1), BacklogLevel::Lenient);
        assert_eq!(BacklogLevel::for_counts(u64::MAX, 1), BacklogLevel::Strict);
    }

    #[test]
    fn level_serializes_as_its_flag_spelling() {
        for level in [
            BacklogLevel::Lenient,
            BacklogLevel::Normal,
            BacklogLevel::Strict,
        ] {
            assert_eq!(serde_json::to_value(level).unwrap(), level.as_str());
        }
    }

    #[test]
    fn collector_keeps_every_backlog_level_attribute() {
        const CONFIG: &str =
            include_str!("../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .expect("the transform/privacy log keep_keys line");
        for key in BACKLOG_LEVEL_LOG_ATTRIBUTE_KEYS.iter().chain(&[
            "loom.repo",
            "loom.role",
            "loom.sweep_id",
        ]) {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
