//! **Issue #8311.** The safehouse ChatOps `!status` round-trip must be
//! budgeted by the registered workspace-root count, not by a fixed 30s — and
//! *only* that command must be.
//!
//! [`IpcExecutor`] is the fourth and last `Request::DaemonStatus` client in
//! the tree; #8163/#8224 (PR #8307) floored the other three
//! (`cli::health::resolve_retry_timeout`, `cli::status::resolve_status_timeout`,
//! `serve::status_fetch::fetch_budget`) and its own "Scope" section recorded
//! that no further callers existed, which this one falsifies. These tests
//! mirror `cli::status::root_scaled_timeout_tests` and
//! `serve::status_fetch::tests` from the ChatOps side.

use super::{command_to_request, is_root_scaled, round_trip_budget, IpcExecutor, IPC_TIMEOUT};
use crate::safehouse_chatops::Command;
use crate::status_budget::{self, DOCUMENTED_MAX_ROOTS, MAX_ROOT_SCALED_PROBE_TIMEOUT};
use crate::types::Request;
use crate::workspace_registry::{WorkspaceRegistry, REGISTRY_PATH_ENV};
use std::path::PathBuf;
use std::time::Duration;

/// The root count at which the `O(roots)` status build's client budget first
/// exceeds the fixed [`IPC_TIMEOUT`]: `(0.5s + 73 * 0.2s) * 2 = 30.2s`. At or
/// above this, a pre-#8311 ChatOps `!status` reported a timeout on a daemon
/// that was merely walking its registry.
const FIRST_FALSE_TIMEOUT_ROOTS: usize = 73;

/// Far enough past the crossover to sit at the [`MAX_ROOT_SCALED_PROBE_TIMEOUT`]
/// cap: `(0.5s + 110 * 0.2s) * 2 = 45s`.
const CAPPED_ROOTS: usize = 110;

/// Every [`Command`] variant, paired with whether its IPC round-trip is
/// root-scaled. The `match` is the point: a seventh ChatOps verb cannot be
/// added without stating its answer here, so "only `status` is scaled" stays a
/// checked claim rather than a comment.
fn expect_root_scaled(command: &Command) -> bool {
    match command {
        Command::Status => true,
        Command::Dispatch { .. }
        | Command::Cancel { .. }
        | Command::Unblock { .. }
        | Command::Watch { .. }
        | Command::Confirm { .. } => false,
    }
}

/// One instance of each variant, for the sweeps below.
fn all_commands() -> Vec<Command> {
    vec![
        Command::Status,
        Command::Dispatch { issue: 42 },
        Command::Cancel {
            sweep: "sweep-issue-42-1".to_owned(),
        },
        Command::Unblock { issue: 42 },
        Command::Watch { number: 42 },
        Command::Confirm {
            nonce: "abc123".to_owned(),
        },
    ]
}

/// Point `REGISTRY_PATH_ENV` at a throwaway registry holding `n` roots; the
/// returned tempdir must outlive the call under test.
fn synthetic_registry(n: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let reg_path = dir.path().join("workspaces.json");
    let mut reg = WorkspaceRegistry::default();
    for i in 0..n {
        let root = dir.path().join(format!("repo-{i:03}"));
        std::fs::create_dir_all(root.join(".loom")).expect("mkdir");
        reg.add(&root, None).expect("register root");
    }
    reg.save(&reg_path).expect("save registry");
    std::env::set_var(REGISTRY_PATH_ENV, &reg_path);
    dir
}

// ===================================================================
// The budget decision (pure — no socket, no registry)
// ===================================================================

/// A single-workspace host is bit-for-bit unchanged: at `root_count == 1` the
/// probe budget is `1.4s`, far under [`IPC_TIMEOUT`], so the raise-only `max`
/// is a no-op and ChatOps' latency profile does not move.
#[test]
fn a_single_root_host_keeps_the_fixed_ipc_timeout() {
    assert_eq!(round_trip_budget(IPC_TIMEOUT, &Request::DaemonStatus, 1), IPC_TIMEOUT);
}

/// Even at the documented ceiling the fixed 30s still covers the build
/// (`(0.5s + 64 * 0.2s) * 2 = 26.6s`) — ChatOps' 30s base was *more* generous
/// than the `5s` the CLI/dashboard used, which is why #8163/#8224 did not
/// surface this caller. The budget must therefore not *shrink* here either.
#[test]
fn the_documented_ceiling_is_still_covered_by_the_fixed_base() {
    let budget = round_trip_budget(IPC_TIMEOUT, &Request::DaemonStatus, DOCUMENTED_MAX_ROOTS);
    assert!(status_budget::client_probe_budget(DOCUMENTED_MAX_ROOTS) < IPC_TIMEOUT);
    assert_eq!(
        budget, IPC_TIMEOUT,
        "the floor raises, never narrows: a {DOCUMENTED_MAX_ROOTS}-root host keeps the wider \
         30s base"
    );
}

