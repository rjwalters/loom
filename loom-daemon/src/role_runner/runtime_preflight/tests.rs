//! Tests for the runtime-aware pre-spawn credential-pool gate (Issue #8408).
//!
//! Kept beside the module they cover rather than in `role_runner/tests.rs`,
//! which is at its `scripts/file-size-baseline.txt` ceiling.
//!
//! Every test that reaches `invoke()` leaves `spawn_bin` unset so runtime
//! admission — and with it the gate — runs for real against an on-disk
//! fixture, exactly like `mixed_runtime_role_launch_is_admitted_and_pinned_
//! before_spawn` in the parent test module.

use super::*;
use crate::tokens_pool::health::record_terminal_at;
use crate::tokens_pool::{record_terminal_for_class_at, AccountId, TerminalClassification};
use serial_test::serial;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicUsize;

/// Every env var that can change which runtime a role resolves to, or which
/// credential source `spawn-codex.sh` (and therefore the gate) would use.
/// Cleared for the scope of a test and restored on drop — including across an
/// assertion panic — so an ambient `LOOM_RUNTIME_JUDGE=codex` or `CODEX_HOME`
/// in a developer shell (or a dispatched sweep's own environment) can neither
/// mask nor fake the behaviour under test.
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

struct EnvGuard(Vec<(&'static str, Option<String>)>);

impl EnvGuard {
    /// Clear every guarded var, then point the codex profile root at
    /// `profile_root` so no test ever reads the host's real
    /// `~/.loom/codex-profiles`.
    fn new(profile_root: &Path) -> Self {
        let prior = GUARDED_ENV
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in GUARDED_ENV {
            std::env::remove_var(key);
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profile_root);
        Self(prior)
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

/// The installed `codex.json` shape: `accountProvider` is what makes
/// `spawn-codex.sh` select from the codex account pool.
const CODEX_MANIFEST: &str =
    r#"{"runtime":"codex","accountProvider":"codex","capabilities":{"mcp":"yes"}}"#;

/// A workspace whose `judge` role is pinned to the codex runtime (the config
/// the issue's live repro had: `runtimes.roles.judge = "codex"`), with a
/// codex-shaped model pin so #5028's mismatch refusal stays out of scope.
///
/// `claude_pool_exhausted` decides the state of the per-repo Claude pool: one
/// token, bad-marked (the live repro's "0/N spawnable") or healthy. A per-repo
/// pool always wins `resolve_tokens_dir`, so `LOOM_SHARED_TOKENS_DIR` never
/// needs touching here.
///
/// Returns the marker path the fake `spawn-worker.sh` touches when it runs.
fn codex_judge_workspace(root: &Path, manifest: &str, claude_pool_exhausted: bool) -> PathBuf {
    for sub in [
        ".loom/roles",
        ".loom/runtimes",
        ".loom/scripts",
        ".loom/tokens",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    if claude_pool_exhausted {
        fs::write(
            root.join(".loom/tokens/.bad_tokens"),
            format!("{} fake auth failure\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
        )
        .unwrap();
    }
    fs::write(
        root.join(".loom/config.json"),
        r#"{"runtimes":{"roles":{"judge":"codex"}},"autonomous":{"roleRunner":{"roleModels":{"judge":"gpt-5-codex"}}}}"#,
    )
    .unwrap();
    fs::write(root.join(".loom/roles/judge.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    fs::write(root.join(".loom/runtimes/codex.json"), manifest).unwrap();
    write_executable(&root.join(".loom/scripts/spawn-codex.sh"), "#!/bin/sh\nexit 0\n");
    let marker = root.join("script-ran");
    write_executable(
        &root.join(".loom/scripts/spawn-worker.sh"),
        &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
    );
    marker
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

fn judge_log(root: &Path) -> String {
    fs::read_to_string(root.join(".loom/logs/role-judge.log")).unwrap_or_default()
}

fn epoch_now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap()
}

// ---- AC1: the repro ---------------------------------------------------------

/// The issue's live repro, as a test: `runtimes.roles.judge = "codex"`, an
/// exhausted Claude pool (0/1 spawnable), and one valid codex account. The
/// tick must reach the spawn — not skip as `PoolExhausted` over a pool the
/// codex runtime never reads.
#[test]
#[serial]
fn codex_pinned_role_spawns_despite_an_exhausted_claude_pool() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = codex_judge_workspace(workspace.path(), CODEX_MANIFEST, true);

    // Precondition: this really is the exhausted-Claude-pool state that used
    // to gate the tick.
    let claude = crate::tokens_pool::select::spawnable_pool_state(workspace.path());
    assert_eq!((claude.total, claude.usable), (1, 0));

    let before = pool_exhausted_skip_count();
    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
    assert!(marker.exists(), "the codex-pinned tick must actually reach the spawn");
    assert_eq!(pool_exhausted_skip_count(), before, "no pool skip may be counted");
    assert!(
        !judge_log(workspace.path()).contains("SKIPPED BEFORE SPAWN"),
        "no pre-spawn skip marker may be written: {}",
        judge_log(workspace.path())
    );
}

// ---- AC2: the symmetric case ------------------------------------------------

/// No codex account at all, and a perfectly healthy Claude pool: the tick
/// skips pre-spawn, and the diagnostic names the codex account pool — never
/// `.loom/tokens`, which the codex runtime does not read.
#[test]
#[serial]
fn codex_pinned_role_with_no_codex_account_skips_naming_the_codex_pool() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    let marker = codex_judge_workspace(workspace.path(), CODEX_MANIFEST, false);

    let before = pool_exhausted_skip_count();
    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    let RoleTickOutcome::PoolExhausted {
        total, pool, hold, ..
    } = outcome
    else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!(pool, CredentialPool::CodexAccounts);
    assert_eq!(total, 0);
    // #8444: an unprovisioned pool can never clear on its own, so it must not
    // be reported as the self-healing hold.
    assert_eq!(hold, PoolHold::Unprovisioned);
    assert!(!marker.exists(), "the doomed spawn must never run");
    assert_eq!(pool_exhausted_skip_count(), before + 1);

    let log = judge_log(workspace.path());
    assert!(
        log.contains("SKIPPED BEFORE SPAWN (#6201): codex account pool exhausted"),
        "{log}"
    );
    assert!(log.contains("`loom-daemon accounts`"), "{log}");
    assert!(log.contains("no enabled codex account is provisioned"), "{log}");
    assert!(!log.contains(".loom/tokens"), "must not name the Claude pool dir: {log}");
    assert!(!log.contains(".ranking"), "must not name the Claude ranking file: {log}");
}

/// Accounts exist but every one is under an account-wide hold the selector
/// could not release: one needs re-auth (not session-managed), one is in an
/// exhaustion cooldown. Same skip, `total` counts the enabled accounts, and
/// the very next tick proceeds once a hold clears — no cached verdict.
#[test]
#[serial]
fn codex_pinned_role_skips_while_every_account_is_held_then_recovers() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    for name in ["alice", "bob"] {
        fs::create_dir(profiles.path().join(name)).unwrap();
    }
    let marker = codex_judge_workspace(workspace.path(), CODEX_MANIFEST, false);
    let now = epoch_now();
    record_terminal_at(
        workspace.path(),
        &codex_id("alice"),
        TerminalClassification::TokenExpired,
        "test",
        now,
    )
    .unwrap();
    record_terminal_at(
        workspace.path(),
        &codex_id("bob"),
        TerminalClassification::TokenExhausted,
        "test",
        now,
    )
    .unwrap();

    let mut runner = judge_runner(workspace.path());
    let outcome = runner.invoke("judge", "/loom:judge");
    let RoleTickOutcome::PoolExhausted {
        total,
        pool,
        next_clear_at,
        hold,
    } = outcome
    else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!((total, pool), (2, CredentialPool::CodexAccounts));
    // #8444: cooldowns/re-auth holds ARE the self-healing case — the one this
    // variant was written for, and the only one kept out of the escalation
    // path.
    assert_eq!(hold, PoolHold::SelfHealing);
    assert!(next_clear_at <= chrono::Utc::now() + chrono::Duration::seconds(901));
    assert!(!marker.exists());
    assert!(
        judge_log(workspace.path())
            .contains("every enabled account is cooling down or needs re-auth"),
        "{}",
        judge_log(workspace.path())
    );

    // Readmission: bob's next run succeeds elsewhere, clearing his cooldown.
    record_terminal_at(
        workspace.path(),
        &codex_id("bob"),
        TerminalClassification::Success,
        "test",
        now + 1,
    )
    .unwrap();
    assert_eq!(runner.invoke("judge", "/loom:judge"), RoleTickOutcome::Success);
    assert!(marker.exists());
}

// ---- the gate never skips a launch that could have succeeded -----------------

/// The pool reader counts only holds the spawn-time selector could not get
/// past: a class-scoped credit hold (#8058) blocks one model class, and a
/// re-auth hold on a session-managed profile can be released by the selector's
/// own probe (#6927) — neither makes the pool "empty".
#[test]
#[serial]
fn codex_pool_state_counts_only_holds_the_selector_cannot_release() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    for name in ["class-held", "session-reauth", "hard-reauth", "cooling"] {
        fs::create_dir(profiles.path().join(name)).unwrap();
    }
    fs::write(
        profiles
            .path()
            .join("session-reauth")
            .join(crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE),
        "{}",
    )
    .unwrap();
    let root = workspace.path();
    fs::create_dir_all(root.join(".loom")).unwrap();
    let now = epoch_now();
    record_terminal_for_class_at(
        root,
        &codex_id("class-held"),
        TerminalClassification::ModelCreditsExhausted,
        Some("opus"),
        "test",
        now,
    )
    .unwrap();
    for name in ["session-reauth", "hard-reauth"] {
        record_terminal_at(
            root,
            &codex_id(name),
            TerminalClassification::TokenExpired,
            "test",
            now,
        )
        .unwrap();
    }
    record_terminal_at(
        root,
        &codex_id("cooling"),
        TerminalClassification::TokenExhausted,
        "test",
        now,
    )
    .unwrap();

    let state = codex_pool_state(root, now);
    assert_eq!(state.read_error, None);
    assert_eq!(state.enabled, 4);
    assert_eq!(state.spawnable, 2, "class-held and session-reauth stay spawnable: {state:?}");
    assert!(state.earliest_clear.is_some_and(|deadline| deadline > now), "{state:?}");
}

/// An unreadable health state is one of the two fail-closed cases — the
/// selector reads the same file and exits 78 on it — and the skip says why.
#[test]
#[serial]
fn codex_pool_state_fails_closed_on_an_unreadable_health_state() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    fs::create_dir_all(workspace.path().join(".loom")).unwrap();
    fs::write(workspace.path().join(".loom/account-health.json"), "{not json").unwrap();

    let state = codex_pool_state(workspace.path(), epoch_now());
    assert_eq!((state.enabled, state.spawnable), (1, 0));
    // #8444: the read error names the file that failed, so the skip text
    // cannot blame the inventory for a health-state parse error.
    let (file, error) = state.read_error.clone().expect("a read error");
    assert_eq!(file, PoolStateFile::HealthState);
    assert!(error.contains("account-health.json"), "{error}");
}

/// The other fail-closed case (#8444): the **inventory** itself is malformed.
/// `spawn-codex.sh`'s selector reads the same `.loom/accounts.json` through
/// the same reader and exits 78 on it, so the gate must skip — and must say
/// which file to repair, through `invoke()`, not just at the state-reader
/// level. The pool reads as 0 enabled because nothing could be enumerated.
#[test]
#[serial]
fn codex_pinned_role_fails_closed_on_a_malformed_account_inventory() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = codex_judge_workspace(workspace.path(), CODEX_MANIFEST, false);
    fs::write(workspace.path().join(".loom/accounts.json"), "{not json").unwrap();

    let state = codex_pool_state(workspace.path(), epoch_now());
    let (file, _) = state.read_error.clone().expect("a read error");
    assert_eq!(file, PoolStateFile::Inventory);
    assert_eq!((state.enabled, state.spawnable), (0, 0), "{state:?}");

    let before = pool_exhausted_skip_count();
    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    let RoleTickOutcome::PoolExhausted { pool, hold, .. } = outcome else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!(pool, CredentialPool::CodexAccounts, "#8444: gated_pool stays the codex pool");
    assert_eq!(hold, PoolHold::Unreadable(PoolStateFile::Inventory));
    assert!(!marker.exists(), "a launch the selector would kill must never run");
    assert_eq!(pool_exhausted_skip_count(), before + 1);

    let log = judge_log(workspace.path());
    assert!(
        log.contains("the account inventory (.loom/accounts.json) could not be read"),
        "{log}"
    );
    assert!(!log.contains("account-health.json"), "must not blame the health state: {log}");
}

/// The health-state half of the same claim, through `invoke()`: the skip text
/// names `account-health.json` — never the inventory, which read fine.
#[test]
#[serial]
fn codex_pinned_role_skip_text_names_the_health_state_when_that_is_what_failed() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = codex_judge_workspace(workspace.path(), CODEX_MANIFEST, false);
    fs::write(workspace.path().join(".loom/account-health.json"), "{not json").unwrap();

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");
    let RoleTickOutcome::PoolExhausted { total, hold, .. } = outcome else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!(hold, PoolHold::Unreadable(PoolStateFile::HealthState));
    assert_eq!(total, 1, "the inventory read fine, so its count is still reported");
    assert!(!marker.exists());

    let log = judge_log(workspace.path());
    assert!(
        log.contains("the account health state (.loom/account-health.json) could not be read"),
        "{log}"
    );
    assert!(
        !log.contains("the account inventory (.loom/accounts.json) could not be read"),
        "must not blame the inventory: {log}"
    );
}

/// #8444 AC1/AC2: the two permanent holds must reach the stuck-role /
/// escalation path, which is built on a byte-identical `detail` repeating
/// tick after tick. So their detail carries no timestamp and no moving count
/// — asserted by recording the same outcome twice, a simulated second apart,
/// and requiring the two details to be equal; the self-healing hold's detail
/// (deliberately volatile) must NOT be.
#[test]
fn a_permanent_pool_hold_has_a_stable_detail_and_a_self_healing_one_does_not() {
    let at = chrono::Utc::now();
    let detail = |hold, next_clear_at| {
        RoleTickOutcome::PoolExhausted {
            total: 0,
            next_clear_at,
            pool: CredentialPool::CodexAccounts,
            hold,
        }
        .pool_exhausted_detail()
    };
    let later = at + chrono::Duration::seconds(1);
    for hold in [
        PoolHold::Unprovisioned,
        PoolHold::Unreadable(PoolStateFile::Inventory),
        PoolHold::Unreadable(PoolStateFile::HealthState),
    ] {
        assert_eq!(detail(hold, at), detail(hold, later), "{hold:?} must be stable");
        assert!(!hold.is_self_healing(), "{hold:?}");
    }
    assert_ne!(
        detail(PoolHold::SelfHealing, at),
        detail(PoolHold::SelfHealing, later),
        "the self-healing detail stays volatile, so it can never build a streak"
    );
    assert!(detail(PoolHold::Unprovisioned, at).starts_with("codex-account-pool-exhausted: "));
}

/// The same split, at the routing layer: only a self-healing hold belongs in
/// `health::RoleTickSummary::pool_exhausted`'s disjoint bucket (#7607). The
/// permanent ones fall through to `persistent`, where an identical-detail
/// streak becomes an escalation — the behaviour `NoTokenPool` already had.
#[test]
fn only_a_self_healing_hold_is_routed_to_the_disjoint_pool_exhausted_bucket() {
    let exhausted = |hold| RoleTickOutcome::PoolExhausted {
        total: 0,
        next_clear_at: chrono::Utc::now(),
        pool: CredentialPool::CodexAccounts,
        hold,
    };
    assert!(exhausted(PoolHold::SelfHealing).self_healing_pool_hold());
    assert!(!exhausted(PoolHold::Unprovisioned).self_healing_pool_hold());
    assert!(!exhausted(PoolHold::Unreadable(PoolStateFile::HealthState)).self_healing_pool_hold());
    assert!(!RoleTickOutcome::NoTokenPool.self_healing_pool_hold());
    assert!(!RoleTickOutcome::Success.self_healing_pool_hold());
    // Whatever the hold, the skip still names the pool it read.
    for hold in [PoolHold::SelfHealing, PoolHold::Unprovisioned] {
        assert_eq!(exhausted(hold).gated_pool(), Some("codex_accounts"));
    }
}

/// #8444 AC1 end to end: a codex-pinned role on a host with zero enabled
/// codex accounts reaches the **escalation path**, exactly as the Claude
/// pool's permanent `NoTokenPool` state does — it builds the byte-identical
/// streak `health::assess_role_liveness` reads as a STUCK role, and lands in
/// `summarize_role_ticks`'s `persistent`/`escalated` lists rather than the
/// disjoint self-healing `pool_exhausted` bucket. The self-healing hold is
/// the control: same variant, same pool, opposite routing.
#[test]
#[serial(role_tick_ring)]
fn a_permanently_empty_codex_pool_escalates_while_a_cooldown_hold_does_not() {
    let at = chrono::Utc::now();
    let tick = |hold, i: i64| RoleTickOutcome::PoolExhausted {
        total: 0,
        // Moves every tick — the self-healing detail's volatility comes from
        // here, and a permanent hold's detail must ignore it.
        next_clear_at: at + chrono::Duration::seconds(i),
        pool: CredentialPool::CodexAccounts,
        hold,
    };
    let threshold = i64::try_from(crate::health::ROLE_TICK_ESCALATION_THRESHOLD).unwrap();
    let summarize = |root: &Path, hold| {
        reset_role_tick_ring();
        reset_last_role_tick_map();
        for i in 0..threshold {
            record_role_tick_at("judge", root, &tick(hold, i), at + chrono::Duration::seconds(i));
        }
        let streak = last_role_tick_snapshot()
            .into_iter()
            .find(|t| t.role == "judge" && t.root == root)
            .expect("a recorded tick");
        (streak, crate::health::summarize_role_ticks(&role_tick_records(), at))
    };

    let root = Path::new("/repo/unprovisioned");
    let (streak, summary) = summarize(root, PoolHold::Unprovisioned);
    assert!(!streak.ok);
    assert_eq!(
        streak.consecutive_identical_failures,
        crate::health::ROLE_TICK_ESCALATION_THRESHOLD,
        "a permanent hold must repeat byte-identically: {:?}",
        streak.detail
    );
    assert_eq!(summary.pool_exhausted, vec![], "not the self-healing bucket");
    assert_eq!(summary.persistent.len(), 1, "{summary:?}");
    assert_eq!(summary.escalated.len(), 1, "{summary:?}");
    assert!(summary.escalated[0]
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("codex-account-pool-exhausted")));

    let root = Path::new("/repo/cooling");
    let (streak, summary) = summarize(root, PoolHold::SelfHealing);
    assert_eq!(
        streak.consecutive_identical_failures, 1,
        "the self-healing detail stays volatile, so no streak forms"
    );
    assert_eq!(summary.persistent, vec![], "{summary:?}");
    assert_eq!(summary.escalated, vec![], "{summary:?}");
    assert_eq!(summary.pool_exhausted.len(), 1, "{summary:?}");
}

