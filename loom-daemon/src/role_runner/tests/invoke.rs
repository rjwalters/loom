//! Scheduled-role invocation through `ScriptRoleInvocationRunner::invoke` /
//! `run_role_with_timeout`: the token-pool preflights, the spawn, its exit
//! and timeout outcomes, model/effort resolution into argv, and the per-role
//! log. Split out of `role_runner/tests.rs` when #9548 registered these
//! launches' roots as writable checkouts (the real write-scope gate now runs
//! in front of every tick), so the over-threshold parent shrinks rather than
//! grows (`.loom/docs/file-size-policy.md`).

use super::*;
// #9548: gate-reaching tests hold the default serial key; see `crate::write_scope_test_support`.

// ===================================================================
// ScriptRoleInvocationRunner — resolution + execution
// ===================================================================

#[test]
fn test_resolve_spawn_bin_missing_is_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf());
    let outcome = runner.invoke("curator", "/curator");
    assert!(!outcome.is_success());
    let RoleTickOutcome::Failure(reason) = outcome else {
        panic!("expected Failure");
    };
    assert!(reason.contains("spawn-worker.sh not found"), "{reason}");
}

/// #4642: a workspace with a resolvable `spawn-worker.sh` but NO token
/// pool (neither per-repo nor shared) must short-circuit to
/// `NoTokenPool` — proving the pre-spawn check fires *before*
/// `run_role_with_timeout` ever runs the script — by asserting a marker
/// file the script would write is never created.
#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_invoke_short_circuits_with_no_token_pool_before_running_the_script() {
    use std::os::unix::fs::PermissionsExt;

    // Force a deterministic "no shared pool" resolution regardless of a
    // real `~/.loom/tokens` on the machine running this test.
    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    // A real, resolvable spawn-worker.sh that proves whether it ran by
    // writing a marker file.
    let marker = root.join("script-ran");
    let worker = root.join(".loom/scripts/spawn-worker.sh");
    fs::write(&worker, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display())).unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

    let before = NO_TOKEN_POOL_SKIP_COUNT.on_this_thread();
    let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
    let outcome = runner.invoke("curator", "/loom:curator");

    assert_eq!(outcome, RoleTickOutcome::NoTokenPool);
    assert!(!outcome.is_success());
    assert!(!marker.exists(), "the doomed script must never actually run");
    assert_eq!(NO_TOKEN_POOL_SKIP_COUNT.on_this_thread(), before + 1);

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
}

/// #4642: the SAME workspace with a per-repo `.loom/tokens/` pool
/// populated proceeds past the check and actually runs the script —
/// proving the gate re-checks live state rather than caching a verdict.
#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_invoke_proceeds_once_a_per_repo_token_pool_exists() {
    use std::os::unix::fs::PermissionsExt;

    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    fs::create_dir_all(root.join(".loom/tokens")).unwrap();
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    let worker = root.join(".loom/scripts/spawn-worker.sh");
    fs::write(&worker, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

    let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
    let outcome = runner.invoke("curator", "/loom:curator");
    // Not asserting `Success` specifically: with no
    // `.loom/roles`/`.loom/runtimes` manifests in this minimal fixture,
    // the runtime-admission step below the token check is expected to
    // reject the (unconfigured) default runtime — the point of this test
    // is only that the token-pool gate itself let the tick past, i.e.
    // the outcome is never `NoTokenPool` once a pool exists.
    assert_ne!(outcome, RoleTickOutcome::NoTokenPool);

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
}

/// #7607: a workspace with a resolvable `spawn-worker.sh` and a per-repo
/// pool that HOLDS token files, but every one of them is bad-marked, must
/// short-circuit to `PoolExhausted` — proving the new preflight fires
/// before `run_role_with_timeout` ever runs the script, exactly like the
/// `#4642` `NoTokenPool` case above but for the "present but exhausted"
/// state that check does not cover.
#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_invoke_short_circuits_with_exhausted_token_pool_before_running_the_script() {
    use std::os::unix::fs::PermissionsExt;

    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    fs::create_dir_all(root.join(".loom/tokens")).unwrap();
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    fs::write(
        root.join(".loom/tokens/.bad_tokens"),
        format!("{} fake auth failure\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
    )
    .unwrap();
    let marker = root.join("script-ran");
    let worker = root.join(".loom/scripts/spawn-worker.sh");
    fs::write(&worker, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display())).unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

    let before = POOL_EXHAUSTED_SKIP_COUNT.on_this_thread();
    let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
    let outcome = runner.invoke("curator", "/loom:curator");

    let RoleTickOutcome::PoolExhausted { total, .. } = outcome else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!(total, 1);
    assert!(!marker.exists(), "the doomed script must never actually run");
    assert_eq!(POOL_EXHAUSTED_SKIP_COUNT.on_this_thread(), before + 1);

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
}

