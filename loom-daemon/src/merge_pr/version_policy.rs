//! The pre-merge version-policy guard (#7827, oracle choice #8284) — a slice
//! of the `merge-pr.sh` port, #8191.
//!
//! # What it guards
//!
//! Feature PRs must not hand-edit a version-bearing value: the post-merge
//! workflow owns every bump (#7743), and CI's `defaults-version-bump-check` job
//! fails any PR that does. This guard is the merge-time twin of that job. It
//! runs the SAME canonical checker (`check-defaults-version-bump.sh
//! --forbid-bump`) against the merge base, so a concurrent automated bump on
//! the base branch is never mistaken for an edit authored by this PR.
//!
//! The checker itself stays a shell script — it is CI's contract too, and the
//! whole point of this guard is to evaluate exactly what CI evaluates. What
//! moved here is everything `merge-pr.sh` wrapped around it: the fetch, the
//! ancestry test, the choice of WHICH ref's checker is the oracle (#8284), the
//! temp-file extraction of the PR head's copy, and the classification of the
//! checker's exit code into pass / skip / block.
//!
//! # Which ref's checker is the oracle (#8284)
//!
//! Normally the operator checkout's copy — the default branch's. That is the
//! wrong oracle for a PR whose purpose is to change the version-bearing SET:
//! the default branch's copy still encodes the old set, so such a PR can never
//! pass. PR #8190 was blocked exactly that way while CI (which runs the
//! checker from the PR head) passed it. So when this PR's OWN commits
//! (`merge-base..head`, never base-branch drift) touch the three files that
//! define "version-bearing" ([`MACHINERY`]), the PR head's checker is
//! extracted and evaluated instead. A head lookup that fails falls BACK to the
//! default branch's copy — a lookup error must never become a free pass.
//!
//! # The best-effort contract, preserved
//!
//! Only a CONFIRMED forbidden version edit (checker exit 1) blocks. Every
//! guard-internal fault — no checker on disk, no default branch, a failed
//! fetch, an unresolvable head, unknown ancestry, a checker exit other than
//! 0/1 — skips, some silently and some with a warning, exactly as the retired
//! shell did. `tests/merge_pr_version_policy_differential.rs` pins that
//! against a frozen copy of the retired function on real repositories.
//!
//! # One deliberate improvement over the retired shell
//!
//! The retired function wrote the PR head's checker to a `mktemp` file and
//! removed it only on its happy paths; an `error` exit between the two left it
//! behind in `$TMPDIR`. Here the extracted checker is a [`tempfile::TempPath`],
//! removed on drop however the evaluation ends. No output changes.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// The canonical checker, relative to the repository root.
pub const CHECKER_REL: &str = "defaults/scripts/check-defaults-version-bump.sh";

/// The files that define what "version-bearing" means. A PR whose own
/// commits touch any of them is judged by its own head's checker (#8284).
pub const MACHINERY: &[&str] = &[
    CHECKER_REL,
    "defaults/scripts/version-check-gate.sh",
    "scripts/version.sh",
];

/// Emitted when the PR's ancestry against the base cannot be established. The
/// checker's shallow-history fallback compares raw tips, which cannot say WHO
/// changed a version, so this skips rather than label anything a confirmed
/// edit.
pub const ANCESTRY_WARNING: &str =
    "Version policy guard: PR ancestry unavailable; skipping unverified comparison.";

/// Everything the retired function read from `merge-pr.sh`'s globals.
pub struct Inputs<'a> {
    /// `$REPO_ROOT` — the checkout whose on-disk checker is the default oracle.
    pub repo_root: &'a Path,
    /// `$DEFAULT_BRANCH_NAME`; empty = unknown, which skips.
    pub default_branch: &'a str,
    /// `$PR_BRANCH`; empty skips.
    pub branch: &'a str,
    /// `$PR_HEAD_SHA`; empty skips.
    pub head_sha: &'a str,
    /// `$PR_NUMBER`, for the operator-facing text only.
    pub pr_number: &'a str,
    /// `--dry-run`: report the would-be block, never refuse.
    pub dry_run: bool,
}

/// The guard's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Merge may proceed (clean, skipped, or a dry-run report).
    Pass,
    /// A confirmed forbidden version edit. Carries the full refusal text.
    Block(String),
}

/// Warnings to print, in order, and the verdict that follows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub warnings: Vec<String>,
    pub verdict: Verdict,
}

