use super::*;
use std::path::PathBuf;

/// A Pi 0.85.1 `--mode json` stream, event shapes copied from the shipped
/// package (see the module doc's "Schema provenance"): header, lifecycle
/// events, a user and two assistant `message_end`s, a cumulative
/// `message_update`, and the three places that REPEAT an assistant message's
/// usage (`entry_appended`, `turn_end`, `agent_end`) and must not be counted.
fn stream(session: &str, cwd: &str, t0: i64) -> String {
    let usage = |input: i64, output: i64, read: i64, write: i64| {
        serde_json::json!({
            "input": input, "output": output, "cacheRead": read, "cacheWrite": write,
            "reasoning": 5, "totalTokens": input + output + read + write,
            "cost": {"input": 0.1, "output": 0.2, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.3}
        })
    };
    let assistant = |model: &str, u: serde_json::Value, at: i64| {
        serde_json::json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "secret-looking text is never retained"}],
            "api": "openai-completions", "provider": "friendli", "model": model,
            "usage": u, "stopReason": "stop", "timestamp": at
        })
    };
    let first = assistant("zai-org/GLM-5.3", usage(1_000, 200, 4_000, 300), t0 + 1_000);
    let second = assistant("zai-org/GLM-5.3", usage(500, 50, 4_500, 0), t0 + 2_000);
    let lines = [
        serde_json::json!({"type": "session", "version": 3, "id": session,
            "timestamp": "2026-09-25T10:00:00.000Z", "cwd": cwd}),
        serde_json::json!({"type": "agent_start"}),
        serde_json::json!({"type": "turn_start"}),
        serde_json::json!({"type": "message_end", "message": {"role": "user",
            "content": "do the thing", "timestamp": t0}}),
        serde_json::json!({"type": "message_start", "message": first}),
        serde_json::json!({"type": "message_update", "usage": usage(1_000, 90, 4_000, 300),
            "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "x"}}),
        serde_json::json!({"type": "message_end", "message": first}),
        serde_json::json!({"type": "entry_appended", "entry": {"type": "message", "id": "a1b2c3d4",
            "parentId": null, "timestamp": "2026-09-25T10:00:01.000Z", "message": first}}),
        serde_json::json!({"type": "tool_execution_end", "toolCallId": "c1", "toolName": "loom_bash",
            "result": {}, "isError": false}),
        // A tool's nested LLM usage carries no model id: never counted.
        serde_json::json!({"type": "message_end", "message": {"role": "toolResult",
            "toolCallId": "c1", "toolName": "loom_bash", "content": [], "isError": false,
            "usage": usage(9_999, 9_999, 0, 0), "timestamp": t0 + 1_500}}),
        serde_json::json!({"type": "turn_end", "message": first, "toolResults": []}),
        serde_json::json!({"type": "message_end", "message": second}),
        // A compaction entry's summarization usage has no model id either.
        serde_json::json!({"type": "entry_appended", "entry": {"type": "compaction",
            "id": "f6g7h8i9", "parentId": "a1b2c3d4", "timestamp": "2026-09-25T10:00:03.000Z",
            "summary": "s", "firstKeptEntryId": "a1b2c3d4", "tokensBefore": 5,
            "usage": usage(7_777, 7_777, 0, 0)}}),
        serde_json::json!({"type": "agent_end", "messages": [first, second], "willRetry": false}),
    ];
    lines.map(|l| l.to_string()).join("\n")
}

const T0: i64 = 1_790_330_400_000; // 2026-09-25T10:00:00Z

fn at(ms: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(ms).unwrap()
}

#[test]
fn only_assistant_message_end_is_counted_and_every_repeat_is_ignored() {
    let messages = messages_in_stream(&stream("s-1", "/w/loom", T0), None);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages.iter().all(|m| m.model == "zai-org/GLM-5.3"));
    assert_eq!(messages[0].provider.as_deref(), Some("friendli"));
    assert_eq!(messages[0].session_id.as_deref(), Some("s-1"));
    assert_eq!(messages[0].cwd.as_deref(), Some("/w/loom"));
    assert_eq!(messages[0].at, at(T0 + 1_000));

    let totals = fold_messages(messages).unwrap();
    assert_eq!(totals.len(), 1);
    let row = &totals[0];
    assert_eq!(row.model, "zai-org/GLM-5.3");
    assert_eq!((row.speed.as_str(), row.service_tier.as_str()), ("standard", "standard"));
    assert_eq!(row.input, 1_500);
    // `reasoning` (5 per message) is a subset of `output` and is NOT added.
    assert_eq!(row.output, 250);
    assert_eq!(row.cache_read, 8_500);
    // No `cacheWrite1h` split reported: the flat count goes to the 1h bucket.
    assert_eq!((row.cache_write_5m, row.cache_write_1h), (0, 300));
}