/// #7607 AC: "within one tick of a readmission, role ticks resume with no
/// manual restart" — the preflight re-checks live pool state on every
/// call, so clearing the `.bad_tokens` entry (the readmission) must let
/// the very next `invoke()` proceed past the exhausted-pool gate with no
/// process restart or cached verdict.
#[test]
#[serial(loom_shared_tokens_dir_env)]
fn test_invoke_recovers_within_one_tick_once_the_pool_is_readmitted() {
    use std::os::unix::fs::PermissionsExt;

    let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    fs::create_dir_all(root.join(".loom/tokens")).unwrap();
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    let bad_tokens = root.join(".loom/tokens/.bad_tokens");
    fs::write(
        &bad_tokens,
        format!("{} fake auth failure\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
    )
    .unwrap();
    let worker = root.join(".loom/scripts/spawn-worker.sh");
    fs::write(&worker, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

    let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
    let outcome = runner.invoke("curator", "/loom:curator");
    assert!(matches!(outcome, RoleTickOutcome::PoolExhausted { .. }), "{outcome:?}");

    // Readmission: the operator clears the bad-token entry. No restart —
    // just re-invoke.
    fs::remove_file(&bad_tokens).unwrap();
    let outcome = runner.invoke("curator", "/loom:curator");
    assert!(
        !matches!(outcome, RoleTickOutcome::PoolExhausted { .. }),
        "expected the very next tick to get past the exhausted-pool gate, got {outcome:?}"
    );

    match prev_shared {
        Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
        None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
    }
}

#[test]
#[serial]
fn test_invoke_success_on_zero_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo ok; exit 0");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
}

#[test]
#[serial]
fn test_invoke_failure_on_nonzero_exit_includes_output_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo boom detail; exit 1");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    let outcome = runner.invoke("curator", "/curator");
    let RoleTickOutcome::Failure(reason) = outcome else {
        panic!("expected Failure");
    };
    assert!(reason.contains("boom detail"), "{reason}");
}

/// Issue #6757 (end-to-end): a real invocation whose stderr carries a
/// pre-flight sentinel followed by unrelated trailing noise must surface
/// the sentinel — and the role's own log path — in the `Failure` reason,
/// not the arbitrary trailing noise line.
#[test]
#[serial]
fn test_invoke_failure_names_preflight_sentinel_not_trailing_noise() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(
        tmp.path(),
        "fake-spawn.sh",
        "echo '# MCP_PREFLIGHT_FAILED' >&2; echo 'resolved /some/path via \\$PATH (mtime: \
             2026-01-01)' >&2; exit 1",
    );
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    let outcome = runner.invoke("curator", "/curator");
    let RoleTickOutcome::Failure(reason) = outcome else {
        panic!("expected Failure");
    };
    assert!(reason.contains("MCP_PREFLIGHT_FAILED"), "{reason}");
    assert!(reason.contains("role-curator.log"), "{reason}");
    assert!(!reason.contains("resolved /some/path"), "{reason}");
}

#[test]
#[serial]
fn test_invoke_receives_prompt_and_skip_permissions_flag() {
    let tmp = tempfile::tempdir().unwrap();
    // Fail unless invoked with
    //   -p "/curator" --model <m> --dangerously-skip-permissions
    // (the `--model` pin was inserted after the prompt by #4501, mirroring
    // `sweep_registry::spawn_child`'s argv order).
    let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$1\" = \"-p\" ] && [ \"$2\" = \"/curator\" ] && [ \"$3\" = \"--model\" ] && [ -n \"$4\" ] && [ \"$5\" = \"--dangerously-skip-permissions\" ] && exit 0 || exit 1",
        );
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
}

