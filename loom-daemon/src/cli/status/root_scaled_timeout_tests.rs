//! **Issue #8224.** `loom-daemon status`'s single-attempt IPC budget must be
//! floored by the registered workspace-root count, not just by host load.
//!
//! These mirror
//! `loom_daemon::status_budget::tests::the_status_build_stays_within_budget_at_the_documented_root_ceiling`
//! (#8163's daemon-side regression guard) from the *client* side: a synthetic
//! registry at the documented root ceiling must not leave `status` waiting on
//! the old fixed `5s`/30s-capped budget for an `O(roots)`
//! `build_daemon_status` that #8163 measured at `13.1s`/`14.3s`.

use super::{
    describe_status_timeout, resolve_status_timeout, scale_timeout_for_load,
    DEFAULT_STATUS_TIMEOUT, MAX_SCALED_STATUS_TIMEOUT,
};
use crate::cli::common::DAEMON_IPC_TIMEOUT_ENV;
use loom_daemon::status_budget::{self, DOCUMENTED_MAX_ROOTS, MAX_ROOT_SCALED_PROBE_TIMEOUT};
use loom_daemon::workspace_registry::{WorkspaceRegistry, REGISTRY_PATH_ENV};
use serial_test::serial;
use std::time::Duration;

/// Point `REGISTRY_PATH_ENV` at a throwaway registry holding `n` roots, and
/// return the tempdir so the caller keeps it alive for the test's duration.
fn synthetic_registry(n: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let reg_path = dir.path().join("workspaces.json");
    let mut reg = WorkspaceRegistry::default();
    for i in 0..n {
        let root = dir.path().join(format!("repo-{i:02}"));
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        reg.add(&root, None).unwrap();
    }
    reg.save(&reg_path).unwrap();
    std::env::set_var(REGISTRY_PATH_ENV, &reg_path);
    dir
}

/// The #8224 regression itself: at the documented root ceiling the resolved
/// budget must cover a `build_daemon_status` walk over that many roots, which
/// means growing well past the `5s` default this invocation got on an idle
/// host before #8224 — the exact fleet shape the #8163 report describes.
#[test]
#[serial]
fn many_roots_raise_the_single_attempt_budget_past_the_old_fixed_default() {
    std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    let _dir = synthetic_registry(DOCUMENTED_MAX_ROOTS);

    let info = resolve_status_timeout(None);
    let expected = status_budget::client_probe_budget(DOCUMENTED_MAX_ROOTS);
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert_eq!(info.root_count, DOCUMENTED_MAX_ROOTS, "root count comes from the registry");
    assert!(
        info.timeout >= expected,
        "a {DOCUMENTED_MAX_ROOTS}-root host resolved {:?}, under the {expected:?} the status \
         build over that many roots is budgeted at — this is the #8224 regression",
        info.timeout
    );
    assert!(
        info.timeout > DEFAULT_STATUS_TIMEOUT,
        "root scaling must beat the pre-#8224 5s default, got {:?}",
        info.timeout
    );
}

/// The root-scaled floor is deliberately **not** clamped by
/// [`MAX_SCALED_STATUS_TIMEOUT`]: that ceiling bounds a speculative load
/// multiplier, whereas the `O(roots)` build cost is measured. A fleet large
/// enough for the root-scaled budget to exceed `30s` must actually get it,
/// bounded instead by
/// [`status_budget::MAX_ROOT_SCALED_PROBE_TIMEOUT`].
#[test]
#[serial]
fn the_root_scaled_floor_is_not_clamped_by_the_load_scaling_ceiling() {
    // Chosen so `client_probe_budget` lands above the 30s load ceiling but
    // still below the 45s root-scaled cap: (0.5s + 96*0.2s) * 2 = 39.4s.
    const ROOTS: usize = 96;
    assert!(status_budget::client_probe_budget(ROOTS) > MAX_SCALED_STATUS_TIMEOUT);

    std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    let _dir = synthetic_registry(ROOTS);
    let info = resolve_status_timeout(None);
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert!(
        info.timeout > MAX_SCALED_STATUS_TIMEOUT,
        "a {ROOTS}-root host must out-budget the load-scaling ceiling, got {:?}",
        info.timeout
    );
    assert!(
        info.timeout <= MAX_ROOT_SCALED_PROBE_TIMEOUT,
        "…but still be bounded: a one-shot CLI must never hang for minutes"
    );
}

