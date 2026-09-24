//! The one place a machine-global `LOOM_REPO` override is applied to a
//! **`gh api`** invocation (Issue #8263).
//!
//! # The bug this module exists to make unrepeatable
//!
//! `gh api` has **no `--repo` flag**. Only the porcelain subcommands
//! (`gh issue`, `gh pr`, `gh repo`) accept one; `gh api` exits with
//! `unknown flag: --repo` *before issuing any request*. It resolves the
//! `{owner}` / `{repo}` placeholders from the cwd's git remote, or — when the
//! cwd is not the intended repo — from the **`GH_REPO` environment variable**.
//!
//! Six `gh api` call sites in this crate copied the `--repo` idiom from their
//! `gh issue` / `gh pr` neighbours. Because every one of those probes is
//! deliberately fail-open (`None` / `false` on any `gh` failure), the result
//! was not a crash but a *silent degradation* on every host that exports
//! `LOOM_REPO`: `claim_reconciliation`'s `issue_is_confirmed_closed` reported
//! a genuinely-closed issue as not-confirmed-closed, and the lease /
//! claim-label / blocked-label timeline probes all returned "cannot verify"
//! forever. A single-owner fleet that never exports `LOOM_REPO` saw nothing
//! at all, which is exactly why it survived six copies.
//!
//! # Why a helper rather than six inline `if let`s
//!
//! The idiom was wrong in five places because it was *copied* — the fix is one
//! named function that cannot be copied wrong, plus a scan
//! (`loom-daemon/tests/gh_api_repo_flag.rs`) that fails CI the moment a new
//! `gh api` builder appends a `--repo` flag instead of calling this.
//!
//! # Not for `gh issue` / `gh pr` / `gh repo`
//!
//! Those take `--repo` and must keep passing it — this helper is strictly for
//! `gh api` builders.
//!
//! # Crate-wide test-isolation invariant for `LOOM_REPO` (#8496)
//!
//! `LOOM_REPO` is process-global, and roughly a dozen test modules mutate it
//! (`gh_repo_env`, `peer_claims::repo_slug_tests`,
//! `claim_reconciliation::repo_env_tests`, `quarantine_reconciliation`, and the
//! `sweep_registry` guards / dispatch / reaper / watchdog / outcome-journal test
//! modules). **Every one of those tests MUST use the crate-default, unnamed
//! `#[serial]` key** — `serial_test` only mutually excludes tests that share the
//! *same* key, so a second key is a second, concurrently-running group and a
//! `remove_var("LOOM_REPO")` landing between another group's `set_var` and its
//! read silently changes that test's repo identity. Same race class as the
//! `LOOM_GH_BIN` split fixed in #8465 / #8480.
//!
//! **Why the default key and not a dedicated `loom_repo_env` one** (the shape
//! #8465 / #8480 used for `LOOM_GH_BIN`): most of those bare `#[serial]`
//! attributes are *already* load-bearing for other shared state they happen to
//! touch as well — a fake `gh` on `PATH`, the process cwd, `HOME` (#4547).
//! Moving them to a `LOOM_REPO`-specific key would keep this variable safe
//! while silently un-serialising all of that against the rest of the default
//! group. The variable with exactly one writer group (`LOOM_GH_BIN`) is the
//! one that can afford a private key; this one cannot.
//!
//! The one test that also needs `loom_config_env` (for `LOOM_GH_BIN`) —
//! `role_collision::tests::rest_queue_source_parses_the_live_listing_shape` —
//! holds **both** locks by nesting a `#[serial]` body inside the
//! `#[serial(loom_config_env)]` test, because multi-key `#[serial(a, b)]` cannot
//! name the default group (its key is the empty string; attribute keys are
//! parsed as identifiers). See that test's own doc comment for the lock order.

use std::process::Command;

/// Apply a machine-global `LOOM_REPO` override to a **`gh api`** command as
/// the `GH_REPO` environment variable.
///
/// A no-op when `LOOM_REPO` is unset (the single-owner-fleet default), in
/// which case `gh api` resolves the repo from the command's `current_dir`
/// remote exactly as before.
pub(crate) fn apply_loom_repo_override(cmd: &mut Command) {
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.env("GH_REPO", repo);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn envs(cmd: &Command) -> Vec<(String, Option<String>)> {
        cmd.get_envs()
            .map(|(k, v)| {
                (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))
            })
            .collect()
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// The whole point: the override lands as an ENV VAR and never as an
    /// argument. A `--repo` argument would make the real `gh api` exit
    /// `unknown flag: --repo` before issuing a request.
    #[test]
    #[serial]
    fn a_loom_repo_override_becomes_the_gh_repo_env_var_never_an_argument() {
        let mut cmd = Command::new("gh");
        cmd.arg("api").arg("repos/{owner}/{repo}/issues/1");

        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        apply_loom_repo_override(&mut cmd);
        std::env::remove_var("LOOM_REPO");

        assert!(
            envs(&cmd).contains(&("GH_REPO".to_string(), Some("rjwalters/loom".to_string()))),
            "LOOM_REPO must reach `gh api` as GH_REPO: {:?}",
            envs(&cmd)
        );
        assert!(
            !args(&cmd).iter().any(|a| a == "--repo"),
            "`gh api` has no --repo flag; passing one aborts the call: {:?}",
            args(&cmd)
        );
    }

    /// Unset `LOOM_REPO` (the single-owner-fleet default) leaves the command
    /// completely untouched — `gh api` keeps resolving from `current_dir`.
    #[test]
    #[serial]
    fn an_unset_loom_repo_leaves_the_command_untouched() {
        let mut cmd = Command::new("gh");
        cmd.arg("api").arg("repos/{owner}/{repo}/issues/1");

        std::env::remove_var("LOOM_REPO");
        apply_loom_repo_override(&mut cmd);

        assert!(
            !envs(&cmd).iter().any(|(k, _)| k == "GH_REPO"),
            "an unset LOOM_REPO must not inject GH_REPO: {:?}",
            envs(&cmd)
        );
        assert!(!args(&cmd).iter().any(|a| a == "--repo"), "{:?}", args(&cmd));
    }
}
