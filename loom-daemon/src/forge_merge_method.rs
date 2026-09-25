//! `loom-daemon forge merge-method` (#8845) — resolve/validate the merge
//! method `merge-pr.sh` passes to `forge_merge_pr` / `forge auto-merge`.
//!
//! Split out of [`crate::forge_cmd`] as a sibling module per the file-size
//! ratchet (`scripts/check-file-size-budget.sh` / `.loom/docs/file-size-policy.md`)
//! — `forge_cmd.rs` was already near the 1000-code-line threshold, so this
//! subcommand's logic (and its tests) land in a new file rather than pushing
//! that one over.
//!
//! Previously `merge-pr.sh:461` called the shell `forge_detect_merge_method`
//! (`defaults/scripts/lib/forge-merge-method.sh`) unconditionally — squash >
//! merge > rebase preference order, no way to request a specific method. This
//! subcommand adds that: with no `--requested`, it preserves the exact same
//! auto-detect preference (including the fail-open-to-squash degenerate
//! case); with `--requested`, it validates the request against the repo's
//! actually allowed strategies and refuses — naming what IS allowed — rather
//! than silently falling back to squash, which would just reproduce the bug
//! #8845 reports with an extra ignored flag.
//!
//! GitHub only, natively (`gh api repos/<nwo>`, mirroring
//! [`crate::forge_cmd::github_auto_merge`]'s house style). Gitea declines
//! with [`crate::forge_cmd::EX_FORGE_DECLINED`], the same signal
//! [`crate::forge_cmd::handle_auto_merge`] already uses to hand Gitea back to
//! its shell fallback — `merge-pr.sh` falls back to its existing shell
//! `forge_detect_merge_method` there, which does not yet validate a requested
//! method on Gitea. That is a real, narrower-scope limitation of this issue
//! (Gitea repo-settings probing has no native Rust path yet anywhere in this
//! module group), not a silently-dropped one: the shell caller logs it.

use std::process::{Command, Stdio};

use anyhow::Result;

use crate::cmd_out::run_command;
use crate::forge_cmd::{detect_forge, gh_bin, ForgeType, EX_FORGE_DECLINED, FORGE_CMD_TIMEOUT};

/// Which of a repo's three merge strategies are actually enabled — the
/// GitHub/Gitea `allow_squash_merge` / `allow_merge_commit(s)` /
/// `allow_rebase_merge` triad, decoded once and passed around as plain
/// `bool`s so [`resolve_merge_method`] never has to know which forge or
/// transport it came from.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
struct RepoMergeFlags {
    #[serde(default)]
    allow_squash_merge: bool,
    #[serde(default, alias = "allow_merge_commits")]
    allow_merge_commit: bool,
    #[serde(default)]
    allow_rebase_merge: bool,
}

/// The pure decision behind `loom-daemon forge merge-method` (#8845): given
/// which strategies a repo allows, resolve either an explicit `requested`
/// method (validated) or today's auto-detect preference order.
///
/// - `requested` present and allowed -> `Ok(requested)`.
/// - `requested` present and NOT allowed -> `Err` naming every method the
///   repo actually allows — never a silent fallback to squash, which would
///   reproduce the bug this subcommand exists to fix (#8845's own report).
/// - `requested` absent -> the same squash > merge > rebase preference
///   `forge_detect_merge_method` (the shell predecessor this augments, in
///   `defaults/scripts/lib/forge-merge-method.sh`) has always used, including
///   its fail-open-to-squash degenerate case (a forge reporting every allow_*
///   flag false, which neither forge's UI actually permits).
fn resolve_merge_method(requested: Option<&str>, flags: RepoMergeFlags) -> Result<String, String> {
    if let Some(req) = requested {
        let allowed = match req {
            "squash" => flags.allow_squash_merge,
            "merge" => flags.allow_merge_commit,
            "rebase" => flags.allow_rebase_merge,
            other => {
                return Err(format!(
                    "unrecognized merge method '{other}' (expected one of: squash, merge, rebase)"
                ))
            }
        };
        if allowed {
            return Ok(req.to_string());
        }
        let mut names = Vec::new();
        if flags.allow_squash_merge {
            names.push("squash");
        }
        if flags.allow_merge_commit {
            names.push("merge");
        }
        if flags.allow_rebase_merge {
            names.push("rebase");
        }
        let allowed_desc = if names.is_empty() {
            "none (the forge reports every merge strategy disabled)".to_string()
        } else {
            names.join(", ")
        };
        return Err(format!(
            "requested merge method '{req}' is not allowed by this repository; allowed method(s): {allowed_desc}"
        ));
    }
    Ok(if flags.allow_squash_merge {
        "squash"
    } else if flags.allow_merge_commit {
        "merge"
    } else if flags.allow_rebase_merge {
        "rebase"
    } else {
        "squash"
    }
    .to_string())
}

