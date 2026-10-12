//! End-to-end (#11049): a daemon-dispatched Claude session in a CONSUMER
//! repo reaches a roll's safe point.
//!
//! The Loom repo wires `roll-pause.sh` in its own committed
//! `.claude/settings.json`, so the existing tests, which ran there, could not
//! see what a consumer install lacks. This test builds a consumer install
//! from the payload (`init::initialize_workspace`, the installer step the
//! workspace resync also runs) and pins:
//!
//! 1. the cause: the install carries `.loom/hooks/roll-pause.sh` but its
//!    `.claude/settings.json` has no entry that runs it;
//! 2. the fix: `agent-resume claude-args`, which `spawn-claude.sh` calls for
//!    every pinned session, hands the launch a `--settings` that wires it;
//! 3. that the wired command, run the way the harness runs it, with the
//!    environment the daemon gives a sweep, keeps the ledger and parks the
//!    call at a safe point once a pause is requested.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use loom_daemon::roll_pause::{self, PauseRequest};

const BIN: &str = env!("CARGO_BIN_EXE_loom-daemon");
const DEFAULTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults");
const ITEM: &str = "sweep-issue-7-11049";
const SESSION: &str = "4b1d2c3e-0000-4000-8000-000000011049";

fn consumer_install(root: &Path) -> std::path::PathBuf {
    let repo = root.join("consumer");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(&repo)
        .status()
        .unwrap()
        .success());
    loom_daemon::init::initialize_workspace(repo.to_str().unwrap(), DEFAULTS, false)
        .expect("install the payload into the consumer repo");
    repo
}

/// The NUL-separated arguments `agent-resume claude-args` prints for a
/// fresh daemon dispatch launched in `repo`.
fn launch_args(root: &Path, repo: &Path) -> Vec<String> {
    let out = Command::new(BIN)
        .args(["agent-resume", "claude-args"])
        .current_dir(repo)
        .env("HOME", root.join("home"))
        .env("LOOM_CLAUDE_SESSION_ID", SESSION)
        .env("LOOM_DAEMON_ITEM_ID", ITEM)
        .env("LOOM_ROLL_PAUSE_DIR", root.join("pause"))
        .env_remove("LOOM_PROJECT_ROOT")
        .env_remove("LOOM_ROLL_PAUSE_HOOK")
        .env_remove("LOOM_RESUME_SESSION_ID")
        .env_remove("LOOM_RESUME_PROMPT")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout)
        .unwrap()
        .split('\0')
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect()
}

/// Run one wired hook command as the harness does (`sh -c`), with the sweep
/// environment and `payload` on stdin. Returns stdout.
fn run_wired(root: &Path, repo: &Path, command: &str, payload: &serde_json::Value) -> String {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(repo)
        .env("HOME", root.join("home"))
        .env("LOOM_DAEMON_ITEM_ID", ITEM)
        .env("LOOM_ROLL_PAUSE_DIR", root.join("pause"))
        .env("LOOM_DAEMON_SELF_BIN", BIN)
        .env("LOOM_ROLL_PAUSE_PARK_SECS", "2")
        .env("LOOM_ROLL_PAUSE_POLL_MS", "20")
        .env_remove("LOOM_ROLL_PAUSE_BIN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn call(event: &str, id: &str) -> serde_json::Value {
    serde_json::json!({"hook_event_name": event, "tool_name": "Bash", "tool_use_id": id,
        "tool_input": {"command": "cargo test"}, "session_id": SESSION})
}

#[test]
fn a_consumer_install_sweep_parks_at_a_safe_point() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let repo = consumer_install(root);

    // 1. The cause: the hook is installed, but nothing in the consumer's
    //    settings runs it.
    assert!(repo.join(".loom/hooks/roll-pause.sh").is_file());
    let settings = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    assert!(
        !settings.contains("roll-pause.sh"),
        "a consumer install's own settings do not wire the pause hook: {settings}"
    );

    // 2. The fix: the launch carries the wiring.
    let args = launch_args(root, &repo);
    assert_eq!(args[..2], ["--session-id".to_string(), SESSION.to_string()]);
    let at = args
        .iter()
        .position(|a| a == "--settings")
        .expect("--settings");
    let wiring: serde_json::Value = serde_json::from_str(&args[at + 1]).unwrap();
    let cmd = |event: &str| {
        let entry = &wiring["hooks"][event][0];
        assert_eq!(entry["matcher"], "*", "{event} covers every tool");
        entry["hooks"][0]["command"].as_str().unwrap().to_string()
    };
    let hook = repo.join(".loom/hooks/roll-pause.sh");
    assert!(cmd("PreToolUse").contains(&*hook.to_string_lossy()));

    // 3a. No pause yet: the ledger opens and closes, and the hook marks itself.
    let item = roll_pause::item_dir(&root.join("pause"), ITEM);
    assert_eq!(run_wired(root, &repo, &cmd("PreToolUse"), &call("PreToolUse", "t1")), "");
    assert_eq!(roll_pause::inflight_count(&item, None), 1);
    assert!(item.join(roll_pause::miss::SEEN_FILE).is_file());
    assert_eq!(run_wired(root, &repo, &cmd("PostToolUse"), &call("PostToolUse", "t1")), "");
    assert_eq!(roll_pause::inflight_count(&item, None), 0);

    // 3b. A pause is requested: the next call parks, the safe point is
    //     recorded for this request, and the call is denied when the park
    //     window runs out.
    roll_pause::request_pause(
        &item,
        &PauseRequest {
            requested_at: chrono::Utc::now().to_rfc3339(),
            manifest_id: Some("rp-11049".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    let out = run_wired(root, &repo, &cmd("PreToolUse"), &call("PreToolUse", "t2"));
    assert!(out.contains("paused for a daemon roll"), "{out}");
    let sp = roll_pause::read_safe_point(&item).expect("a safe point");
    assert_eq!(sp.parked_tool, "Bash");
    assert_eq!(sp.request_id.as_deref(), Some("rp-11049"));
    assert_eq!(sp.session_id.as_deref(), Some(SESSION));

    // An attended launch (no daemon item) is never wired.
    let out = Command::new(BIN)
        .args(["agent-resume", "claude-args"])
        .current_dir(&repo)
        .env("HOME", root.join("home"))
        .env("LOOM_CLAUDE_SESSION_ID", SESSION)
        .env_remove("LOOM_DAEMON_ITEM_ID")
        .output()
        .unwrap();
    assert!(out.status.success());
    let args = String::from_utf8(out.stdout).unwrap();
    assert!(!args.contains("--settings"), "{args:?}");
}
