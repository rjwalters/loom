//! Tests for the Codex burn source (Issue #8930).

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};

use super::{CodexSource, Rollout};
use crate::observability::ops::quota::burn::{Burn, BurnEvent, Emit};

fn turn_context(at: DateTime<Utc>, model: &str) -> String {
    serde_json::json!({
        "timestamp": at.to_rfc3339(),
        "type": "turn_context",
        "payload": {"model": model, "cwd": "/w"},
    })
    .to_string()
}

/// A `token_count` event with cumulative `(input, cached, output)`.
fn token_count(at: DateTime<Utc>, total: (i64, i64, i64)) -> String {
    serde_json::json!({
        "timestamp": at.to_rfc3339(),
        "type": "event_msg",
        "payload": {"type": "token_count", "info": {"total_token_usage": {
            "input_tokens": total.0, "cached_input_tokens": total.1,
            "output_tokens": total.2, "reasoning_output_tokens": 1,
            "total_tokens": total.0 + total.2,
        }}},
    })
    .to_string()
}

fn emit_all(now: DateTime<Utc>) -> Emit {
    Emit {
        not_before: now - Duration::days(1),
        now,
    }
}

#[test]
fn cumulative_counters_become_per_turn_deltas_and_duplicates_add_nothing() {
    let now = Utc::now();
    let mut rollout = Rollout::default();
    let mut out: Vec<BurnEvent> = Vec::new();
    for line in [
        // Usage before any model is named is unattributable.
        token_count(now, (10, 0, 1)),
        turn_context(now, "gpt-5"),
        token_count(now, (1_000, 400, 100)),
        token_count(now, (1_000, 400, 100)),
        turn_context(now, "gpt-5-mini"),
        token_count(now, (1_500, 900, 130)),
        r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#.to_string(),
    ] {
        rollout.add_line(&line, emit_all(now), &mut out);
    }
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0].provider, "codex");
    assert_eq!(out[0].model, "gpt-5");
    assert_eq!(
        (out[0].usage.input, out[0].usage.cache_read, out[0].usage.output),
        (990 - 400, 400, 99)
    );
    assert_eq!(out[1].model, "gpt-5-mini");
    assert_eq!(
        (
            out[1].usage.input,
            out[1].usage.cache_read,
            out[1].usage.output,
            out[1].usage.requests
        ),
        (0, 500, 30, 1)
    );
}

/// Issue #8966: `cache_write_input_tokens` is a subset of `input_tokens`;
/// its delta is cache-write burn and is taken out of `input`.
#[test]
fn cache_write_input_tokens_are_cache_write_burn_not_input() {
    let now = Utc::now();
    let with_writes = |total: (i64, i64, i64), writes: i64| {
        let mut value: serde_json::Value = serde_json::from_str(&token_count(now, total)).unwrap();
        value["payload"]["info"]["total_token_usage"]["cache_write_input_tokens"] = writes.into();
        value.to_string()
    };
    let mut rollout = Rollout::default();
    let mut out: Vec<BurnEvent> = Vec::new();
    for line in [
        turn_context(now, "gpt-5"),
        with_writes((1_000, 400, 100), 0),
        with_writes((3_000, 900, 150), 1_200),
    ] {
        rollout.add_line(&line, emit_all(now), &mut out);
    }
    assert_eq!(out.len(), 2, "{out:?}");
    let second = &out[1].usage;
    assert_eq!(
        (second.input, second.cache_read, second.cache_write, second.output),
        (2_000 - 500 - 1_200, 500, 1_200, 50)
    );
}

fn append(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

/// Run `f` with no Codex home or profile-root override in the environment,
/// so `home` alone decides where the stores are.
fn without_codex_env<T>(f: impl FnOnce() -> T) -> T {
    let keys = [
        crate::codex_usage::CODEX_HOME_ENV,
        crate::codex_usage::CODEX_NATIVE_HOME_ENV,
        crate::tokens_pool::paths::CODEX_PROFILE_ROOT_ENV,
    ];
    let saved: Vec<_> = keys.iter().map(|k| std::env::var_os(k)).collect();
    for key in keys {
        std::env::remove_var(key);
    }
    let result = f();
    for (key, value) in keys.iter().zip(saved) {
        if let Some(value) = value {
            std::env::set_var(key, value);
        }
    }
    result
}

/// Acceptance (#8930): a session spanning two windows is split between them
/// with no double count, and a rollout reachable through a pooled profile's
/// symlinked `sessions/` is counted once.
#[serial_test::serial(codex_home_env)]
#[test]
fn a_session_spanning_two_windows_is_split_with_no_double_count() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    let base = Utc::now();
    let day = base.format("%Y/%m/%d").to_string();
    let rollout: PathBuf = home
        .join(".codex/sessions")
        .join(&day)
        .join("rollout-2026-a.jsonl");
    append(
        &rollout,
        &[
            turn_context(base, "gpt-5"),
            token_count(base - Duration::days(2), (5_000, 0, 50)),
        ],
    );
    let profile = home.join(".loom/codex-profiles/acct-2");
    std::fs::create_dir_all(&profile).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(home.join(".codex/sessions"), profile.join("sessions")).unwrap();
    // Credential-bearing sibling that must never be read.
    std::fs::write(home.join(".codex/auth.json"), "{\"token_count\":1}\n").unwrap();

    let source = CodexSource {
        home: Some(home.clone()),
        ..CodexSource::default()
    };
    let mut burn = Burn::new(vec![Box::new(source)]);
    let (first, second) = without_codex_env(|| {
        assert!(burn.sample(base).is_none(), "anchor: history is a baseline only");
        let t1 = base + Duration::seconds(30);
        append(
            &rollout,
            &[
                token_count(t1, (6_000, 400, 80)),
                token_count(t1, (6_000, 400, 80)),
            ],
        );
        let (_, end1, first) = burn.sample(base + Duration::seconds(300)).unwrap();
        let t2 = end1 + Duration::seconds(10);
        append(&rollout, &[token_count(t2, (7_000, 900, 100))]);
        let (_, _, second) = burn.sample(base + Duration::seconds(600)).unwrap();
        (first, second)
    });
    let key = ("codex".to_string(), "gpt-5".to_string());
    assert_eq!(
        (
            first[&key].input,
            first[&key].cache_read,
            first[&key].output,
            first[&key].requests
        ),
        (600, 400, 30, 1)
    );
    assert_eq!(
        (
            second[&key].input,
            second[&key].cache_read,
            second[&key].output,
            second[&key].requests
        ),
        (500, 500, 20, 1)
    );
}
