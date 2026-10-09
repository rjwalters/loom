//! Diagnosing a missed safe point from the item's pause state (#11049).

use super::*;
use crate::roll_pause::{item_dir, request_pause, run_hook, HookEnv, HookOutcome};
use std::time::Duration;

fn env(root: &Path, item: &str) -> HookEnv {
    HookEnv {
        item: Some(item.to_string()),
        pause_root: root.to_path_buf(),
        ledger: true,
        park: Duration::from_millis(100),
        poll: Duration::from_millis(10),
        runtime: "claude".to_string(),
        harness_pid: None,
    }
}

fn event(name: &str, id: &str) -> String {
    serde_json::json!({"hook_event_name": name, "tool_name": "Bash", "tool_use_id": id,
        "tool_input": {"command": "true"}})
    .to_string()
}

fn raise(dir: &Path) -> PauseRequest {
    let r = PauseRequest {
        requested_at: chrono::Utc::now().to_rfc3339(),
        manifest_id: Some("rp-1".to_string()),
        ..Default::default()
    };
    request_pause(dir, &r).unwrap();
    r
}

/// Make every trace in `dir` look older than a request raised now.
fn age(dir: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(30);
    let walk = |p: &Path| {
        let f = std::fs::File::options().write(true).open(p);
        if let Ok(f) = f {
            let _ = f.set_modified(old);
        }
    };
    walk(&dir.join(SEEN_FILE));
    for sub in [INFLIGHT_DIR, PARKED_DIR] {
        if let Ok(rd) = std::fs::read_dir(dir.join(sub)) {
            for e in rd.flatten() {
                walk(&e.path());
            }
        }
    }
}

#[test]
fn no_trace_of_the_hook_is_no_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = item_dir(tmp.path(), "item-1");
    let r = raise(&dir);
    let m = diagnose(Some(&dir), &r);
    assert_eq!(m.cause, "no-hook", "{m:?}");
    assert!(m.detail.contains("hook config"), "{m:?}");
    assert_eq!(diagnose(None, &r).cause, "no-hook");
}

#[test]
fn a_hook_that_last_ran_before_the_request_is_no_tool_call() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), "item-2");
    let dir = item_dir(tmp.path(), "item-2");
    // One call is still running when the pause is requested, and none starts.
    assert_eq!(run_hook(&e, &event("PreToolUse", "long")), HookOutcome::Allow);
    age(&dir);
    let r = raise(&dir);
    let m = diagnose(Some(&dir), &r);
    assert_eq!(m.cause, "no-tool-call", "{m:?}");
    assert!(m.detail.contains("1 leaf call(s) in flight"), "{m:?}");
    assert!(m.detail.contains("before the request"), "{m:?}");
}

#[test]
fn a_call_parked_behind_an_open_ledger_is_hook_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), "item-3");
    let dir = item_dir(tmp.path(), "item-3");
    let _ = run_hook(&e, &event("PreToolUse", "running"));
    age(&dir);
    let r = raise(&dir);
    // Parks, cannot reach a safe point while `running` runs, then denies.
    assert!(matches!(run_hook(&e, &event("PreToolUse", "next")), HookOutcome::Deny(_)));
    assert!(crate::roll_pause::read_safe_point(&dir).is_none());
    let m = diagnose(Some(&dir), &r);
    assert_eq!(m.cause, "hook-refused", "{m:?}");
    assert!(m.detail.contains("parked 1 call(s), but 1 leaf call(s)"), "{m:?}");
}

#[test]
fn the_hook_marks_every_pre_tool_use_it_sees() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), "item-4");
    let dir = item_dir(tmp.path(), "item-4");
    let _ = run_hook(&e, &event("PostToolUse", "a"));
    assert!(!dir.join(SEEN_FILE).exists(), "only PreToolUse marks the hook as run");
    let _ = run_hook(&e, &event("PreToolUse", "a"));
    assert!(dir.join(SEEN_FILE).is_file());
}

#[test]
fn the_record_round_trips_and_tolerates_an_unknown_cause() {
    let m: SafePointMiss = serde_json::from_str(r#"{"cause": "from-the-future"}"#).unwrap();
    assert_eq!(m.cause, "from-the-future");
    assert_eq!(m.detail, "");
}
