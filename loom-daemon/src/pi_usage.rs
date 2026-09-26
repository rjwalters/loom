//! Read-only per-model token accounting from Pi's own `--mode json` event
//! stream, as captured in a launch's own log (Issue #8594, the Pi half).
//!
//! # Why the stream, not Pi's session store
//!
//! Pi keeps two records of a run: the `--mode json` event stream on stdout,
//! and a session JSONL under `$PI_CODING_AGENT_SESSION_DIR` (by default
//! `~/.pi/agent/sessions/--<cwd>--/<ts>_<uuid>.jsonl`). This module reads the
//! **stream**, for three reasons that all come from how Loom launches Pi:
//!
//! 1. **It is already captured, per launch.** `worker_spawn::attach_log`
//!    points the harness's stdout at the launch's own `--log` file — a sweep's
//!    `.loom/logs/sweep-issue-<N>.log`, a role tick's `role-<role>.log` — the
//!    same capture #8448's toolless verdict and #8556's tap accounting read.
//!    The file is per issue / per role, so it attributes by construction,
//!    with no directory matching at all.
//! 2. **The session store is private and short-lived.** A guarded (role-tagged)
//!    Pi launch relocates `PI_CODING_AGENT_SESSION_DIR` into its own per-launch
//!    uuid-named native-state directory (`native_tools::provision::state`),
//!    which the #8663 reaper removes at the workspace's next launch once the
//!    harness has exited — so a post-hoc reader could neither find it by path
//!    nor rely on it still existing at a sweep's terminal transition.
//! 3. **It keeps this reader away from Pi's auth store.** That per-launch state
//!    directory, like `~/.pi/agent/`, also holds Pi's `auth.json`. Reading only
//!    the Loom-owned launch log means this module never has to walk a directory
//!    that contains a credential.
//!
//! # Security: one guarded open, of a Loom launch log only
//!
//! [`read_log`] is the only function in this module that touches the
//! filesystem, and it refuses any path [`is_launch_log_path`] rejects: only a
//! `sweep-issue-<N>.log` or `role-<role>.log` directly under a `.loom/logs/`
//! directory. `tests::the_only_file_open_in_this_module_is_the_guarded_log_reader`
//! pins that against this module's own **source text**, not only its behavior,
//! so a future edit that adds a second open, or names Pi's auth store, fails a
//! test that says why. Every open is read-only (`File::open`).
//!
//! What is lifted *out* of the log is equally narrow: a `message_end` event's
//! model, provider, timestamp and `usage` counters, plus the `session`
//! header's `id`/`cwd`. Nothing else in a line (message text, tool arguments
//! or results) is retained.
//!
//! # Schema provenance
//!
//! Verified on 2026-09-25 against the published `@earendil-works/pi-coding-agent`
//! **0.85.1** package, the version `docker/native/README.md` pins (`PI_VERSION`)
//! and `harness.rs` records as tested, and its `@earendil-works/pi-ai` 0.85.1
//! dependency. No host in the fleet had a run's stream on disk to survey —
//! `~/.pi/agent/sessions/` on the operator host held only an empty probe
//! directory — so the schema is taken from the shipped code itself, the same
//! treatment `kimi_usage`'s provenance section documents for Kimi's bundle:
//!
//! - `dist/modes/print-mode.js`: in `--mode json`, the session header
//!   (`{"type":"session","version":3,"id":…,"timestamp":…,"cwd":…}`) is written
//!   first, then every `AgentSessionEvent` through `toJsonEvent`, which passes
//!   every event **except** `message_update` through unchanged.
//! - `docs/json.md`: "`message_end` contains the final authoritative message";
//!   `message_update.usage` is a *cumulative* in-flight snapshot and is not
//!   counted here.
//! - `dist/core/agent-session.d.ts`: `agent_end` repeats every message and
//!   `entry_appended` re-emits each persisted session entry, assistant
//!   messages included. Counting either would double every run, so **only
//!   `message_end`** is read (the same exclusion `crate::tap_usage` documents).
//! - `pi-ai`'s `AssistantMessage`: `role:"assistant"`, `provider`, `model`,
//!   `usage`, `timestamp` (Unix **milliseconds**). The key is `model` — the id
//!   Pi requested, the same one the `# LOOM_LAUNCH` record names — not the
//!   optional `responseModel` a provider may echo back.
//! - `pi-ai`'s `Usage`: `input`, `output`, `cacheRead`, `cacheWrite`, optional
//!   `cacheWrite1h` ("subset of `cacheWrite` … only Anthropic reports this
//!   split") and optional `reasoning` ("a subset of `output`: `output` already
//!   includes these tokens"). `openai-completions.js`' `parseChunkUsage`
//!   confirms `input` is already net of `cacheRead` and `cacheWrite`
//!   (`prompt_tokens - cacheRead - cacheWrite`).
//!
//! # Mapping to [`ModelUsageTotals`]
//!
//! `input`, `output` and `cacheRead` map one-to-one: Pi's normalized `input`
//! already excludes cached tokens, exactly as Claude's does, and `reasoning` is
//! **never** added to `output`, which already contains it. `cacheWrite` is
//! split by `cacheWrite1h` when the provider reported that split; otherwise the
//! flat count goes to the 1-hour bucket, the same fallback
//! [`crate::opencode_usage`] and the Claude reader apply. Pi has no
//! `speed`/`service_tier` axes, so both are the shared `"standard"` default.
//!
//! **Deliberately not counted**, because the counters carry no model id and
//! this reader never guesses one: a `toolResult` message's nested `usage`
//! (LLM work done inside a tool), and the `usage` on a `compaction` /
//! `branch_summary` entry (the summarization call). Both are rare in a headless
//! `--print` run; each is an undercount, never a misattribution.
//!
//! # Exact session attribution
//!
//! #8507's deferred design note asked for attribution by the launch's exact
//! session id rather than by directory + wall-clock window. The stream carries
//! it: every usage row records the `session` header it followed
//! ([`PiMessageUsage::session_id`]). Selection is still by the caller's window
//! over each message's **own** timestamp — not a session's start time — inside
//! a log that already belongs to one issue or one role, which is what
//! separates this dispatch from an earlier one appended to the same file.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api_keys_pool::ingest::LAUNCH_RECORD_PREFIX;
use crate::script_helpers::sweep_experiment::ModelUsageTotals;