/// The repeat-skip DEBUG line's clause (#8444): pool-aware, so a codex skip
/// never reports "token pool", and hold-aware, so a pool that was never
/// provisioned is not described as exhausted.
#[test]
fn the_repeat_skip_clause_names_the_gated_pool_and_its_hold() {
    let claude = CredentialPool::ClaudeTokens.repeat_phrase(PoolHold::SelfHealing);
    assert_eq!(claude, "token pool still exhausted");
    let codex = CredentialPool::CodexAccounts.repeat_phrase(PoolHold::SelfHealing);
    assert_eq!(codex, "codex account pool still exhausted");
    let unprovisioned = CredentialPool::CodexAccounts.repeat_phrase(PoolHold::Unprovisioned);
    assert!(!unprovisioned.contains("token pool"), "{unprovisioned}");
    assert!(unprovisioned.contains("no account provisioned"), "{unprovisioned}");
    let unreadable =
        CredentialPool::CodexAccounts.repeat_phrase(PoolHold::Unreadable(PoolStateFile::Inventory));
    assert!(unreadable.contains(".loom/accounts.json"), "{unreadable}");
}

/// An explicit profile pin means `spawn-codex.sh` never reaches the account
/// selector, so an empty account pool is not the wall — the tick proceeds.
#[test]
#[serial]
fn codex_gate_stands_down_for_an_explicit_profile_pin() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    let marker = codex_judge_workspace(workspace.path(), CODEX_MANIFEST, false);
    std::env::set_var("LOOM_CODEX_PROFILE", "pinned-elsewhere");

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");
    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
    assert!(marker.exists());
}

