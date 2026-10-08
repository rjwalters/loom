use super::*;
use chrono::TimeZone;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 7, 17, 15, 0).unwrap()
}

fn sample() -> PauseManifest {
    PauseManifest {
        schema_version: SCHEMA_VERSION,
        manifest_id: "rp-20261007T171500Z-4f2a".into(),
        phase: Phase::Pausing,
        written_by: WrittenBy {
            version: "0.19.876".into(),
            ..WrittenBy::default()
        },
        roll: Roll {
            from_version: Some("0.19.876".into()),
            to_version: "0.19.877".into(),
            to_artifact_sha256: Some("abc".into()),
            target_source: Some(TargetSource::Floor),
            staged_at: None,
            pause_started_at: t0(),
            pause_completed_at: None,
            pause_budget_secs: Some(120),
            min_resumable_age_secs: Some(300),
            max_age_secs: 900,
        },
        items: vec![ManifestItem {
            id: "sweep-issue-10714-x".into(),
            kind: ItemKind::Sweep,
            repo: "/r".into(),
            disposition: Disposition::Resume,
            status: ItemStatus::Planned,
            reason: None,
            issue: Some(10714),
            pr: None,
            pid: Some(51234),
            pid_started_at: Some("p".into()),
            pgid: Some(51234),
            scope_unit: Some("loom-agent-1.scope".into()),
            agent_started_at: Some(t0()),
            run_started_at: Some(t0()),
            resume_handle: Some(ResumeHandle {
                runtime: Runtime::Claude,
                session_id: Some("4b1d0000-0000-0000-0000-000000000000".into()),
                session_store: Some("~/.claude/projects/x".into()),
                account: Some("acct-3".into()),
                model: Some("opus".into()),
                effort: Some("high".into()),
                cwd: Some("/r/.loom/worktrees/issue-10714".into()),
                container: None,
                sandbox: None,
                resume_count: 0,
                resume_of: None,
                lease_sweep_id: None,
            }),
            safe_point: None,
            checkpoint_phase: Some("builder".into()),
            worktree: Some(WorktreeRecord {
                path: "/w".into(),
                branch: None,
                head: None,
                dirty: None,
            }),
            claim: Some(serde_json::json!({"label": "loom:building", "on": "issue"})),
            lease_comment_id: Some(5_822_662_982),
            lease_refreshed_at: None,
            log_path: None,
            overflow: false,
            role: None,
            timeout_remaining_secs: None,
            holds_issue_creation_mutex: false,
            stopped_at: None,
        }],
        events: vec![],
    }
}

#[test]
fn a_manifest_round_trips_through_save_and_load() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(MANIFEST_FILE);
    save(&path, &sample()).unwrap();
    assert_eq!(load(&path, t0()), LoadOutcome::Loaded(sample()));
}

#[test]
fn unknown_fields_are_ignored() {
    let mut v = serde_json::to_value(sample()).unwrap();
    v["future_top"] = serde_json::json!({"x": 1});
    v["roll"]["future_roll"] = serde_json::json!(true);
    v["items"][0]["future_item"] = serde_json::json!([1, 2]);
    v["items"][0]["resume_handle"]["future_handle"] = serde_json::json!("y");
    assert_eq!(parse(&v.to_string(), t0()), LoadOutcome::Loaded(sample()));
}

#[test]
fn unknown_enum_values_become_requeue_with_a_named_reason() {
    for (field, path, expect) in [
        ("kind", vec!["kind"], "unknown-kind-epic"),
        ("disposition", vec!["disposition"], "unknown-disposition-epic"),
        ("status", vec!["status"], "unknown-status-epic"),
        ("runtime", vec!["resume_handle", "runtime"], "unknown-runtime-epic"),
    ] {
        let mut v = serde_json::to_value(sample()).unwrap();
        let mut slot = &mut v["items"][0];
        for p in &path {
            slot = &mut slot[*p];
        }
        *slot = serde_json::json!("epic");
        let LoadOutcome::Loaded(m) = parse(&v.to_string(), t0()) else {
            panic!("{field}: an unknown value must still parse");
        };
        assert_eq!(m.items.len(), 1, "{field}: the item is never dropped");
        assert_eq!(
            m.items[0].effective_disposition(),
            (Disposition::Requeue, Some(expect.to_string())),
            "{field}"
        );
        // And the unknown value survives a re-save verbatim.
        let again = serde_json::to_value(&m).unwrap();
        assert!(again.to_string().contains("\"epic\""), "{field}");
    }
    let known = &sample().items[0];
    assert_eq!(known.effective_disposition(), (Disposition::Resume, None));
}

#[test]
fn a_newer_schema_version_is_unknown_version_even_if_its_shape_changed() {
    let raw = r#"{"schema_version": 2, "entirely": "different"}"#;
    assert_eq!(parse(raw, t0()), LoadOutcome::UnknownVersion(2));
}

#[test]
fn corrupt_missing_and_stale_manifests_are_typed() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(load(&tmp.path().join("absent.json"), t0()), LoadOutcome::Missing);
    assert!(matches!(parse("{not json", t0()), LoadOutcome::Corrupt(_)));
    assert!(matches!(parse(r#"{"manifest_id":"x"}"#, t0()), LoadOutcome::Corrupt(_)));
    assert!(matches!(parse(r#"{"schema_version":1}"#, t0()), LoadOutcome::Corrupt(_)));

    let raw = serde_json::to_string(&sample()).unwrap();
    let at_limit = t0() + chrono::Duration::seconds(900);
    assert!(matches!(parse(&raw, at_limit), LoadOutcome::Loaded(_)));
    let past = t0() + chrono::Duration::seconds(901);
    assert_eq!(parse(&raw, past), LoadOutcome::Stale(sample()));
}

#[test]
fn an_atomic_save_leaves_no_partial_or_temp_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(MANIFEST_FILE);
    save(&path, &sample()).unwrap();
    let mut changed = sample();
    changed.phase = Phase::Paused;
    save(&path, &changed).unwrap();
    let names: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![MANIFEST_FILE.to_string()], "no temp file left behind");
    assert_eq!(load(&path, t0()), LoadOutcome::Loaded(changed));
    // A save into a directory that cannot be created fails, leaving nothing.
    let blocker = tmp.path().join("file");
    std::fs::write(&blocker, "x").unwrap();
    assert!(save(&blocker.join("m.json"), &sample()).is_err());
}

#[test]
fn every_frozen_v1_core_field_is_present_in_a_written_manifest() {
    let v = serde_json::to_value(sample()).unwrap();
    for path in FROZEN_V1_CORE {
        let mut node = &v;
        for part in path.split('.') {
            node = match part.strip_suffix("[]") {
                Some(arr) => &node[arr][0],
                None => &node[part],
            };
        }
        assert!(
            node.is_null() == (path.ends_with("reason") || path.ends_with("container")),
            "frozen field {path} must be written (got {node})"
        );
        let key = path.rsplit('.').next().unwrap();
        assert!(v.to_string().contains(&format!("\"{key}\"")), "{path}");
    }
}
