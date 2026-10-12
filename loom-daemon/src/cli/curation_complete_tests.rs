#![allow(clippy::unwrap_used)]
use super::*;
use loom_daemon::telemetry::{TelemetryEnvelope, TelemetryRecord};
use std::os::unix::fs::PermissionsExt;

#[test]
fn cli_reads_current_issue_and_retries_without_forge_access() {
    let dir = tempfile::tempdir().unwrap();
    let gh = dir.path().join("fake-gh");
    std::fs::write(
        &gh,
        r#"#!/bin/sh
sleep 0.05
printf '%s' '{"body":"<!-- loom:estimate difficulty=2 lines=12 files=1 doctor_risk=0.1 -->"}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let args = || CurationCompleteArgs {
        repo: "fixture/repo".to_string(),
        issue: 42,
        model: "actual-curator-model".to_string(),
        runtime: "codex".to_string(),
        completion_id: id.clone(),
        workspace_root: dir.path().to_path_buf(),
    };
    let before = Utc::now();
    args().run_with(gh.as_os_str()).unwrap();
    let file = dir
        .path()
        .join(format!(".loom/logs/issue-estimates/{id}.json"));
    let original = std::fs::read(&file).unwrap();
    let saved: TelemetryEnvelope = serde_json::from_slice(&original).unwrap();
    let TelemetryRecord::IssueEstimate(r) = saved.record else {
        panic!("wrong kind")
    };
    assert_eq!(r.repo, "fixture/repo");
    assert_eq!(r.issue, 42);
    assert_eq!(r.model.as_deref(), Some("actual-curator-model"));
    assert_eq!(r.runtime.as_deref(), Some("codex"));
    assert_eq!(r.estimate.lines, 12);
    assert!(r.estimated_at >= before + chrono::Duration::milliseconds(40));
    assert!(r.estimated_at <= Utc::now());
    args()
        .run_with(dir.path().join("unavailable-gh").as_os_str())
        .unwrap();
    assert_eq!(std::fs::read(file).unwrap(), original);
}
