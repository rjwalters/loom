//! Coverage for the role-tick provider-health bridge (issue #8443) — the
//! `codex`-runtime analogue of
//! `sweep_registry::provider_health_feedback_tests`.
//!
//! Every test reaches [`super::apply_role_tick_provider_health_feedback`]
//! only indirectly, through [`ScriptRoleInvocationRunner::invoke`], with a
//! fake `spawn-worker.sh` standing in for the real adapter and printing the
//! same `# LOOM_ACCOUNT name=…` / `# LOOM_TERMINAL_RESULT …` lines
//! `spawn-codex.sh` does — never by calling `record_terminal_at` (or this
//! module's own function) directly to fabricate the write under test. A
//! pre-existing hold used as a test PRECONDITION (the "clears an existing
//! hold" case) is the one place `record_terminal_at` appears, mirroring how
//! `runtime_preflight::tests::codex_pinned_role_skips_while_every_account_is_held_then_recovers`
//! already uses it to seed state ahead of the behavior under test.

use super::*;
use crate::tokens_pool::health::record_terminal_at;
use crate::tokens_pool::TerminalClassification;
use serial_test::serial;
use std::fs;
use std::os::unix::fs::PermissionsExt;

/// Every env var that can change which runtime a role resolves to, or which
/// credential source `spawn-codex.sh` would use — cleared for the scope of a
/// test and restored on drop, exactly like
/// `runtime_preflight::tests::EnvGuard`.
const GUARDED_ENV: [&str; 8] = [
    "LOOM_RUNTIME",
    "LOOM_RUNTIME_JUDGE",
    "LOOM_CODEX_HOME",
    "CODEX_HOME",
    "LOOM_CODEX_PROFILE",
    "LOOM_SPAWN_NO_EXPORT",
    "LOOM_CODEX_NO_EXEC",
    "LOOM_CODEX_PROFILE_ROOT",
];

