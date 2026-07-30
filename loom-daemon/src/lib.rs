//! Loom daemon internals, exposed as a library so unit tests can run without
//! the binary's tokio runtime.
//!
//! # Test isolation convention (READ BEFORE ADDING TESTS)
//!
//! **Env mutation and child-spawning tests share one isolation domain.** This
//! crate reads configuration from process-global environment variables in
//! hundreds of places, and its tests set/remove those vars to exercise the
//! `env > config > default` precedence. Many other tests call
//! `std::process::Command::spawn` (fake `gh`/`git` shims, real `loom-daemon`
//! children, `tmux`). `spawn` snapshots the process environ non-atomically, so a
//! concurrent `env::set_var` / `remove_var` can hand a child a torn environment
//! — the exact hazard that made `env::set_var` `unsafe` in Rust edition 2024. It
//! is not theoretical: it produced intermittent CI failures in otherwise
//! hermetic, untouched tests — `pipeline_snapshot` on PR #4305 and
//! `quarantine_reconciliation` on PR #4320, both on 2026-07-29 (issue #4385).
//!
//! `#[serial]` (from `serial_test`) does **not** solve this on its own. Its lock
//! is advisory and in-process: it serializes marked tests against each other,
//! while every *unmarked* test in the same binary — including every future one
//! nobody remembers to mark — still runs concurrently with them.
//!
//! **Thread-local Tokio runtime context is the same class of hazard.** A
//! #4494 Doctor-verification run of the full `cargo test --workspace` suite
//! hit a Tokio-runtime-context test failure that did not reproduce in
//! isolation — consistent with thread-local runtime state (not just env vars)
//! colliding between two unrelated tests under `cargo test`'s shared-process,
//! multi-threaded harness (issue #4561). `cargo nextest run` closes it for the
//! same structural reason it closes the env-mutation race: no thread-local (or
//! process-global) state survives a process boundary. Verified via 3
//! consecutive `cargo nextest run --workspace --profile ci -p loom-daemon
//! --lib` runs (2,486 tests each) with zero failures — see #4561 and the
//! matching note in `.config/nextest.toml`.
//!
//! ## How the suite is meant to run
//!
//! The workspace suite runs under [`cargo nextest`](https://nexte.st) — **one
//! process per test**. One test's env mutation is then invisible to every other
//! test by construction, and so are process-global statics (`OnceLock` / `Mutex`
//! caches, atomics). Configuration lives in `.config/nextest.toml`. Doctests need
//! a separate `cargo test --workspace --doc` invocation, since nextest does not
//! run them.
//!
//! <div class="warning">
//!
//! The `backend-tests` CI job has **not** been switched over yet: the workflow
//! edit needs a credential with GitHub's `workflow` scope, which the automation
//! account lacks. Tracked in #4559, with the rationale at the top of
//! `.config/nextest.toml` (#4385). Treat the rules below as the standing
//! convention regardless — they are what makes the switch a no-op when it lands.
//!
//! </div>
//!
//! ## What you should do
//!
//! * **Prefer `cargo nextest run` for full-suite local runs.** Plain `cargo
//!   test` runs every test in a binary on shared threads and therefore still
//!   races; a failure seen only under `cargo test` may be this, not your change.
//!   CI is the arbiter of green.
//! * **Keep `#[serial]` on env-mutating tests.** It is a no-op under nextest but
//!   still load-bearing for developers running plain `cargo test`, and it
//!   documents the dependency. Adding it to a new env-mutating test is correct.
//! * **Restore what you mutate.** Set the var, assert, then remove it — process
//!   isolation makes leakage invisible under nextest but not under `cargo test`.
//! * **Do not depend on a var being *absent*.** Process isolation gives you the
//!   *real* ambient environment, not a sibling test's leftovers. A test that
//!   needs `$LOOM_*` unset must clear it itself (see
//!   `workspace_pool::tests::clear_safehouse_env`) — any agent session spawned by
//!   a running daemon exports a pile of `LOOM_*` vars.
//! * **Prefer an explicit seam over an env var** where one exists (e.g. pass a
//!   `gh` binary path in with `with_gh_bin`, take a root `&Path` argument). A
//!   test that never touches the environ cannot participate in this class of bug
//!   at all. Likewise use `tempfile::TempDir` for paths and explicit `.env()` on
//!   `Command` for child environments.
//!
//! ## When a test truly needs cross-process exclusive state
//!
//! Process isolation supersedes `#[serial]` only for *process*-scoped state (env
//! vars, cwd, process-local statics). It does nothing for a resource shared
//! *between* processes: a fixed path outside a `tempfile::TempDir`, the real
//! `$HOME`, global git config, a fixed TCP port, or the shared `tmux -L loom`
//! server. `#[serial]` is not enough for those either, because nextest runs tests
//! from *all* binaries concurrently (plain `cargo test` ran binaries one at a
//! time). Declare such tests in `.config/nextest.toml` — a `max-threads = 1`
//! [test group](https://nexte.st/docs/configuration/test-groups/) to serialize a
//! family against itself, or `threads-required = 'num-test-threads'` for a test
//! that must run completely alone — or use `serial_test::file_serial`, which takes
//! an inter-process file lock.
//!
//! An audit of every `#[serial]` test in this crate (#4385) found exactly one such
//! resource: the host-global `tmux -L loom` server, reached by the
//! `integration_*` binaries in `loom-daemon/tests/`. Each spawns real
//! `loom-daemon` children against it, and two of the four
//! (`integration_security.rs`, `integration_factory_reset.rs`) call
//! `cleanup_all_loom_sessions()`, which kills **every** `loom-*` session on
//! that server, not just its own — a deliberate, commented exception for
//! suites whose hardcoded/unprefixed terminal IDs a scoped cleanup cannot see
//! (issue #4622; see the doc comment on `cleanup_all_loom_sessions()` in
//! `tests/common/mod.rs`). The other two use the `TEST_PREFIX`-scoped
//! `cleanup_test_sessions()`. All four are nonetheless placed in the
//! `daemon-integration` test group (`max-threads = 1`), so at most one is ever
//! in flight — they still share the same host-global tmux server and spawn
//! real daemons against it, and two of them retain the host-wide kill.
//! Everything else those tests touch is per-test (`tempfile::TempDir` roots,
//! ephemeral ports, per-binary session prefixes). Confirm group membership
//! rather than assuming it:
//!
//! ```text
//! cargo nextest show-config test-groups --profile ci
//! ```
//!
//! Two footguns worth knowing:
//!
//! * **Named profiles do not inherit `[[profile.default.overrides]]`.** An
//!   override declared only on `default` silently does not apply to
//!   `--profile ci`. Declare it on both.
//! * **The group bounds one nextest run, not the host.** A `cargo test` in another
//!   checkout on the same machine can still run `cleanup_all_loom_sessions()`
//!   (from `integration_security.rs`/`integration_factory_reset.rs`) and destroy
//!   this run's tmux sessions. If the `integration_*` suites fail with
//!   "session ... does not exist" or daemon-startup timeouts, check for a sibling
//!   test run before suspecting your change — the same failures reproduce under
//!   plain `cargo test`.
//!
//! If you add a test that touches the tmux server, or any other machine-global
//! resource, put it in the group — do not rely on `#[serial]`.

