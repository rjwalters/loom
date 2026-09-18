use super::*;
use serial_test::serial;
use std::fs;

fn write_config(root: &Path, contents: &str) {
    fs::create_dir_all(root.join(".loom")).unwrap();
    fs::write(root.join(".loom").join("config.json"), contents).unwrap();
}

// -- clean_and_cap_detail (#5024) ---------------------------------------

#[test]
fn clean_and_cap_detail_strips_ansi_and_trims() {
    let raw = "\x1b[31merror:\x1b[0m something failed\n";
    assert_eq!(clean_and_cap_detail(raw), "error: something failed");
}

#[test]
fn clean_and_cap_detail_round_trips_short_clean_text_unchanged() {
    let raw = "exit code 1: connection refused";
    assert_eq!(clean_and_cap_detail(raw), raw);
}

#[test]
fn clean_and_cap_detail_caps_oversized_text() {
    let raw = "x".repeat(MAX_FAILURE_DETAIL_CHARS * 4);
    let cleaned = clean_and_cap_detail(&raw);
    // Capped body + a short "… [truncated]" marker — bound generously
    // above MAX_FAILURE_DETAIL_CHARS so the assertion doesn't hardcode
    // the marker's exact byte/char width.
    assert!(
        cleaned.chars().count() <= MAX_FAILURE_DETAIL_CHARS + 32,
        "cleaned detail was not capped: {} chars",
        cleaned.chars().count()
    );
    assert!(cleaned.ends_with("[truncated]"));
}

#[test]
fn clean_and_cap_detail_never_cuts_mid_token() {
    // Issue #6757 AC3: a byte-count cap must not slice through a word.
    // Build text whose exact-char-count cut point (MAX_FAILURE_DETAIL_CHARS)
    // lands in the middle of a distinctive token, and assert that token
    // never appears fragmented in the output.
    let filler = "word ".repeat(MAX_FAILURE_DETAIL_CHARS); // plenty over the cap
    let raw = format!("{filler}UNMISTAKABLE_TOKEN_BOUNDARY more text after");
    let cleaned = clean_and_cap_detail(&raw);
    assert!(
        !cleaned.contains("UNMISTAKABLE_TOKEN"),
        "token should have been cut before it started: {cleaned:?}"
    );
    assert!(cleaned.ends_with("… [truncated]"));
    // The retained body (before the truncation marker) must end on a
    // whole "word", never a fragment like "wor" or "wo".
    let body = cleaned
        .strip_suffix("… [truncated]")
        .expect("checked ends_with above");
    assert!(
        body.is_empty() || body.ends_with("word"),
        "cap did not land on a word boundary: {body:?}"
    );
}

// -- truncate_tail (#6757 AC3) -------------------------------------

#[test]
fn truncate_tail_round_trips_short_text_unchanged() {
    let raw = "short output, well under the cap";
    assert_eq!(truncate_tail(raw), raw);
}

#[test]
fn truncate_tail_never_cuts_mid_token() {
    // Construct text where the raw byte-window start (len - MAX_OUTPUT_TAIL_BYTES)
    // lands inside a distinctive token, and assert the retained tail
    // never contains a fragment of it — only the whole token or nothing.
    let padding = "x".repeat(MAX_OUTPUT_TAIL_BYTES - 5);
    let raw = format!("{padding}resolved /some/very/long/path/to/loom-daemon via $PATH (mtime: 2026-01-01T00:00:00Z)");
    let tail = truncate_tail(&raw);
    assert!(
        !tail.contains("solved") && !tail.contains("esolved"),
        "tail must not contain a fragment of \"resolved\": {tail:?}"
    );
    // Either the whole word survived, or the cut landed past it entirely.
    if tail.contains("resolved") {
        assert!(tail.starts_with("resolved") || tail.split_whitespace().next() == Some("resolved"));
    }
}

#[test]
fn truncate_tail_falls_back_to_byte_cut_for_a_single_giant_token() {
    // No whitespace anywhere in the oversized text — no word boundary
    // exists, so the pre-#6757 byte-cut behavior must still apply
    // rather than the result becoming empty.
    let raw = "x".repeat(MAX_OUTPUT_TAIL_BYTES * 3);
    let tail = truncate_tail(&raw);
    assert!(!tail.is_empty());
    assert!(tail.chars().all(|c| c == 'x'));
}

// `find_failure_sentinel` / `describe_role_failure` (#6757 AC1/AC2, #8123)
// tests live with their implementation in `role_runner/failure_sentinel.rs`
// (`.loom/docs/file-size-policy.md` — new code for an over-threshold parent
// goes to a new sibling module, not inline here).

// -- had_ever_succeeded / failure_history_note (#6757 AC4) --------------

#[test]
fn failure_history_note_distinguishes_never_succeeded_from_regressed() {
    assert_eq!(failure_history_note(false), "has never completed a successful tick");
    assert_eq!(
        failure_history_note(true),
        "regressed after previously completing at least one successful tick"
    );
}

#[test]
#[serial(role_tick_ring)]
fn had_ever_succeeded_false_for_a_pair_that_has_only_ever_failed() {
    let root = PathBuf::from("/tmp/loom-6757-never-succeeded");
    record_role_tick("champion", &root, &RoleTickOutcome::Failure("boom".into()));
    assert!(!had_ever_succeeded("champion", &root));
    record_role_tick("champion", &root, &RoleTickOutcome::Failure("boom again".into()));
    assert!(!had_ever_succeeded("champion", &root));
}

#[test]
#[serial(role_tick_ring)]
fn had_ever_succeeded_stays_true_after_a_regression() {
    let root = PathBuf::from("/tmp/loom-6757-regressed-after-success");
    record_role_tick("curator", &root, &RoleTickOutcome::Success);
    assert!(had_ever_succeeded("curator", &root));
    // A later failure must not clear the sticky flag.
    record_role_tick("curator", &root, &RoleTickOutcome::Failure("boom".into()));
    assert!(had_ever_succeeded("curator", &root));
}

#[test]
#[serial(role_tick_ring)]
fn had_ever_succeeded_is_independent_per_role_and_root() {
    let root_a = PathBuf::from("/tmp/loom-6757-independent-a");
    let root_b = PathBuf::from("/tmp/loom-6757-independent-b");
    record_role_tick("judge", &root_a, &RoleTickOutcome::Success);
    record_role_tick("doctor", &root_a, &RoleTickOutcome::Failure("boom".into()));
    record_role_tick("judge", &root_b, &RoleTickOutcome::Failure("boom".into()));

    assert!(had_ever_succeeded("judge", &root_a));
    assert!(!had_ever_succeeded("doctor", &root_a));
    assert!(!had_ever_succeeded("judge", &root_b));
}

/// RAII guard that clears the ambient `LOOM_RUNTIME` env var for the
/// scope of a test and restores whatever value (if any) it previously
/// had — including across a mid-test assertion panic, since Rust
/// unwinds through `Drop`. Some host/dev-container shells export
/// `LOOM_RUNTIME` (as the `spawn-worker.sh` runtime selector), and
/// without this guard that ambient value silently outranks the
/// `runtimes.roles` config precedence this test exercises (#4739).
struct ClearedLoomRuntimeEnv(Option<String>);

impl ClearedLoomRuntimeEnv {
    fn new() -> Self {
        let prior = std::env::var("LOOM_RUNTIME").ok();
        std::env::remove_var("LOOM_RUNTIME");
        Self(prior)
    }
}

impl Drop for ClearedLoomRuntimeEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("LOOM_RUNTIME", v),
            None => std::env::remove_var("LOOM_RUNTIME"),
        }
    }
}

/// As [`ClearedLoomRuntimeEnv`] but for `GH_CONFIG_DIR` (#5508): the test
/// process may itself be running under a `GH_CONFIG_DIR` (a developer
/// shell, or the daemon's own #4458 process-global default), which would
/// otherwise leak into a spawned child's environment and make the
/// "unregistered root leaves GH_CONFIG_DIR untouched" test observe an
/// ambient value instead of a genuine absence.
struct ClearedGhConfigDirEnv(Option<String>);

impl ClearedGhConfigDirEnv {
    fn new() -> Self {
        let prior = std::env::var("GH_CONFIG_DIR").ok();
        std::env::remove_var("GH_CONFIG_DIR");
        Self(prior)
    }
}

impl Drop for ClearedGhConfigDirEnv {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var("GH_CONFIG_DIR", v),
            None => std::env::remove_var("GH_CONFIG_DIR"),
        }
    }
}

#[test]
#[serial]
fn mixed_runtime_role_launch_is_admitted_and_pinned_before_spawn() {
    use std::os::unix::fs::PermissionsExt;

    let _env_guard = ClearedLoomRuntimeEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for sub in [
        ".loom/roles",
        ".loom/runtimes",
        ".loom/scripts",
        ".loom/tokens",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    // #4642: a per-repo token pool so the new pre-spawn token-pool check
    // does not short-circuit this test's runtime-admission scenario.
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    // #5028: without a matching `roleModels.curator` override, curator
    // admitted onto `codex` would resolve the Claude-shaped default model
    // (`sonnet`) and now get refused as a `ModelRuntimeMismatch` BEFORE
    // this test's runtime-admission/pinning scenario ever reaches the
    // adapter — supplying the override keeps this test's scope on
    // admission/pinning, not the (separately tested) mismatch refusal.
    write_config(
        root,
        r#"{"runtimes":{"roles":{"curator":"codex"}},"autonomous":{"roleRunner":{"roleModels":{"curator":"gpt-5-codex"}}}}"#,
    );
    fs::write(root.join(".loom/roles/curator.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    fs::write(
        root.join(".loom/runtimes/codex.json"),
        r#"{"runtime":"codex","capabilities":{"mcp":"yes","worktreeIsolation":"partial"}}"#,
    )
    .unwrap();
    let adapter = root.join(".loom/scripts/spawn-codex.sh");
    fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
    let observed = root.join("observed-runtime");
    let worker = root.join(".loom/scripts/spawn-worker.sh");
    fs::write(
        &worker,
        format!("#!/bin/sh\nprintf '%s' \"$LOOM_RUNTIME\" > '{}'\n", observed.display()),
    )
    .unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

    let mut runner =
        ScriptRoleInvocationRunner::new(root.to_path_buf()).with_timeout(Duration::from_secs(5));
    assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    assert_eq!(fs::read_to_string(observed).unwrap(), "codex");
}

/// Issue #6507: regression test pinning the `LOOM_ROLE` env-var contract
/// (documented in `defaults/docs/daemon-reference.md` § "The `LOOM_ROLE`
/// contract") for `role_runner.rs`'s admission-success spawn path
/// (`run_role_with_timeout`'s `cmd.env("LOOM_ROLE", ...)` call, mirroring
/// `sweep_registry::spawn_child`'s #4768 fix).
///
/// Every other fixture in this module reaches `invoke()` via
/// `.with_spawn_bin(fake_script)`, which — per `invoke()`'s own
/// `self.spawn_bin.is_none()` gate — ALSO disables runtime admission, so
/// `admission` is always `None` there and the `LOOM_ROLE`-setting branch
/// is never exercised. This test instead leaves `spawn_bin` unset (like
/// `mixed_runtime_role_launch_is_admitted_and_pinned_before_spawn` above)
/// so `resolve_spawn_bin()` falls through to the on-disk
/// `.loom/scripts/spawn-worker.sh` fixture and admission runs for real,
/// on the built-in `claude` runtime (no codex adapter/model-override
/// fixture needed).
///
/// Negative control (see this issue's Test Plan): commenting out the
/// `cmd.env("LOOM_ROLE", &admission.role);` line in `run_role_with_timeout`
/// makes this test fail — confirmed manually while authoring it (the
/// child then inherits whatever `LOOM_ROLE` happens to be ambient in the
/// *test process's own* environment, e.g. `sweep-lifecycle` when the test
/// itself runs under a dispatched Loom sweep, rather than reading
/// `"curator"` — either way, not the admitted role, so the assertion
/// fails either way).
#[test]
#[serial]
fn invoke_sets_loom_role_env_on_admitted_spawn() {
    use std::os::unix::fs::PermissionsExt;

    let _env_guard = ClearedLoomRuntimeEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for sub in [
        ".loom/roles",
        ".loom/runtimes",
        ".loom/scripts",
        ".loom/tokens",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    // #4642: a per-repo token pool so the pre-spawn token-pool check does
    // not short-circuit before admission ever runs.
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    fs::write(root.join(".loom/roles/curator.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
    fs::write(
        root.join(".loom/runtimes/claude.json"),
        r#"{"runtime":"claude","capabilities":{}}"#,
    )
    .unwrap();
    // `resolve_and_admit` requires the chosen runtime's adapter script to
    // exist on disk even for the built-in `claude` runtime — it is never
    // invoked here (the recording `spawn-worker.sh` fixture below is what
    // actually runs), just checked for presence.
    let claude_adapter = root.join(".loom/scripts/spawn-claude.sh");
    fs::write(&claude_adapter, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&claude_adapter, fs::Permissions::from_mode(0o755)).unwrap();
    let observed = root.join("observed-role");
    let worker = root.join(".loom/scripts/spawn-worker.sh");
    fs::write(
        &worker,
        format!("#!/bin/sh\nprintf '%s' \"${{LOOM_ROLE:-unset}}\" > '{}'\n", observed.display()),
    )
    .unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

    let mut runner =
        ScriptRoleInvocationRunner::new(root.to_path_buf()).with_timeout(Duration::from_secs(5));
    assert_eq!(runner.invoke("curator", "/loom:curator"), RoleTickOutcome::Success);
    assert_eq!(
        fs::read_to_string(&observed).unwrap(),
        "curator",
        "role_runner's admission-success spawn must carry LOOM_ROLE (issue #6507)"
    );
}

/// A fake script that just exits with a fixed code, optionally writing to
/// stdout/stderr first. Written with a shebang so it's directly
/// executable — mirrors `token_ranking_refresh`'s test helper.
fn write_fake_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }
    path
}

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

    let before = no_token_pool_skip_count();
    let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
    let outcome = runner.invoke("curator", "/loom:curator");

    assert_eq!(outcome, RoleTickOutcome::NoTokenPool);
    assert!(!outcome.is_success());
    assert!(!marker.exists(), "the doomed script must never actually run");
    assert_eq!(no_token_pool_skip_count(), before + 1);

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

    let before = pool_exhausted_skip_count();
    let mut runner = ScriptRoleInvocationRunner::new(root.to_path_buf());
    let outcome = runner.invoke("curator", "/loom:curator");

    let RoleTickOutcome::PoolExhausted { total, .. } = outcome else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!(total, 1);
    assert!(!marker.exists(), "the doomed script must never actually run");
    assert_eq!(pool_exhausted_skip_count(), before + 1);

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
fn test_invoke_success_on_zero_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo ok; exit 0");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
}

#[test]
fn test_invoke_failure_on_nonzero_exit_includes_output_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo boom detail; exit 1");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
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
fn test_invoke_failure_names_preflight_sentinel_not_trailing_noise() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(
        tmp.path(),
        "fake-spawn.sh",
        "echo '# MCP_PREFLIGHT_FAILED' >&2; echo 'resolved /some/path via \\$PATH (mtime: \
             2026-01-01)' >&2; exit 1",
    );
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
    let outcome = runner.invoke("curator", "/curator");
    let RoleTickOutcome::Failure(reason) = outcome else {
        panic!("expected Failure");
    };
    assert!(reason.contains("MCP_PREFLIGHT_FAILED"), "{reason}");
    assert!(reason.contains("role-curator.log"), "{reason}");
    assert!(!reason.contains("resolved /some/path"), "{reason}");
}

#[test]
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
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
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
    let mut runner = ScriptRoleInvocationRunner::new(root.clone()).with_spawn_bin(script);
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
    let mut runner = ScriptRoleInvocationRunner::new(root.clone()).with_spawn_bin(script);
    assert_eq!(runner.invoke("champion", "/loom:champion"), RoleTickOutcome::Success);
    assert_eq!(fs::read_to_string(&observed).unwrap(), "__unset__");
}

/// Issue #4501: a role spawn pins the model explicitly — a role child must
/// never inherit the account's interactive CLI default (`fable` on the host
/// that filed the issue, where every child instantly died on "You've reached
/// your Fable 5 limit"). With no config the pin is the shipped
/// `DEFAULT_DISPATCH_MODEL` (`sonnet`).
#[test]
fn test_invoke_appends_resolved_model_defaulting_to_sonnet() {
    let tmp = tempfile::tempdir().unwrap();
    let script =
        write_fake_script(tmp.path(), "fake-spawn.sh", "printf '%s\\n' \"$@\" > argv.txt; exit 0");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
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
fn test_invoke_config_model_override_wins() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"model": "opus", "roleRunner": {"enabled": true, "model": "claude-sonnet-4-6"}}}"#,
    );
    let script =
        write_fake_script(tmp.path(), "fake-spawn.sh", "printf '%s\\n' \"$@\" > argv.txt; exit 0");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
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

    // Judge gets its per-role Codex model.
    let mut judge =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script.clone());
    assert_eq!(judge.invoke("judge", "/loom:judge"), RoleTickOutcome::Success);
    let judge_argv = fs::read_to_string(tmp.path().join("argv-last.txt")).unwrap();
    assert!(
        judge_argv.contains("--model\ngpt-5-codex\n"),
        "judge must pin its per-role model; argv: {judge_argv}"
    );

    // Curator (no override) still gets the global roleRunner.model.
    let mut curator =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
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
fn test_invoke_log_header_records_pinned_model() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "exit 0");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
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
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
    assert_eq!(runner.invoke("curator", "/curator"), RoleTickOutcome::Success);
}

/// Issue #4255: the `LOOM_USE_WRAPPER=0` debug opt-out restores the legacy
/// single-shot argv — argv ends at `--dangerously-skip-permissions` with no
/// `--use-wrapper` token.
#[test]
#[serial(loom_use_wrapper_env)]
fn test_invoke_opt_out_omits_use_wrapper_flag() {
    std::env::set_var("LOOM_USE_WRAPPER", "0");
    let tmp = tempfile::tempdir().unwrap();
    // Succeeds only when nothing follows --dangerously-skip-permissions
    // (argv ends there; the #4501 model pin shifted it to $5).
    let script = write_fake_script(
        tmp.path(),
        "fake-spawn.sh",
        "[ \"$5\" = \"--dangerously-skip-permissions\" ] && [ -z \"$6\" ] && exit 0 || exit 1",
    );
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
    let outcome = runner.invoke("curator", "/curator");
    std::env::remove_var("LOOM_USE_WRAPPER");
    assert_eq!(outcome, RoleTickOutcome::Success);
}