/// The second field holds the crate-wide `LOOM_CODEX_PROFILE_ROOT` lock (#9964).
struct EnvGuard(
    Vec<(&'static str, Option<String>)>,
    #[allow(dead_code)] crate::tokens_pool::profile_root_env::ProfileRootLock,
);

impl EnvGuard {
    fn new(profile_root: &Path) -> Self {
        let lock = crate::tokens_pool::profile_root_env::lock();
        let prior = GUARDED_ENV
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in GUARDED_ENV {
            std::env::remove_var(key);
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profile_root);
        Self(prior, lock)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A workspace whose `judge` role is pinned to the codex runtime, with one
/// enabled codex account (`alice`) and a fake `spawn-worker.sh` this test
/// overwrites per-case to script the adapter's own terminal output.
fn codex_judge_workspace(root: &Path) {
    for sub in [
        ".loom/roles",
        ".loom/runtimes",
        ".loom/scripts",
        ".loom/tokens",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::write(
        root.join(".loom/config.json"),
        r#"{"runtimes":{"roles":{"judge":"codex"}},"autonomous":{"roleRunner":{"roleModels":{"judge":"gpt-5-codex"}}}}"#,
    )
    .unwrap();
    fs::write(root.join(".loom/roles/judge.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    fs::write(
        root.join(".loom/runtimes/codex.json"),
        r#"{"runtime":"codex","accountProvider":"codex","capabilities":{"mcp":"yes"}}"#,
    )
    .unwrap();
    write_executable(&root.join(".loom/scripts/spawn-codex.sh"), "#!/bin/sh\nexit 0\n");
}

fn codex_id(name: &str) -> AccountId {
    AccountId {
        provider: AccountProvider::Codex,
        name: name.to_string(),
    }
}

fn judge_runner(root: &Path) -> ScriptRoleInvocationRunner {
    ScriptRoleInvocationRunner::new(root.to_path_buf()).with_timeout(Duration::from_secs(5))
}

fn epoch_now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap()
}

/// AC1: a codex-runtime tick whose fake adapter prints `TOKEN_EXHAUSTED`
/// leaves an account-wide hold — and AC2: with that (only) account held,
/// the *next* tick of the same role skips pre-spawn via #8442's gate
/// instead of spawning.
#[test]
#[serial]
fn codex_role_tick_token_exhausted_records_a_hold_that_gates_the_next_tick() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    codex_judge_workspace(workspace.path());
    write_executable(
        &workspace.path().join(".loom/scripts/spawn-worker.sh"),
        "#!/bin/sh\n\
         echo '# LOOM_ACCOUNT name=alice'\n\
         echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=alice \
category=TOKEN_EXHAUSTED exit_code=1 model=none'\n\
         exit 1\n",
    );

    let ws = crate::write_scope_test_support::WritableRoot::register(workspace.path());
    let outcome = judge_runner(workspace.path())
        .with_gh_bin(ws.gh.clone())
        .invoke("judge", "/loom:judge");
    assert!(!outcome.is_success(), "{outcome:?}");

    let health = tokens_pool::account_health(workspace.path(), &codex_id("alice"))
        .unwrap()
        .expect("a codex-runtime TOKEN_EXHAUSTED tick must write account health");
    assert!(
        health
            .cooldown_until
            .is_some_and(|deadline| deadline > epoch_now()),
        "TOKEN_EXHAUSTED must leave a live account-wide cooldown: {health:?}"
    );

    // AC2: the account is now the ONLY enabled codex account and it is held
    // — the very next tick must skip pre-spawn via the #8442 gate rather
    // than spawn again.
    let before = pool_exhausted_skip_count();
    let outcome = judge_runner(workspace.path())
        .with_gh_bin(ws.gh.clone())
        .invoke("judge", "/loom:judge");
    let RoleTickOutcome::PoolExhausted { pool, .. } = outcome else {
        panic!("expected the #8442 gate to skip pre-spawn, got {outcome:?}");
    };
    assert_eq!(pool, CredentialPool::CodexAccounts);
    assert_eq!(pool_exhausted_skip_count(), before + 1);
}

/// AC3: a `SUCCESS` terminal result from a role tick clears a pre-existing
/// hold, exactly as it does for sweeps. The pre-existing hold is seeded
/// directly (the test's precondition, not the behavior under test) — but
/// backdated far enough that its `cooldown_until` has already elapsed, so
/// #8442's pre-spawn gate does not itself block this tick (the point of
/// this test is the WRITE side clearing a stale-but-still-recorded hold,
/// not the read side, which `codex_pinned_role_skips_while_every_account_is_held_then_recovers`
/// in `runtime_preflight::tests` already covers). The clearing itself
/// happens only through `invoke()`.
#[test]
#[serial]
fn codex_role_tick_success_clears_a_pre_existing_hold() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    codex_judge_workspace(workspace.path());
    record_terminal_at(
        workspace.path(),
        &codex_id("alice"),
        TerminalClassification::TokenExhausted,
        "test",
        epoch_now().saturating_sub(6 * 60 * 60),
    )
    .unwrap();
    write_executable(
        &workspace.path().join(".loom/scripts/spawn-worker.sh"),
        "#!/bin/sh\n\
         echo '# LOOM_ACCOUNT name=alice'\n\
         echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=alice \
category=SUCCESS exit_code=0 model=none'\n\
         exit 0\n",
    );

    let ws = crate::write_scope_test_support::WritableRoot::register(workspace.path());
    let outcome = judge_runner(workspace.path())
        .with_gh_bin(ws.gh.clone())
        .invoke("judge", "/loom:judge");
    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");

    let health = tokens_pool::account_health(workspace.path(), &codex_id("alice"))
        .unwrap()
        .expect("health record still exists after a clearing SUCCESS");
    assert!(
        health.cooldown_until.is_none(),
        "SUCCESS must clear the account-wide hold: {health:?}"
    );
}

/// AC4: a terminal record whose `account=` does not match the account the
/// adapter actually selected is ignored — no health write for either name
/// — matching the sweep path's own mismatch guard.
#[test]
#[serial]
fn codex_role_tick_ignores_a_terminal_result_with_a_mismatched_account() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    codex_judge_workspace(workspace.path());
    write_executable(
        &workspace.path().join(".loom/scripts/spawn-worker.sh"),
        "#!/bin/sh\n\
         echo '# LOOM_ACCOUNT name=alice'\n\
         echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=someone-else \
category=TOKEN_EXHAUSTED exit_code=1 model=none'\n\
         exit 1\n",
    );

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");
    assert!(!outcome.is_success(), "{outcome:?}");

    assert!(
        tokens_pool::account_health(workspace.path(), &codex_id("alice"))
            .unwrap()
            .is_none(),
        "the actually-selected account must not be marked from a mismatched record"
    );
    assert!(
        tokens_pool::account_health(workspace.path(), &codex_id("someone-else"))
            .unwrap()
            .is_none(),
        "the terminal record's claimed account must not be trusted on a mismatch"
    );
}

/// AC4 (exit-code arm): a terminal record whose `exit_code=` disagrees with
/// the process's own observed exit status is ignored the same way.
#[test]
#[serial]
fn codex_role_tick_ignores_a_terminal_result_with_a_mismatched_exit_code() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    codex_judge_workspace(workspace.path());
    write_executable(
        &workspace.path().join(".loom/scripts/spawn-worker.sh"),
        "#!/bin/sh\n\
         echo '# LOOM_ACCOUNT name=alice'\n\
         echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=alice \
category=TOKEN_EXHAUSTED exit_code=1 model=none'\n\
         exit 7\n",
    );

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");
    assert!(!outcome.is_success(), "{outcome:?}");

    assert!(
        tokens_pool::account_health(workspace.path(), &codex_id("alice"))
            .unwrap()
            .is_none(),
        "a terminal record whose exit_code disagrees with the real exit status must be ignored"
    );
}

