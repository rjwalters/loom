//! `loom-daemon forge check-branch <issue>` — the #9447 branch-collision
//! hard-stop probe (issue #9453 Phase 4, requirement 2 of the original
//! design).
//!
//! # Why this exists
//!
//! On 2026-09-29, issue #9447's incident showed the failure this command
//! closes: a second worker pushed `feature/issue-9447` and opened PR #9450;
//! the first worker then detected the branch already existed on remote and
//! **fell back to a suffix branch** (`feature/issue-9447-install-merge`),
//! opening a second, competing PR (#9451) rather than stopping. Nothing in
//! the Builder flow checked whether the branch it was about to push already
//! existed — the suffix fallback was ad-hoc agent improvisation, not scripted
//! behavior. This subcommand is the missing probe: a cheap, pre-push question
//! ("does `feature/issue-N` already exist on `origin`?") that a caller wires
//! in immediately before its own first `git push -u origin feature/issue-N`
//! so the answer is always "no, because I have not pushed it yet" unless a
//! peer got there first — in which case the caller must hard-abort, never
//! rename-and-push.
//!
//! # One leg, shared with `forge check-claim`
//!
//! This is the same `git ls-remote --heads origin feature/issue-N` leg
//! [`crate::forge_check_claim`]'s aggregated probe already runs as its leg 4
//! (`BranchLeg` there) — exposed standalone so a caller that only needs the
//! branch answer (the pre-push fence, not a full pre-claim probe) can ask for
//! it without paying `check-claim`'s other three legs' forge reads. Zero
//! forge-API calls either way: `git ls-remote` is the git wire protocol, not
//! `gh`/GraphQL/REST, so this works identically on GitHub and Gitea and needs
//! no token beyond whatever `origin` is already configured with.
//!
//! # Exit-code contract (the whole public surface)
//!
//! | Exit | Meaning | stdout | What the caller must do |
//! |---|---|---|---|
//! | `0` | Verified: `feature/issue-N` exists on `origin` | the branch's last-commit timestamp (`%cI`) when locally reachable, else the tip SHA | **`BRANCH_COLLISION`.** Hard-abort before push. Never create a suffix branch. |
//! | [`EX_BRANCH_ABSENT`] (1) | Verified: no such branch on `origin` | empty | Safe to push. |
//! | [`EX_PROBE_FAILED`] (5) | No verdict — `git ls-remote` itself failed (no network, no `origin` remote, no repo, a transport error) | empty | **Fail closed.** Not a verified absence; check by hand before pushing. |
//!
//! `0` is "found" rather than "all clear", mirroring `forge check-open-pr` and
//! `forge check-claim`'s convention: it makes the *unsafe* state the one a
//! bare `if` fires on, and every non-zero code that is not exactly `1` means
//! **the question was not answered**, which a caller must treat as "the
//! branch might already exist" — never as permission to push.
//!
//! # No commit-date fetch
//!
//! The stdout timestamp on a `0` verdict is **best-effort**: it comes from a
//! second, purely local `git log -1 --format=%cI <sha>` against whatever
//! objects this worktree already has (typically populated by an earlier
//! `git fetch origin` in the same Builder run, e.g. the pre-push rebase
//! step). It never performs a `git fetch` of its own — that would turn a
//! zero-network-round-trip existence check into a variable-cost one, and the
//! exit-code contract above does not depend on it: when the commit is not
//! locally reachable, stdout falls back to the tip SHA `ls-remote` already
//! returned, and the verdict is unchanged.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Exit code for a **verified** "branch absent" — the only safe-to-push
/// answer. Deliberately the same value [`crate::forge_check_open_pr`]'s
/// verified absence and [`crate::forge_check_claim`]'s verified safe-to-claim
/// use, so a caller migrating between them keeps its `if` arms.
pub const EX_BRANCH_ABSENT: i32 = 1;

/// Exit code for "the probe could not produce a verdict" — fail CLOSED. Same
/// value and meaning as `crate::forge_check_open_pr::EX_PROBE_FAILED` /
/// `crate::forge_check_claim::EX_PROBE_FAILED`.
pub const EX_PROBE_FAILED: i32 = 5;

/// The pure classification of one `git ls-remote --heads origin
/// feature/issue-N` result (before the best-effort commit-date lookup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LsRemote {
    /// A matching ref was reported; carries the tip SHA.
    Exists(String),
    /// `ls-remote` answered with no matching ref.
    Absent,
    /// The `ls-remote` invocation itself failed — NOT a verified absence.
    Failed,
}