impl Report {
    fn pass(warnings: Vec<String>) -> Self {
        Report {
            warnings,
            verdict: Verdict::Pass,
        }
    }
}

/// `'main' (<sha>)` — how the default branch's checker is named in output.
pub fn default_checker_ref(default_branch: &str, base_sha: &str) -> String {
    format!("'{default_branch}' ({base_sha})")
}

/// `the PR head (<sha>)` — how the PR head's checker is named in output.
pub fn head_checker_ref(head_sha: &str) -> String {
    format!("the PR head ({head_sha})")
}

/// The warning printed whenever the PR touches [`MACHINERY`], naming the
/// oracle actually used. The doubled quote in `'main''s` is the retired text,
/// kept byte-for-byte so an operator's grep and the differential both hold.
pub fn machinery_warning(checker_ref: &str, default_branch: &str) -> String {
    format!(
        "Version policy guard: this PR's own commits change the version-policy machinery, so the guard evaluates the checker from {checker_ref} — the ref CI's defaults-version-bump-check job evaluates (#8284). A head lookup that fails falls back to '{default_branch}''s copy, never to skipping the check."
    )
}

/// The skip warning for a checker exit other than 0 or 1 (bad usage, an
/// unresolved ref): a guard fault, not a confirmed edit.
pub fn checker_fault_warning(
    checker_ref: &str,
    rc: i32,
    default_branch: &str,
    base_sha: &str,
    output: &str,
) -> String {
    format!(
        "Version policy guard: check-defaults-version-bump.sh (from {checker_ref}) exited {rc} against current '{default_branch}' ({base_sha}) — skipping (not a confirmed version edit):\n{output}"
    )
}

/// The refusal text for a confirmed forbidden version edit.
pub fn block_message(pr_number: &str, output: &str, checker_ref: &str) -> String {
    format!(
        "Merge blocked: PR #{pr_number} hand-edits a version-bearing value (#7827).\n\n{output}\n\nRevert the version-value changes authored by this PR, preserving its other changes,\nthen rerun CI and review. Version bumps are applied automatically by the merge workflow\n(#7743); a no-surface-change marker cannot waive this policy. (Checker from {checker_ref}.)"
    )
}

/// What `--dry-run` reports in place of [`block_message`].
pub fn dry_run_message(
    pr_number: &str,
    default_branch: &str,
    base_sha: &str,
    checker_ref: &str,
) -> String {
    format!(
        "[dry-run] Would BLOCK merge of PR #{pr_number}: forbidden version edit relative to '{default_branch}' ({base_sha}), per the checker from {checker_ref}."
    )
}

/// Classify the checker's exit code into a report. Pure, so every arm is unit
/// testable without a repository.
pub fn classify(
    inputs: &Inputs<'_>,
    mut warnings: Vec<String>,
    base_sha: &str,
    checker_ref: &str,
    rc: i32,
    output: &str,
) -> Report {
    match rc {
        0 => Report::pass(warnings),
        1 if inputs.dry_run => {
            warnings.push(dry_run_message(
                inputs.pr_number,
                inputs.default_branch,
                base_sha,
                checker_ref,
            ));
            Report::pass(warnings)
        }
        1 => Report {
            warnings,
            verdict: Verdict::Block(block_message(inputs.pr_number, output, checker_ref)),
        },
        _ => {
            warnings.push(checker_fault_warning(
                checker_ref,
                rc,
                inputs.default_branch,
                base_sha,
                output,
            ));
            Report::pass(warnings)
        }
    }
}

/// Run `git -C <repo> <args>`, returning stdout on success. stdin is closed
/// and nothing is inherited, so no child can write into the caller's
/// protocol stream.
fn git(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// `$(git …)` — stdout with trailing newlines stripped, `None` on failure.
fn git_str(repo: &Path, args: &[&str]) -> Option<String> {
    git(repo, args).map(|b| strip_trailing_newlines(&String::from_utf8_lossy(&b)))
}

/// Bash command substitution strips every trailing newline, and the retired
/// guard interpolated the checker's output that way.
fn strip_trailing_newlines(s: &str) -> String {
    s.trim_end_matches('\n').to_string()
}

fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Write the PR head's checker to a private executable temp file. `None`
/// (fall back to the default branch's copy) when the blob is missing, empty,
/// or cannot be written — the retired `git show … && [[ -s ]] && chmod +x`.
fn extract_head_checker(repo: &Path, head_sha: &str) -> Option<tempfile::TempPath> {
    let blob = git(repo, &["show", &format!("{head_sha}:{CHECKER_REL}")])?;
    if blob.is_empty() {
        return None;
    }
    let mut f = tempfile::Builder::new()
        .prefix("loom-version-policy-checker.")
        .tempfile()
        .ok()?;
    f.write_all(&blob).ok()?;
    f.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o755))
        .ok()?;
    // Closing the handle before exec matters on Linux: executing a file some
    // process still holds open for writing fails with ETXTBSY.
    Some(f.into_temp_path())
}

