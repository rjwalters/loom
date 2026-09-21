//! Exercise the real offline CLI without a daemon, forge, provider, or Docker.
#![allow(clippy::unwrap_used)]
use std::process::Command;

#[test]
fn offline_fixture_publishes_manifest_and_refuses_to_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("trial");
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
            .args([
                "telemetry-fixture",
                "--run-id",
                "integration-test",
                "--start-time",
                "2026-09-21T12:00:00Z",
                "--output",
            ])
            .arg(&output)
            .output()
            .unwrap()
    };
    let first = invoke();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["synthetic"], true);
    assert_eq!(manifest["expected_distinct"]["spans"], 37);
    let original = std::fs::read(output.join("envelopes.jsonl")).unwrap();
    let text = std::str::from_utf8(&original).unwrap();
    let records: Vec<loom_daemon::telemetry::TelemetryEnvelope> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 52);
    assert!(!invoke().status.success());
    assert_eq!(std::fs::read(output.join("envelopes.jsonl")).unwrap(), original);
    assert!(!dir.path().join(".loom").exists());
}

#[test]
fn live_canary_without_execute_never_reads_keys_or_creates_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("must-not-exist");
    let result = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["telemetry-live-canary", "--output"])
        .arg(&output)
        .args([
            "--endpoint",
            "http://127.0.0.1:1",
            "--key-file",
            "/does-not-exist/collector-key",
            "--guard-dir",
            "/does-not-exist/guards",
            "--zshrc",
            "/does-not-exist/zshrc",
        ])
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let plan: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(plan["executed"], false);
    assert_eq!(plan["attempts"], 2);
    assert!(!output.exists());
}