#[test]
fn a_reported_one_hour_split_divides_cache_writes_between_buckets() {
    let line = serde_json::json!({"type": "message_end", "message": {
        "role": "assistant", "provider": "anthropic", "model": "claude-sonnet-4-5",
        "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 100, "cacheWrite1h": 40},
        "timestamp": T0}});
    let totals = fold_messages(messages_in_stream(&line.to_string(), None)).unwrap();
    assert_eq!((totals[0].cache_write_5m, totals[0].cache_write_1h), (60, 40));
}

#[test]
fn the_window_selects_by_each_messages_own_timestamp() {
    // Two dispatches appended to one per-issue log: only the second is in window.
    let log = format!(
        "==== dispatch sweep_id=a ====\n# LOOM_LAUNCH {{\"runtime\":\"pi\"}}\n{}\n\
         ==== dispatch sweep_id=b ====\n# LOOM_LAUNCH {{\"runtime\":\"pi\"}}\n{}\n",
        stream("old", "/w/loom", T0),
        stream("new", "/w/loom", T0 + 3_600_000),
    );
    let window = Some((at(T0 + 3_000_000), at(T0 + 4_000_000)));
    let messages = messages_in_stream(&log, window);
    assert_eq!(messages.len(), 2);
    assert!(messages
        .iter()
        .all(|m| m.session_id.as_deref() == Some("new")));
    assert_eq!(messages_in_stream(&log, None).len(), 4);
}

#[test]
fn a_new_launch_record_drops_the_previous_launchs_session_id() {
    let log = format!(
        "{}\n# LOOM_LAUNCH {{\"runtime\":\"pi\"}}\n{}",
        stream("s-1", "/w/loom", T0),
        serde_json::json!({"type": "message_end", "message": {"role": "assistant",
            "model": "m", "usage": {"input": 1, "output": 1}, "timestamp": T0}}),
    );
    let messages = messages_in_stream(&log, None);
    assert_eq!(messages.last().unwrap().session_id, None);
}

#[test]
fn unknown_is_not_zero_and_a_model_is_never_guessed() {
    for line in [
        // no model
        r#"{"type":"message_end","message":{"role":"assistant","usage":{"input":1,"output":1},"timestamp":1}}"#,
        // blank model
        r#"{"type":"message_end","message":{"role":"assistant","model":"  ","usage":{"input":1,"output":1},"timestamp":1}}"#,
        // no usage object
        r#"{"type":"message_end","message":{"role":"assistant","model":"m","timestamp":1}}"#,
        // usage with neither input nor output
        r#"{"type":"message_end","message":{"role":"assistant","model":"m","usage":{"cacheRead":5},"timestamp":1}}"#,
        // no timestamp
        r#"{"type":"message_end","message":{"role":"assistant","model":"m","usage":{"input":1,"output":1}}}"#,
        // an OpenCode-shaped event in the same log
        r#"{"type":"step_finish","part":{"tokens":{"input":1,"output":1}}}"#,
        "not json at all",
    ] {
        assert!(messages_in_stream(line, None).is_empty(), "{line}");
    }
    // An all-zero usage (an aborted request) is decoded but never folded into a
    // fabricated zero-token row.
    let zero = r#"{"type":"message_end","message":{"role":"assistant","model":"m","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"timestamp":1}}"#;
    assert_eq!(messages_in_stream(zero, None).len(), 1);
    assert_eq!(fold_messages(messages_in_stream(zero, None)), None);
    assert_eq!(fold_messages(Vec::new()), None);
}

fn logs_dir(root: &Path) -> PathBuf {
    let dir = root.join(".loom").join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn tokens_by_model_reads_a_sweep_log_and_returns_none_when_it_finds_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let log = logs_dir(tmp.path()).join("sweep-issue-8594.log");
    std::fs::write(&log, stream("s-1", "/w/loom", T0)).unwrap();
    let totals = tokens_by_model(&log, None).unwrap();
    assert_eq!(totals[0].input, 1_500);
    // Outside the window, missing, and usage-free logs are all `None`.
    assert_eq!(tokens_by_model(&log, Some((at(0), at(1)))), None);
    assert_eq!(tokens_by_model(&log.with_file_name("sweep-issue-1.log"), None), None);
    std::fs::write(&log, "==== dispatch ====\nno pi here\n").unwrap();
    assert_eq!(tokens_by_model(&log, None), None);
    assert_eq!(messages_in_log(&log, None), Some(Vec::new()));
}

