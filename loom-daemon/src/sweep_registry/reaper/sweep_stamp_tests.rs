//! #13171: the legacy issue-sweep cancel path stamps its forge calls with the
//! sweep id, and leaves no thread identity behind.
use super::*;
use crate::observability::ops::capture::capture;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

#[test]
fn cancelling_an_issue_sweep_stamps_its_claim_check_with_the_sweep_id() {
    let dir = tempdir().unwrap();
    let fake_gh = dir.path().join("fake-gh.sh");
    std::fs::write(&fake_gh, "#!/bin/sh\nprintf '2026-01-01T00:00:00Z\\n'\n").unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = true;
    let mut registry = SweepRegistry::new(config);

    let kind = SweepKind::Issue(8802);
    let started_at = Utc::now();
    let sweep_id = "sweep-issue-8802-stamp".to_string();
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid: 2_147_483_640,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(8802),
            idempotency_key: None,
            started_at,
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
            overflow: false,
        },
    );

    let (_, captured) =
        capture(|| registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true));

    let stamped: Vec<_> = captured
        .spans
        .iter()
        .filter(|s| s.attributes.get("loom.sweep_id").map(String::as_str) == Some(&sweep_id))
        .collect();
    assert!(
        !stamped.is_empty(),
        "no captured `invoke github` span carried loom.sweep_id; spans: {:?}",
        captured
            .spans
            .iter()
            .map(|s| &s.attributes)
            .collect::<Vec<_>>()
    );
    assert_eq!(crate::gh_invocation::telemetry::ambient_sweep(), (None, None));
}
