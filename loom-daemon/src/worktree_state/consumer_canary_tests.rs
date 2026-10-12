//! Tests for the opt-in consumer canary (#8372).
//!
//! Every fixture is a real git repository: the gate resolves config against the
//! MAIN checkout via `git rev-parse --git-common-dir`, and the guard measures a
//! real linked worktree, so a mocked layer would test the wrong thing.

use super::*;
use crate::worktree_state::MANAGED_SENTINEL;
use std::process::Command;

/// A transcript line no outcome record may ever contain.
const TRANSCRIPT_CANARY_TEXT: &str = "FAKE-TRANSCRIPT-BODY-not-a-real-secret-0000";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A main checkout with a `.loom/config.json` and one managed linked worktree
/// at `<main>/.loom/worktrees/issue-1`, branched from `main`.
struct Ws {
    main: tempfile::TempDir,
    logs: tempfile::TempDir,
}

impl Ws {
    fn new(config: &str) -> Self {
        let main = tempfile::tempdir().unwrap();
        let m = main.path();
        git(m, &["init", "-q", "-b", "main"]);
        git(m, &["config", "user.email", "t@example.com"]);
        git(m, &["config", "user.name", "Test"]);
        std::fs::create_dir_all(m.join(".loom")).unwrap();
        std::fs::write(m.join(".loom/config.json"), config).unwrap();
        std::fs::write(m.join(".gitignore"), ".loom/worktrees/\n.loom-local/\n").unwrap();
        std::fs::write(m.join("README.md"), "seed\n").unwrap();
        git(m, &["add", "-A"]);
        git(m, &["commit", "-qm", "seed"]);
        let wt = m.join(".loom/worktrees/issue-1");
        git(
            m,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "feature/issue-1",
            ],
        );
        std::fs::write(wt.join(MANAGED_SENTINEL), "").unwrap();
        Ws {
            main,
            logs: tempfile::tempdir().unwrap(),
        }
    }

    fn opted_in() -> Self {
        Ws::new(r#"{"guards": {"uncommittedWorkConsumerCanary": true}}"#)
    }

    fn main(&self) -> &Path {
        self.main.path()
    }

    fn wt(&self) -> PathBuf {
        self.main.path().join(".loom/worktrees/issue-1")
    }

    fn log(&self) -> PathBuf {
        self.logs.path().join("canary.jsonl")
    }

    /// Write a transcript whose only ownership evidence is a `Write` into the
    /// worktree — the Task-subagent shape (cwd is the main checkout).
    fn transcript(&self) -> PathBuf {
        let p = self.logs.path().join("transcript.jsonl");
        let target = self.wt().join("probe.sh");
        let line = serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "text", "text": TRANSCRIPT_CANARY_TEXT},
                {"type": "tool_use", "name": "Write",
                 "input": {"file_path": target.display().to_string(), "content": TRANSCRIPT_CANARY_TEXT}}
            ]}
        });
        std::fs::write(&p, format!("{line}\n")).unwrap();
        p
    }

    fn payload(&self, event: &str, cwd: &Path, transcript: Option<&Path>, active: bool) -> String {
        serde_json::json!({
            "session_id": "sess-fake-0001",
            "transcript_path": transcript.map(|t| t.display().to_string()),
            "cwd": cwd.display().to_string(),
            "stop_hook_active": active,
            "hook_event_name": event,
        })
        .to_string()
    }

    fn run(&self, raw: &str) -> Option<serde_json::Value> {
        run(raw, "main", self.main(), Some(&self.log()), "inv-test-1")
    }

    fn records(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("each record is one JSON line"))
            .collect()
    }
}

fn is_block(out: &Option<serde_json::Value>) -> bool {
    out.as_ref()
        .and_then(|v| v.get("decision"))
        .and_then(|d| d.as_str())
        == Some("block")
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn canary_key_absent_false_or_non_boolean_is_off() {
    for cfg in [
        "{}",
        r#"{"guards": {"uncommittedWorkConsumerCanary": false}}"#,
        r#"{"guards": {"uncommittedWorkConsumerCanary": "true"}}"#,
        r#"{"guards": {"uncommittedWork": true}}"#,
    ] {
        let ws = Ws::new(cfg);
        assert!(!canary_enabled(ws.main()), "{cfg} must not opt in");
        // Even a deliverable-shaped worktree stays silent and unrecorded.
        std::fs::write(ws.wt().join("probe.sh"), "#!/bin/sh\n").unwrap();
        let out = ws.run(&ws.payload("Stop", &ws.wt(), None, false));
        assert_eq!(out, None, "{cfg}: opt-out must be silent");
        assert!(!ws.log().exists(), "{cfg}: opt-out must record nothing");
    }
}

#[test]
fn canary_true_is_on_and_resolves_from_a_worktree_to_the_main_checkout() {
    let ws = Ws::new("{}");
    // The opt-in lives only in the main checkout's gitignored local tier.
    let local = ws.main().join(crate::config_resolver::LOCAL_CONFIG_REL);
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, r#"{"guards": {"uncommittedWorkConsumerCanary": true}}"#).unwrap();

    let root = workspace_root(&ws.wt());
    assert_eq!(std::fs::canonicalize(&root).unwrap(), std::fs::canonicalize(ws.main()).unwrap());
    assert!(canary_enabled(&root));
}

#[test]
fn a_non_loom_directory_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let log = logs.path().join("c.jsonl");
    let raw =
        serde_json::json!({"cwd": dir.path().display().to_string(), "hook_event_name": "Stop"})
            .to_string();
    assert_eq!(run(&raw, "main", dir.path(), Some(&log), "inv"), None);
    assert!(!log.exists());
}