/// A Claude-runtime tick must never reach this module's write path at all —
/// mirrors `apply_provider_health_feedback`'s own `info.runtime != "codex"`
/// guard. Uses `with_spawn_bin` (the pre-existing test pattern for the
/// Claude/unadmitted path), which leaves `admission` as `None`.
#[test]
fn non_codex_tick_never_writes_account_health() {
    let workspace = tempfile::tempdir().unwrap();
    fs::create_dir_all(workspace.path().join(".loom/tokens")).unwrap();
    fs::write(workspace.path().join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    let script = workspace.path().join("fake-spawn.sh");
    write_executable(
        &script,
        "#!/bin/sh\n\
         echo '# LOOM_ACCOUNT name=alice'\n\
         echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=alice \
category=TOKEN_EXHAUSTED exit_code=1 model=none'\n\
         exit 1\n",
    );
    let mut runner = ScriptRoleInvocationRunner::new(workspace.path().to_path_buf())
        .with_spawn_bin(script)
        .with_timeout(Duration::from_secs(5));

    let outcome = runner.invoke("curator", "/loom:curator");
    assert!(!outcome.is_success(), "{outcome:?}");
    assert!(
        tokens_pool::account_health(workspace.path(), &codex_id("alice"))
            .unwrap()
            .is_none(),
        "an unadmitted (test spawn_bin) tick must never write account health"
    );
}

/// #8539 call-site wiring, role-tick side: when the adapter's refusal names a
/// reset horizon, the hold this tick records ends at *that* instant rather
/// than at `now + LOOM_CODEX_EXHAUSTED_COOLDOWN_SECS`.
///
/// The sweep path has the same assertion
/// (`sweep_registry::provider_health_feedback_tests`). Both exist because
/// `codex_reset`'s own tests call the parser directly, so a call site here
/// passing the wrong contents or the wrong tick anchor would go unnoticed.
///
/// The horizon is derived from *now* (not a fixed date) so the test does not
/// start asserting about the fallback cooldown once a hard-coded fixture date
/// drifts into the past.
#[test]
#[serial]
fn codex_role_tick_honours_the_refusals_own_reset_horizon() {
    use chrono::{Local, Timelike};

    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    codex_judge_workspace(workspace.path());

    let horizon = (Local::now() + chrono::Duration::days(2))
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let rendered = horizon.format("%B %d, %Y %I:%M %p").to_string();
    let expected_epoch = u64::try_from(horizon.timestamp()).unwrap();

    write_executable(
        &workspace.path().join(".loom/scripts/spawn-worker.sh"),
        &format!(
            "#!/bin/sh\n\
             echo '# LOOM_ACCOUNT name=alice'\n\
             echo \"ERROR: You've hit your usage limit. Visit \
             https://chatgpt.com/codex/settings/usage to purchase more credits or \
             try again at {rendered}.\"\n\
             echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=alice \
category=TOKEN_EXHAUSTED exit_code=1 model=none'\n\
             exit 1\n"
        ),
    );

    let ws = crate::write_scope_test_support::WritableRoot::register(workspace.path());
    let outcome = judge_runner(workspace.path())
        .with_gh_bin(ws.gh.clone())
        .invoke("judge", "/loom:judge");
    assert!(!outcome.is_success(), "{outcome:?}");

    let health = tokens_pool::account_health(workspace.path(), &codex_id("alice"))
        .unwrap()
        .expect("a codex-runtime TOKEN_EXHAUSTED tick must write account health");
    assert_eq!(
        health.cooldown_until,
        Some(expected_epoch),
        "the hold must end at the horizon the provider's own refusal named: {health:?}"
    );
}

// ---- #8931: reason-classified `loom.pool.account_marks` at this seam ----

/// The Codex half: one real role tick whose adapter reports
/// `TOKEN_EXHAUSTED` emits exactly one `exhausted` mark for `codex`.
#[test]
#[serial]
fn codex_role_tick_mark_emits_exactly_one_reason_classified_point() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    codex_judge_workspace(workspace.path());
    write_executable(
        &workspace.path().join(".loom/scripts/spawn-worker.sh"),
        "#!/bin/sh\n\
         echo '# LOOM_ACCOUNT name=alice'\n\
         echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=alice \
category=TOKEN_EXHAUSTED exit_code=1 model=none'\n\
         exit 1\n",
    );
    let (outcome, captured) = crate::observability::ops::capture::capture(|| {
        let ws = crate::write_scope_test_support::WritableRoot::register(workspace.path());
        judge_runner(workspace.path())
            .with_gh_bin(ws.gh.clone())
            .invoke("judge", "/loom:judge")
    });
    assert!(!outcome.is_success(), "{outcome:?}");
    let marks: Vec<_> = captured
        .metrics
        .iter()
        .filter(|p| p.name.as_str() == "loom.pool.account_marks")
        .collect();
    assert_eq!(marks.len(), 1, "{:?}", captured.metrics);
    assert_eq!(marks[0].labels["provider"], "codex");
    assert_eq!(marks[0].labels["reason"], "exhausted");
    assert!(!serde_json::to_string(marks[0]).unwrap().contains("alice"));
}

