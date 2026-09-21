//! The no-op re-date-commit remedy for the required-check freshness guard
//! (#8248, automated by #8508).
//!
//! # Why this exists
//!
//! #8248's guard is correct: a green required check whose evidence predates
//! the base branch's current tip must not be trusted. But nothing in the
//! fleet automatically produces FRESH evidence once the guard fires. On a
//! busy `main`, PR #8493 failed three consecutive Champion merge attempts
//! (2026-09-21) with the identical refusal, because:
//!
//! - the PR's branch had no new commits, so no CI run ever re-dated its
//!   checks;
//! - `main` kept advancing, so every retry compared against an even later
//!   base tip;
//! - both an internal check re-run and a direct `gh run rerun` failed with
//!   `Resource not accessible by integration` — the installation token has
//!   no `actions:write`.
//!
//! The only escapes on record were a human merging with an elevated token, or
//! a human pushing a no-op commit by hand. Neither happens automatically, so
//! a Judge-approved, safety-criteria-clean PR could sit blocked forever with
//! no visible record once Champion's own idempotency guard suppressed the
//! third near-identical failure comment.
//!
//! # The remedy
//!
//! `actions:write` (re-running a workflow) is not the only way to produce a
//! fresh check run: pushing ANY commit re-triggers every `pull_request`
//! workflow on the new head, which is exactly `stale_checks::stale_message`'s
//! own documented remedy ("push any no-op commit to re-date every check").
//! `merge-pr.sh` already exercises `contents: write` on the head branch —
//! `head_sync`'s self-sync commit lands there via the same token — so this
//! needs no new grant.
//!
//! This module creates a new commit that repoints at the SAME tree as the
//! current head (so the diff Judge reviewed is byte-for-byte unchanged) with
//! ONE new parent, and fast-forwards the branch ref onto it via the Git Data
//! API (`git/commits` + `git/refs/heads/<branch>`) rather than a local clone —
//! `merge-pr.sh`'s own "worktree-safe, API-only" discipline.
//!
//! # Concurrency
//!
//! The caller supplies `expected_head_sha` — the SHA the blocked merge
//! attempt actually gated on. Before writing anything, this module re-reads
//! the branch's current ref and refuses to proceed if it has already moved:
//! a foreign push already changed the tree (Judge's approval is stale
//! regardless, per #5686) and a second, unrelated no-op commit on top would
//! only add noise. [`RedateOutcome::HeadMoved`] mirrors `merge-pr.sh`'s own
//! #5579 exit-3 contract ("not a failure, re-queue and re-evaluate fresh").
//!
//! # What this deliberately does NOT do
//!
//! It never asserts a check passed, fabricates a check run, or otherwise
//! forges evidence — it only produces a new, genuinely-unreviewed-by-nobody
//! commit for CI to run against for real. The #8248 guard is left completely
//! unweakened: the NEXT merge attempt still requires a check that actually
//! started at/after the (possibly-still-moving) base tip. It also moves the
//! head SHA, which — correctly, per #5686 — invalidates any standing Judge
//! approval; the PR is expected to cycle back through Judge once CI on the
//! new head is green, exactly as any other push would.

use std::process::Command;

/// The `gh` binary, honoring `LOOM_GH_BIN` — the same seam
/// `stale_checks::fetch` / `head_sync::fetch` provide.
fn gh_bin() -> String {
    std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string())
}