#[test]
fn test_invoke_writes_per_role_log_file() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "echo hello-from-role; exit 0");
    let mut runner =
        ScriptRoleInvocationRunner::new(tmp.path().to_path_buf()).with_spawn_bin(script);
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
fn test_invoke_times_out_on_hung_script() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
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
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30; echo done");
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
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
fn test_invoke_times_out_under_low_load_is_still_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
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
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_script(tmp.path(), "fake-spawn.sh", "sleep 30");
    let mut runner = ScriptRoleInvocationRunner::new(tmp.path().to_path_buf())
        .with_spawn_bin(script)
        .with_timeout(Duration::from_millis(300))
        .with_load_per_core_override(2.0);

    let before = load_skipped_count();
    let outcome = runner.invoke("auditor", "/loom:auditor");
    assert!(matches!(outcome, RoleTickOutcome::LoadSkipped { .. }), "{outcome:?}");
    assert_eq!(load_skipped_count(), before + 1);
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

// ===================================================================
// Config surface — autonomous.roleRunner
// ===================================================================

// NOTE: these tests read `read_role_runner_config`, which merges the
// private-defaults tier (`config_resolver::private_defaults_path()`) ahead
// of the tempdir-scoped config under test. That tier resolves off
// `$LOOM_CONFIG_DEFAULTS_FILE` / `$HOME` — independent of `tmp.path()` — so
// a host's real `~/.local/share/loom/config/defaults.json` can leak into
// the result. Neutralize it for the duration of each test (#4538), and use
// the same named serial group (`loom_config_env`) as the other tests below
// that mutate this exact env var — a bare `#[serial]` would not serialize
// against it, since `serial_test` locks are per-key.
#[test]
#[serial(loom_config_env)]
fn test_config_missing_file_is_default() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    let cfg = read_role_runner_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, RoleRunnerConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_malformed_json_is_default() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), "{not valid json");
    let cfg = read_role_runner_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, RoleRunnerConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_missing_block_is_default() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
    let cfg = read_role_runner_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg, RoleRunnerConfig::default());
}

#[test]
#[serial(loom_config_env)]
fn test_config_reads_enabled_roles_and_interval() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"enabled": true, "roles": ["curator", "guide"], "intervalSecs": 120}}}"#,
    );
    let cfg = read_role_runner_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(
        cfg,
        RoleRunnerConfig {
            enabled: Some(true),
            roles: Some(vec!["curator".to_string(), "guide".to_string()]),
            interval_secs: Some(120),
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            effort: None,
            role_efforts: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
            on_idle_max_wait: None,
        }
    );
}

#[test]
#[serial(loom_config_env)]
fn test_config_zero_interval_is_dropped_to_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"intervalSecs": 0}}}"#);
    let interval_secs = read_role_runner_config(tmp.path()).interval_secs;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(interval_secs, None);
}

// ===================================================================
// config_resolver migration (#4058) — tier precedence
// ===================================================================

fn write_project_config(root: &Path, contents: &str) {
    let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, contents).unwrap();
}

#[test]
#[serial(loom_config_env)]
fn test_config_project_tier_only_is_honored_like_legacy() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_project_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"enabled": true, "roles": ["curator"], "intervalSecs": 60}}}"#,
    );
    let cfg = read_role_runner_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(
        cfg,
        RoleRunnerConfig {
            enabled: Some(true),
            roles: Some(vec!["curator".to_string()]),
            interval_secs: Some(60),
            on_idle: None,
            model: None,
            role_models: BTreeMap::new(),
            effort: None,
            role_efforts: BTreeMap::new(),
            architect_max_proposals: None,
            max_concurrent: None,
            on_idle_max_wait: None,
        }
    );
}

#[test]
#[serial(loom_config_env)]
fn test_config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"enabled": true, "intervalSecs": 120}}}"#,
    );
    write_project_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"intervalSecs": 30}}}"#);
    let cfg = read_role_runner_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    // Overlapping `intervalSecs` -> project tier wins.
    assert_eq!(cfg.interval_secs, Some(30));
    // Non-overlapping `enabled` still supplied by legacy tier.
    assert_eq!(cfg.enabled, Some(true));
}

// ===================================================================
// resolve_roles
// ===================================================================

#[test]
fn test_resolve_roles_absent_is_the_interval_default_subset() {
    // Pre-#5656 this asserted `DEFAULT_ROLES.to_vec()`. It is now the
    // *interval-default subset* — every entry except the
    // idle-addressable-only ones (`architect`) — because putting
    // `architect` in DEFAULT_ROLES (the prerequisite for `onIdle` to
    // resolve it at all) must not make every repo that never pins
    // `roles` run a proposal generator on a timer.
    assert_eq!(resolve_roles(&RoleRunnerConfig::default()), interval_default_roles());
    // Every interval-default role is still present, unchanged.
    assert_eq!(
        resolve_roles(&RoleRunnerConfig::default())
            .iter()
            .map(|r| r.name)
            .collect::<Vec<_>>(),
        vec!["champion", "curator", "judge", "doctor", "auditor", "hermit", "guide"]
    );
}

#[test]
fn test_resolve_roles_empty_array_is_none() {
    let config = RoleRunnerConfig {
        enabled: None,
        roles: Some(vec![]),
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    assert_eq!(resolve_roles(&config), Vec::new());
}

#[test]
fn test_resolve_roles_filters_and_preserves_default_order() {
    let config = RoleRunnerConfig {
        enabled: None,
        roles: Some(vec!["guide".to_string(), "champion".to_string()]),
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    let roles = resolve_roles(&config);
    assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion", "guide"]);
}

#[test]
fn test_resolve_roles_ignores_unknown_names() {
    let config = RoleRunnerConfig {
        enabled: None,
        roles: Some(vec!["curator".to_string(), "not-a-role".to_string()]),
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    let roles = resolve_roles(&config);
    assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["curator"]);
}

// ===================================================================
// missing_defaults (#5339) — the pinned-allowlist-goes-stale warning
// ===================================================================

#[test]
fn test_missing_defaults_warns_for_default_absent_from_pinned_list() {
    // A pinned `roles: ["curator"]` predates `doctor` joining
    // DEFAULT_ROLES (#5272/#5291) — every other default is silently
    // missing from the resolved set too, but this asserts the specific
    // regression from the issue.
    let names = vec!["curator".to_string()];
    let missing = missing_defaults(&names);
    assert!(missing.contains(&"doctor"), "expected \"doctor\" in {missing:?}");
    // And resolve_roles's actual output omits it, per the allowlist semantics.
    let config = RoleRunnerConfig {
        enabled: None,
        roles: Some(names),
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    let resolved = resolve_roles(&config);
    assert!(!resolved.iter().any(|r| r.name == "doctor"));
}

#[test]
fn test_missing_defaults_empty_list_is_deliberate_opt_out_no_warning() {
    // An explicit `"roles": []` means "run none" — not staleness — so it
    // must not be reported as missing anything.
    assert_eq!(missing_defaults(&[]), Vec::<&str>::new());
}

#[test]
fn test_missing_defaults_empty_when_list_covers_every_default() {
    let names: Vec<String> = DEFAULT_ROLES.iter().map(|s| s.name.to_string()).collect();
    assert_eq!(missing_defaults(&names), Vec::<&str>::new());
}

#[test]
fn test_resolve_roles_unknown_name_and_missing_default_fire_independently() {
    // A list with both an unknown name (already-handled case) and a
    // missing DEFAULT_ROLES entry (#5339) must trigger both warning
    // paths in the same call without either suppressing the other —
    // asserted here via each function's independent, testable output.
    let config = RoleRunnerConfig {
        enabled: None,
        roles: Some(vec!["curator".to_string(), "not-a-role".to_string()]),
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    let roles = resolve_roles(&config);
    assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["curator"]);
    let names = vec!["curator".to_string(), "not-a-role".to_string()];
    assert!(missing_defaults(&names).contains(&"doctor"));
}

// ===================================================================
// default_roles_snapshot_id / roles_source_label / resolved_roles_log_line
// (#5654 — per-repo, per-tick diagnosability for the resolved role list)
// ===================================================================

#[test]
fn test_default_roles_snapshot_id_is_stable_and_content_derived() {
    let id = default_roles_snapshot_id();
    // Count prefix matches the live DEFAULT_ROLES length.
    assert!(id.starts_with(&format!("{}:", DEFAULT_ROLES.len())), "{id}");
    // Every current default role name appears in the identifier, in
    // DEFAULT_ROLES order — so a reader can tell exactly which roster a
    // log line was evaluated against without cross-referencing source.
    for spec in DEFAULT_ROLES {
        assert!(id.contains(spec.name), "expected {:?} in snapshot id {id:?}", spec.name);
    }
    assert!(id.contains("doctor"), "doctor must be part of the snapshot id: {id}");
    // Deterministic across calls (pure function of the static list).
    assert_eq!(id, default_roles_snapshot_id());
}

#[test]
fn test_missing_defaults_warning_embeds_snapshot_id_via_resolve_roles() {
    // missing_defaults_warning_line's warning text is not directly
    // capturable here (it goes through the `log` crate, and the
    // `log::warn!` call site itself lives in `spawn_multi_role_task`'s
    // tick loop, not in `resolve_roles`, since #6163), but the snapshot
    // id it embeds is the same pure `default_roles_snapshot_id()` this
    // test can assert independently — see
    // `test_missing_defaults_warning_line_names_workspace_and_snapshot`
    // below for a direct assertion against the built line's content.
    let id = default_roles_snapshot_id();
    assert!(id.contains("doctor"), "snapshot id must name doctor: {id}");
}

// ===================================================================
// missing_defaults_uncovered_by_on_idle / missing_defaults_warning_line
// (#6163) — workspace-naming, onIdle-awareness, and the aggregated line
// the multi-workspace tick loop now dedups on a per-resolved-config-change
// basis instead of warning on every tick.
// ===================================================================

#[test]
fn test_missing_defaults_uncovered_by_on_idle_excludes_on_idle_covered_roles() {
    // Mirrors this repo's own live config (#6163's motivating example):
    // roles pins curator/champion/judge/doctor/guide, onIdle covers
    // auditor. Only hermit is genuinely uncovered by either path.
    let names: Vec<String> = ["curator", "champion", "judge", "doctor", "guide"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let on_idle: Vec<String> = vec!["auditor".to_string()];
    let missing = missing_defaults_uncovered_by_on_idle(&names, &on_idle);
    assert_eq!(
        missing,
        vec!["hermit"],
        "auditor is onIdle-covered, must not appear: {missing:?}"
    );
}

#[test]
fn test_missing_defaults_uncovered_by_on_idle_no_on_idle_reports_everything_missing() {
    let names = vec!["curator".to_string()];
    let missing = missing_defaults_uncovered_by_on_idle(&names, &[]);
    assert_eq!(missing, missing_defaults(&names), "empty onIdle must filter nothing");
}

#[test]
fn test_missing_defaults_uncovered_by_on_idle_all_missing_covered_by_on_idle_is_empty() {
    let names = vec!["curator".to_string()];
    let on_idle: Vec<String> = missing_defaults(&names)
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        missing_defaults_uncovered_by_on_idle(&names, &on_idle),
        Vec::<&str>::new(),
        "every missing default is onIdle-covered — nothing left to warn about"
    );
}

#[test]
fn test_exactly_one_default_role_is_the_missing_defaults_reporter() {
    // AC3/AC4: the diagnostic is a property of the workspace, not of a
    // role, so exactly one of the spawned DEFAULT_ROLES loops may emit it
    // — otherwise every workspace's line is repeated once per loop.
    let reporters: Vec<&str> = DEFAULT_ROLES
        .iter()
        .filter(|spec| is_missing_defaults_reporter(spec))
        .map(|spec| spec.name)
        .collect();
    assert_eq!(
        reporters.len(),
        1,
        "exactly one DEFAULT_ROLES loop may report missing defaults: {reporters:?}"
    );
}

#[test]
fn test_missing_defaults_warning_line_is_none_when_nothing_missing() {
    assert_eq!(missing_defaults_warning_line(Path::new("/repo"), &[]), None);
}

#[test]
fn test_missing_defaults_warning_line_names_workspace_and_snapshot() {
    // AC1: names the workspace. AC4: one aggregated line for multiple
    // missing roles (not one `log::warn!` call per role).
    let root = Path::new("/Users/example/repo");
    let line = missing_defaults_warning_line(root, &["auditor", "hermit"]).unwrap();
    assert!(line.contains("/Users/example/repo"), "expected workspace path in line: {line}");
    assert!(line.contains("auditor"), "{line}");
    assert!(line.contains("hermit"), "{line}");
    assert!(line.contains(&default_roles_snapshot_id()), "expected snapshot id: {line}");
    assert!(
        line.contains("2 of"),
        "expected an aggregated count, not one line per role: {line}"
    );
}

#[test]
#[serial(loom_config_env)]
fn test_roles_source_label_absent_key_is_default() {
    // Neutralize the private/shared defaults tier (#4538 pattern above):
    // a real host can have `~/.local/share/loom/config/defaults.json`
    // set `autonomous.roleRunner.roles`, which would otherwise leak into
    // this "no tier sets it" assertion.
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    // No .loom/config.json at all — no tier sets `roles`.
    let label = roles_source_label(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(label, "default (no tier sets roles)");
}

#[test]
fn test_roles_source_label_names_the_legacy_tier() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"roles": ["curator", "champion", "judge", "doctor", "guide"]}}}"#,
    );
    let label = roles_source_label(tmp.path());
    assert!(label.starts_with("legacy ("), "{label}");
    assert!(label.contains(".loom/config.json"), "{label}");
}

#[test]
fn test_roles_source_label_names_the_project_tier_over_legacy() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"roles": ["curator"]}}}"#);
    write_project_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"roles": ["curator", "judge"]}}}"#,
    );
    let label = roles_source_label(tmp.path());
    assert!(label.starts_with("project ("), "expected project tier to win: {label}");
}

#[test]
fn test_missing_defaults_for_loom_repos_own_pinned_list_names_only_auditor_and_hermit() {
    // The exact pinned list from this repo's own `.loom/config.json`
    // (`["curator","champion","judge","doctor","guide"]`) — missing only
    // auditor and hermit, not doctor, per the issue's own "Verified
    // corrections" trace.
    let names: Vec<String> = ["curator", "champion", "judge", "doctor", "guide"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let missing = missing_defaults(&names);
    assert_eq!(missing, vec!["auditor", "hermit"]);
    assert!(
        !missing.contains(&"doctor"),
        "doctor is present in this pinned list: {missing:?}"
    );
}

#[test]
#[serial(loom_config_env)]
fn test_resolved_roles_log_line_reports_full_default_roles_with_default_source() {
    // Mirrors the Test Plan's first manual-verification case: a repo
    // config with `roleRunner: {}` (roles: None) resolves to the full
    // DEFAULT_ROLES list, including doctor, sourced from the default.
    // Neutralize the private/shared defaults tier — see the
    // `#4538`-pattern comment on `test_config_missing_file_is_default`
    // above — since a real host's own defaults file could otherwise
    // supply a `roles` value under this empty `roleRunner: {}` overlay.
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {}}}"#);
    let config = read_role_runner_config(tmp.path());
    let resolved = resolve_roles(&config);
    let line = resolved_roles_log_line(tmp.path(), &resolved);
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert!(line.contains("doctor"), "expected doctor in resolved roles line: {line}");
    assert!(
        line.contains("source=default (no tier sets roles)"),
        "expected default source label: {line}"
    );
    assert!(
        line.contains(&default_roles_snapshot_id()),
        "expected the DEFAULT_ROLES snapshot id embedded: {line}"
    );
    for spec in DEFAULT_ROLES {
        assert!(line.contains(spec.name), "expected {:?} in line: {line}", spec.name);
    }
}

#[test]
#[serial(loom_config_env)]
fn test_resolved_roles_log_line_reports_pinned_list_with_its_source() {
    // Mirrors the Test Plan's second manual-verification case: a repo
    // pinning a non-empty roles list (matching the `loom` repo's own
    // config) reports exactly that list with the legacy-tier source.
    // `roles` is explicitly set here, so this does not depend on the
    // private/shared defaults tier — env neutralization is only for
    // parallel-test isolation against the other tests in this
    // `loom_config_env` serial group.
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"roles": ["curator", "champion", "judge", "doctor", "guide"]}}}"#,
    );
    let config = read_role_runner_config(tmp.path());
    let resolved = resolve_roles(&config);
    let line = resolved_roles_log_line(tmp.path(), &resolved);
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    let names: Vec<&str> = resolved.iter().map(|r| r.name).collect();
    assert_eq!(names, vec!["champion", "curator", "judge", "doctor", "guide"]);
    // The resolved-list portion of the line (before `source=`) must omit
    // auditor/hermit — they're correctly absent from *this* pinned
    // list's resolution. (The trailing `default_roles=` segment of the
    // line intentionally names every DEFAULT_ROLES entry regardless —
    // that's the whole-roster snapshot identifier, not the resolved
    // subset, so it is not asserted against here.)
    let resolved_segment = line.split("source=").next().unwrap();
    assert!(
        !resolved_segment.contains("auditor"),
        "auditor must be absent from the resolved-roles segment: {resolved_segment}"
    );
    assert!(
        !resolved_segment.contains("hermit"),
        "hermit must be absent from the resolved-roles segment: {resolved_segment}"
    );
    assert!(line.contains("doctor"), "{line}");
    assert!(line.starts_with(&format!("role_runner: {} resolved roles=", tmp.path().display())));
    assert!(line.contains("source=legacy ("), "expected legacy-tier source: {line}");
}

// ===================================================================
// Precedence — env > config > default
// ===================================================================

#[test]
#[serial]
fn test_resolve_enabled_default_is_false() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    assert!(!resolve_enabled(&RoleRunnerConfig::default()));
}

#[test]
#[serial]
fn test_resolve_enabled_config_can_enable() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    assert!(resolve_enabled(&RoleRunnerConfig {
        enabled: Some(true),
        roles: None,
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    }));
}

#[test]
#[serial]
fn test_resolve_enabled_env_overrides_config() {
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    assert!(!resolve_enabled(&RoleRunnerConfig {
        enabled: Some(true),
        roles: None,
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    }));
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "1");
    assert!(resolve_enabled(&RoleRunnerConfig {
        enabled: Some(false),
        roles: None,
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    }));
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
}

// ===================================================================
// #6469 — the disabled branch must log at INFO and name its source
// ===================================================================

#[test]
#[serial]
fn test_resolve_enabled_with_source_names_default_when_nothing_set() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let (enabled, source) = resolve_enabled_with_source(&RoleRunnerConfig::default());
    assert!(!enabled);
    assert_eq!(source, EnabledSource::Default);
}

#[test]
#[serial]
fn test_resolve_enabled_with_source_names_config_when_env_unset() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let cfg = RoleRunnerConfig {
        enabled: Some(false),
        ..RoleRunnerConfig::default()
    };
    let (enabled, source) = resolve_enabled_with_source(&cfg);
    assert!(!enabled);
    assert_eq!(source, EnabledSource::Config);
}

