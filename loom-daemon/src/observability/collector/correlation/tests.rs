use super::*;

/// The overwhelmingly common case: this host's registry holds no adoption
/// evidence for the issue, so correlation is exactly what it was before
/// Issue #8720 — the in-memory dispatch map or the synthesized fallback.
fn no_evidence() -> Option<TrackedSweepIdentity> {
    None
}

fn dispatch(issue: u32, sweep: &str) -> Event {
    Event::SweepGlobalDispatch {
        sweep_id: sweep.into(),
        kind: SweepKind::Issue(issue),
        runtime: None,
        runtime_source: None,
        repo: None,
    }
}
fn phase(issue: u32) -> Event {
    Event::SweepPhase {
        issue,
        phase: "judge".into(),
        pr_number: None,
        repo: None,
    }
}
fn terminal(issue: u32) -> Event {
    Event::SweepCrashed {
        issue,
        checkpoint_phase: None,
        classification: None,
        death_class: None,
        repo: None,
    }
}

#[test]
fn same_issue_number_in_different_repositories_retains_independent_dispatches() {
    let mut state = HashMap::new();
    for (repo, sweep) in [
        ("synthetic/alpha", "alpha-run"),
        ("synthetic/beta", "beta-run"),
    ] {
        map_event_to_records(&dispatch(18, sweep), 18, repo, RepoVisibility::Private, &mut state);
    }
    for (repo, sweep) in [
        ("synthetic/alpha", "alpha-run"),
        ("synthetic/beta", "beta-run"),
    ] {
        let records =
            map_event_to_records(&phase(18), 18, repo, RepoVisibility::Private, &mut state);
        let TelemetryRecord::SweepPhase(record) = &records[0] else {
            panic!("missing phase")
        };
        assert_eq!(record.sweep_id, sweep);
        assert_eq!(record.repo, repo);
        let records =
            map_event_to_records(&terminal(18), 18, repo, RepoVisibility::Private, &mut state);
        let TelemetryRecord::SweepCompleted(record) = &records[0] else {
            panic!("missing terminal")
        };
        assert_eq!(record.sweep_id, sweep);
    }
    assert!(state.is_empty());
}

#[cfg(feature = "otlp")]
struct TraceEnvironment(Vec<(&'static str, Option<std::ffi::OsString>)>);
#[cfg(feature = "otlp")]
impl TraceEnvironment {
    fn isolate() -> Self {
        let saved = [
            crate::observability::ENABLED_ENV,
            crate::observability::ENDPOINT_ENV,
            crate::observability::EXPORTER_ENV,
        ]
        .into_iter()
        .map(|key| {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            (key, previous)
        })
        .collect();
        Self(saved)
    }
}
#[cfg(feature = "otlp")]
impl Drop for TraceEnvironment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(feature = "otlp")]
fn configured_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".loom")).unwrap();
    std::fs::write(dir.path().join(".loom/config.json"), r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#).unwrap();
    assert!(crate::observability::tracing::enabled(dir.path()));
    dir
}

#[cfg(feature = "otlp")]
#[test]
#[serial_test::serial]
fn live_logs_keep_correct_context_after_durable_execution_state_is_retired() {
    let _environment = TraceEnvironment::isolate();
    let mut state = HashMap::new();
    let roots = [configured_workspace(), configured_workspace()];
    let mut saved = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let repo = format!("synthetic/repo-{index}");
        let store = TraceStore::new(root.path());
        let context = store
            .load_or_create(root.path(), "same-execution-label")
            .unwrap()
            .context;
        let envelopes = map_envelopes(
            &dispatch(18, "same-execution-label"),
            18,
            &repo,
            RepoVisibility::Private,
            root.path(),
            "host",
            &mut state,
            &no_evidence,
        );
        assert_eq!(envelopes[0].trace_context.as_ref(), Some(&context));
        store.complete(root.path(), "same-execution-label").unwrap();
        saved.push(context);
    }
    assert_ne!(saved[0].trace_id, saved[1].trace_id);
    for (index, root) in roots.iter().enumerate() {
        let repo = format!("synthetic/repo-{index}");
        for event in [phase(18), terminal(18)] {
            let envelopes = map_envelopes(
                &event,
                18,
                &repo,
                RepoVisibility::Private,
                root.path(),
                "host",
                &mut state,
                &no_evidence,
            );
            assert!(!envelopes.is_empty());
            assert!(envelopes
                .iter()
                .all(|e| e.trace_context.as_ref() == Some(&saved[index])));
        }
    }
    assert!(state.is_empty());
}