/// One `forge check-branch` probe's full evidence: existence plus, when it
/// exists, the best-effort commit timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchProbe {
    /// The branch exists on `origin`. `committed_at` is `None` when the tip
    /// commit is not locally reachable without a fetch this probe never
    /// performs (see the module doc).
    Exists {
        sha: String,
        committed_at: Option<String>,
    },
    /// Verified: no matching ref on `origin`.
    Absent,
    /// The `git ls-remote` call itself failed — NOT a verified absence.
    ProbeFailed,
}

/// What the CLI prints and exits with for one probe verdict. Split out from
/// [`handle`] so the contract is unit-testable without a live `git` — the
/// same seam `crate::forge_check_open_pr::Verdict` /
/// `crate::forge_check_claim::Verdict` provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Process exit status.
    pub code: i32,
    /// Machine-readable stdout (a timestamp, a SHA, or empty).
    pub stdout: String,
    /// Human-readable explanation, always non-empty.
    pub stderr: String,
}

/// Render a [`BranchProbe`] into its [`Verdict`].
///
/// The wording is load-bearing, not decoration, exactly as it is in
/// `forge_check_open_pr::verdict`: a `ProbeFailed` message must never read as
/// an absence, and an `Exists` message must never read as safe — the failure
/// mode this command exists to prevent is precisely a caller concluding "the
/// branch is free" from a question that was never answered, or worse,
/// falling back to a suffix branch on an *answered* collision.
#[must_use]
pub fn verdict(issue: u32, probe: BranchProbe) -> Verdict {
    let branch = format!("feature/issue-{issue}");
    match probe {
        BranchProbe::Exists { sha, committed_at } => {
            let stdout = committed_at.clone().unwrap_or_else(|| sha.clone());
            let date_clause = committed_at
                .as_ref()
                .map(|t| format!(", last commit at {t}"))
                .unwrap_or_default();
            Verdict {
                code: 0,
                stdout,
                stderr: format!(
                    "BRANCH_COLLISION: remote branch {branch} already exists on origin \
                     (tip {sha}{date_clause}) — a prior or racing claimant pushed it. \
                     Hard-abort before push. Never create a suffix branch past it (#9447); \
                     if it is this claim's own branch, adopt it via create-pr.sh's adopt-first \
                     path instead of pushing a new one."
                ),
            }
        }
        BranchProbe::Absent => Verdict {
            code: EX_BRANCH_ABSENT,
            stdout: String::new(),
            stderr: format!("remote branch {branch} does not exist on origin — safe to push."),
        },
        BranchProbe::ProbeFailed => Verdict {
            code: EX_PROBE_FAILED,
            stdout: String::new(),
            stderr: format!(
                "could not determine whether {branch} exists on origin (git ls-remote failed: \
                 no network, no origin remote, or a transport error). This is NOT a verified \
                 absence — treat the branch as possibly already existing and check by hand \
                 before pushing."
            ),
        },
    }
}

/// Classify one `git ls-remote --heads origin feature/issue-N` result. Pure.
#[must_use]
pub(crate) fn classify_ls_remote(success: bool, stdout: &str) -> LsRemote {
    if !success {
        return LsRemote::Failed;
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return LsRemote::Absent;
    }
    // `git ls-remote` prints "<sha>\t<ref>" per line; the SHA is whatever
    // precedes the first run of whitespace on the first matching line.
    match trimmed
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
    {
        Some(sha) if !sha.is_empty() => LsRemote::Exists(sha.to_string()),
        _ => LsRemote::Absent,
    }
}