#[test]
#[serial]
fn test_resolve_enabled_with_source_names_env_even_over_config() {
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    let cfg = RoleRunnerConfig {
        enabled: Some(true),
        ..RoleRunnerConfig::default()
    };
    let (enabled, source) = resolve_enabled_with_source(&cfg);
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    assert!(!enabled);
    assert_eq!(source, EnabledSource::Env);
}

// ===================================================================
// #6470 — `host_env_override()` resolution table (config-independent)
// ===================================================================

#[test]
#[serial]
fn test_host_env_override_none_when_unset() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    assert_eq!(host_env_override(), None);
}

#[test]
#[serial]
fn test_host_env_override_some_true_for_every_truthy_spelling() {
    for v in ["1", "true", "TRUE", "yes", "on", " on "] {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, v);
        assert_eq!(host_env_override(), Some(true), "{v:?} must resolve truthy");
    }
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
}

#[test]
#[serial]
fn test_host_env_override_some_false_for_every_falsy_spelling() {
    // Any *set* value that isn't one of the truthy spellings above is
    // falsy — including a value that isn't "0"/"false" at all, mirroring
    // `resolve_enabled_with_source`'s own precedence rule (env decides
    // regardless of the exact non-truthy spelling).
    for v in ["0", "false", "no", "off", "garbage", ""] {
        std::env::set_var(ROLE_RUNNER_ENABLE_ENV, v);
        assert_eq!(host_env_override(), Some(false), "{v:?} must resolve falsy");
    }
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
}

#[test]
#[serial]
fn test_host_env_override_agrees_with_resolve_enabled_with_source_env_branch() {
    // `host_env_override()` must never disagree with the config-aware
    // resolver's own `EnabledSource::Env` branch — it is a
    // config-independent read of the same precedence rule, not a
    // second, potentially-drifting implementation of it.
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    let cfg_true = RoleRunnerConfig {
        enabled: Some(true),
        ..RoleRunnerConfig::default()
    };
    let (enabled, source) = resolve_enabled_with_source(&cfg_true);
    assert_eq!(source, EnabledSource::Env);
    assert_eq!(host_env_override(), Some(enabled));
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
}

/// AC1/AC2 (env case): the disabled-branch line names `env:LOOM_ROLE_RUNNER=<value>`
/// as the source and states the "no role loops … any registered root" scope.
#[test]
#[serial]
fn test_disabled_role_runner_log_line_names_env_source() {
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    let tmp = tempfile::tempdir().unwrap();
    let line = disabled_role_runner_log_line(tmp.path(), &RoleRunnerConfig::default());
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    assert!(line.contains("source=env:LOOM_ROLE_RUNNER=\"0\""), "unexpected line: {line}");
    assert!(
        line.contains("no role loops will run on this host for any registered root"),
        "unexpected line: {line}"
    );
}

/// AC1/AC2 (config case): the disabled-branch line names the config tier
/// that resolved `autonomous.roleRunner.enabled` to `false`, not just "config".
#[test]
#[serial]
fn test_disabled_role_runner_log_line_names_config_source() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    write_project_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"enabled": false}}}"#);
    let cfg = RoleRunnerConfig {
        enabled: Some(false),
        ..RoleRunnerConfig::default()
    };
    let line = disabled_role_runner_log_line(tmp.path(), &cfg);
    assert!(
        line.starts_with(
            "role_runner: disabled source=config:autonomous.roleRunner.enabled=false from "
        ),
        "unexpected line: {line}"
    );
    assert!(
        line.contains("no role loops will run on this host for any registered root"),
        "unexpected line: {line}"
    );
}

/// AC1 (default case, no source set at all): still names its tier
/// explicitly rather than silently reading as "config" when nothing set it.
#[test]
#[serial]
fn test_disabled_role_runner_log_line_names_default_source() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let line = disabled_role_runner_log_line(tmp.path(), &RoleRunnerConfig::default());
    assert!(
        line.contains("source=default (no tier sets autonomous.roleRunner.enabled)"),
        "unexpected line: {line}"
    );
}

/// AC3: the disabled branch's actual log emission (not just the string
/// content) must land at `Level::Info`, not `Level::Debug` — this is the
/// regression #6469 was filed against (a fleet running at INFO saw zero
/// trace that role loops were off). Exercises `log_role_runner_disabled`,
/// the exact function `daemon_service.rs`'s boot sequence calls.
#[test]
#[serial]
fn test_log_role_runner_disabled_emits_at_info_for_env_source() {
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "false");
    let tmp = tempfile::tempdir().unwrap();
    let records = crate::test_log_capture::capture_logs(|| {
        log_role_runner_disabled(tmp.path(), &RoleRunnerConfig::default());
    });
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

    let disabled_lines: Vec<_> = records
        .iter()
        .filter(|(_, msg)| msg.starts_with("role_runner: disabled"))
        .collect();
    assert_eq!(disabled_lines.len(), 1, "expected exactly one line, got {records:?}");
    let (level, msg) = disabled_lines[0];
    assert_eq!(*level, log::Level::Info, "disabled branch must log at INFO, not debug: {msg}");
    assert!(msg.contains("source=env:LOOM_ROLE_RUNNER=\"false\""), "unexpected line: {msg}");
}

/// AC3 (config case): same INFO-level assertion, but with the source
/// resolved from config rather than env.
#[test]
#[serial]
fn test_log_role_runner_disabled_emits_at_info_for_config_source() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let cfg = RoleRunnerConfig {
        enabled: Some(false),
        ..RoleRunnerConfig::default()
    };
    let records = crate::test_log_capture::capture_logs(|| {
        log_role_runner_disabled(tmp.path(), &cfg);
    });

    let disabled_lines: Vec<_> = records
        .iter()
        .filter(|(_, msg)| msg.starts_with("role_runner: disabled"))
        .collect();
    assert_eq!(disabled_lines.len(), 1, "expected exactly one line, got {records:?}");
    let (level, msg) = disabled_lines[0];
    assert_eq!(*level, log::Level::Info, "disabled branch must log at INFO, not debug: {msg}");
    assert!(
        msg.contains("source=config:autonomous.roleRunner.enabled=false"),
        "unexpected line: {msg}"
    );
}

#[test]
#[serial]
fn test_resolve_interval_for_role_precedence() {
    std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    let spec = DEFAULT_ROLES[0];

    // Absent config + unset env => the role's own built-in default.
    assert_eq!(
        resolve_interval_for_role(&spec, &RoleRunnerConfig::default()),
        Duration::from_secs(spec.default_interval_secs)
    );

    // Config sets a uniform override.
    assert_eq!(
        resolve_interval_for_role(
            &spec,
            &RoleRunnerConfig {
                enabled: None,
                roles: None,
                interval_secs: Some(42),
                on_idle: None,
                model: None,
                role_models: BTreeMap::new(),
                effort: None,
                role_efforts: BTreeMap::new(),
                architect_max_proposals: None,
                max_concurrent: None,
                on_idle_max_wait: None,
            }
        ),
        Duration::from_secs(42)
    );

    // Env overrides config.
    std::env::set_var(ROLE_RUNNER_INTERVAL_ENV, "7");
    assert_eq!(
        resolve_interval_for_role(
            &spec,
            &RoleRunnerConfig {
                enabled: None,
                roles: None,
                interval_secs: Some(42),
                on_idle: None,
                model: None,
                role_models: BTreeMap::new(),
                effort: None,
                role_efforts: BTreeMap::new(),
                architect_max_proposals: None,
                max_concurrent: None,
                on_idle_max_wait: None,
            }
        ),
        Duration::from_secs(7)
    );
    std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
}

// -- per-role built-in intervals + source attribution (#6204) -----------

/// The shipped built-ins are **per-role**, not a uniform value — the claim
/// `daemon-reference.md`'s config table makes ("per-role built-in
/// (5–15 min)"). #6204 was filed after every role logged an identical
/// interval; this pins the documented shape so a future uniform-collapse
/// (or a table that drifts from the code) fails here instead of in a
/// fleet's throughput.
#[test]
#[serial]
fn test_builtin_intervals_are_per_role_not_uniform() {
    std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    let cfg = RoleRunnerConfig::default();
    let resolved: Vec<(&str, u64)> = DEFAULT_ROLES
        .iter()
        .map(|spec| (spec.name, resolve_interval_for_role(spec, &cfg).as_secs()))
        .collect();

    assert_eq!(
        resolved,
        vec![
            ("champion", 600),
            ("curator", 300),
            ("judge", 300),
            ("doctor", 300),
            ("auditor", 600),
            ("hermit", 600),
            ("guide", 900),
            ("architect", 3600),
        ],
        "built-in per-role intervals drifted — update defaults/docs/daemon-reference.md's \
             role-runner table in the same change (#6204)"
    );

    // Every interval-cadence default role sits inside the documented
    // 5–15 minute band (architect is idle-addressable-only, #5656, and is
    // deliberately the slow outlier).
    for spec in DEFAULT_ROLES.iter().filter(|s| s.is_interval_default()) {
        let secs = resolve_interval_for_role(spec, &cfg).as_secs();
        assert!(
            (300..=900).contains(&secs),
            "{} built-in interval {secs}s is outside the documented 5–15 min band",
            spec.name
        );
    }

    // …and they are genuinely diverse: the reported #6204 symptom was one
    // value for all eight roles.
    let distinct: std::collections::BTreeSet<u64> = resolved.iter().map(|(_, s)| *s).collect();
    assert!(
        distinct.len() > 1,
        "built-in intervals collapsed to a uniform value: {distinct:?}"
    );
}

#[test]
#[serial]
fn test_resolved_interval_log_line_names_builtin_source() {
    std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let champion = DEFAULT_ROLES.iter().find(|s| s.name == "champion").unwrap();
    let line = resolved_interval_log_line(tmp.path(), champion, &RoleRunnerConfig::default());
    assert_eq!(
        line,
        "role_runner: champion interval=600s source=built-in (RoleSpec::default_interval_secs)"
    );
}

#[test]
#[serial]
fn test_resolved_interval_log_line_names_config_source() {
    std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let champion = DEFAULT_ROLES.iter().find(|s| s.name == "champion").unwrap();
    let cfg = RoleRunnerConfig {
        interval_secs: Some(1800),
        ..RoleRunnerConfig::default()
    };
    let line = resolved_interval_log_line(tmp.path(), champion, &cfg);
    assert!(
        line.starts_with(
            "role_runner: champion interval=1800s \
                 source=config:autonomous.roleRunner.intervalSecs from "
        ),
        "unexpected line: {line}"
    );
    // The overridden per-role built-in is named, so "every role shows the
    // same interval" is self-diagnosing from one line.
    assert!(
        line.contains("(uniform override; per-role built-in 600s not used)"),
        "unexpected line: {line}"
    );
}

#[test]
#[serial]
fn test_resolved_interval_log_line_names_env_source() {
    std::env::set_var(ROLE_RUNNER_INTERVAL_ENV, "1800");
    let tmp = tempfile::tempdir().unwrap();
    let champion = DEFAULT_ROLES.iter().find(|s| s.name == "champion").unwrap();
    // Env wins even over a config value, and says so.
    let cfg = RoleRunnerConfig {
        interval_secs: Some(42),
        ..RoleRunnerConfig::default()
    };
    let line = resolved_interval_log_line(tmp.path(), champion, &cfg);
    std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    assert_eq!(
        line,
        "role_runner: champion interval=1800s \
             source=env:LOOM_ROLE_RUNNER_INTERVAL_SECS (uniform override; per-role built-in 600s \
             not used)"
    );
}

#[test]
#[serial]
fn test_resolve_interval_for_role_with_source_tiers() {
    std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    let spec = DEFAULT_ROLES[0];

    let (d, source) = resolve_interval_for_role_with_source(&spec, &RoleRunnerConfig::default());
    assert_eq!(d, Duration::from_secs(spec.default_interval_secs));
    assert_eq!(source, IntervalSource::BuiltIn);
    assert!(!source.is_uniform_override());

    let cfg = RoleRunnerConfig {
        interval_secs: Some(42),
        ..RoleRunnerConfig::default()
    };
    let (d, source) = resolve_interval_for_role_with_source(&spec, &cfg);
    assert_eq!(d, Duration::from_secs(42));
    assert_eq!(source, IntervalSource::Config);
    assert!(source.is_uniform_override());

    std::env::set_var(ROLE_RUNNER_INTERVAL_ENV, "7");
    let (d, source) = resolve_interval_for_role_with_source(&spec, &cfg);
    std::env::remove_var(ROLE_RUNNER_INTERVAL_ENV);
    assert_eq!(d, Duration::from_secs(7));
    assert_eq!(source, IntervalSource::Env);
    assert!(source.is_uniform_override());
}

// ===================================================================
// Loop wiring — a scripted fake runner proves ticks + panics behave
// ===================================================================

struct FakeRunner {
    outcomes: Vec<RoleTickOutcome>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RoleInvocationRunner for FakeRunner {
    fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.outcomes.get(n).cloned().unwrap_or_else(|| {
            self.outcomes
                .last()
                .cloned()
                .unwrap_or(RoleTickOutcome::Success)
        })
    }
}

async fn wait_for_calls(calls: &std::sync::atomic::AtomicUsize, target: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if calls.load(std::sync::atomic::Ordering::SeqCst) >= target {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for call count to reach {target} (saw {})",
            calls.load(std::sync::atomic::Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn test_loop_ticks_repeatedly_skipping_first_tick() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runner = FakeRunner {
        outcomes: vec![RoleTickOutcome::Success; 3],
        calls: calls.clone(),
    };
    let spec = RoleSpec {
        name: "curator",
        prompt: "/loom:curator",
        default_interval_secs: 1,
        interval_default: true,
    };
    let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = spawn_role_task(
        runner,
        spec,
        Duration::from_millis(20),
        drain,
        PathBuf::from("/tmp/loom-test-root"),
        new_in_progress_guard(),
    );

    wait_for_calls(&calls, 1, Duration::from_secs(2)).await;
    wait_for_calls(&calls, 3, Duration::from_secs(2)).await;

    handle.abort();
}

/// **#6201 AC2, stated affirmatively**: a tick that ends in
/// [`RoleTickOutcome::Failure`] is followed by another invocation on the
/// role's very next interval — no backoff, no benching, no persistent
/// "this role failed once" state gating any future tick.
///
/// The incident report for #6201 read the nine-day curator silence as
/// "permanent silent benching after a RECOVERABLE failure". Investigating
/// it (see [`note_pre_spawn_skip`]) showed the loop never benched anything
/// — but nothing in this module actually *proved* that, so the claim could
/// not be checked against the code either way. This test is that proof,
/// and it is what would fail if someone later added a
/// failure-count-gated skip.
#[tokio::test]
async fn failure_outcome_is_retried_on_the_very_next_tick_never_benched() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runner = FakeRunner {
        // Every tick fails, forever (the `unwrap_or_else(last)` fallback in
        // `FakeRunner::invoke` repeats the final entry).
        outcomes: vec![RoleTickOutcome::Failure("codex 400 (RECOVERABLE)".into())],
        calls: calls.clone(),
    };
    let spec = RoleSpec {
        name: "curator",
        prompt: "/loom:curator",
        default_interval_secs: 1,
        interval_default: true,
    };
    let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = spawn_role_task(
        runner,
        spec,
        Duration::from_millis(20),
        drain,
        PathBuf::from("/tmp/loom-test-root-6201-ac2"),
        new_in_progress_guard(),
    );

    // Four consecutive failing ticks still produce four invocations: the
    // first failure does not suppress the second, third, or fourth.
    wait_for_calls(&calls, 4, Duration::from_secs(5)).await;

    handle.abort();
}

/// **#6201 AC4**: a role failing on a broken runtime recovers
/// automatically once the runtime works again — no daemon restart, no
/// operator un-benching step, no manual re-enable. Drives the real tick
/// loop through the incident's own shape (several consecutive failures,
/// then the underlying condition is fixed) and asserts the very next tick
/// succeeds.
#[tokio::test]
async fn role_recovers_automatically_once_the_broken_runtime_works_again() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outcomes = vec![
        RoleTickOutcome::Failure("codex: model not supported (RECOVERABLE)".into()),
        RoleTickOutcome::Failure("codex: model not supported (RECOVERABLE)".into()),
        RoleTickOutcome::Failure("codex: model not supported (RECOVERABLE)".into()),
        // The operator corrects the runtime/model config here; the loop
        // has taken no action of its own to make this reachable.
        RoleTickOutcome::Success,
    ];
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    struct RecordingFake {
        outcomes: Vec<RoleTickOutcome>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        observed: std::sync::Arc<std::sync::Mutex<Vec<RoleTickOutcome>>>,
    }
    impl RoleInvocationRunner for RecordingFake {
        fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let outcome = self
                .outcomes
                .get(n)
                .cloned()
                .unwrap_or(RoleTickOutcome::Success);
            self.observed.lock().unwrap().push(outcome.clone());
            outcome
        }
    }

    let spec = RoleSpec {
        name: "curator",
        prompt: "/loom:curator",
        default_interval_secs: 1,
        interval_default: true,
    };
    let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = spawn_role_task(
        RecordingFake {
            outcomes,
            calls: calls.clone(),
            observed: observed.clone(),
        },
        spec,
        Duration::from_millis(20),
        drain,
        PathBuf::from("/tmp/loom-test-root-6201-ac4"),
        new_in_progress_guard(),
    );

    wait_for_calls(&calls, 4, Duration::from_secs(5)).await;
    handle.abort();

    let seen = observed.lock().unwrap().clone();
    assert!(
        seen.len() >= 4,
        "expected at least 4 ticks through the failing period and out the other side, saw {}",
        seen.len()
    );
    assert!(
        seen[..3].iter().all(|o| !o.is_success()),
        "fixture precondition: the first three ticks must be the failing period"
    );
    assert!(
        seen[3].is_success(),
        "the tick immediately after the underlying condition was fixed must succeed — the \
             loop must not have benched the role during the failing period (#6201 AC4)"
    );
}