/// Run the checker from `repo` with stdout and stderr interleaved into one
/// stream (the retired `2>&1`), returning `(exit code, output)` with bash's
/// conventions: a signal death is `128 + signo`, and a checker that cannot be
/// executed at all is 126 — both non-1, so both skip rather than block.
fn run_checker(checker: &Path, repo: &Path, base_sha: &str, head_sha: &str) -> (i32, String) {
    let Ok((mut reader, writer)) = std::io::pipe() else {
        return (126, String::new());
    };
    let Ok(writer_err) = writer.try_clone() else {
        return (126, String::new());
    };
    let spawned = {
        // Scoped so the Command — which holds the pipe's write ends — is
        // dropped before reading; otherwise the read never sees EOF.
        let mut cmd = Command::new(checker);
        cmd.current_dir(repo)
            .args(["--forbid-bump", "--base", base_sha, "--head", head_sha])
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(writer_err);
        cmd.spawn()
    };
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return (126, e.to_string()),
    };
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    let rc = match child.wait() {
        Ok(status) => status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        }),
        Err(_) => 126,
    };
    (rc, strip_trailing_newlines(&String::from_utf8_lossy(&buf)))
}

/// Evaluate the guard against a real repository.
pub fn evaluate(inputs: &Inputs<'_>) -> Report {
    let repo = inputs.repo_root;
    let local_checker = repo.join(CHECKER_REL);
    if !is_executable_file(&local_checker)
        || inputs.default_branch.is_empty()
        || inputs.head_sha.is_empty()
        || inputs.branch.is_empty()
    {
        return Report::pass(Vec::new());
    }

    // A failed fetch means nothing fresher than what is local can be seen:
    // skip rather than block on stale or missing data.
    if git(
        repo,
        &[
            "fetch",
            "--quiet",
            "origin",
            inputs.default_branch,
            inputs.branch,
        ],
    )
    .is_none()
    {
        return Report::pass(Vec::new());
    }

    let base_sha = match git_str(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("origin/{}", inputs.default_branch),
        ],
    ) {
        Some(s) if !s.is_empty() => s,
        _ => return Report::pass(Vec::new()),
    };

    // A fork PR or already-deleted branch can leave the head unresolvable.
    if git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{}^{{commit}}", inputs.head_sha),
        ],
    )
    .is_none()
    {
        return Report::pass(Vec::new());
    }

    let Some(merge_base) = git_str(repo, &["merge-base", &base_sha, inputs.head_sha]) else {
        return Report::pass(vec![ANCESTRY_WARNING.to_string()]);
    };

    let mut warnings = Vec::new();
    let mut checker_ref = default_checker_ref(inputs.default_branch, &base_sha);
    let mut head_checker: Option<tempfile::TempPath> = None;

    let mut diff_args = vec![
        "diff",
        "--name-only",
        merge_base.as_str(),
        inputs.head_sha,
        "--",
    ];
    diff_args.extend_from_slice(MACHINERY);
    let touches_machinery = git_str(repo, &diff_args).is_some_and(|s| !s.is_empty());
    if touches_machinery {
        head_checker = extract_head_checker(repo, inputs.head_sha);
        if head_checker.is_some() {
            checker_ref = head_checker_ref(inputs.head_sha);
        }
        warnings.push(machinery_warning(&checker_ref, inputs.default_branch));
    }

    let checker: &Path = head_checker.as_deref().unwrap_or(&local_checker);
    let (rc, output) = run_checker(checker, repo, &base_sha, inputs.head_sha);
    drop(head_checker);

    classify(inputs, warnings, &base_sha, &checker_ref, rc, &output)
}

#[cfg(test)]
mod tests;
