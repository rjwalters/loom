//! Tests for `role_runner/model_resolution.rs` — which model a scheduled role
//! tick resolves (#4501/#5001), the `"default"` CLI pass-through sentinel and
//! the unpinned-conflict degradation (#7894), and the #5028 mismatch refusal
//! that still fires on an explicit pin. Split out of `role_runner/tests.rs` so
//! the over-threshold parent shrinks rather than grows
//! (`.loom/docs/file-size-policy.md`).

use super::*;

/// Shared fixture for the #5028 end-to-end mismatch tests: a workspace
/// admitted onto the `codex` runtime for `judge`, with a real per-repo
/// token pool (so the #4642 preflight does not short-circuit first) and a
/// fake `spawn-worker.sh` (the actual script `resolve_spawn_bin` resolves
/// and `invoke` runs — mirrors `mixed_runtime_role_launch_is_admitted_and_pinned_before_spawn`)
/// that writes a marker file if it is ever actually invoked — proving a
/// refused launch never reaches the spawn. The marker's *contents* are the
/// adapter's own argv, one argument per line, so a test can additionally assert
/// whether a `--model` pin was forwarded (#7894).
fn setup_codex_judge_fixture(root: &Path, config_extra: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    for sub in [
        ".loom/roles",
        ".loom/runtimes",
        ".loom/scripts",
        ".loom/tokens",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    write_config(
        root,
        &format!(r#"{{"runtimes":{{"roles":{{"judge":"codex"}}}}{}}}"#, config_extra),
    );
    fs::write(root.join(".loom/roles/judge.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
    fs::write(
        root.join(".loom/runtimes/codex.json"),
        r#"{"runtime":"codex","capabilities":{}}"#,
    )
    .unwrap();
    // Admission (`resolve_and_admit`) validates that the `codex` adapter
    // file exists on disk before admitting the runtime at all — it is
    // never actually exec'd in this fixture (that's `spawn-worker.sh`
    // below), but its mere absence would itself refuse the launch with a
    // `RuntimeRejected`, which is not what these tests are exercising.
    let adapter = root.join(".loom/scripts/spawn-codex.sh");
    fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();

    let marker = root.join("spawn-ran");
    let worker = root.join(".loom/scripts/spawn-worker.sh");
    fs::write(
        &worker,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 0\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();
    marker
}

/// Issue #5028 (#5001 AC2/AC3), narrowed to an EXPLICIT pin by #7894:
/// `runtimes.roles.judge = "codex"` with
/// `autonomous.roleRunner.roleModels.judge = "sonnet"` states an intent that is
/// provably wrong — a Claude-shaped model forwarded to the Codex adapter, which
/// 400s. `invoke` must still refuse it as `ModelRuntimeMismatch` BEFORE the
/// spawn, never create the adapter's marker file, and increment the dedicated
/// skip counter — never a bare `Failure`/`RuntimeRejected`. This is the
/// no-regression half of #7894's acceptance criteria.
#[test]
#[serial]
fn test_invoke_refuses_a_provable_model_runtime_mismatch_before_spawning() {
    let _env_guard = ClearedLoomRuntimeEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = setup_codex_judge_fixture(
        root,
        r#","autonomous":{"roleRunner":{"roleModels":{"judge":"sonnet"}}}"#,
    );

    let before = model_runtime_mismatch_skip_count();
    let mut runner =
        ScriptRoleInvocationRunner::new(root.to_path_buf()).with_timeout(Duration::from_secs(5));
    let outcome = runner.invoke("judge", "/loom:judge");

    let RoleTickOutcome::ModelRuntimeMismatch(mismatch) = outcome else {
        panic!("expected ModelRuntimeMismatch, got {outcome:?}");
    };
    assert_eq!(mismatch.role, "judge");
    assert_eq!(mismatch.runtime, "codex");
    assert_eq!(mismatch.model, "sonnet", "the explicitly pinned Claude-shaped model");
    assert_eq!(
        mismatch.model_source, "autonomous.roleRunner.roleModels.judge",
        "the refusal must name the tier that stated the wrong intent"
    );
    assert!(!marker.exists(), "a doomed launch must never actually spawn the adapter");
    assert_eq!(model_runtime_mismatch_skip_count(), before + 1);
}

/// Issue #7894 (the bug #6565 hit in production): `runtimes.roles.judge =
/// "codex"` with NO `roleModels.judge` pin used to resolve the Claude-shaped
/// shipped default (`sonnet`) and get refused as a `ModelRuntimeMismatch` —
/// every tick, forever (453+ consecutive skips on the affected host), with no
/// escape, because a ChatGPT-plan Codex seat rejects an explicit pin too. That
/// configuration must now ADMIT: the unpinned conflict degrades to the runtime
/// CLI's own default, so the adapter is actually spawned and receives no
/// `--model` argument at all.
#[test]
#[serial]
fn test_invoke_admits_an_unpinned_codex_role_with_no_model_pin() {
    let _env_guard = ClearedLoomRuntimeEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = setup_codex_judge_fixture(root, "");

    let before = model_runtime_mismatch_skip_count();
    let mut runner =
        ScriptRoleInvocationRunner::new(root.to_path_buf()).with_timeout(Duration::from_secs(5));
    let outcome = runner.invoke("judge", "/loom:judge");

    assert_eq!(
        outcome,
        RoleTickOutcome::Success,
        "an unpinned codex role must not skip (#7894)"
    );
    assert_eq!(
        model_runtime_mismatch_skip_count(),
        before,
        "the unpinned case must not count as a mismatch skip at all"
    );
    let argv = fs::read_to_string(&marker).expect("the adapter must actually have been spawned");
    assert!(
        !argv.lines().any(|a| a == "--model"),
        "an unpinned codex role must forward NO model pin, letting the CLI choose: {argv}"
    );
    // The Claude-shaped default in particular must never reach the adapter —
    // forwarding it is the original #5001 outage.
    assert!(!argv.lines().any(|a| a == "sonnet"), "{argv}");
}

/// Issue #7894: the same admission is reachable *deliberately* — a
/// `roleModels.<role> = "default"` pin is an explicit "let the runtime CLI pick
/// its own model" statement (the only shape a ChatGPT-plan Codex seat accepts),
/// and it must spawn with no `--model` rather than being resolved to a model
/// name or falling through to the shipped Claude default.
#[test]
#[serial]
fn test_invoke_admits_an_explicit_cli_default_sentinel_pin() {
    let _env_guard = ClearedLoomRuntimeEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = setup_codex_judge_fixture(
        root,
        r#","autonomous":{"roleRunner":{"model":"sonnet","roleModels":{"judge":"default"}}}"#,
    );

    let mut runner =
        ScriptRoleInvocationRunner::new(root.to_path_buf()).with_timeout(Duration::from_secs(5));
    let outcome = runner.invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success);
    let argv = fs::read_to_string(&marker).expect("the adapter must actually have been spawned");
    assert!(!argv.lines().any(|a| a == "--model"), "{argv}");
    assert!(
        !argv.lines().any(|a| a == "sonnet"),
        "the sentinel must NOT fall through to the global roleRunner.model tier: {argv}"
    );
}

/// Issue #5028: the SAME fixture with `roleModels.judge` pointed at a
/// Codex-valid model spawns successfully — proving the check is a
/// targeted refusal, not a blanket block on Judge-on-Codex, and that it
/// self-heals the moment the config is corrected (no restart needed).
#[test]
#[serial]
fn test_invoke_succeeds_once_role_models_supplies_a_matching_model() {
    let _env_guard = ClearedLoomRuntimeEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = setup_codex_judge_fixture(
        root,
        r#","autonomous":{"roleRunner":{"roleModels":{"judge":"gpt-5-codex"}}}"#,
    );

    let mut runner =
        ScriptRoleInvocationRunner::new(root.to_path_buf()).with_timeout(Duration::from_secs(5));
    let outcome = runner.invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success);
    assert!(marker.exists(), "a matching model must let the launch actually spawn");
}

/// Issue #7894: a `"default"` pin is a sentinel, not a model name — it resolves
/// to the empty string (which `run_role_with_timeout` renders as *no* `--model`
/// argument) instead of falling through to a lower tier or being run through
/// the `sweep.modelAliases` map. This is the only configuration a ChatGPT-plan
/// Codex seat accepts, since such a seat rejects every explicit pin on the wire.
#[test]
#[serial(loom_config_env)]
fn test_resolve_role_runner_model_cli_default_sentinel() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    // Per-role sentinel: does NOT fall through to the global tier below it.
    let per_role = tempfile::tempdir().unwrap();
    write_config(
        per_role.path(),
        r#"{"autonomous": {"model": "opus", "roleRunner": {
                "model": "sonnet",
                "roleModels": {"judge": "default", "curator": "cli-default"}
            }}}"#,
    );
    assert_eq!(
        resolve_role_runner_model(per_role.path(), "judge"),
        (
            String::new(),
            "autonomous.roleRunner.roleModels.judge (CLI default)".to_string()
        )
    );
    // `cli-default` is the more explicit synonym.
    assert_eq!(
        resolve_role_runner_model(per_role.path(), "curator"),
        (
            String::new(),
            "autonomous.roleRunner.roleModels.curator (CLI default)".to_string()
        )
    );
    // Roles with no sentinel entry are untouched.
    assert_eq!(
        resolve_role_runner_model(per_role.path(), "champion"),
        ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
    );

    // The global role-runner tier accepts the sentinel too, for a fleet whose
    // every role runs on a seat that cannot serve a pinned model.
    let global = tempfile::tempdir().unwrap();
    write_config(global.path(), r#"{"autonomous": {"roleRunner": {"model": "Default"}}}"#);
    assert_eq!(
        resolve_role_runner_model(global.path(), "guide"),
        (String::new(), "autonomous.roleRunner.model (CLI default)".to_string()),
        "the sentinel match is case-insensitive"
    );

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #7894: the sentinel predicate recognizes only the two documented
/// spellings (trimmed, case-insensitive) — never a real model name.
#[test]
fn test_is_cli_default_model_sentinel() {
    for value in [
        "default",
        "Default",
        "  DEFAULT  ",
        "cli-default",
        "CLI-Default",
    ] {
        assert!(is_cli_default_model_sentinel(value), "{value:?} must be the sentinel");
    }
    for value in [
        "sonnet",
        "gpt-5-codex",
        "claude-opus-5",
        "",
        "   ",
        "defaults",
        "default-x",
    ] {
        assert!(!is_cli_default_model_sentinel(value), "{value:?} must NOT be the sentinel");
    }
}

/// Issue #7894 (the unit-level statement of the fix): only a model that came
/// from the SHIPPED-DEFAULT tier degrades to the runtime CLI's own default on a
/// cross-family conflict. Anything an operator configured is returned verbatim,
/// so #5028's refusal of an explicit mismatched pin is preserved.
#[test]
fn test_reconcile_unpinned_model_with_runtime() {
    // Unpinned + conflicting runtime -> pass through to the CLI's own default.
    let (model, source) = reconcile_unpinned_model_with_runtime(
        "codex",
        sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(),
        "default".to_string(),
    );
    assert_eq!(model, "", "an unpinned conflict must emit no model at all");
    assert!(source.contains("CLI default"), "the source label must explain why: {source}");

    // Unpinned + agreeing runtime -> completely untouched.
    assert_eq!(
        reconcile_unpinned_model_with_runtime(
            "claude",
            sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(),
            "default".to_string()
        ),
        (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
    );

    // Unpinned + an unclassifiable runtime -> untouched (fail-open, #5028).
    assert_eq!(
        reconcile_unpinned_model_with_runtime(
            "aider",
            sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(),
            "default".to_string()
        ),
        (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
    );

    // EXPLICITLY pinned + conflicting runtime -> untouched, so the caller's
    // #5028 preflight still refuses it. This is the no-regression guarantee.
    for source in [
        "autonomous.roleRunner.roleModels.judge",
        "autonomous.roleRunner.model",
        "autonomous.model",
        "override",
    ] {
        assert_eq!(
            reconcile_unpinned_model_with_runtime(
                "codex",
                "sonnet".to_string(),
                source.to_string()
            ),
            ("sonnet".to_string(), source.to_string()),
            "an explicit {source} pin must stay refusable"
        );
    }
}

/// Issue #7894 AC4: the mismatch detail no longer presents a `roleModels` model
/// pin as the ONLY remedy — a pin is not viable at all on a ChatGPT-plan Codex
/// seat, so the text must also name the CLI-default pass-through.
#[test]
fn test_model_runtime_mismatch_detail_offers_the_cli_default_passthrough() {
    let detail = ModelRuntimeMismatch {
        role: "judge".to_string(),
        runtime: "codex".to_string(),
        model: "sonnet".to_string(),
        model_source: "autonomous.roleRunner.roleModels.judge".to_string(),
        reason: "runtime \"codex\" only accepts Codex models".to_string(),
    }
    .detail();
    assert!(detail.contains("autonomous.roleRunner.roleModels.judge"), "{detail}");
    assert!(detail.contains("\"default\""), "must name the sentinel verbatim: {detail}");
    assert!(
        detail.contains("ChatGPT-plan"),
        "must say why a pin may be impossible: {detail}"
    );
}
