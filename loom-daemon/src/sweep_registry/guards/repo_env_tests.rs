//! Regression coverage for Issue #8263: the dispatch guards' `gh api`
//! claim-label timeline probe must pass a machine-global `LOOM_REPO` override
//! as the **`GH_REPO` environment variable**, never as a `--repo` flag.
//!
//! `gh api` has no `--repo` flag and exits `unknown flag: --repo` before
//! issuing any request. Because
//! [`SweepRegistry::fetch_claim_labeled_at`](super::super::SweepRegistry::fetch_claim_labeled_at)
//! is deliberately fail-open, the flag never surfaced as an error — it made
//! the cross-host claim signal permanently unverifiable on every host that
//! exports `LOOM_REPO`, so `claim_superseded_on_forge` always answered "not
//! superseded" and the label-restore path lost its only forge-side check that
//! another host had re-claimed the issue.
//!
//! The fake `gh` here enforces both halves of the real `gh api` contract: it
//! rejects a `--repo` argument, and it answers only when `GH_REPO` names the
//! expected repo.
//!
//! Sibling file rather than an inline `mod`: `guards.rs` is over the
//! file-size ratchet threshold (`scripts/file-size-baseline.txt`), and keeping
//! test modules out of it is that policy's own preferred remedy.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

const WANT_REPO: &str = "rjwalters/loom";

/// A registry whose `gh` is a fake that mimics the real `gh api`'s repo
/// selection: `--repo` is rejected outright, and the timeline answer is
/// printed only when `GH_REPO` matches [`WANT_REPO`].
fn repo_env_registry(ws: &Path, stdout_payload: &str) -> SweepRegistry {
    let fake_gh = ws.join("fake-gh-repo-env.sh");
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
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    SweepRegistry::new(config)
}

/// AC (#8263): with `LOOM_REPO` exported, the claim-label timeline probe still
/// reaches `gh` and returns the timestamp, rather than its fail-open `None`.
#[test]
#[serial]
fn fetch_claim_labeled_at_reaches_gh_under_a_loom_repo_override() {
    let dir = tempdir().unwrap();
    let registry = repo_env_registry(dir.path(), "\"2026-09-19T12:00:00Z\"");

    std::env::set_var("LOOM_REPO", WANT_REPO);
    let at = registry.fetch_claim_labeled_at(8263);
    std::env::remove_var("LOOM_REPO");

    assert_eq!(
        at.map(|t| t.to_rfc3339()),
        Some("2026-09-19T12:00:00+00:00".to_string()),
        "a `--repo` flag would have made `gh api` exit before the request, leaving \
         this fail-open probe permanently unable to see a peer's re-claim"
    );
}

/// And the signal that depends on it: a claim (re-)applied after this sweep's
/// own claim time is reported as superseded. A rejected `--repo` flag made
/// this answer `false` unconditionally on `LOOM_REPO` hosts.
#[test]
#[serial]
fn claim_superseded_on_forge_is_observable_under_a_loom_repo_override() {
    let dir = tempdir().unwrap();
    let registry = repo_env_registry(dir.path(), "\"2026-09-19T12:00:00Z\"");
    let claimed_at = DateTime::parse_from_rfc3339("2026-09-19T11:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    std::env::set_var("LOOM_REPO", WANT_REPO);
    let superseded = registry.claim_superseded_on_forge(8263, claimed_at);
    std::env::remove_var("LOOM_REPO");

    assert!(
        superseded,
        "the forge says loom:building was re-applied an hour after this sweep claimed \
         it; a fail-open `false` here is the label-restore race #5017/#5282 guards"
    );
}

/// The negative control: the fixture answers ONLY when `GH_REPO` is set, so
/// the assertions above cannot pass vacuously.
#[test]
#[serial]
fn an_unset_loom_repo_is_rejected_by_the_fixture_so_the_assertions_are_real() {
    let dir = tempdir().unwrap();
    let registry = repo_env_registry(dir.path(), "\"2026-09-19T12:00:00Z\"");

    std::env::remove_var("LOOM_REPO");
    assert!(
        registry.fetch_claim_labeled_at(8263).is_none(),
        "precondition: this fixture requires GH_REPO"
    );
}
