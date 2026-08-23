//! End-to-end CLI coverage for `loom-daemon tokens unblock` (issue #6759).
//!
//! Exercised as a subprocess (not an in-process unit test) because the
//! handler uses `std::process::exit` on several paths — see
//! `tests/accounts_cli.rs` for the established pattern in this crate.
//!
//! Covers the three problems fixed by #6759:
//! 1. An unknown name no longer aborts the whole batch — recognized names in
//!    the same invocation are still processed.
//! 2. A `.bad_tokens` entry for an account no longer in the live pool can be
//!    cleared on demand (no pool-membership requirement).
//! 3. `--shared` targets the machine-level shared pool, mirroring
//!    `bootstrap --shared` / `import-from-monitor --shared`.

use std::path::Path;
use std::process::Command;

/// Seed a `.loom/tokens` pool directory under `workspace` with a `.token`
/// file per name in `names` (content is irrelevant to `unblock`).
fn seed_pool(workspace: &Path, names: &[&str]) {
    let dir = workspace.join(".loom").join("tokens");
    std::fs::create_dir_all(&dir).unwrap();
    for n in names {
        std::fs::write(dir.join(format!("{n}.token")), "sk-ant-oat01-fake").unwrap();
    }
}

fn bad_tokens_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(".loom").join("tokens").join(".bad_tokens")
}

fn mark_bad(workspace: &Path, name: &str, reason: &str) {
    let status = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "tokens",
            "mark-bad",
            "--workspace",
            workspace.to_str().unwrap(),
            name,
            "--reason",
            reason,
        ])
        .status()
        .unwrap();
    assert!(status.success(), "mark-bad {name} failed");
}

fn run_unblock(workspace: &Path, extra_args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["tokens", "unblock", "--workspace", workspace.to_str().unwrap()])
        .args(extra_args)
        // Deterministic: never let a real host shared pool leak into a
        // non-`--shared` test run.
        .env("LOOM_SHARED_TOKENS_DIR", "")
        .output()
        .unwrap()
}

/// #6759, problem 1: previously any single unrecognized name in the batch
/// called `std::process::exit(2)` before `bad_tokens::unblock()` ever ran,
/// discarding the recognized names too. Now the unknown name is reported and
/// skipped while the recognized ones are still processed.
#[test]
fn unblock_processes_recognized_names_and_reports_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    seed_pool(ws, &["good1", "good2"]);
    mark_bad(ws, "good1", "401 unauthorized");
    mark_bad(ws, "good2", "oauth token expired");

    let out = run_unblock(ws, &["good1", "good2", "totally-unrecognized-name"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    assert!(
        stderr.contains("totally-unrecognized-name"),
        "stderr should name the unknown account: {stderr}"
    );
    assert!(
        stdout.contains("good1") && stdout.contains("good2"),
        "stdout should confirm the recognized accounts were unblocked: {stdout}"
    );

    let bad_tokens = std::fs::read_to_string(bad_tokens_path(ws)).unwrap_or_default();
    assert!(!bad_tokens.contains("good1"));
    assert!(!bad_tokens.contains("good2"));
}

/// #6759, problem 2: an account with a stale `.bad_tokens` entry but no
/// `.token` file in the live pool (a removed/retired account) must still be
/// clearable on demand — the CLI must not gate `unblock` on pool membership.
#[test]
fn unblock_clears_entry_for_account_absent_from_pool() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    // Seed one unrelated account so the per-repo pool is picked deterministically
    // (resolve_tokens_dir only prefers a per-repo pool that holds token files).
    seed_pool(ws, &["seed"]);
    mark_bad(ws, "retired-account", "401 unauthorized");
    assert!(
        !ws.join(".loom")
            .join("tokens")
            .join("retired-account.token")
            .exists(),
        "retired-account must not have a live token file"
    );

    let out = run_unblock(ws, &["retired-account"]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    assert!(!stderr.contains("Unknown"), "stderr: {stderr}");
    assert!(stdout.contains("retired-account"), "stdout: {stdout}");

    let bad_tokens = std::fs::read_to_string(bad_tokens_path(ws)).unwrap_or_default();
    assert!(!bad_tokens.contains("retired-account"));
}

/// A name matching neither the live pool nor any `.bad_tokens` entry is
/// genuinely unknown; when it is the *only* name given, `unblock` reports it
/// and exits non-zero instead of silently succeeding.
#[test]
fn unblock_all_unknown_names_exits_nonzero_without_touching_the_pool() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    seed_pool(ws, &["seed"]);

    let out = run_unblock(ws, &["nonexistent-account"]);

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nonexistent-account"), "stderr: {stderr}");
}

/// #6759, problem 3: `--shared` redirects to the machine-level shared pool,
/// mirroring `bootstrap --shared` / `import-from-monitor --shared`.
#[test]
fn unblock_shared_targets_the_shared_pool() {
    let tmp = tempfile::tempdir().unwrap();
    let shared_dir = tmp.path().join("shared-pool");
    std::fs::create_dir_all(&shared_dir).unwrap();
    std::fs::write(shared_dir.join("shared-account.token"), "sk-ant-oat01-fake").unwrap();
    std::fs::write(
        shared_dir.join(".bad_tokens"),
        "2026-01-01T00:00:00Z shared-account 401 unauthorized\n",
    )
    .unwrap();

    // A distinct, unrelated repo-local workspace — must be left untouched by
    // `--shared`, proving the flag (not the default `--workspace .`) chose
    // the target.
    let local_ws = tmp.path().join("local-ws");
    seed_pool(&local_ws, &["seed"]);

    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "tokens",
            "unblock",
            "--workspace",
            local_ws.to_str().unwrap(),
            "--shared",
            "shared-account",
        ])
        .env("LOOM_SHARED_TOKENS_DIR", &shared_dir)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    assert!(
        stderr.contains("shared machine-level pool"),
        "stderr should confirm the shared pool was targeted: {stderr}"
    );

    let shared_bad_tokens = std::fs::read_to_string(shared_dir.join(".bad_tokens")).unwrap();
    assert!(!shared_bad_tokens.contains("shared-account"));

    // The unrelated local workspace never got a `.bad_tokens` file at all.
    assert!(!bad_tokens_path(&local_ws).exists());
}

/// `--shared` with the shared pool explicitly disabled fails clearly instead
/// of silently falling back to the repo-local pool.
#[test]
fn unblock_shared_disabled_errors_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    seed_pool(ws, &["seed"]);

    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "tokens",
            "unblock",
            "--workspace",
            ws.to_str().unwrap(),
            "--shared",
            "some-account",
        ])
        .env("LOOM_SHARED_TOKENS_DIR", "")
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("shared pool is disabled"), "stderr: {stderr}");
}
