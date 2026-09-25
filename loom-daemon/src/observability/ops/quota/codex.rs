//! Codex burn source: `sessions/<Y>/<M>/<D>/rollout-*.jsonl` under every
//! `$CODEX_HOME` a launch could write to — the ambient home plus each pooled
//! profile under `~/.loom/codex-profiles/` (Issue #8930).
//!
//! Paths come only from [`codex_usage::discover_rollouts`], so the
//! `is_rollout_path` gate that keeps `auth.json` and its siblings unreachable
//! applies here too, and a rollout reachable through a profile's symlinked
//! `sessions/` (#8694) is one file. Each line carries its own `timestamp`.
//! `event_msg`/`token_count` carries the session's **cumulative**
//! `total_token_usage`, written twice per turn, so each file keeps its
//! previous counters and the model named by the latest `turn_context`: one
//! event is the delta over the previous counters (the duplicate adds zero),
//! and one non-empty delta is one request. The token mapping is
//! [`codex_usage`]'s: `input` excludes `cached_input_tokens`, which is
//! `cache_read`, and `reasoning_output_tokens` is already inside `output`.

use std::path::PathBuf;

use super::burn::{BurnEvent, BurnSource, Emit, ModelBurn};
use super::tail::TailSet;
use crate::codex_usage::{self, Counters};

/// `provider` label — the value `tokens.snapshot` uses for the Codex pool.
pub const CODEX_PROVIDER: &str = "codex";

/// One rollout's running state.
#[derive(Debug, Default)]
pub struct Rollout {
    previous: Counters,
    model: Option<String>,
}

impl Rollout {
    /// Decode one rollout line, pushing the usage it adds (if any).
    pub fn add_line(&mut self, line: &str, emit: Emit, out: &mut Vec<BurnEvent>) {
        let is_turn = line.contains("\"turn_context\"");
        if !is_turn && !line.contains("\"token_count\"") {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let Some(payload) = value.get("payload") else {
            return;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("turn_context") => {
                if let Some(model) = codex_usage::string_field(payload, "model") {
                    self.model = Some(model);
                }
            }
            Some("event_msg")
                if payload.get("type").and_then(serde_json::Value::as_str)
                    == Some("token_count") =>
            {
                let Some(current) = payload
                    .get("info")
                    .and_then(|info| info.get("total_token_usage"))
                    .and_then(Counters::from_total_usage)
                else {
                    return;
                };
                let delta = self.previous.delta(current);
                self.previous = current;
                let (Some(model), Some(at)) = (
                    self.model.clone(),
                    codex_usage::string_field(&value, "timestamp")
                        .and_then(|ts| codex_usage::parse_instant(&ts)),
                ) else {
                    return;
                };
                let usage = ModelBurn {
                    input: (delta.input - delta.cached_input).max(0),
                    output: delta.output,
                    cache_read: delta.cached_input,
                    cache_write: 0,
                    requests: 1,
                };
                if emit.accepts(at) && (usage.input, usage.output, usage.cache_read) != (0, 0, 0) {
                    out.push(BurnEvent {
                        provider: CODEX_PROVIDER.to_string(),
                        model,
                        at,
                        usage,
                    });
                }
            }
            _ => {}
        }
    }
}

/// Every Codex rollout store on this host, tailed.
#[derive(Debug, Default)]
pub struct CodexSource {
    /// The home directory stores are resolved under (tests); `None` in
    /// production.
    pub home: Option<PathBuf>,
    files: TailSet<Rollout>,
}

impl BurnSource for CodexSource {
    fn poll(&mut self, emit: Emit, out: &mut Vec<BurnEvent>) {
        // The whole tree, not recent date directories: a resumed session
        // appends to the rollout in its start date's directory. Idle files
        // cost one `stat` each.
        let paths = codex_usage::discover_rollouts(self.home.as_deref(), None);
        self.files
            .poll(paths, emit.active_since(), |rollout, line| {
                rollout.add_line(line, emit, out);
            });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "codex_tests.rs"]
mod tests;
