use super::*;

// ===================================================================
// Role-tick health ring (#4761)
// ===================================================================

#[test]
#[serial(role_tick_ring)]
fn recording_a_tick_makes_it_readable_cross_process() {
    reset_role_tick_ring();
    let at = chrono::Utc::now();
    record_role_tick_at("curator", Path::new("/r/loom"), &RoleTickOutcome::Success, at);

    let records = role_tick_records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].role, "curator");
    assert_eq!(records[0].root, PathBuf::from("/r/loom"));
    assert_eq!(records[0].at, at);
    assert!(records[0].ok);
    assert_eq!(records[0].detail, None);
}

#[test]
#[serial(role_tick_ring)]
fn a_failure_records_its_reason() {
    reset_role_tick_ring();
    record_role_tick(
        "champion",
        Path::new("/r/loom"),
        &RoleTickOutcome::Failure("mcp preflight failed".to_string()),
    );
    let records = role_tick_records();
    assert!(!records[0].ok);
    assert_eq!(records[0].detail.as_deref(), Some("mcp preflight failed"));
}

/// #4642's permanent no-pool state must surface as NOT-ok — a role that
/// cannot run at all is precisely what a health check exists to report.
#[test]
#[serial(role_tick_ring)]
fn a_missing_token_pool_records_as_not_ok() {
    reset_role_tick_ring();
    record_role_tick("guide", Path::new("/r/loom"), &RoleTickOutcome::NoTokenPool);
    let records = role_tick_records();
    assert!(!records[0].ok);
    assert_eq!(records[0].detail.as_deref(), Some("no-token-pool"));
}

/// Issue #6637: unlike `NoTokenPool`/`ModelRuntimeMismatch` (permanent
/// config defects), a load-saturated tick ceiling records as OK — it is
/// a transient, self-clearing condition, not a role/machinery failure a
/// health check must surface as degraded.
#[test]
#[serial(role_tick_ring)]
fn a_load_skipped_tick_records_as_ok() {
    reset_role_tick_ring();
    record_role_tick(
        "auditor",
        Path::new("/r/loom"),
        &RoleTickOutcome::LoadSkipped {
            load_per_core: 1.8,
            detail: "cargo nextest run".to_string(),
        },
    );
    let records = role_tick_records();
    assert!(records[0].ok, "a load-skip must not be tallied as a failure");
    assert!(
        records[0]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("1.80") && d.contains("cargo nextest run")),
        "{:?}",
        records[0].detail
    );
}

#[test]
#[serial(role_tick_ring)]
fn the_ring_is_bounded_and_keeps_the_newest_entries() {
    reset_role_tick_ring();
    for _ in 0..(ROLE_TICK_RING_CAPACITY + 10) {
        record_role_tick("curator", Path::new("/r/loom"), &RoleTickOutcome::Success);
    }
    record_role_tick(
        "curator",
        Path::new("/r/loom"),
        &RoleTickOutcome::Failure("newest".to_string()),
    );
    let records = role_tick_records();
    assert_eq!(records.len(), ROLE_TICK_RING_CAPACITY);
    assert_eq!(records.last().unwrap().detail.as_deref(), Some("newest"));
}

/// The log-dedup path (#4349) downgrades a *repeat* failure to DEBUG, but
/// the health ring must still see every one of them — otherwise a
/// persistently-broken root would look quiet to a health check.
#[test]
#[serial(role_tick_ring)]
fn repeat_failures_are_all_recorded_even_though_the_log_dedups_them() {
    reset_role_tick_ring();
    let mut failing = HashMap::new();
    let mut no_pool = HashMap::new();
    let mut pool_exhausted = HashMap::new();
    let mut model_mismatch = HashMap::new();
    let root = PathBuf::from("/r/loom");
    for _ in 0..3 {
        log_outcome_for_root_deduped(
            "curator",
            &root,
            &RoleTickOutcome::Failure("boom".to_string()),
            Duration::from_secs(30),
            &mut failing,
            &mut no_pool,
            &mut pool_exhausted,
            &mut model_mismatch,
        );
    }
    let records = role_tick_records();
    assert_eq!(records.len(), 3, "every tick is recorded, not just the fail edge");
    assert!(records.iter().all(|r| !r.ok));
}

