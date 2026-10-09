//! `auto_update.tick` (#10414): one record per self-update loop decision.
//!
//! Before this kind the self-update loop had no telemetry at all. When both AWS
//! workers stopped rolling at ~01:30Z on 2026-10-05, SigNoz could answer
//! neither "is this host current?" nor "why not?". This record answers both on
//! every tick (default every 900 s). It names what the tick decided, the
//! installed and target versions, why a roll was held back, and the drain
//! state.
//!
//! **OTLP only**, like the `eta.*` log kinds. The scalars ride as
//! `loom.auto_update.*` attributes ([`AUTO_UPDATE_LOG_ATTRIBUTE_KEYS`], which
//! the collector's log `keep_keys` must list; contract-tested). The body is the
//! record's JSON.
//!
//! **Provenance is required**: the running build's version, full revision and
//! tree state ride on every record, so "which build decided this" never needs a
//! join. A record whose provenance does not validate is not emitted.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::eta::Provenance;

/// Every log attribute key `auto_update.tick` exports. The collector's
/// `transform/privacy` log `keep_keys` must list each one
/// (`defaults/observability/collector/config.yaml`, contract-tested).
pub const AUTO_UPDATE_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.auto_update.tick_id",
    "loom.auto_update.decision",
    "loom.auto_update.reason",
    "loom.auto_update.outcome",
    "loom.auto_update.roll_armed",
    "loom.auto_update.installed_version",
    "loom.auto_update.target_version",
    "loom.auto_update.target_published_at",
    "loom.auto_update.commits_behind",
    "loom.auto_update.hours_behind",
    "loom.auto_update.in_flight",
    "loom.auto_update.drain_armed",
    "loom.auto_update.drain_pending",
    "loom.auto_update.drain_refusals",
    "loom.auto_update.drain_target",
    "loom.auto_update.consecutive_failures",
    "loom.auto_update.duration_ms",
    "loom.auto_update.version",
    "loom.auto_update.revision",
    "loom.auto_update.tree_state",
    "loom.auto_update.provenance_complete",
];

/// What one tick decided. A closed set, so dashboards can group on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickDecisionKind {
    /// Nothing to roll onto: up to date, or nothing decidable.
    Skip,
    /// A newer target exists, but a gate held the roll this tick (settle
    /// window, backoff, terminal failure, in-flight sweeps, roll window).
    Defer,
    /// The artifact resolved from a repo other than the one this binary was
    /// built from (#8513): no progress is possible until that is fixed.
    StaleRepo,
    /// A release artifact fetch ran.
    Fetch,
    /// A source rebuild ran.
    Rebuild,
    /// A roll or drain is already armed; the tick leaves it to finish.
    DrainWait,
    /// The tick panicked. The loop recorded it and keeps running.
    Panic,
}

impl TickDecisionKind {
    /// The wire spelling (`skip`, `defer`, …).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Defer => "defer",
            Self::StaleRepo => "stale_repo",
            Self::Fetch => "fetch",
            Self::Rebuild => "rebuild",
            Self::DrainWait => "drain_wait",
            Self::Panic => "panic",
        }
    }
}

/// The drain the tick saw armed when it started, if any.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainSnapshot {
    /// A drain/roll is armed.
    pub armed: bool,
    /// It can no longer be superseded: its pause has stopped an agent, or it
    /// is an operator drain. (Before #10831: it had survived a deadline
    /// refusal.)
    pub pending: bool,
    /// Always `0` since #10831: a roll no longer refuses deadlines.
    pub refusals: u32,
    /// The artifact identity it rolls to, when the auto-updater armed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// One self-update tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoUpdateTickRecord {
    /// Derived, never random: `derived_hex(["loom.auto_update.tick", host,
    /// started_at])`.
    pub tick_id: String,
    /// When the tick started; the record's time.
    pub started_at: DateTime<Utc>,
    /// What the tick decided.
    pub decision: TickDecisionKind,
    /// The tick's note: why it skipped/deferred, or the roll's outcome. This is
    /// the same text `loom-daemon status` shows as `last tick:`.
    pub reason: String,
    /// `success` / `retryable` / `terminal` for a fetch or rebuild.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// A successful fetch/rebuild whose drain-and-restart was accepted: the
    /// host is rolling.
    pub roll_armed: bool,
    /// The installed binary's version as the artifact probe read it, else the
    /// running build's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// The newest release artifact resolved for this host, when one resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    /// Its publish time, as the release reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_published_at: Option<String>,
    /// Source-path staleness: commits behind the checkout's HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits_behind: Option<u32>,
    /// Source-path staleness: hours behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours_behind: Option<u32>,
    /// In-flight sweeps the tick read, when it read them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<u64>,
    /// The drain armed at tick start.
    pub drain: DrainSnapshot,
    /// #10712: the fleet floor is above every published release, so this host
    /// cannot reach it (the alert text). Absent when the floor is unset,
    /// satisfied, or being rolled to. Raises the record's severity to ERROR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_stall: Option<String>,
    /// Consecutive retryable failures for the tracked target.
    pub consecutive_failures: u32,
    /// Wall time the tick took, milliseconds.
    pub duration_ms: u64,
    /// The deciding (running) build.
    pub loom: Provenance,
}

impl AutoUpdateTickRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}