#[cfg(feature = "otlp")]
#[test]
#[serial_test::serial]
fn restart_without_dispatch_does_not_guess_trace_identity_from_issue_number() {
    let _environment = TraceEnvironment::isolate();
    let root = configured_workspace();
    TraceStore::new(root.path())
        .load_or_create(root.path(), "unrelated-execution")
        .unwrap();
    let envelopes = map_envelopes(
        &phase(18),
        18,
        "synthetic/alpha",
        RepoVisibility::Private,
        root.path(),
        "host",
        &mut HashMap::new(),
        &no_evidence,
    );
    assert!(envelopes[0].trace_context.is_none());
}

// ---------------------------------------------------------------------------
// Issue #8720 — lifecycle correlation for a sweep ADOPTED across a daemon
// restart. These drive the real `sweep_registry` adoption paths (lock-based
// and journal-only) rather than a hand-built identity, so the id asserted here
// is the one production adoption actually produces.
// ---------------------------------------------------------------------------

/// Every lifecycle sweep id carried by `envelopes`, in emission order.
fn sweep_ids(envelopes: &[TelemetryEnvelope]) -> Vec<String> {
    envelopes
        .iter()
        .filter_map(|envelope| match &envelope.record {
            TelemetryRecord::SweepStarted(r) => Some(r.sweep_id.clone()),
            TelemetryRecord::SweepPhase(r) => Some(r.sweep_id.clone()),
            TelemetryRecord::SweepCompleted(r) => Some(r.sweep_id.clone()),
            TelemetryRecord::SweepOutcome(r) => Some(r.sweep_id.clone()),
            _ => None,
        })
        .collect()
}

fn outcome_duration_sec(envelopes: &[TelemetryEnvelope]) -> i64 {
    envelopes
        .iter()
        .find_map(|envelope| match &envelope.record {
            TelemetryRecord::SweepOutcome(r) => Some(r.total_duration_sec),
            _ => None,
        })
        .expect("a terminal event yields a sweep.outcome record")
}

/// Reproduction step 1: a registry entry created by **lock-based adoption**,
/// preserving the pre-restart sweep id, with the surviving per-sweep log
/// carrying that exact dispatch header.
fn lock_adopted_registry(
    workspace: &Path,
    issue: u32,
    sweep_id: &str,
    acquired_at: DateTime<Utc>,
) -> crate::sweep_registry::SweepRegistry {
    let (mut registry, _record_log) =
        crate::sweep_registry::test_support::fixture_registry(workspace);
    let lock = registry.config().locks_dir().join(format!("issue-{issue}"));
    std::fs::create_dir_all(&lock).unwrap();
    let owner = crate::sweep_registry::LockOwner {
        issue,
        // Alive by construction, so the lock pass adopts rather than reaps it.
        owner_pid: std::process::id(),
        acquired_at: acquired_at.to_rfc3339(),
        sweep_id: sweep_id.to_string(),
        pgid: None,
        model: None,
        effort: None,
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string(&owner).unwrap()).unwrap();
    let log_path = registry.compute_log_path(issue);
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    std::fs::write(
        &log_path,
        format!("==== loom-daemon dispatch: now sweep_id={sweep_id} issue={issue} ====\n"),
    )
    .unwrap();
    assert!(registry.reconstruct().unwrap() >= 1, "the live lock must be adopted");
    assert_eq!(
        registry
            .get(sweep_id)
            .map(|info| info.started_at.timestamp()),
        Some(acquired_at.timestamp()),
        "adoption must keep the lock's own acquired_at, not stamp a new start"
    );
    registry
}

#[test]
fn a_lock_adopted_sweep_correlates_its_later_events_to_the_original_sweep_id() {
    let dir = tempfile::tempdir().unwrap();
    let sweep_id = "sweep-issue-8720-1790000000";
    // The sweep has been running for 15 minutes; the daemon that dispatched it
    // is gone.
    let acquired_at = Utc::now() - chrono::Duration::seconds(900);
    let registry = lock_adopted_registry(dir.path(), 8720, sweep_id, acquired_at);
    let evidence = || registry.tracked_sweep_identity(8720);

    // Reproduction step 2: collector correlation starts EMPTY — no
    // `sweep.global.dispatch` was ever observed by this process.
    let mut state = HashMap::new();
    let envelopes = map_envelopes(
        &phase(8720),
        8720,
        "rjwalters/loom",
        RepoVisibility::Private,
        dir.path(),
        "host-a",
        &mut state,
        &evidence,
    );
    // Reproduction step 3: the phase resolves to the adopted registry entry's
    // own id, not `unknown-issue-8720`.
    assert_eq!(sweep_ids(&envelopes), vec![sweep_id.to_string()]);

    // ...and so does the eventual terminal event, whose elapsed time is
    // measured from the registry's real `acquired_at` rather than reset.
    let envelopes = map_envelopes(
        &terminal(8720),
        8720,
        "rjwalters/loom",
        RepoVisibility::Private,
        dir.path(),
        "host-a",
        &mut state,
        &evidence,
    );
    assert_eq!(sweep_ids(&envelopes), vec![sweep_id.to_string(); 2]);
    assert!(
        outcome_duration_sec(&envelopes) >= 900,
        "an adopted sweep's terminal record must measure from the adopted start, not zero"
    );
    assert!(state.is_empty(), "a terminal event still clears correlation state");
}

