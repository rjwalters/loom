//! Read one Claude Code transcript into the deduped, per-`(model, day)` token
//! totals `activity.db` ingestion stores (issue #8059).
//!
//! This is the *reading* half of the ingestion path; [`super::transcript_ingest`]
//! is the *writing* half. Per-record decoding is not re-implemented here — it
//! is [`crate::script_helpers::transcript_usage::usage_from_record`], the same
//! decoder the safehouse completion feed folds — so the two paths can never
//! disagree about what a record cost.
//!
//! # Why deduping on `message.id` is mandatory
//!
//! A streamed assistant message is written to the transcript once per chunk,
//! and every chunk repeats the same `message.id` carrying the **cumulative**
//! usage for that message — not a delta. Summing every `usage` block therefore
//! multiplies a streamed message's cost by its chunk count. Measured on this
//! host's own transcripts (2026-09-18, five most recent sweep sessions): 47-87
//! usage blocks per session collapsing to 18-40 distinct message ids, i.e. a
//! ~2.3x over-count, with every repeat of an id carrying byte-identical
//! counters. Folding by id and taking the per-counter maximum is correct for
//! both shapes (identical repeats and genuinely growing cumulative chunks).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::script_helpers::transcript_usage::usage_from_record;

/// Role names a transcript's first user message may name, in the convention
/// every Loom prompt already follows (#8052's method note (a)). Ordered as the
/// parent issue lists them; attribution picks the *earliest textual* match, so
/// this order is not a priority ranking.
pub const ROLE_KEYWORDS: [&str; 10] = [
    "builder",
    "judge",
    "champion",
    "curator",
    "guide",
    "architect",
    "hermit",
    "auditor",
    "doctor",
    "sweep",
];

/// One `resource_usage` row's worth of deduped totals: a single model's
/// consumption on a single UTC day within one transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageBucket {
    pub model: String,
    /// UTC day, `YYYY-MM-DD` — the grouping key alongside `model`.
    pub day: String,
    /// Earliest message timestamp in this bucket; the row's `timestamp`.
    pub timestamp: DateTime<Utc>,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    /// Distinct `message.id`s folded into this bucket.
    pub messages: usize,
}

/// Everything ingestion needs from one transcript file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedTranscript {
    pub session_id: Option<String>,
    pub role: Option<String>,
    pub cwd: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub issue: Option<i32>,
    /// Sorted by `(model, day)` for deterministic row order.
    pub buckets: Vec<UsageBucket>,
    /// Usage blocks seen (before dedupe), excluding `<synthetic>` ones.
    pub usage_records: usize,
    /// Usage blocks collapsed into an already-seen `message.id`.
    pub duplicate_records: usize,
    /// Usage blocks skipped because `model == "<synthetic>"`.
    pub synthetic_skipped: usize,
    // -----------------------------------------------------------------
    // Session shape (Issue #8757, `session.summary`). Counts and spans
    // only — the parse never copies message text, tool arguments, or tool
    // output into these, so the record built from them is a summary, not
    // a transcript excerpt.
    // -----------------------------------------------------------------
    /// Real user turns: user records whose content is not a tool result.
    pub turns: u64,
    /// Assistant `tool_use` blocks by tool name, deduped on `message.id`
    /// (a streamed message's repeated chunks restate the same blocks, so
    /// the first occurrence counts and repeats do not).
    pub tool_calls: BTreeMap<String, u64>,
    /// Tool results flagged `is_error`.
    pub tool_errors: u64,
    /// Earliest record-level `timestamp` seen on any line of the file.
    pub first_timestamp: Option<DateTime<Utc>>,
    /// Latest record-level `timestamp` seen on any line of the file.
    pub last_timestamp: Option<DateTime<Utc>>,
    // -----------------------------------------------------------------
    // Session analysis (Issue #8760, G3 part 2 of #8714). Order and
    // paired-timing only — same discipline as the session-shape fields
    // above: no tool arguments or output, ever.
    // -----------------------------------------------------------------
    /// Ordered tool names, one entry per counted `tool_use` block — the same
    /// dedup discipline as `tool_calls` (a streamed message's repeated
    /// chunks count once), kept in call order so a downstream pass can
    /// detect a run of identical back-to-back invocations (a candidate
    /// retry loop) without needing raw tool arguments.
    pub tool_call_order: Vec<String>,
    /// The tool_use -> tool_result pairing with the largest elapsed wall
    /// time between the two records' own timestamps, matched by the content
    /// block's own `id`/`tool_use_id` (never by content). `None` when no
    /// pair could be matched — a transcript whose `tool_use` blocks carry no
    /// `id` (every live Claude Code transcript does; only a hand-built
    /// fixture omits it), or one truncated before any `tool_result` arrived.
    pub longest_tool_call: Option<ToolCallSpan>,
}

