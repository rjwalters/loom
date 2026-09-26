//! Kimi burn source: `<root>/sessions/<workdir>/<session>/agents/<agent>/wire.jsonl`
//! under every Kimi data root [`kimi_usage::discover_kimi_homes`] resolves
//! (Issue #8930).
//!
//! `wire.jsonl` is an append-only event log. Each `usage.record` is one
//! request's own usage, stamped with its `time` (epoch ms), so it is one event
//! as it stands; the file's `llm.request` records resolve its model alias to
//! the provider's model id, the same rule [`kimi_usage`] applies. Only those
//! two record types are decoded, and only their model strings and four
//! counters are kept: the log also holds prompts and tool output.
//! `usageScope: "session"` records are skipped in case they restate a
//! session's turns; every record observed on a live host so far is `"turn"`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::burn::{BurnEvent, BurnSource, Emit, ModelBurn};
use super::tail::TailSet;
use crate::kimi_usage::{self, LLM_REQUEST_TYPE, USAGE_RECORD_TYPE};

/// `provider` label — the API-key pool's directory name for Kimi.
pub const KIMI_PROVIDER: &str = "kimi";

/// One wire log's model-alias resolutions.
#[derive(Debug, Default)]
pub struct WireLog {
    aliases: HashMap<String, String>,
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl WireLog {
    /// Decode one wire-log line, pushing the usage it records (if any).
    pub fn add_line(&mut self, line: &str, emit: Emit, out: &mut Vec<BurnEvent>) {
        let is_usage = line.contains(USAGE_RECORD_TYPE);
        if !is_usage && !line.contains(LLM_REQUEST_TYPE) {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some(LLM_REQUEST_TYPE) => {
                if let (Some(alias), Some(model)) =
                    (string_field(&value, "modelAlias"), string_field(&value, "model"))
                {
                    self.aliases.insert(alias, model);
                }
            }
            Some(USAGE_RECORD_TYPE) => {
                if value.get("usageScope").and_then(serde_json::Value::as_str) == Some("session") {
                    return;
                }
                let (Some(alias), Some(at)) = (
                    string_field(&value, "model"),
                    value
                        .get("time")
                        .and_then(serde_json::Value::as_i64)
                        .and_then(DateTime::<Utc>::from_timestamp_millis),
                ) else {
                    return;
                };
                let usage = value.get("usage");
                let counter = |key: &str| {
                    usage
                        .and_then(|u| u.get(key))
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0)
                        .max(0)
                };
                let burn = ModelBurn {
                    input: counter("inputOther"),
                    output: counter("output"),
                    cache_read: counter("inputCacheRead"),
                    cache_write: counter("inputCacheCreation"),
                    requests: 1,
                };
                let spent =
                    (burn.input, burn.output, burn.cache_read, burn.cache_write) != (0, 0, 0, 0);
                if spent && emit.accepts(at) {
                    out.push(BurnEvent {
                        provider: KIMI_PROVIDER.to_string(),
                        model: self.aliases.get(&alias).cloned().unwrap_or(alias),
                        at,
                        usage: burn,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Every `wire.jsonl` under `root/sessions/`, at its fixed depth.
pub fn wire_logs_under(root: &Path) -> Vec<PathBuf> {
    fn dirs(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            },
        )
    }
    let mut out = Vec::new();
    for workdir in dirs(&root.join("sessions")) {
        for session in dirs(&workdir) {
            for agent in dirs(&session.join("agents")) {
                out.push(agent.join("wire.jsonl"));
            }
        }
    }
    out
}

/// Every Kimi session store on this host, tailed.
#[derive(Debug, Default)]
pub struct KimiSource {
    /// Overrides [`kimi_usage::discover_kimi_homes`] (tests).
    pub roots: Option<Vec<PathBuf>>,
    files: TailSet<WireLog>,
}

impl BurnSource for KimiSource {
    fn poll(&mut self, emit: Emit, out: &mut Vec<BurnEvent>) {
        let roots = self
            .roots
            .clone()
            .unwrap_or_else(|| kimi_usage::discover_kimi_homes(None));
        let paths: Vec<PathBuf> = roots
            .iter()
            .flat_map(|root| wire_logs_under(root))
            .collect();
        self.files.poll(paths, emit.active_since(), |log, line| {
            log.add_line(line, emit, out);
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "kimi_tests.rs"]
mod tests;