/// #14026 (Judge finding): every role-runner daemon-log lane must render a
/// `runtime_rejected` refusal as ONE line — `refusal_line()`, never the
/// multi-line operator `reason` whose `\n  preference order: …` continuation
/// is unprefixed and unfilterable. Covers both `log::` sites:
/// `log_outcome_for_root` (the fire-and-forget idle path) and
/// `log_outcome_for_root_deduped` (the multi-workspace interval loop's
/// fail-edge WARN).
#[test]
#[serial(role_tick_ring)]
fn a_runtime_rejected_tick_logs_the_one_line_refusal_not_the_multiline_reason() {
    let obligation = "owned account lease plus durable workspace preparation for model \
                      claude/sonnet on profile zai-metered is still outstanding (#13916)";
    let rejection = crate::runtime_admission::RuntimeRejection {
        role: "curator".into(),
        runtime: String::new(),
        source: crate::runtime_admission::RuntimeSource::Preference,
        unmet_capabilities: vec![],
        reason: format!(
            "No runtime in the preference list can serve role \"curator\" right now\n  \
             codex -> not-admitted(unmet capabilities: worktreeIsolation; {obligation})"
        ),
        exhausted_summary_line: Some(
            format!(
                "runtime_rejected role=curator: codex -> not-admitted(unmet capabilities: \
             worktreeIsolation; {obligation})"
            )
            .into_boxed_str(),
        ),
    };
    let outcome = RoleTickOutcome::RuntimeRejected(rejection);
    let root = PathBuf::from("/r/loom-14026");
    reset_role_tick_ring();

    let records = crate::test_log_capture::capture_logs(|| {
        log_outcome_for_root("curator", &root, &outcome, Duration::from_secs(1));
        let (mut failing, mut no_pool, mut pool_exhausted, mut model_mismatch) =
            (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
        log_outcome_for_root_deduped(
            "curator",
            &root,
            &outcome,
            Duration::from_secs(1),
            &mut failing,
            &mut no_pool,
            &mut pool_exhausted,
            &mut model_mismatch,
        );
    });

    let warn_lines: Vec<&str> = records
        .iter()
        .filter(|(l, _)| *l >= log::Level::Warn)
        .map(|(_, m)| m.as_str())
        .collect();
    assert_eq!(warn_lines.len(), 2, "one WARN per site: {records:?}");
    for line in warn_lines {
        assert!(!line.contains('\n'), "one line, never split: {line:?}");
        assert!(line.contains(obligation), "full reason, uncapped: {line:?}");
        assert!(
            line.contains("runtime_rejected role=curator"),
            "the one-line candidate rendering, not `reason`'s prose: {line:?}"
        );
    }
}

/// #5028 AC2: a `ModelRuntimeMismatch` outcome records as NOT-ok with an
/// operator-facing `detail()` string that names the broken config key —
/// exactly what `assess_roles` in `health.rs` renders verbatim into
/// `loom-daemon health`, so an operator learns the fix without reading a
/// spawn transcript.
#[test]
#[serial(role_tick_ring)]
fn a_model_runtime_mismatch_records_its_operator_facing_detail() {
    reset_role_tick_ring();
    record_role_tick("judge", Path::new("/r/loom"), &mismatch_outcome());
    let records = role_tick_records();
    assert!(!records[0].ok);
    let detail = records[0].detail.as_deref().unwrap();
    assert!(
        detail.contains("autonomous.roleRunner.roleModels.judge"),
        "detail must name the broken config key: {detail}"
    );
    assert!(detail.contains("model/runtime mismatch"), "detail: {detail}");
}