/// A codex manifest with no `accountProvider` (an install that predates the
/// key) makes `spawn-codex.sh` fall open to the `claude` provider, so the
/// codex account pool is not what that launch selects from — no codex gate.
#[test]
#[serial]
fn codex_gate_stands_down_when_the_manifest_names_no_codex_account_provider() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    let marker = codex_judge_workspace(
        workspace.path(),
        r#"{"runtime":"codex","capabilities":{"mcp":"yes"}}"#,
        false,
    );

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");
    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
    assert!(marker.exists());
}

// ---- AC3: the claude runtime is byte-identical -------------------------------

/// The Claude-runtime skip — outcome, counter, and the role-log line — is what
/// it was before #8408, byte for byte. The expected line is rebuilt here from
/// the pre-#8408 literal, not from the production formatter.
#[test]
#[serial]
fn claude_runtime_pool_exhausted_skip_is_byte_identical() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    let root = workspace.path();
    fs::create_dir_all(root.join(".loom/scripts")).unwrap();
    fs::create_dir_all(root.join(".loom/tokens")).unwrap();
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    fs::write(
        root.join(".loom/tokens/.bad_tokens"),
        format!("{} fake auth failure\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
    )
    .unwrap();
    let marker = root.join("script-ran");
    write_executable(
        &root.join(".loom/scripts/spawn-worker.sh"),
        &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
    );

    let before = pool_exhausted_skip_count();
    let outcome = ScriptRoleInvocationRunner::new(root.to_path_buf()).invoke("curator", "/x");
    let RoleTickOutcome::PoolExhausted {
        total,
        next_clear_at,
        pool,
        hold,
    } = outcome
    else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!((total, pool), (1, CredentialPool::ClaudeTokens));
    assert_eq!(hold, PoolHold::SelfHealing, "#8444: the Claude arm is only ever self-healing");
    assert!(!marker.exists());
    assert_eq!(pool_exhausted_skip_count(), before + 1);

    let pool_dir = crate::tokens_pool::select::spawnable_pool_state(root).dir;
    let expected_reason = format!(
        "token pool exhausted: 0/1 spawnable in {} (every account bad-marked or hard-excluded by \
         .ranking); next check ~{} — run `loom-daemon tokens check --ranking` or `loom-daemon \
         tokens unblock <name>` — #7607",
        pool_dir.display(),
        next_clear_at.to_rfc3339()
    );
    let log = fs::read_to_string(root.join(".loom/logs/role-curator.log")).unwrap();
    assert!(
        log.ends_with(&format!(
            " role=curator SKIPPED BEFORE SPAWN (#6201): {expected_reason} ====\n"
        )),
        "{log}"
    );
}

