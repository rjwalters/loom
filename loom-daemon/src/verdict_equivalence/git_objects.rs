//! The local-git plumbing [`super::clean_merge`] needs, and nothing else
//! (Issue #9416).
//!
//! Every function here is **fail-closed by return type**: an answer is only
//! ever `Some`/`Tree(..)` when git actually said so. A missing binary, a
//! missing object, a shallow clone, a git too old for
//! `merge-tree --write-tree`, or any non-zero exit collapses into the
//! indeterminate arm, which [`super::detect`] turns into "invalidate as
//! before" — never into an assumed equivalence.
//!
//! # One bounded fetch, and nothing else touches the network (#10134)
//!
//! #9416 shipped this module network-free on the theory that an absent object
//! cost nothing, because [`super::patch_identity`] (which asks the forge)
//! "answers the clean-merge shape too". It does not, whenever the base moved a
//! file the PR also touches: the PR's patch context and resulting blob ids
//! legitimately change, `patch_identity` refutes, and the ONLY kind that can
//! carry the verdict is the clean-merge one — which then answered
//! `Indeterminate` on every host whose clone had not yet seen the brand-new
//! update-branch merge commit. That is exactly the 2am#2150/#2090/#2173/#2121
//! incident (2026-10-03): `loom:pr` stripped from four byte-identical reviewed
//! changes within a minute of `gh pr update-branch`.
//!
//! So [`ensure_commit`] now brings an absent commit in with ONE bounded
//! `git fetch origin -- <sha>` (falling back to the PR's `refs/pull/<n>/head`).
//! It writes objects only — `--no-write-fetch-head`, no refspec destination,
//! no tags — so no ref, no index and no working tree moves. Every other
//! function here stays read-only; the only other write is `merge-tree
//! --write-tree`'s inert loose tree object (see [`merge_tree`]). A fetch that
//! fails or times out is still `Indeterminate` (fail closed), but it now comes
//! back with a reason the caller surfaces instead of swallowing.
//!
//! The git-version floor (`>= 2.38`, where `merge-tree --write-tree` exists)
//! and the exit-code reading (`0` clean, `1` conflict, anything else a
//! failure) are deliberately the same ones
//! [`crate::overlap_replay::conflict`] and
//! [`crate::merge_pr::mergeable_recheck`] already use — this module does not
//! re-derive them differently, it just answers a different question with them
//! (verdict equivalence, not conflict prediction).

use std::path::Path;
use std::process::{Command, Stdio};

/// Run `git -C <repo> <args>` and return its trimmed stdout, or `None` on any
/// failure (absent binary, non-zero exit, non-UTF-8 output).
pub(super) fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

/// Is `repo` inside a git working tree at all? The CLI path inherits the
/// caller's cwd, which is normally a managed worktree but is not guaranteed to
/// be a repo at all.
pub(super) fn is_git_repo(repo: &Path) -> bool {
    git(repo, &["rev-parse", "--git-dir"]).is_some()
}

/// Is `repo` a shallow clone? `None` when git could not be asked — treated by
/// callers exactly like `Some(true)`, because a clone whose depth is unknown
/// cannot be trusted to hold the merge-base history `merge-tree` needs.
pub(super) fn is_shallow(repo: &Path) -> Option<bool> {
    match git(repo, &["rev-parse", "--is-shallow-repository"])?.as_str() {
        "false" => Some(false),
        "true" => Some(true),
        // `--is-shallow-repository` predates every git that has
        // `merge-tree --write-tree`, so an unrecognized answer means something
        // else is wrong. Unknown depth is not "full".
        _ => None,
    }
}