/// The native half, at the seam itself: a pool-selected launch whose harness
/// hit a rate limit is bad-marked and emits one `rate_limited` mark.
#[test]
fn native_role_tick_mark_emits_exactly_one_reason_classified_point() {
    use crate::api_keys_pool::{ingest::LAUNCH_RECORD_PREFIX, paths, registry as keys};
    let workspace = tempfile::tempdir().unwrap();
    let pool = paths::per_repo_api_keys_dir(workspace.path());
    keys::add(&pool, "loomtest", "bob", "LOOM_TEST_KEY_8931", "fake-key", false).unwrap();
    let anchor = "==== role tick 8931 ====";
    let launch = serde_json::json!({
        "schema": 1, "runtime": "opencode", "provider": "zai-coding-plan",
        "model": "glm-5.3-flash", "profile": "zai-flash", "effort": null,
        "credentialSource": "pool", "credentialProvider": "loomtest",
        "credentialAccount": "bob", "usage": "native-json-events",
        "billing": "not-measured",
    });
    let log_path = workspace.path().join("role.log");
    fs::write(
        &log_path,
        format!(
            "{anchor}\n{LAUNCH_RECORD_PREFIX}{launch}\n# LOOM_CLI_START runtime=opencode\n\
             {}\n",
            crate::api_keys_pool::classify::CAPTURED_KIMI_RATE_LIMIT_EXHAUSTED
        ),
    )
    .unwrap();
    let admission = crate::runtime_admission::ResolvedRuntime {
        role: "judge".into(),
        runtime: "opencode".into(),
        source: crate::runtime_admission::RuntimeSource::Explicit,
        adapter: PathBuf::new(),
        role_manifest: PathBuf::new(),
        runtime_manifest: PathBuf::new(),
        suggested_worker_type: None,
        preference: None,
        execution: None,
    };
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        apply_role_tick_provider_health_feedback(
            workspace.path(),
            &log_path,
            Some(&admission),
            anchor,
            Some(1),
        );
    });
    assert_eq!(captured.metrics.len(), 1, "{:?}", captured.metrics);
    assert_eq!(captured.metrics[0].labels["provider"], "loomtest");
    assert_eq!(captured.metrics[0].labels["reason"], "rate_limited");
}