/// GitHub backend for `loom-daemon forge merge-method`: fetch `repos/<nwo>`
/// and run [`resolve_merge_method`] against its `allow_*` flags. Returns the
/// process exit code to use — 0 with the resolved method on stdout, 1 with
/// the reason on stderr (either the vocabulary/validation error from
/// [`resolve_merge_method`], or "could not verify" when the repo probe
/// itself failed, which — unlike the no-`requested` case — must NOT fail
/// open to squash: silently ignoring an explicit, unverifiable request would
/// reproduce the exact bug #8845 reports.
fn github_merge_method(gh: &str, nwo: &str, requested: Option<&str>) -> i32 {
    let mut cmd = Command::new(gh);
    cmd.args(["api", &format!("repos/{nwo}")])
        .stdin(Stdio::null());
    let query = crate::cmd_out::decode_json::<RepoMergeFlags, _>(
        run_command(cmd, FORGE_CMD_TIMEOUT),
        |_| false,
    );
    let flags = match query {
        crate::cmd_out::Query::Populated(f) => f,
        // A struct with every field `#[serde(default)]` can never legitimately
        // decode to Query::Empty (an all-false decode is Populated(false,
        // false, false), which resolve_merge_method already treats as the
        // degenerate case) — kept only so the match is exhaustive.
        crate::cmd_out::Query::Empty => RepoMergeFlags::default(),
        crate::cmd_out::Query::Malformed { error, .. } => {
            if requested.is_none() {
                println!("squash");
                return 0;
            }
            eprintln!("could not verify {nwo}'s allowed merge methods: {error}");
            return 1;
        }
        crate::cmd_out::Query::Failed { stderr, .. } => {
            if requested.is_none() {
                println!("squash");
                return 0;
            }
            eprintln!("could not verify {nwo}'s allowed merge methods: {stderr}");
            return 1;
        }
        crate::cmd_out::Query::Unavailable(u) => {
            if requested.is_none() {
                println!("squash");
                return 0;
            }
            eprintln!("could not verify {nwo}'s allowed merge methods: {u}");
            return 1;
        }
    };
    match resolve_merge_method(requested, flags) {
        Ok(method) => {
            println!("{method}");
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// Handle `loom-daemon forge merge-method --repo <nwo> [--requested M]`
/// (#8845). GitHub resolves/validates natively via [`github_merge_method`];
/// Gitea declines with [`EX_FORGE_DECLINED`], mirroring
/// [`crate::forge_cmd::handle_auto_merge`] — `merge-pr.sh` falls back to its
/// existing shell `forge_detect_merge_method` (which does not yet accept a
/// requested method on Gitea; a documented, narrower-scope limitation for
/// this issue, not a silent one — the shell caller logs it). Never returns
/// (exits the process).
pub fn handle_merge_method(nwo: &str, requested: Option<&str>) -> Result<()> {
    let ft = detect_forge(None);
    match ft {
        ForgeType::GitHub => std::process::exit(github_merge_method(&gh_bin(), nwo, requested)),
        ForgeType::Gitea => {
            eprintln!(
                "loom-daemon forge merge-method: gitea repo-settings probe is not handled \
                 natively; falling back to the caller's shell auto-detect"
            );
            std::process::exit(EX_FORGE_DECLINED);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    fn flags(squash: bool, merge: bool, rebase: bool) -> RepoMergeFlags {
        RepoMergeFlags {
            allow_squash_merge: squash,
            allow_merge_commit: merge,
            allow_rebase_merge: rebase,
        }
    }

    #[test]
    fn resolve_merge_method_no_request_preserves_squash_first_preference() {
        // Every combination the auto-detect preference order (squash > merge
        // > rebase) has always used — verbatim, no request supplied.
        assert_eq!(resolve_merge_method(None, flags(true, true, true)).unwrap(), "squash");
        assert_eq!(resolve_merge_method(None, flags(false, true, true)).unwrap(), "merge");
        assert_eq!(resolve_merge_method(None, flags(false, false, true)).unwrap(), "rebase");
        // Degenerate case: every strategy reports disabled -- fails open to
        // squash, exactly like the shell predecessor's fail-open branch.
        assert_eq!(resolve_merge_method(None, flags(false, false, false)).unwrap(), "squash");
    }

    #[test]
    fn resolve_merge_method_requested_and_allowed_is_honored() {
        assert_eq!(resolve_merge_method(Some("merge"), flags(true, true, false)).unwrap(), "merge");
        assert_eq!(
            resolve_merge_method(Some("rebase"), flags(true, false, true)).unwrap(),
            "rebase"
        );
        // Even when squash (the auto-detect favorite) is ALSO allowed, an
        // explicit non-squash request still wins -- the whole point of #8845.
        assert_eq!(
            resolve_merge_method(Some("rebase"), flags(true, true, true)).unwrap(),
            "rebase"
        );
    }

    #[test]
    fn resolve_merge_method_requested_and_disallowed_names_the_allowed_set() {
        let err = resolve_merge_method(Some("merge"), flags(true, false, true)).unwrap_err();
        assert!(err.contains("merge"), "error should name the rejected method: {err}");
        assert!(err.contains("squash"), "error should name what IS allowed: {err}");
        assert!(err.contains("rebase"), "error should name what IS allowed: {err}");
        // Never a silent fallback to squash: the Err variant itself is the
        // contract that no method string was resolved.
    }

    #[test]
    fn resolve_merge_method_requested_against_nothing_allowed_says_so() {
        let err = resolve_merge_method(Some("squash"), flags(false, false, false)).unwrap_err();
        assert!(err.contains("none"), "expected 'none' in: {err}");
    }

    #[test]
    fn resolve_merge_method_rejects_unrecognized_vocabulary() {
        let err = resolve_merge_method(Some("fast-forward"), flags(true, true, true)).unwrap_err();
        assert!(err.contains("fast-forward"));
        assert!(err.contains("unrecognized"));
    }

    /// A fake `gh` that answers `api repos/<nwo>` with the given JSON body
    /// (or a nonzero exit, when `body` is `None`).
    fn write_mock_gh_repo(dir: &Path, body: Option<&str>) -> PathBuf {
        let path = dir.join("fake-gh-repo.sh");
        let script = match body {
            Some(b) => format!("#!/bin/sh\ncat <<'EOF'\n{b}\nEOF\nexit 0\n"),
            None => "#!/bin/sh\necho 'gh: not found' 1>&2\nexit 1\n".to_string(),
        };
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    fn github_merge_method_requested_allowed_prints_it_and_exits_zero() {
        let dir = tempdir().unwrap();
        let gh = write_mock_gh_repo(
            dir.path(),
            Some(
                r#"{"allow_squash_merge":true,"allow_merge_commit":true,"allow_rebase_merge":false}"#,
            ),
        );
        let rc = github_merge_method(gh.to_str().unwrap(), "acme/widgets", Some("merge"));
        assert_eq!(rc, 0);
    }

    #[test]
    fn github_merge_method_requested_disallowed_exits_one() {
        let dir = tempdir().unwrap();
        let gh = write_mock_gh_repo(
            dir.path(),
            Some(
                r#"{"allow_squash_merge":true,"allow_merge_commit":false,"allow_rebase_merge":false}"#,
            ),
        );
        let rc = github_merge_method(gh.to_str().unwrap(), "acme/widgets", Some("merge"));
        assert_eq!(rc, 1);
    }

    #[test]
    fn github_merge_method_no_request_auto_detects_unchanged() {
        let dir = tempdir().unwrap();
        let gh = write_mock_gh_repo(
            dir.path(),
            Some(
                r#"{"allow_squash_merge":false,"allow_merge_commit":true,"allow_rebase_merge":true}"#,
            ),
        );
        let rc = github_merge_method(gh.to_str().unwrap(), "acme/widgets", None);
        assert_eq!(rc, 0);
    }

    #[test]
    fn github_merge_method_probe_failure_with_no_request_fails_open_to_squash() {
        let dir = tempdir().unwrap();
        let gh = write_mock_gh_repo(dir.path(), None);
        let rc = github_merge_method(gh.to_str().unwrap(), "acme/widgets", None);
        assert_eq!(rc, 0);
    }

    #[test]
    fn github_merge_method_probe_failure_with_request_refuses_rather_than_fail_open() {
        // The no-request fail-open-to-squash behavior must NOT apply once a
        // caller made an explicit request that could not be verified --
        // silently ignoring it would reproduce the #8845 bug with a request
        // flag that nobody's watching.
        let dir = tempdir().unwrap();
        let gh = write_mock_gh_repo(dir.path(), None);
        let rc = github_merge_method(gh.to_str().unwrap(), "acme/widgets", Some("merge"));
        assert_eq!(rc, 1);
    }
}