/// The pre-#8408 literals every Claude-pool rendering is now built from.
#[test]
fn claude_pool_renderings_are_the_pre_8408_literals() {
    assert_eq!(
        CredentialPool::ClaudeTokens.exhausted_phrase(21),
        "token pool exhausted: 0/21 spawnable (every account bad-marked or hard-excluded by \
         .ranking)"
    );
    assert_eq!(CredentialPool::ClaudeTokens.detail_tag(), "pool-exhausted");
    let codex = CredentialPool::CodexAccounts.exhausted_phrase(4);
    assert!(codex.starts_with("codex account pool exhausted: 0/4 spawnable"), "{codex}");
    assert!(!codex.contains(".loom/tokens") && !codex.contains(".ranking"), "{codex}");
}

// ---- AC4: which pool gated the skip ------------------------------------------

#[test]
fn gated_pool_names_the_pool_that_was_read() {
    let at = chrono::Utc::now();
    let exhausted = |pool| RoleTickOutcome::PoolExhausted {
        total: 2,
        next_clear_at: at,
        pool,
        hold: PoolHold::SelfHealing,
    };
    assert_eq!(exhausted(CredentialPool::ClaudeTokens).gated_pool(), Some("claude_tokens"));
    assert_eq!(exhausted(CredentialPool::CodexAccounts).gated_pool(), Some("codex_accounts"));
    assert_eq!(RoleTickOutcome::NoTokenPool.gated_pool(), Some("claude_tokens"));
    assert_eq!(RoleTickOutcome::Success.gated_pool(), None);
    assert_eq!(RoleTickOutcome::Failure("boom".into()).gated_pool(), None);
}