/// **#6201, the confirmed mechanism**: every pre-spawn preflight bail-out
/// leaves a dated line in the role's OWN log
/// (`.loom/logs/role-<role>.log`) — the file an operator greps to answer
/// "is this role still running here?", and the file that stayed frozen
/// for nine days on the affected host precisely because these bail-outs
/// return before `run_role_with_timeout` (its only other writer) is
/// reached. See [`note_pre_spawn_skip`].
#[test]
fn pre_spawn_skip_is_recorded_in_the_roles_own_log() {
    let dir = tempfile::tempdir().unwrap();
    let logs_dir = dir.path().join(".loom").join("logs");

    note_pre_spawn_skip(
        &logs_dir,
        "curator",
        "model/runtime mismatch: runtime \"codex\" only accepts Codex models, but the \
             resolved model \"sonnet\" is a Claude model",
    );

    // The marker must land in the SAME file a real invocation writes its
    // header to — a skip logged to a different path would still leave the
    // operator-facing artifact silent.
    let log = std::fs::read_to_string(role_log_path(&logs_dir, "curator")).unwrap();
    assert!(
        log.contains("SKIPPED BEFORE SPAWN (#6201)"),
        "skip marker missing from role log: {log}"
    );
    assert!(
        log.contains("runtime \"codex\" only accepts Codex models"),
        "skip marker must name the actual reason so the log alone is diagnostic: {log}"
    );
    assert!(log.contains("role=curator"), "skip marker must name the role: {log}");

    // Repeated skips append rather than overwrite: a role stuck in this
    // state shows a growing, timestamped trail instead of one stale line.
    note_pre_spawn_skip(&logs_dir, "curator", "no token pool available");
    let log = std::fs::read_to_string(role_log_path(&logs_dir, "curator")).unwrap();
    assert_eq!(
        log.matches("SKIPPED BEFORE SPAWN (#6201)").count(),
        2,
        "each skipped tick must leave its own line: {log}"
    );
}

/// A drain in progress (#4090) stops role ticks from *starting*: with the
/// drain flag set before the loop runs, `spawn_role_task` performs ZERO
/// `invoke` calls even after several tick intervals elapse. This is the
/// highest-value new role-runner coverage (Finding 2 — role ticks had no
/// halt gate at all before this).
#[tokio::test]
async fn test_drain_stops_role_ticks_from_starting() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runner = FakeRunner {
        outcomes: vec![RoleTickOutcome::Success; 3],
        calls: calls.clone(),
    };
    let spec = RoleSpec {
        name: "champion",
        prompt: "/loom:champion",
        default_interval_secs: 1,
        interval_default: true,
    };
    // Drain already engaged before the loop starts.
    let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let handle = spawn_role_task(
        runner,
        spec,
        Duration::from_millis(20),
        drain.clone(),
        PathBuf::from("/tmp/loom-test-root"),
        new_in_progress_guard(),
    );

    // Let several tick intervals elapse; not a single invoke may fire.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no role tick may start while draining"
    );

    // Clearing the drain resumes dispatch — proving the gate, not a dead loop.
    drain.store(false, std::sync::atomic::Ordering::SeqCst);
    wait_for_calls(&calls, 1, Duration::from_secs(2)).await;

    handle.abort();
}

#[tokio::test]
async fn test_loop_stops_cleanly_when_runner_panics() {
    struct PanicOnceRunner;
    impl RoleInvocationRunner for PanicOnceRunner {
        fn invoke(&mut self, _role: &str, _prompt: &str) -> RoleTickOutcome {
            panic!("boom");
        }
    }
    let spec = RoleSpec {
        name: "curator",
        prompt: "/loom:curator",
        default_interval_secs: 1,
        interval_default: true,
    };
    let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = spawn_role_task(
        PanicOnceRunner,
        spec,
        Duration::from_millis(20),
        drain,
        PathBuf::from("/tmp/loom-test-root"),
        new_in_progress_guard(),
    );
    let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "loop task should finish (not hang) after the runner panics");
}

// ===================================================================
// DEFAULT_ROLES prompts — regression guard for #4034 (bare `/curator`
// matches no real command; the installed commands are namespaced).
// ===================================================================

#[test]
fn test_default_roles_prompts_are_namespaced() {
    for spec in DEFAULT_ROLES {
        let expected = format!("/loom:{}", spec.name);
        assert_eq!(
            spec.prompt, expected,
            "RoleSpec {:?} prompt must be the namespaced `/loom:<role>` command, not a bare \
                 `/<role>` (see #4034 — a bare prompt matches no installed slash command and \
                 silently no-ops)",
            spec.name
        );
    }
}

// ===================================================================
// Doctor in DEFAULT_ROLES — regression guard for #5272 (before this,
// a `loom:changes-requested` PR whose sweep ended had no role left to
// pick it up standalone, ever).
// ===================================================================

#[test]
fn test_default_roles_includes_doctor_with_no_pr_number() {
    let doctor = DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "doctor")
        .expect("#5272: DEFAULT_ROLES must include doctor as a standalone role");
    assert_eq!(
        doctor.prompt, "/loom:doctor",
        "must invoke Doctor's own Finding Work queue scan, not PR Fix Mode \
             (no PR number appended to the prompt)"
    );
    // Same cadence as `judge` — its paired stage in the PR lifecycle: a
    // fresh Judge rejection should not sit unaddressed materially longer
    // than a fresh Judge review sits unclaimed.
    let judge = DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "judge")
        .expect("judge is default");
    assert_eq!(doctor.default_interval_secs, judge.default_interval_secs);
}

#[test]
fn test_resolve_roles_can_select_doctor_alone() {
    let config = RoleRunnerConfig {
        roles: Some(vec!["doctor".to_string()]),
        ..Default::default()
    };
    let resolved = resolve_roles(&config);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "doctor");
}

// ===================================================================
// Hermit in DEFAULT_ROLES — regression guard for #5601 (before this,
// `hermit` was entirely absent from DEFAULT_ROLES, so naming it in
// `autonomous.roleRunner.roles`/`onIdle` was silently discarded with a
// "not a known standalone role" warning).
// ===================================================================

#[test]
fn test_default_roles_includes_hermit() {
    let hermit = DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "hermit")
        .expect("#5601: DEFAULT_ROLES must include hermit as a standalone role");
    assert_eq!(hermit.prompt, "/loom:hermit");
    // Same cadence as `auditor` — both are proposal-generating roles with
    // no PR/issue-queue argument.
    let auditor = DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "auditor")
        .expect("auditor is default");
    assert_eq!(hermit.default_interval_secs, auditor.default_interval_secs);
}

#[test]
fn test_resolve_roles_can_select_hermit_alone() {
    let config = RoleRunnerConfig {
        roles: Some(vec!["hermit".to_string()]),
        ..Default::default()
    };
    let resolved = resolve_roles(&config);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "hermit");
}

#[test]
fn test_resolve_on_idle_roles_can_select_hermit_alone() {
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["hermit".to_string()]),
        ..Default::default()
    };
    let resolved = resolve_on_idle_roles(&config);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "hermit");
}

// ===================================================================
// Architect in DEFAULT_ROLES, idle-addressable ONLY — regression guards
// for #5656 (before this, `architect` was entirely absent from
// DEFAULT_ROLES, so naming it in `autonomous.roleRunner.onIdle` was
// silently discarded and a repo whose backlog emptied had no mechanism
// to acquire more work). Mirrors the `doctor` (#5272) / `hermit` (#5601)
// guards above, plus the interval-exclusion half that is unique to it.
// ===================================================================

#[test]
fn test_default_roles_includes_architect_as_idle_only() {
    let architect = DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "architect")
        .expect("#5656: DEFAULT_ROLES must include architect so onIdle can resolve it");
    assert_eq!(architect.prompt, "/loom:architect");
    // The load-bearing half: it must NOT be an interval-cadence default.
    assert!(
        !architect.is_interval_default(),
        "#5656: architect must be idle-addressable ONLY — an interval-default architect \
             floods every unpinned repo's backlog with speculative proposals"
    );
    // Every other shipped role is an interval default; architect is the
    // sole carve-out today.
    assert_eq!(
        DEFAULT_ROLES
            .iter()
            .filter(|s| !s.is_interval_default())
            .count(),
        1,
        "a new idle-only role needs its own docs/table update (see daemon-reference.md)"
    );
}

#[test]
fn test_resolve_roles_default_excludes_architect() {
    // The core silent-flood regression: an unset `autonomous.roleRunner.roles`
    // must never put architect on a timer.
    let resolved = resolve_roles(&RoleRunnerConfig::default());
    assert!(
        !resolved.iter().any(|r| r.name == "architect"),
        "#5656: unset `roles` must not dispatch architect on the interval cadence; got {:?}",
        resolved.iter().map(|r| r.name).collect::<Vec<_>>()
    );
}

#[test]
fn test_resolve_on_idle_roles_can_select_architect_alone() {
    // ...while `onIdle` DOES resolve it — the other half of the pair.
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["architect".to_string()]),
        ..Default::default()
    };
    let resolved = resolve_on_idle_roles(&config);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "architect");
    // And naming it in `onIdle` alone still leaves the interval set free
    // of it (`roles` is unset here).
    assert!(!resolve_roles(&config).iter().any(|r| r.name == "architect"));
}

#[test]
fn test_resolve_roles_explicit_allowlist_can_opt_architect_into_the_interval() {
    // Idle-only is the *default*, not a prohibition: a repo that
    // deliberately names architect in `roles` still gets a timer.
    let config = RoleRunnerConfig {
        roles: Some(vec!["architect".to_string()]),
        ..Default::default()
    };
    let resolved = resolve_roles(&config);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "architect");
}

#[test]
fn test_missing_defaults_never_reports_architect() {
    // A pinned allowlist omitting architect is correct, not stale — so it
    // must not be nagged into adding it (which would reintroduce the
    // flood this carve-out prevents).
    let names = vec!["curator".to_string()];
    let missing = missing_defaults(&names);
    assert!(!missing.contains(&"architect"), "expected no \"architect\" in {missing:?}");
    // The #5339 staleness warning still fires for real interval defaults.
    assert!(missing.contains(&"doctor"));
}

#[test]
fn test_missing_defaults_empty_when_list_covers_every_interval_default() {
    let names: Vec<String> = interval_default_roles()
        .iter()
        .map(|s| s.name.to_string())
        .collect();
    assert_eq!(missing_defaults(&names), Vec::<&str>::new());
}

// ===================================================================
// Per-invocation architect proposal cap (#5656) — the actuator
// saturation limit, per-repo configurable rather than a constant.
// ===================================================================

#[test]
#[serial]
fn test_architect_cap_default_when_unset() {
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    assert_eq!(
        resolve_architect_max_proposals(&RoleRunnerConfig::default()),
        DEFAULT_ARCHITECT_MAX_PROPOSALS
    );
}

#[test]
#[serial]
fn test_architect_cap_config_overrides_default() {
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    let config = RoleRunnerConfig {
        architect_max_proposals: Some(9),
        ..Default::default()
    };
    assert_eq!(resolve_architect_max_proposals(&config), 9);
}

#[test]
#[serial]
fn test_architect_cap_env_wins_over_config() {
    let config = RoleRunnerConfig {
        architect_max_proposals: Some(9),
        ..Default::default()
    };
    std::env::set_var(ARCHITECT_MAX_PROPOSALS_ENV, "3");
    let resolved = resolve_architect_max_proposals(&config);
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    assert_eq!(resolved, 3);
}

#[test]
#[serial]
fn test_architect_cap_invalid_env_falls_through_to_config() {
    let config = RoleRunnerConfig {
        architect_max_proposals: Some(7),
        ..Default::default()
    };
    for bad in ["0", "-1", "many", ""] {
        std::env::set_var(ARCHITECT_MAX_PROPOSALS_ENV, bad);
        let resolved = resolve_architect_max_proposals(&config);
        assert_eq!(resolved, 7, "env {bad:?} should have been dropped to the config tier");
    }
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
}

#[test]
#[serial(loom_config_env)]
fn test_read_architect_max_proposals_parses_and_soft_fails() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let ok = tempfile::tempdir().unwrap();
    write_config(ok.path(), r#"{"autonomous": {"roleRunner": {"architectMaxProposals": 12}}}"#);
    assert_eq!(read_role_runner_config(ok.path()).architect_max_proposals, Some(12));

    // Zero / negative / non-integer all soft-fail to None (→ env → default).
    for bad in ["0", "-4", "\"seven\"", "null", "{}"] {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            &format!(r#"{{"autonomous": {{"roleRunner": {{"architectMaxProposals": {bad}}}}}}}"#),
        );
        assert_eq!(
            read_role_runner_config(tmp.path()).architect_max_proposals,
            None,
            "architectMaxProposals={bad} should soft-fail to None"
        );
    }

    // Absent key → None, and the rest of the block still parses.
    let absent = tempfile::tempdir().unwrap();
    write_config(absent.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
    let cfg = read_role_runner_config(absent.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.architect_max_proposals, None);
    assert_eq!(cfg.enabled, Some(true));
}

#[test]
#[serial]
fn test_resolve_role_prompt_carries_the_cap_for_architect_only() {
    std::env::remove_var(ARCHITECT_MAX_PROPOSALS_ENV);
    let architect = DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "architect")
        .expect("architect is shipped");
    // Default cap.
    assert_eq!(
        resolve_role_prompt(architect, &RoleRunnerConfig::default()),
        format!("/loom:architect --max-proposals {DEFAULT_ARCHITECT_MAX_PROPOSALS}")
    );
    // Per-repo override reaches the prompt actually dispatched.
    let config = RoleRunnerConfig {
        architect_max_proposals: Some(7),
        ..Default::default()
    };
    assert_eq!(
        resolve_role_prompt(architect, &config),
        "/loom:architect --max-proposals 7".to_string()
    );
    // Every other role's prompt is byte-for-byte its static spec prompt.
    for spec in DEFAULT_ROLES.iter().filter(|s| s.name != "architect") {
        assert_eq!(resolve_role_prompt(spec, &config), spec.prompt.to_string());
    }
}

// ===================================================================
// tick_is_implausibly_fast — #4034 AC #4 (a no-op success must be
// distinguishable in the log from a real, slower tick).
// ===================================================================

#[test]
fn test_implausibly_fast_success_is_flagged() {
    assert!(tick_is_implausibly_fast(
        &RoleTickOutcome::Success,
        Duration::from_millis(1400) // the observed #4034 incident duration
    ));
}

#[test]
fn test_success_at_or_above_threshold_is_not_flagged() {
    assert!(!tick_is_implausibly_fast(&RoleTickOutcome::Success, IMPLAUSIBLY_FAST_TICK));
    assert!(!tick_is_implausibly_fast(
        &RoleTickOutcome::Success,
        IMPLAUSIBLY_FAST_TICK + Duration::from_secs(60)
    ));
}

#[test]
fn test_failure_is_never_flagged_regardless_of_duration() {
    assert!(!tick_is_implausibly_fast(
        &RoleTickOutcome::Failure("boom".to_string()),
        Duration::from_millis(1)
    ));
}

// ===================================================================
// onIdle config parsing (#4364)
// ===================================================================

// NOTE: see the comment above `test_config_missing_file_is_default` — these
// tests read `read_role_runner_config` too, so they need the same
// private-defaults-tier guard + `#[serial(loom_config_env)]` (#4538).
#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_absent_is_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"enabled": true}}}"#);
    let on_idle = read_role_runner_config(tmp.path()).on_idle;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(on_idle, None);
}

#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_parses_array() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": ["champion"]}}}"#);
    let on_idle = read_role_runner_config(tmp.path()).on_idle;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(on_idle, Some(vec!["champion".to_string()]));
}

#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_non_array_soft_fails_to_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    // A non-array (string) value must not panic — it soft-fails to `None`,
    // matching the `roles` contract.
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": "champion"}}}"#);
    let on_idle = read_role_runner_config(tmp.path()).on_idle;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(on_idle, None);
}

#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_drops_non_string_entries() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    // Non-string entries are dropped; string entries survive (unknown
    // *names* are filtered later in `resolve_on_idle_roles`).
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"onIdle": ["champion", 7, true]}}}"#,
    );
    let on_idle = read_role_runner_config(tmp.path()).on_idle;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(on_idle, Some(vec!["champion".to_string()]));
}

// ===================================================================
// parse_duration_suffix (#7511)
// ===================================================================

#[test]
fn test_parse_duration_suffix_recognizes_every_unit() {
    assert_eq!(parse_duration_suffix("30s"), Some(Duration::from_secs(30)));
    assert_eq!(parse_duration_suffix("90m"), Some(Duration::from_secs(90 * 60)));
    assert_eq!(parse_duration_suffix("24h"), Some(Duration::from_secs(24 * 3600)));
    assert_eq!(parse_duration_suffix("7d"), Some(Duration::from_secs(7 * 86_400)));
}

#[test]
fn test_parse_duration_suffix_trims_whitespace() {
    assert_eq!(parse_duration_suffix("  24h  "), Some(Duration::from_secs(24 * 3600)));
}

#[test]
fn test_parse_duration_suffix_rejects_malformed_values() {
    // Empty / whitespace-only.
    assert_eq!(parse_duration_suffix(""), None);
    assert_eq!(parse_duration_suffix("   "), None);
    // Unknown / missing unit suffix.
    assert_eq!(parse_duration_suffix("24"), None);
    assert_eq!(parse_duration_suffix("24x"), None);
    // Non-numeric leading component.
    assert_eq!(parse_duration_suffix("abch"), None);
    // Compound forms are not supported.
    assert_eq!(parse_duration_suffix("1h30m"), None);
    // Zero is rejected (almost certainly a typo, not "promote every tick").
    assert_eq!(parse_duration_suffix("0h"), None);
    assert_eq!(parse_duration_suffix("0s"), None);
}

// ===================================================================
// RoleRunnerConfig.on_idle_max_wait parsing (#7511)
// ===================================================================

#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_max_wait_absent_is_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": ["hermit"]}}}"#);
    let cfg = read_role_runner_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.on_idle_max_wait, None, "absent key must be zero behavior change");
}

#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_max_wait_parses_valid_entries() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"onIdle": ["hermit", "auditor"], "onIdleMaxWait": {"hermit": "24h", "auditor": "72h"}}}}"#,
    );
    let on_idle_max_wait = read_role_runner_config(tmp.path()).on_idle_max_wait;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    let mut expected = BTreeMap::new();
    expected.insert("hermit".to_string(), Duration::from_secs(24 * 3600));
    expected.insert("auditor".to_string(), Duration::from_secs(72 * 3600));
    assert_eq!(on_idle_max_wait, Some(expected));
}

#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_max_wait_lower_cases_and_trims_keys() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"onIdleMaxWait": {"  Hermit  ": "24h"}}}}"#,
    );
    let on_idle_max_wait = read_role_runner_config(tmp.path()).on_idle_max_wait;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    let mut expected = BTreeMap::new();
    expected.insert("hermit".to_string(), Duration::from_secs(24 * 3600));
    assert_eq!(on_idle_max_wait, Some(expected));
}

#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_max_wait_non_object_soft_fails_to_none() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdleMaxWait": ["hermit"]}}}"#);
    let on_idle_max_wait = read_role_runner_config(tmp.path()).on_idle_max_wait;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(on_idle_max_wait, None);
}

