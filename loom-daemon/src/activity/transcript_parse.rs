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

use std::collections::HashMap;
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

        if first_user_text.is_none() && obj.get("type").and_then(Value::as_str) == Some("user") {
            first_user_text = message_text(&obj);
        }

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
}