/// The budget covers the builds #8163 actually measured (`13.1s`/`14.3s` on a
/// "several dozen" root host) — the same assertion `status_budget`'s own
/// suite makes about the primitive, re-made here against the value `status`
/// really waits on.
#[test]
#[serial]
fn the_resolved_budget_covers_the_builds_issue_8163_measured() {
    std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    let _dir = synthetic_registry(36);

    let info = resolve_status_timeout(None);
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert_eq!(info.root_count, 36);
    assert!(
        info.timeout > Duration::from_millis(14_300),
        "a 36-root host must out-budget the worst build #8163 measured, got {:?}",
        info.timeout
    );
}

/// A single-workspace host is bit-for-bit unchanged: at `root_count == 1` the
/// probe budget is `1.4s`, well under the `5s` default, so the raise-only
/// `max` is a no-op. This is what makes #8224 safe to apply unconditionally
/// to the single attempt (no escalation gate, unlike `cli::health`).
#[test]
#[serial]
fn a_single_root_host_is_unchanged() {
    std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    let _dir = synthetic_registry(1);

    let info = resolve_status_timeout(None);
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert_eq!(info.root_count, 1);
    assert!(
        status_budget::client_probe_budget(1) < DEFAULT_STATUS_TIMEOUT,
        "sanity: one root must not on its own raise the default"
    );
    // Compared against the load-scaled base rather than the raw 5s constant so
    // this stays exact on a contended CI runner: the claim is "root scaling
    // changed nothing", not "this host happened to be idle".
    assert_eq!(
        info.timeout,
        scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, info.load_per_core),
        "a single-workspace host must resolve exactly the pre-#8224 budget"
    );
}

/// An explicit `--timeout-secs` still wins verbatim, even on a many-root host:
/// an operator asking for a 3s probe is asking for a fast negative, and the
/// existing AC1 precedence (#6011) is unchanged. The root count is still
/// *reported*, so the resulting message can explain why 3s was optimistic.
#[test]
#[serial]
fn an_explicit_flag_still_wins_over_the_root_scaled_floor() {
    std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    let _dir = synthetic_registry(DOCUMENTED_MAX_ROOTS);

    let info = resolve_status_timeout(Some(3));
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert_eq!(info.timeout, Duration::from_secs(3));
    assert_eq!(info.root_count, DOCUMENTED_MAX_ROOTS);
}

/// `LOOM_DAEMON_IPC_TIMEOUT_MS` and the root-scaled floor are both raise-only,
/// so the wider of the two wins and neither narrows the other.
#[test]
#[serial]
fn the_env_floor_and_the_root_floor_are_both_raise_only() {
    let _dir = synthetic_registry(DOCUMENTED_MAX_ROOTS);

    // Env floor below the root-scaled budget: root scaling wins.
    std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "6000");
    let narrow = resolve_status_timeout(None);
    // Env floor above even the root-scaled cap: the operator's value wins.
    std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "90000");
    let wide = resolve_status_timeout(None);
    std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    std::env::remove_var(REGISTRY_PATH_ENV);

    assert!(
        narrow.timeout >= status_budget::client_probe_budget(DOCUMENTED_MAX_ROOTS),
        "a 6s env floor must not narrow the root-scaled budget, got {:?}",
        narrow.timeout
    );
    assert_eq!(
        wide.timeout,
        Duration::from_secs(90),
        "an operator asking for 90s must not be narrowed to the 45s root-scaled cap"
    );
}

/// #6011's "say why it fired" contract, extended: the rendered timeout
/// message names the root count, so an operator reading a `status` timeout on
/// a many-workspace host can see the input that sized the budget without
/// reading source.
#[test]
#[serial]
fn the_timeout_message_names_the_root_count() {
    std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    let _dir = synthetic_registry(7);

    let info = resolve_status_timeout(None);
    std::env::remove_var(REGISTRY_PATH_ENV);

    let rendered = describe_status_timeout(&info);
    assert!(
        rendered.contains("7 registered workspace root(s)"),
        "expected the root count in the rendered timeout, got: {rendered}"
    );
}