/// Is `sha` present locally AS A COMMIT? `cat-file -e <sha>^{commit}` is the
/// narrow question: a SHA that resolves to a blob or tag, or to nothing at
/// all, answers `false`.
pub(super) fn has_commit(repo: &Path, sha: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Hang ceiling for [`ensure_commit`]'s fetch. A fetch of one PR head into an
/// up-to-date clone takes well under a second; this bounds a wedged remote.
pub(super) const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Make sure `sha` is present locally as a commit, fetching it from `origin`
/// when it is not (Issue #10134). `Ok(())` once it is present; `Err(reason)`
/// — redacted, one line, for a log or a PR comment — when it could not be
/// brought in.
///
/// Tried in order: `git fetch origin -- <sha>` (GitHub serves any reachable commit
/// by id), then `refs/pull/<pr>/head` when the PR number is known (a commit the
/// forge only reaches through the PR ref). Both fetch objects only — no ref,
/// no `FETCH_HEAD`, no tags — and both run with prompting disabled and stdin
/// nulled under [`FETCH_TIMEOUT`], so a credential prompt can never wedge the
/// daemon pass.
pub(super) fn ensure_commit(repo: &Path, pr: Option<u32>, sha: &str) -> Result<(), String> {
    if has_commit(repo, sha) {
        return Ok(());
    }
    let mut specs = vec![sha.to_string()];
    if let Some(pr) = pr {
        specs.push(format!("refs/pull/{pr}/head"));
    }
    let mut failures = Vec::with_capacity(specs.len());
    for spec in &specs {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .args([
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "origin",
                "--",
                spec,
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null());
        let outcome = crate::cmd_out::run_command(cmd, FETCH_TIMEOUT);
        if outcome.succeeded() && has_commit(repo, sha) {
            return Ok(());
        }
        failures.push(if outcome.succeeded() {
            format!("`git fetch origin -- {spec}` succeeded but {sha} is still absent")
        } else {
            outcome.failure_reason(&format!("git fetch origin -- {spec}"))
        });
    }
    Err(redact(&failures.join("; ")))
}

/// Make a git diagnostic safe and compact enough for a log line or a public PR
/// comment: credentials embedded in a URL (`https://user:token@host`) are
/// masked, whitespace is collapsed to one line, and the result is capped.
pub(super) fn redact(s: &str) -> String {
    const CAP: usize = 400;
    let words: Vec<String> = s
        .split_whitespace()
        .map(|w| {
            let Some(scheme) = w.find("://") else {
                return w.to_string();
            };
            let rest = &w[scheme + 3..];
            let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
            match authority.rfind('@') {
                Some(at) => format!("{}://***{}", &w[..scheme], &rest[at..]),
                None => w.to_string(),
            }
        })
        .collect();
    let line = words.join(" ");
    if line.chars().count() <= CAP {
        line
    } else {
        format!("{}…", line.chars().take(CAP).collect::<String>())
    }
}

/// `git rev-parse --verify <rev>` — resolve a rev to a full 40-hex OID.
pub(super) fn rev_parse(repo: &Path, rev: &str) -> Option<String> {
    let oid = git(repo, &["rev-parse", "--verify", "--quiet", rev])?;
    if oid.len() == 40 && oid.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(oid)
    } else {
        None
    }
}

/// The parent OIDs of `sha`, in order. An empty vector (a root commit) is a
/// legitimate `Some`; `None` means git could not be asked.
pub(super) fn parents(repo: &Path, sha: &str) -> Option<Vec<String>> {
    let line = git(repo, &["rev-list", "--parents", "-n", "1", sha])?;
    let mut fields = line.split_whitespace();
    // First field is the commit itself.
    fields.next()?;
    Some(fields.map(str::to_string).collect())
}

/// git >= 2.38, where `git merge-tree --write-tree` exists. Same floor and
/// same probe shape as [`crate::overlap_replay::conflict`].
pub(super) fn supports_merge_tree(repo: &Path) -> bool {
    let Some(out) = git(repo, &["--version"]) else {
        return false;
    };
    out.split_whitespace()
        .nth(2)
        .is_some_and(merge_tree_in_version)
}

pub(super) fn merge_tree_in_version(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(Ok(major)) = parts.next().map(str::parse::<u32>) else {
        return false;
    };
    let minor = parts
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    major > 2 || (major == 2 && minor >= 38)
}

/// What a `git merge-tree --write-tree` run produced.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MergeTree {
    /// A clean automatic merge whose result tree is this OID.
    Tree(String),
    /// Textual conflicts: there IS no automatic merge, so the head in front of
    /// us cannot be one. Positive evidence, not an error.
    Conflict,
    /// Could not be computed (git too old, missing object, unparsable output,
    /// any other exit status).
    Failed,
}

/// `git merge-tree --write-tree <a> <b>` — the automatic three-way merge of
/// two commits, with git computing their merge base itself (exactly the check
/// Issue #9416's acceptance criterion names).
///
/// Writes the result tree into `repo`'s object store as a loose object and
/// prints its OID. That write is inert: no ref points at it, nothing reads it
/// but this function, and ordinary gc reclaims it — the index and working tree
/// are untouched (`merge-tree` is plumbing).
pub(super) fn merge_tree(repo: &Path, a: &str, b: &str) -> MergeTree {
    if !supports_merge_tree(repo) {
        return MergeTree::Failed;
    }
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-tree", "--write-tree", a, b])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    else {
        return MergeTree::Failed;
    };
    match out.status.code() {
        Some(0) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let Some(oid) = stdout.lines().next().map(str::trim) else {
                return MergeTree::Failed;
            };
            if oid.len() == 40 && oid.chars().all(|c| c.is_ascii_hexdigit()) {
                MergeTree::Tree(oid.to_string())
            } else {
                MergeTree::Failed
            }
        }
        Some(1) => MergeTree::Conflict,
        _ => MergeTree::Failed,
    }
}
