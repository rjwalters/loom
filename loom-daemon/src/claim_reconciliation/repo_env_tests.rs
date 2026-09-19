//! Regression coverage for Issue #8263: this module's three `gh api` probes
//! must pass a machine-global `LOOM_REPO` override as the **`GH_REPO`
//! environment variable**, never as a `--repo` flag.
//!
//! `gh api` has no `--repo` flag and exits `unknown flag: --repo` before
//! issuing any request. Every probe here is deliberately fail-open, so the
//! flag did not produce an error anyone could see — it produced a permanent
//! "cannot verify" on every host that exports `LOOM_REPO`:
//! [`forge::issue_is_confirmed_closed`] reported a genuinely-closed issue as
//! not-confirmed-closed (so its stale `loom:building` claim was reclaimed),
//! and both timeline/lease probes returned `None`/`ReadFailed` forever.
//!
//! The fake `gh` below enforces BOTH halves of the contract the way the real
//! `gh` does: it rejects a `--repo` argument outright, and it refuses to
//! answer unless `GH_REPO` names the expected repo. A regression therefore
//! shows up as the fail-open result, which is exactly the production symptom.
//!
//! Sibling file rather than an inline `mod`: `claim_reconciliation.rs` is over
//! the file-size ratchet threshold (`scripts/file-size-baseline.txt`), and
//! keeping test modules out of it is that policy's own preferred remedy.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use serial_test::serial;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

const WANT_REPO: &str = "rjwalters/loom";

/// A fake `gh` that behaves like the real `gh api` about repo selection: it
/// REJECTS a `--repo` argument (`gh api` has none — only `gh issue`/`gh pr`
/// do) and resolves the repo from `GH_REPO`, printing `stdout_payload` only
/// when that env var matches [`WANT_REPO`].
fn write_fake_gh_requiring_gh_repo_env(
    dir: &std::path::Path,
    name: &str,
    stdout_payload: &str,
) -> std::path::PathBuf {
    let fake_gh = dir.join(name);
    let script = format!(
        "#!/usr/bin/env bash\n\
         for a in \"$@\"; do\n\
         if [[ \"$a\" == \"--repo\" ]]; then\n\
         printf 'unknown flag: --repo\\n' >&2\n\
         exit 1\n\
         fi\n\
         done\n\
         if [[ \"${{GH_REPO:-}}\" != '{WANT_REPO}' ]]; then\n\
         printf 'gh: could not determine the repository\\n' >&2\n\
         exit 1\n\
         fi\n\
         printf '%s\\n' '{payload}'\n\
         exit 0\n",
        payload = stdout_payload.replace('\'', "'\\''"),
    );
    std::fs::write(&fake_gh, script).unwrap();
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
    }
    fake_gh
}

/// AC (#8263): with `LOOM_REPO` exported, the closed-state gate still reaches
/// `gh` and returns its non-fail-open answer (`true` for a closed issue).
#[test]
#[serial]
fn issue_is_confirmed_closed_reaches_gh_under_a_loom_repo_override() {
    let dir = tempdir().unwrap();
    let gh = write_fake_gh_requiring_gh_repo_env(dir.path(), "fake-gh-state.sh", "closed");

    std::env::set_var("LOOM_REPO", WANT_REPO);
    let confirmed = forge::issue_is_confirmed_closed(&gh, dir.path(), 8263);
    std::env::remove_var("LOOM_REPO");

    assert!(
        confirmed,
        "a `--repo` flag would have made `gh api` exit before the request, and this \
         fail-open probe would have reported a genuinely-closed issue as unconfirmed"
    );
}

/// AC (#8263): the claim-label timeline probe still parses a real timestamp
/// under `LOOM_REPO`, rather than falling back to its `None` "no recency
/// signal" leg.
#[test]
#[serial]
fn fetch_claim_labeled_at_reaches_gh_under_a_loom_repo_override() {
    let dir = tempdir().unwrap();
    let gh = write_fake_gh_requiring_gh_repo_env(
        dir.path(),
        "fake-gh-claim-labeled.sh",
        "\"2026-09-19T12:00:00Z\"",
    );

    std::env::set_var("LOOM_REPO", WANT_REPO);
    let at = forge::fetch_claim_labeled_at(&gh, dir.path(), 8263, "loom:building");
    std::env::remove_var("LOOM_REPO");

    assert_eq!(
        at.map(|t| t.to_rfc3339()),
        Some("2026-09-19T12:00:00+00:00".to_string()),
        "a `--repo` flag would have made this probe return None on every \
         LOOM_REPO-configured host"
    );
}

/// AC (#8263): the lease-freshness probe — the FINAL gate before a
/// `loom:building` claim is reclaimed — still reports `Found` under
/// `LOOM_REPO`, not the `ReadFailed` a rejected `--repo` flag produced.
#[test]
#[serial]
fn fetch_freshest_lease_updated_at_reaches_gh_under_a_loom_repo_override() {
    let dir = tempdir().unwrap();
    let gh = write_fake_gh_requiring_gh_repo_env(
        dir.path(),
        "fake-gh-lease.sh",
        "\"2026-09-19T13:00:00Z\"",
    );

    std::env::set_var("LOOM_REPO", WANT_REPO);
    let probe = forge::fetch_freshest_lease_updated_at(&gh, dir.path(), 8263);
    std::env::remove_var("LOOM_REPO");

    assert_eq!(
        probe,
        forge::LeaseProbe::Found(
            DateTime::parse_from_rfc3339("2026-09-19T13:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        ),
        "a `--repo` flag made this read fail, and a ReadFailed lease probe is \
         exactly what #7591 showed can evict a still-live claim"
    );
}

/// The negative control: the same probes with `LOOM_REPO` UNSET must still
/// work (the single-owner-fleet default), proving the fixture's discriminating
/// power comes from the env var and not from an always-failing fake `gh`.
#[test]
#[serial]
fn an_unset_loom_repo_is_rejected_by_the_fixture_so_the_assertions_are_real() {
    let dir = tempdir().unwrap();
    let gh = write_fake_gh_requiring_gh_repo_env(dir.path(), "fake-gh-control.sh", "closed");

    std::env::remove_var("LOOM_REPO");
    assert!(
        !forge::issue_is_confirmed_closed(&gh, dir.path(), 8263),
        "precondition: this fixture answers ONLY when GH_REPO is set, so the \
         positive tests above cannot pass vacuously"
    );
}
