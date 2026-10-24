//! What a dispatch knew about the sweep it started (Issue #11280, #10196):
//! the model, effort, runtime and attempt lineage, carried on `sweep.started`
//! and on every `fleet.state` row of a sweep this host runs.
//!
//! A fit may use only what was knowable at its as-of instant, and a live
//! estimate must see the same inputs. Before #11280 these facts reached SigNoz
//! only on `sweep.outcome`, after the sweep ended. The key names match
//! `sweep.outcome`'s, so a reader joins the two by name.

use serde::{Deserialize, Serialize};

/// `model_source` values.
pub mod model_source {
    /// The dispatch request named the model.
    pub const EXPLICIT: &str = "explicit";
    /// The daemon chose it (config, experiment arm or runtime default), or
    /// the runtime picks its own (no `model`).
    pub const DEFAULT: &str = "default";
    /// No model id could be named (empty, or the runtime picks its own); the
    /// `model` fact is then absent (#11370).
    pub const UNKNOWN: &str = "unknown";
}

/// `effort_source` values (#11370).
pub mod effort_source {
    /// The dispatch request named the effort.
    pub const EXPLICIT: &str = "explicit";
    /// A config, environment or runtime default applies.
    pub const DEFAULT: &str = "default";
}

/// Who or what asked for a sweep (Issue #11444), exported as
/// `loom.dispatch_origin` on `sweep.started` and `sweep.outcome`.
///
/// A closed, low-cardinality vocabulary serialized as a snake_case string
/// like `trigger`. Every production caller of the dispatch core names its
/// own origin; nothing defaults to [`Self::Operator`]. An IPC request that
/// does not say (a legacy client) records [`Self::Unknown`], counted and
/// never guessed. A value this daemon does not know (a newer client) also
/// reads as `unknown` rather than refusing the dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchOrigin {
    /// A human asked: `loom-daemon dispatch`, the `dispatch_sweep` MCP tool,
    /// or a Safehouse ChatOps `dispatch` command.
    Operator,
    /// The daemon's autonomous work finder picked the issue.
    WorkFinder,
    /// The epic supervisor dispatched the next phase.
    EpicSupervisor,
    /// A registry watchdog re-dispatched after a stall or early exit.
    Watchdog,
    /// The reaper resumed a crashed sweep from its checkpoint.
    ReaperRetry,
    /// The requester did not say, or named a value this daemon does not know.
    #[serde(other)]
    Unknown,
}

impl DispatchOrigin {
    /// The wire value, as exported on `loom.dispatch_origin`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::WorkFinder => "work_finder",
            Self::EpicSupervisor => "epic_supervisor",
            Self::Watchdog => "watchdog",
            Self::ReaperRetry => "reaper_retry",
            Self::Unknown => "unknown",
        }
    }

    /// The origin an IPC `DispatchSweep` request records: what it named, or
    /// `unknown` when the field was absent. Never assumed `operator`, since
    /// automated tools and legacy clients share the same request.
    #[must_use]
    pub fn from_request(origin: Option<Self>) -> Self {
        origin.unwrap_or(Self::Unknown)
    }

    /// The `trigger` this dispatch records, given the journal-derived one
    /// (Issue #11444). Only an operator re-dispatch of an issue that already
    /// has an attempt on this host becomes `operator_redispatch`; a first
    /// attempt stays `first`, and every other origin keeps the journal
    /// derivation, which never yields `operator_redispatch` on its own.
    #[must_use]
    pub fn apply_to_trigger(self, derived: &'static str) -> &'static str {
        if self == Self::Operator && derived != super::trigger::FIRST {
            super::trigger::OPERATOR_REDISPATCH
        } else {
            derived
        }
    }
}

/// The OTLP log attributes `sweep.started` exports for these facts. The
/// collector's `transform/privacy` log `keep_keys` must list each one
/// (`defaults/observability/collector/config.yaml`, contract-tested).
pub const SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.model",
    "loom.effort",
    "loom.effort_source",
    "loom.model_source",
    "loom.runtime",
    "loom.attempt_index",
    "loom.trigger",
    "loom.previous_sweep_id",
    "loom.dispatch_origin",
];

/// The `sweep.outcome` log attributes [`SweepRoleDraw`] maps to. The
/// collector's log `keep_keys` must include every key (contract-tested).
pub const SWEEP_ROLE_DRAW_LOG_ATTRIBUTE_KEYS: &[&str] =
    &["loom.role_draw.pick", "loom.role_draw.model_pinned"];

/// The weighted agent draw that chose a sweep's tap (#11726;
/// 2AMLogic/2am#4393): the grouping keys only. The full decision record
/// (seed, table, rolls) stays on the daemon log's preference marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepRoleDraw {
    /// The candidate key the draw picked (`none` when nothing could launch).
    pub pick: String,
    /// Whether the sweep was launched pinned to the pick's `/<model>`.
    /// `false` today: a sweep does not pin `/model`, so the pick's model is
    /// not what the sweep ran (rjwalters/loom#11782 item 2).
    pub model_pinned: bool,
}

impl SweepRoleDraw {
    /// The summary of `draw`, as a sweep records it.
    #[must_use]
    pub fn from_draw(draw: &crate::runtime_preference::role_draw::RoleDrawRecord) -> Self {
        Self {
            pick: draw.pick.clone().unwrap_or_else(|| "none".into()),
            model_pinned: false,
        }
    }
}

/// Dispatch-time facts of one sweep. Every field is absent when unknown,
/// never guessed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepStartFacts {
    /// The model the sweep was launched with, as `sweep.outcome`'s `model`
    /// names the dispatched one. Absent when the runtime picks its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning-effort level, when one was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// [`effort_source::EXPLICIT`] or [`effort_source::DEFAULT`]; absent with
    /// `effort` (#11370).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_source: Option<String>,
    /// [`model_source::EXPLICIT`], [`model_source::DEFAULT`] or
    /// [`model_source::UNKNOWN`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<String>,
    /// The admitted runtime adapter (`claude`, `codex`, ...). Never defaulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// 1-based attempt ordinal for this repo#issue on this host's outcome
    /// journal, the same derivation `sweep.outcome` uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_index: Option<u32>,
    /// Why this attempt was dispatched (`crate::telemetry::trigger`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// The previous attempt's `sweep_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_sweep_id: Option<String>,
    /// Who asked for this sweep ([`DispatchOrigin`], Issue #11444).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_origin: Option<String>,
    /// The weighted agent draw that chose the tap, when a `roleDraw` table
    /// decided (2AMLogic/2am#4393). Carried to `sweep.outcome`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_draw: Option<SweepRoleDraw>,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS;

    #[test]
    fn collector_keeps_every_sweep_start_fact_attribute() {
        const CONFIG: &str = include_str!("../../../defaults/observability/collector/config.yaml");
        let log_keep = CONFIG
            .lines()
            .find(|l| {
                l.contains("keep_keys(attributes, [")
                    && l.contains("loom.ci.chunk_index")
                    && l.contains("loom.eta.pr.state")
            })
            .expect("the transform/privacy log keep_keys line");
        for key in SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS
            .iter()
            .chain(super::SWEEP_ROLE_DRAW_LOG_ATTRIBUTE_KEYS)
        {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
