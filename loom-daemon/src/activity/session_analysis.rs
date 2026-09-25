//! Build the `session.analysis` telemetry record from one parsed transcript
//! and its already-built `session.summary` record (Issue #8760, G3 part 2 of
//! epic #8714).
//!
//! Pure derivation only — this module reads a
//! [`SessionSummaryRecord`](crate::telemetry::SessionSummaryRecord) (for
//! identity: `repo`/`visibility`/`session_id`/`parent_session_id`, plus the
//! token totals the anomaly check reads) alongside the
//! [`ParsedTranscript`](super::transcript_parse::ParsedTranscript) that
//! produced it (for the tool-call order, the paired `tool_use`/`tool_result`
//! timing, and the per-model usage buckets a multi-model session needs for
//! an accurate cost) and produces a
//! [`SessionAnalysisRecord`](crate::telemetry::SessionAnalysisRecord). The
//! ingest pass ([`super::transcript_ingest`]) owns *when* it is emitted
//! (the same point `session.summary` is built and pushed, so a still-growing
//! session is re-analyzed on each re-read pass) and *where* it goes (the
//! observability
//! [`SessionAnalysisSink`](crate::observability::session_analysis::SessionAnalysisSink),
//! when one is configured).
//!
//! # Bounded, mechanical derivation only
//!
//! Per #8714's own scoping for this slice: retry-loop detection, the
//! longest paired tool call, a USD cost estimate (from the existing single
//! [`ModelPricing`] rate card), and a fixed-threshold anomaly flag. No
//! LLM-written prose summary — that is explicitly a later slice.
//!
//! # Wire safety
//!
//! Every field here is sourced from the parse's counts, ids, allowlisted
//! tool names, and timestamps — none of which carry message text, tool
//! arguments or tool output (see `scan_session_shape`'s doc in
//! `transcript_parse`). The unit tests pin that by fixture, mirroring
//! `session_summary`'s own redaction test.

use crate::activity::resource_usage::ModelPricing;
use crate::activity::transcript_parse::ParsedTranscript;
use crate::telemetry::{
    AnomalyFlag, LongestToolCall, RetryLoop, SessionAnalysisRecord, SessionSummaryRecord,
    HIGH_TOKEN_USAGE_THRESHOLD, RETRY_LOOP_MIN_RUN,
};

/// Derive one transcript's `session.analysis` record from its already-built
/// `session.summary` record and the [`ParsedTranscript`] both are sourced
/// from.
#[must_use]
pub fn build_session_analysis(
    summary: &SessionSummaryRecord,
    parsed: &ParsedTranscript,
) -> SessionAnalysisRecord {
    SessionAnalysisRecord {
        repo: summary.repo.clone(),
        visibility: summary.visibility,
        session_id: summary.session_id.clone(),
        parent_session_id: summary.parent_session_id.clone(),
        retry_loops: detect_retry_loops(&parsed.tool_call_order),
        longest_tool_call: parsed
            .longest_tool_call
            .as_ref()
            .map(|span| LongestToolCall {
                tool: span.tool.clone(),
                duration_ms: span.duration_ms,
            }),
        cost_usd: cost_from_buckets(parsed),
        anomalies: detect_anomalies(summary),
    }
}

/// Scan `order` (the session's tool calls in call order — see
/// [`ParsedTranscript::tool_call_order`]) for maximal runs of
/// [`RETRY_LOOP_MIN_RUN`] or more consecutive invocations of the identical
/// tool name.
fn detect_retry_loops(order: &[String]) -> Vec<RetryLoop> {
    let mut loops = Vec::new();
    let mut i = 0;
    while i < order.len() {
        let mut j = i + 1;
        while j < order.len() && order[j] == order[i] {
            j += 1;
        }
        // `j - i` never overflows u32 in practice (a transcript's tool-call
        // count is bounded well below u32::MAX), but saturate rather than
        // panic on a pathological input.
        let run_len = u32::try_from(j - i).unwrap_or(u32::MAX);
        if run_len >= RETRY_LOOP_MIN_RUN {
            loops.push(RetryLoop {
                tool: order[i].clone(),
                length: run_len,
            });
        }
        i = j;
    }
    loops
}

/// USD cost, summed per-model across `parsed`'s own usage buckets via the
/// shared rate card ([`ModelPricing::for_model`]/`calculate_cost`) — the
/// same helper `transcript_ingest::bucket_cost` uses, so this and
/// `activity.db`'s own cost accounting can never disagree about what a
/// bucket cost. `None` when the transcript contributed no usage buckets at
/// all (never a fabricated `0.0`).
fn cost_from_buckets(parsed: &ParsedTranscript) -> Option<f64> {
    if parsed.buckets.is_empty() {
        return None;
    }
    let total: f64 = parsed
        .buckets
        .iter()
        .map(|bucket| {
            ModelPricing::for_model(&bucket.model).calculate_cost(
                bucket.tokens_input,
                bucket.tokens_output,
                Some(bucket.tokens_cache_read),
                Some(bucket.tokens_cache_write),
            )
        })
        .sum();
    Some(total)
}

