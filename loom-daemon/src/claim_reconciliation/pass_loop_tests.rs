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
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

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
fn run_reconciliation_pass_over_roots_stops_after_breaker_trips_mid_pass() {
    let dir = tempdir().unwrap();
    let root_a = dir.path().join("repo-a");
    let root_b = dir.path().join("repo-b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = write_fake_gh_logging_cwd(dir.path(), &gh_log);

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
fn run_reconciliation_pass_over_roots_visits_every_root_when_never_suppressed() {
    let dir = tempdir().unwrap();
    let root_a = dir.path().join("repo-a");
    let root_b = dir.path().join("repo-b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = write_fake_gh_logging_cwd(dir.path(), &gh_log);

    let roots = vec![root_a.clone(), root_b.clone()];
    let stats = run_reconciliation_pass_over_roots(&roots, &fake_gh, false, || false);

    assert_eq!(stats.roots_processed, 2, "both roots must be processed when never suppressed");
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(gh_calls.contains("repo-a"), "root A should have been called; got: {gh_calls:?}");
    assert!(gh_calls.contains("repo-b"), "root B should have been called; got: {gh_calls:?}");
}

/// #9548: a root this installation may not write to gets no pass at all —
/// not the listing reads that feed the writes, and never a write — while the
/// loop still counts it as visited.
#[test]
fn a_root_outside_the_write_scope_is_skipped_without_a_forge_call() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("fork-checkout");
    std::fs::create_dir_all(&root).unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = write_fake_gh_logging_cwd(dir.path(), &gh_log);

    crate::write_scope::test_override::deny(Some("gh resolves this checkout to upstream"));
    let stats = run_reconciliation_pass_over_roots(&[root], &fake_gh, false, || false);
    crate::write_scope::test_override::deny(None);

    assert_eq!(stats.roots_processed, 1);
    assert_eq!(stats.total_checked + stats.total_pr_checked, 0);
    assert!(
        std::fs::read_to_string(&gh_log)
            .unwrap_or_default()
            .is_empty(),
        "no gh invocation for a refused root"
    );
}