// ---- the sweep-dispatch brake is a Claude-pool signal only -------------------

#[derive(Default)]
struct CountingObserver(AtomicUsize);

impl PoolExhaustedObserver for CountingObserver {
    fn note_pool_exhausted(&self, _root: &Path, _role: &str) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// #7607's observer feeds the #6614 brake that holds *sweep* dispatch on a
/// token-selection wall. A dry codex account pool must never trip it: sweeps
/// do not draw from the pool the codex-pinned role found empty.
#[test]
fn a_dry_codex_pool_never_feeds_the_sweep_dispatch_brake() {
    let observer = CountingObserver::default();
    let root = Path::new("/repo/a");
    let exhausted = |pool| RoleTickOutcome::PoolExhausted {
        total: 3,
        next_clear_at: chrono::Utc::now(),
        pool,
        hold: PoolHold::SelfHealing,
    };

    feed_pool_exhausted_observer(
        Some(&observer),
        &exhausted(CredentialPool::CodexAccounts),
        root,
        "judge",
    );
    assert_eq!(observer.0.load(Ordering::Relaxed), 0);

    feed_pool_exhausted_observer(
        Some(&observer),
        &exhausted(CredentialPool::ClaudeTokens),
        root,
        "judge",
    );
    assert_eq!(observer.0.load(Ordering::Relaxed), 1);
}

// ---- #8554: the preference list decides the tap, and fails closed ----

/// A workspace with NO per-role runtime pin — `judge` resolves to the
/// built-in default (`claude`) exactly as an unconfigured install would —
/// so `config_extra` is free to install `runtimes.preference` /
/// `runtimes.rolePreference` instead. Otherwise identical to
/// `codex_judge_workspace`: a real (if minimal) `judge` role manifest, a
/// `codex` runtime manifest, and executable adapter stubs for **both**
/// runtimes — so `claude` is genuinely admitted and a skip of it is the
/// pool-exhaustion shape under test, never the materially different
/// `not-admitted(...)` a missing adapter would produce. No
/// `.loom/runtimes/claude.json` is written: #4688's bundled zero-config
/// fallback covers it, exactly as on a real install.
///
/// The fake `spawn-worker.sh` records `$LOOM_RUNTIME` rather than merely
/// touching a marker, so a test can assert **which** tap the tick launched
/// on — the difference between "a spawn happened" and "the preference list
/// actually re-pointed the launch".
fn preference_judge_workspace(
    root: &Path,
    config_extra: &serde_json::Value,
    claude_pool_exhausted: bool,
) -> PathBuf {
    for sub in [
        ".loom/roles",
        ".loom/runtimes",
        ".loom/scripts",
        ".loom/tokens",
    ] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::write(root.join(".loom/tokens/fake.token"), "sk-ant-oat01-fake").unwrap();
    if claude_pool_exhausted {
        fs::write(
            root.join(".loom/tokens/.bad_tokens"),
            format!("{} fake auth failure\n", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
        )
        .unwrap();
    }
    fs::write(root.join(".loom/config.json"), config_extra.to_string()).unwrap();
    fs::write(root.join(".loom/roles/judge.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    fs::write(root.join(".loom/runtimes/codex.json"), CODEX_MANIFEST).unwrap();
    write_executable(&root.join(".loom/scripts/spawn-codex.sh"), "#!/bin/sh\nexit 0\n");
    write_executable(&root.join(".loom/scripts/spawn-claude.sh"), "#!/bin/sh\nexit 0\n");
    let marker = root.join("script-ran");
    write_executable(
        &root.join(".loom/scripts/spawn-worker.sh"),
        &format!("#!/bin/sh\nprintf '%s' \"$LOOM_RUNTIME\" >'{}'\nexit 0\n", marker.display()),
    );
    marker
}

/// The genuine fall-through: `claude` is listed FIRST and admitted, its pool
/// is dry, so the walk records `unavailable(claude_tokens: …)` for it and
/// lands on the codex tap below — the shape a fleet with
/// `preference: ["claude", "codex", …]` actually runs.
#[test]
#[serial]
fn a_dry_first_tap_falls_through_to_the_next_listed_tap() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = preference_judge_workspace(
        workspace.path(),
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
        true,
    );

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
    assert_eq!(fs::read_to_string(&marker).unwrap_or_default(), "codex");
}

/// …and the same list with a HEALTHY first tap stays on it: the fleet-wide
/// `preference` key must not move work off a subscription seat that can serve
/// it (the "never pass over a tap that could have served" half of the
/// availability contract).
#[test]
#[serial]
fn a_healthy_first_tap_keeps_the_tick_on_it() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = preference_judge_workspace(
        workspace.path(),
        &serde_json::json!({"runtimes": {"preference": ["claude", "codex"]}}),
        false,
    );

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
    assert_eq!(fs::read_to_string(&marker).unwrap_or_default(), "claude");
}

/// The core #8554 acceptance criterion: Claude (the static default, no
/// `runtimes.roles.judge` pin at all) is dry, but `rolePreference.judge`
/// names a codex tap with a healthy account — the tick must reach the spawn
/// on codex, not skip as `PoolExhausted` over a Claude pool it no longer
/// needs. It is also the `rolePreference.judge` acceptance criterion: the key
/// that resolves is the role-scoped one, which outranks (and here exists
/// without) the fleet-wide `runtimes.preference`.
#[test]
#[serial]
fn an_exhausted_claude_pool_falls_through_via_role_preference_instead_of_skipping() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = preference_judge_workspace(
        workspace.path(),
        &serde_json::json!({"runtimes": {"rolePreference": {"judge": ["codex"]}}}),
        true,
    );

    let before = pool_exhausted_skip_count();
    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
    assert_eq!(
        fs::read_to_string(&marker).unwrap_or_default(),
        "codex",
        "the tick must fall through to codex and actually spawn"
    );
    assert_eq!(pool_exhausted_skip_count(), before, "no pool skip may be counted");
    let log = judge_log(workspace.path());
    assert!(!log.contains("SKIPPED BEFORE SPAWN"), "{log}");
}

/// `rolePreference.judge` is honoured on a **healthy** Claude pool too — the
/// whole point of the key (Judge independence: a native sweep reviews in the
/// same session that wrote the change, so Judge must be able to run on a
/// different tap than the builder). A list consulted only when the top tap is
/// exhausted would silently ignore this configuration for as long as the
/// Claude pool stayed healthy, i.e. almost always.
#[test]
#[serial]
fn role_preference_decides_the_tap_even_when_the_static_pool_is_healthy() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = preference_judge_workspace(
        workspace.path(),
        &serde_json::json!({"runtimes": {"rolePreference": {"judge": ["codex", "claude"]}}}),
        false,
    );

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
    assert_eq!(
        fs::read_to_string(&marker).unwrap_or_default(),
        "codex",
        "a healthy Claude pool must not override the operator's ordering"
    );
}