/// **The #8311 regression itself.** Past the crossover the status build's own
/// budget exceeds the fixed 30s, so a pre-#8311 `!status` replied "round-trip
/// timed out" on a healthy daemon. The resolved budget must now cover it.
#[test]
fn many_roots_raise_the_status_budget_past_the_fixed_thirty_seconds() {
    for roots in [FIRST_FALSE_TIMEOUT_ROOTS, CAPPED_ROOTS] {
        let budget = round_trip_budget(IPC_TIMEOUT, &Request::DaemonStatus, roots);
        let expected = status_budget::client_probe_budget(roots);
        assert!(
            expected > IPC_TIMEOUT,
            "test premise: {roots} roots must out-cost the fixed {IPC_TIMEOUT:?}, got {expected:?}"
        );
        assert_eq!(
            budget, expected,
            "a {roots}-root host must wait the {expected:?} its status build is budgeted at, not \
             the fixed {IPC_TIMEOUT:?} — this is the #8311 false ChatOps timeout"
        );
    }
    assert_eq!(
        round_trip_budget(IPC_TIMEOUT, &Request::DaemonStatus, CAPPED_ROOTS),
        MAX_ROOT_SCALED_PROBE_TIMEOUT,
        "{CAPPED_ROOTS} roots should land exactly on the documented cap"
    );
}

/// Monotonic and bounded: one more registered workspace never buys a *smaller*
/// budget, and a corrupted/enormous registry can never pin the ChatOps
/// executor (and with it the inbound-steering task's reply) for minutes.
#[test]
fn the_status_budget_is_monotonic_and_bounded() {
    let mut prev = round_trip_budget(IPC_TIMEOUT, &Request::DaemonStatus, 0);
    for n in 1..=DOCUMENTED_MAX_ROOTS * 3 {
        let next = round_trip_budget(IPC_TIMEOUT, &Request::DaemonStatus, n);
        assert!(next >= prev, "round_trip_budget regressed at n={n}");
        prev = next;
    }
    assert_eq!(
        round_trip_budget(IPC_TIMEOUT, &Request::DaemonStatus, usize::MAX),
        MAX_ROOT_SCALED_PROBE_TIMEOUT
    );
}

// ===================================================================
// Scope: only `status` is root-scaled
// ===================================================================

/// The regression guard against widening the shared constant: every non-status
/// verb keeps the unscaled 30s at any root count, however absurd. Their 30s
/// has its own documented rationale (`DISPATCH_ACK_TIMEOUT`), and their cost
/// does not depend on how many workspaces this host has registered.
#[test]
fn only_status_is_root_scaled_every_other_command_keeps_the_fixed_budget() {
    for command in all_commands() {
        let Some(request) = command_to_request(&command) else {
            assert!(
                matches!(command, Command::Confirm { .. }),
                "only `confirm` maps to no request, got {command:?}"
            );
            continue;
        };
        assert_eq!(
            is_root_scaled(&request),
            expect_root_scaled(&command),
            "`{}` disagrees with the per-variant scaling decision",
            command.verb()
        );
        if expect_root_scaled(&command) {
            continue;
        }
        for roots in [1, DOCUMENTED_MAX_ROOTS, CAPPED_ROOTS, usize::MAX] {
            assert_eq!(
                round_trip_budget(IPC_TIMEOUT, &request, roots),
                IPC_TIMEOUT,
                "`{}` must keep the unscaled {IPC_TIMEOUT:?} at {roots} root(s) — #8311 is \
                 scoped to the status path and must not widen the shared constant",
                command.verb()
            );
        }
    }
}

// ===================================================================
// Wiring: the executor actually resolves from the local registry
// ===================================================================

fn executor() -> IpcExecutor {
    // `resolve_budget` never touches the socket, so an absent path is fine.
    IpcExecutor::new(PathBuf::from("/nonexistent/loom-daemon.sock"))
}

/// The executor reads the root count from the **local** workspace registry
/// (the same file `build_daemon_status` walks) and budgets `status` from it —
/// the wiring the pure tests above cannot see.
#[test]
#[serial_test::serial]
fn the_executor_budgets_status_from_the_local_registry() {
    let _dir = synthetic_registry(FIRST_FALSE_TIMEOUT_ROOTS);
    let (timeout, root_count) = executor().resolve_budget(&Request::DaemonStatus);
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert_eq!(root_count, Some(FIRST_FALSE_TIMEOUT_ROOTS));
    assert_eq!(timeout, status_budget::client_probe_budget(FIRST_FALSE_TIMEOUT_ROOTS));
    assert!(
        timeout > IPC_TIMEOUT,
        "a {FIRST_FALSE_TIMEOUT_ROOTS}-root host must out-wait the fixed {IPC_TIMEOUT:?}, got \
         {timeout:?}"
    );
}

/// …and it does not pay (or apply) that registry read for a non-status verb:
/// `dispatch` keeps the fixed budget and reports no root count, so the timeout
/// message never claims a root count that did not size it.
#[test]
#[serial_test::serial]
fn the_executor_leaves_other_commands_on_the_fixed_budget() {
    let _dir = synthetic_registry(CAPPED_ROOTS);
    let request = command_to_request(&Command::Dispatch { issue: 42 }).expect("dispatch maps");
    let (timeout, root_count) = executor().resolve_budget(&request);
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert_eq!(timeout, IPC_TIMEOUT);
    assert_eq!(root_count, None, "no root count sized this budget");
}

/// An absent/unreadable registry falls back to one root — byte-for-byte the
/// pre-#8311 behaviour on the overwhelmingly common single-workspace host.
#[test]
#[serial_test::serial]
fn an_absent_registry_reproduces_the_pre_fix_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var(REGISTRY_PATH_ENV, dir.path().join("does-not-exist.json"));
    let (timeout, root_count) = executor().resolve_budget(&Request::DaemonStatus);
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert_eq!(root_count, Some(1));
    assert_eq!(timeout, IPC_TIMEOUT);
    assert_eq!(status_budget::client_probe_budget(1), Duration::from_millis(1_400));
}
