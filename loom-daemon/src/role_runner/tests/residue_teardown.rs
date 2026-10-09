//! #10802 (Judge finding 3 on #11238): a role run's terminal outcome hands
//! the run to the agent-residue exit teardown, with its recorded scope unit,
//! exactly as a terminal sweep does. Under `cfg(test)` the teardown is only
//! recorded (`agent_residue_reaper::take_requested`), never performed.

use super::*;

/// One tick of a child that records its pid, then runs `body`.
fn tick(body: &str, timeout: Duration) -> (RoleTickOutcome, u32, Vec<ExitReq>) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let pid_file = root.join("pid");
    let script = write_fake_script(
        &root.join("bin"),
        "spawn-worker.sh",
        &format!("printf '%s' $$ > '{}'\n{body}", pid_file.display()),
    );
    let ws = crate::write_scope_test_support::WritableRoot::register(root);
    let outcome = run_role_with_timeout(
        &script,
        root,
        &ws.gh,
        "judge",
        "/loom:judge",
        root.join("logs"),
        timeout,
        "",
        "default",
        "",
        "default",
        None,
        Some(0.0),
        None,
        None,
        None,
    );
    let pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (outcome, pid, crate::agent_residue_reaper::take_requested(root))
}

type ExitReq = crate::agent_residue_reaper::ExitRequest;

#[test]
#[serial]
fn every_role_outcome_requests_the_exit_teardown_with_its_scope() {
    let _env = ClearedLoomRuntimeEnv::new();
    for (body, timeout, want_success) in [
        ("exit 0", Duration::from_secs(30), true),
        ("exit 3", Duration::from_secs(30), false),
        ("sleep 30", Duration::from_secs(1), false),
    ] {
        let (outcome, pid, requested) = tick(body, timeout);
        assert_eq!(matches!(outcome, RoleTickOutcome::Success), want_success, "{outcome:?}");
        assert_eq!(requested.len(), 1, "{body}: exactly one teardown per run");
        let req = &requested[0];
        assert_eq!((req.pid, req.pgid, req.issue), (pid, Some(pid), None), "{body}");
        assert!(req.sweep_id.starts_with("role-judge-"), "{}", req.sweep_id);
        // The scope the run was launched with (Linux names one), not a guess.
        let want_scope =
            cfg!(target_os = "linux").then(|| format!("loom-agent-{}.scope", req.sweep_id));
        assert_eq!(req.scope_unit, want_scope, "{body}");
    }
}