#[test]
#[serial(loom_config_env)]
fn test_config_on_idle_max_wait_drops_only_the_malformed_entry() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    // "auditor"'s value is malformed (no unit suffix) — dropped, but
    // "hermit"'s well-formed entry in the SAME object still parses.
    write_config(
        tmp.path(),
        r#"{"autonomous": {"roleRunner": {"onIdleMaxWait": {"hermit": "24h", "auditor": "72", "": "1h", "guide": 5}}}}"#,
    );
    let on_idle_max_wait = read_role_runner_config(tmp.path()).on_idle_max_wait;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    let mut expected = BTreeMap::new();
    expected.insert("hermit".to_string(), Duration::from_secs(24 * 3600));
    assert_eq!(on_idle_max_wait, Some(expected));
}

// ===================================================================
// resolve_on_idle_roles (#4364)
// ===================================================================

#[test]
fn test_resolve_on_idle_roles_absent_is_empty() {
    // Opposite default from `roles`: absent key means NO idle triggering.
    assert_eq!(resolve_on_idle_roles(&RoleRunnerConfig::default()), Vec::new());
}

#[test]
fn test_resolve_on_idle_roles_parses_and_preserves_order() {
    let config = RoleRunnerConfig {
        enabled: None,
        roles: None,
        interval_secs: None,
        on_idle: Some(vec!["guide".to_string(), "champion".to_string()]),
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    let roles = resolve_on_idle_roles(&config);
    assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion", "guide"]);
}

#[test]
fn test_resolve_on_idle_roles_ignores_unknown_names() {
    let config = RoleRunnerConfig {
        enabled: None,
        roles: None,
        interval_secs: None,
        on_idle: Some(vec![
            "champion".to_string(),
            "builder".to_string(),
            "nope".to_string(),
        ]),
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    let roles = resolve_on_idle_roles(&config);
    assert_eq!(roles.iter().map(|r| r.name).collect::<Vec<_>>(), vec!["champion"]);
}

#[test]
fn test_resolve_on_idle_roles_empty_array_is_empty() {
    let config = RoleRunnerConfig {
        enabled: None,
        roles: None,
        interval_secs: None,
        on_idle: Some(vec![]),
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    assert_eq!(resolve_on_idle_roles(&config), Vec::new());
}

// ===================================================================
// is_overdue / on_idle_role_is_promotable / resolve_on_idle_max_wait_status
// (#7511)
// ===================================================================

fn hermit_spec() -> RoleSpec {
    *DEFAULT_ROLES
        .iter()
        .find(|s| s.name == "hermit")
        .expect("hermit is shipped")
}

#[test]
fn test_is_overdue_missing_tick_is_always_overdue() {
    // "First registration" (no prior tick recorded at all) must be
    // immediately eligible — never permanently exempt.
    assert!(is_overdue(None, Duration::from_secs(1)));
    assert!(is_overdue(None, Duration::from_secs(u64::from(u32::MAX))));
}

#[test]
fn test_is_overdue_boundary_just_under_at_and_just_over() {
    let max_wait = Duration::from_secs(3600);
    assert!(
        !is_overdue(Some(chrono::Duration::seconds(3599)), max_wait),
        "just under is not overdue"
    );
    assert!(
        is_overdue(Some(chrono::Duration::seconds(3600)), max_wait),
        "exactly at the deadline IS overdue (>=)"
    );
    assert!(
        is_overdue(Some(chrono::Duration::seconds(3601)), max_wait),
        "just over is overdue"
    );
}

#[test]
fn test_is_overdue_negative_age_is_not_overdue() {
    // Clock skew: a tick recorded in the "future" reads as fresh, not
    // infinitely stale.
    assert!(!is_overdue(Some(chrono::Duration::seconds(-5)), Duration::from_secs(60)));
}

#[test]
#[serial(role_tick_ring)]
fn test_on_idle_role_is_promotable_first_registration() {
    reset_role_tick_ring();
    let root = Path::new("/tmp/loom-role-runner-7511-a");
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["hermit".to_string()]),
        on_idle_max_wait: Some(BTreeMap::from([("hermit".to_string(), Duration::from_secs(3600))])),
        ..Default::default()
    };
    assert!(
        on_idle_role_is_promotable(&hermit_spec(), &config, root, chrono::Utc::now()),
        "a role that has never ticked at all must be immediately promotable"
    );
}

#[test]
#[serial(role_tick_ring)]
fn test_on_idle_role_is_promotable_respects_the_recorded_tick() {
    reset_role_tick_ring();
    let root = Path::new("/tmp/loom-role-runner-7511-b");
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["hermit".to_string()]),
        on_idle_max_wait: Some(BTreeMap::from([("hermit".to_string(), Duration::from_secs(3600))])),
        ..Default::default()
    };
    let now = chrono::Utc::now();
    // Ticked 30 minutes ago — under the 1h deadline, not yet promotable.
    record_role_tick_at(
        "hermit",
        root,
        &RoleTickOutcome::Success,
        now - chrono::Duration::minutes(30),
    );
    assert!(!on_idle_role_is_promotable(&hermit_spec(), &config, root, now));
    // Ticked 2 hours ago — past the deadline, now promotable.
    record_role_tick_at(
        "hermit",
        root,
        &RoleTickOutcome::Success,
        now - chrono::Duration::hours(2),
    );
    assert!(on_idle_role_is_promotable(&hermit_spec(), &config, root, now));
}

#[test]
fn test_on_idle_role_is_promotable_false_when_not_in_on_idle() {
    // Configured in `onIdleMaxWait` but NOT in `onIdle` — never promotes
    // (promotion is only meaningful for a role already opted into the
    // idle edge; see `RoleRunnerConfig::on_idle_max_wait`'s doc comment).
    let root = Path::new("/tmp/loom-role-runner-7511-c");
    let config = RoleRunnerConfig {
        on_idle: None,
        on_idle_max_wait: Some(BTreeMap::from([("hermit".to_string(), Duration::from_secs(1))])),
        ..Default::default()
    };
    assert!(!on_idle_role_is_promotable(&hermit_spec(), &config, root, chrono::Utc::now()));
}

#[test]
fn test_on_idle_role_is_promotable_false_when_no_max_wait_configured() {
    // In `onIdle` but the key is entirely absent — today's behavior,
    // unaffected.
    let root = Path::new("/tmp/loom-role-runner-7511-d");
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["hermit".to_string()]),
        on_idle_max_wait: None,
        ..Default::default()
    };
    assert!(!on_idle_role_is_promotable(&hermit_spec(), &config, root, chrono::Utc::now()));
}

#[test]
fn test_on_idle_role_is_promotable_false_when_this_role_absent_from_the_map() {
    // `onIdleMaxWait` is configured, but not for THIS role.
    let root = Path::new("/tmp/loom-role-runner-7511-e");
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["hermit".to_string(), "auditor".to_string()]),
        on_idle_max_wait: Some(BTreeMap::from([("auditor".to_string(), Duration::from_secs(1))])),
        ..Default::default()
    };
    assert!(!on_idle_role_is_promotable(&hermit_spec(), &config, root, chrono::Utc::now()));
}

#[test]
#[serial(role_tick_ring)]
fn test_resolve_on_idle_max_wait_status_reports_age_and_promoted() {
    reset_role_tick_ring();
    let root = Path::new("/tmp/loom-role-runner-7511-f");
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["hermit".to_string(), "auditor".to_string()]),
        on_idle_max_wait: Some(BTreeMap::from([
            ("hermit".to_string(), Duration::from_secs(3600)),
            ("auditor".to_string(), Duration::from_secs(3600)),
        ])),
        ..Default::default()
    };
    let now = chrono::Utc::now();
    record_role_tick_at(
        "hermit",
        root,
        &RoleTickOutcome::Success,
        now - chrono::Duration::minutes(30),
    );
    // "auditor" never ticked at all.

    let status = resolve_on_idle_max_wait_status(&config, root, now);
    let hermit = status
        .iter()
        .find(|s| s.role == "hermit")
        .expect("hermit entry present");
    assert_eq!(hermit.max_wait_secs, 3600);
    assert_eq!(hermit.age_secs, Some(30 * 60));
    assert!(!hermit.promoted);

    let auditor = status
        .iter()
        .find(|s| s.role == "auditor")
        .expect("auditor entry present");
    assert_eq!(auditor.age_secs, None);
    assert!(auditor.promoted, "never-ticked role is always promoted (infinitely overdue)");
}

#[test]
fn test_resolve_on_idle_max_wait_status_omits_roles_without_both_keys() {
    // "guide" is in `onIdle` but has no `onIdleMaxWait` entry — omitted.
    // "hermit" has a max-wait but is NOT in `onIdle` — also omitted.
    let root = Path::new("/tmp/loom-role-runner-7511-g");
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["guide".to_string()]),
        on_idle_max_wait: Some(BTreeMap::from([("hermit".to_string(), Duration::from_secs(60))])),
        ..Default::default()
    };
    assert!(resolve_on_idle_max_wait_status(&config, root, chrono::Utc::now()).is_empty());
}

#[test]
fn test_resolve_on_idle_max_wait_status_absent_key_is_empty() {
    let root = Path::new("/tmp/loom-role-runner-7511-h");
    let config = RoleRunnerConfig {
        on_idle: Some(vec!["hermit".to_string()]),
        on_idle_max_wait: None,
        ..Default::default()
    };
    assert!(resolve_on_idle_max_wait_status(&config, root, chrono::Utc::now()).is_empty());
}

// ===================================================================
// IdleTrigger — edge detection + debounce (#4364)
// ===================================================================

#[test]
fn test_idle_trigger_boot_idle_does_not_fire() {
    let mut t = IdleTrigger::new();
    let root = Path::new("/tmp/loom-root-a");
    // First-ever observation is idle: boot on an empty queue must NOT fire.
    assert!(!t.observe_edge(root, true));
}

#[test]
fn test_idle_trigger_fires_on_non_idle_to_idle_edge() {
    let mut t = IdleTrigger::new();
    let root = Path::new("/tmp/loom-root-b");
    // Boot idle (no fire), then busy, then idle => the edge fires exactly on
    // the busy → idle transition.
    assert!(!t.observe_edge(root, true));
    assert!(!t.observe_edge(root, false));
    assert!(t.observe_edge(root, true));
}

#[test]
fn test_idle_trigger_does_not_refire_on_sustained_idle() {
    let mut t = IdleTrigger::new();
    let root = Path::new("/tmp/loom-root-c");
    assert!(!t.observe_edge(root, false)); // busy
    assert!(t.observe_edge(root, true)); // edge
                                         // Staying idle across N further ticks must not re-fire.
    assert!(!t.observe_edge(root, true));
    assert!(!t.observe_edge(root, true));
}

#[test]
fn test_idle_trigger_no_fire_while_in_flight_then_fires_when_drained() {
    let mut t = IdleTrigger::new();
    let root = Path::new("/tmp/loom-root-d");
    // A tick that dispatched nothing but still has in-flight sweeps is
    // non-idle (not empty) — no edge; the edge fires on the later tick where
    // in-flight reaches zero.
    assert!(!t.observe_edge(root, false));
    assert!(!t.observe_edge(root, false));
    assert!(t.observe_edge(root, true));
}

#[test]
fn test_idle_trigger_edge_is_per_root() {
    let mut t = IdleTrigger::new();
    let a = Path::new("/tmp/loom-root-e1");
    let b = Path::new("/tmp/loom-root-e2");
    // Drive root a busy→idle (edge) while b stays idle from boot (no edge).
    assert!(!t.observe_edge(a, false));
    assert!(!t.observe_edge(b, true));
    assert!(t.observe_edge(a, true)); // a fires
    assert!(!t.observe_edge(b, true)); // b never fired
}

#[test]
fn test_idle_trigger_debounce_window() {
    let mut t = IdleTrigger::new();
    let root = Path::new("/tmp/loom-root-f");
    let t0 = Instant::now();
    // Never fired => outside the window.
    assert!(t.debounce_ok(root, "champion", t0));
    t.record_fired(root, "champion", t0);
    // Within 60s => debounced.
    assert!(!t.debounce_ok(root, "champion", t0 + Duration::from_secs(30)));
    assert!(!t.debounce_ok(root, "champion", t0 + Duration::from_secs(59)));
    // At/after 60s => allowed again.
    assert!(t.debounce_ok(root, "champion", t0 + IDLE_TRIGGER_DEBOUNCE));
    assert!(t.debounce_ok(root, "champion", t0 + Duration::from_secs(61)));
    // Debounce is per-role: a different role is unaffected.
    assert!(t.debounce_ok(root, "curator", t0 + Duration::from_secs(1)));
}

// ===================================================================
// RoleRunGuard — in-progress overlap protection (#4364)
// ===================================================================

#[test]
fn test_role_run_guard_blocks_second_acquire_then_releases_on_drop() {
    let set = new_in_progress_guard();
    let root = PathBuf::from("/tmp/loom-root-g");
    let g1 = RoleRunGuard::try_acquire(set.clone(), root.clone(), "champion");
    assert!(g1.is_some(), "first acquire should succeed");
    // Second acquire of the same (root, role) is refused while held.
    assert!(
        RoleRunGuard::try_acquire(set.clone(), root.clone(), "champion").is_none(),
        "second acquire of the same key must be refused"
    );
    // A different role on the same root is independent.
    assert!(RoleRunGuard::try_acquire(set.clone(), root.clone(), "curator").is_some());
    // Dropping the first guard clears the entry — a later acquire succeeds.
    drop(g1);
    assert!(
        RoleRunGuard::try_acquire(set, root, "champion").is_some(),
        "guard must clear its entry on drop"
    );
}

// ===================================================================
// Concurrent role-agent ceiling (#6102)
// ===================================================================

/// The ceiling refuses admission once the process-wide active count reaches
/// it — the bound `autonomous.workFinder.maxConcurrent` never provided,
/// because role agents never pass through work-finder admission.
///
/// Crucially the refusal is counted **across roots**: the incident host had
/// 25 registered workspaces, so a per-root ceiling would have bounded
/// nothing.
#[test]
fn test_admit_refuses_once_ceiling_reached_across_roots() {
    let set = new_in_progress_guard();
    let a = PathBuf::from("/tmp/loom-ceiling-a");
    let b = PathBuf::from("/tmp/loom-ceiling-b");
    let c = PathBuf::from("/tmp/loom-ceiling-c");

    let g1 = RoleRunGuard::admit(set.clone(), a, "champion", 2)
        .into_guard()
        .expect("first admit under a ceiling of 2");
    // A DIFFERENT root and a DIFFERENT role — still counts against the same
    // host-wide budget.
    let g2 = RoleRunGuard::admit(set.clone(), b, "curator", 2)
        .into_guard()
        .expect("second admit under a ceiling of 2");

    match RoleRunGuard::admit(set.clone(), c.clone(), "judge", 2) {
        RoleAdmission::CeilingReached { active, ceiling } => {
            assert_eq!(active, 2, "refusal must report the sampled active count");
            assert_eq!(ceiling, 2, "refusal must report the ceiling it compared against");
        }
        other => panic!("expected CeilingReached, got {other:?}"),
    }

    // Releasing one guard frees exactly one slot.
    drop(g1);
    assert!(
        RoleRunGuard::admit(set.clone(), c, "judge", 2)
            .into_guard()
            .is_some(),
        "a dropped guard must free a slot in the ceiling"
    );
    drop(g2);
}

/// `InProgress` (cadence overlap, #4364) and `CeilingReached` (resource
/// limit, #6102) are distinct outcomes. Conflating them is what made
/// role-agent load invisible: an operator grepping for a skip reason could
/// not tell "this role is already running" from "the host is full".
#[test]
fn test_admit_distinguishes_in_progress_from_ceiling_reached() {
    let set = new_in_progress_guard();
    let root = PathBuf::from("/tmp/loom-ceiling-distinct");
    let _held = RoleRunGuard::admit(set.clone(), root.clone(), "champion", 4)
        .into_guard()
        .expect("first admit");

    // Same (root, role) with headroom to spare ⇒ overlap, not a ceiling hit.
    assert!(
        matches!(
            RoleRunGuard::admit(set.clone(), root.clone(), "champion", 4),
            RoleAdmission::InProgress
        ),
        "same (root, role) while held must report InProgress"
    );
    // Different role, but the ceiling is already met ⇒ ceiling, not overlap.
    assert!(
        matches!(
            RoleRunGuard::admit(set, root, "curator", 1),
            RoleAdmission::CeilingReached {
                active: 1,
                ceiling: 1
            }
        ),
        "a full ceiling must report CeilingReached, not InProgress"
    );
}

/// `try_acquire` keeps its pre-#6102 unbounded behavior, so the #4364
/// overlap contract is unchanged for every caller that has no ceiling.
#[test]
fn test_try_acquire_remains_unbounded() {
    let set = new_in_progress_guard();
    let mut held = Vec::new();
    for (i, role) in ["champion", "curator", "judge", "doctor", "guide"]
        .iter()
        .enumerate()
    {
        let g = RoleRunGuard::try_acquire(set.clone(), PathBuf::from(format!("/tmp/r{i}")), role);
        assert!(g.is_some(), "try_acquire must not apply any ceiling");
        held.push(g);
    }
    assert_eq!(active_run_count(&set), 5);
}

/// The shipped default is derived from the interval-default role table, not
/// hard-coded — so adding a role raises the ceiling by one instead of
/// silently squeezing every other role.
#[test]
fn test_default_max_concurrent_is_derived_from_interval_default_roles() {
    assert_eq!(default_max_concurrent(), interval_default_roles().len());
    assert!(default_max_concurrent() >= 1, "a 0 ceiling would admit nothing");
}

#[test]
#[serial(loom_config_env)]
fn test_config_max_concurrent_parses_and_rejects_zero() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"maxConcurrent": 3}}}"#);
    let parsed = read_role_runner_config(tmp.path()).max_concurrent;

    let tmp0 = tempfile::tempdir().unwrap();
    // 0 soft-fails to `None` (falls through to env/default) rather than
    // being honored as "admit nothing" — that is `enabled: false`.
    write_config(tmp0.path(), r#"{"autonomous": {"roleRunner": {"maxConcurrent": 0}}}"#);
    let zero = read_role_runner_config(tmp0.path()).max_concurrent;

    let tmp_bad = tempfile::tempdir().unwrap();
    write_config(tmp_bad.path(), r#"{"autonomous": {"roleRunner": {"maxConcurrent": "8"}}}"#);
    let bad = read_role_runner_config(tmp_bad.path()).max_concurrent;
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);

    assert_eq!(parsed, Some(3));
    assert_eq!(zero, None, "0 must soft-fail to None");
    assert_eq!(bad, None, "a non-integer must soft-fail to None");
}

