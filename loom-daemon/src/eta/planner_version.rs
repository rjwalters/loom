//! The planner's identity (#10528, slice a): which scheduling regime an
//! estimate was served under or a training row was observed in.
//!
//! `<daemon version>+<first 12 hex of sha256 over the canonical JSON of the
//! planner-relevant config>`. The planner-relevant config is the
//! `autonomous.workFinder` block (which carries the slot and per-repo caps)
//! and `autonomous.mergeSequencing` when present. Canonical JSON has sorted
//! keys and no whitespace, so key order and unrelated config (`terminals`,
//! the `eta` block) never change the stamp. No random ids: the stamp is a pure
//! function of its inputs (trace-identity policy).

use super::tracker::Emission;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::OnceLock;

/// Hex characters of the config digest kept in the stamp.
const DIGEST_HEX: usize = 12;

/// The planner-relevant slice of `.loom/config.json`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlannerConfigView {
    /// `autonomous.workFinder` (slot and per-repo caps live here).
    pub work_finder: Option<Value>,
    /// `autonomous.mergeSequencing`.
    pub merge_sequencing: Option<Value>,
}

impl PlannerConfigView {
    /// Pick the planner-relevant blocks out of a whole config document.
    #[must_use]
    pub fn from_config(config: &Value) -> Self {
        let autonomous = config.get("autonomous");
        let pick = |key: &str| autonomous.and_then(|a| a.get(key)).cloned();
        Self {
            work_finder: pick("workFinder"),
            merge_sequencing: pick("mergeSequencing"),
        }
    }

    /// Read `<workspace>/.loom/config.json`; an unreadable or malformed file
    /// is the empty view.
    #[must_use]
    pub fn read(workspace_root: &Path) -> Self {
        std::fs::read_to_string(workspace_root.join(".loom/config.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map_or_else(Self::default, |config| Self::from_config(&config))
    }
}

/// Write `value` with object keys sorted and no whitespace.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// The planner's identity: `<version>+<12 hex>`.
#[must_use]
pub fn planner_version(version: &str, cfg: &PlannerConfigView) -> String {
    let mut text = String::from("{\"mergeSequencing\":");
    canonical(cfg.merge_sequencing.as_ref().unwrap_or(&Value::Null), &mut text);
    text.push_str(",\"workFinder\":");
    canonical(cfg.work_finder.as_ref().unwrap_or(&Value::Null), &mut text);
    text.push('}');
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{version}+{}", &hex[..DIGEST_HEX])
}

/// Stamps a pass's emissions with this daemon's planner regime: the one serve
/// seam, so no heuristic sets it. The regime is resolved from the workspace
/// config on first use and kept for the process (like the rest of the ETA
/// config, a change needs a daemon restart).
pub trait Stamped {
    /// `self` with every explanation's `planner_version` set.
    #[must_use]
    fn stamped(self, workspace_root: &Path) -> Self;
}

impl Stamped for Vec<Emission> {
    fn stamped(mut self, workspace_root: &Path) -> Self {
        static REGIME: OnceLock<String> = OnceLock::new();
        let regime = REGIME.get_or_init(|| {
            planner_version(env!("CARGO_PKG_VERSION"), &PlannerConfigView::read(workspace_root))
        });
        for emission in &mut self {
            emission.explanation.planner_version = Some(regime.clone());
        }
        self
    }
}