// -- #5508: per-owner GH_CONFIG_DIR forwarded to role-runner children --

/// A role-runner child spawned for a workspace registered under a
/// non-default owner (mirrors a `2AMLogic/*` managed repo, #5401/#5431)
/// must carry that owner's `GH_CONFIG_DIR` — otherwise it inherits the
/// daemon's own installation token (scoped to the root owner only) and
/// every forge call the spawned Champion/Judge/etc. session makes 404s,
/// exactly the live incident #5508 reported.
#[test]
#[serial]
fn run_role_with_timeout_forwards_owner_gh_config_dir_for_a_registered_root() {
    crate::credential_preflight::clear_owner_root_registry();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let owner_dir = root.join(".loom/gh-config-by-owner/2AMLogic");
    crate::credential_preflight::register_root_gh_config_dir(&root, &owner_dir);

    let observed = root.join("observed-gh-config-dir");
    let script = write_fake_script(
        &root,
        "fake-spawn.sh",
        &format!("printf '%s' \"$GH_CONFIG_DIR\" > '{}'", observed.display()),
    );
    let ws = crate::write_scope_test_support::WritableRoot::register(std::path::Path::new(&root));
    let mut runner = ScriptRoleInvocationRunner::new(root.clone())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("champion", "/loom:champion"), RoleTickOutcome::Success);
    assert_eq!(
        fs::read_to_string(&observed).unwrap(),
        owner_dir.to_string_lossy(),
        "a registered root's role child must carry the owner's GH_CONFIG_DIR"
    );

    crate::credential_preflight::clear_owner_root_registry();
}

/// The flip side: a workspace that is NOT registered under a non-default
/// owner (the common single-owner fleet, or the root owner's own repos)
/// must be a byte-identical no-op — the child's `GH_CONFIG_DIR` is left
/// untouched so it inherits the daemon's own process-global default.
#[test]
#[serial]
fn run_role_with_timeout_leaves_gh_config_dir_untouched_for_an_unregistered_root() {
    let _env_guard = ClearedGhConfigDirEnv::new();
    crate::credential_preflight::clear_owner_root_registry();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();

    let observed = root.join("observed-gh-config-dir");
    let script = write_fake_script(
        &root,
        "fake-spawn.sh",
        &format!("printf '%s' \"${{GH_CONFIG_DIR:-__unset__}}\" > '{}'", observed.display()),
    );
    let ws = crate::write_scope_test_support::WritableRoot::register(std::path::Path::new(&root));
    let mut runner = ScriptRoleInvocationRunner::new(root.clone())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("champion", "/loom:champion"), RoleTickOutcome::Success);
    assert_eq!(fs::read_to_string(&observed).unwrap(), "__unset__");
}

/// Issue #4501: a role spawn pins the model explicitly — a role child must
/// never inherit the account's interactive CLI default (`fable` on the host
/// that filed the issue, where every child instantly died on "You've reached
/// your Fable 5 limit"). With no config the pin is the shipped
/// `DEFAULT_DISPATCH_MODEL` (`sonnet`).
#[test]
#[serial]
fn test_invoke_appends_resolved_model_defaulting_to_sonnet() {
    // #9360: "with no config" has to mean no ambient runtime pin either — an
    // inherited `LOOM_RUNTIME=opencode` (every native dispatch worker's agent
    // session) selects `resolve_dispatch_model`'s native branch, which has no
    // shipped default, and the `--model` token asserted below is then never
    // emitted at all.
    let _runtime_env = ClearedLoomRuntimeEnv::new();
    let tmp = tempfile::tempdir().unwrap();
    let script =
        write_fake_script(tmp.path(), "fake-spawn.sh", "printf '%s\\n' \"$@\" > argv.txt; exit 0");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    let argv = fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
    let args: Vec<&str> = argv.lines().collect();
    let idx = args
        .iter()
        .position(|a| *a == "--model")
        .expect("role spawn argv must contain --model");
    assert_eq!(
        args[idx + 1],
        sweep_registry::DEFAULT_DISPATCH_MODEL,
        "default role-runner model must be the shipped dispatch default; argv: {args:?}"
    );
    assert_ne!(args[idx + 1], "fable", "role children must never run fable by default");
}

