//! Tests for the Kimi burn source (Issue #8930). Record shapes match the
//! `wire.jsonl` lines on a live host (`kimi-code` 2.x, 2026-09-25).

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};

use super::{KimiSource, WireLog};
use crate::observability::ops::quota::burn::{Burn, Emit};

fn usage(at: DateTime<Utc>, alias: &str, counters: (i64, i64, i64, i64), scope: &str) -> String {
    serde_json::json!({
        "type": "usage.record", "agentId": "main", "model": alias,
        "usage": {"inputOther": counters.0, "output": counters.1,
                  "inputCacheRead": counters.2, "inputCacheCreation": counters.3},
        "usageScope": scope, "time": at.timestamp_millis(),
    })
    .to_string()
}

fn llm_request(alias: &str, model: &str) -> String {
    serde_json::json!({
        "type": "llm.request", "provider": "kimi", "model": model, "modelAlias": alias,
        "systemPrompt": "sk-PLANTED-SECRET", "time": 1,
    })
    .to_string()
}

#[test]
fn usage_records_are_events_with_resolved_models_and_nothing_else_is_kept() {
    let now = Utc::now();
    let emit = Emit {
        not_before: now - Duration::hours(1),
        now,
    };
    let mut log = WireLog::default();
    let mut out = Vec::new();
    for line in [
        llm_request("kimi-code/kimi-for-coding", "kimi-k2.7-code"),
        r#"{"type":"message","content":"sk-PLANTED-SECRET usage.record"}"#.to_string(),
        usage(now, "kimi-code/kimi-for-coding", (1296, 428, 22016, 0), "turn"),
        usage(now, "unmapped", (1, 1, 0, 0), "turn"),
        usage(now, "kimi-code/kimi-for-coding", (9, 9, 9, 9), "session"),
        usage(now, "kimi-code/kimi-for-coding", (0, 0, 0, 0), "turn"),
        usage(now - Duration::hours(2), "kimi-code/kimi-for-coding", (5, 5, 5, 5), "turn"),
    ] {
        log.add_line(&line, emit, &mut out);
    }
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!((out[0].provider.as_str(), out[0].model.as_str()), ("kimi", "kimi-k2.7-code"));
    assert_eq!(
        (
            out[0].usage.input,
            out[0].usage.output,
            out[0].usage.cache_read,
            out[0].usage.requests
        ),
        (1296, 428, 22016, 1)
    );
    assert_eq!(out[1].model, "unmapped");
    assert!(!format!("{out:?}").contains("PLANTED"));
}

fn append(path: &Path, line: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

/// Acceptance (#8930): a session spanning two windows is split between them
/// with no double count.
#[test]
fn a_session_spanning_two_windows_is_split_with_no_double_count() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(".kimi-code");
    let wire = root.join("sessions/wd_repo_1/session_a/agents/main/wire.jsonl");
    let sub = root.join("sessions/wd_repo_1/session_a/agents/sub-1/wire.jsonl");
    let base = Utc::now();
    append(&wire, &llm_request("k", "kimi-k2.7-code"));
    append(&wire, &usage(base - Duration::seconds(120), "k", (1000, 1000, 0, 0), "turn"));
    let source = KimiSource {
        roots: Some(vec![root]),
        ..KimiSource::default()
    };
    let mut burn = Burn::new(vec![Box::new(source)]);
    assert!(burn.sample(base).is_none(), "anchor");
    append(&wire, &usage(base + Duration::seconds(20), "k", (10, 1, 100, 0), "turn"));
    append(
        &sub,
        &usage(base + Duration::seconds(30), "kimi-k2.7-code", (5, 1, 0, 0), "turn"),
    );
    let (_, end1, first) = burn.sample(base + Duration::seconds(300)).unwrap();
    append(&wire, &usage(end1 + Duration::seconds(5), "k", (20, 2, 200, 3), "turn"));
    let (_, _, second) = burn.sample(base + Duration::seconds(600)).unwrap();
    let key = ("kimi".to_string(), "kimi-k2.7-code".to_string());
    assert_eq!((first[&key].input, first[&key].requests), (15, 2));
    assert_eq!(
        (
            second[&key].input,
            second[&key].cache_read,
            second[&key].cache_write,
            second[&key].requests
        ),
        (20, 200, 3, 1)
    );
}
