use super::*;

fn invoke(root: &Path, args: &[&str]) -> Result<String> {
    let mut out = Vec::new();
    execute(root, &args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(), &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

#[test]
fn legacy_missing_and_optional_field_contract() {
    let dir = tempfile::tempdir().unwrap();
    for field in ["phase", "attempt", "model"] {
        assert_eq!(invoke(dir.path(), &[field, "42"]).unwrap(), "");
    }
    for field in ["read", "exists"] {
        assert_eq!(invoke(dir.path(), &[field, "42"]).unwrap_err().0, 1);
    }
    invoke(dir.path(), &["write", "42", "builder-done"]).unwrap();
    let record: Value =
        serde_json::from_str(&invoke(dir.path(), &["read", "42"]).unwrap()).unwrap();
    assert!(record.get("attempt").is_none());
    assert!(record.get("model").is_none());
    assert!(record["pr_number"].is_null());
    assert_eq!(invoke(dir.path(), &["phase", "42"]).unwrap(), "builder-done\n");
    invoke(dir.path(), &["delete", "42"]).unwrap();
    invoke(dir.path(), &["delete", "42"]).unwrap();
}

#[test]
fn rejection_retains_pr_and_repeated_repair_attempts() {
    let dir = tempfile::tempdir().unwrap();
    for attempt in ["2", "3", "6"] {
        invoke(
            dir.path(),
            &[
                "write",
                "42",
                "judge-rejected",
                "--pr-number",
                "43",
                "--attempt",
                attempt,
            ],
        )
        .unwrap();
        assert_eq!(invoke(dir.path(), &["attempt", "42"]).unwrap(), format!("{attempt}\n"));
    }
    let before = invoke(dir.path(), &["read", "42"]).unwrap();
    assert_eq!(
        invoke(dir.path(), &["write", "42", "judge-rejected"])
            .unwrap_err()
            .0,
        1
    );
    assert_eq!(invoke(dir.path(), &["read", "42"]).unwrap(), before);
    assert_eq!(
        invoke(dir.path(), &["write", "42", "not-a-phase"])
            .unwrap_err()
            .0,
        2
    );
}

#[test]
fn serializes_freeform_task_ids_and_validates_numbers_without_corrupting_existing_state() {
    let dir = tempfile::tempdir().unwrap();
    let task = "run-\"quoted\"\\escaped\n";
    invoke(
        dir.path(),
        &[
            "write",
            "42",
            "doctor-done",
            "--task-id",
            task,
            "--model",
            "glm-5.3-flash",
        ],
    )
    .unwrap();
    let text = invoke(dir.path(), &["read", "42"]).unwrap();
    let record: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(record["task_id"], task);
    for args in [
        vec!["write", "../escape", "doctor-done"],
        vec!["write", "42", "doctor-done", "--attempt", "0"],
        vec!["write", "42", "doctor-done", "--attempt", "-1"],
        vec!["write", "42", "doctor-done", "--pr-number", "4294967296"],
        vec!["write", "42", "doctor-done", "--model", "bad\"model"],
        vec!["write", "42", "doctor-done", "--task-id"],
    ] {
        assert_eq!(invoke(dir.path(), &args).unwrap_err().0, 1);
    }
    assert_eq!(invoke(dir.path(), &["read", "42"]).unwrap(), text);
}

#[test]
fn jev_patches_an_existing_checkpoint_without_disturbing_other_fields() {
    let dir = tempfile::tempdir().unwrap();
    invoke(dir.path(), &["write", "42", "curator-done", "--task-id", "run-1"]).unwrap();
    invoke(dir.path(), &["jev", "42", "routine", "0.62"]).unwrap();
    let record: Value =
        serde_json::from_str(&invoke(dir.path(), &["read", "42"]).unwrap()).unwrap();
    assert_eq!(record["jev_tier"], "routine");
    assert_eq!(record["jev_confidence"], 0.62);
    // Untouched.
    assert_eq!(record["phase"], "curator-done");
    assert_eq!(record["task_id"], "run-1");
}

#[test]
fn jev_on_a_missing_checkpoint_is_a_silent_no_op() {
    let dir = tempfile::tempdir().unwrap();
    invoke(dir.path(), &["jev", "42", "routine", "0.62"]).unwrap();
    assert_eq!(invoke(dir.path(), &["exists", "42"]).unwrap_err().0, 1);
}

#[test]
fn jev_rejects_an_out_of_vocabulary_tier_or_out_of_range_confidence() {
    let dir = tempfile::tempdir().unwrap();
    invoke(dir.path(), &["write", "42", "curator-done"]).unwrap();
    for args in [
        vec!["jev", "42", "trivial", "0.5"],
        vec!["jev", "42", "routine", "1.5"],
        vec!["jev", "42", "routine", "not-a-number"],
        vec!["jev", "42", "routine"],
    ] {
        assert_eq!(invoke(dir.path(), &args).unwrap_err().0, 1);
    }
    // The checkpoint is unchanged by any of the rejected attempts.
    let record: Value =
        serde_json::from_str(&invoke(dir.path(), &["read", "42"]).unwrap()).unwrap();
    assert!(record.get("jev_tier").is_none());
}

#[test]
fn concurrent_atomic_writers_leave_complete_checkpoint_without_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    std::thread::scope(|scope| {
        for attempt in 1..9 {
            let root = dir.path();
            scope.spawn(move || {
                invoke(
                    root,
                    &[
                        "write",
                        "42",
                        "doctor-done",
                        "--attempt",
                        &attempt.to_string(),
                    ],
                )
                .unwrap();
            });
        }
    });
    let record: Value =
        serde_json::from_str(&invoke(dir.path(), &["read", "42"]).unwrap()).unwrap();
    assert!((1..9).contains(&record["attempt"].as_u64().unwrap()));
    assert_eq!(
        std::fs::read_dir(dir.path().join(".loom/sweep-checkpoint"))
            .unwrap()
            .count(),
        1
    );
}

/// #9935: `begin` is telemetry only. Without an inherited trace it is a
/// silent no-op, and it never creates, reads, or alters the checkpoint file
/// that drives resume.
#[test]
fn begin_is_telemetry_only_and_never_touches_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(invoke(dir.path(), &["begin", "42", "builder"]).unwrap(), "");
    assert_eq!(invoke(dir.path(), &["exists", "42"]).unwrap_err().0, 1);
    invoke(dir.path(), &["write", "42", "curator-done", "--task-id", "run-1"]).unwrap();
    let before = invoke(dir.path(), &["read", "42"]).unwrap();
    for role in ["curator", "builder", "judge", "doctor", "merge"] {
        invoke(dir.path(), &["begin", "42", role, "--attempt", "2", "--model", "opus"]).unwrap();
    }
    assert_eq!(invoke(dir.path(), &["read", "42"]).unwrap(), before);
    assert_eq!(invoke(dir.path(), &["phase", "42"]).unwrap(), "curator-done\n");
    assert_eq!(
        invoke(dir.path(), &["begin", "42", "builder-done"])
            .unwrap_err()
            .0,
        2
    );
    assert_eq!(invoke(dir.path(), &["begin", "42"]).unwrap_err().0, 2);
    for bad in [
        &["begin", "42", "builder", "--attempt", "0"][..],
        &["begin", "42", "builder", "--model", "a b"],
        &["begin", "42", "builder", "--task-id", "x"],
        &["begin", "42", "builder", "--attempt"],
        &["begin", "x", "builder"],
    ] {
        assert_eq!(invoke(dir.path(), bad).unwrap_err().0, 1, "{bad:?}");
    }
}

