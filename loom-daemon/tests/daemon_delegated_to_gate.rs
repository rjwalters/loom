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

const DELEGATED_CONFIG: &str = r#"{"daemon": {"delegatedTo": "/Users/rwalters/GitHub/2am"}}"#;

fn run_daemon(args: &[&str], cwd: &Path, workspaces_path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(args)
        .current_dir(cwd)
        .env("LOOM_WORKSPACES_PATH", workspaces_path)
        // Deterministic regardless of host machine-level defaults tier
        // (issue #4039's private/shared defaults file) or a real
        // ~/.loom/tokens shared pool leaking into `tokens select`.
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_TOKENS_DIR", "")
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
        stderr.contains("/Users/rwalters/GitHub/2am"),
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
        stderr.contains("/Users/rwalters/GitHub/2am"),
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
        stderr.contains("/Users/rwalters/GitHub/2am"),
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
        stderr.contains("/Users/rwalters/GitHub/2am"),
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
    // here), so seed the pool directly.
    let tokens_dir = fixture.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens_dir).unwrap();
    std::fs::write(tokens_dir.join("alice.token"), "fake-oauth-token-value\n").unwrap();

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

    // No delegation configured: bootstrap must reach its normal
    // no-account-source failure, not our delegation refusal message.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("daemon admin is delegated to"),
        "undelegated workspace must not hit the delegation refusal, stderr: {stderr}"
    );
}
