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