// ---- #11359: GLM Doctor rejection state --------------------------------

fn set_k(root: &Path, body: &str) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(root.join(".loom/config.json"), body).unwrap();
}

fn rejected(root: &Path) {
    invoke(root, &["write", "42", "judge-rejected", "--pr-number", "43"]).unwrap();
}

fn doctor(root: &Path, head: &str, runtime: &str, profile: &str) -> String {
    invoke(
        root,
        &[
            "doctor-attempt",
            "42",
            "--pr",
            "43",
            "--head",
            head,
            "--runtime",
            runtime,
            "--profile",
            profile,
        ],
    )
    .unwrap()
}

fn reject(root: &Path, head: &str) -> String {
    invoke(root, &["doctor-rejection", "42", "--pr", "43", "--head", head]).unwrap()
}

fn route(root: &Path, attempt: &str, cap: &str) -> String {
    invoke(
        root,
        &[
            "doctor-route",
            "42",
            "--pr",
            "43",
            "--attempt",
            attempt,
            "--cap",
            cap,
        ],
    )
    .unwrap()
}

#[test]
fn default_k_two_escalates_on_the_second_glm_rejection_and_survives_writes() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    rejected(r);
    assert_eq!(route(r, "2", "3"), "route=preference\n");
    doctor(r, "aaa1", "opencode", "glm-5.3");
    assert!(reject(r, "aaa1").starts_with("counted count=1 k=2 escalated=false"));
    // Restart at K-1: replaying the same verdict does not double count.
    assert!(reject(r, "aaa1").starts_with("replay count=1"));
    assert_eq!(route(r, "3", "3"), "route=preference\n");
    // The multi-cycle re-rejection write must retain the state.
    invoke(
        r,
        &[
            "write",
            "42",
            "judge-rejected",
            "--pr-number",
            "43",
            "--attempt",
            "3",
        ],
    )
    .unwrap();
    doctor(r, "bbb2", "opencode", "glm-5.3");
    assert!(reject(r, "bbb2").starts_with("counted count=2 k=2 escalated=true"));
    // Restart at K: same next tap, no duplicate counting.
    assert!(reject(r, "bbb2").starts_with("replay count=2"));
    invoke(
        r,
        &[
            "write",
            "42",
            "judge-rejected",
            "--pr-number",
            "43",
            "--attempt",
            "4",
        ],
    )
    .unwrap();
    assert!(route(r, "4", "3").starts_with("route=claude reason=2 Judge rejection(s)"));
    // The cap stays authoritative.
    assert_eq!(route(r, "4", "2"), "route=cap-exhausted\n");
}