/// With **no** preference list the same fixture is byte-identical to
/// pre-#8554 behaviour: static resolution picks `claude`, the healthy pool
/// gate passes, and the launch goes to claude even though a perfectly
/// spawnable codex seat exists. The control for the test above — it is the
/// preference key that moves the tap, not the fixture.
#[test]
#[serial]
fn no_preference_list_leaves_the_static_runtime_in_charge() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = preference_judge_workspace(workspace.path(), &serde_json::json!({}), false);

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    assert_eq!(outcome, RoleTickOutcome::Success, "{outcome:?}");
    assert_eq!(fs::read_to_string(&marker).unwrap_or_default(), "claude");
}

/// The fail-closed half: every tap in the list is unavailable too (here,
/// zero codex accounts), so resolution yields no tap and the tick reports the
/// statically-admitted runtime's own Claude-pool skip — byte-identical to the
/// pre-#8554 outcome (self-healing, kept out of #7607's stuck-role streak),
/// not a new failure class.
#[test]
#[serial]
fn a_wholly_unavailable_role_preference_list_still_skips_pre_spawn() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    // No codex profile directories created: the codex pool is provisioned
    // (the manifest/adapter exist) but has zero enabled accounts.
    let marker = preference_judge_workspace(
        workspace.path(),
        &serde_json::json!({"runtimes": {"rolePreference": {"judge": ["codex"]}}}),
        true,
    );

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    let RoleTickOutcome::PoolExhausted { pool, hold, .. } = outcome else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!(pool, CredentialPool::ClaudeTokens, "the ORIGINAL skip stands, unchanged");
    assert_eq!(hold, PoolHold::SelfHealing);
    assert!(!marker.exists(), "a doomed spawn must never run");
}