// These modules were originally private to the binary crate. Exposing them as
// a library (to allow unit tests to run without the binary's tokio runtime)
// triggers public-API clippy lints that don't apply to internal-use code.
#![allow(clippy::must_use_candidate)]
#![allow(clippy::should_implement_trait)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::single_match_else)]
#![allow(clippy::new_without_default)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

pub mod activity;
pub mod agent_session;
pub mod auto_update;
pub mod autonomy_marker;
pub mod build_slot;
pub mod calibrate;
pub mod capacity;
pub mod claim_reconciliation;
pub mod config_resolver;
pub mod cpu_headroom;
pub mod credential_preflight;
pub mod daemon_heartbeat;
pub mod daemon_install_state;
pub mod disk_headroom;
pub mod epic_state;
pub mod epic_supervisor;
pub mod errors;
pub mod event_bus;
pub mod fleet;
pub mod forge_cmd;
pub mod forge_listing;
pub mod forge_parser;
pub mod git_parser;
pub mod git_utils;
pub mod health_monitor;
pub mod host_breaker;
pub mod idle_exit;
pub mod init;
pub mod ipc;
pub mod issue_creation_mutex;
pub mod live_claim;
pub mod main_health_gate;
pub mod metrics_collector;
pub mod peer_claims;
pub mod phase_join;
pub mod pipeline_snapshot;
pub mod quarantine_reconciliation;
pub mod rate_limit_breaker;
pub mod role_collision;
pub mod role_runner;
pub mod role_validation;
pub mod runtime_admission;
pub mod safehouse;
pub mod script_helpers;
pub mod self_update;
pub mod serve;
pub mod sweep_journal;
pub mod sweep_outcomes;
pub mod sweep_registry;
pub mod terminal;
/// Test-only capturing logger (see module docs) — single-sourced so the crate's
/// one-per-process `log::set_boxed_logger` installation is shared by every test
/// module that asserts on log severity.
#[cfg(test)]
pub mod test_log_capture;
pub mod token_ranking_refresh;
pub mod tokens;
pub mod tokens_pool;
pub mod types;
pub mod watch_registry;
pub mod work_finder;
pub mod workspace_pool;
pub mod workspace_registry;
pub mod worktree_ops;
pub mod worktree_root;

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Rotate log file if it exceeds max size.
/// Keeps last `max_files` rotated files (log.1, log.2, ..., log.N).
pub fn rotate_log_file(log_path: &Path, max_size: u64, max_files: usize) -> anyhow::Result<()> {
    if !log_path.exists() {
        return Ok(());
    }

    let metadata = fs::metadata(log_path)?;
    if metadata.len() < max_size {
        return Ok(());
    }

    // Remove oldest rotated file if it exists
    let oldest_file = format!("{}.{max_files}", log_path.display());
    let _ = fs::remove_file(&oldest_file);

    // Shift existing rotated files (log.N-1 -> log.N, etc.)
    for i in (1..max_files).rev() {
        let old_path = format!("{}.{i}", log_path.display());
        let new_path = format!("{}.{}", log_path.display(), i + 1);
        if Path::new(&old_path).exists() {
            let _ = fs::rename(&old_path, &new_path);
        }
    }

    // Rotate current log file to log.1
    let rotated_path = format!("{}.1", log_path.display());
    fs::rename(log_path, rotated_path)?;

    Ok(())
}

