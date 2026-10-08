//! Hermetic tests of the pause hook and ledger (#10830): a fake harness feeds
//! the hook the payloads Claude Code / Codex send, against a temp state dir.

use super::*;
use std::time::Duration;

fn env(root: &Path, item: Option<&str>) -> HookEnv {
    HookEnv {
        item: item.map(str::to_string),
        pause_root: root.to_path_buf(),
        ledger: true,
        park: Duration::from_millis(300),
        poll: Duration::from_millis(20),
        runtime: "claude".to_string(),
        harness_pid: Some(4242),
    }
}

fn pre(tool: &str, id: &str) -> String {
    serde_json::json!({"hook_event_name": "PreToolUse", "tool_name": tool, "tool_use_id": id,
        "tool_input": {"command": format!("echo {id}")}, "session_id": "sess-1"})
    .to_string()
}

fn post(tool: &str, id: &str, event: &str) -> String {
    serde_json::json!({"hook_event_name": event, "tool_name": tool, "tool_use_id": id}).to_string()
}

fn request(dir: &Path) {
    request_pause(
        dir,
        &PauseRequest {
            requested_at: "t".into(),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn an_in_session_agent_is_never_counted_or_parked() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), None);
    // Even with a request lying around for some item, no item id means inert.
    request(&item_dir(tmp.path(), "other"));
    assert_eq!(run_hook(&e, &pre("Bash", "t1")), HookOutcome::Allow);
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1, "nothing written");
}

#[test]
fn the_ledger_counts_leaf_calls_and_excludes_subagent_containers() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-1"));
    let dir = item_dir(tmp.path(), "item-1");
    assert_eq!(run_hook(&e, &pre("Bash", "a")), HookOutcome::Allow);
    assert_eq!(run_hook(&e, &pre("Read", "b")), HookOutcome::Allow);
    assert_eq!(run_hook(&e, &pre("Task", "c")), HookOutcome::Allow);
    assert_eq!(run_hook(&e, &pre("Agent", "d")), HookOutcome::Allow);
    assert_eq!(inflight_count(&dir, None), 2, "Task/Agent are not counted");
    let _ = run_hook(&e, &post("Bash", "a", "PostToolUse"));
    let _ = run_hook(&e, &post("Read", "b", "PostToolUseFailure"));
    assert_eq!(inflight_count(&dir, None), 0, "post and post-failure both decrement");
}

#[test]
fn a_parked_call_waits_for_the_ledger_then_records_the_safe_point_and_denies_on_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-2"));
    let dir = item_dir(tmp.path(), "item-2");
    let _ = run_hook(&e, &pre("Bash", "running"));
    request(&dir);

    // While `running` is still executing there is no safe point.
    let e2 = e.clone();
    let parked = std::thread::spawn(move || run_hook(&e2, &pre("Bash", "next")));
    std::thread::sleep(Duration::from_millis(100));
    assert!(read_safe_point(&dir).is_none(), "a leaf call is still executing");
    assert!(dir.join(PARKED_DIR).join("next").is_file());

    // It finishes: the parked call now records the safe point, then the park
    // window runs out and the call is denied, never run.
    let _ = run_hook(&e, &post("Bash", "running", "PostToolUse"));
    let outcome = parked.join().unwrap();
    assert_eq!(outcome, HookOutcome::Deny(DENY_REASON.to_string()));
    let sp = read_safe_point(&dir).expect("safe point written");
    assert_eq!(sp.parked_tool, "Bash");
    assert_eq!(sp.parked_summary, "echo next");
    assert_eq!(sp.parked_tool_use_id, "next");
    assert_eq!(sp.harness_pid, Some(4242));
    assert_eq!(sp.session_id.as_deref(), Some("sess-1"));
    let json = outcome.to_json().unwrap();
    assert!(json.contains("\"permissionDecision\":\"deny\""), "{json}");
}

#[test]
fn a_parked_subagent_call_is_parked_too() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-3"));
    let dir = item_dir(tmp.path(), "item-3");
    request(&dir);
    assert!(matches!(run_hook(&e, &pre("Task", "t")), HookOutcome::Deny(_)));
    assert!(read_safe_point(&dir).is_some());
}

#[test]
fn only_the_first_parked_call_writes_the_safe_point() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-4"));
    let dir = item_dir(tmp.path(), "item-4");
    request(&dir);
    let _ = run_hook(&e, &pre("Bash", "first"));
    let _ = run_hook(&e, &pre("Read", "second"));
    assert_eq!(read_safe_point(&dir).unwrap().parked_tool_use_id, "first");
    assert_eq!(std::fs::read_dir(dir.join(PARKED_DIR)).unwrap().count(), 2);
}

