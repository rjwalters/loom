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

// ===================================================================
// Reasoning-effort resolution (#8054)
// ===================================================================

/// Issue #8054: the `roleEfforts.<role>` > `effort` > unset precedence chain,
/// and its blank-is-unset contract at both tiers.
///
/// NOTE: `#[serial(loom_config_env)]` + the emptied private-defaults env for
/// the same reason `test_resolve_role_runner_model_precedence_chain` needs
/// them (#4593) — `resolve_role_runner_effort` reads the full four-tier
/// effective config, so a machine-level `defaults.json` on the host running
/// the suite would otherwise leak into these assertions.
#[test]
#[serial(loom_config_env)]
fn test_resolve_role_runner_effort_precedence_chain() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    // No config at all -> UNSET (empty string), labelled `unset`. This is the
    // no-regression case: the emission site turns an empty effort into no
    // `--effort` argument at all.
    let bare = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_role_runner_effort(bare.path(), "curator"),
        (String::new(), UNSET_EFFORT_SOURCE.to_string())
    );

    // Global `effort` only -> that value for every role, labelled with the key.
    let global = tempfile::tempdir().unwrap();
    write_config(global.path(), r#"{"autonomous": {"roleRunner": {"effort": "medium"}}}"#);
    for role in ["curator", "judge", "champion"] {
        assert_eq!(
            resolve_role_runner_effort(global.path(), role),
            ("medium".to_string(), "autonomous.roleRunner.effort".to_string()),
            "the global effort applies to {role}"
        );
    }

    // Per-role override beats the global, and names its own tier.
    let both = tempfile::tempdir().unwrap();
    write_config(
        both.path(),
        r#"{"autonomous": {"roleRunner": {
                "effort": "medium",
                "roleEfforts": {"curator": "low"}
            }}}"#,
    );
    assert_eq!(
        resolve_role_runner_effort(both.path(), "curator"),
        ("low".to_string(), "autonomous.roleRunner.roleEfforts.curator".to_string())
    );
    // A role with no per-role entry still falls through to the global tier.
    assert_eq!(
        resolve_role_runner_effort(both.path(), "judge"),
        ("medium".to_string(), "autonomous.roleRunner.effort".to_string())
    );

    // Per-role override with NO global: the named role gets its override,
    // every other role falls all the way through to UNSET (never the
    // override, and never an invented default).
    let no_global = tempfile::tempdir().unwrap();
    write_config(
        no_global.path(),
        r#"{"autonomous": {"roleRunner": {"roleEfforts": {"curator": "low"}}}}"#,
    );
    assert_eq!(
        resolve_role_runner_effort(no_global.path(), "curator"),
        ("low".to_string(), "autonomous.roleRunner.roleEfforts.curator".to_string())
    );
    assert_eq!(
        resolve_role_runner_effort(no_global.path(), "judge"),
        (String::new(), UNSET_EFFORT_SOURCE.to_string())
    );

    // The per-role lookup is case-insensitive: a `Curator` config key matches
    // the lower-cased `curator` role name the runner dispatches under.
    let cased = tempfile::tempdir().unwrap();
    write_config(
        cased.path(),
        r#"{"autonomous": {"roleRunner": {"roleEfforts": {"  Curator ": "low"}}}}"#,
    );
    assert_eq!(
        resolve_role_runner_effort(cased.path(), "curator"),
        ("low".to_string(), "autonomous.roleRunner.roleEfforts.curator".to_string())
    );

    // A blank per-role value is dropped at parse time and falls through to the
    // global tier — never `--effort ""`.
    let blank_per_role = tempfile::tempdir().unwrap();
    write_config(
        blank_per_role.path(),
        r#"{"autonomous": {"roleRunner": {"effort": "medium", "roleEfforts": {"curator": "   "}}}}"#,
    );
    assert!(read_role_runner_config(blank_per_role.path())
        .role_efforts
        .is_empty());
    assert_eq!(
        resolve_role_runner_effort(blank_per_role.path(), "curator"),
        ("medium".to_string(), "autonomous.roleRunner.effort".to_string())
    );

    // A blank global value is dropped too, and falls through to UNSET.
    let blank_global = tempfile::tempdir().unwrap();
    write_config(blank_global.path(), r#"{"autonomous": {"roleRunner": {"effort": "  "}}}"#);
    assert_eq!(read_role_runner_config(blank_global.path()).effort, None);
    assert_eq!(
        resolve_role_runner_effort(blank_global.path(), "curator"),
        (String::new(), UNSET_EFFORT_SOURCE.to_string())
    );

    // Blank at BOTH tiers still resolves to UNSET, never to `""`-with-a-tier.
    let blank_both = tempfile::tempdir().unwrap();
    write_config(
        blank_both.path(),
        r#"{"autonomous": {"roleRunner": {"effort": "", "roleEfforts": {"curator": ""}}}}"#,
    );
    assert_eq!(
        resolve_role_runner_effort(blank_both.path(), "curator"),
        (String::new(), UNSET_EFFORT_SOURCE.to_string())
    );

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #8054: the parse contract itself — soft-fail shapes that must not
/// take the whole config (or the whole field) down with them, mirroring
/// `roleModels`.
#[test]
#[serial(loom_config_env)]
fn test_read_role_runner_config_effort_soft_fails() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    // Non-string / non-object values soft-fail the field, not the block: the
    // sibling `model` key in the same object must still parse.
    let malformed = tempfile::tempdir().unwrap();
    write_config(
        malformed.path(),
        r#"{"autonomous": {"roleRunner": {"model": "sonnet", "effort": 3, "roleEfforts": "low"}}}"#,
    );
    let cfg = read_role_runner_config(malformed.path());
    assert_eq!(cfg.effort, None, "a non-string effort is unset, not `3`");
    assert!(cfg.role_efforts.is_empty(), "a non-object roleEfforts is an empty map");
    assert_eq!(cfg.model.as_deref(), Some("sonnet"), "the sibling key must be unaffected");

    // Per-ENTRY soft-fail: one bad entry must not discard its well-formed
    // siblings (the contract `roleModels` set, and the reason `roleEfforts`
    // does not soft-fail the whole field).
    let mixed = tempfile::tempdir().unwrap();
    write_config(
        mixed.path(),
        r#"{"autonomous": {"roleRunner": {"roleEfforts": {
                "curator": "low", "judge": 7, "guide": "  ", "": "high", "doctor": "medium"
            }}}}"#,
    );
    let cfg = read_role_runner_config(mixed.path());
    assert_eq!(cfg.role_efforts.get("curator").map(String::as_str), Some("low"));
    assert_eq!(cfg.role_efforts.get("doctor").map(String::as_str), Some("medium"));
    assert_eq!(
        cfg.role_efforts.len(),
        2,
        "bad entries dropped, good ones kept: {:?}",
        cfg.role_efforts
    );

    // Values are trimmed but NOT lower-cased and NOT validated — the runtime
    // owns the vocabulary, so a level it rejects must reach the CLI and fail
    // the tick loudly rather than be silently swallowed here.
    let opaque = tempfile::tempdir().unwrap();
    write_config(
        opaque.path(),
        r#"{"autonomous": {"roleRunner": {"effort": " XHIGH ", "roleEfforts": {"judge": "not-a-level"}}}}"#,
    );
    let cfg = read_role_runner_config(opaque.path());
    assert_eq!(cfg.effort.as_deref(), Some("XHIGH"));
    assert_eq!(cfg.role_efforts.get("judge").map(String::as_str), Some("not-a-level"));

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #8054, the no-regression criterion: with nothing configured anywhere,
/// the spawned argv must be byte-identical to the pre-#8054 argv — **no
/// `--effort` token at all**, so the runtime CLI's own session-default effort
/// survives end-to-end on all of today's workspaces.
#[test]
#[serial(loom_config_env)]
fn test_invoke_omits_effort_entirely_when_nothing_is_configured() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    let tmp = tempfile::tempdir().unwrap();
    let script =
        write_fake_script(tmp.path(), "fake-spawn.sh", "printf '%s\\n' \"$@\" > argv.txt; exit 0");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    let argv = fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    assert!(
        !args.contains(&"--effort"),
        "an unconfigured workspace must emit NO --effort token; argv: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a.is_empty()),
        "and certainly never an empty value: {args:?}"
    );
    // The pre-#8054 argv shape, positionally: the model value is immediately
    // followed by the permissions flag, with nothing wedged between them.
    let model_idx = args
        .iter()
        .position(|a| *a == "--model")
        .expect("--model must still be pinned");
    assert_eq!(
        args[model_idx + 2],
        "--dangerously-skip-permissions",
        "nothing may be inserted between the model pin and the permissions flag: {args:?}"
    );

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #8054: a configured global `effort` reaches the real argv, positioned
/// immediately after `--model` — the same positional contract the
/// sweep-dispatch path documents (`-p`, `--model`, `--effort`,
/// `--dangerously-skip-permissions`).
#[test]
#[serial(loom_config_env)]
fn test_invoke_emits_configured_effort_immediately_after_model() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"enabled": true, "model": "sonnet", "effort": "low"}}}"#,
    );
    let script =
        write_fake_script(tmp.path(), "fake-spawn.sh", "printf '%s\\n' \"$@\" > argv.txt; exit 0");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    let argv = fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    assert_eq!(
        args[..5],
        ["-p", "/loom:curator", "--model", "sonnet", "--effort"],
        "argv order must match the dispatch path's contract; argv: {args:?}"
    );
    assert_eq!(args[5], "low");
    assert_eq!(args[6], "--dangerously-skip-permissions");

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #8054: end-to-end, a `roleEfforts.<role>` override reaches that role's
/// actual `--effort` argv while a peer role (no override) still gets the global
/// `autonomous.roleRunner.effort` — the argv-level proof of the per-role tier,
/// mirroring `test_invoke_per_role_model_override_reaches_argv`.
#[test]
#[serial(loom_config_env)]
fn test_invoke_per_role_effort_override_reaches_argv() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {
                "enabled": true,
                "effort": "medium",
                "roleEfforts": {"curator": "low"}
            }}}"#,
    );
    let script = write_fake_script(
        tmp.path(),
        "fake-spawn.sh",
        "printf '%s\\n' \"$@\" > argv-last.txt; exit 0",
    );

    let mut curator =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script.clone());
    assert_eq!(curator.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    let curator_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
    assert!(
        curator_argv.contains("--effort\nlow\n"),
        "curator must get its per-role effort; argv: {curator_argv}"
    );

    let mut judge =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
    assert_eq!(judge.invoke("judge", "/loom:judge"), RoleTickOutcome::Success);
    let judge_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
    assert!(
        judge_argv.contains("--effort\nmedium\n"),
        "judge must keep the global effort; argv: {judge_argv}"
    );

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #8054: the per-role log header reports the resolved effort and the
/// tier that supplied it, alongside the pre-existing model fields — the surface
/// the transcript-side cost telemetry (#8052/#8059) attributes a tick's spend
/// to. Both the set and the unset rendering are pinned here.
#[test]
fn test_role_log_header_reports_model_and_effort_with_sources() {
    use crate::role_runner::model_resolution::role_log_header;

    let set = role_log_header(
        "2026-09-17T00:00:00+00:00",
        "curator",
        "haiku",
        "autonomous.roleRunner.roleModels.curator",
        "low",
        "autonomous.roleRunner.roleEfforts.curator",
    );
    assert_eq!(
        set,
        "==== loom-daemon role_runner: 2026-09-17T00:00:00+00:00 role=curator model=haiku \
         (source=autonomous.roleRunner.roleModels.curator) effort=low \
         (source=autonomous.roleRunner.roleEfforts.curator) ===="
    );

    // Unset effort renders as a NAME, not as `effort=` followed by nothing —
    // the same `<runtime CLI default>` placecard the model field already uses
    // for its own pass-through, for the same reason.
    let unset = role_log_header(
        "2026-09-17T00:00:00+00:00",
        "curator",
        "sonnet",
        "default",
        "",
        UNSET_EFFORT_SOURCE,
    );
    assert!(
        unset.contains("effort=<runtime CLI default> (source=unset)"),
        "an unset effort must read like a name, not a bug: {unset}"
    );
    assert!(!unset.contains("effort= "), "never an empty rendered value: {unset}");
    // The pre-#8054 model fields are untouched.
    assert!(unset.contains("model=sonnet (source=default)"), "{unset}");
}

/// Issue #8054: the header is actually written to `role-<role>.log` by a real
/// invocation — the unit test above pins the format, this pins the wiring (the
/// log file is the only artifact an operator inspects on a live host).
#[test]
#[serial(loom_config_env)]
fn test_invoke_writes_resolved_effort_into_the_role_log_header() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"enabled": true, "roleEfforts": {"curator": "low"}}}}"#,
    );
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "exit 0");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script.clone());
    assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    let log = fs::read_to_string(tmp.path().join(".loom/logs/role-curator.log")).unwrap();
    assert!(
        log.contains("effort=low (source=autonomous.roleRunner.roleEfforts.curator)"),
        "role-curator.log header must attribute the effort to its tier: {log}"
    );

    // A role with no entry at any tier records the unset rendering instead.
    let mut judge =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
    assert_eq!(judge.invoke("judge", "/loom:judge"), RoleTickOutcome::Success);
    let judge_log = fs::read_to_string(tmp.path().join(".loom/logs/role-judge.log")).unwrap();
    assert!(
        judge_log.contains("effort=<runtime CLI default> (source=unset)"),
        "an unconfigured role's header must say so by name: {judge_log}"
    );

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}