/// Issue #4501: `autonomous.roleRunner.model` wins over the shipped default
/// (and over `autonomous.model`) — the explicit-request tier of the shared
/// `resolve_dispatch_model` chain.
#[test]
#[serial]
fn test_invoke_config_model_override_wins() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"model": "opus", "roleRunner": {"enabled": true, "model": "claude-sonnet-4-6"}}}"#,
    );
    let script =
        write_fake_script(tmp.path(), "fake-spawn.sh", "printf '%s\\n' \"$@\" > argv.txt; exit 0");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    let argv = fs::read_to_string(tmp.path().join("argv.txt")).unwrap();
    assert!(
        argv.contains("--model\nclaude-sonnet-4-6\n"),
        "autonomous.roleRunner.model must win; argv: {argv}"
    );
}

/// Issue #5001: end-to-end, a `roleModels.<role>` override reaches the actual
/// `--model` argv for that role while a peer role (no override) still gets the
/// global `autonomous.roleRunner.model`. This is the argv-level proof of the
/// mixed-runtime fix: the Codex-bound Judge pins a Codex-valid model while the
/// Claude-bound Curator keeps the Claude alias — from one config block.
#[test]
#[serial]
fn test_invoke_per_role_model_override_reaches_argv() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {
                "enabled": true,
                "model": "sonnet",
                "roleModels": {"judge": "gpt-5-codex"}
            }}}"#,
    );
    let script = write_fake_script(
        tmp.path(),
        "fake-spawn.sh",
        "printf '%s\\n' \"$@\" > argv-last.txt; exit 0",
    );

    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    // Judge gets its per-role Codex model.
    let mut judge = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script.clone());
    assert_eq!(judge.invoke("judge", "/loom:judge"), RoleTickOutcome::Success);
    let judge_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
    assert!(
        judge_argv.contains("--model\ngpt-5-codex\n"),
        "judge must pin its per-role model; argv: {judge_argv}"
    );

    // Curator (no override) still gets the global roleRunner.model.
    let mut curator = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(curator.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    let curator_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
    assert!(
        curator_argv.contains("--model\nsonnet\n"),
        "curator must keep the global roleRunner.model; argv: {curator_argv}"
    );
}

