//! Tests for the non-claiming roles' live-output hooks (#10120).

use super::*;

fn attended_env() -> AttendEnv {
    AttendEnv {
        session_id: Some("755d0cb5-f9ec-464b-8d7a-c134ee5a1ca9".to_string()),
        daemon_launched: false,
    }
}

fn configured() -> Result<(), String> {
    Ok(())
}

fn never_asked() -> Result<Vec<i64>, String> {
    panic!("the forge must not be asked for a PR that cannot be attended")
}

#[test]
fn a_pr_closing_exactly_one_issue_attends_that_issue() {
    let step = decide_pr(10130, &attended_env(), configured, || Ok(vec![10120]));
    assert_eq!(step, PrStep::Attend(10120));
}

#[test]
fn a_pr_closing_no_issue_stays_unscoped() {
    let PrStep::Unscoped(why) = decide_pr(10130, &attended_env(), configured, || Ok(vec![])) else {
        panic!("a PR with no closing issue must not be attended");
    };
    assert!(why.contains("PR #10130") && why.contains("closes no issue"), "{why}");
}

#[test]
fn a_pr_closing_several_issues_is_never_guessed() {
    let step = decide_pr(10130, &attended_env(), configured, || Ok(vec![10116, 10120]));
    let PrStep::Unscoped(why) = step else {
        panic!("a PR closing two issues must not pick one");
    };
    assert!(why.contains("closes 2 issues (#10116, #10120)"), "{why}");
}

#[test]
fn an_unreadable_closing_list_stays_unscoped() {
    let step = decide_pr(10130, &attended_env(), configured, || Err("gh exited 1".into()));
    let PrStep::Unscoped(why) = step else {
        panic!("an unknown closing issue must not be attended");
    };
    assert!(why.contains("gh exited 1"), "{why}");
}

#[test]
fn unconfigured_live_output_is_silent_and_never_asks_the_forge() {
    let step = decide_pr(
        10130,
        &attended_env(),
        || Err("observability is not enabled".into()),
        never_asked,
    );
    assert_eq!(step, PrStep::Quiet);
    let not_configured = Outcome::NotConfigured("observability is not enabled".into());
    assert_eq!(report("premise-check", 10120, &not_configured), None);
}

/// A sweep child carries `LOOM_SWEEP_ID` (#8835) alongside
/// `LOOM_SWEEP_LEASE_RENEW_DISPATCHED`: the daemon's own producer covers it.
#[test]
fn a_daemon_launched_child_starts_nothing() {
    let env = AttendEnv::from_lookup(|key| match key {
        "LOOM_SWEEP_ID" => Some("sweep-issue-10120-1".to_string()),
        "LOOM_SWEEP_LEASE_RENEW_DISPATCHED" => Some("10120".to_string()),
        attended::SESSION_ID_ENV => Some("755d0cb5".to_string()),
        _ => None,
    });
    assert!(env.daemon_launched);
    let step = decide_pr(10130, &env, || panic!("not even config is read"), never_asked);
    assert_eq!(step, PrStep::Quiet);

    // The Curator hook goes straight to `attended::start`, which refuses first.
    let outcome = attended::start(&request(10120, PathBuf::from("/nonexistent")), &env);
    assert_eq!(outcome, Outcome::DaemonLaunched);
    assert_eq!(report("premise-check", 10120, &outcome), None);
}

#[test]
fn without_a_session_nothing_is_located_and_the_forge_is_not_asked() {
    let env = AttendEnv {
        session_id: None,
        daemon_launched: false,
    };
    let PrStep::Unscoped(why) = decide_pr(10130, &env, configured, never_asked) else {
        panic!("no session, no run");
    };
    assert!(why.contains(attended::SESSION_ID_ENV), "{why}");
}

#[test]
fn a_hook_request_names_no_role_so_the_subagent_type_does() {
    let request = request(10120, PathBuf::from("."));
    assert_eq!(request.issue, 10120);
    assert_eq!(request.role, None);
    assert_eq!(request.transcript, None);
    assert_eq!(request.max_age_secs, DEFAULT_MAX_AGE_SECS);
    assert_eq!(request.idle_exit_secs, DEFAULT_IDLE_EXIT_SECS);
}

#[test]
fn a_refusal_that_matters_is_reported_in_one_line() {
    let line = report("premise-check", 10120, &Outcome::TopLevelSession).unwrap();
    assert!(line.starts_with("premise-check: live output: issue #10120:"), "{line}");
    assert!(line.contains("#10129") && !line.contains('\n'), "{line}");
}

/// `/loom:curator 10120` typed into a top-level session: live output is
/// configured, yet the session's own transcript is refused (#10129), exactly
/// as for a Builder's `lease ensure`.
#[cfg(feature = "otlp")]
#[test]
fn a_top_level_session_starts_nothing() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("checkout");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    let key = scratch.path().join("ingest.key");
    std::fs::write(&key, "test-ingest-key\n").unwrap();
    let config = serde_json::json!({
        "observability": {
            "enabled": true,
            "exporter": "otlp",
            "endpoint": "http://127.0.0.1:9",
            "ingestKeyFile": key,
            "liveOutput": { "enabled": true }
        }
    });
    std::fs::write(root.join(".loom/config.json"), config.to_string()).unwrap();
    let transcript = scratch.path().join("projects/-p/755d0cb5.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(
        &transcript,
        "{\"type\":\"user\",\"message\":{\"content\":\"/loom:curator 10120\"}}\n",
    )
    .unwrap();

    let request = StartRequest {
        transcript: Some(transcript),
        ..request(10120, root)
    };
    let outcome = attended::start(&request, &attended_env());
    assert_eq!(outcome, Outcome::TopLevelSession);
    assert!(report("premise-check", 10120, &outcome).is_some());
}
