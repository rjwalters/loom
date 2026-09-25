//! Tests for the Claude burn source (Issues #8857, #8930).

use std::io::Write;

use chrono::{DateTime, Duration, Utc};

use super::{ClaudeMessages, ClaudeSource};
use crate::observability::ops::quota::burn::{Burn, BurnEvent, Emit, ModelBurn};

/// One assistant record carrying usage, as Claude Code writes it.
fn record(id: Option<&str>, model: &str, at: DateTime<Utc>, input: i64, output: i64) -> String {
    let id = id.map_or(String::new(), |id| format!(r#""id":"{id}","#));
    format!(
        r#"{{"type":"assistant","timestamp":"{}","message":{{{id}"model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":100,"cache_creation":{{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":4}}}}}}}}"#,
        at.to_rfc3339()
    )
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

fn emit_all() -> Emit {
    Emit {
        not_before: now() - Duration::days(1),
        now: now(),
    }
}

fn total(events: &[BurnEvent]) -> ModelBurn {
    let mut sum = ModelBurn::default();
    for e in events {
        sum.input += e.usage.input;
        sum.output += e.usage.output;
        sum.cache_read += e.usage.cache_read;
        sum.cache_write += e.usage.cache_write;
        sum.requests += e.usage.requests;
    }
    sum
}

#[test]
fn streamed_chunks_count_once_at_their_maximum_even_across_polls() {
    let mut messages = ClaudeMessages::default();
    let t = now() - Duration::seconds(30);
    let mut out = Vec::new();
    messages.add_line(&record(Some("m1"), "claude-opus", t, 5, 1), emit_all(), &mut out);
    // The next chunks are read by a later poll.
    let mut later = Vec::new();
    for output in [40, 30] {
        let line = record(Some("m1"), "claude-opus", t + Duration::seconds(1), 5, output);
        messages.add_line(&line, emit_all(), &mut later);
    }
    assert_eq!(
        total(&out),
        ModelBurn {
            input: 5,
            output: 1,
            cache_read: 100,
            cache_write: 7,
            requests: 1,
        }
    );
    assert_eq!(
        total(&later),
        ModelBurn {
            output: 39,
            ..ModelBurn::default()
        },
        "a later chunk adds only its increase, and no request"
    );
}

#[test]
fn a_message_copied_into_another_transcript_counts_once() {
    let mut messages = ClaudeMessages::default();
    let line = record(Some("m1"), "m", now(), 5, 5);
    let mut out = Vec::new();
    messages.add_line(&line, emit_all(), &mut out);
    messages.add_line(&line, emit_all(), &mut out);
    assert_eq!(total(&out).requests, 1);
}

#[test]
fn records_without_an_id_are_each_one_request() {
    let mut messages = ClaudeMessages::default();
    let mut out = Vec::new();
    for _ in 0..2 {
        messages.add_line(&record(None, "m", now(), 1, 1), emit_all(), &mut out);
    }
    assert_eq!(total(&out).requests, 2);
}

#[test]
fn synthetic_untimestamped_old_and_garbage_lines_are_not_counted() {
    let mut messages = ClaudeMessages::default();
    let mut out = Vec::new();
    let untimestamped = r#"{"message":{"id":"x","model":"m","usage":{"input_tokens":9}}}"#;
    for line in [
        record(Some("s"), "<synthetic>", now(), 9, 9),
        untimestamped.to_string(),
        "not json".to_string(),
        r#"{"type":"user","timestamp":"2026-09-25T00:00:00Z"}"#.to_string(),
        record(Some("history"), "m", now() - Duration::days(2), 9, 9),
    ] {
        messages.add_line(&line, emit_all(), &mut out);
    }
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn old_message_ids_are_forgotten() {
    let mut messages = ClaudeMessages::default();
    let mut out = Vec::new();
    messages.add_line(
        &record(Some("m1"), "m", now() - Duration::hours(3), 1, 1),
        emit_all(),
        &mut out,
    );
    messages.add_line(&record(Some("m2"), "m", now(), 1, 1), emit_all(), &mut out);
    messages.prune(now());
    assert_eq!(messages.seen.len(), 1);
}

fn append(path: &std::path::Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// Acceptance (#8930): one session written across two windows is split
/// between them, and nothing is counted twice — including the transcript
/// that is re-read on every tick in phase 1.
#[test]
fn a_session_spanning_two_windows_is_split_with_no_double_count() {
    let dir = tempfile::tempdir().unwrap();
    let session = dir.path().join("-repo").join("s1.jsonl");
    let subagents = dir.path().join("-repo").join("s1").join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    let base = Utc::now();
    append(&session, &(record(Some("old"), "m", base - Duration::days(2), 50, 5) + "\n"));
    let source = ClaudeSource {
        projects_dir: Some(dir.path().to_path_buf()),
        ..ClaudeSource::default()
    };
    let mut burn = Burn::new(vec![Box::new(source)]);
    assert!(burn.sample(base).is_none(), "anchor");

    append(&session, &(record(Some("a"), "m", base + Duration::seconds(10), 10, 1) + "\n"));
    // First chunk of "b" lands in window 1, its final chunk in window 2.
    append(&session, &(record(Some("b"), "m", base + Duration::seconds(200), 20, 1) + "\n"));
    append(
        &subagents.join("agent.jsonl"),
        &(record(None, "m", base + Duration::seconds(30), 1, 0) + "\n"),
    );
    append(&dir.path().join("-repo").join("notes.txt"), &record(Some("t"), "m", base, 9, 9));
    let (_, end1, first) = burn.sample(base + Duration::seconds(300)).unwrap();
    append(&session, &(record(Some("b"), "m", end1 + Duration::seconds(5), 20, 9) + "\n"));
    append(&session, &(record(Some("c"), "m", end1 + Duration::seconds(10), 40, 2) + "\n"));
    let (start2, _, second) = burn.sample(base + Duration::seconds(600)).unwrap();
    assert_eq!(start2, end1);

    let key = ("claude".to_string(), "m".to_string());
    assert_eq!(first[&key].requests, 3, "a, b and the subagent record");
    assert_eq!(first[&key].input, 31);
    assert_eq!(first[&key].output, 2);
    assert_eq!(second[&key].requests, 1, "only c is a new request");
    assert_eq!(second[&key].input, 40);
    assert_eq!(second[&key].output, 8 + 2, "b's growth plus c");
}