// ---------------------------------------------------------------------------
// Decisions under the canary, both events
// ---------------------------------------------------------------------------

#[test]
fn owned_deliverable_blocks_once_on_both_events_and_is_recorded() {
    for event in ["Stop", "SubagentStop"] {
        let ws = Ws::opted_in();
        std::fs::write(ws.wt().join("probe.sh"), "#!/bin/sh\n").unwrap();
        let transcript = ws.transcript();

        // Subagent shape: cwd is the main checkout, ownership from the transcript.
        let out = ws.run(&ws.payload(event, ws.main(), Some(&transcript), false));
        assert!(is_block(&out), "{event}: first stop blocks: {out:?}");

        // The one-extra-turn limit is preserved.
        let again = ws.run(&ws.payload(event, ws.main(), Some(&transcript), true));
        assert!(!is_block(&again), "{event}: stop_hook_active must not block twice");

        let recs = ws.records();
        assert_eq!(recs.len(), 2, "{event}: one record per invocation");
        assert_eq!(recs[0]["outcome"], "block");
        assert_eq!(recs[1]["outcome"], "advisory");
        for r in &recs {
            assert_eq!(r["event"], event);
            assert_eq!(r["source"], "daemon");
            assert_eq!(r["schema"], SCHEMA_VERSION);
            assert_eq!(r["session_id"], "sess-fake-0001");
            assert_eq!(r["invocation_id"], "inv-test-1");
            assert_eq!(r["version"], env!("CARGO_PKG_VERSION"));
            assert!(r["ts"].as_str().unwrap().ends_with('Z'));
            assert_eq!(r["guard_enabled"], true);
            assert_eq!(r["state"]["verdict"], "uncommitted");
            assert!(r["worktree"].as_str().unwrap().ends_with("issue-1"));
        }
        let raw_log = std::fs::read_to_string(ws.log()).unwrap();
        assert!(
            !raw_log.contains(TRANSCRIPT_CANARY_TEXT),
            "transcript contents must never reach the outcome log"
        );
        assert!(!raw_log.contains("STOP BLOCKED"), "the reason text is not recorded");
    }
}

#[test]
fn committed_empty_and_scratch_only_states_do_not_block() {
    // Empty worktree.
    let ws = Ws::opted_in();
    let out = ws.run(&ws.payload("Stop", &ws.wt(), None, false));
    assert_eq!(out, None);
    // Scratch only.
    std::fs::write(ws.wt().join(".no-changes-needed"), "already on main\n").unwrap();
    assert_eq!(ws.run(&ws.payload("Stop", &ws.wt(), None, false)), None);
    // Committed deliverable -> advisory, never a block.
    std::fs::remove_file(ws.wt().join(".no-changes-needed")).unwrap();
    std::fs::write(ws.wt().join("probe.sh"), "#!/bin/sh\n").unwrap();
    git(&ws.wt(), &["add", "probe.sh"]);
    git(&ws.wt(), &["commit", "-qm", "probe"]);
    let out = ws.run(&ws.payload("SubagentStop", &ws.wt(), None, false));
    assert!(!is_block(&out));
    let outcomes: Vec<_> = ws.records().iter().map(|r| r["outcome"].clone()).collect();
    assert_eq!(outcomes, vec!["allow", "allow", "advisory"]);
}

#[test]
fn unrelated_primary_checkout_wip_does_not_block() {
    let ws = Ws::opted_in();
    std::fs::write(ws.main().join("operator-wip.txt"), "draft\n").unwrap();
    let out = ws.run(&ws.payload("Stop", ws.main(), None, false));
    assert_eq!(out, None, "the primary checkout is never owned");
    let recs = ws.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["outcome"], "allow");
    assert_eq!(recs[0]["worktree"], serde_json::Value::Null);
    assert_eq!(recs[0]["guard_enabled"], serde_json::Value::Null);
}

#[test]
fn the_existing_disable_switch_still_works_inside_an_opted_in_canary() {
    if std::env::var(stop_hook::TOGGLE_ENV_VAR).is_ok() {
        return; // the env override wins by design; cannot assert config here
    }
    let ws =
        Ws::new(r#"{"guards": {"uncommittedWorkConsumerCanary": true, "uncommittedWork": false}}"#);
    std::fs::write(ws.wt().join("probe.sh"), "#!/bin/sh\n").unwrap();
    let out = ws.run(&ws.payload("Stop", &ws.wt(), None, false));
    assert_eq!(out, None, "guards.uncommittedWork=false disables the guard");
    let recs = ws.records();
    assert_eq!(recs[0]["outcome"], "allow");
    assert_eq!(recs[0]["guard_enabled"], false);
}

#[test]
fn a_malformed_payload_fails_open_and_is_recorded_as_an_error() {
    let ws = Ws::opted_in();
    let out = ws.run("{not json at all");
    assert_eq!(out, None);
    let recs = ws.records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["outcome"], "error");
    assert_eq!(recs[0]["error"], "malformed_payload");
}

