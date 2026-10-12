//! Issue #5345 — `daemon.delegatedTo` gates `loom-daemon workspace
//! add|set-priority|remove` and `loom-daemon tokens bootstrap` when the
//! invoking (workspace, resp. target) repo declares delegation, while
//! leaving `workspace list` and `tokens select` (read-only / hot-path
//! client actions) unaffected.
//!
//! Spawns the real compiled `loom-daemon` binary (`CARGO_BIN_EXE_loom-daemon`)
//! — mirrors the existing `accounts_cli.rs` pattern — rather than calling the
//! CLI handler functions in-process, so this exercises the actual argument
//! parsing + exit-code contract an operator sees.

use std::path::Path;
use std::process::{Command, Output};

/// Build a minimal Loom-repo-shaped fixture: a `.git` marker (a bare
/// directory is sufficient — `repo_root::find_repo_root` only checks
/// existence, not validity) and a `.loom/config.json`. Both are required for
/// `resolve_repo_root(".")` to resolve the fixture as a repo at all (see
/// `loom-daemon/src/repo_root.rs`).
fn write_fixture_repo(config_json: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(dir.path().join(".loom").join("config.json"), config_json).unwrap();
    dir
}

const DELEGATED_CONFIG: &str = r#"{"daemon": {"delegatedTo": "/Users/alice/GitHub/other-repo"}}"#;

fn run_daemon(args: &[&str], cwd: &Path, workspaces_path: &Path) -> Output {
    run_daemon_with_shared_pool(args, cwd, workspaces_path, "")
}

/// As [`run_daemon`], with an explicit `LOOM_SHARED_TOKENS_DIR`. Since issue
/// #9135 the shared machine-level pool is the only location a token pool may
/// live in — the fixture repo has a `.git` marker, so a pool seeded at
/// `<fixture>/.loom/tokens` is deliberately refused — so any test that needs a
/// *usable* pool, or a bootstrap destination, names one here.
fn run_daemon_with_shared_pool(
    args: &[&str],
    cwd: &Path,
    workspaces_path: &Path,
    shared_tokens_dir: &str,
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(args)
        .current_dir(cwd)
        .env("LOOM_WORKSPACES_PATH", workspaces_path)
        // Deterministic regardless of host machine-level defaults tier
        // (issue #4039's private/shared defaults file) or a real
        // ~/.loom/tokens shared pool leaking into `tokens select`.
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        // No Codex profile root: `workspace add`/`remove` must not ask the
        // host's real docker about session containers (#10364).
        .env("LOOM_CODEX_PROFILE_ROOT", "")
        .env("LOOM_SHARED_TOKENS_DIR", shared_tokens_dir)
        .output()
        .unwrap()
}

// ===== workspace add/set-priority/remove: gated =====

#[test]
fn workspace_add_refuses_when_invoking_repo_is_delegated() {
    let fixture = write_fixture_repo(DELEGATED_CONFIG);
    let registry = fixture.path().join("workspaces.json");
    let target = fixture.path().join("some-other-repo");

    let output =
        run_daemon(&["workspace", "add", target.to_str().unwrap()], fixture.path(), &registry);

    assert!(!output.status.success(), "workspace add must refuse under delegation");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("/Users/alice/GitHub/other-repo"),
        "stderr must name the delegate repo, got: {stderr}"
    );
    assert!(
        !registry.exists()
            || std::fs::read_to_string(&registry)
                .unwrap()
                .trim()
                .is_empty(),
        "the registry must not have been mutated"
    );
}

#[test]
fn workspace_set_priority_refuses_when_invoking_repo_is_delegated() {
    let fixture = write_fixture_repo(DELEGATED_CONFIG);
    let registry = fixture.path().join("workspaces.json");
    let target = fixture.path().join("some-other-repo");

    let output = run_daemon(
        &["workspace", "set-priority", target.to_str().unwrap(), "5"],
        fixture.path(),
        &registry,
    );

    assert!(!output.status.success(), "workspace set-priority must refuse under delegation");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("/Users/alice/GitHub/other-repo"),
        "stderr must name the delegate repo, got: {stderr}"
    );
}