/// The fail-closed remainder: the list (`["codex"]`, unavailable) excludes
/// the statically-admitted runtime, whose pool is **healthy** — so there is
/// no pool skip to borrow. The tick must still refuse, reporting the
/// preference list's own exhausted diagnostic, and must NOT quietly launch
/// the unlisted-but-healthy runtime, which would route around the operator's
/// configuration.
#[test]
#[serial]
fn an_unavailable_list_refuses_rather_than_launching_an_unlisted_runtime() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    // No codex profile directories: the only listed tap is unavailable. The
    // Claude pool is deliberately HEALTHY (no `.bad_tokens`).
    let marker = preference_judge_workspace(
        workspace.path(),
        &serde_json::json!({"runtimes": {"rolePreference": {"judge": ["codex"]}}}),
        false,
    );

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    let RoleTickOutcome::RuntimeRejected(rejection) = &outcome else {
        panic!("expected a fail-closed RuntimeRejected, got {outcome:?}");
    };
    assert_eq!(
        rejection.source,
        crate::runtime_admission::RuntimeSource::Preference,
        "{rejection:?}"
    );
    assert!(rejection.reason.contains("preference order: codex"), "{}", rejection.reason);
    assert!(!marker.exists(), "no tap could serve the work, so nothing may spawn");
    assert!(judge_log(workspace.path()).contains("SKIPPED BEFORE SPAWN"));
}

/// A role-scoped operator pin (`LOOM_RUNTIME_JUDGE`) outranks
/// `rolePreference.judge` and disables fall-through, exactly as
/// `runtime_preference`'s own invariant states — a pin is a deliberate
/// operator act, and silently routing around it at the pre-spawn gate would
/// make it useless for the debugging it exists for.
#[test]
#[serial]
fn an_operator_pin_disables_preference_fall_through_at_the_pre_spawn_gate() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    fs::create_dir(profiles.path().join("alice")).unwrap();
    let marker = preference_judge_workspace(
        workspace.path(),
        &serde_json::json!({"runtimes": {"rolePreference": {"judge": ["codex"]}}}),
        true,
    );
    std::env::set_var("LOOM_RUNTIME_JUDGE", "claude");

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    let RoleTickOutcome::PoolExhausted { pool, .. } = outcome else {
        panic!("expected PoolExhausted, got {outcome:?}");
    };
    assert_eq!(pool, CredentialPool::ClaudeTokens, "the pin must keep the tick on claude");
    assert!(!marker.exists(), "a pinned-but-dry launch must still skip, never fall through");
}
