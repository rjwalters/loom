//! #9473: a scheduled role tick admitted for Claude never carries the LLM
//! gateway contract, even when it launches through `spawn-worker.sh` (the one
//! launcher that may receive it). `guard_dispatch`'s own matrix lives in
//! `worker_spawn::llm_gateway::tests`; this pins the role-runner call site.

use super::*;

fn admitted(runtime: &str) -> crate::runtime_admission::ResolvedRuntime {
    crate::runtime_admission::ResolvedRuntime {
        role: "champion".into(),
        runtime: runtime.into(),
        source: crate::runtime_admission::RuntimeSource::RoleConfig,
        adapter: PathBuf::from(format!("/nonexistent/spawn-{runtime}.sh")),
        role_manifest: PathBuf::from("/nonexistent/champion.json"),
        runtime_manifest: PathBuf::from(format!("/nonexistent/{runtime}.json")),
        suggested_worker_type: None,
        preference: None,
        execution: None,
    }
}

/// What the child saw: `<LOOM_RUNTIME>|<gateway URL>|<virtual key>`.
fn tick(runtime: &str) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let observed = root.join("observed-gateway-env");
    // Named like the real seam, so only the admitted-runtime rule can strip it.
    let script = write_fake_script(
        &root.join("bin"),
        "spawn-worker.sh",
        &format!(
            "printf '%s|%s|%s' \"${{LOOM_RUNTIME:-}}\" \"${{LOOM_LLM_GATEWAY_URL:-__unset__}}\" \
             \"${{LOOM_LLM_GATEWAY_VK:-__unset__}}\" > '{}'",
            observed.display()
        ),
    );
    let ws = crate::write_scope_test_support::WritableRoot::register(root);
    let admission = admitted(runtime);
    std::env::set_var("LOOM_LLM_GATEWAY_URL", "http://llm-gateway.example.net:8080/v1");
    std::env::set_var("LOOM_LLM_GATEWAY_VK", "sk-bf-role-runner-fixture");
    let outcome = run_role_with_timeout(
        &script,
        root,
        &ws.gh,
        "champion",
        "/loom:champion",
        root.join("logs"),
        Duration::from_secs(30),
        "",
        "default",
        "",
        "default",
        Some(&admission),
        None,
        None,
        None,
    );
    std::env::remove_var("LOOM_LLM_GATEWAY_URL");
    std::env::remove_var("LOOM_LLM_GATEWAY_VK");
    assert_eq!(outcome, RoleTickOutcome::Success, "{runtime}");
    fs::read_to_string(&observed).unwrap()
}

#[test]
#[serial]
fn a_claude_or_codex_role_tick_never_carries_the_llm_gateway_contract() {
    for runtime in ["claude", "codex"] {
        assert_eq!(tick(runtime), format!("{runtime}|__unset__|__unset__"));
    }
    // Control: a tick admitted for a mapped native harness keeps it, for
    // `spawn-worker` to map per launch (or scrub, for an unrouted profile).
    assert_eq!(
        tick("opencode"),
        "opencode|http://llm-gateway.example.net:8080/v1|sk-bf-role-runner-fixture"
    );
}