/// One completed `tool_use` -> `tool_result` pairing (Issue #8760): the tool
/// name and elapsed wall time between the two records' own timestamps — the
/// unit [`ParsedTranscript::longest_tool_call`] tracks the maximum of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallSpan {
    pub tool: String,
    pub duration_ms: i64,
}

impl ParsedTranscript {
    /// Whether this transcript contributes any `resource_usage` row.
    #[must_use]
    pub fn has_usage(&self) -> bool {
        !self.buckets.is_empty()
    }
}

/// Per-message accumulator, keyed by `message.id`.
#[derive(Debug, Clone)]
struct MessageUsage {
    model: String,
    timestamp: DateTime<Utc>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
}

/// Attribute a transcript to a Loom role from its first user message.
///
/// Two passes, in order:
///
/// 1. A `<command-name>/loom:NAME</command-name>` slash-command marker — how a
///    `/loom:sweep` parent session and every role-runner session start.
/// 2. Otherwise the **earliest** whole-word occurrence of a [`ROLE_KEYWORDS`]
///    entry, which is how a subagent's dispatch prompt names its role ("Load
///    and follow the instructions in `.claude/commands/loom/doctor.md`…",
///    "You are the Loom Builder…").
///
/// `None` when neither matches — the row then aggregates under `unknown` in
/// `cost_by_role`, which is honest rather than a guess.
#[must_use]
pub fn attribute_role(first_user_text: &str) -> Option<String> {
    const NAME_OPEN: &str = "<command-name>/loom:";
    if let Some(at) = first_user_text.find(NAME_OPEN) {
        let rest = &first_user_text[at + NAME_OPEN.len()..];
        let name = rest
            .split('<')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if ROLE_KEYWORDS.contains(&name.as_str()) {
            return Some(name);
        }
    }

    // `to_ascii_lowercase` is byte-length preserving, so indexes into `lower`
    // are valid indexes into the original text too.
    let lower = first_user_text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut best: Option<(usize, &str)> = None;
    for keyword in ROLE_KEYWORDS {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(keyword) {
            let at = from + rel;
            let end = at + keyword.len();
            let boundary_before = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
            let boundary_after = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if boundary_before && boundary_after {
                if best.is_none_or(|(best_at, _)| at < best_at) {
                    best = Some((at, keyword));
                }
                break;
            }
            from = end;
        }
    }
    best.map(|(_, keyword)| keyword.to_string())
}

/// The issue a `/loom:<role> <issue>` session names, if any.
///
/// Only the slash command's own first argument counts. A number mentioned in
/// prose ("PR #7759 closes #7726") is deliberately NOT read: a wrong issue
/// attribution is worse than an absent one for cost reporting.
#[must_use]
pub fn attribute_issue(first_user_text: &str) -> Option<i32> {
    const ARGS_OPEN: &str = "<command-args>";
    if !first_user_text.contains("<command-name>/loom:") {
        return None;
    }
    let at = first_user_text.find(ARGS_OPEN)?;
    let args = &first_user_text[at + ARGS_OPEN.len()..];
    let args = args.split("</command-args>").next().unwrap_or(args);
    args.split_whitespace().next()?.parse::<i32>().ok()
}

/// Repository name for a transcript's `cwd`.
///
/// A Loom agent's cwd is either the workspace root (`…/GitHub/loom`) or a
/// managed worktree inside it (`…/GitHub/loom/.loom/worktrees/issue-42`); both
/// belong to the same repo, so everything from the first `.loom` component on
/// is dropped before taking the final component.
#[must_use]
pub fn repo_from_cwd(cwd: &str) -> Option<String> {
    let mut last: Option<&str> = None;
    for component in cwd.split('/') {
        if component == ".loom" {
            break;
        }
        if !component.is_empty() {
            last = Some(component);
        }
    }
    last.map(ToString::to_string)
}