/// Best-effort commit-committer-date lookup (`git log -1 --format=%cI <sha>`)
/// against whatever objects are already locally reachable. Deliberately never
/// fetches — see the module doc's "No commit-date fetch" section. `None` on
/// any failure (missing object, non-zero exit, unparseable/empty output) is
/// not itself a probe failure; the caller already has a verified `Exists`
/// answer and simply falls back to the SHA on stdout.
fn read_commit_timestamp(git_bin: &Path, root: &Path, sha: &str) -> Option<String> {
    let mut cmd = Command::new(git_bin);
    cmd.arg("log")
        .arg("-1")
        .arg("--format=%cI")
        .arg(sha)
        .current_dir(root);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The probe: `git ls-remote --heads origin feature/issue-N`, then, only on a
/// verified `Exists`, the best-effort local commit-date lookup above.
pub(crate) fn probe_remote_branch(git_bin: &Path, root: &Path, issue: u32) -> BranchProbe {
    let branch = format!("feature/issue-{issue}");
    let mut cmd = Command::new(git_bin);
    cmd.arg("ls-remote")
        .arg("--heads")
        .arg("origin")
        .arg(&branch)
        .current_dir(root);
    let out = match cmd.output() {
        Ok(out) => out,
        Err(_) => return BranchProbe::ProbeFailed,
    };
    match classify_ls_remote(out.status.success(), &String::from_utf8_lossy(&out.stdout)) {
        LsRemote::Failed => BranchProbe::ProbeFailed,
        LsRemote::Absent => BranchProbe::Absent,
        LsRemote::Exists(sha) => {
            let committed_at = read_commit_timestamp(git_bin, root, &sha);
            BranchProbe::Exists { sha, committed_at }
        }
    }
}

/// Handle `loom-daemon forge check-branch <issue>`. Never returns (exits the
/// process with the code from [`verdict`]); returns `Err` only when the
/// current directory cannot be resolved.
///
/// Resolution is cwd-scoped, like `forge check-open-pr` / `forge
/// check-claim`: run it from inside the repository (a managed worktree
/// included) whose `feature/issue-N` you are about to push. `LOOM_GIT_BIN`
/// overrides the `git` binary invoked (test seam), mirroring
/// `LOOM_GH_BIN`'s role for the `gh`-backed probes.
pub fn handle(issue: u32) -> Result<()> {
    let root: PathBuf = std::env::current_dir().context(
        "loom-daemon forge check-branch: could not resolve the current directory; run it from \
         inside the repository whose feature/issue-N branch you are about to push",
    )?;
    let git_bin = PathBuf::from(std::env::var("LOOM_GIT_BIN").unwrap_or_else(|_| "git".into()));

    let v = verdict(issue, probe_remote_branch(&git_bin, &root, issue));
    if !v.stdout.is_empty() {
        println!("{}", v.stdout);
    }
    eprintln!("{}", v.stderr);
    std::process::exit(v.code);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // --- Pure verdict rendering ----------------------------------------------

    #[test]
    fn branch_exists_is_exit_zero_and_prints_the_timestamp_when_known() {
        let v = verdict(
            9447,
            BranchProbe::Exists {
                sha: "aef1c2d".to_string(),
                committed_at: Some("2026-09-29T03:48:22+00:00".to_string()),
            },
        );
        assert_eq!(v.code, 0);
        assert_eq!(v.stdout, "2026-09-29T03:48:22+00:00");
        assert!(v.stderr.contains("BRANCH_COLLISION"), "{}", v.stderr);
        assert!(v.stderr.contains("Hard-abort"), "{}", v.stderr);
        assert!(v.stderr.contains("Never create a suffix branch"), "{}", v.stderr);
    }

    #[test]
    fn branch_exists_falls_back_to_sha_when_timestamp_unknown() {
        let v = verdict(
            9447,
            BranchProbe::Exists {
                sha: "aef1c2d".to_string(),
                committed_at: None,
            },
        );
        assert_eq!(v.code, 0);
        assert_eq!(v.stdout, "aef1c2d");
        assert!(v.stderr.contains("BRANCH_COLLISION"), "{}", v.stderr);
    }

    #[test]
    fn verified_absence_is_exit_one_with_empty_stdout() {
        let v = verdict(42, BranchProbe::Absent);
        assert_eq!(v.code, EX_BRANCH_ABSENT);
        assert!(v.stdout.is_empty(), "{:?}", v.stdout);
        assert!(v.stderr.contains("safe to push"), "{}", v.stderr);
    }

    #[test]
    fn probe_failure_fails_closed_and_is_distinguishable_from_absence() {
        let v = verdict(42, BranchProbe::ProbeFailed);
        assert_eq!(v.code, EX_PROBE_FAILED);
        assert_ne!(
            v.code, EX_BRANCH_ABSENT,
            "a probe failure must not share the verified-absence exit code"
        );
        assert!(v.stdout.is_empty(), "{:?}", v.stdout);
        assert!(
            !v.stderr.contains("safe to push"),
            "a failed probe must never read as an all-clear: {}",
            v.stderr
        );
        assert!(v.stderr.contains("NOT a verified absence"), "{}", v.stderr);
    }

    /// The three verdicts must occupy three distinct exit codes, and none may
    /// collide with the other `forge` surface codes a caller may also see.
    #[test]
    fn exit_codes_are_mutually_distinct() {
        let codes = [
            verdict(
                1,
                BranchProbe::Exists {
                    sha: "x".to_string(),
                    committed_at: None,
                },
            )
            .code,
            verdict(1, BranchProbe::Absent).code,
            verdict(1, BranchProbe::ProbeFailed).code,
        ];
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "{codes:?}");
        assert!(!codes.contains(&crate::forge_cmd::EX_FORGE_DECLINED), "{codes:?}");
        assert!(!codes.contains(&crate::forge_cmd::EX_FORGE_HEAD_MISMATCH), "{codes:?}");
    }

    // --- classify_ls_remote (pure) --------------------------------------------

    #[test]
    fn classify_ls_remote_distinguishes_exists_absent_and_failed() {
        assert_eq!(
            classify_ls_remote(true, "aef1c2d\trefs/heads/feature/issue-42\n"),
            LsRemote::Exists("aef1c2d".to_string())
        );
        assert_eq!(classify_ls_remote(true, "\n"), LsRemote::Absent);
        assert_eq!(classify_ls_remote(true, ""), LsRemote::Absent);
        assert_eq!(classify_ls_remote(false, ""), LsRemote::Failed);
        // A non-zero exit with some stray stdout is still a failure, never a
        // verified `Exists` — success is load-bearing, not just non-empty
        // output.
        assert_eq!(
            classify_ls_remote(false, "aef1c2d\trefs/heads/feature/issue-42\n"),
            LsRemote::Failed
        );
    }

    // --- IO leg readers against a fake git (no network) -----------------------

    use tempfile::tempdir;

    fn write_exec(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/usr/bin/env bash\n{body}")).unwrap();
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
    fn probe_remote_branch_reports_absent_when_ls_remote_prints_nothing() {
        let dir = tempdir().unwrap();
        let git = write_exec(dir.path(), "git-absent.sh", "true");
        assert_eq!(probe_remote_branch(&git, dir.path(), 42), BranchProbe::Absent);
    }

    #[test]
    fn probe_remote_branch_fails_closed_when_ls_remote_errors() {
        let dir = tempdir().unwrap();
        let git = write_exec(dir.path(), "git-fail.sh", "echo 'no network' >&2\nexit 128");
        assert_eq!(probe_remote_branch(&git, dir.path(), 42), BranchProbe::ProbeFailed);
    }

    #[test]
    fn probe_remote_branch_fails_closed_when_the_binary_cannot_be_spawned() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert_eq!(probe_remote_branch(&missing, dir.path(), 42), BranchProbe::ProbeFailed);
    }

    #[test]
    fn probe_remote_branch_reports_exists_with_timestamp_when_the_commit_is_locally_reachable() {
        let dir = tempdir().unwrap();
        let git = write_exec(
            dir.path(),
            "git-exists-with-log.sh",
            r#"case "$1" in
  ls-remote) echo "aef1c2d	refs/heads/feature/issue-42" ;;
  log) echo "2026-09-29T03:48:22+00:00" ;;
esac"#,
        );
        assert_eq!(
            probe_remote_branch(&git, dir.path(), 42),
            BranchProbe::Exists {
                sha: "aef1c2d".to_string(),
                committed_at: Some("2026-09-29T03:48:22+00:00".to_string()),
            }
        );
    }

    #[test]
    fn probe_remote_branch_reports_exists_without_timestamp_when_the_commit_is_not_reachable() {
        let dir = tempdir().unwrap();
        let git = write_exec(
            dir.path(),
            "git-exists-no-log.sh",
            r#"case "$1" in
  ls-remote) echo "aef1c2d	refs/heads/feature/issue-42" ;;
  log) echo 'fatal: bad object aef1c2d' >&2; exit 128 ;;
esac"#,
        );
        assert_eq!(
            probe_remote_branch(&git, dir.path(), 42),
            BranchProbe::Exists {
                sha: "aef1c2d".to_string(),
                committed_at: None,
            }
        );
    }
}