#[test]
fn withdrawing_the_request_releases_a_parked_call() {
    let tmp = tempfile::tempdir().unwrap();
    let mut e = env(tmp.path(), Some("item-5"));
    e.park = Duration::from_secs(30);
    let dir = item_dir(tmp.path(), "item-5");
    request(&dir);
    let e2 = e.clone();
    let parked = std::thread::spawn(move || run_hook(&e2, &pre("Bash", "x")));
    std::thread::sleep(Duration::from_millis(100));
    withdraw(&dir).unwrap();
    assert_eq!(parked.join().unwrap(), HookOutcome::Allow);
    assert_eq!(inflight_count(&dir, None), 1, "the released call is now executing");
}

#[test]
fn without_a_ledger_a_parked_call_is_itself_the_safe_point() {
    // Codex: pre-tool-use only, sequential calls.
    let tmp = tempfile::tempdir().unwrap();
    let mut e = env(tmp.path(), Some("item-6"));
    e.ledger = false;
    e.runtime = "codex".to_string();
    let dir = item_dir(tmp.path(), "item-6");
    let _ = run_hook(&e, &pre("exec_command", "a"));
    assert_eq!(inflight_count(&dir, None), 0, "no ledger entries without a ledger");
    request(&dir);
    assert!(matches!(run_hook(&e, &pre("exec_command", "b")), HookOutcome::Deny(_)));
    assert_eq!(read_safe_point(&dir).unwrap().runtime, "codex");
}

#[test]
fn garbage_and_unknown_events_are_allowed() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-7"));
    request(&item_dir(tmp.path(), "item-7"));
    assert_eq!(run_hook(&e, "not json"), HookOutcome::Allow);
    assert_eq!(run_hook(&e, r#"{"hook_event_name":"PreToolUse"}"#), HookOutcome::Allow);
    let stop = r#"{"hook_event_name":"Stop","tool_name":"Bash"}"#;
    assert_eq!(run_hook(&e, stop), HookOutcome::Allow);
}

#[test]
fn a_call_without_a_tool_use_id_still_pairs_pre_and_post() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-8"));
    let dir = item_dir(tmp.path(), "item-8");
    let p = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let q = r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let _ = run_hook(&e, p);
    assert_eq!(inflight_count(&dir, None), 1);
    let _ = run_hook(&e, q);
    assert_eq!(inflight_count(&dir, None), 0);
}

#[test]
fn item_ids_cannot_escape_the_state_dir() {
    for bad in ["", ".", "..", "a/b", "../x", "a b"] {
        assert!(!valid_item_id(bad), "{bad:?}");
    }
    assert!(valid_item_id("sweep-issue-10830-20261007T054302Z"));
}

#[test]
fn request_and_withdraw_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = item_dir(tmp.path(), "i");
    assert!(!is_requested(&dir));
    request(&dir);
    assert!(is_requested(&dir));
    withdraw(&dir).unwrap();
    withdraw(&dir).unwrap();
    assert!(!is_requested(&dir));
}

/// #10831 (Judge on #10864): a call another guard hook denied leaves a ledger
/// entry with no post-tool-use event. Past the stale limit it stops counting,
/// so the agent can still reach a safe point; a fresh entry still counts.
#[test]
fn a_stale_inflight_entry_no_longer_blocks_the_safe_point() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item"));
    let dir = item_dir(tmp.path(), "item");
    assert_eq!(run_hook(&e, &pre("Bash", "denied")), HookOutcome::Allow);
    assert_eq!(inflight_count_with(&dir, None, Duration::from_secs(600)), 1);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        inflight_count_with(&dir, None, Duration::from_millis(10)),
        0,
        "an entry older than the stale limit is not executing"
    );
}

/// #10831 (Judge on #10864): the park window is clamped below Claude Code's
/// 60 s hook timeout whatever the env asks for.
#[test]
#[serial_test::serial(roll_pause_env)]
fn the_park_window_is_clamped_below_the_hook_timeout() {
    std::env::set_var(PARK_SECS_ENV, "600");
    let e = HookEnv::from_env(None);
    std::env::remove_var(PARK_SECS_ENV);
    assert_eq!(e.park, Duration::from_secs(MAX_PARK_SECS));
}

// The clamp is only a clamp while it is under Claude Code's 60 s hook timeout.
const _: () = assert!(MAX_PARK_SECS < 60);

// ---- A safe point answers one request (Judge finding 2 on #10974) -----------

fn request_as(dir: &Path, id: &str) -> PauseRequest {
    let request = PauseRequest {
        requested_at: now_rfc3339(),
        manifest_id: Some(id.to_string()),
        ..Default::default()
    };
    request_pause(dir, &request).unwrap();
    request
}

