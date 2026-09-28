//! `worktree.sh`'s staleness **reference** for the already-registered-worktree
//! fast path (#8287, ported here per the shell language policy — #8354).
//!
//! # The defect this answers
//!
//! `worktree.sh`'s "the worktree directory already exists and git knows about
//! it" fast path decides whether to preserve the checkout or reset it, and it
//! decided both against `BASE_REF` — `origin/<default>`, or the parent branch
//! for a stacked child (#3729). A worktree is judged *stale* there when it is
//! 0 commits ahead of that reference with no uncommitted changes, and a stale
//! worktree is `git reset --hard` onto it.
//!
//! That is wrong whenever a **live `origin/<branch>` exists**. A local branch
//! sitting at the base is the *normal* state for a worktree that never
//! advanced past whatever it was created from, while the branch's real commits
//! — an open PR's entire content — live on `origin`. Resetting to the base
//! discards the only local trace of them and hands the next session (Judge or
//! Doctor) an EMPTY branch to rebase and force-push over the real PR. That is
//! the #8147/#8190 incident.
//!
//! So the reference — and therefore the reset target — is `origin/<branch>`
//! whenever that ref exists and has **not** already landed as a merged PR.
//! Otherwise it stays `BASE_REF`, exactly as before.
//!
//! # The #5657 skip is the whole subtlety
//!
//! A branch name is reused (a partial-increment slice's branch reused by the
//! next slice, #3667/#3599), so `origin/<branch>` can be the head of a branch
//! that has already merged. That tip is dead history: resetting a worktree
//! onto it would resurrect merged work as if it were pending. For that case
//! falling back to `BASE_REF` is still correct, which is why this is a
//! [`branch_landed`] question and not a bare `show-ref`.
//!
//! Two consequences of that, both load-bearing:
//!
//! 1. **The key is `origin/<branch>`, never the bare `<branch>`.** A LOCAL
//!    `<branch>` also exists here — that is this code path's entire premise —
//!    and [`branch_landed::probe`]'s resolution ladder prefers a local ref over
//!    the remote one. Passing the bare name would judge the STALE LOCAL tip's
//!    landed status instead of the live remote tip's, which is the opposite
//!    question. ([`branch_landed::forge_probe`] strips the `origin/` prefix
//!    before asking the forge, so the forge rung still asks about the right
//!    branch name.)
//! 2. **Only [`Verdict::Landed`] falls back.** `NotLanded` and `Unknown` both
//!    keep `origin/<branch>` as the reference, because the fail-closed
//!    direction here is *preserve the remote content*: over-preserving costs a
//!    rebase, under-preserving costs a PR.
//!
//! # Why this is Rust and not four more lines of `worktree.sh`
//!
//! It shipped first as `_worktree_resolve_stale_reset_ref` in
//! `lib/worktree-forge-pr-check.sh` (PR #8351) and the shell budget ratchet
//! refused it: that file is `contract`-category, whose growth has no override
//! (`.loom/docs/shell-language-policy.md`). The decision also *already* had a
//! tested Rust implementation of its hard part — [`branch_landed`] is the one
//! Rust copy of the #7812 ladder since #8470, and it is parameterised by
//! default-branch name, which is exactly what the stacked-child case needs.
//! Re-deriving the ladder in bash to avoid crossing the language boundary was
//! never the cheaper option; it was a second implementation of the question
//! this repo has spent five issues consolidating.
//!
//! # Contract
//!
//! One line on stdout, four space-separated tokens:
//!
//! ```text
//! <ref> <display> <ahead> <behind>
//! ```
//!
//! and **exit 0, always**. There is no failure mode to report: `BASE_REF` is
//! always available as an answer, so every path here has one. The caller's
//! `|| echo …` arm is therefore not an error path — it is the
//! no-daemon / daemon-predates-this-subcommand degradation, and what it
//! substitutes is precisely the pre-#8287 reading (`BASE_REF` plus the two
//! counts measured against it) that the shell used to compute inline.
//!
//! `<ahead>` / `<behind>` are measured against the reference this module
//! **chose**, not against `BASE_REF`, because the decision the caller makes
//! from them ("preserve or reset?") is about that reference. Measuring them in
//! the shell and choosing the reference here would leave the two out of step —
//! the shape of the original defect.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::branch_landed::{self, ForgeProbe, Verdict};