/// Plain text of a record's `message.content`, which is either a string or an
/// array of content blocks.
fn message_text(obj: &Value) -> Option<String> {
    let content = obj.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    let mut out = String::new();
    for block in blocks {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn first_non_empty_str(target: &mut Option<String>, obj: &Value, key: &str) {
    if target.is_none() {
        if let Some(value) = obj
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            *target = Some(value.to_string());
        }
    }
}

/// Fold one record's session shape (Issue #8757, extended by #8760) into
/// `parsed`: user turns, the tool-call histogram and call order, tool
/// errors, and the `tool_use` -> `tool_result` timing pair.
///
/// Only structural fields are read — block `type`s, `tool_use` block `name`s
/// and `id`s, and `tool_result` `is_error`/`tool_use_id` fields. Message
/// text, tool arguments and tool output are never copied anywhere, which is
/// what makes the downstream `session.summary` / `session.analysis` records
/// summaries rather than transcript excerpts.
///
/// Assistant records are scanned on the **first** occurrence of their
/// `message.id` only (a streamed message restates its blocks per chunk);
/// records with no id are scanned once each under the same `__line_N` key
/// convention the usage fold uses. User tool-result records are never
/// chunk-restated, so `tool_errors` counts every flagged block directly.
///
/// `timestamp` is this record's own already-parsed timestamp (`None` when
/// absent/unparseable) — threaded in rather than re-read here so this
/// function stays a pure fold over already-extracted values, matching the
/// rest of the parse loop.
fn scan_session_shape(
    parsed: &mut ParsedTranscript,
    obj: &Value,
    index: usize,
    timestamp: Option<DateTime<Utc>>,
    scanned_content_ids: &mut HashMap<String, ()>,
    pending_tool_uses: &mut HashMap<String, (String, DateTime<Utc>)>,
) {
    let content = match obj.get("message").and_then(|m| m.get("content")) {
        Some(content) => content,
        None => return,
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("user") => {
            let blocks = match content.as_array() {
                Some(blocks) => blocks,
                // A plain-text user message is a real turn; there is no
                // tool-result block to inspect.
                None => {
                    parsed.turns += 1;
                    return;
                }
            };
            let mut is_tool_result = false;
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                is_tool_result = true;
                if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    parsed.tool_errors += 1;
                }
                // Pair this result with its `tool_use` by the content
                // block's own opaque call id (never by message text) to
                // measure elapsed wall time (Issue #8760).
                if let (Some(call_id), Some(end_ts)) =
                    (block.get("tool_use_id").and_then(Value::as_str), timestamp)
                {
                    if let Some((tool, start_ts)) = pending_tool_uses.remove(call_id) {
                        let duration_ms = (end_ts - start_ts).num_milliseconds().max(0);
                        let is_longer = parsed
                            .longest_tool_call
                            .as_ref()
                            .is_none_or(|current| duration_ms > current.duration_ms);
                        if is_longer {
                            parsed.longest_tool_call = Some(ToolCallSpan { tool, duration_ms });
                        }
                    }
                }
            }
            // A user record carrying only tool results is the runtime's
            // turn-boundary machinery, not a human turn.
            if !is_tool_result {
                parsed.turns += 1;
            }
        }
        Some("assistant") => {
            let message = obj.get("message");
            let key = message
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("__line_{index}"));
            if scanned_content_ids.insert(key, ()).is_some() {
                return;
            }
            let Some(blocks) = content.as_array() else {
                return;
            };
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                if let Some(name) = block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    *parsed.tool_calls.entry(name.to_string()).or_insert(0) += 1;
                    parsed.tool_call_order.push(name.to_string());
                    if let (Some(call_id), Some(start_ts)) =
                        (block.get("id").and_then(Value::as_str), timestamp)
                    {
                        pending_tool_uses.insert(call_id.to_string(), (name.to_string(), start_ts));
                    }
                }
            }
        }
        _ => {}
    }
}