#[test]
#[serial(loom_role_runner_max_concurrent_env)]
fn test_resolve_max_concurrent_precedence_env_over_config_over_default() {
    std::env::remove_var(ROLE_RUNNER_MAX_CONCURRENT_ENV);
    let unset = RoleRunnerConfig::default();
    let configured = RoleRunnerConfig {
        max_concurrent: Some(2),
        on_idle_max_wait: None,
        ..RoleRunnerConfig::default()
    };
    assert_eq!(resolve_max_concurrent(&unset), default_max_concurrent());
    assert_eq!(resolve_max_concurrent(&configured), 2);

    std::env::set_var(ROLE_RUNNER_MAX_CONCURRENT_ENV, "9");
    assert_eq!(resolve_max_concurrent(&configured), 9, "env must outrank config");

    // A zero / unparseable env value drops to the next tier rather than
    // being honored — same contract as `architectMaxProposals`.
    std::env::set_var(ROLE_RUNNER_MAX_CONCURRENT_ENV, "0");
    assert_eq!(resolve_max_concurrent(&configured), 2);
    std::env::set_var(ROLE_RUNNER_MAX_CONCURRENT_ENV, "lots");
    assert_eq!(resolve_max_concurrent(&unset), default_max_concurrent());
    std::env::remove_var(ROLE_RUNNER_MAX_CONCURRENT_ENV);
}

/// The status surface's read path (#6102 AC3): after the daemon registers
/// its guard, `global_active_run_count` tracks live role agents — this is
/// the number `loom-daemon status` reports next to in-flight sweeps, and
/// the number that previously required `pgrep` to obtain.
///
/// `#[serial]` because `GLOBAL_IN_PROGRESS` is a process-wide `OnceLock`:
/// this is the only test that registers it, and it must not race a
/// concurrent reader.
#[test]
#[serial(loom_role_runner_global_guard)]
fn test_global_active_run_count_tracks_registered_guard() {
    // Unregistered (or before this process registers) reads as 0 rather
    // than panicking — the contract `calibrate` and every non-daemon
    // process rely on.
    let set = new_in_progress_guard();
    register_global_in_progress(set.clone());
    assert_eq!(global_active_run_count(), 0, "an empty guard reads as 0");

    let g =
        RoleRunGuard::admit(set.clone(), PathBuf::from("/tmp/loom-global-count"), "champion", 4)
            .into_guard()
            .expect("admit under the ceiling");
    assert_eq!(global_active_run_count(), 1, "a live role agent must be visible to status");
    drop(g);
    assert_eq!(global_active_run_count(), 0, "the count must fall as agents finish");
}

// ===================================================================
// invoke_with_collision_probe — cross-host collision detection (#4623)
// ===================================================================

/// A runner that records every `(role, prompt)` it was asked to invoke and
/// returns a scripted outcome.
struct RecordingRunner {
    calls: Vec<(String, String)>,
    outcome: RoleTickOutcome,
}

impl RoleInvocationRunner for RecordingRunner {
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
        self.calls.push((role.to_string(), prompt.to_string()));
        self.outcome.clone()
    }
}

#[test]
#[serial(loom_config_env)]
fn test_collision_probe_wrapper_is_transparent_to_the_invocation() {
    // Detection is opt-in and default-off: with it disabled the wrapper
    // must pass the invocation through byte-for-byte (same role, same
    // prompt, same outcome) and make no forge call.
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    std::env::remove_var(crate::role_collision::ROLE_COLLISION_DETECT_ENV);
    std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let mut runner = RecordingRunner {
        calls: Vec::new(),
        outcome: RoleTickOutcome::Failure("boom".into()),
    };
    let outcome = invoke_with_collision_probe(
        &mut runner,
        tmp.path(),
        "champion",
        "/loom:champion",
        Duration::from_secs(600),
    );
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(outcome, RoleTickOutcome::Failure("boom".into()));
    assert_eq!(runner.calls, vec![("champion".to_string(), "/loom:champion".to_string())]);
}

#[test]
#[serial(loom_config_env)]
fn test_collision_probe_wrapper_records_the_self_run_window() {
    // The baseline the NEXT tick attributes foreign forge activity
    // against: the wrapper must open and close a self-run window around
    // every invocation, even a failing one, and even with detection off
    // (so enabling it mid-run has a baseline immediately).
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    std::env::remove_var(crate::role_collision::ROLE_COLLISION_DETECT_ENV);
    std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let mut runner = RecordingRunner {
        calls: Vec::new(),
        outcome: RoleTickOutcome::Failure("boom".into()),
    };
    let before = chrono::Utc::now();
    let _ = invoke_with_collision_probe(
        &mut runner,
        tmp.path(),
        "guide",
        "/loom:guide",
        Duration::from_secs(900),
    );
    let after = chrono::Utc::now();
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    let window = crate::role_collision::last_self_run(tmp.path(), "guide")
        .expect("a self-run window must be recorded");
    assert!(window.started >= before && window.started <= after);
    let ended = window
        .ended
        .expect("the window must be closed after the invocation");
    assert!(ended >= window.started && ended <= after);
}

// ===================================================================
// plan_idle_runs — the composed edge/drain/enabled/debounce/guard decision
// ===================================================================

fn on_idle_config(enabled: Option<bool>, roles: Vec<&str>) -> RoleRunnerConfig {
    RoleRunnerConfig {
        enabled,
        roles: None,
        interval_secs: None,
        on_idle: Some(roles.into_iter().map(str::to_string).collect()),
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    }
}

#[test]
#[serial]
fn test_plan_idle_runs_fires_on_edge_when_enabled() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-a");
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let now = Instant::now();
    // Boot idle: no edge, so no plan.
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
    // Go busy: no edge.
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    // Busy → idle edge: champion fires (and its guard is now held).
    let plan = plan_idle_runs(&mut t, &set, root, &cfg, true, false, now);
    assert_eq!(plan.iter().map(|(s, _)| s.name).collect::<Vec<_>>(), vec!["champion"]);
}

#[test]
#[serial]
fn test_plan_idle_runs_drain_suppresses() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-b");
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let now = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    // Edge present, but draining => suppressed.
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, true, now).is_empty());
}

#[test]
#[serial]
fn test_plan_idle_runs_disabled_suppresses() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-c");
    let cfg = on_idle_config(Some(false), vec!["champion"]);
    let now = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    // Edge present, but role runner disabled => no fire.
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
    // #4377: onIdle is configured for this root, so the disabled-suppression
    // must be observable, not silent.
    assert!(t.disabled_warned(root), "onIdle configured + disabled must record a warning");
}

// ===================================================================
// #4377 — idle-path disabled-suppression is visible, not silent
// ===================================================================

#[test]
#[serial]
fn test_plan_idle_runs_disabled_without_on_idle_does_not_warn() {
    // A root with no `onIdle` roles configured is disabled in its normal,
    // unconfigured state — not a misconfiguration, so no warning.
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-no-onidle");
    let cfg = RoleRunnerConfig {
        enabled: Some(false),
        roles: None,
        interval_secs: None,
        on_idle: None,
        model: None,
        role_models: BTreeMap::new(),
        effort: None,
        role_efforts: BTreeMap::new(),
        architect_max_proposals: None,
        max_concurrent: None,
        on_idle_max_wait: None,
    };
    let now = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
    assert!(
        !t.disabled_warned(root),
        "no onIdle configured => disabled is normal, must not warn"
    );
}

#[test]
#[serial]
fn test_plan_idle_runs_disabled_warning_dedupes_across_repeated_edges() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-dedupe");
    let cfg = on_idle_config(Some(false), vec!["champion"]);
    let t0 = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0).is_empty());
    // First edge: disabled, onIdle configured => warns.
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0).is_empty());
    assert!(t.disabled_warned(root));
    // Flap busy -> idle again: still disabled; the warning stays deduped
    // (no observable way to detect a re-warn other than the state not
    // regressing — the log line itself is the thing that must not repeat).
    assert!(
        plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0 + Duration::from_secs(5))
            .is_empty()
    );
    assert!(
        plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(10))
            .is_empty()
    );
    assert!(t.disabled_warned(root), "still deduped on the second edge");
}

#[test]
#[serial]
fn test_plan_idle_runs_disabled_warning_clears_once_enabled() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-clears");
    let disabled_cfg = on_idle_config(Some(false), vec!["champion"]);
    let enabled_cfg = on_idle_config(Some(true), vec!["champion"]);
    let t0 = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &disabled_cfg, false, false, t0).is_empty());
    assert!(plan_idle_runs(&mut t, &set, root, &disabled_cfg, true, false, t0).is_empty());
    assert!(t.disabled_warned(root));

    // Root flips to enabled (hot-apply) well outside the debounce window.
    assert!(plan_idle_runs(
        &mut t,
        &set,
        root,
        &enabled_cfg,
        false,
        false,
        t0 + Duration::from_secs(70)
    )
    .is_empty());
    let fire =
        plan_idle_runs(&mut t, &set, root, &enabled_cfg, true, false, t0 + Duration::from_secs(80));
    assert_eq!(fire.len(), 1, "enabled root must fire normally");
    assert!(
        !t.disabled_warned(root),
        "warned flag must clear once the root resolves enabled"
    );
}

// ===================================================================
// #6470 — idle-edge WARN names the true cause (env vs config) and
// collapses the per-root #4377 warning to one host-level line when the
// host-wide LOOM_ROLE_RUNNER env override is what disabled it.
// ===================================================================

#[test]
#[serial]
fn test_plan_idle_runs_env_disabled_warns_host_level_not_per_root() {
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-env-disabled");
    // This root's OWN config says enabled — the env override still wins
    // and is the true cause, not this root's config.
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let now = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty());
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    // The env-override branch records the HOST-level dedup, not the
    // per-root #4377 one — the per-root cause was never the reason.
    assert!(t.host_env_warned(), "env-caused disable must record the host-level warning");
    assert!(
        !t.disabled_warned(root),
        "env-caused disable must NOT record the per-root #4377 warning"
    );
}

#[test]
#[serial]
fn test_plan_idle_runs_env_disabled_collapses_across_multiple_roots() {
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root_a = Path::new("/tmp/loom-plan-env-a");
    let root_b = Path::new("/tmp/loom-plan-env-b");
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let now = Instant::now();
    // Both roots boot idle (no edge yet).
    assert!(plan_idle_runs(&mut t, &set, root_a, &cfg, false, false, now).is_empty());
    assert!(plan_idle_runs(&mut t, &set, root_b, &cfg, false, false, now).is_empty());
    // Root A's idle edge fires the (one-time) host-level warning.
    assert!(plan_idle_runs(&mut t, &set, root_a, &cfg, true, false, now).is_empty());
    assert!(t.host_env_warned());
    // Root B's idle edge, same host-wide cause: must NOT warn again —
    // `warn_if_idle_configured_but_disabled` is a no-op the second time,
    // regardless of which root triggers it (this is the whole point of
    // collapsing to a single host-level line instead of N per-root ones).
    assert!(plan_idle_runs(&mut t, &set, root_b, &cfg, true, false, now).is_empty());
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    assert!(
        !t.disabled_warned(root_a) && !t.disabled_warned(root_b),
        "env-caused disable never records the per-root dedup for either root"
    );
}

#[test]
#[serial]
fn test_plan_idle_runs_env_disabled_warning_clears_once_reenabled() {
    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-env-clears");
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let t0 = Instant::now();
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0).is_empty());
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0).is_empty());
    assert!(t.host_env_warned());

    // The env override clears (host re-enabled) well outside the
    // debounce window — this root's own config (`enabled: true`) now
    // decides, and it fires normally.
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    assert!(
        plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0 + Duration::from_secs(70))
            .is_empty()
    );
    let fire = plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(80));
    assert_eq!(fire.len(), 1, "root must fire once the env override clears");
    assert!(
        !t.host_env_warned(),
        "host-level warned flag must clear once the override is no longer disabling"
    );
}

/// Cross-config case (#4377 curated AC): a target root has `onIdle` set
/// but its own per-root `enabled` is absent (resolves `false`) —
/// independent of whatever the daemon's own workspace's master switch is
/// set to (the master switch only decides whether these loops start at
/// all, never a target root's own gate). `observe_and_fire_idle` is the
/// real entry point the work-finder loop calls, reading the root's own
/// on-disk config each tick — exercised here end-to-end rather than via
/// the already-parsed `RoleRunnerConfig` the other tests use.
// NOTE: see the comment above `test_config_missing_file_is_default` — this
// test's `observe_and_fire_idle` calls read the private-defaults tier via
// `read_role_runner_config` too, so it needs the same guard +
// `#[serial(loom_config_env)]` (#4593, discovered during review of #4590 /
// #4538).
#[test]
#[serial(loom_config_env)]
fn test_observe_and_fire_idle_cross_config_disabled_target_root_warns_and_suppresses() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"onIdle": ["champion"]}}}"#);
    let mut trigger = IdleTrigger::new();
    let in_progress = new_in_progress_guard();

    observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false); // boot idle: no edge
    observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), false, false); // go busy: no edge
    observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false); // busy -> idle edge

    assert!(
        trigger.disabled_warned(tmp.path()),
        "idle edge on a disabled-but-onIdle-configured root must record the warning"
    );
    assert!(
        in_progress.lock().unwrap().is_empty(),
        "a disabled root must never acquire/fire a run"
    );

    // A second flap must stay deduped — no panic, no re-fire, warned state
    // holds (this is the "second edge does not re-warn" acceptance case).
    observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), false, false);
    observe_and_fire_idle(&mut trigger, &in_progress, tmp.path(), true, false);
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert!(trigger.disabled_warned(tmp.path()));
    assert!(in_progress.lock().unwrap().is_empty());
}

// ===================================================================
// #4377 — interval-path disabled-root warn-once dedup
// ===================================================================

#[test]
fn test_should_warn_disabled_root_warns_once_then_dedupes_until_reenable() {
    let mut warned: HashSet<PathBuf> = HashSet::new();
    let root = PathBuf::from("/tmp/loom-interval-disabled-root");
    assert!(
        should_warn_disabled_root(&mut warned, &root),
        "first sighting of a disabled root must warn"
    );
    assert!(
        !should_warn_disabled_root(&mut warned, &root),
        "repeat sighting must be deduped (downgraded to DEBUG by the caller)"
    );
    assert!(
        !should_warn_disabled_root(&mut warned, &root),
        "stays deduped across further ticks"
    );
    // Caller clears the entry once the root resolves enabled again.
    warned.remove(&root);
    assert!(
        should_warn_disabled_root(&mut warned, &root),
        "a re-disable after a re-enable must warn again"
    );
}

#[test]
#[serial]
fn test_plan_idle_runs_debounced_second_edge_then_fires_after_window() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-d");
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let t0 = Instant::now();
    // First edge fires and records the debounce timestamp.
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0).is_empty());
    let first = plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0);
    assert_eq!(first.len(), 1);
    drop(first); // release the guard so only debounce can block the next edge
                 // Flap busy→idle again within 60s: edge present but debounced.
    assert!(
        plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0 + Duration::from_secs(10))
            .is_empty()
    );
    let debounced =
        plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(20));
    assert!(debounced.is_empty(), "second edge within 60s must be debounced");
    // Flap again after the window: fires.
    assert!(
        plan_idle_runs(&mut t, &set, root, &cfg, false, false, t0 + Duration::from_secs(70))
            .is_empty()
    );
    let after = plan_idle_runs(&mut t, &set, root, &cfg, true, false, t0 + Duration::from_secs(80));
    assert_eq!(after.len(), 1, "edge after the debounce window must fire");
}

#[test]
#[serial]
fn test_plan_idle_runs_skips_when_guard_already_held() {
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
    let mut t = IdleTrigger::new();
    let set = new_in_progress_guard();
    let root = Path::new("/tmp/loom-plan-e");
    let cfg = on_idle_config(Some(true), vec!["champion"]);
    let now = Instant::now();
    // Simulate an interval run already holding the guard for (root, champion).
    let _held = RoleRunGuard::try_acquire(set.clone(), root.to_path_buf(), "champion");
    assert!(plan_idle_runs(&mut t, &set, root, &cfg, false, false, now).is_empty());
    // Edge present, but the guard is held by the interval run => idle skips.
    assert!(
        plan_idle_runs(&mut t, &set, root, &cfg, true, false, now).is_empty(),
        "idle trigger must skip while an interval run holds the guard"
    );
}

// ===================================================================
// Interval loop honors the shared in-progress guard (#4364)
// ===================================================================

/// A pre-held guard for (root, role) makes the interval loop skip every
/// tick (0 invokes); clearing it resumes dispatch — proving the interval
/// path also respects the shared guard, so an idle-triggered run in
/// progress cannot be overlapped by an interval tick.
#[tokio::test]
async fn test_interval_loop_skips_while_guard_held() {
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let runner = FakeRunner {
        outcomes: vec![RoleTickOutcome::Success; 3],
        calls: calls.clone(),
    };
    let spec = RoleSpec {
        name: "champion",
        prompt: "/loom:champion",
        default_interval_secs: 1,
        interval_default: true,
    };
    let root = PathBuf::from("/tmp/loom-interval-guard");
    let in_progress = new_in_progress_guard();
    // Pre-hold the guard for (root, champion) so the loop cannot acquire it.
    in_progress
        .lock()
        .unwrap()
        .insert((root.clone(), "champion"));
    let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = spawn_role_task(
        runner,
        spec,
        Duration::from_millis(20),
        drain,
        root.clone(),
        in_progress.clone(),
    );

    // Several intervals elapse; not a single invoke may fire.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "interval tick must skip while the shared guard is held"
    );

    // Release the guard — dispatch resumes, proving the gate (not a dead loop).
    in_progress.lock().unwrap().remove(&(root, "champion"));
    wait_for_calls(&calls, 1, Duration::from_secs(2)).await;

    handle.abort();
}

// ===================================================================
// classify_root_tick_log / log_outcome_for_root_deduped — #4349 state-
// change log dedup: a repeatedly failing root logs once on the fail
// edge and once on recovery, not once per tick.
// ===================================================================

const NORMAL_TICK: Duration = Duration::from_secs(90);

#[test]
fn test_classify_first_failure_is_edge() {
    assert_eq!(
        classify_root_tick_log(
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            false,
            false,
            false,
            false
        ),
        RootTickLogAction::FailureEdge
    );
}

#[test]
fn test_classify_repeat_failure_is_downgraded() {
    assert_eq!(
        classify_root_tick_log(
            &RoleTickOutcome::Failure("boom".into()),
            NORMAL_TICK,
            true,
            false,
            false,
            false
        ),
        RootTickLogAction::FailureRepeat
    );
}

#[test]
fn test_classify_success_after_failure_is_recovery() {
    assert_eq!(
        classify_root_tick_log(&RoleTickOutcome::Success, NORMAL_TICK, true, false, false, false),
        RootTickLogAction::Recovered
    );
}