/// Issue #4501: with only `autonomous.model` set, the role runner joins the
/// SAME chain sweep dispatch uses rather than keeping a private default.
//
// NOTE: see the comment above `test_config_missing_file_is_default` —
// `resolve_role_runner_model` reads `read_role_runner_config` internally
// (and this test also calls it directly for the `blank` case), so it needs
// the same private-defaults-tier guard + `#[serial(loom_config_env)]`
// (#4593, discovered during review of #4590 / #4538).
#[test]
#[serial(loom_config_env)]
fn test_resolve_role_runner_model_precedence_chain() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    // No config at all -> shipped default, labelled `default`.
    let bare = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_role_runner_model(bare.path(), "curator"),
        (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
    );

    // `autonomous.model` only -> that value, labelled `autonomous.model`.
    // Routing through `resolve_dispatch_model` also means the role runner
    // inherits the #3982 logical-tier alias resolution for free
    // (`opus` -> `claude-opus-5`), exactly as sweep dispatch does.
    let shared = tempfile::tempdir().unwrap();
    write_config(shared.path(), r#"{"autonomous": {"model": "opus"}}"#);
    assert_eq!(
        resolve_role_runner_model(shared.path(), "curator"),
        ("claude-opus-5".to_string(), "autonomous.model".to_string())
    );

    // Both -> the role-runner-specific value, labelled as such.
    let both = tempfile::tempdir().unwrap();
    write_config(
        both.path(),
        r#"{"autonomous": {"model": "opus", "roleRunner": {"model": "haiku"}}}"#,
    );
    assert_eq!(
        resolve_role_runner_model(both.path(), "curator"),
        ("haiku".to_string(), "autonomous.roleRunner.model".to_string())
    );

    // A blank override is treated as unset at every tier (never `--model ""`).
    let blank = tempfile::tempdir().unwrap();
    write_config(blank.path(), r#"{"autonomous": {"roleRunner": {"model": "   "}}}"#);
    assert_eq!(read_role_runner_config(blank.path()).model, None);
    assert_eq!(
        resolve_role_runner_model(blank.path(), "curator"),
        (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
    );

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #5001: `autonomous.roleRunner.roleModels.<role>` is a tier ABOVE the
/// global `autonomous.roleRunner.model` — a repo can point one role (Judge,
/// on Codex) at a provider-valid model while the other roles
/// (Curator/Champion, on Claude) keep a Claude alias, all from config. This
/// is the config-only fix for the `LOOM_RUNTIME_JUDGE=codex` -> `sonnet` 400
/// incident: the per-role and global model axes can finally disagree.
#[test]
#[serial(loom_config_env)]
fn test_resolve_role_runner_model_per_role_override() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    // Judge gets a Codex-valid model; curator/champion keep the global
    // Claude alias — the exact mixed-runtime shape the incident needed.
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"{"autonomous": {"roleRunner": {
                "model": "sonnet",
                "roleModels": {"judge": "gpt-5-codex"}
            }}}"#,
    );
    assert_eq!(
        resolve_role_runner_model(dir.path(), "judge"),
        ("gpt-5-codex".to_string(), "autonomous.roleRunner.roleModels.judge".to_string())
    );
    // A role with no per-role entry falls through to the global tier.
    assert_eq!(
        resolve_role_runner_model(dir.path(), "curator"),
        ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
    );
    assert_eq!(
        resolve_role_runner_model(dir.path(), "champion"),
        ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
    );

    // Per-role override with NO global model set: the overridden role uses
    // its override; every other role falls all the way through to the
    // shipped default (not the override).
    let no_global = tempfile::tempdir().unwrap();
    write_config(
        no_global.path(),
        r#"{"autonomous": {"roleRunner": {"roleModels": {"judge": "gpt-5-codex"}}}}"#,
    );
    assert_eq!(
        resolve_role_runner_model(no_global.path(), "judge"),
        ("gpt-5-codex".to_string(), "autonomous.roleRunner.roleModels.judge".to_string())
    );
    assert_eq!(
        resolve_role_runner_model(no_global.path(), "guide"),
        (sweep_registry::DEFAULT_DISPATCH_MODEL.to_string(), "default".to_string())
    );

    // The lookup is case-insensitive: a `Judge` config key matches the
    // lower-cased `judge` role name the runner dispatches under.
    let cased = tempfile::tempdir().unwrap();
    write_config(
        cased.path(),
        r#"{"autonomous": {"roleRunner": {"roleModels": {"Judge": "gpt-5-codex"}}}}"#,
    );
    assert_eq!(resolve_role_runner_model(cased.path(), "judge").0, "gpt-5-codex".to_string());

    // A per-role override that is a logical Claude alias still resolves
    // through the #3982 tier map (`opus` -> `claude-opus-5`), exactly like
    // the other tiers.
    let alias = tempfile::tempdir().unwrap();
    write_config(
        alias.path(),
        r#"{"autonomous": {"roleRunner": {"roleModels": {"judge": "opus"}}}}"#,
    );
    assert_eq!(resolve_role_runner_model(alias.path(), "judge").0, "claude-opus-5");

    // A blank per-role value is dropped at parse time and falls through to
    // the global tier — never `--model ""`.
    let blank = tempfile::tempdir().unwrap();
    write_config(
        blank.path(),
        r#"{"autonomous": {"roleRunner": {"model": "sonnet", "roleModels": {"judge": "   "}}}}"#,
    );
    assert!(read_role_runner_config(blank.path()).role_models.is_empty());
    assert_eq!(
        resolve_role_runner_model(blank.path(), "judge"),
        ("sonnet".to_string(), "autonomous.roleRunner.model".to_string())
    );

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #5001: `read_role_runner_config` soft-fails a malformed / absent /
/// non-object `roleModels` to an empty map (every role falls through to the
/// global chain), and drops blank keys — mirroring the soft-fail contract of
/// every other `autonomous.roleRunner.*` field.
#[test]
#[serial(loom_config_env)]
fn test_read_role_models_soft_fails_and_normalizes() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");

    // Absent key -> empty map.
    let absent = tempfile::tempdir().unwrap();
    write_config(absent.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
    assert!(read_role_runner_config(absent.path())
        .role_models
        .is_empty());

    // Non-object value -> empty map (no panic).
    let non_object = tempfile::tempdir().unwrap();
    write_config(non_object.path(), r#"{"autonomous": {"roleRunner": {"roleModels": "sonnet"}}}"#);
    assert!(read_role_runner_config(non_object.path())
        .role_models
        .is_empty());

    // Blank keys and blank/non-string values are dropped; good entries are
    // kept, lower-cased, and trimmed.
    let mixed = tempfile::tempdir().unwrap();
    write_config(
        mixed.path(),
        r#"{"autonomous": {"roleRunner": {"roleModels": {
                "  Judge  ": "  gpt-5-codex  ",
                "curator": "",
                "   ": "sonnet",
                "guide": 42
            }}}}"#,
    );
    let models = read_role_runner_config(mixed.path()).role_models;
    assert_eq!(models.get("judge").map(String::as_str), Some("gpt-5-codex"));
    assert_eq!(models.len(), 1, "blank/non-string entries must be dropped: {models:?}");

    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
}