#[test]
fn a_missing_transcript_is_an_allow() {
    let ws = Ws::opted_in();
    std::fs::write(ws.wt().join("probe.sh"), "#!/bin/sh\n").unwrap();
    let missing = ws.logs.path().join("no-such-transcript.jsonl");
    let out = ws.run(&ws.payload("SubagentStop", ws.main(), Some(&missing), false));
    assert_eq!(out, None);
    assert_eq!(ws.records()[0]["outcome"], "allow");
}

// ---------------------------------------------------------------------------
// Recording failures and bounds
// ---------------------------------------------------------------------------

#[test]
fn a_log_write_failure_fails_open_and_surfaces_the_gap() {
    let ws = Ws::opted_in();
    std::fs::write(ws.wt().join("probe.sh"), "#!/bin/sh\n").unwrap();
    // A log path whose parent is a regular file cannot be created.
    let blocker = ws.logs.path().join("not-a-dir");
    std::fs::write(&blocker, "").unwrap();
    let bad_log = blocker.join("canary.jsonl");
    for event in ["Stop", "SubagentStop"] {
        let raw = ws.payload(event, &ws.wt(), None, false);
        // Sanity: with a working log this very input is a block.
        assert!(is_block(&ws.run(&raw)), "{event}: fixture must block when recorded");

        let out = run(&raw, "main", ws.main(), Some(&bad_log), "inv");
        assert!(!is_block(&out), "{event}: an unrecorded decision must not block");
        let out = out.expect("the gap must be visible, not silent");
        assert!(out.get("decision").is_none(), "{event}: no decision field: {out}");
        let msg = out["systemMessage"].as_str().unwrap();
        assert!(msg.contains("NOT recorded"), "{msg}");

        // No log location at all: same contract.
        let out = run(&raw, "main", ws.main(), None, "inv");
        assert!(!is_block(&out), "{event}: no log location must not block");
        assert!(out.unwrap()["systemMessage"]
            .as_str()
            .unwrap()
            .contains("NOT recorded"));
    }
}

#[test]
fn a_log_override_inside_any_checkout_is_rejected_fails_open_and_surfaces_the_gap() {
    let ws = Ws::opted_in();
    std::fs::write(ws.wt().join("probe.sh"), "#!/bin/sh\n").unwrap();
    let inside_main = ws.main().join("logs/canary.jsonl");
    let inside_wt = ws.wt().join("canary.jsonl");
    let dotdot = ws
        .logs
        .path()
        .join("../../../../../../../../..")
        .join(inside_main.strip_prefix("/").unwrap());
    for event in ["Stop", "SubagentStop"] {
        let raw = ws.payload(event, &ws.wt(), None, false);
        for bad in [&inside_main, &inside_wt, &dotdot] {
            let out = run(&raw, "main", ws.main(), Some(bad), "inv");
            assert!(!is_block(&out), "{event}: {}: must not block", bad.display());
            let out = out.expect("the gap must be visible, not silent");
            let msg = out["systemMessage"].as_str().unwrap();
            assert!(
                msg.contains("NOT recorded") && msg.contains("inside the Git checkout"),
                "{msg}"
            );
            assert!(!bad.exists(), "{event}: nothing may be written inside a checkout");
        }
        assert!(!ws.main().join("logs").exists(), "no directory created in the checkout");
    }
}

#[test]
fn the_outcome_log_is_bounded_by_rotation() {
    let ws = Ws::opted_in();
    let log = ws.log();
    let oversized = vec![b'x'; usize::try_from(OUTCOME_LOG_MAX_BYTES).unwrap() + 1];
    std::fs::write(&log, oversized).unwrap();
    let rec = serde_json::json!({"outcome": "allow"});
    append_record(&log, &rec).unwrap();
    let rotated = PathBuf::from(format!("{}.1", log.display()));
    assert!(rotated.exists(), "the oversized log was rotated aside");
    assert!(std::fs::metadata(&log).unwrap().len() < 1024, "live log restarted");
    for _ in 0..(OUTCOME_LOG_KEEP + 3) {
        std::fs::write(&log, vec![b'x'; usize::try_from(OUTCOME_LOG_MAX_BYTES).unwrap()]).unwrap();
        append_record(&log, &rec).unwrap();
    }
    let beyond = PathBuf::from(format!("{}.{}", log.display(), OUTCOME_LOG_KEEP + 1));
    assert!(!beyond.exists(), "no generation beyond OUTCOME_LOG_KEEP is kept");
}

#[test]
fn outcome_names_cover_every_decision() {
    assert_eq!(outcome_name(&Decision::Silent), "allow");
    assert_eq!(outcome_name(&Decision::Advise(String::new())), "advisory");
    assert_eq!(outcome_name(&Decision::Block(String::new())), "block");
}