/// No `sweep.started` record is ever synthesized for an adopted sweep: the row
/// it would create/resurrect already exists upstream with the true start
/// instant, and re-announcing it here would stamp a fabricated one.
#[test]
fn adopting_a_sweep_never_replays_a_start_record() {
    let dir = tempfile::tempdir().unwrap();
    let registry = lock_adopted_registry(dir.path(), 8721, "sweep-issue-8721-adopted", Utc::now());
    let evidence = || registry.tracked_sweep_identity(8721);
    let envelopes = map_envelopes(
        &phase(8721),
        8721,
        "rjwalters/loom",
        RepoVisibility::Private,
        dir.path(),
        "host-a",
        &mut HashMap::new(),
        &evidence,
    );
    assert!(
        envelopes
            .iter()
            .all(|e| !matches!(e.record, TelemetryRecord::SweepStarted(_))),
        "correlating an adopted sweep must not emit a sweep.started replay"
    );
}

/// Journal-only recovery (Issue #6262) is covered **separately** from the lock
/// path because the original dispatch id is genuinely unrecoverable there: the
/// lock did not survive, so the journal can only supply its own
/// `journal-adopted-…` id. That is still strictly better than
/// `unknown-issue-N` — it is the same id this daemon reports for that sweep in
/// `host.health`'s `active_sweep_ids` and its `sweep.identity` record, so the
/// dashboard converges on one row instead of two.
#[test]
fn a_journal_adopted_sweep_correlates_to_the_id_the_registry_reports_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mut registry, _record_log) =
        crate::sweep_registry::test_support::fixture_registry(dir.path());
    let pid = std::process::id();
    let entry = crate::sweep_journal::JournalEntry {
        issue: 8722,
        pid,
        repo: registry.config().workspace_root.display().to_string(),
        started_at: Utc::now() - chrono::Duration::seconds(60),
    };
    assert_eq!(registry.adopt_live_journal_sweeps(&[entry]), 1);
    let expected = format!("journal-adopted-issue-8722-{pid}");
    assert_eq!(
        registry.tracked_sweep_identity(8722).map(|i| i.sweep_id),
        Some(expected.clone())
    );

    let envelopes = map_envelopes(
        &phase(8722),
        8722,
        "rjwalters/loom",
        RepoVisibility::Private,
        dir.path(),
        "host-a",
        &mut HashMap::new(),
        &|| registry.tracked_sweep_identity(8722),
    );
    assert_eq!(sweep_ids(&envelopes), vec![expected]);
}

/// The pre-#8720 fallback is retained wherever there is no authoritative
/// evidence — including the issue's own `unknown-issue-8715` example: a phase
/// event injected by hand for an issue this host is not running.
#[test]
fn an_event_with_no_registry_evidence_keeps_the_synthesized_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, _record_log) = crate::sweep_registry::test_support::fixture_registry(dir.path());
    assert!(registry.tracked_sweep_identity(8715).is_none());
    let mut state = HashMap::new();
    for event in [phase(8715), terminal(8715)] {
        let envelopes = map_envelopes(
            &event,
            8715,
            "rjwalters/loom",
            RepoVisibility::Private,
            dir.path(),
            "host-a",
            &mut state,
            &|| registry.tracked_sweep_identity(8715),
        );
        assert!(envelopes
            .iter()
            .all(|e| sweep_ids(std::slice::from_ref(e)) == vec!["unknown-issue-8715".to_string()]));
    }
}

/// A live dispatch observed by THIS process always wins: registry evidence is
/// consulted only when the in-memory correlation map has nothing, so a running
/// sweep's own id can never be overwritten by a registry read.
#[test]
fn an_observed_dispatch_is_never_overridden_by_registry_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = HashMap::new();
    map_envelopes(
        &dispatch(8724, "live-dispatch"),
        8724,
        "rjwalters/loom",
        RepoVisibility::Private,
        dir.path(),
        "host-a",
        &mut state,
        &no_evidence,
    );
    let envelopes = map_envelopes(
        &phase(8724),
        8724,
        "rjwalters/loom",
        RepoVisibility::Private,
        dir.path(),
        "host-a",
        &mut state,
        &|| {
            panic!("evidence must not be consulted while a dispatch is tracked");
        },
    );
    assert_eq!(sweep_ids(&envelopes), vec!["live-dispatch".to_string()]);
}
