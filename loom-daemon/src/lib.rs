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
//! * **Never let a test reach the machine-wide build slot.** A test that runs
//!   the build gate, or anything else that calls `build_slot::slot_dir`, holds a
//!   `build_slot::test_support::BuildSlotEnvGuard` for its whole body. Without
//!   one the slot resolves to the host's `~/.loom/locks/build-slot`, and
//!   `slot_dir` panics in test builds rather than touch it (#11014).
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
pub mod admission_brake;
pub mod agent_gh;
pub mod agent_session;
pub mod agent_skills;
pub mod api_keys_pool;
pub mod auto_update;
pub mod autonomy_marker;
pub mod build_slot;
pub mod calibrate;
pub mod capability;
pub mod capacity;
pub mod ci_telemetry;
pub mod claim_reconciliation;
pub mod cmd_out;
pub mod codex_sandbox_noop;
pub mod codex_usage;
pub mod comment_trust;
pub mod config_resolver;
pub mod cpu_headroom;
pub mod credential_preflight;
pub mod daemon_bin_resolve;
pub mod daemon_heartbeat;
pub mod daemon_install_state;
pub mod daemon_pidfile;
pub mod daemon_start;
/// Non-blocking wrapper around the startup claim-reconciliation +
/// stranded-quarantine reconciliation passes (Issue #7974) — a new sibling
/// module rather than growing `daemon_service.rs`, which the file-size
/// ratchet freezes at its current line count
/// (`.loom/docs/file-size-policy.md`).
pub mod daemon_startup_reconciliation;
pub mod daemon_update;
pub mod deep_clean;
pub mod dep_classify;
pub mod dep_recheck;
pub mod disk_admission;
pub mod disk_footprint;
pub mod disk_full_halt;
pub mod disk_headroom;
pub mod docker_image_clean;
/// Doctor hand-back as a conditional, verified label transition (#9388).
pub mod doctor_handback;
pub mod eager_reclaim;
pub mod epic_state;
pub mod epic_supervisor;
pub mod errors;
pub mod event_bus;
pub mod fetch_headroom;
pub mod filing_lock;
pub mod fleet;
pub mod fleet_alert;
pub mod fleet_captain;
pub mod fleet_outputs;
pub mod fleet_singletons;
pub mod fleet_state;
pub mod fleet_store;
pub mod fleet_sync;
pub mod foreign_load;
pub mod forge_bucket_book;
pub mod forge_cached_list;
pub mod forge_cached_view;
pub mod forge_call_stats;
pub mod forge_check_branch;
pub mod forge_check_claim;
pub mod forge_check_open_pr;
pub mod forge_cmd;
pub mod forge_comment;
pub mod forge_contract;
pub mod forge_denial;
pub mod forge_disable_auto_merge;
pub mod forge_egress;
pub(crate) mod forge_etag_store;
pub mod forge_events;
pub mod forge_identity;
pub mod forge_inventory;
pub mod forge_listing;
pub mod forge_merge_config;
pub mod forge_merge_method;
pub mod forge_merge_queue;
pub mod forge_parser;
pub mod forge_pr_congestion;
pub mod forge_priority_labels;
pub mod forge_probe;
pub mod forge_pull_listing;
pub mod forge_read_pool;
pub(crate) mod forge_repo_facts;
pub mod forge_rerun;
pub mod forge_tree_unchanged;
pub mod forge_version_only_diff;
pub mod forge_wait_checks;
/// Generated content a quarantine must never stash: name-based artifacts and
/// content-verified cargo build trees (#5690, #11075).
pub mod generated_artifact;
pub mod gh_invocation;
pub mod gh_repo_env;
pub mod gh_state_probe;
pub mod git_parser;
pub mod git_tmp_reclaim;
pub mod git_utils;
pub mod guard_wiring;
pub mod guards_status;
pub mod hard_exclusion;
pub mod health;
pub mod health_monitor;
pub mod host_affinity;
pub mod host_breaker;
pub mod host_optout;
pub mod host_pressure;
pub mod hyperparams;
pub mod idle_exit;
pub mod inflight;
pub mod init;
pub mod install_compat;
pub mod install_compat_harness;
pub mod install_self_check;
pub mod intake_reconcile;
pub mod ipc;
pub mod issue_creation_mutex;
pub mod jev_merge_risk;
pub mod jev_tier;
pub mod kimi_usage;
pub mod label_registry;
pub mod launch_env;
pub mod launch_record;
pub mod launchd_env_drift;
pub mod launchd_reload;
pub mod limit_calibration;
pub mod live_claim;
/// Test-only: a `gh` stand-in prepended to `PATH` before `main`, failing the
/// run on any spawn of the real `gh` (#10138).
#[cfg(test)]
mod live_gh_guard;
pub mod main_health_gate;
pub mod mcp_tool_guard;
pub mod merge_group_ci;
pub mod merge_pr;
pub mod metrics_collector;
pub mod observability;
pub mod opencode_usage;
pub mod operator_decision;
pub mod operator_levels;
pub mod operator_stop;
pub mod orphan_process_reaper;
pub mod overlap_replay;
pub mod park_record;
pub mod peer_claims;
pub mod phase_join;
pub mod pi_usage;
pub mod pipeline_snapshot;
pub mod points_marker;
pub mod pr_latency;
pub mod pr_planning;
pub mod preflight;
pub mod premise_check;
pub mod primary_checkout_reaper;
pub mod priority_pick;
pub mod proc_exec;
pub mod provenance;
pub mod quarantine_reconciliation;
pub mod quarantine_stash_status;
pub mod ram_headroom;
pub mod ram_peaks;
pub mod rate_limit_breaker;
pub mod reclaim_pr_warning;
pub mod reconcile_stack;
/// Ref-operand validation for forge-derived branch names (#9106). A sibling
/// module rather than a `reconcile_stack::` submodule: the predicate gates
/// every place a forge ref reaches a `Command` argv, not just the stacked
/// reconcile, and it is the Rust half of `check_branch_name` in
/// `defaults/scripts/lib/default-branch.sh`.
pub mod refname;
pub mod release_fetch;
pub mod release_provenance;
pub mod release_resolve;
pub mod renovate_labels;
pub mod repo_root;
pub mod restart_verify;
/// Fork-point provenance for `.loom/resync-ignore` pins (#8726).
pub mod resync_pin;
pub mod retry_classify;
/// The `sweep.outcome` rework-event marker protocol (#9444): where the file
/// lives, the `kind` vocabulary, the substantive/environmental table, and the
/// writer. Public because the *writers* are outside the sweep registry — the
/// `record-rework` subcommand a merge/doctor/CI path shells out to — while the
/// reader stays beside the outcome journal that samples it.
pub mod rework_events;
pub mod role_collision;
pub mod role_runner;
pub mod role_shard;
pub mod role_tick_telemetry;
pub mod role_tool_policy;
pub mod role_validation;
/// Safe-point pause hook, pause state and resume handles for a daemon roll (#10830).
pub mod roll_pause;
/// A Loom-owned `CARGO_TARGET_DIR` per role run under `.loom/targets/` (#8370).
pub mod run_target_dir;
pub mod runtime_admission;
pub mod runtime_launch;
pub mod runtime_preference;
#[cfg(test)]
pub(crate) mod runtime_selection_test_support;
pub mod safehouse;
/// Inbound safehouse ChatOps steering (#7893, Phase 3a of #4196). A sibling
/// module rather than a `safehouse::` submodule: `safehouse.rs` is an
/// over-threshold file frozen by the file-size ratchet
/// (`.loom/docs/file-size-policy.md`), and the whole point of the ratchet is
/// that new code lands in a new module instead.
pub mod safehouse_chatops;
pub mod scratch_reclaim;
pub mod script_helpers;
/// Content scan for credential-shaped values before commit/push (#9133).
pub mod secret_scan;
pub mod self_update;
pub mod serve;
pub mod session_reconcile;
pub mod session_status;
pub mod shell_budget;
pub mod short_hash;
pub mod signoz_read;
pub mod stale_blocked;
pub mod star_liveness;
pub mod startup_adoption;
pub mod stash_retirement;
/// Root-count-aware cost model for `build_daemon_status` and the `status`/
/// `health` IPC probes that wait on it (#8163). A sibling module rather than
/// code inside `ipc.rs`/`cli/health.rs`: `ipc.rs` is an over-threshold file
/// frozen by the file-size ratchet, and stating the model once is what keeps
/// the daemon-side budget and the client-side probe budget from drifting.
pub mod status_budget;
/// The named top-level sections of `status --json` and the build phases each
/// needs (#10787): one list for `--section` parsing, `--help`, the wire request
/// and the renderer's key filter.
pub mod status_section;
/// `points:*` story-point size labels (#9432, epic #9429) — the one parser both
/// telemetry consumers (`sweep.started` at dispatch, `sweep.outcome` at the
/// terminal transition) resolve points through, including the daemon-side
/// one-label-per-issue guard.
pub mod story_points;
pub mod sweep_journal;
pub mod sweep_outcome_summary;
pub mod sweep_outcomes;
pub mod sweep_registry;
/// One sweep's token usage *and* the `tokens_status` that explains it (Issue
/// #9440) — the single resolver both `sweep.outcome` construction sites share,
/// so a failed, cancelled or watchdog-killed sweep reports its spend exactly
/// the way a successful one does. A sibling module rather than more code in
/// [`usage_source`]: this adds a *decision* (measured / not-spawned /
/// unattributable) on top of that module's reader dispatch, and the two emit
/// sites must not be able to disagree about it.
pub mod sweep_usage;
pub mod tap_usage;
pub mod target_dir_gc;
/// Orphan sweep for Loom-owned and agent-improvised cargo target dirs (#8370).
pub mod target_orphan_reclaim;
/// Per-task liveness heartbeats for the daemon's long-running loops (Issue
/// #10414): the `loom.daemon.task_alive` gauge and `Task liveness:` in status.
pub mod task_liveness;
pub mod telemetry;
pub mod telemetry_replay;
pub mod terminal;
pub mod terminal_restore;
/// Test-only capturing logger (see module docs) — single-sourced so the crate's
/// one-per-process `log::set_boxed_logger` installation is shared by every test
/// module that asserts on log severity.
#[cfg(test)]
pub mod test_log_capture;
/// Host-wide reclaim of orphaned Loom-named scratch directories parked on a
/// `tmpfs`/`ramfs` mount (issue #8512) — see the module docs for the
/// deliberately different, narrower safety model this needs relative to the
/// worktree-attribution-based cargo-target reclaim (#7239).
pub mod tmpfs_reclaim;
/// tmpfs/`shared`-RAM and kernel OOM-kill visibility (issue #8572, split from
/// #8512) — the read-only counterpart to [`tmpfs_reclaim`]'s write path,
/// consumed by `loom-daemon health` and the work finder's bounded warning.
pub mod tmpfs_visibility;
pub mod token_ranking_refresh;
pub mod tokens;
pub mod tokens_pool;
pub mod transcript_tokens;
pub mod types;
pub mod usage_source;
/// Is a verdict comment body a rationale at all? (`forge verdict-body-check`, #9258)
pub mod verdict_body;
pub mod verdict_equivalence;
/// The verdict-time gate and label transition behind `post-verdict.sh` (#10581).
pub mod verdict_gate;
/// The stale-verdict notice both stale-clear paths post (#9709).
pub mod verdict_stale_notice;
pub mod watch_registry;
pub mod watchdog;
pub mod watchdog_provisioning_guard;
pub mod work_finder;
pub mod worker_spawn;
pub mod workspace_hold;
pub mod workspace_pool;
pub mod workspace_registry;
pub mod worktree_activity;
pub mod worktree_cli;
pub mod worktree_disk_status;
pub mod worktree_ops;
pub mod worktree_reaper;
pub mod worktree_root;
pub mod worktree_state;
pub mod write_scope;
#[cfg(test)]
pub(crate) mod write_scope_test_support;

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
    #[serial_test::serial(loom_config_env)]
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
    #[serial_test::serial(loom_config_env)]
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

pub mod native_readiness;
pub mod native_state_reclaim;
pub mod native_tools;
pub mod session_exec;