/// Extract configured terminal IDs from workspace config.
///
/// Resolves the workspace's effective config through the tier chain
/// (`config_resolver::resolve_effective_config`: private/shared defaults →
/// legacy `.loom/config.json` → `.loom-project/project.json` →
/// `.loom-local/local.json`) and extracts the `id` field from each terminal
/// entry. Returns `None` when no tier supplies a non-empty `terminals` array —
/// the empty-set-⇒-`None` contract is preserved (#4059).
///
/// **Diagnostic note (#4059, Finding 3):** the previous direct-read
/// implementation emitted a function-local `log::warn!` for a malformed
/// `.loom/config.json`. That warn is not lost: `config_resolver`'s
/// `soft_read_json_object` still emits a `log::warn!` when a tier's JSON fails
/// to parse. A malformed config therefore collapses to `{}` at the resolver
/// layer (so it now reaches the "no terminals" branch here rather than a
/// dedicated parse-error branch), but the operator still sees a warning — it
/// just names the resolver tier rather than this call site. This diagnosability
/// change is accepted deliberately; the observable `None` contract is unchanged
/// and the existing invalid-JSON test guards the collapsed path.
pub fn extract_configured_terminal_ids(workspace: &Path) -> Option<HashSet<String>> {
    let config = config_resolver::resolve_effective_config(workspace);

    let terminals = config.get("terminals")?.as_array()?;

    let ids: HashSet<String> = terminals
        .iter()
        .filter_map(|t| t.get("id")?.as_str().map(String::from))
        .collect();

    if ids.is_empty() {
        log::debug!("No terminal IDs found in resolved config for {}", workspace.display());
        return None;
    }

    log::info!(
        "Loaded {} configured terminal IDs from resolved config for {}",
        ids.len(),
        workspace.display()
    );

    Some(ids)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ===== rotate_log_file tests =====

    #[test]
    fn test_rotate_log_file_no_file_exists() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");
        rotate_log_file(&log_path, 1024, 10).unwrap();
    }

    #[test]
    fn test_rotate_log_file_under_limit() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");
        fs::write(&log_path, "small content").unwrap();

        rotate_log_file(&log_path, 1024 * 1024, 10).unwrap();

        assert!(log_path.exists());
        assert_eq!(fs::read_to_string(&log_path).unwrap(), "small content");
    }

    #[test]
    fn test_rotate_log_file_at_limit() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        let content = "x".repeat(100);
        fs::write(&log_path, &content).unwrap();

        rotate_log_file(&log_path, 50, 5).unwrap();

        assert!(!log_path.exists());
        let rotated = dir.path().join("daemon.log.1");
        assert!(rotated.exists());
        assert_eq!(fs::read_to_string(rotated).unwrap(), content);
    }

    #[test]
    fn test_rotate_log_file_shifts_existing() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        fs::write(dir.path().join("daemon.log.1"), "old content").unwrap();
        fs::write(&log_path, "x".repeat(100)).unwrap();

        rotate_log_file(&log_path, 50, 5).unwrap();

        assert!(dir.path().join("daemon.log.2").exists());
        assert_eq!(fs::read_to_string(dir.path().join("daemon.log.2")).unwrap(), "old content");
        assert!(dir.path().join("daemon.log.1").exists());
    }

    #[test]
    fn test_rotate_log_file_removes_oldest() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("daemon.log");

        fs::write(dir.path().join("daemon.log.3"), "oldest").unwrap();
        fs::write(&log_path, "x".repeat(100)).unwrap();

        rotate_log_file(&log_path, 50, 3).unwrap();

        assert!(dir.path().join("daemon.log.1").exists());
    }

    // ===== extract_configured_terminal_ids tests =====

    #[test]
    fn test_extract_terminal_ids_missing_config() {
        let dir = tempdir().unwrap();
        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_terminal_ids_invalid_json() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();
        fs::write(loom_dir.join("config.json"), "not valid json").unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_terminal_ids_no_terminals_key() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();
        fs::write(loom_dir.join("config.json"), r#"{"other": "data"}"#).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_terminal_ids_empty_terminals() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();
        fs::write(loom_dir.join("config.json"), r#"{"terminals": []}"#).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_none());
    }

    /// #4059: terminals supplied ONLY by the `.loom-project/project.json`
    /// tier (no legacy `.loom/config.json` at all) are discovered through the
    /// resolver. The private/shared defaults tier is disabled for hermeticity.
    #[test]
    #[serial_test::serial]
    fn test_extract_terminal_ids_from_project_tier_only() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join(".loom-project");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("project.json"),
            r#"{"terminals": [{"id": "terminal-1"}, {"id": "terminal-2"}]}"#,
        )
        .unwrap();
        // No .loom/config.json exists.
        assert!(!dir.path().join(".loom").join("config.json").exists());

        let result = extract_configured_terminal_ids(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);

        let ids = result.expect("terminals from project tier should be discovered");
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("terminal-1"));
        assert!(ids.contains("terminal-2"));
    }

    /// #4059: an empty `terminals` array in the project tier still yields
    /// `None`, preserving the empty-set-⇒-`None` contract across tiers.
    #[test]
    #[serial_test::serial]
    fn test_extract_terminal_ids_empty_terminals_project_tier() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join(".loom-project");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(project_dir.join("project.json"), r#"{"terminals": []}"#).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_terminal_ids_valid_config() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();

        let config = r#"{
            "nextAgentNumber": 3,
            "terminals": [
                {"id": "terminal-1", "name": "Builder", "role": "builder"},
                {"id": "terminal-2", "name": "Judge", "role": "judge"},
                {"id": "shepherd-1", "name": "Shepherd", "role": "shepherd"}
            ]
        }"#;
        fs::write(loom_dir.join("config.json"), config).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_some());
        let ids = result.unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains("terminal-1"));
        assert!(ids.contains("terminal-2"));
        assert!(ids.contains("shepherd-1"));
    }

    #[test]
    fn test_extract_terminal_ids_skips_entries_without_id() {
        let dir = tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        fs::create_dir_all(&loom_dir).unwrap();

        let config = r#"{
            "terminals": [
                {"id": "terminal-1", "name": "Builder"},
                {"name": "No ID"},
                {"id": "terminal-3", "name": "Third"}
            ]
        }"#;
        fs::write(loom_dir.join("config.json"), config).unwrap();

        let result = extract_configured_terminal_ids(dir.path());
        assert!(result.is_some());
        let ids = result.unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("terminal-1"));
        assert!(ids.contains("terminal-3"));
    }
}