/// The `speed`/`service_tier` bucket every Pi row is grouped under — see the
/// module doc's "Mapping" section.
const DEFAULT_BUCKET: &str = "standard";

/// The one event type whose `message` is counted — see "Schema provenance".
const USAGE_EVENT: &str = "message_end";

/// The stream's header line type (`{"type":"session","version":3,"id":…}`).
const SESSION_HEADER: &str = "session";

/// Whether `path` is a Loom launch log this reader may open: a
/// `sweep-issue-<digits>.log` or `role-<name>.log` whose parent directory is
/// `logs` inside a `.loom` directory. Everything else — Pi's own state tree
/// included — is refused before any open.
#[must_use]
pub fn is_launch_log_path(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let in_loom_logs = parent.file_name().is_some_and(|n| n == "logs")
        && parent
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|n| n == ".loom");
    if !in_loom_logs {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".log") else {
        return false;
    };
    if let Some(issue) = stem.strip_prefix("sweep-issue-") {
        return !issue.is_empty() && issue.bytes().all(|b| b.is_ascii_digit());
    }
    if let Some(role) = stem.strip_prefix("role-") {
        return !role.is_empty()
            && role
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    }
    false
}

/// Read a launch log, or `None` when `path` is not one ([`is_launch_log_path`])
/// or cannot be read. The single file open in this module.
fn read_log(path: &Path) -> Option<String> {
    if !is_launch_log_path(path) {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// One assistant `message_end` event's usage, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiMessageUsage {
    /// `message.model`, verbatim (trimmed).
    pub model: String,
    /// `message.provider`, when present and non-empty.
    pub provider: Option<String>,
    /// The `id` of the most recent `session` header before this event in the
    /// same launch, when the stream carried one.
    pub session_id: Option<String>,
    /// That header's `cwd`.
    pub cwd: Option<String>,
    /// `message.timestamp`, decoded from epoch milliseconds.
    pub at: DateTime<Utc>,
    pub input: i64,
    pub output: i64,
    /// Reported for display only: a subset of `output`, never added to it.
    pub reasoning: Option<i64>,
    pub cache_read: i64,
    pub cache_write: i64,
    /// The subset of `cache_write` written with 1-hour retention, when the
    /// provider reported the split.
    pub cache_write_1h: Option<i64>,
}

impl PiMessageUsage {
    /// Whether this message reported any token usage at all. An aborted or
    /// errored request can end with an all-zero `usage`; folding it in would
    /// publish a fabricated zero-token model badge.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0
    }
}

/// A non-negative integer counter, or `None` when absent or not a number.
fn counter(usage: &Value, field: &str) -> Option<i64> {
    let value = usage.get(field)?;
    value
        .as_i64()
        .or_else(|| value.as_f64().filter(|v| v.is_finite()).map(|v| v as i64))
        .map(|v| v.max(0))
}

