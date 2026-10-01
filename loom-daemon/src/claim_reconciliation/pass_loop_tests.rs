//! Mid-pass rate-limit-breaker re-check coverage for
//! [`super::run_reconciliation_pass_over_roots`] (Issue #8953).
//!
//! `run_reconciliation_pass_over_roots` is exercised directly (rather than
//! through `run_reconciliation_pass`) with an INJECTED `is_suppressed`
//! closure, not the real process-global rate-limit breaker: that breaker's
//! `GLOBAL` handle is a `OnceLock` shared by the whole test binary
//! ("first registration wins"), so registering it from a test here would leak
//! into every other test that happens to run afterward in the same process —
//! the same hazard `observability/collector/tests.rs` documents and avoids
//! for the sibling `host_breaker`. Injecting the check keeps this fully
//! self-contained.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::run_reconciliation_pass_over_roots;
use crate::write_scope_test_support::WritableRoot;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;
// #9548: gate-reaching tests hold the default serial key; see `crate::write_scope_test_support`.

/// Fake `gh` (tests only, Issue #8953) that logs `$(pwd) $*` for every
/// invocation — the `$(pwd)` prefix is what lets this test tell which of two
/// workspace roots a given call came from, since the argv alone
/// (`reconcile_workspace` / `reconcile_pr_claims` / `reconcile_pr_verdicts`
/// all issue the identical argv regardless of `--repo`/cwd here) does not.
/// Every subcommand this pass's helpers issue (`gh api ...`, `gh pr list
/// ...`) is answered with a trivial empty-but-well-formed success so none of
/// the four per-root passes has anything to act on.
fn write_fake_gh_logging_cwd(
    dir: &std::path::Path,
    gh_log: &std::path::Path,
) -> std::path::PathBuf {
    let fake_gh = dir.join("fake-gh-cwd.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s %s\n' "$(pwd)" "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[]'
  exit 0
fi
exit 0
"#,
        log = gh_log.display(),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

/// AC: once the injected breaker check reports suppressed, the per-repo loop
/// stops issuing further `gh` calls for the REMAINDER of the pass — the
/// second (and any later) root gets ZERO invocations, mirroring "a trip
/// triggered by repo N stops repos N+1..len() in the same pass" from the
/// issue's root-cause summary. The closure simulates a trip caused by repo
/// A's own `gh` call (in production, surfaced via `global_observe_failure`
/// inside `forge::reconcile_workspace` et al) by reporting suppressed from
/// the SECOND check onward — i.e. every check after the one guarding root A.
#[test]
#[serial_test::serial]
fn run_reconciliation_pass_over_roots_stops_after_breaker_trips_mid_pass() {
    let dir = tempdir().unwrap();
    let root_a = dir.path().join("repo-a");
    let root_b = dir.path().join("repo-b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let logging_gh = write_fake_gh_logging_cwd(dir.path(), &gh_log);
    // #9548: both roots are registered, writable checkouts. One `gh` answers
    // for the pass, so the two fixtures' permission answers are chained in
    // front of the logging fake.
    let ws_a = WritableRoot::register_with_gh(&root_a, &logging_gh);
    let ws_b = WritableRoot::register_with_gh(&root_b, &ws_a.gh);
    let fake_gh = ws_b.gh.clone();

    let roots = vec![root_a.clone(), root_b.clone()];
    let checks = std::sync::atomic::AtomicUsize::new(0);
    let stats = run_reconciliation_pass_over_roots(&roots, &fake_gh, false, || {
        checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1
    });

    assert_eq!(
        stats.roots_processed, 1,
        "the loop must stop after processing exactly the first root"
    );
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("repo-a"),
        "root A (processed before the trip) should have made at least one gh call; got: \
         {gh_calls:?}"
    );
    assert!(
        !gh_calls.contains("repo-b"),
        "root B must receive ZERO gh calls once the breaker trips mid-pass; got: {gh_calls:?}"
    );
}

/// Symmetric negative: when the breaker never trips, every root is visited —
/// confirms the new per-iteration check does not change behavior in the
/// common (not-suppressed) case.
#[test]
#[serial_test::serial]
fn run_reconciliation_pass_over_roots_visits_every_root_when_never_suppressed() {
    let dir = tempdir().unwrap();
    let root_a = dir.path().join("repo-a");
    let root_b = dir.path().join("repo-b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let logging_gh = write_fake_gh_logging_cwd(dir.path(), &gh_log);
    // #9548: both roots are registered, writable checkouts. One `gh` answers
    // for the pass, so the two fixtures' permission answers are chained in
    // front of the logging fake.
    let ws_a = WritableRoot::register_with_gh(&root_a, &logging_gh);
    let ws_b = WritableRoot::register_with_gh(&root_b, &ws_a.gh);
    let fake_gh = ws_b.gh.clone();

    let roots = vec![root_a.clone(), root_b.clone()];
    let stats = run_reconciliation_pass_over_roots(&roots, &fake_gh, false, || false);

    assert_eq!(stats.roots_processed, 2, "both roots must be processed when never suppressed");
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(gh_calls.contains("repo-a"), "root A should have been called; got: {gh_calls:?}");
    assert!(gh_calls.contains("repo-b"), "root B should have been called; got: {gh_calls:?}");
}

/// #9548 negative controls, through the real gate: a root this installation
/// may not write to gets no pass at all (not the listing reads that feed the
/// writes, and never a write), while the loop still counts it as visited.
///
/// - A fork checkout, whose `gh` target is its `upstream` remote, is refused
///   before any forge call, even the permission probe.
/// - A registered checkout whose credential only has `pull` is refused on the
///   probe's answer; the pass itself makes no call.
#[test]
#[serial_test::serial]
fn a_root_outside_the_write_scope_is_skipped_without_a_forge_call() {
    let dir = tempdir().unwrap();
    let fork = dir.path().join("fork-checkout");
    std::fs::create_dir_all(fork.join(".loom")).unwrap();
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/me/widgets.git",
        ],
        &[
            "remote",
            "add",
            "upstream",
            "https://github.com/acme/widgets.git",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(&fork)
            .status()
            .unwrap()
            .success());
    }
    let read_only = dir.path().join("read-only-checkout");
    let gh_log = dir.path().join("gh-invocations.log");
    let logging_gh = write_fake_gh_logging_cwd(dir.path(), &gh_log);

    let stats = run_reconciliation_pass_over_roots(&[fork], &logging_gh, false, || false);
    assert_eq!(stats.roots_processed, 1);
    assert_eq!(stats.total_checked + stats.total_pr_checked, 0);
    assert!(
        std::fs::read_to_string(&gh_log)
            .unwrap_or_default()
            .is_empty(),
        "no gh invocation at all for a checkout gh resolves to upstream"
    );

    let ws = WritableRoot::read_only(&read_only, Some(&logging_gh));
    let stats = run_reconciliation_pass_over_roots(&[read_only], &ws.gh, false, || false);
    assert_eq!(stats.roots_processed, 1);
    assert_eq!(stats.total_checked + stats.total_pr_checked, 0);
    assert!(
        std::fs::read_to_string(&gh_log)
            .unwrap_or_default()
            .is_empty(),
        "no pass runs on a root whose credential has only pull"
    );
}
