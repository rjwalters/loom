//! End-to-end CLI coverage for `loom-daemon tokens check --all-pools` (issue
//! #7527).
//!
//! Uses `--source monitor` with `LOOM_CLAUDE_MONITOR_DIR` pointed at an empty
//! directory so `run_check` takes the "no fresh claude-monitor ranking.json"
//! early-return path (an empty report, no probe) — deterministic and
//! network-free, since this suite is only exercising the `--all-pools`
//! pool-enumeration/fan-out plumbing, not the probe itself (already covered
//! by `tokens_pool::check`'s own unit tests).

use std::path::Path;
use std::process::Command;

fn write_registry(path: &Path, roots: &[&Path]) {
    let workspaces: Vec<serde_json::Value> = roots
        .iter()
        .map(|r| serde_json::json!({ "root": r.to_str().unwrap(), "priority": 100 }))
        .collect();
    std::fs::write(
        path,
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "workspaces": workspaces,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn seed_pool(workspace: &Path, names: &[&str]) {
    let dir = workspace.join(".loom").join("tokens");
    std::fs::create_dir_all(&dir).unwrap();
    for n in names {
        std::fs::write(dir.join(format!("{n}.token")), "sk-ant-oat01-fake").unwrap();
    }
}

/// `--all-pools` walks every registered workspace's own repo-local pool plus
/// the shared pool, printing one section per pool — and does NOT emit a
/// separate section for a registered workspace that has no repo-local pool
/// (it is covered by the single shared-pool entry instead, issue #7527's
/// "mixed pool presence" edge case).
#[test]
fn check_all_pools_prints_one_section_per_discovered_pool() {
    let tmp = tempfile::tempdir().unwrap();

    let ws_a = tmp.path().join("repo-a");
    seed_pool(&ws_a, &["a-account"]);

    // Registered but no repo-local pool of its own.
    let ws_b = tmp.path().join("repo-b-no-local-pool");
    std::fs::create_dir_all(&ws_b).unwrap();

    let shared_dir = tmp.path().join("shared-pool");
    std::fs::create_dir_all(&shared_dir).unwrap();
    std::fs::write(shared_dir.join("shared-account.token"), "sk-ant-oat01-fake").unwrap();

    let registry_path = tmp.path().join("workspaces.json");
    write_registry(&registry_path, &[&ws_a, &ws_b]);

    let empty_monitor_dir = tmp.path().join("no-monitor-here");

    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["tokens", "check", "--all-pools", "--source", "monitor"])
        .env("LOOM_WORKSPACES_PATH", &registry_path)
        .env("LOOM_SHARED_TOKENS_DIR", &shared_dir)
        .env("LOOM_CLAUDE_MONITOR_DIR", &empty_monitor_dir)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "expected success, got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );

    let repo_a_pool = ws_a.join(".loom").join("tokens");
    assert!(
        stdout.contains(&repo_a_pool.display().to_string()),
        "expected a section for repo-a's own pool: {stdout}"
    );
    assert!(
        stdout.contains(&shared_dir.display().to_string()),
        "expected a section for the shared pool: {stdout}"
    );
    assert!(
        !stdout.contains(&ws_b.display().to_string()),
        "a pool-less registered workspace must not get its own --all-pools section: {stdout}"
    );
}

/// `--json --all-pools` emits one JSON array entry per discovered pool
/// (repo-a's own pool + the one shared entry — never one per registered
/// workspace).
#[test]
fn check_all_pools_json_lists_each_discovered_pool_once() {
    let tmp = tempfile::tempdir().unwrap();

    let ws_a = tmp.path().join("repo-a");
    seed_pool(&ws_a, &["a-account"]);
    let ws_b = tmp.path().join("repo-b-no-local-pool");
    std::fs::create_dir_all(&ws_b).unwrap();

    let shared_dir = tmp.path().join("shared-pool");
    std::fs::create_dir_all(&shared_dir).unwrap();
    std::fs::write(shared_dir.join("shared-account.token"), "sk-ant-oat01-fake").unwrap();

    let registry_path = tmp.path().join("workspaces.json");
    write_registry(&registry_path, &[&ws_a, &ws_b]);

    let empty_monitor_dir = tmp.path().join("no-monitor-here");

    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "tokens",
            "check",
            "--all-pools",
            "--source",
            "monitor",
            "--json",
        ])
        .env("LOOM_WORKSPACES_PATH", &registry_path)
        .env("LOOM_SHARED_TOKENS_DIR", &shared_dir)
        .env("LOOM_CLAUDE_MONITOR_DIR", &empty_monitor_dir)
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let pools = parsed["pools"].as_array().expect("pools array");
    assert_eq!(pools.len(), 2, "expected exactly repo-a + shared, got: {pools:?}");
}

/// With nothing registered and the shared pool disabled, `--all-pools`
/// reports "no pools found" instead of silently no-opping or crashing.
#[test]
fn check_all_pools_with_nothing_registered_reports_no_pools() {
    let tmp = tempfile::tempdir().unwrap();
    let registry_path = tmp.path().join("workspaces.json");
    write_registry(&registry_path, &[]);

    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["tokens", "check", "--all-pools", "--source", "monitor"])
        .env("LOOM_WORKSPACES_PATH", &registry_path)
        .env("LOOM_SHARED_TOKENS_DIR", "")
        .output()
        .unwrap();

    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("No token pools found"), "stderr: {stderr}");
}
