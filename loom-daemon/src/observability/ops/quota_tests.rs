//! Tests for quota burn and pool state (Issue #8857).

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{burn_points, claude_burn, BurnFold, ModelBurn, PoolAccount, QuotaState};
use crate::telemetry::ops::{MetricName, MetricPoint, MetricValue};

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
}

/// One assistant record carrying usage, as Claude Code writes it.
fn record(id: Option<&str>, model: &str, secs: i64, input: i64, output: i64) -> String {
    let id = id.map_or(String::new(), |id| format!(r#""id":"{id}","#));
    format!(
        r#"{{"type":"assistant","timestamp":"{}","message":{{{id}"model":"{model}","usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":100,"cache_creation":{{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":4}}}}}}}}"#,
        at(secs).to_rfc3339()
    )
}

fn value(point: &MetricPoint) -> i64 {
    match point.value {
        MetricValue::Int(v) => v,
        MetricValue::Double(_) => panic!("expected int"),
    }
}

fn find<'a>(
    points: &'a [MetricPoint],
    name: MetricName,
    labels: &[(&str, &str)],
) -> Option<&'a MetricPoint> {
    points.iter().find(|p| {
        p.name == name
            && labels
                .iter()
                .all(|(k, v)| p.labels.get(*k).map(String::as_str) == Some(*v))
    })
}

// ------------------------------------------------------------------ burn

#[test]
fn streamed_chunks_count_once_at_their_maximum() {
    let mut fold = BurnFold::default();
    let lines = [
        record(Some("m1"), "claude-opus", 10, 5, 1),
        record(Some("m1"), "claude-opus", 11, 5, 40),
        record(Some("m1"), "claude-opus", 12, 5, 30),
    ];
    fold.add_lines(lines.iter().map(String::as_str));
    let burn = fold.window(at(0), at(100));
    assert_eq!(
        burn["claude-opus"],
        ModelBurn {
            input: 5,
            output: 40,
            cache_read: 100,
            cache_write: 7,
            requests: 1,
        }
    );
}

#[test]
fn a_message_counts_in_the_window_of_its_first_chunk_only() {
    let mut fold = BurnFold::default();
    let lines = [
        record(Some("early"), "m", 50, 10, 1),
        // Straddles the boundary at 60: first chunk before, last after.
        record(Some("straddle"), "m", 59, 20, 1),
        record(Some("straddle"), "m", 61, 20, 9),
        record(Some("late"), "m", 70, 30, 1),
    ];
    fold.add_lines(lines.iter().map(String::as_str));
    let first = fold.window(at(0), at(60));
    let second = fold.window(at(60), at(120));
    assert_eq!(first["m"].requests, 2);
    assert_eq!(first["m"].input, 30);
    assert_eq!(first["m"].output, 10, "straddling message counted at its max");
    assert_eq!(second["m"].requests, 1);
    assert_eq!(second["m"].input, 30);
    // The window is (start, end]: a message exactly at `end` is in, at
    // `start` is out, so abutting windows never both count it.
    assert_eq!(fold.window(at(50), at(59))["m"].requests, 1);
    assert!(!fold.window(at(50), at(58)).contains_key("m"));
}

#[test]
fn a_message_copied_into_another_transcript_counts_once() {
    let mut fold = BurnFold::default();
    let line = record(Some("m1"), "m", 10, 5, 5);
    fold.add_lines([line.as_str()]);
    fold.add_lines([line.as_str()]);
    assert_eq!(fold.window(at(0), at(100))["m"].requests, 1);
}

#[test]
fn records_without_an_id_are_each_one_request() {
    let mut fold = BurnFold::default();
    let lines = [record(None, "m", 10, 1, 1), record(None, "m", 11, 1, 1)];
    fold.add_lines(lines.iter().map(String::as_str));
    assert_eq!(fold.window(at(0), at(100))["m"].requests, 2);
}