/// Raise every anomaly flag class this bounded slice checks. One class
/// today ([`AnomalyFlag::HighTokenUsage`]); a future class is an additional
/// push here, not a rewrite.
fn detect_anomalies(summary: &SessionSummaryRecord) -> Vec<AnomalyFlag> {
    let mut flags = Vec::new();
    if summary.tokens_input.saturating_add(summary.tokens_output) > HIGH_TOKEN_USAGE_THRESHOLD {
        flags.push(AnomalyFlag::HighTokenUsage);
    }
    flags
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::activity::session_summary::build_session_summary;
    use crate::activity::transcript_parse::parse_transcript;
    use chrono::TimeZone as _;
    use std::path::Path;

    fn fallback() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 9, 23, 12, 0, 0).unwrap()
    }

    fn write_transcript(dir: &Path, name: &str, lines: &[String]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        for line in lines {
            std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()).unwrap();
        }
        path
    }

    fn user_turn_line(text: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "message": {"role": "user", "content": text},
        })
        .to_string()
    }

    fn tool_use_line(msg_id: &str, call_id: &str, tool: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "message": {
                "model": "claude-sonnet-5",
                "id": msg_id,
                "content": [{"type": "tool_use", "id": call_id, "name": tool, "input": {}}],
                "usage": {"input_tokens": 1, "output_tokens": 1},
            },
        })
        .to_string()
    }

    fn tool_result_line(call_id: &str, is_error: bool, ts: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": ts,
            "sessionId": "uuid-a",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": call_id, "is_error": is_error},
            ]},
        })
        .to_string()
    }

    /// Build the `(summary, analysis)` pair the way `transcript_ingest`
    /// does — parse once, build `session.summary`, then derive
    /// `session.analysis` from both.
    fn analyze(path: &Path) -> SessionAnalysisRecord {
        let parsed = parse_transcript(path, fallback());
        let summary = build_session_summary(path, &parsed);
        build_session_analysis(&summary, &parsed)
    }

    #[test]
    fn a_run_of_three_or_more_identical_calls_is_a_retry_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_transcript(
            tmp.path(),
            "uuid-a.jsonl",
            &[
                user_turn_line("do the thing", "2026-09-23T04:00:00Z"),
                tool_use_line("m1", "c1", "Bash", "2026-09-23T04:00:01Z"),
                tool_result_line("c1", true, "2026-09-23T04:00:02Z"),
                tool_use_line("m2", "c2", "Bash", "2026-09-23T04:00:03Z"),
                tool_result_line("c2", true, "2026-09-23T04:00:04Z"),
                tool_use_line("m3", "c3", "Bash", "2026-09-23T04:00:05Z"),
                tool_result_line("c3", true, "2026-09-23T04:00:06Z"),
            ],
        );

        let record = analyze(&path);
        assert_eq!(
            record.retry_loops,
            vec![RetryLoop {
                tool: "Bash".to_string(),
                length: 3
            }]
        );
    }

    #[test]
    fn two_identical_calls_in_a_row_is_not_yet_a_retry_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_transcript(
            tmp.path(),
            "uuid-a.jsonl",
            &[
                user_turn_line("read two files", "2026-09-23T04:00:00Z"),
                tool_use_line("m1", "c1", "Read", "2026-09-23T04:00:01Z"),
                tool_result_line("c1", false, "2026-09-23T04:00:02Z"),
                tool_use_line("m2", "c2", "Read", "2026-09-23T04:00:03Z"),
                tool_result_line("c2", false, "2026-09-23T04:00:04Z"),
            ],
        );

        let record = analyze(&path);
        assert_eq!(record.retry_loops, Vec::new(), "two in a row is below the threshold");
    }

    #[test]
    fn a_broken_run_never_merges_across_a_different_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_transcript(
            tmp.path(),
            "uuid-a.jsonl",
            &[
                user_turn_line("interleaved calls", "2026-09-23T04:00:00Z"),
                tool_use_line("m1", "c1", "Bash", "2026-09-23T04:00:01Z"),
                tool_result_line("c1", false, "2026-09-23T04:00:02Z"),
                tool_use_line("m2", "c2", "Read", "2026-09-23T04:00:03Z"),
                tool_result_line("c2", false, "2026-09-23T04:00:04Z"),
                tool_use_line("m3", "c3", "Bash", "2026-09-23T04:00:05Z"),
                tool_result_line("c3", false, "2026-09-23T04:00:06Z"),
            ],
        );

        let record = analyze(&path);
        assert_eq!(record.retry_loops, Vec::new(), "Bash, Read, Bash is not a run of Bash");
    }

    #[test]
    fn the_longest_tool_call_and_cost_are_derived_from_the_parsed_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_transcript(
            tmp.path(),
            "uuid-a.jsonl",
            &[
                user_turn_line("one call", "2026-09-23T04:00:00Z"),
                tool_use_line("m1", "c1", "Bash", "2026-09-23T04:00:00Z"),
                tool_result_line("c1", false, "2026-09-23T04:05:00Z"), // 5 minutes
            ],
        );

        let record = analyze(&path);
        let longest = record.longest_tool_call.expect("a pair was matched");
        assert_eq!(longest.tool, "Bash");
        assert_eq!(longest.duration_ms, 300_000);
        // One assistant usage record (1 input / 1 output token) on
        // claude-sonnet-5 contributes a nonzero, computed (never fabricated
        // zero) cost.
        assert!(record.cost_usd.unwrap() > 0.0);
    }

    #[test]
    fn a_session_with_no_usage_buckets_has_no_cost() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_transcript(
            tmp.path(),
            "uuid-a.jsonl",
            &[user_turn_line("no assistant reply", "2026-09-23T04:00:00Z")],
        );

        let record = analyze(&path);
        assert_eq!(record.cost_usd, None);
        assert_eq!(record.anomalies, Vec::new());
    }

    #[test]
    fn high_token_usage_raises_the_anomaly_flag() {
        let summary = SessionSummaryRecord {
            repo: "loom".to_string(),
            visibility: crate::telemetry::RepoVisibility::Private,
            session_id: "uuid-a".to_string(),
            parent_session_id: None,
            runtime: "claude".to_string(),
            role: None,
            issue: None,
            pr_number: None,
            models: vec!["claude-sonnet-5".to_string()],
            tokens_input: HIGH_TOKEN_USAGE_THRESHOLD,
            tokens_output: 1,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            wall_ms: 0,
            turns: 1,
            tool_calls: Vec::new(),
            tool_errors: 0,
            outcome: None,
        };
        let parsed = ParsedTranscript::default();
        let record = build_session_analysis(&summary, &parsed);
        assert_eq!(record.anomalies, vec![AnomalyFlag::HighTokenUsage]);
    }

    #[test]
    fn ordinary_token_usage_raises_no_anomaly() {
        let summary = SessionSummaryRecord {
            repo: "loom".to_string(),
            visibility: crate::telemetry::RepoVisibility::Private,
            session_id: "uuid-a".to_string(),
            parent_session_id: None,
            runtime: "claude".to_string(),
            role: None,
            issue: None,
            pr_number: None,
            models: vec!["claude-sonnet-5".to_string()],
            tokens_input: 100,
            tokens_output: 100,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            wall_ms: 0,
            turns: 1,
            tool_calls: Vec::new(),
            tool_errors: 0,
            outcome: None,
        };
        let parsed = ParsedTranscript::default();
        let record = build_session_analysis(&summary, &parsed);
        assert_eq!(record.anomalies, Vec::new());
    }

    /// Issue #8760's redaction criterion, mirroring #8757's own: a
    /// transcript containing a prompt, raw tool output, a key-shaped string
    /// and an email produces a `session.analysis` record whose JSON
    /// contains none of the four.
    #[test]
    fn no_prompt_tool_output_key_or_email_ever_reaches_the_record() {
        let tmp = tempfile::tempdir().unwrap();
        let secret_key = "sk-ant-api03-SECRETKEYVALUE000000000";
        let email = "agent-secret@example.com";
        let path = write_transcript(
            tmp.path(),
            "uuid-a.jsonl",
            &[
                user_turn_line(
                    &format!(
                        "Investigate {email} using credential {secret_key} and read ~/notes.txt"
                    ),
                    "2026-09-23T04:00:00Z",
                ),
                tool_use_line("m1", "c1", "Read", "2026-09-23T04:00:01Z"),
                tool_result_line("c1", false, "2026-09-23T04:00:02Z"),
                tool_use_line("m2", "c2", "Read", "2026-09-23T04:00:03Z"),
                tool_result_line("c2", false, "2026-09-23T04:00:04Z"),
                tool_use_line("m3", "c3", "Read", "2026-09-23T04:00:05Z"),
                tool_result_line("c3", false, "2026-09-23T04:00:06Z"),
            ],
        );

        let record = analyze(&path);
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("SECRET"), "raw secret material leaked: {json}");
        assert!(!json.contains(email), "email leaked: {json}");
        assert!(!json.contains("notes.txt"), "prompt path leaked: {json}");
        // The record still carries the real shape (a genuine 3-run retry
        // loop of Read calls).
        assert_eq!(record.retry_loops.len(), 1);
        assert_eq!(record.retry_loops[0].tool, "Read");
    }
}