/// `gh_api`, parameterized on the binary — the injection seam [`redate`]'s
/// tests use so a stub `gh` can be passed as a plain function argument
/// instead of a global `LOOM_GH_BIN` env var, which would race across
/// parallel `cargo test` threads in the same process. [`gh_bin`]'s env lookup
/// stays the CLI's own default; only tests take this path directly.
fn gh_api_with(bin: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin)
        .arg("api")
        .args(args)
        .output()
        .map_err(|e| format!("could not exec gh api: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let trimmed = stderr.trim();
        let hint = if trimmed.is_empty() {
            format!("exit {}", out.status)
        } else {
            trimmed.to_string()
        };
        return Err(format!("gh api {} failed: {hint}", args.first().unwrap_or(&"")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit message every re-date commit carries — deterministic and
/// greppable, so a later pass (or a human) can tell "this is an automated
/// remedy commit" from the commit log alone without reading this source.
#[must_use]
pub fn commit_message(pr: &str) -> String {
    format!(
        "chore: re-date required checks for PR #{pr} (#8248 guard, automated by #8508)\n\n\
This commit intentionally changes NOTHING in the tree — it exists only to \
give every required check a fresh started_at, because the #8248 \
required-check-freshness guard blocked this merge on evidence that predates \
the base branch's current tip, and the merge token lacks actions:write to \
re-run the stale check directly. See champion-pr-merge.md's \"Stale-Check \
Freshness Remedy\" section."
    )
}

/// Whether a re-date push may proceed against the evidence the caller gated
/// on, or the branch already moved out from under it.
///
/// Pure and unit-testable, deliberately split from [`redate`]'s forge I/O —
/// mirrors `merge-pr.sh`'s own #5579 "gate on a fresh read, refuse to act on
/// a stale one" discipline.
#[derive(Debug, Clone, PartialEq)]
pub enum PushDecision {
    /// The branch's current head still matches what the caller gated on —
    /// safe to push the re-date commit on top of it.
    Proceed,
    /// The branch has already moved; pushing here would land on a tree
    /// nobody gated this decision on. Not a failure — see
    /// [`RedateOutcome::HeadMoved`].
    HeadMoved { current: String },
}

#[must_use]
pub fn decide(expected_head_sha: &str, current_head_sha: &str) -> PushDecision {
    if expected_head_sha == current_head_sha {
        PushDecision::Proceed
    } else {
        PushDecision::HeadMoved {
            current: current_head_sha.to_string(),
        }
    }
}

/// The result of attempting the remedy.
#[derive(Debug, Clone, PartialEq)]
pub enum RedateOutcome {
    /// A new, tree-identical commit was pushed as the branch's new tip.
    Pushed { new_sha: String },
    /// The branch already moved past `expected_head_sha` before this ran —
    /// no push was attempted. Mirrors `merge-pr.sh`'s #5579 exit-3 contract:
    /// re-evaluate fresh next tick, do not treat this as an error.
    HeadMoved { current: String },
    /// Could not read or write the forge state needed to push the remedy.
    Failed(String),
}

/// Push a tree-identical, single-parent commit onto `branch`'s current tip,
/// provided that tip still matches `expected_head_sha`.
///
/// `nwo` is `owner/repo`. `pr` is used only to compose the commit message.
pub fn redate(nwo: &str, branch: &str, expected_head_sha: &str, pr: &str) -> RedateOutcome {
    redate_with(&gh_bin(), nwo, branch, expected_head_sha, pr)
}

/// [`redate`]'s implementation, parameterized on the `gh` binary — the
/// injection seam the test suite drives directly (see [`gh_api_with`]).
fn redate_with(
    gh: &str,
    nwo: &str,
    branch: &str,
    expected_head_sha: &str,
    pr: &str,
) -> RedateOutcome {
    let current = match gh_api_with(
        gh,
        &[
            &format!("repos/{nwo}/git/refs/heads/{branch}"),
            "--jq",
            ".object.sha",
        ],
    ) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            return RedateOutcome::Failed(format!("ref heads/{branch} resolved to an empty sha"))
        }
        Err(e) => return RedateOutcome::Failed(e),
    };

    match decide(expected_head_sha, &current) {
        PushDecision::HeadMoved { current } => return RedateOutcome::HeadMoved { current },
        PushDecision::Proceed => {}
    }

    let tree = match gh_api_with(
        gh,
        &[
            &format!("repos/{nwo}/git/commits/{current}"),
            "--jq",
            ".tree.sha",
        ],
    ) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            return RedateOutcome::Failed(format!("commit {current} resolved to an empty tree sha"))
        }
        Err(e) => return RedateOutcome::Failed(e),
    };

    let message = commit_message(pr);
    let message_arg = format!("message={message}");
    let tree_arg = format!("tree={tree}");
    let parent_arg = format!("parents[]={current}");
    let new_sha = match gh_api_with(
        gh,
        &[
            &format!("repos/{nwo}/git/commits"),
            "-f",
            &message_arg,
            "-f",
            &tree_arg,
            "-f",
            &parent_arg,
            "--jq",
            ".sha",
        ],
    ) {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return RedateOutcome::Failed("commit creation returned an empty sha".to_string()),
        Err(e) => {
            return RedateOutcome::Failed(format!("could not create the re-date commit: {e}"))
        }
    };

    let sha_arg = format!("sha={new_sha}");
    match gh_api_with(
        gh,
        &[
            "-X",
            "PATCH",
            &format!("repos/{nwo}/git/refs/heads/{branch}"),
            "-f",
            &sha_arg,
            "-F",
            "force=false",
        ],
    ) {
        Ok(_) => RedateOutcome::Pushed { new_sha },
        Err(e) => RedateOutcome::Failed(format!(
            "commit {new_sha} was created but updating heads/{branch} to it failed \
(the commit is dangling, not on any branch, and will be garbage-collected): {e}"
        )),
    }
}

#[cfg(test)]
mod tests;
