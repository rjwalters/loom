//! `message.id` dedupe regression tests (issue #8186, fixed by #9303).

use super::*;
use crate::script_helpers::sweep_experiment::{
    sum_transcript_usage, sum_transcript_usage_by_model,
};

/// One streamed assistant message written as three chunks that share
/// `msg_A` (identical cumulative usage, as measured on real transcripts), a
/// second message `msg_B` whose chunks *grow*, and one id-less record.
fn streamed_transcript() -> String {
    let chunk = |id: &str, ts: &str, input: i64, output: i64, read: i64, w5: i64, w1: i64| {
        serde_json::json!({"type": "assistant", "timestamp": ts, "message": {
            "model": "claude-opus-5", "id": id, "usage": {
                "input_tokens": input, "output_tokens": output,
                "cache_read_input_tokens": read,
                "cache_creation_input_tokens": w5 + w1,
                "cache_creation": {"ephemeral_5m_input_tokens": w5,
                                   "ephemeral_1h_input_tokens": w1}}}})
        .to_string()
    };
    [
        chunk("msg_A", "2026-09-28T01:00:00Z", 10, 100, 1000, 5, 50),
        chunk("msg_A", "2026-09-28T01:00:01Z", 10, 100, 1000, 5, 50),
        chunk("msg_A", "2026-09-28T01:00:02Z", 10, 100, 1000, 5, 50),
        chunk("msg_B", "2026-09-28T01:01:00Z", 3, 7, 0, 0, 0),
        chunk("msg_B", "2026-09-28T01:01:01Z", 3, 40, 0, 0, 0),
        r#"{"message":{"model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":1}}}"#
            .to_string(),
    ]
    .join("\n")
}

#[test]
fn repeated_message_id_chunks_count_once_at_their_maximum() {
    let mut fold = UsageFold::default();
    fold.add_text(&streamed_transcript());
    assert_eq!(fold.blocks, 6, "every block is seen");
    let rows = fold.rows();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    // msg_A once + msg_B at its max + the id-less record.
    assert_eq!(row.input, 10 + 3 + 1);
    assert_eq!(row.output, 100 + 40 + 1);
    assert_eq!(row.cache_read, 1000);
    assert_eq!(row.cache_write_5m, 5);
    assert_eq!(row.cache_write_1h, 50);
    assert_eq!(fold.first_timestamp.as_deref(), Some("2026-09-28T01:00:00Z"));
    assert_eq!(fold.last_timestamp.as_deref(), Some("2026-09-28T01:01:01Z"));
}

#[test]
fn both_shared_readers_dedupe_and_still_reconcile() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent-x.jsonl");
    std::fs::write(&path, streamed_transcript()).unwrap();

    let rows = sum_transcript_usage_by_model(&path);
    let grouped: i64 = rows
        .iter()
        .map(|r| r.input + r.output + r.cache_read + r.cache_write_5m + r.cache_write_1h)
        .sum();
    assert_eq!(grouped, 14 + 141 + 1000 + 55, "deduped, not summed per chunk");

    let flat = sum_transcript_usage(&path);
    assert_eq!(flat.usage_blocks, 6);
    assert_eq!(flat.input_tokens, 14);
    assert_eq!(flat.output_tokens, 141);
    assert_eq!(flat.cache_read_input_tokens, 1000);
    assert_eq!(flat.cache_creation_input_tokens, 55);
    assert_eq!(flat.model.as_deref(), Some("claude-opus-5"));
}

#[test]
fn merge_rows_adds_matching_tuples() {
    let mut fold = UsageFold::default();
    fold.add_text(&streamed_transcript());
    let mut totals = BTreeMap::new();
    merge_rows(&mut totals, fold.rows());
    merge_rows(&mut totals, fold.rows());
    let merged: Vec<_> = totals.into_values().collect();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].input, 28);
}