/// The record names the request it answered, and only that request's reader
/// accepts it.
#[test]
fn a_safe_point_is_accepted_only_for_the_request_it_answered() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-a"));
    let dir = item_dir(tmp.path(), "item-a");
    let first = request_as(&dir, "rp-1");
    assert!(matches!(run_hook(&e, &pre("Bash", "x")), HookOutcome::Deny(_)));
    let sp = read_safe_point_for(&dir, &first).expect("the record answers rp-1");
    assert_eq!(sp.request_id.as_deref(), Some("rp-1"));

    let other = PauseRequest {
        requested_at: now_rfc3339(),
        manifest_id: Some("rp-2".to_string()),
        ..Default::default()
    };
    assert!(read_safe_point_for(&dir, &other).is_none(), "not rp-2's safe point");
}

/// Raising a request removes a record left by an earlier one.
#[test]
fn raising_a_request_clears_an_earlier_requests_safe_point() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-b"));
    let dir = item_dir(tmp.path(), "item-b");
    request_as(&dir, "rp-1");
    let _ = run_hook(&e, &pre("Bash", "x"));
    assert!(read_safe_point(&dir).is_some());
    let second = request_as(&dir, "rp-2");
    assert!(read_safe_point(&dir).is_none());
    assert!(read_safe_point_for(&dir, &second).is_none());
}

/// A call released by a withdrawn request takes its safe point with it: the
/// agent is running again.
#[test]
fn a_released_call_removes_the_safe_point_it_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let mut e = env(tmp.path(), Some("item-c"));
    e.park = Duration::from_secs(30);
    let dir = item_dir(tmp.path(), "item-c");
    request_as(&dir, "rp-1");
    let e2 = e.clone();
    let parked = std::thread::spawn(move || run_hook(&e2, &pre("Bash", "x")));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while read_safe_point(&dir).is_none() {
        assert!(std::time::Instant::now() < deadline, "the call never parked");
        std::thread::sleep(Duration::from_millis(10));
    }
    withdraw(&dir).unwrap();
    assert_eq!(parked.join().unwrap(), HookOutcome::Allow);
    assert!(read_safe_point(&dir).is_none(), "a stale record was left behind");
}

/// A hook parked across a stand-down and a new request answers the new one.
#[test]
fn a_call_parked_for_one_request_replaces_a_record_left_by_another() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-d"));
    let dir = item_dir(tmp.path(), "item-d");
    let second = request_as(&dir, "rp-2");
    // A record for rp-1 turns up after rp-2 was raised.
    std::fs::write(
        dir.join(SAFE_POINT_FILE),
        serde_json::json!({"reached_at": now_rfc3339(), "parked_tool": "Bash",
            "parked_summary": "old", "parked_tool_use_id": "old", "runtime": "claude",
            "request_id": "rp-1"})
        .to_string(),
    )
    .unwrap();
    assert!(read_safe_point_for(&dir, &second).is_none());
    let _ = run_hook(&e, &pre("Bash", "new"));
    let sp = read_safe_point_for(&dir, &second).expect("replaced by rp-2's own record");
    assert_eq!(sp.parked_tool_use_id, "new");
}

/// A record with no request id is from a hook older than the field. It is
/// accepted only when it is no older than the request.
#[test]
fn a_record_without_a_request_id_is_accepted_only_if_no_older_than_the_request() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = item_dir(tmp.path(), "item-e");
    std::fs::create_dir_all(&dir).unwrap();
    let write = |reached_at: &str| {
        std::fs::write(
            dir.join(SAFE_POINT_FILE),
            serde_json::json!({"reached_at": reached_at, "parked_tool": "Bash",
                "parked_summary": "s", "parked_tool_use_id": "k", "runtime": "claude"})
            .to_string(),
        )
        .unwrap();
    };
    let request = PauseRequest {
        requested_at: "2026-10-08T12:00:00Z".to_string(),
        manifest_id: Some("rp-1".to_string()),
        ..Default::default()
    };
    write("2026-10-08T11:59:59Z");
    assert!(read_safe_point_for(&dir, &request).is_none(), "older than the request");
    write("2026-10-08T12:00:03Z");
    assert!(read_safe_point_for(&dir, &request).is_some());
    write("not a time");
    assert!(
        read_safe_point_for(&dir, &request).is_none(),
        "an undatable record is not trusted"
    );
}

/// A stand-down removes its own request and record, and nothing another
/// pause has put in the same item dir.
#[test]
fn a_stand_down_touches_only_its_own_request_and_safe_point() {
    let tmp = tempfile::tempdir().unwrap();
    let e = env(tmp.path(), Some("item-f"));
    let dir = item_dir(tmp.path(), "item-f");
    request_as(&dir, "rp-2");
    let _ = run_hook(&e, &pre("Bash", "x"));

    stand_down(&dir, "rp-1"); // a superseded run, late
    assert!(is_requested(&dir) && read_safe_point(&dir).is_some());
    withdraw_for(&dir, "rp-1");
    assert!(is_requested(&dir));

    stand_down(&dir, "rp-2");
    assert!(!is_requested(&dir) && read_safe_point(&dir).is_none());
}
