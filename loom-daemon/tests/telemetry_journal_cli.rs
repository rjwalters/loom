//! CLI contract for `loom-daemon telemetry append|verify` (#11348), end to end
//! through the real binary on temporary workspaces.
//!
//! `append`: `0` appended or disabled (no-op), `1` journal failure, `2`
//! invalid input. `verify`: `0` clean, `1` divergence/anomaly, `2` nothing to
//! verify (disabled, or no stream yet) — never green.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use loom_daemon::journal::{Journal, Position};
use serde_json::{json, Value};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_loom-daemon");

fn command(ws: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.arg("telemetry")
        .current_dir(ws)
        .env_remove("LOOM_TELEMETRY_JOURNAL")
        .env_remove("LOOM_TELEMETRY_JOURNAL_ROOT")
        .env_remove("LOOM_GUIDE_DOCS_TELEMETRY_LOG")
        .env_remove("LOOM_MERGE_ADMISSION_TELEMETRY_LOG");
    cmd
}

fn append(ws: &Path, args: &[&str], enabled: bool, input: &[u8]) -> Output {
    let mut cmd = command(ws);
    cmd.arg("append").args(args);
    if enabled {
        cmd.env("LOOM_TELEMETRY_JOURNAL", "1");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A refused (oversize) input may close stdin early; ignore EPIPE.
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait_with_output().unwrap()
}

fn envelope(pr: u64) -> Value {
    json!({
        "schema_version": 1,
        "emitted_at": "2026-10-11T03:00:00Z",
        "emitted_at_epoch": 1_791_687_600,
        "host_id": "h",
        "record": {"kind": "merge.admission_recheck", "repo": "o/r", "pr_number": pr,
                   "action": "merge", "reason": "ok", "retries_used": null}
    })
}

fn root_of(ws: &Path) -> std::path::PathBuf {
    ws.join(".loom/state/journal/telemetry")
}

fn shell_records(ws: &Path) -> Vec<Value> {
    Journal::open(root_of(ws))
        .stream("shell")
        .unwrap()
        .reader(Position::START)
        .map(|r| r.unwrap().envelope.data)
        .collect()
}

#[test]
fn append_is_a_no_op_while_disabled() {
    let ws = TempDir::new().unwrap();
    let out = append(ws.path(), &[], false, envelope(1).to_string().as_bytes());
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("disabled"));
    assert!(!ws.path().join(".loom").exists());
}

#[test]
fn append_validates_and_bounds_its_input() {
    let ws = TempDir::new().unwrap();
    let refused = |input: &[u8], args: &[&str]| {
        let out = append(ws.path(), args, true, input);
        assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    };
    refused(b"not json", &[]);
    refused(b"[1,2]", &[]);
    refused(b"{\"record\":{\"kind\":\"x\"}}", &[]); // not an envelope
    let mut bad_time = envelope(1);
    bad_time["emitted_at"] = json!("yesterday");
    refused(bad_time.to_string().as_bytes(), &[]);
    refused(envelope(1).to_string().as_bytes(), &["--stream", "ci"]); // T7 out of scope
    refused(envelope(1).to_string().as_bytes(), &["--kind", "bad kind"]);
    refused(envelope(1).to_string().as_bytes(), &["--key", "a\nb"]);
    let mut huge = envelope(1);
    huge["record"]["reason"] = json!("x".repeat(40 * 1024));
    refused(huge.to_string().as_bytes(), &[]);
    // Non-envelope streams need a kind.
    refused(b"{\"issue\":1}", &["--stream", "experiment"]);
    assert!(!root_of(ws.path()).join("shell").exists(), "nothing refused was written");
}

#[test]
fn append_then_verify_round_trips_and_reports_divergence() {
    let ws = TempDir::new().unwrap();
    let legacy = ws.path().join(".loom/logs/merge-admission-telemetry.jsonl");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let mut log = std::fs::File::create(&legacy).unwrap();
    for pr in 1..=2 {
        writeln!(log, "{}", envelope(pr)).unwrap();
        let out = append(ws.path(), &[], true, format!("{}\n", envelope(pr)).as_bytes());
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let verify = |args: &[&str]| {
        command(ws.path())
            .arg("verify")
            .args(args)
            .env("LOOM_TELEMETRY_JOURNAL", "1")
            .output()
            .unwrap()
    };
    let out = verify(&["--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["comparisons"][0]["matched"], 2);

    writeln!(log, "{}", envelope(3)).unwrap(); // a lost shadow
    let out = verify(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("DIVERGED"));

    // Disabled, and nothing under an explicit empty root: never green.
    let out = command(ws.path()).arg("verify").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let empty = TempDir::new().unwrap();
    let out = command(ws.path())
        .args(["verify", "--root", empty.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn concurrent_append_processes_produce_a_gapless_stream() {
    let ws = TempDir::new().unwrap();
    let children: Vec<_> = (0..6u64)
        .map(|i| {
            let mut child = command(ws.path())
                .arg("append")
                .env("LOOM_TELEMETRY_JOURNAL", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(envelope(i).to_string().as_bytes())
                .unwrap();
            child
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let seqs: Vec<u64> = Journal::open(root_of(ws.path()))
        .stream("shell")
        .unwrap()
        .reader(Position::START)
        .map(|r| r.unwrap().envelope.seq)
        .collect();
    assert_eq!(seqs, (1..=6).collect::<Vec<_>>());
}

#[test]
fn the_shell_producer_shadows_through_the_native_entry_point() {
    let ws = TempDir::new().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../defaults/scripts/merge-admission-telemetry.sh");
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(ws.path())
        .status()
        .unwrap()
        .success());
    let run = |enabled: bool, pr: &str| {
        let mut cmd = Command::new("bash");
        cmd.arg(&script)
            .args([
                "record", "--pr", pr, "--action", "merge", "--reason", "ok", "--repo", "o/r",
            ])
            .current_dir(ws.path())
            .env("LOOM_DAEMON_BIN", BIN)
            .env_remove("LOOM_TELEMETRY_JOURNAL")
            .env_remove("LOOM_TELEMETRY_JOURNAL_ROOT")
            .env_remove("LOOM_MERGE_ADMISSION_TELEMETRY_LOG");
        if enabled {
            cmd.env("LOOM_TELEMETRY_JOURNAL", "1");
        }
        assert!(cmd.status().unwrap().success());
    };
    run(false, "1");
    assert!(!root_of(ws.path()).exists(), "default off");
    run(true, "2");
    let legacy: Vec<Value> =
        std::fs::read_to_string(ws.path().join(".loom/logs/merge-admission-telemetry.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    assert_eq!(legacy.len(), 2);
    assert_eq!(shell_records(ws.path()), vec![legacy[1].clone()]);
    // A broken native entry point never fails the legacy write.
    let mut cmd = Command::new("bash");
    let status = cmd
        .arg(&script)
        .args([
            "record", "--pr", "3", "--action", "merge", "--reason", "ok", "--repo", "o/r",
        ])
        .current_dir(ws.path())
        .env("LOOM_DAEMON_BIN", "/nonexistent/loom-daemon")
        .env("LOOM_TELEMETRY_JOURNAL", "1")
        .status()
        .unwrap();
    assert!(status.success());
}