#[test]
fn is_launch_log_path_refuses_pis_auth_store_and_every_decoy() {
    let logs = Path::new("/w/loom/.loom/logs");
    for accepted in [
        "sweep-issue-8594.log",
        "role-curator.log",
        "role-judge_2.log",
    ] {
        assert!(is_launch_log_path(&logs.join(accepted)), "{accepted}");
    }
    for refused in [
        PathBuf::from("/home/u/.pi/agent/auth.json"),
        PathBuf::from("/home/u/.pi/agent/sessions/--w-loom--/2026_x.jsonl"),
        PathBuf::from("/state/loom/native-tools/ws/uuid/pi-agent/auth.json"),
        PathBuf::from("/state/loom/native-tools/ws/uuid/pi-sessions/x.jsonl"),
        logs.join("auth.json"),
        logs.join("sweep-issue-.log"),
        logs.join("sweep-issue-12a.log"),
        logs.join("sweep-issue-1.log.bak"),
        logs.join("role-.log"),
        logs.join("role-../../x.log"),
        logs.join("daemon.log"),
        // right name, wrong directory
        PathBuf::from("/w/loom/logs/sweep-issue-1.log"),
        PathBuf::from("/w/loom/.loom/sweep-issue-1.log"),
        PathBuf::from("/home/u/.pi/agent/logs/role-x.log"),
    ] {
        assert!(!is_launch_log_path(&refused), "{}", refused.display());
    }
}

#[test]
fn a_refused_path_is_never_opened_even_when_it_holds_a_valid_stream() {
    let tmp = tempfile::tempdir().unwrap();
    // A Pi agent dir laid out like the real one, holding a readable stream
    // right beside an auth store: the reader must still refuse it.
    let agent = tmp.path().join(".pi").join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("auth.json"), stream("s", "/w", T0)).unwrap();
    assert_eq!(messages_in_log(&agent.join("auth.json"), None), None);
    assert_eq!(tokens_by_model(&agent.join("auth.json"), None), None);
}

/// This module's own source with comments stripped, so the scan below checks
/// CODE — not the module doc, which legitimately names the auth store it
/// exists to avoid.
fn reader_code() -> String {
    include_str!("../pi_usage.rs")
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !(t.starts_with("//") || t.starts_with("/*") || t.starts_with('*'))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_only_file_open_in_this_module_is_the_guarded_log_reader() {
    // Behavioural tests prove what the rows contain; this proves the module
    // never grows a second file-open. A future edit that reads Pi's agent dir
    // (which holds `auth.json`) fails here with the reason.
    let code = reader_code();
    for (idiom, allowed) in [
        ("File::open(", 1),
        ("read_to_string(", 1),
        ("File::create(", 0),
        ("fs::read(", 0),
        ("fs::read_dir(", 0),
        ("fs::write(", 0),
        ("OpenOptions", 0),
        ("include_str!", 0),
        ("Command::new(", 0),
    ] {
        assert_eq!(
            code.matches(idiom).count(),
            allowed,
            "this module's code may contain exactly {allowed} `{idiom}`: every file read \
             must go through `read_log`, which refuses any path `is_launch_log_path` rejects"
        );
    }
    let body = code
        .split_once("fn read_log(")
        .expect("read_log must exist")
        .1;
    let body = body.split_once("\nfn ").map_or(body, |(head, _)| head);
    let gate = body
        .find("if !is_launch_log_path(")
        .expect("read_log must gate on is_launch_log_path");
    let open = body.find("File::open(").expect("the one open lives here");
    assert!(gate < open, "the authorization gate must precede the open");
    // No credential-bearing name, and no Pi state location, may appear in CODE.
    let lowered = code.to_ascii_lowercase();
    for forbidden in [
        "auth.json",
        "credential",
        "models.json",
        "settings.json",
        ".pi/",
        "pi_coding_agent",
        "pi-agent",
        "pi-sessions",
        "home_dir",
        "env::var",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "this module's code must never name {forbidden}: the only file it opens is a \
             Loom launch log"
        );
    }
}