/// Read `path` into deduped per-`(model, day)` totals plus its attribution
/// metadata.
///
/// Best-effort in the same way the rest of the transcript tooling is: an
/// unreadable file or malformed line yields fewer totals, never an error.
/// `fallback_timestamp` (normally the file's mtime) dates any usage record
/// that carries no parseable `timestamp` of its own.
#[must_use]
pub fn parse_transcript(path: &Path, fallback_timestamp: DateTime<Utc>) -> ParsedTranscript {
    let mut parsed = ParsedTranscript::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return parsed;
    };

    let mut by_message: HashMap<String, MessageUsage> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut first_user_text: Option<String> = None;
    // `message.id`s whose content blocks were already scanned (Issue #8757):
    // a streamed assistant message repeats its id per chunk, so tool_use
    // blocks are counted on the first occurrence only — the same dedupe
    // discipline `by_message` applies to usage.
    let mut scanned_content_ids: HashMap<String, ()> = HashMap::new();
    // `tool_use` call ids awaiting their `tool_result` pairing (Issue
    // #8760), keyed by the content block's own `id` — never by message
    // text or content.
    let mut pending_tool_uses: HashMap<String, (String, DateTime<Utc>)> = HashMap::new();

    for (index, raw) in text.lines().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(raw) else {
            continue;
        };

        first_non_empty_str(&mut parsed.session_id, &obj, "sessionId");
        first_non_empty_str(&mut parsed.cwd, &obj, "cwd");
        first_non_empty_str(&mut parsed.branch, &obj, "gitBranch");

        let record_timestamp: Option<DateTime<Utc>> = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| dt.with_timezone(&Utc));
        if let Some(at) = record_timestamp {
            parsed.first_timestamp = Some(
                parsed
                    .first_timestamp
                    .map_or(at, |first: DateTime<Utc>| first.min(at)),
            );
            parsed.last_timestamp = Some(
                parsed
                    .last_timestamp
                    .map_or(at, |last: DateTime<Utc>| last.max(at)),
            );
        }

        if first_user_text.is_none() && obj.get("type").and_then(Value::as_str) == Some("user") {
            first_user_text = message_text(&obj);
        }

        scan_session_shape(
            &mut parsed,
            &obj,
            index,
            record_timestamp,
            &mut scanned_content_ids,
            &mut pending_tool_uses,
        );

        let Some(record) = usage_from_record(&obj) else {
            continue;
        };
        if record.synthetic {
            parsed.synthetic_skipped += 1;
            continue;
        }
        parsed.usage_records += 1;

        let timestamp = record
            .timestamp
            .as_deref()
            .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
            .map_or(fallback_timestamp, |dt| dt.with_timezone(&Utc));
        // A record with no id cannot collide with another message, so it gets
        // a key that is unique to its line.
        let key = record
            .message_id
            .clone()
            .unwrap_or_else(|| format!("__line_{index}"));
        let cache_write = record.cache_write_5m.saturating_add(record.cache_write_1h);

        match by_message.get_mut(&key) {
            // Repeat of a streamed message: every chunk restates the message's
            // cumulative usage, so take the per-counter maximum rather than
            // adding (see the module doc).
            Some(existing) => {
                parsed.duplicate_records += 1;
                existing.input = existing.input.max(record.input);
                existing.output = existing.output.max(record.output);
                existing.cache_read = existing.cache_read.max(record.cache_read);
                existing.cache_write = existing.cache_write.max(cache_write);
            }
            None => {
                order.push(key.clone());
                by_message.insert(
                    key,
                    MessageUsage {
                        model: record.model,
                        timestamp,
                        input: record.input,
                        output: record.output,
                        cache_read: record.cache_read,
                        cache_write,
                    },
                );
            }
        }
    }

    if let Some(text) = first_user_text.as_deref() {
        parsed.role = attribute_role(text);
        parsed.issue = attribute_issue(text);
    }
    if let Some(cwd) = parsed.cwd.as_deref() {
        parsed.repo = repo_from_cwd(cwd);
    }

    let mut buckets: HashMap<(String, String), UsageBucket> = HashMap::new();
    for key in order {
        let Some(message) = by_message.get(&key) else {
            continue;
        };
        let day = message.timestamp.format("%Y-%m-%d").to_string();
        let bucket = buckets
            .entry((message.model.clone(), day.clone()))
            .or_insert_with(|| UsageBucket {
                model: message.model.clone(),
                day,
                timestamp: message.timestamp,
                tokens_input: 0,
                tokens_output: 0,
                tokens_cache_read: 0,
                tokens_cache_write: 0,
                messages: 0,
            });
        bucket.timestamp = bucket.timestamp.min(message.timestamp);
        bucket.tokens_input = bucket.tokens_input.saturating_add(message.input);
        bucket.tokens_output = bucket.tokens_output.saturating_add(message.output);
        bucket.tokens_cache_read = bucket.tokens_cache_read.saturating_add(message.cache_read);
        bucket.tokens_cache_write = bucket
            .tokens_cache_write
            .saturating_add(message.cache_write);
        bucket.messages += 1;
    }

    let mut buckets: Vec<UsageBucket> = buckets.into_values().collect();
    buckets.sort_by(|a, b| (&a.model, &a.day).cmp(&(&b.model, &b.day)));
    // A message whose four counters are all zero contributes no cost and no
    // signal; a bucket of only such messages is dropped so "no usage" stays
    // distinguishable from "zero usage" downstream.
    buckets.retain(|b| {
        b.tokens_input != 0
            || b.tokens_output != 0
            || b.tokens_cache_read != 0
            || b.tokens_cache_write != 0
    });
    parsed.buckets = buckets;
    parsed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use std::io::Write as _;

    fn fallback() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, 12, 0, 0).unwrap()
    }

    fn write_transcript(lines: &[String]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file.flush().unwrap();
        file
    }

    fn user_line(text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "sessionId": "session-1",
            "cwd": "/home/ubuntu/GitHub/loom",
            "gitBranch": "main",
            "message": {"role": "user", "content": text},
        })
        .to_string()
    }

    /// One assistant record, in the shape Claude Code actually writes.
    fn assistant_line(
        id: &str,
        model: &str,
        ts: &str,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "sessionId": "session-1",
            "cwd": "/home/ubuntu/GitHub/loom",
            "message": {
                "model": model,
                "id": id,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": cache_read,
                    "cache_creation_input_tokens": cache_write,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 0,
                        "ephemeral_1h_input_tokens": cache_write,
                    },
                },
            },
        })
        .to_string()
    }

    // --- dedupe on message.id --------------------------------------------

    #[test]
    fn repeated_streamed_chunks_of_one_message_are_counted_once() {
        // Measured shape: every chunk of a streamed message repeats the id
        // with the same cumulative usage. Summing would triple it.
        let chunk = assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T03:59:57Z",
            2,
            182,
            29616,
            19868,
        );
        let file = write_transcript(&[chunk.clone(), chunk.clone(), chunk]);

        let parsed = parse_transcript(file.path(), fallback());

        assert_eq!(parsed.usage_records, 3, "all three blocks are seen");
        assert_eq!(parsed.duplicate_records, 2, "two of them collapse");
        assert_eq!(parsed.buckets.len(), 1);
        let bucket = &parsed.buckets[0];
        assert_eq!(bucket.messages, 1);
        assert_eq!(bucket.tokens_input, 2);
        assert_eq!(bucket.tokens_output, 182);
        assert_eq!(bucket.tokens_cache_read, 29616);
        assert_eq!(bucket.tokens_cache_write, 19868);
    }

    #[test]
    fn a_growing_cumulative_stream_keeps_the_largest_counters() {
        // Partial chunks: usage grows toward the final cumulative value.
        let file = write_transcript(&[
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 2, 40, 100, 0),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:01Z", 2, 120, 100, 0),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:02Z", 2, 182, 100, 0),
        ]);

        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.buckets.len(), 1);
        assert_eq!(parsed.buckets[0].tokens_output, 182, "final cumulative value, not the sum");
        assert_eq!(parsed.buckets[0].tokens_input, 2);
    }

    #[test]
    fn distinct_messages_are_summed_not_collapsed() {
        let file = write_transcript(&[
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 1, 10, 100, 5),
            assistant_line("msg_2", "claude-sonnet-5", "2026-09-18T04:05:00Z", 2, 20, 200, 6),
        ]);

        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.duplicate_records, 0);
        assert_eq!(parsed.buckets.len(), 1);
        assert_eq!(parsed.buckets[0].messages, 2);
        assert_eq!(parsed.buckets[0].tokens_input, 3);
        assert_eq!(parsed.buckets[0].tokens_output, 30);
        assert_eq!(parsed.buckets[0].tokens_cache_read, 300);
        assert_eq!(parsed.buckets[0].tokens_cache_write, 11);
    }

    #[test]
    fn a_usage_record_without_an_id_is_never_folded_into_another() {
        let no_id = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-09-18T04:00:00Z",
            "message": {"model": "claude-sonnet-5", "usage": {"input_tokens": 5, "output_tokens": 5}},
        })
        .to_string();
        let file = write_transcript(&[no_id.clone(), no_id]);

        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.duplicate_records, 0);
        assert_eq!(parsed.buckets[0].messages, 2);
        assert_eq!(parsed.buckets[0].tokens_input, 10);
    }

    // --- <synthetic> ------------------------------------------------------

    #[test]
    fn synthetic_model_records_are_skipped_entirely() {
        let file = write_transcript(&[
            assistant_line("msg_1", "<synthetic>", "2026-09-18T04:00:00Z", 999, 999, 999, 999),
            assistant_line("msg_2", "claude-sonnet-5", "2026-09-18T04:00:01Z", 1, 2, 3, 4),
        ]);

        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.synthetic_skipped, 1);
        assert_eq!(parsed.usage_records, 1);
        assert_eq!(parsed.buckets.len(), 1);
        assert_eq!(parsed.buckets[0].model, "claude-sonnet-5");
        assert_eq!(parsed.buckets[0].tokens_input, 1);
    }

    // --- grouping ---------------------------------------------------------

    #[test]
    fn a_multi_model_session_yields_one_bucket_per_model() {
        let file = write_transcript(&[
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 1, 2, 3, 4),
            assistant_line("msg_2", "claude-opus-5", "2026-09-18T04:01:00Z", 10, 20, 30, 40),
            assistant_line("msg_3", "claude-sonnet-5", "2026-09-18T04:02:00Z", 5, 5, 5, 5),
        ]);

        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.buckets.len(), 2);
        // Sorted by (model, day): opus before sonnet.
        assert_eq!(parsed.buckets[0].model, "claude-opus-5");
        assert_eq!(parsed.buckets[0].tokens_input, 10);
        assert_eq!(parsed.buckets[1].model, "claude-sonnet-5");
        assert_eq!(parsed.buckets[1].tokens_input, 6);
    }

    #[test]
    fn a_session_spanning_midnight_splits_into_one_bucket_per_utc_day() {
        let file = write_transcript(&[
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-17T23:59:00Z", 1, 1, 1, 1),
            assistant_line("msg_2", "claude-sonnet-5", "2026-09-18T00:01:00Z", 2, 2, 2, 2),
        ]);

        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.buckets.len(), 2, "cost_by_day must not attribute both to one date");
        assert_eq!(parsed.buckets[0].day, "2026-09-17");
        assert_eq!(parsed.buckets[1].day, "2026-09-18");
    }

    #[test]
    fn a_record_without_a_timestamp_falls_back_to_the_file_mtime() {
        let no_ts = serde_json::json!({
            "type": "assistant",
            "message": {"model": "claude-sonnet-5", "id": "m", "usage": {"input_tokens": 3}},
        })
        .to_string();
        let file = write_transcript(&[no_ts]);

        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.buckets[0].day, "2026-09-18");
        assert_eq!(parsed.buckets[0].timestamp, fallback());
    }

    // --- empty / degenerate sessions --------------------------------------

    #[test]
    fn a_session_with_no_assistant_messages_yields_no_buckets() {
        let file = write_transcript(&[user_line(
            "<command-name>/loom:builder</command-name>\n<command-args>42</command-args>",
        )]);

        let parsed = parse_transcript(file.path(), fallback());
        assert!(!parsed.has_usage());
        assert_eq!(parsed.usage_records, 0);
        // Attribution still works — it just has nothing to attribute.
        assert_eq!(parsed.role.as_deref(), Some("builder"));
    }

    #[test]
    fn an_all_zero_usage_session_yields_no_buckets() {
        let file = write_transcript(&[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            0,
            0,
            0,
            0,
        )]);

        let parsed = parse_transcript(file.path(), fallback());
        assert!(!parsed.has_usage(), "zero tokens is not a row worth writing");
    }

    #[test]
    fn malformed_lines_and_a_missing_file_are_survivable() {
        let file = write_transcript(&[
            "not json at all".to_string(),
            "{\"truncated\": ".to_string(),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 1, 1, 1, 1),
        ]);
        assert_eq!(parse_transcript(file.path(), fallback()).buckets.len(), 1);

        let absent = parse_transcript(Path::new("/nope/nowhere.jsonl"), fallback());
        assert_eq!(absent, ParsedTranscript::default());
    }

    // --- attribution ------------------------------------------------------

    #[test]
    fn a_slash_command_names_the_role_exactly() {
        assert_eq!(
            attribute_role(
                "<command-name>/loom:sweep</command-name>\n<command-args>8059</command-args>"
            )
            .as_deref(),
            Some("sweep")
        );
        assert_eq!(
            attribute_role("<command-name>/loom:judge</command-name>").as_deref(),
            Some("judge")
        );
        // A non-role loom command is not forced into the role set.
        assert_eq!(attribute_role("<command-name>/loom:watch</command-name>"), None);
    }

    #[test]
    fn a_subagent_prompt_is_attributed_by_its_earliest_role_mention() {
        // The real shape of a dispatched subagent's first user message.
        assert_eq!(
            attribute_role(
                "Load and follow the instructions in `.claude/commands/loom/doctor.md` in full, \
                 then address PR #7759. Judge rejected it."
            )
            .as_deref(),
            Some("doctor"),
            "the role it was dispatched as wins over a later mention of another role"
        );
        assert_eq!(
            attribute_role("You are the Loom Builder (Development Worker) for this repository.")
                .as_deref(),
            Some("builder")
        );
    }

    #[test]
    fn role_matching_respects_word_boundaries_and_can_decline() {
        assert_eq!(attribute_role("rebuilders and judgemental prose"), None);
        assert_eq!(attribute_role("please summarize this file"), None);
        assert_eq!(attribute_role("").as_deref(), None);
        // A hyphen is a boundary, an alphanumeric is not.
        assert_eq!(attribute_role("run the judge-phase now").as_deref(), Some("judge"));
    }

    #[test]
    fn the_first_user_message_is_what_attributes_the_transcript() {
        let file = write_transcript(&[
            user_line("<command-name>/loom:sweep</command-name>\n<command-args>8059 --claim-owned 8059</command-args>"),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 1, 1, 1, 1),
            user_line("now act as the champion"),
        ]);

        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.role.as_deref(), Some("sweep"));
        assert_eq!(parsed.issue, Some(8059));
        assert_eq!(parsed.session_id.as_deref(), Some("session-1"));
        assert_eq!(parsed.repo.as_deref(), Some("loom"));
        assert_eq!(parsed.branch.as_deref(), Some("main"));
    }

    #[test]
    fn an_issue_number_is_only_read_from_the_slash_commands_own_argument() {
        assert_eq!(
            attribute_issue("<command-name>/loom:sweep</command-name>\n<command-args>8059 --prs 999</command-args>"),
            Some(8059)
        );
        // Prose numbers are deliberately ignored: a wrong attribution is
        // worse than none.
        assert_eq!(attribute_issue("address PR #7759 which closes #7726"), None);
        assert_eq!(
            attribute_issue(
                "<command-name>/loom:judge</command-name>\n<command-args>--all</command-args>"
            ),
            None
        );
    }

    #[test]
    fn repo_is_the_workspace_root_even_from_inside_a_worktree() {
        assert_eq!(repo_from_cwd("/home/ubuntu/GitHub/loom").as_deref(), Some("loom"));
        assert_eq!(
            repo_from_cwd("/home/ubuntu/GitHub/loom/.loom/worktrees/issue-8059").as_deref(),
            Some("loom")
        );
        assert_eq!(
            repo_from_cwd("/home/ubuntu/GitHub/lean-genius/").as_deref(),
            Some("lean-genius")
        );
        assert_eq!(repo_from_cwd(""), None);
    }

    // ------------------------------------------------------------------
    // Session shape (Issue #8757) — turns, tool histogram, tool errors,
    // timestamp span. Counts only; never message text or tool output.
    // ------------------------------------------------------------------

    fn shape_user_line(kind: &str, is_error: bool, ts: &str) -> String {
        let content = if kind == "tool_result" {
            serde_json::json!([{"type": "tool_result", "tool_use_id": "t1", "is_error": is_error}])
        } else {
            serde_json::json!("a human turn")
        };
        serde_json::json!({
            "type": "user", "timestamp": ts, "sessionId": "s1",
            "message": {"role": "user", "content": content},
        })
        .to_string()
    }

    fn tool_use_line(id: &str, tool: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "assistant", "timestamp": ts, "sessionId": "s1",
            "message": {"id": id, "model": "m", "content": [
                {"type": "tool_use", "name": tool, "input": {}},
            ]},
        })
        .to_string()
    }

    #[test]
    fn session_shape_counts_turns_tool_calls_errors_and_span() {
        let file = write_transcript(&[
            shape_user_line("text", false, "2026-09-23T04:00:00Z"),
            tool_use_line("msg_1", "Bash", "2026-09-23T04:01:00Z"),
            shape_user_line("tool_result", false, "2026-09-23T04:01:30Z"),
            tool_use_line("msg_2", "Read", "2026-09-23T04:02:00Z"),
            shape_user_line("tool_result", true, "2026-09-23T04:02:30Z"),
            shape_user_line("text", false, "2026-09-23T04:03:00Z"),
        ]);
        let parsed = parse_transcript(file.path(), fallback());

        assert_eq!(parsed.turns, 2, "two human turns; tool results are not turns");
        assert_eq!(parsed.tool_errors, 1);
        let calls: Vec<(&String, &u64)> = parsed.tool_calls.iter().collect();
        assert_eq!(calls, vec![(&"Bash".to_string(), &1), (&"Read".to_string(), &1)]);
        assert!(parsed
            .first_timestamp
            .unwrap()
            .to_rfc3339()
            .starts_with("2026-09-23T04:00:00"));
        assert!(parsed
            .last_timestamp
            .unwrap()
            .to_rfc3339()
            .starts_with("2026-09-23T04:03:00"));
    }

    #[test]
    fn a_streamed_repeat_of_an_assistant_message_counts_its_blocks_once() {
        let file = write_transcript(&[
            tool_use_line("msg_1", "Bash", "2026-09-23T04:00:00Z"),
            // Same message.id restated per chunk — blocks deduped by id.
            tool_use_line("msg_1", "Bash", "2026-09-23T04:00:05Z"),
            tool_use_line("msg_1", "Bash", "2026-09-23T04:00:10Z"),
        ]);
        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.tool_calls.get("Bash"), Some(&1));
    }

    #[test]
    fn a_record_without_timestamps_leaves_the_span_unset() {
        let file = write_transcript(&[serde_json::json!({
            "type": "user", "sessionId": "s1",
            "message": {"role": "user", "content": "hi"},
        })
        .to_string()]);
        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.first_timestamp, None);
        assert_eq!(parsed.last_timestamp, None);
        assert_eq!(parsed.turns, 1);
    }

    // ------------------------------------------------------------------
    // Session analysis (Issue #8760) — ordered tool calls and the
    // tool_use/tool_result timing pair, matched by call id.
    // ------------------------------------------------------------------

    /// A `tool_use` block carrying its own opaque call `id` (the real shape
    /// Claude Code writes; `tool_use_line` above omits it deliberately for
    /// the session-shape-only tests it serves).
    fn tool_use_with_call_id(msg_id: &str, call_id: &str, tool: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "assistant", "timestamp": ts, "sessionId": "s1",
            "message": {"id": msg_id, "model": "m", "content": [
                {"type": "tool_use", "id": call_id, "name": tool, "input": {}},
            ]},
        })
        .to_string()
    }

    /// A `tool_result` block paired to `call_id` by `tool_use_id`.
    fn tool_result_for_call(call_id: &str, is_error: bool, ts: &str) -> String {
        serde_json::json!({
            "type": "user", "timestamp": ts, "sessionId": "s1",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": call_id, "is_error": is_error},
            ]},
        })
        .to_string()
    }

    #[test]
    fn tool_calls_are_recorded_in_call_order() {
        let file = write_transcript(&[
            tool_use_with_call_id("m1", "c1", "Bash", "2026-09-23T04:00:00Z"),
            tool_result_for_call("c1", false, "2026-09-23T04:00:05Z"),
            tool_use_with_call_id("m2", "c2", "Read", "2026-09-23T04:01:00Z"),
            tool_result_for_call("c2", false, "2026-09-23T04:01:01Z"),
            tool_use_with_call_id("m3", "c3", "Bash", "2026-09-23T04:02:00Z"),
            tool_result_for_call("c3", false, "2026-09-23T04:02:01Z"),
        ]);
        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.tool_call_order, vec!["Bash", "Read", "Bash"]);
    }

    #[test]
    fn the_longest_paired_tool_call_wins() {
        let file = write_transcript(&[
            tool_use_with_call_id("m1", "c1", "Bash", "2026-09-23T04:00:00Z"),
            tool_result_for_call("c1", false, "2026-09-23T04:00:05Z"), // 5s
            tool_use_with_call_id("m2", "c2", "Read", "2026-09-23T04:01:00Z"),
            tool_result_for_call("c2", false, "2026-09-23T04:03:00Z"), // 120s
            tool_use_with_call_id("m3", "c3", "Grep", "2026-09-23T04:04:00Z"),
            tool_result_for_call("c3", false, "2026-09-23T04:04:02Z"), // 2s
        ]);
        let parsed = parse_transcript(file.path(), fallback());
        let longest = parsed.longest_tool_call.expect("a pair was matched");
        assert_eq!(longest.tool, "Read");
        assert_eq!(longest.duration_ms, 120_000);
    }

    #[test]
    fn an_unpaired_tool_use_never_yields_a_longest_call() {
        // No matching tool_result at all: nothing to pair, so the field
        // stays absent rather than guessing a duration of zero.
        let file = write_transcript(&[tool_use_with_call_id(
            "m1",
            "c1",
            "Bash",
            "2026-09-23T04:00:00Z",
        )]);
        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.longest_tool_call, None);
        assert_eq!(parsed.tool_call_order, vec!["Bash"]);
    }

    #[test]
    fn tool_use_blocks_with_no_call_id_never_pair_but_still_count() {
        // The existing session-shape fixtures never set a block-level `id`;
        // this must remain a legitimate, unpaired shape (older/short
        // transcripts, or a hand-built fixture) rather than a parse error.
        let file = write_transcript(&[
            tool_use_line("msg_1", "Bash", "2026-09-23T04:00:00Z"),
            shape_user_line("tool_result", false, "2026-09-23T04:00:05Z"),
        ]);
        let parsed = parse_transcript(file.path(), fallback());
        assert_eq!(parsed.longest_tool_call, None);
        assert_eq!(parsed.tool_call_order, vec!["Bash"]);
    }
}