#[test]
fn test_classify_steady_state_success_is_plain() {
    assert_eq!(
        classify_root_tick_log(&RoleTickOutcome::Success, NORMAL_TICK, false, false, false, false),
        RootTickLogAction::Success
    );
}

#[test]
fn test_classify_implausibly_fast_variants() {
    assert_eq!(
        classify_root_tick_log(
            &RoleTickOutcome::Success,
            Duration::from_millis(100),
            false,
            false,
            false,
            false
        ),
        RootTickLogAction::SuccessImplausiblyFast
    );
    assert_eq!(
        classify_root_tick_log(
            &RoleTickOutcome::Success,
            Duration::from_millis(100),
            true,
            false,
            false,
            false
        ),
        RootTickLogAction::RecoveredImplausiblyFast
    );
}

// ---- no-token-pool classification (#4642) -------------------------

#[test]
fn test_classify_first_no_token_pool_is_edge() {
    assert_eq!(
        classify_root_tick_log(
            &RoleTickOutcome::NoTokenPool,
            NORMAL_TICK,
            false,
            false,
            false,
            false
        ),
        RootTickLogAction::NoTokenPoolEdge
    );
}

#[test]
fn test_classify_repeat_no_token_pool_is_downgraded() {
    assert_eq!(
        classify_root_tick_log(
            &RoleTickOutcome::NoTokenPool,
            NORMAL_TICK,
            false,
            true,
            false,
            false
        ),
        RootTickLogAction::NoTokenPoolRepeat
    );
}

#[test]
fn test_classify_no_token_pool_is_independent_of_failing_state() {
    // A root that was previously `Failure`-failing must not have its
    // no-token-pool skip demoted to `Repeat` just because `was_failing`
    // is true — the two conditions are tracked on separate axes.
    assert_eq!(
        classify_root_tick_log(
            &RoleTickOutcome::NoTokenPool,
            NORMAL_TICK,
            true,
            false,
            false,
            false
        ),
        RootTickLogAction::NoTokenPoolEdge
    );
}

#[test]
fn test_root_tick_log_action_no_token_pool_is_not_failing() {
    // #4642: a no-token-pool skip must never contribute to the
    // Failure/RuntimeRejected tally.
    assert!(!RootTickLogAction::NoTokenPoolEdge.is_failing());
    assert!(!RootTickLogAction::NoTokenPoolRepeat.is_failing());
    assert!(RootTickLogAction::NoTokenPoolEdge.is_no_token_pool());
    assert!(RootTickLogAction::NoTokenPoolRepeat.is_no_token_pool());
    assert!(!RootTickLogAction::FailureEdge.is_no_token_pool());
    assert!(!RootTickLogAction::FailureRepeat.is_no_token_pool());
}

// ---- pool-exhausted classification (#7607) -------------------------

fn pool_exhausted_outcome() -> RoleTickOutcome {
    RoleTickOutcome::PoolExhausted {
        total: 3,
        next_clear_at: chrono::Utc::now() + chrono::Duration::seconds(60),
    }
}

#[test]
fn test_classify_first_pool_exhausted_is_edge() {
    assert_eq!(
        classify_root_tick_log(&pool_exhausted_outcome(), NORMAL_TICK, false, false, false, false),
        RootTickLogAction::PoolExhaustedEdge
    );
}

#[test]
fn test_classify_repeat_pool_exhausted_is_downgraded() {
    assert_eq!(
        classify_root_tick_log(&pool_exhausted_outcome(), NORMAL_TICK, false, false, true, false),
        RootTickLogAction::PoolExhaustedRepeat
    );
}

#[test]
fn test_classify_pool_exhausted_is_independent_of_other_axes() {
    // A root previously `Failure`-failing OR previously no-token-pool
    // must not have its pool-exhausted skip demoted to `Repeat` just
    // because one of the OTHER axes is `true` — all axes are tracked
    // independently.
    assert_eq!(
        classify_root_tick_log(&pool_exhausted_outcome(), NORMAL_TICK, true, false, false, false),
        RootTickLogAction::PoolExhaustedEdge
    );
    assert_eq!(
        classify_root_tick_log(&pool_exhausted_outcome(), NORMAL_TICK, false, true, false, false),
        RootTickLogAction::PoolExhaustedEdge
    );
}

#[test]
fn test_root_tick_log_action_pool_exhausted_is_not_failing_or_no_token_pool() {
    // #7607: a pool-exhausted skip must never contribute to the
    // Failure/RuntimeRejected tally, nor to the NoTokenPool tally.
    assert!(!RootTickLogAction::PoolExhaustedEdge.is_failing());
    assert!(!RootTickLogAction::PoolExhaustedRepeat.is_failing());
    assert!(!RootTickLogAction::PoolExhaustedEdge.is_no_token_pool());
    assert!(!RootTickLogAction::PoolExhaustedRepeat.is_no_token_pool());
    assert!(RootTickLogAction::PoolExhaustedEdge.is_pool_exhausted());
    assert!(RootTickLogAction::PoolExhaustedRepeat.is_pool_exhausted());
    assert!(!RootTickLogAction::FailureEdge.is_pool_exhausted());
    assert!(!RootTickLogAction::NoTokenPoolEdge.is_pool_exhausted());
}

// ---- #6614 empty-pool brake feed from role ticks (#7607) -----------

/// Recording [`PoolExhaustedObserver`] — captures every `(root, role)` it
/// is notified about so the feed predicate can be asserted without a
/// registry, a reaper, or a real tick loop.
#[derive(Default)]
struct RecordingObserver {
    seen: std::sync::Mutex<Vec<(PathBuf, String)>>,
}

impl PoolExhaustedObserver for RecordingObserver {
    fn note_pool_exhausted(&self, root: &Path, role: &str) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((root.to_path_buf(), role.to_string()));
    }
}

#[test]
fn pool_exhausted_outcomes_feed_the_observer_and_nothing_else_does() {
    let observer = RecordingObserver::default();
    let root = PathBuf::from("/r/loom");

    // Every non-PoolExhausted outcome — including the adjacent #4642
    // "no pool at all" skip, whose remedy is `tokens bootstrap`, not the
    // self-healing wait an exhausted pool implies — must not feed it.
    for outcome in [
        RoleTickOutcome::Success,
        RoleTickOutcome::NoTokenPool,
        RoleTickOutcome::Failure("boom".into()),
        RoleTickOutcome::LoadSkipped {
            load_per_core: 9.0,
            detail: "busy".into(),
        },
    ] {
        feed_pool_exhausted_observer(Some(&observer), &outcome, &root, "champion");
    }
    assert!(observer.seen.lock().unwrap().is_empty());

    feed_pool_exhausted_observer(Some(&observer), &pool_exhausted_outcome(), &root, "champion");
    assert_eq!(*observer.seen.lock().unwrap(), vec![(root.clone(), "champion".to_string())]);

    // Deliberately NOT deduped here: the brake needs every observation to
    // keep its entry inside the trailing window while the pool stays dry.
    feed_pool_exhausted_observer(Some(&observer), &pool_exhausted_outcome(), &root, "champion");
    assert_eq!(observer.seen.lock().unwrap().len(), 2);
}

#[test]
fn a_none_observer_is_a_silent_no_op() {
    // A deployment with no sweep registry wired in must run role loops
    // exactly as before, never panicking on the absent feed (#7607).
    feed_pool_exhausted_observer(None, &pool_exhausted_outcome(), Path::new("/r/loom"), "champion");
}

// ---- model/runtime mismatch classification (#5028) -----------------

fn mismatch_outcome() -> RoleTickOutcome {
    RoleTickOutcome::ModelRuntimeMismatch(ModelRuntimeMismatch {
        role: "judge".to_string(),
        runtime: "codex".to_string(),
        model: "sonnet".to_string(),
        model_source: "default".to_string(),
        reason: "runtime \"codex\" only accepts an OpenAI/Codex model but got \"sonnet\""
            .to_string(),
    })
}

#[test]
fn test_classify_first_model_mismatch_is_edge() {
    assert_eq!(
        classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, false, false, false),
        RootTickLogAction::ModelMismatchEdge
    );
}

#[test]
fn test_classify_repeat_model_mismatch_is_downgraded() {
    assert_eq!(
        classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, false, false, true),
        RootTickLogAction::ModelMismatchRepeat
    );
}

#[test]
fn test_classify_model_mismatch_is_independent_of_failing_and_no_token_pool_state() {
    // A root previously `Failure`-failing OR previously no-token-pool must
    // not have its model-mismatch skip demoted to `Repeat` just because
    // one of the OTHER axes is `true` — all axes are tracked
    // independently.
    assert_eq!(
        classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, true, false, false, false),
        RootTickLogAction::ModelMismatchEdge
    );
    assert_eq!(
        classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, true, false, false),
        RootTickLogAction::ModelMismatchEdge
    );
    assert_eq!(
        classify_root_tick_log(&mismatch_outcome(), NORMAL_TICK, false, false, true, false),
        RootTickLogAction::ModelMismatchEdge
    );
}

#[test]
fn test_root_tick_log_action_model_mismatch_is_not_failing_or_no_token_pool() {
    // #5028: a model-mismatch skip must never contribute to the
    // Failure/RuntimeRejected tally, nor to the NoTokenPool tally.
    assert!(!RootTickLogAction::ModelMismatchEdge.is_failing());
    assert!(!RootTickLogAction::ModelMismatchRepeat.is_failing());
    assert!(!RootTickLogAction::ModelMismatchEdge.is_no_token_pool());
    assert!(!RootTickLogAction::ModelMismatchRepeat.is_no_token_pool());
    assert!(RootTickLogAction::ModelMismatchEdge.is_model_mismatch());
    assert!(RootTickLogAction::ModelMismatchRepeat.is_model_mismatch());
    assert!(!RootTickLogAction::FailureEdge.is_model_mismatch());
    assert!(!RootTickLogAction::NoTokenPoolEdge.is_model_mismatch());
}

#[test]
#[serial(role_tick_ring)]
fn test_log_outcome_for_root_deduped_tracks_failing_state_across_ticks() {
    let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test");
    let mut failing: HashMap<PathBuf, bool> = HashMap::new();
    let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
    let mut pool_exhausted: HashMap<PathBuf, bool> = HashMap::new();
    let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

    // Tick 1: failure -> edge, marks failing.
    log_outcome_for_root_deduped(
        "champion",
        &root,
        &RoleTickOutcome::Failure("MCP_PREFLIGHT_FAILED".into()),
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(failing.get(&root), Some(&true));

    // Ticks 2-4: identical repeat failures -> still marked failing (the
    // dedup happens in the log call, not observable here directly, but
    // the state must remain `true` without ever clearing).
    for _ in 0..3 {
        log_outcome_for_root_deduped(
            "champion",
            &root,
            &RoleTickOutcome::Failure("MCP_PREFLIGHT_FAILED".into()),
            NORMAL_TICK,
            &mut failing,
            &mut no_token_pool,
            &mut pool_exhausted,
            &mut model_mismatch,
        );
        assert_eq!(failing.get(&root), Some(&true));
    }

    // Tick 5: recovers -> state flips back to healthy.
    log_outcome_for_root_deduped(
        "champion",
        &root,
        &RoleTickOutcome::Success,
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(failing.get(&root), Some(&false));

    // Tick 6: steady-state success keeps it healthy.
    log_outcome_for_root_deduped(
        "champion",
        &root,
        &RoleTickOutcome::Success,
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(failing.get(&root), Some(&false));
}

#[test]
#[serial(role_tick_ring)]
fn test_log_outcome_for_root_deduped_is_independent_per_root() {
    // A failure on one registered root must not affect another root's
    // failing state (each workspace's health is tracked independently).
    let root_a = PathBuf::from("/tmp/root-a");
    let root_b = PathBuf::from("/tmp/root-b");
    let mut failing: HashMap<PathBuf, bool> = HashMap::new();
    let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
    let mut pool_exhausted: HashMap<PathBuf, bool> = HashMap::new();
    let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

    log_outcome_for_root_deduped(
        "curator",
        &root_a,
        &RoleTickOutcome::Failure("boom".into()),
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    log_outcome_for_root_deduped(
        "curator",
        &root_b,
        &RoleTickOutcome::Success,
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );

    assert_eq!(failing.get(&root_a), Some(&true));
    assert_eq!(failing.get(&root_b), Some(&false));
}

#[test]
#[serial(role_tick_ring)]
fn test_log_outcome_for_root_deduped_no_token_pool_tracked_independently_of_failing() {
    // #4642: a NoTokenPool tick must never mark `failing` true, and a
    // real Failure tick must never mark `no_token_pool` true — the two
    // maps are independent axes even for the SAME root.
    let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-2");
    let mut failing: HashMap<PathBuf, bool> = HashMap::new();
    let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
    let mut pool_exhausted: HashMap<PathBuf, bool> = HashMap::new();
    let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

    log_outcome_for_root_deduped(
        "auditor",
        &root,
        &RoleTickOutcome::NoTokenPool,
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(no_token_pool.get(&root), Some(&true));
    assert_eq!(failing.get(&root), Some(&false));

    // A subsequent real failure must still log as a fresh `FailureEdge`
    // (not `FailureRepeat`) even though the root was just skipped for no
    // token pool — proving the two states never cross-contaminate.
    log_outcome_for_root_deduped(
        "auditor",
        &root,
        &RoleTickOutcome::Failure("boom".into()),
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(failing.get(&root), Some(&true));
    assert_eq!(no_token_pool.get(&root), Some(&false));
}

#[test]
#[serial(role_tick_ring)]
fn test_log_outcome_for_root_deduped_pool_exhausted_tracked_independently_of_failing() {
    // #7607: a PoolExhausted tick must never mark `failing` or
    // `no_token_pool` true, and a real Failure tick must never mark
    // `pool_exhausted` true — the three maps are independent axes even
    // for the SAME root.
    let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-7607");
    let mut failing: HashMap<PathBuf, bool> = HashMap::new();
    let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
    let mut pool_exhausted: HashMap<PathBuf, bool> = HashMap::new();
    let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

    log_outcome_for_root_deduped(
        "auditor",
        &root,
        &pool_exhausted_outcome(),
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(pool_exhausted.get(&root), Some(&true));
    assert_eq!(failing.get(&root), Some(&false));
    assert_eq!(no_token_pool.get(&root), Some(&false));

    // A subsequent real failure must still log as a fresh `FailureEdge`
    // (not `FailureRepeat`) even though the root was just skipped for an
    // exhausted pool — proving the two states never cross-contaminate.
    log_outcome_for_root_deduped(
        "auditor",
        &root,
        &RoleTickOutcome::Failure("boom".into()),
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(failing.get(&root), Some(&true));
    assert_eq!(pool_exhausted.get(&root), Some(&false));
}

#[test]
#[serial(role_tick_ring)]
fn test_log_outcome_for_root_deduped_model_mismatch_tracked_independently() {
    // #5028: a ModelRuntimeMismatch tick must never mark `failing` or
    // `no_token_pool` true, and must not itself be marked by either of
    // those two axes — all three maps are independent even for the SAME
    // root.
    let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-3");
    let mut failing: HashMap<PathBuf, bool> = HashMap::new();
    let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
    let mut pool_exhausted: HashMap<PathBuf, bool> = HashMap::new();
    let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

    log_outcome_for_root_deduped(
        "judge",
        &root,
        &mismatch_outcome(),
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(model_mismatch.get(&root), Some(&true));
    assert_eq!(failing.get(&root), Some(&false));
    assert_eq!(no_token_pool.get(&root), Some(&false));

    // A subsequent real failure must still log as a fresh `FailureEdge`
    // even though the root was just skipped for a model/runtime mismatch.
    log_outcome_for_root_deduped(
        "judge",
        &root,
        &RoleTickOutcome::Failure("boom".into()),
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(failing.get(&root), Some(&true));
    assert_eq!(model_mismatch.get(&root), Some(&false));
}

/// Issue #6637: a `LoadSkipped` tick must never mark `failing`,
/// `no_token_pool`, or `model_mismatch` true — it is its own axis, and a
/// load-induced skip must not be tallied against any of the three
/// existing dedup states (mirroring the independence tests above).
#[test]
fn test_log_outcome_for_root_deduped_load_skipped_tracked_independently() {
    let root = PathBuf::from("/tmp/does-not-need-to-exist-for-this-test-4");
    let mut failing: HashMap<PathBuf, bool> = HashMap::new();
    let mut no_token_pool: HashMap<PathBuf, bool> = HashMap::new();
    let mut pool_exhausted: HashMap<PathBuf, bool> = HashMap::new();
    let mut model_mismatch: HashMap<PathBuf, bool> = HashMap::new();

    log_outcome_for_root_deduped(
        "auditor",
        &root,
        &RoleTickOutcome::LoadSkipped {
            load_per_core: 2.4,
            detail: "still resolving spawn-worker.sh".to_string(),
        },
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(failing.get(&root), Some(&false));
    assert_eq!(no_token_pool.get(&root), Some(&false));
    assert_eq!(pool_exhausted.get(&root), Some(&false));
    assert_eq!(model_mismatch.get(&root), Some(&false));

    // A subsequent real failure must still log as a fresh `FailureEdge`
    // even though the root was just load-skipped.
    log_outcome_for_root_deduped(
        "auditor",
        &root,
        &RoleTickOutcome::Failure("boom".into()),
        NORMAL_TICK,
        &mut failing,
        &mut no_token_pool,
        &mut pool_exhausted,
        &mut model_mismatch,
    );
    assert_eq!(failing.get(&root), Some(&true));
}

// ===================================================================
// spawn_multi_role_task missing-root hygiene (#4326/#4349) — a
// registered root whose directory no longer exists is skipped, not
// spawned against, mirroring work_finder's filter_missing_roots.
// ===================================================================

#[tokio::test]
#[serial]
async fn test_multi_role_task_skips_missing_registered_root() {
    let tmp = tempfile::tempdir().unwrap();
    let existing_root = tmp.path().join("existing");
    let missing_root = tmp.path().join("gone");
    std::fs::create_dir_all(&existing_root).unwrap();
    write_config(&existing_root, r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
    // `add` validates the path exists at registration time, so create the
    // "missing" root first, register it, then delete it — reproducing a
    // registered-but-later-deleted worktree (#4349's #4188 scenario).
    std::fs::create_dir_all(&missing_root).unwrap();

    let registry_path = tmp.path().join("workspaces.json");
    std::env::set_var(
        crate::workspace_registry::REGISTRY_PATH_ENV,
        registry_path.to_str().unwrap(),
    );
    let mut registry = WorkspaceRegistry::default();
    registry.add(&existing_root, None).unwrap();
    registry.add(&missing_root, None).unwrap();
    registry.save_default().unwrap();
    std::fs::remove_dir_all(&missing_root).unwrap();

    let spec = RoleSpec {
        name: "curator",
        prompt: "/loom:curator",
        default_interval_secs: 1,
        interval_default: true,
    };
    let drain = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let in_progress = new_in_progress_guard();
    let handle = spawn_multi_role_task(
        spec,
        tmp.path().to_path_buf(),
        Duration::from_millis(20),
        drain,
        in_progress,
        None,
    );

    // Let a couple of ticks fire. The missing root must never be spawned
    // against (there is no script at its `.loom/config.json`/spawn path
    // to invoke, so a spawn attempt would either fail loudly or panic
    // the resolve step; the assertion here is simply that the loop
    // survives several ticks without erroring the test process, which
    // it would if the missing root were not filtered before dispatch).
    tokio::time::sleep(Duration::from_millis(80)).await;
    handle.abort();

    std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
}

// ===================================================================
// decide_root_tick + its catch_unwind isolation (#6201 AC2)
// ===================================================================

fn curator_spec() -> RoleSpec {
    RoleSpec {
        name: "curator",
        prompt: "/loom:curator",
        default_interval_secs: 300,
        interval_default: true,
    }
}

/// Put [`ROLE_RUNNER_ENABLE_ENV`] back exactly as it was found — set to
/// its prior value, or unset if it was unset (#6644). Paired with a
/// `std::env::var(ROLE_RUNNER_ENABLE_ENV).ok()` capture + `remove_var` at
/// the top of a `#[serial]` test.
fn restore_role_runner_env(prev: Option<String>) {
    match prev {
        Some(v) => std::env::set_var(ROLE_RUNNER_ENABLE_ENV, v),
        None => std::env::remove_var(ROLE_RUNNER_ENABLE_ENV),
    }
}

#[test]
fn decide_root_tick_skips_a_disabled_root_and_warns_once() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":false}}}"#);
    let mut disabled_warned = HashSet::new();
    let mut resolved_logged = HashMap::new();
    let in_progress = new_in_progress_guard();

    let decision = decide_root_tick(
        tmp.path(),
        &curator_spec(),
        &in_progress,
        &mut disabled_warned,
        &mut resolved_logged,
        &mut HashMap::new(),
    );
    assert!(decision.is_none());
    assert!(disabled_warned.contains(tmp.path()));
}

#[test]
fn decide_root_tick_skips_a_role_not_in_the_resolved_list() {
    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["judge"]}}}"#,
    );
    let mut disabled_warned = HashSet::new();
    let mut resolved_logged = HashMap::new();
    let in_progress = new_in_progress_guard();

    let decision = decide_root_tick(
        tmp.path(),
        &curator_spec(),
        &in_progress,
        &mut disabled_warned,
        &mut resolved_logged,
        &mut HashMap::new(),
    );
    assert!(decision.is_none());
}

/// #6644: `decide_root_tick` resolves enablement through
/// [`resolve_enabled_with_source`], which consults the ambient
/// [`ROLE_RUNNER_ENABLE_ENV`] **before** the root's own config — so an
/// inherited falsy `LOOM_ROLE_RUNNER` (e.g. on an agent dispatched by a
/// daemon whose unit/plist baked the var into its environment) would
/// override the tempdir config this test writes and make the admission
/// assertion fail for reasons unrelated to the code under test. Scope the
/// var explicitly, under `#[serial]` so this does not race the file's
/// other `ROLE_RUNNER_ENABLE_ENV`-mutating tests.
#[test]
#[serial]
fn decide_root_tick_admits_and_returns_a_guard_when_configured() {
    let prev_env = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
    let mut disabled_warned = HashSet::new();
    let mut resolved_logged = HashMap::new();
    let in_progress = new_in_progress_guard();

    let decision = decide_root_tick(
        tmp.path(),
        &curator_spec(),
        &in_progress,
        &mut disabled_warned,
        &mut resolved_logged,
        &mut HashMap::new(),
    );

    // Restore BEFORE asserting: a failing assertion must not leak this
    // test's scoped value into the rest of the suite.
    restore_role_runner_env(prev_env);

    let (prompt, _guard) = decision.expect("curator is enabled and in the default role set");
    assert_eq!(prompt, "/loom:curator");
    // The guard holds the (root, role) pair in-progress until dropped.
    assert_eq!(active_run_count(&in_progress), 1);
}

/// #7511: a role absent from `autonomous.roleRunner.roles` (so ordinarily
/// skipped by the line-3105-equivalent membership check) IS admitted when
/// it is configured `onIdle` + `onIdleMaxWait` and has never ticked at
/// all (first registration ⇒ immediately eligible).
#[test]
#[serial]
fn decide_root_tick_promotes_an_overdue_on_idle_role() {
    let prev_env = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["judge"],"onIdle":["hermit"],"onIdleMaxWait":{"hermit":"1s"}}}}"#,
    );
    let in_progress = new_in_progress_guard();

    let decision = decide_root_tick(
        tmp.path(),
        &hermit_spec(),
        &in_progress,
        &mut HashSet::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
    );

    restore_role_runner_env(prev_env);

    let (prompt, _guard) = decision.expect(
        "hermit is not in `roles` but is `onIdle` + `onIdleMaxWait`-overdue \
             (never ticked) — must be promoted",
    );
    assert_eq!(prompt, "/loom:hermit");
    assert_eq!(active_run_count(&in_progress), 1);
}

/// #7511: a role absent from `roles` and NOT covered by `onIdleMaxWait`
/// stays skipped exactly like before this issue — zero behavior change.
#[test]
#[serial]
fn decide_root_tick_still_skips_an_on_idle_role_with_no_max_wait_configured() {
    let prev_env = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

    let tmp = tempfile::tempdir().unwrap();
    write_config(
        tmp.path(),
        r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["judge"],"onIdle":["hermit"]}}}"#,
    );
    let in_progress = new_in_progress_guard();

    let decision = decide_root_tick(
        tmp.path(),
        &hermit_spec(),
        &in_progress,
        &mut HashSet::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
    );

    restore_role_runner_env(prev_env);

    assert!(decision.is_none(), "no onIdleMaxWait entry ⇒ no promotion, today's behavior");
}

