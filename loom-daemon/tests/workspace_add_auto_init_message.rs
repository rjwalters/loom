//! Issue #6636 — `workspace add`'s auto-init of a missing `/loom:sweep`
//! install (issue #5682) writes the Loom surfaces straight into the target's
//! working tree as **uncommitted** files (`initialize_workspace` never
//! commits). Registering the same tracked repo independently on multiple
//! hosts can therefore drift, and the generated files are one `git add -A`
//! away from being swept into an unrelated commit. The auto-init success
//! message must name both the consequence and the follow-up
//! (`install.sh --quick <path>` + commit) so an operator does not have to
//! discover it the way issue #6636 did.
//!
//! Spawns the real compiled `loom-daemon` binary (`CARGO_BIN_EXE_loom-daemon`)
//! — mirrors the `daemon_delegated_to_gate.rs` pattern — so this exercises
//! the actual CLI output an operator sees, not just the in-process handler.

use std::path::Path;
use std::process::{Command, Output};

fn run_daemon(args: &[&str], cwd: &Path, workspaces_path: &Path) -> Output {
    // `LOOM_DAEMON_DEFAULTS_DIR` points auto-init's `resolve_defaults_path`
    // at this crate's own repo-root `defaults/` payload. Unlike the
    // in-process unit tests in `workspace_fleet.rs` (whose cwd is the cargo
    // test process's, still inside the loom repo checkout, so the
    // git-root-relative fallback resolves it for free), a spawned binary's
    // cwd here is the fixture tempdir — a fake `.git` marker of its own with
    // no `defaults/` sibling — so the machine-level override is needed.
    let defaults_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("defaults");
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(args)
        .current_dir(cwd)
        .env("LOOM_WORKSPACES_PATH", workspaces_path)
        // Deterministic regardless of host machine-level defaults tier
        // (issue #4039's private/shared defaults file) or a real
        // ~/.loom/tokens shared pool leaking in.
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_TOKENS_DIR", "")
        .env("LOOM_DAEMON_DEFAULTS_DIR", defaults_dir)
        .output()
        .unwrap()
}

#[test]
fn workspace_add_auto_init_names_uncommitted_consequence_and_follow_up() {
    let fixture = tempfile::tempdir().unwrap();
    let registry = fixture.path().join("workspaces.json");
    let target = fixture.path().join("target-repo");
    std::fs::create_dir_all(&target).unwrap();
    let status = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(&target)
        .status()
        .expect("git init should run");
    assert!(status.success(), "git init failed");

    let output =
        run_daemon(&["workspace", "add", target.to_str().unwrap()], fixture.path(), &registry);

    assert!(
        output.status.success(),
        "workspace add against a fresh git repo missing sweep.md should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("UNCOMMITTED"),
        "auto-init success output must name the uncommitted-files consequence, got: {stdout}"
    );
    assert!(
        stdout.contains("install.sh --quick"),
        "auto-init success output must name the install.sh --quick follow-up, got: {stdout}"
    );
    assert!(
        target
            .join(".claude")
            .join("commands")
            .join("loom")
            .join("sweep.md")
            .exists(),
        "sweep.md should have been installed by auto-init"
    );
}

#[test]
fn workspace_add_help_documents_auto_init_uncommitted_consequence() {
    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["workspace", "add", "--help"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "workspace add --help should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("UNCOMMITTED"),
        "workspace add --help must document the auto-init untracked-files consequence, got: {stdout}"
    );
    assert!(
        stdout.contains("install.sh --quick"),
        "workspace add --help must name the follow-up command, got: {stdout}"
    );
}