#[test]
fn workspace_remove_refuses_when_invoking_repo_is_delegated() {
    let fixture = write_fixture_repo(DELEGATED_CONFIG);
    let registry = fixture.path().join("workspaces.json");
    let target = fixture.path().join("some-other-repo");

    let output =
        run_daemon(&["workspace", "remove", target.to_str().unwrap()], fixture.path(), &registry);

    assert!(!output.status.success(), "workspace remove must refuse under delegation");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("/Users/alice/GitHub/other-repo"),
        "stderr must name the delegate repo, got: {stderr}"
    );
}

// ===== workspace list: NOT gated =====

#[test]
fn workspace_list_is_not_gated_by_delegation() {
    let fixture = write_fixture_repo(DELEGATED_CONFIG);
    let registry = fixture.path().join("workspaces.json");

    let output = run_daemon(&["workspace", "list"], fixture.path(), &registry);

    assert!(
        output.status.success(),
        "workspace list is read-only and must never be gated, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ===== tokens bootstrap: gated on --workspace target =====

#[test]
fn tokens_bootstrap_refuses_when_target_workspace_is_delegated() {
    let fixture = write_fixture_repo(DELEGATED_CONFIG);
    let registry = fixture.path().join("workspaces.json");

    let output = run_daemon(
        &[
            "tokens",
            "bootstrap",
            "--workspace",
            fixture.path().to_str().unwrap(),
        ],
        fixture.path(),
        &registry,
    );

    assert!(!output.status.success(), "tokens bootstrap must refuse under delegation");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("/Users/alice/GitHub/other-repo"),
        "stderr must name the delegate repo, got: {stderr}"
    );
    assert!(
        !fixture.path().join(".loom").join("tokens").exists(),
        "bootstrap must not have written a token pool"
    );
}

// ===== tokens select: NOT gated (read-only, spawn hot path) =====

#[test]
fn tokens_select_still_succeeds_when_workspace_is_delegated() {
    let fixture = write_fixture_repo(DELEGATED_CONFIG);
    let registry = fixture.path().join("workspaces.json");

    // A minimal pre-provisioned pool — `tokens select` finding a token has
    // nothing to do with `tokens bootstrap` (deliberately not exercised
    // here), so seed the pool directly. It goes in the SHARED machine-level
    // location: the fixture repo carries a `.git` marker, and since #9135 a
    // pool inside a git worktree is refused.
    let shared = tempfile::tempdir().unwrap();
    std::fs::write(shared.path().join("alice.token"), "fake-oauth-token-value\n").unwrap();

    let output = run_daemon_with_shared_pool(
        &[
            "tokens",
            "select",
            "--workspace",
            fixture.path().to_str().unwrap(),
            "--no-key",
        ],
        fixture.path(),
        &registry,
        shared.path().to_str().unwrap(),
    );

    assert!(
        output.status.success(),
        "tokens select must remain unaffected by delegation, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("delegated"),
        "tokens select must not be gated by daemon.delegatedTo, stderr: {stderr}"
    );
}

// ===== Retired in-worktree pool (issue #9135) =====

/// End-to-end through the real binary: a pool seeded at
/// `<repo>/.loom/tokens` inside a git worktree is refused, and the failure
/// names the pool plus the migration instead of silently using it.
#[test]
fn tokens_select_refuses_a_pool_inside_a_git_worktree() {
    let fixture = write_fixture_repo(r#"{"nextAgentNumber": 1}"#);
    let registry = fixture.path().join("workspaces.json");

    let in_repo_pool = fixture.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&in_repo_pool).unwrap();
    std::fs::write(in_repo_pool.join("alice.token"), "fake-oauth-token-value\n").unwrap();

    let output = run_daemon(
        &[
            "tokens",
            "select",
            "--workspace",
            fixture.path().to_str().unwrap(),
            "--no-key",
        ],
        fixture.path(),
        &registry,
    );

    assert!(
        !output.status.success(),
        "an in-worktree pool must not satisfy selection, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("REFUSED TOKEN POOL") && stderr.contains("#9135"),
        "the error must explain the refusal, stderr: {stderr}"
    );
    assert!(
        stderr.contains(&in_repo_pool.display().to_string()),
        "the error must name the refused pool, stderr: {stderr}"
    );
    // Refused, not repaired: the credentials are still exactly where they were.
    assert!(in_repo_pool.join("alice.token").is_file(), "the pool must not be deleted");
}

/// `tokens bootstrap` refuses to materialize a pool when the only place it
/// could write is inside a git worktree — `LOOM_SHARED_TOKENS_DIR=""` (the
/// operator opt-out) is a hard failure now, not a fallback to `<repo>/.loom`.
#[test]
fn tokens_bootstrap_refuses_when_the_shared_pool_is_disabled() {
    let fixture = write_fixture_repo(r#"{"nextAgentNumber": 1}"#);
    let registry = fixture.path().join("workspaces.json");
    std::fs::write(
        fixture.path().join(".loom").join("accounts.env"),
        "ACCOUNT_EMAIL_1=alice@example.com\nACCOUNT_KEY_1=sk-ant-oat01-a\n",
    )
    .unwrap();

    let output = run_daemon(
        &[
            "tokens",
            "bootstrap",
            "--workspace",
            fixture.path().to_str().unwrap(),
        ],
        fixture.path(),
        &registry,
    );

    assert!(!output.status.success(), "bootstrap must refuse with no supported destination");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("#9135"), "the refusal must cite the policy, stderr: {stderr}");
    assert!(
        !fixture.path().join(".loom").join("tokens").exists(),
        "bootstrap must never fall back to a pool inside the worktree"
    );
}

/// …and with a supported destination it writes there, never into the repo.
#[test]
fn tokens_bootstrap_writes_only_to_the_shared_pool() {
    let fixture = write_fixture_repo(r#"{"nextAgentNumber": 1}"#);
    let registry = fixture.path().join("workspaces.json");
    std::fs::write(
        fixture.path().join(".loom").join("accounts.env"),
        "ACCOUNT_EMAIL_1=alice@example.com\nACCOUNT_KEY_1=sk-ant-oat01-a\n",
    )
    .unwrap();
    let shared = tempfile::tempdir().unwrap();
    let shared_pool = shared.path().join("tokens");

    let output = run_daemon_with_shared_pool(
        &[
            "tokens",
            "bootstrap",
            "--workspace",
            fixture.path().to_str().unwrap(),
            "--no-home",
        ],
        fixture.path(),
        &registry,
        shared_pool.to_str().unwrap(),
    );

    assert!(
        output.status.success(),
        "bootstrap must succeed against the shared pool, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        shared_pool.join("alice-example.token").is_file(),
        "the token must land in the shared pool"
    );
    assert!(
        !fixture.path().join(".loom").join("tokens").exists(),
        "nothing may be written inside the worktree"
    );
}

// ===== Negative fixture: no `daemon` key — default-off regression guard =====

#[test]
fn workspace_add_behaves_unchanged_with_no_delegation_configured() {
    let fixture = write_fixture_repo(r#"{"nextAgentNumber": 1}"#);
    let registry = fixture.path().join("workspaces.json");
    let target = fixture.path().join("some-other-repo");
    std::fs::create_dir_all(&target).unwrap();

    let output =
        run_daemon(&["workspace", "add", target.to_str().unwrap()], fixture.path(), &registry);

    assert!(
        output.status.success(),
        "workspace add without daemon.delegatedTo must succeed exactly as before, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(registry.exists(), "the registry must have been written");
}

#[test]
fn tokens_bootstrap_reaches_normal_error_path_with_no_delegation_configured() {
    let fixture = write_fixture_repo(r#"{"nextAgentNumber": 1}"#);
    let registry = fixture.path().join("workspaces.json");
    // Name a supported destination (issue #9135) so this reaches bootstrap's
    // own no-account-source failure rather than the "nowhere to write" refusal.
    let shared = tempfile::tempdir().unwrap();

    let output = run_daemon_with_shared_pool(
        &[
            "tokens",
            "bootstrap",
            "--workspace",
            fixture.path().to_str().unwrap(),
        ],
        fixture.path(),
        &registry,
        shared.path().join("tokens").to_str().unwrap(),
    );

    // No delegation configured: bootstrap must reach its normal
    // no-account-source failure, not our delegation refusal message.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("daemon admin is delegated to"),
        "undelegated workspace must not hit the delegation refusal, stderr: {stderr}"
    );
}