/// Everything the decision needs, all of it already resolved by the caller.
#[derive(Debug, Clone)]
pub struct Options {
    /// `$WORKTREE_PATH` — the existing worktree every git call runs in. Its
    /// `HEAD` is what the counts are measured from.
    pub worktree: PathBuf,
    /// `$BRANCH_NAME` — the worktree's own branch, e.g. `feature/issue-42`.
    pub branch: String,
    /// `$DEFAULT_BRANCH` (a bare name such as `main`), handed to
    /// [`branch_landed`] as the branch a merge would have landed on. Dynamic
    /// rather than the literal `origin/main` because a stacked child measures
    /// against its parent (#3729).
    pub default_branch: String,
    /// `$BASE_REF` — the reference used when no live remote branch applies.
    pub base_ref: String,
    /// `$BASE_DISPLAY` — how `$BASE_REF` is spelled in a human-facing message
    /// (`main`, not `origin/main`). Passed in rather than derived: the shell
    /// computes it from `--base` resolution and it is not recoverable from
    /// `base_ref` alone.
    pub base_display: String,
}

/// The chosen reference and the two counts measured against it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The git reference to compare and reset against.
    pub reference: String,
    /// How that reference is named in a human-facing message.
    pub display: String,
    /// Commits `HEAD` has that `reference` does not.
    pub ahead: u64,
    /// Commits `reference` has that `HEAD` does not.
    pub behind: u64,
}

impl Resolved {
    /// The caller's one line of stdout.
    #[must_use]
    pub fn render(&self) -> String {
        format!("{} {} {} {}", self.reference, self.display, self.ahead, self.behind)
    }
}

/// Print the resolved reference and exit code (always 0).
pub fn run(opts: &Options) -> i32 {
    println!("{}", resolve(opts).render());
    0
}

/// The decision, against the real forge.
#[must_use]
pub fn resolve(opts: &Options) -> Resolved {
    let worktree = opts.worktree.as_path();
    resolve_with(opts, &|branch| branch_landed::forge_probe(worktree, branch))
}

/// [`resolve`] with the forge round-trip injected — the seam the tests use to
/// drive the #5657 merged-tip skip against a real throwaway repo, offline.
#[must_use]
pub fn resolve_with(opts: &Options, forge: &dyn Fn(&str) -> ForgeProbe) -> Resolved {
    let worktree = opts.worktree.as_path();
    let (reference, display) = match remote_reference(opts, forge) {
        // `origin/<branch>` is its own display name — there is no shorter
        // spelling of it that stays unambiguous next to the local branch.
        Some(remote) => (remote.clone(), remote),
        None => (opts.base_ref.clone(), opts.base_display.clone()),
    };
    let ahead = rev_count(worktree, &format!("{reference}..HEAD"));
    let behind = rev_count(worktree, &format!("HEAD..{reference}"));
    Resolved {
        reference,
        display,
        ahead,
        behind,
    }
}

/// `Some("origin/<branch>")` when the live remote branch is the reference to
/// use: it exists, and it is not the head of an already-merged PR.
fn remote_reference(opts: &Options, forge: &dyn Fn(&str) -> ForgeProbe) -> Option<String> {
    let worktree = opts.worktree.as_path();
    let remote = format!("origin/{}", opts.branch);
    if git_code(
        worktree,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{}", opts.branch),
        ],
    ) != 0
    {
        return None;
    }
    // See the module docs: the key is the REMOTE spelling, and only `landed`
    // disqualifies it.
    let answer =
        branch_landed::probe_with(worktree, &remote, Some(opts.default_branch.as_str()), "", forge);
    (answer.verdict != Verdict::Landed).then_some(remote)
}

/// `git rev-list --count <range>`, or 0 when git cannot answer.
///
/// 0-on-failure mirrors the shell's `|| local_commits_ahead="0"` exactly,
/// including for an unresolvable reference. It is the safe direction only
/// because it is paired with `loom_worktree_reset_or_rescue`, which re-derives
/// commits-ahead itself immediately before the destructive reset (#6334) and
/// refuses rather than discarding.
fn rev_count(worktree: &Path, range: &str) -> u64 {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-list", "--count", range])
        .stderr(Stdio::null())
        .output()
    else {
        return 0;
    };
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// `git -C <dir> … >/dev/null 2>&1; echo $?`
fn git_code(dir: &Path, args: &[&str]) -> i32 {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(127, |s| s.code().unwrap_or(127))
}

#[cfg(test)]
mod tests;