#[test]
fn synthetic_untimestamped_and_garbage_lines_are_skipped() {
    let mut fold = BurnFold::default();
    let untimestamped = r#"{"message":{"id":"x","model":"m","usage":{"input_tokens":9}}}"#;
    let lines = [
        record(Some("s"), "<synthetic>", 10, 9, 9),
        untimestamped.to_string(),
        "not json".to_string(),
        r#"{"type":"user","timestamp":"2026-09-25T00:00:00Z"}"#.to_string(),
    ];
    fold.add_lines(lines.iter().map(String::as_str));
    assert!(fold.window(at(-1_000_000), at(1_000_000)).is_empty());
}

#[test]
fn burn_points_label_provider_and_model_and_skip_zeroes() {
    let by_model: BTreeMap<String, ModelBurn> = [(
        "claude-opus".to_string(),
        ModelBurn {
            input: 5,
            output: 0,
            cache_read: 100,
            cache_write: 7,
            requests: 2,
        },
    )]
    .into_iter()
    .collect();
    let points = burn_points("claude", &by_model);
    let names: Vec<MetricName> = points.iter().map(|p| p.name).collect();
    assert_eq!(
        names,
        vec![
            MetricName::LlmTokensInput,
            MetricName::LlmTokensCacheRead,
            MetricName::LlmTokensCacheWrite,
            MetricName::LlmRequests,
        ]
    );
    for point in &points {
        assert_eq!(point.labels["provider"], "claude");
        assert_eq!(point.labels["model"], "claude-opus");
    }
    assert_eq!(value(&points[3]), 2);
}

#[test]
fn claude_burn_reads_nested_transcripts_modified_in_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let subagents = dir.path().join("-repo").join("session-1").join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    // Timestamps relative to now, since file mtimes are real.
    let now = Utc::now();
    let line = |id: &str, ago: i64| {
        let mut rec = record(Some(id), "m", 0, 10, 1);
        rec = rec.replace(&at(0).to_rfc3339(), &(now - Duration::seconds(ago)).to_rfc3339());
        rec
    };
    std::fs::write(
        dir.path().join("-repo").join("session-1.jsonl"),
        format!("{}\n{}\n", line("old", 7200), line("new", 30)),
    )
    .unwrap();
    std::fs::write(subagents.join("agent-a.jsonl"), line("sub", 20) + "\n").unwrap();
    std::fs::write(dir.path().join("-repo").join("notes.txt"), line("txt", 10)).unwrap();
    let burn = claude_burn(dir.path(), now - Duration::seconds(3600), now);
    assert_eq!(burn["m"].requests, 2, "old message is outside, .txt is not a transcript");
    assert_eq!(burn["m"].input, 20);
}

#[test]
fn the_first_burn_window_only_anchors_and_later_ones_abut() {
    let mut state = QuotaState::default();
    assert_eq!(state.next_burn_window(at(1000)), None);
    let (start, end) = state.next_burn_window(at(1300)).unwrap();
    assert_eq!(start, at(1000 - super::MESSAGE_SETTLE_LAG_SECS));
    assert_eq!(end, at(1300 - super::MESSAGE_SETTLE_LAG_SECS));
    let (next_start, _) = state.next_burn_window(at(1600)).unwrap();
    assert_eq!(next_start, end);
    // A clock that went backwards yields nothing and keeps the anchor.
    assert_eq!(state.next_burn_window(at(1200)), None);
    assert_eq!(state.next_burn_window(at(1900)).unwrap().0, at(1600 - 60));
}

// ------------------------------------------------------------------ pool

fn account(provider: &str, name: &str, usable: bool, exhausted: bool) -> PoolAccount {
    PoolAccount {
        provider: provider.into(),
        account: name.into(),
        usable,
        exhausted,
    }
}

