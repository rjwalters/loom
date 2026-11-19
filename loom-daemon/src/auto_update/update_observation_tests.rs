use super::*;

fn facts(running: &str, on_disk: Option<&str>, target: Option<&str>) -> Facts {
    Facts {
        running_version: running.to_string(),
        loop_facts: LoopFacts {
            observed: true,
            on_disk_version: on_disk.map(str::to_string),
            target_version: target.map(str::to_string),
            ..LoopFacts::default()
        },
        ..Facts::default()
    }
}

#[test]
fn each_state_is_reached_from_its_facts() {
    let current = facts("0.19.10", Some("0.19.10"), None);
    let settling = facts("0.19.10", Some("0.19.10"), Some("0.19.11"));
    let staged = facts("0.19.10", Some("0.19.11"), Some("0.19.11"));
    let mut fetching = facts("0.19.10", Some("0.19.10"), Some("0.19.11"));
    fetching.loop_facts.fetching = true;
    let pausing = Facts {
        pausing: true,
        ..current.clone()
    };
    let verifying = Facts {
        verifying: true,
        ..current.clone()
    };
    let backing_off = Facts {
        backing_off: true,
        ..settling.clone()
    };
    let terminal = Facts {
        terminal: true,
        ..settling.clone()
    };
    let table = [
        (current, HostState::Current),
        (settling, HostState::Settling),
        (fetching, HostState::Fetching),
        (staged, HostState::Staged),
        (pausing, HostState::Pausing),
        (verifying, HostState::Verifying),
        (backing_off, HostState::Stalled),
        (terminal, HostState::Stalled),
    ];
    for (f, want) in table {
        assert_eq!(classify(&f), want, "{f:?}");
    }
}

#[test]
fn simultaneous_signals_follow_the_documented_precedence() {
    let mut all = facts("0.19.10", Some("0.19.11"), Some("0.19.11"));
    all.pausing = true;
    all.verifying = true;
    all.terminal = true;
    all.backing_off = true;
    all.loop_facts.fetching = true;
    assert_eq!(classify(&all), HostState::Pausing);
    all.pausing = false;
    assert_eq!(classify(&all), HostState::Verifying);
    all.verifying = false;
    assert_eq!(classify(&all), HostState::Fetching);
    all.loop_facts.fetching = false;
    assert_eq!(classify(&all), HostState::Stalled);
    all.terminal = false;
    all.backing_off = false;
    assert_eq!(classify(&all), HostState::Staged);
}

#[test]
fn an_unreadable_on_disk_version_is_unknown_never_the_running_version() {
    assert_eq!(classify(&facts("0.19.10", None, None)), HostState::Unknown);
    assert_eq!(classify(&facts("0.19.10", None, Some("0.19.11"))), HostState::Unknown);
    // A loop that has not ticked yet has no evidence either.
    assert_eq!(classify(&Facts::default()), HostState::Unknown);
}

#[test]
fn states_that_need_no_disk_evidence_stand_without_it() {
    let mut f = facts("0.19.10", None, None);
    f.terminal = true;
    assert_eq!(classify(&f), HostState::Stalled);
    f.terminal = false;
    f.pausing = true;
    assert_eq!(classify(&f), HostState::Pausing);
}

#[test]
fn an_older_binary_on_disk_is_inconsistent_evidence_not_staged() {
    assert_eq!(classify(&facts("0.19.10", Some("0.19.9"), None)), HostState::Unknown);
}

fn resume(step: &str, finished: bool) -> PauseResumeStatus {
    PauseResumeStatus {
        step: step.to_string(),
        finished_at: finished.then(chrono::Utc::now),
        ..PauseResumeStatus::default()
    }
}

#[test]
fn only_an_unfinished_h5_run_counts_as_verifying() {
    for step in ["hold", "residue", "lease", "probation", "resume", "requeue"] {
        assert!(is_verifying(&resume(step, false)), "{step}");
        assert!(!is_verifying(&resume(step, true)), "{step} finished");
    }
    // A finished manifest stays in the status as history.
    assert!(!is_verifying(&resume("done", true)));
    assert!(!is_verifying(&resume("done", false)));
    assert!(!is_verifying(&resume("unreadable", false)));
    // Waiting on an operator stop or a real drain is not H5 itself.
    assert!(!is_verifying(&resume("waiting", false)));
}

#[test]
fn observe_carries_the_loop_facts_and_the_floor() {
    let snapshot = AutoUpdateStatusSnapshot {
        observation: LoopFacts {
            observed: true,
            on_disk_version: Some("0.19.10".into()),
            floor: Some("0.19.9".into()),
            target_version: Some("0.19.11".into()),
            target_source: ObservedTargetSource::RepoAhead,
            fetching: false,
        },
        ..AutoUpdateStatusSnapshot::default()
    };
    let seen = observe(&snapshot, Some(&resume("done", true)), false, "0.19.10");
    assert_eq!(seen.state, HostState::Settling);
    assert_eq!(seen.floor.as_deref(), Some("0.19.9"));
    assert_eq!(seen.target_version.as_deref(), Some("0.19.11"));
    assert_eq!(seen.target_source.as_str(), "repo-ahead");
    let verifying = observe(&snapshot, Some(&resume("probation", false)), false, "0.19.10");
    assert_eq!(verifying.state, HostState::Verifying);
}

#[test]
fn backoff_and_terminal_reason_from_the_snapshot_stall_the_host() {
    let mut snapshot = AutoUpdateStatusSnapshot {
        observation: facts("0.19.10", Some("0.19.10"), None).loop_facts,
        backoff_secs: Some(60),
        ..AutoUpdateStatusSnapshot::default()
    };
    assert_eq!(observe(&snapshot, None, false, "0.19.10").state, HostState::Stalled);
    snapshot.backoff_secs = None;
    snapshot.terminal_reason = Some("build failed".into());
    assert_eq!(observe(&snapshot, None, false, "0.19.10").state, HostState::Stalled);
}

#[test]
fn wire_spellings_are_stable() {
    assert_eq!(serde_json::to_value(HostState::Staged).unwrap(), "H3");
    assert_eq!(serde_json::to_value(HostState::Unknown).unwrap(), "unknown");
    assert_eq!(serde_json::to_value(ObservedTargetSource::AutoUpdate).unwrap(), "autoUpdate");
    assert_eq!(serde_json::to_value(ObservedTargetSource::None).unwrap(), "none");
    assert_eq!(HostState::Fetching.as_str(), "H2");
}