/// #10003: an exit-0 codex tick whose adapter reports `SANDBOX_UNAVAILABLE`
/// (every exec refused by bubblewrap) is a failed tick, never `Success`. It
/// writes no account hold and no `last_success`, because the account is not at
/// fault, and it arms the host-wide sandbox hold the preference walk falls
/// through on.
#[test]
#[serial]
fn codex_role_tick_sandbox_no_op_is_a_failure_that_leaves_account_health_alone() {
    use crate::runtime_preference::sandbox_hold;
    struct ClearHold;
    impl Drop for ClearHold {
        fn drop(&mut self) {
            sandbox_hold::clear("codex");
        }
    }
    let _clear = ClearHold;
    sandbox_hold::clear("codex");
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    codex_judge_workspace(workspace.path());
    write_executable(
        &workspace.path().join(".loom/scripts/spawn-worker.sh"),
        "#!/bin/sh\n\
         echo '# LOOM_ACCOUNT name=alice'\n\
         echo 'exec'\n\
         echo ' exited 1 in 0ms:'\n\
         echo 'bwrap: No permissions to create a new namespace'\n\
         echo '# LOOM_RUNTIME_NOOP runtime=codex reason=sandbox-unavailable \
shape=exec-denied execs=1 denied=1 succeeded=0'\n\
         echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=alice \
category=SANDBOX_UNAVAILABLE exit_code=0 model=none'\n\
         exit 0\n",
    );

    let ws = crate::write_scope_test_support::WritableRoot::register(workspace.path());
    let outcome = judge_runner(workspace.path())
        .with_gh_bin(ws.gh.clone())
        .invoke("judge", "/loom:judge");
    let RoleTickOutcome::Failure(detail) = outcome else {
        panic!("a sandbox no-op must not be Success, got {outcome:?}");
    };
    assert!(
        detail.starts_with(super::super::launch::sandbox_noop::REASON_PREFIX),
        "{detail}"
    );
    assert!(detail.contains("shape=exec-denied execs=1 denied=1 succeeded=0"), "{detail}");

    let health = tokens_pool::account_health(workspace.path(), &codex_id("alice")).unwrap();
    assert!(
        health
            .as_ref()
            .is_none_or(|h| h.cooldown_until.is_none() && h.last_success.is_none()),
        "SANDBOX_UNAVAILABLE must neither hold the account nor stamp a success: {health:?}"
    );
    assert!(
        sandbox_hold::active("codex", epoch_now()).is_some(),
        "the no-op arms the host-wide sandbox hold"
    );
}

/// #10455: a codex tick refused because the account's session container was
/// not running ends as `category=SESSION_DOWN` (exit 78 kept). It must get a
/// distinct failure reason and `loom.admission.reason` — not bare
/// `RECOVERABLE`/78 — and, like the sandbox no-op, record no account hold and
/// no `last_success`: the account's credentials are fine.
#[test]
#[serial]
fn codex_role_tick_session_down_is_a_distinct_outcome_with_no_account_hold() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    codex_judge_workspace(workspace.path());
    write_executable(
        &workspace.path().join(".loom/scripts/spawn-worker.sh"),
        "#!/bin/sh\n\
         echo '# LOOM_ACCOUNT name=alice'\n\
         echo 'session container loom-codex-session-alice posture not verified (not-running)'\n\
         echo '# LOOM_TERMINAL_RESULT v=2 provider=codex account=alice \
category=SESSION_DOWN exit_code=78 model=none'\n\
         exit 78\n",
    );

    let ws = crate::write_scope_test_support::WritableRoot::register(workspace.path());
    let outcome = judge_runner(workspace.path())
        .with_gh_bin(ws.gh.clone())
        .invoke("judge", "/loom:judge");
    let RoleTickOutcome::Failure(detail) = &outcome else {
        panic!("a refused tick must be a Failure, got {outcome:?}");
    };
    assert!(crate::role_tick_telemetry::is_session_down_reason(detail), "{detail}");
    assert!(detail.contains("alice"), "{detail}");
    let attrs = crate::observability::lifecycle::admission_attributes(&outcome);
    assert_eq!(attrs["loom.admission.reason"], "session-down");

    let health = tokens_pool::account_health(workspace.path(), &codex_id("alice")).unwrap();
    assert!(
        health
            .as_ref()
            .is_none_or(|h| h.cooldown_until.is_none() && h.last_success.is_none()),
        "SESSION_DOWN must neither hold the account nor stamp a success: {health:?}"
    );
}

#[test]
fn session_down_parses_and_a_generic_failure_keeps_the_generic_reason() {
    use std::str::FromStr;
    assert_eq!(
        tokens_pool::health::TerminalClassification::from_str("SESSION_DOWN").unwrap(),
        tokens_pool::health::TerminalClassification::SessionDown
    );
    let generic = RoleTickOutcome::Failure("`x` exited with 78: boom".into());
    let attrs = crate::observability::lifecycle::admission_attributes(&generic);
    assert_eq!(attrs["loom.admission.reason"], "failure");
}