fn non_empty_str(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Decode one assistant message off a `message_end` event. `None` for any
/// other role, a missing/empty model (never guessed), a missing timestamp, or
/// a `usage` that reports neither `input` nor `output` (unmeasured, not zero).
fn decode_message(
    message: &Value,
    session_id: Option<&String>,
    cwd: Option<&String>,
) -> Option<PiMessageUsage> {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let model = non_empty_str(message, "model")?;
    let usage = message.get("usage").filter(|u| u.is_object())?;
    let input = counter(usage, "input");
    let output = counter(usage, "output");
    if input.is_none() && output.is_none() {
        return None;
    }
    let at = DateTime::<Utc>::from_timestamp_millis(message.get("timestamp")?.as_i64()?)?;
    let cache_write = counter(usage, "cacheWrite").unwrap_or(0);
    Some(PiMessageUsage {
        model,
        provider: non_empty_str(message, "provider"),
        session_id: session_id.cloned(),
        cwd: cwd.cloned(),
        at,
        input: input.unwrap_or(0),
        output: output.unwrap_or(0),
        reasoning: counter(usage, "reasoning"),
        cache_read: counter(usage, "cacheRead").unwrap_or(0),
        cache_write,
        cache_write_1h: counter(usage, "cacheWrite1h").map(|v| v.min(cache_write)),
    })
}

/// Every assistant `message_end` usage in a captured stream whose own
/// timestamp falls inside `window` (inclusive; `None` = all), in stream order.
///
/// Line-oriented and lenient in the way every reader of these logs is: any line
/// that is not a JSON object — the dispatch header, `# LOOM_LAUNCH`,
/// `spawn-worker:` prose, another harness's chatter — is skipped. A
/// `# LOOM_LAUNCH` line starts a new launch, so the session id carried from an
/// earlier launch's header is dropped there rather than leaking forward.
#[must_use]
pub fn messages_in_stream(
    text: &str,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Vec<PiMessageUsage> {
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(LAUNCH_RECORD_PREFIX) {
            session_id = None;
            cwd = None;
            continue;
        }
        if !line.starts_with('{') {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some(SESSION_HEADER) => {
                session_id = non_empty_str(&event, "id");
                cwd = non_empty_str(&event, "cwd");
            }
            Some(USAGE_EVENT) => {
                let Some(usage) = event
                    .get("message")
                    .and_then(|m| decode_message(m, session_id.as_ref(), cwd.as_ref()))
                else {
                    continue;
                };
                if let Some((start, end)) = window {
                    if usage.at < start || usage.at > end {
                        continue;
                    }
                }
                found.push(usage);
            }
            _ => {}
        }
    }
    found
}

/// [`messages_in_stream`] over one launch log. `None` — never `Some(vec![])` —
/// when the path is refused or unreadable; `Some(vec![])` when it was read and
/// held nothing attributable.
#[must_use]
pub fn messages_in_log(
    log_path: &Path,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<Vec<PiMessageUsage>> {
    read_log(log_path).map(|text| messages_in_stream(&text, window))
}

/// Per-`(model, speed, service_tier)` token totals for one launch log — the Pi
/// counterpart of [`crate::opencode_usage::tokens_by_model`], reached through
/// [`crate::usage_source`].
///
/// `None` — never `Some(vec![])` — when the log is unreadable or held no
/// attributable, non-empty usage: "unknown != zero".
#[must_use]
pub fn tokens_by_model(
    log_path: &Path,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Option<Vec<ModelUsageTotals>> {
    fold_messages(messages_in_log(log_path, window)?)
}

/// Fold decoded messages into per-model totals — the whole of this module's
/// arithmetic (see the module doc's "Mapping" section).
#[must_use]
pub fn fold_messages(
    messages: impl IntoIterator<Item = PiMessageUsage>,
) -> Option<Vec<ModelUsageTotals>> {
    let mut totals: BTreeMap<String, ModelUsageTotals> = BTreeMap::new();
    for message in messages.into_iter().filter(|m| !m.is_empty()) {
        let entry = totals
            .entry(message.model.clone())
            .or_insert_with(|| ModelUsageTotals {
                model: message.model.clone(),
                speed: DEFAULT_BUCKET.to_string(),
                service_tier: DEFAULT_BUCKET.to_string(),
                ..ModelUsageTotals::default()
            });
        let (write_5m, write_1h) = match message.cache_write_1h {
            Some(one_hour) => (message.cache_write - one_hour, one_hour),
            None => (0, message.cache_write),
        };
        entry.input = entry.input.saturating_add(message.input);
        entry.output = entry.output.saturating_add(message.output);
        entry.cache_read = entry.cache_read.saturating_add(message.cache_read);
        entry.cache_write_5m = entry.cache_write_5m.saturating_add(write_5m);
        entry.cache_write_1h = entry.cache_write_1h.saturating_add(write_1h);
    }
    (!totals.is_empty()).then(|| totals.into_values().collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