/// Issue #4501: the per-role log header records the pinned model and the tier
/// that supplied it, so an operator can verify the pin from
/// `role-<role>.log` alone on a live host.
#[test]
#[serial]
fn test_invoke_log_header_records_pinned_model() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "exit 0");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("guide", "/loom:guide"), RoleTickOutcome::Success);
    let log =
        fs::read_to_string(tmp.path().join(".loom").join("logs").join("role-guide.log")).unwrap();
    assert!(
        log.contains(&format!("model={} (source=default)", sweep_registry::DEFAULT_DISPATCH_MODEL)),
        "{log}"
    );
}

/// Issue #4255: a scheduled role spawn routes through `claude-wrapper.sh` by
/// appending `--use-wrapper` after `--dangerously-skip-permissions`, so a
/// transient API death is retried instead of killing the unattended role run
/// on the first failure. Serialized on a named lock shared with the opt-out
/// test so the `LOOM_USE_WRAPPER` env mutation cannot race it.
#[test]
#[serial(loom_use_wrapper_env)]
fn test_invoke_appends_use_wrapper_flag() {
    test_invoke_appends_use_wrapper_flag_body();
}

#[serial]
fn test_invoke_appends_use_wrapper_flag_body() {
    std::env::remove_var("LOOM_USE_WRAPPER");
    let tmp = tempfile::tempdir().unwrap();
    // Succeeds only when --use-wrapper directly follows
    // --dangerously-skip-permissions (argv is now
    // `-p <prompt> --model <m> --dangerously-skip-permissions --use-wrapper`
    // since the #4501 model pin).
    let script = write_fake_script(
            tmp.path(),
            "fake-spawn.sh",
            "[ \"$5\" = \"--dangerously-skip-permissions\" ] && [ \"$6\" = \"--use-wrapper\" ] && exit 0 || exit 1",
        );
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
}

/// Issue #4255: the `LOOM_USE_WRAPPER=0` debug opt-out restores the legacy
/// single-shot argv — argv ends at `--dangerously-skip-permissions` with no
/// `--use-wrapper` token.
#[test]
#[serial(loom_use_wrapper_env)]
fn test_invoke_opt_out_omits_use_wrapper_flag() {
    test_invoke_opt_out_omits_use_wrapper_flag_body();
}

#[serial]
fn test_invoke_opt_out_omits_use_wrapper_flag_body() {
    std::env::set_var("LOOM_USE_WRAPPER", "0");
    let tmp = tempfile::tempdir().unwrap();
    // Succeeds only when nothing follows --dangerously-skip-permissions
    // (argv ends there; the #4501 model pin shifted it to $5).
    let script = write_fake_script(
        tmp.path(),
        "fake-spawn.sh",
        "[ \"$5\" = \"--dangerously-skip-permissions\" ] && [ -z \"$6\" ] && exit 0 || exit 1",
    );
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    let outcome = runner.invoke("curator", "/curator");
    std::env::remove_var("LOOM_USE_WRAPPER");
    assert_eq!(outcome, RoleTickOutcome::Success);
}