#[test]
fn pool_gauges_count_usable_and_exhausted_per_provider() {
    let mut state = QuotaState::default();
    let accounts = [
        account("claude", "a1", true, false),
        account("claude", "a2", false, true),
        account("codex", "c1", false, true),
        // Malformed/unverifiable API key: neither usable nor exhausted.
        account("zai", "z1", false, false),
    ];
    let points = state.pool_points(&accounts, at(0));
    let get = |name, labels: &[(&str, &str)]| value(find(&points, name, labels).unwrap());
    assert_eq!(get(MetricName::PoolAccounts, &[("provider", "claude"), ("state", "usable")]), 1);
    assert_eq!(
        get(MetricName::PoolAccounts, &[("provider", "claude"), ("state", "exhausted")]),
        1
    );
    assert_eq!(get(MetricName::PoolExhausted, &[("provider", "claude")]), 0);
    assert_eq!(get(MetricName::PoolExhausted, &[("provider", "codex")]), 1);
    assert_eq!(
        get(MetricName::PoolExhausted, &[("provider", "zai")]),
        0,
        "no usable account but none exhausted is not exhaustion"
    );
    assert!(
        find(&points, MetricName::PoolExhaustions, &[]).is_none()
            && find(&points, MetricName::PoolExhaustedSeconds, &[]).is_none(),
        "the first sample emits no deltas"
    );
    assert!(points.iter().all(|p| !p.labels.contains_key("account")));
}

#[test]
fn exhaustions_count_only_new_transitions_and_downtime_accrues_while_exhausted() {
    let mut state = QuotaState::default();
    state.pool_points(
        &[
            account("codex", "c1", true, false),
            account("codex", "c2", false, true),
        ],
        at(0),
    );

    // c1 runs dry too: one new exhaustion, and the pool is now exhausted.
    let second = state.pool_points(
        &[
            account("codex", "c1", false, true),
            account("codex", "c2", false, true),
        ],
        at(300),
    );
    assert_eq!(
        value(find(&second, MetricName::PoolExhaustions, &[("provider", "codex")]).unwrap()),
        1
    );
    assert_eq!(
        value(find(&second, MetricName::PoolExhausted, &[("provider", "codex")]).unwrap()),
        1
    );
    assert!(
        find(&second, MetricName::PoolExhaustedSeconds, &[]).is_none(),
        "the pool was not exhausted at the start of this interval"
    );

    // Still exhausted: the whole interval is downtime, no new exhaustion.
    let third = state.pool_points(
        &[
            account("codex", "c1", false, true),
            account("codex", "c2", false, true),
        ],
        at(600),
    );
    assert!(find(&third, MetricName::PoolExhaustions, &[]).is_none());
    assert_eq!(
        value(find(&third, MetricName::PoolExhaustedSeconds, &[("provider", "codex")]).unwrap()),
        300
    );

    // c2 recovers: the interval it recovered in still counts (sample-and-hold).
    let fourth = state.pool_points(
        &[
            account("codex", "c1", false, true),
            account("codex", "c2", true, false),
        ],
        at(900),
    );
    assert_eq!(
        value(find(&fourth, MetricName::PoolExhausted, &[("provider", "codex")]).unwrap()),
        0
    );
    assert_eq!(
        value(find(&fourth, MetricName::PoolExhaustedSeconds, &[("provider", "codex")]).unwrap()),
        300
    );
    let fifth = state.pool_points(
        &[
            account("codex", "c1", false, true),
            account("codex", "c2", true, false),
        ],
        at(1200),
    );
    assert!(find(&fifth, MetricName::PoolExhaustedSeconds, &[]).is_none());
}

#[test]
fn a_provider_first_seen_later_counts_its_exhausted_accounts_as_new() {
    let mut state = QuotaState::default();
    state.pool_points(&[account("claude", "a1", true, false)], at(0));
    let points = state.pool_points(
        &[
            account("claude", "a1", true, false),
            account("kimi", "k1", false, true),
        ],
        at(300),
    );
    assert_eq!(
        value(find(&points, MetricName::PoolExhaustions, &[("provider", "kimi")]).unwrap()),
        1
    );
}

#[test]
fn token_snapshot_accounts_map_to_pool_accounts() {
    let state = crate::telemetry::TokenAccountState {
        account: "agent-1".into(),
        provider: "claude".into(),
        rank: Some(1),
        usage_fraction: Some(0.4),
        limit_window_reset_at: None,
        exhausted: true,
    };
    assert_eq!(PoolAccount::from(&state), account("claude", "agent-1", false, true));
}
