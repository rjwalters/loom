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
}

/// The OTLP log attributes `sweep.started` exports for these facts. The
/// collector's `transform/privacy` log `keep_keys` must list each one
/// (`defaults/observability/collector/config.yaml`, contract-tested).
pub const SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.model",
    "loom.effort",
    "loom.model_source",
    "loom.runtime",
    "loom.attempt_index",
    "loom.trigger",
    "loom.previous_sweep_id",
];

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
    /// [`model_source::EXPLICIT`] or [`model_source::DEFAULT`].
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
        for key in SWEEP_START_FACT_LOG_ATTRIBUTE_KEYS {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