#[test]
#[serial]
fn test_invoke_writes_per_role_log_file() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo hello-from-role; exit 0");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
    let log_path = tmp
        .path()
        .join(".loom")
        .join("logs")
        .join("role-curator.log");
    let contents = fs::read_to_string(log_path).unwrap();
    assert!(contents.contains("hello-from-role"), "{contents}");
}

#[test]
#[serial]
fn test_invoke_times_out_on_hung_script() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_gh_bin(ws.gh.clone())
            .with_spawn_bin(script)
            .with_timeout(Duration::from_millis(300))
            // Issue #7242: pin load-per-core low so this test deterministically
            // exercises the plain-timeout `Failure` path, independent of the
            // real host's load at test time (which would otherwise route
            // through `LoadSkipped` per issue #6637's saturation check).
            .with_load_per_core_override(0.0);
    let outcome = runner.invoke("curator", "/curator");
    let RoleTickOutcome::Failure(reason) = outcome else {
        panic!("expected Failure");
    };
    assert!(reason.contains("timed out"), "{reason}");
}

/// Issue #6637 AC4: a fake `spawn-worker.sh` that sleeps past the tick
/// ceiling, under an injected high load-per-core, must produce a
/// [`RoleTickOutcome::LoadSkipped`] — never the bare unscaled `Failure` a
/// timeout normally records. This is the exact scenario from the
/// incident that filed the issue: the auditor's 1800s ceiling firing on
/// a host that was simultaneously running sweeps, not a broken role.
#[test]
#[serial(load_skipped_count)]
fn test_invoke_times_out_under_high_load_is_load_skipped_not_failed() {
    test_invoke_times_out_under_high_load_is_load_skipped_not_failed_body();
}

#[serial]
fn test_invoke_times_out_under_high_load_is_load_skipped_not_failed_body() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30; echo done");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script)
        .with_timeout(Duration::from_millis(300))
        .with_load_per_core_override(3.5);

    let outcome = runner.invoke("auditor", "/loom:auditor");

    let RoleTickOutcome::LoadSkipped {
        load_per_core,
        detail: _,
    } = outcome
    else {
        panic!("expected LoadSkipped, got {outcome:?}");
    };
    assert!((load_per_core - 3.5).abs() < f64::EPSILON, "{load_per_core}");
}

/// Counterpart to the above: the SAME hung-script/ceiling scenario, but
/// with load-per-core measured BELOW the saturation threshold, must
/// still classify as an ordinary `Failure` — the load-skip path must
/// never fire on an unloaded host (issue #6637's fail-safe requirement).
#[test]
#[serial]
fn test_invoke_times_out_under_low_load_is_still_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script)
        .with_timeout(Duration::from_millis(300))
        .with_load_per_core_override(0.2);

    let outcome = runner.invoke("auditor", "/loom:auditor");

    let RoleTickOutcome::Failure(reason) = outcome else {
        panic!("expected Failure, got {outcome:?}");
    };
    assert!(reason.contains("timed out"), "{reason}");
}

/// Issue #6637: the `LoadSkipped` outcome must increment its own
/// counter, and must NOT be tallied under any of the pre-existing
/// skip/failure counters — mirrors the equivalent `NoTokenPool`/
/// `ModelRuntimeMismatch` counter-isolation tests below.
#[test]
#[serial(load_skipped_count)]
fn test_load_skipped_count_increments_on_load_skip() {
    test_load_skipped_count_increments_on_load_skip_body();
}

#[serial]
fn test_load_skipped_count_increments_on_load_skip_body() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
    let ws = crate::write_scope_test_support::WritableRoot::register(tmp.path());
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_gh_bin(ws.gh.clone())
        .with_spawn_bin(script)
        .with_timeout(Duration::from_millis(300))
        .with_load_per_core_override(2.0);

    let before = LOAD_SKIPPED_COUNT.on_this_thread();
    let outcome = runner.invoke("auditor", "/loom:auditor");
    assert!(matches!(outcome, RoleTickOutcome::LoadSkipped { .. }), "{outcome:?}");
    assert_eq!(LOAD_SKIPPED_COUNT.on_this_thread(), before + 1);
}

#[test]
fn test_invoke_spawn_failure_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let bogus = tmp.path().join("does-not-exist.sh");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(bogus);
    let outcome = runner.invoke("curator", "/curator");
    assert!(!outcome.is_success());
}