/// #7511 AC: a promoted role is admitted through the SAME
/// `RoleRunGuard::admit` ceiling as any interval role — proven here by
/// saturating a `maxConcurrent: 1` ceiling with one promoted root's guard
/// and showing a second, equally-overdue promotable root is refused, not
/// waved through.
#[test]
#[serial]
fn decide_root_tick_promotion_respects_the_concurrency_ceiling() {
    let prev_env = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

    let config_body = r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["judge"],"onIdle":["hermit"],"onIdleMaxWait":{"hermit":"1s"},"maxConcurrent":1}}}"#;
    let root_a = tempfile::tempdir().unwrap();
    write_config(root_a.path(), config_body);
    let root_b = tempfile::tempdir().unwrap();
    write_config(root_b.path(), config_body);

    let in_progress = new_in_progress_guard();

    let decision_a = decide_root_tick(
        root_a.path(),
        &hermit_spec(),
        &in_progress,
        &mut HashSet::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
    );
    let (_prompt_a, guard_a) =
        decision_a.expect("root_a's hermit has never ticked — promotable, ceiling has room");
    assert_eq!(active_run_count(&in_progress), 1);

    // root_b's hermit is EQUALLY overdue (never ticked) and would also be
    // promoted in isolation — but the process-wide ceiling of 1 is
    // already saturated by root_a's guard, so this must be refused
    // exactly like an ordinary interval role hitting the same ceiling
    // (#6102) — promotion is not a bypass.
    let decision_b = decide_root_tick(
        root_b.path(),
        &hermit_spec(),
        &in_progress,
        &mut HashSet::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
    );

    restore_role_runner_env(prev_env);
    drop(guard_a);

    assert!(
        decision_b.is_none(),
        "a promoted role must still be refused once maxConcurrent is saturated"
    );
}

/// #6201 AC2: the exact `catch_unwind(AssertUnwindSafe(...))` shape
/// `spawn_multi_role_task`'s loop wraps [`decide_root_tick`] in isolates
/// a panic — it must never propagate out of the tick, and a subsequent
/// call using the SAME shared dedup state must still succeed normally
/// (proving the caught panic left no poisoned/inconsistent state behind
/// that would itself wedge later ticks).
///
/// `#[serial]` + explicit [`ROLE_RUNNER_ENABLE_ENV`] scoping for the same
/// reason as the test above (#6644): the recovery half of this test calls
/// [`decide_root_tick`] for real, so an ambient `LOOM_ROLE_RUNNER` would
/// otherwise decide the outcome instead of the tempdir config.
#[test]
#[serial]
fn root_tick_decision_panic_is_isolated_and_does_not_propagate() {
    let prev_env = std::env::var(ROLE_RUNNER_ENABLE_ENV).ok();
    std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);

    let mut disabled_warned: HashSet<PathBuf> = HashSet::new();
    let mut resolved_logged: HashMap<PathBuf, String> = HashMap::new();

    // Same call shape as the production site, but the closure panics
    // instead of calling `decide_root_tick` — reproducing "a panic
    // anywhere in the synchronous decision phase" without depending on
    // an actual panic trigger existing in today's (deliberately
    // soft-failing) config-parsing code.
    let result: Result<Option<(String, RoleRunGuard)>, _> =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = &mut disabled_warned;
            let _ = &mut resolved_logged;
            panic!("synthetic tick-decision panic (#6201 regression fixture)");
        }));
    assert!(result.is_err(), "the panic must be caught, not propagated");
    let msg = describe_panic(&*result.unwrap_err());
    assert!(msg.contains("synthetic tick-decision panic"), "{msg}");

    // The loop's own recovery: the SAME shared dedup maps are still
    // usable afterward, and a real decision call succeeds normally —
    // exactly "skip only this tick; the loop continues on the next
    // interval".
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
    let in_progress = new_in_progress_guard();
    let decision = decide_root_tick(
        tmp.path(),
        &curator_spec(),
        &in_progress,
        &mut disabled_warned,
        &mut resolved_logged,
        &mut HashMap::new(),
    );

    // Restore BEFORE asserting (see the sibling test above).
    restore_role_runner_env(prev_env);

    assert!(
        decision.is_some(),
        "a later, healthy tick must still succeed after a caught panic"
    );
}

#[test]
fn describe_panic_extracts_str_and_string_payloads() {
    let str_panic = std::panic::catch_unwind(|| -> () { panic!("literal message") }).unwrap_err();
    assert_eq!(describe_panic(&*str_panic), "literal message");

    let string_panic =
        std::panic::catch_unwind(|| -> () { panic!("{}", "formatted message".to_string()) })
            .unwrap_err();
    assert_eq!(describe_panic(&*string_panic), "formatted message");
}

// ===================================================================
// Host sharding at the dispatch surface (#6374)
//
// `role_shard`'s own tests pin the *arithmetic* (exactly one owner per
// key, an even spread, the fail-safe fallbacks). These pin the thing
// that arithmetic alone cannot: that `decide_root_tick` and
// `plan_idle_runs` — the two surfaces that actually spend a token —
// honor it, in the right order relative to the `LOOM_ROLE_RUNNER`
// kill switch.
// ===================================================================

/// Capture and clear every env var these tests manipulate, restoring the
/// prior values on drop so a failing assertion cannot leak state into the
/// rest of the (serial) suite.
struct ShardEnvGuard {
    enable: Option<String>,
    index: Option<String>,
    count: Option<String>,
}

impl ShardEnvGuard {
    fn capture() -> Self {
        let g = Self {
            enable: std::env::var(ROLE_RUNNER_ENABLE_ENV).ok(),
            index: std::env::var(crate::role_shard::SHARD_INDEX_ENV).ok(),
            count: std::env::var(crate::role_shard::SHARD_COUNT_ENV).ok(),
        };
        std::env::remove_var(ROLE_RUNNER_ENABLE_ENV);
        std::env::remove_var(crate::role_shard::SHARD_INDEX_ENV);
        std::env::remove_var(crate::role_shard::SHARD_COUNT_ENV);
        g
    }

    /// Pretend to be host `index` of a `count`-host fleet.
    fn become_host(index: usize, count: usize) {
        std::env::set_var(crate::role_shard::SHARD_INDEX_ENV, index.to_string());
        std::env::set_var(crate::role_shard::SHARD_COUNT_ENV, count.to_string());
    }
}

impl Drop for ShardEnvGuard {
    fn drop(&mut self) {
        for (name, prev) in [
            (ROLE_RUNNER_ENABLE_ENV, &self.enable),
            (crate::role_shard::SHARD_INDEX_ENV, &self.index),
            (crate::role_shard::SHARD_COUNT_ENV, &self.count),
        ] {
            match prev {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A role-runner-enabled tempdir workspace whose shard key is its own
/// (random) basename — so a set of them stands in for a fleet of
/// distinctly-keyed workspaces without needing real git remotes.
fn enabled_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), r#"{"autonomous":{"roleRunner":{"enabled":true}}}"#);
    tmp
}

/// Whether one host (identified by the ambient shard env) would spend a
/// curator tick on `root` this interval.
fn tick_admitted(root: &Path) -> bool {
    let in_progress = new_in_progress_guard();
    let decision = decide_root_tick(
        root,
        &curator_spec(),
        &in_progress,
        &mut HashSet::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
    );
    decision.is_some()
}

/// AC1 (first half), at the dispatch surface rather than in the hash:
/// across a two-host fleet, each workspace's curator tick is admitted by
/// **exactly one** host per interval — never zero (the slice would go
/// unrotated fleet-wide) and never two (the #6332 / #6352 duplication
/// this issue exists to prevent).
#[test]
#[serial]
fn two_host_fleet_admits_each_workspace_curator_tick_on_exactly_one_host() {
    let _env = ShardEnvGuard::capture();
    let fleet: Vec<tempfile::TempDir> = (0..12).map(|_| enabled_workspace()).collect();

    for workspace in &fleet {
        let root = workspace.path();
        let admitting: Vec<usize> = (0..2)
            .filter(|host| {
                ShardEnvGuard::become_host(*host, 2);
                tick_admitted(root)
            })
            .collect();
        assert_eq!(
            admitting.len(),
            1,
            "{} admitted by hosts {admitting:?}; exactly one host must run each workspace's \
                 role tick per interval (#6374)",
            root.display()
        );
    }
}

/// AC2, measured the way the incident measured it: the fleet-wide *count
/// of role sessions spawned per interval*. Unsharded, a 4-host fleet
/// spends 4 curator ticks per workspace; sharded, it spends 1 — the token
/// draw scales with workspaces, not workspaces x hosts.
#[test]
#[serial]
fn sharding_makes_the_fleet_wide_tick_draw_scale_with_workspaces_not_hosts() {
    let _env = ShardEnvGuard::capture();
    let fleet: Vec<tempfile::TempDir> = (0..12).map(|_| enabled_workspace()).collect();
    const HOSTS: usize = 4;

    // Unsharded (today's behavior, and the fail-safe fallback): every
    // host spends a tick on every workspace.
    let unsharded: usize = (0..HOSTS)
        .map(|_| fleet.iter().filter(|w| tick_admitted(w.path())).count())
        .sum();
    assert_eq!(unsharded, fleet.len() * HOSTS);

    // Sharded: the same fleet spends exactly one tick per workspace.
    let sharded: usize = (0..HOSTS)
        .map(|host| {
            ShardEnvGuard::become_host(host, HOSTS);
            fleet.iter().filter(|w| tick_admitted(w.path())).count()
        })
        .sum();
    assert_eq!(
        sharded,
        fleet.len(),
        "a {HOSTS}-host fleet drew {sharded} curator ticks for {} workspaces (#6374 AC2)",
        fleet.len()
    );
}

/// AC3: the blunt per-host kill switch keeps working, and keeps
/// short-circuiting **before** sharding is consulted — so an operator who
/// sets `LOOM_ROLE_RUNNER=0` gets zero ticks regardless of whether this
/// host owns the slice. Asserted for the owning host specifically, since
/// a non-owning host would skip for the wrong reason and prove nothing.
#[test]
#[serial]
fn role_runner_env_zero_still_disables_the_host_that_owns_the_slice() {
    let _env = ShardEnvGuard::capture();
    let workspace = enabled_workspace();
    let root = workspace.path();

    let owner = (0..2)
        .find(|host| {
            ShardEnvGuard::become_host(*host, 2);
            tick_admitted(root)
        })
        .expect("exactly one of the two hosts owns this workspace");

    ShardEnvGuard::become_host(owner, 2);
    assert!(tick_admitted(root), "precondition: the owning host ticks");

    std::env::set_var(ROLE_RUNNER_ENABLE_ENV, "0");
    assert!(
        !tick_admitted(root),
        "LOOM_ROLE_RUNNER=0 must still disable role ticks on the host that owns the slice \
             (#6374 AC3)"
    );
}

/// AC3, the other direction: sharding must not *weaken* the kill switch's
/// counterpart either — an unsharded host (no shard env at all) behaves
/// exactly as it did before #6374, owning every workspace.
#[test]
#[serial]
fn an_unsharded_host_still_ticks_every_workspace() {
    let _env = ShardEnvGuard::capture();
    let fleet: Vec<tempfile::TempDir> = (0..6).map(|_| enabled_workspace()).collect();
    for workspace in &fleet {
        assert!(
            tick_admitted(workspace.path()),
            "an unsharded host must keep rotating every workspace (#6374 fail-safe)"
        );
    }
}

/// AC1 (second half), as far as this PR's **static** assignment goes:
/// shrinking the ring reassigns the departed host's slice to the
/// survivors, and no workspace is left unowned by the reassignment. This
/// is the operator-driven reassignment path (lower `shardCount`, or point
/// the survivor at the vacated index); automatic, roster-driven
/// reassignment on host loss is deliberately deferred to #6704 — see
/// `role_shard`'s module docs for why.
#[test]
#[serial]
fn shrinking_the_ring_reassigns_the_departed_hosts_slice_to_the_survivor() {
    let _env = ShardEnvGuard::capture();
    let fleet: Vec<tempfile::TempDir> = (0..12).map(|_| enabled_workspace()).collect();

    // Host 1 dies. Its slice is exactly what host 0 was NOT ticking.
    ShardEnvGuard::become_host(0, 2);
    let orphaned: Vec<&Path> = fleet
        .iter()
        .map(tempfile::TempDir::path)
        .filter(|root| !tick_admitted(root))
        .collect();
    assert!(!orphaned.is_empty(), "precondition: host 1 must have owned something to orphan");

    // The operator shrinks the ring to the one survivor; every orphaned
    // workspace is picked up, and nothing is dropped in the process.
    ShardEnvGuard::become_host(0, 1);
    for root in &fleet {
        assert!(
            tick_admitted(root.path()),
            "{} must be rotated by the surviving host after the ring shrinks (#6374)",
            root.path().display()
        );
    }
}

mod model_resolution;
mod prompt_cache_prefix;
mod roster_fence;
mod tick_ring;
