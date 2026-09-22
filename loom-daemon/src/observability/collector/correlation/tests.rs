use super::*;

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
    );
    assert!(envelopes[0].trace_context.is_none());
}