#[test]
fn k_one_and_larger_k_come_from_config() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    set_k(r, r#"{"sweep":{"glmDoctorRejectionsBeforeEscalation":1}}"#);
    rejected(r);
    doctor(r, "a1", "opencode", "glm-5.3");
    assert!(reject(r, "a1").contains("escalated=true"));
    set_k(r, r#"{"sweep":{"glmDoctorRejectionsBeforeEscalation":3}}"#);
    let d2 = tempfile::tempdir().unwrap();
    let r = d2.path();
    set_k(r, r#"{"sweep":{"glmDoctorRejectionsBeforeEscalation":3}}"#);
    rejected(r);
    for (i, head) in ["a1", "a2", "a3"].iter().enumerate() {
        doctor(r, head, "opencode", "glm-5.3");
        let line = reject(r, head);
        assert_eq!(line.contains("escalated=true"), i == 2, "{line}");
    }
}

#[test]
fn invalid_k_is_a_config_error_and_does_not_count() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    set_k(r, r#"{"sweep":{"glmDoctorRejectionsBeforeEscalation":0}}"#);
    rejected(r);
    doctor(r, "a1", "opencode", "glm-5.3");
    let err = invoke(r, &["doctor-rejection", "42", "--pr", "43", "--head", "a1"]).unwrap_err();
    assert_eq!(err.0, 78);
    assert!(err.1.contains("sweep.glmDoctorRejectionsBeforeEscalation"));
    let rec: Value = serde_json::from_str(&invoke(r, &["read", "42"]).unwrap()).unwrap();
    assert_eq!(rec["doctor_escalation"]["rejections"], 0);
}

#[test]
fn non_qualifying_failures_never_advance_the_counter() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    set_k(r, r#"{"sweep":{"glmDoctorRejectionsBeforeEscalation":1}}"#);
    rejected(r);
    // Initial Builder rejection, no Doctor yet.
    assert!(reject(r, "b0").starts_with("not-counted"));
    // Mixed: a Claude attempt's rejection, and a stale verdict for a GLM head.
    doctor(r, "c1", "claude", "");
    assert!(reject(r, "c1").contains("attempt-not-on-glm"));
    doctor(r, "d1", "opencode", "glm-5.3");
    assert!(reject(r, "5ca1e9").contains("stale-or-unrelated-head"));
    // Launch failure / no seat / conflict-only maintenance record no attempt at all.
    assert_eq!(route(r, "2", "1"), "route=preference\n");
}

#[test]
fn legacy_checkpoint_means_zero_and_fresh_sweep_resets() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    set_k(r, r#"{"sweep":{"glmDoctorRejectionsBeforeEscalation":1}}"#);
    rejected(r); // no doctor_escalation key: legacy
    assert_eq!(route(r, "2", "1"), "route=preference\n");
    doctor(r, "a1", "opencode", "glm-5.3");
    reject(r, "a1");
    assert!(route(r, "2", "1").starts_with("route=claude"));
    // A new lineage (Builder completion) resets deliberately.
    invoke(r, &["write", "42", "builder-done", "--pr-number", "43"]).unwrap();
    assert_eq!(route(r, "2", "1"), "route=preference\n");
    // Without a checkpoint the policy is inactive and routes by preference.
    invoke(r, &["delete", "42"]).unwrap();
    assert_eq!(route(r, "2", "1"), "route=preference\n");
    assert!(doctor(r, "a1", "opencode", "glm-5.3").starts_with("inactive"));
}

#[test]
fn restart_after_doctor_completion_before_review_keeps_attempt_identity() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    rejected(r);
    doctor(r, "a1", "opencode", "glm-5.3");
    invoke(
        r,
        &[
            "write",
            "42",
            "doctor-done",
            "--pr-number",
            "43",
            "--attempt",
            "2",
            "--model",
            "glm-5.3",
        ],
    )
    .unwrap();
    assert!(reject(r, "a1").starts_with("counted count=1"));
}
