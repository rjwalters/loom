//! `worktree.sh`'s upstream-tracking correction and the stale-worktree drift
//! report (#8195 slice 9, epic #7810) — the two places the create path asks
//! *"is this branch pointed at its own remote, and is this checkout still the
//! branch's real tip?"*.
//!
//! # What moved here
//!
//! Two blocks of `worktree.sh`, not one, because they were **two copies of the
//! same fix**:
//!
//! - **`local-branch`** (#6095/#6100) — the "local `feature/issue-N` already
//!   exists, no worktree directory yet" reuse arm. A branch left tracking
//!   `origin/<default>` instead of `origin/<branch>` turns a later
//!   `git pull --ff-only` in that worktree into a silent fast-forward onto the
//!   **default branch's tip**, diverging the checkout from the PR's actual
//!   head. Observed on #6086/PR #6093.
//! - **`registered-worktree`** (#6257/#6291) — the "worktree directory already
//!   exists and git knows about it" fast path. `worktree.sh`'s own comment at
//!   that site says it plainly: *"this is a completely different code path
//!   from the reuse path below — that fix's upstream-tracking correction never
//!   runs here"*. #6257 fixed the same defect a second time, by hand, in a
//!   second place, and added a drift report on top: a worktree whose HEAD is a
//!   strict ancestor of the branch's pushed tip is handed to a Judge or Doctor
//!   with no signal that it is stale (the #5609 incident).
//!
//! That duplication is the epic's *"two implementations of the same question"*
//! in its most literal form — same `git fetch`, same `show-ref --verify`, same
//! `rev-parse --abbrev-ref <branch>@{u}` comparison, same
//! `git branch --set-upstream-to`, differing only in the noun the message uses
//! and in whether the drift report runs afterwards. One implementation now
//! answers both, selected by [`Arm`], so the next fix to either cannot land in
//! only one of them.
//!
//! # Why this family
//!
//! It is not destructive, and that is exactly why it was worth taking early:
//! the whole body is *conditional configuration writes driven by string
//! comparison of git output*, the shape whose failure mode is silence. Both
//! defects above shipped and were only caught by an incident — neither made
//! anything exit non-zero, and neither produced a wrong message; they produced
//! **no** message. The retained assertions are correspondingly negative ones
//! ("did NOT print a correction for the already-correct case", "no
//! false-positive stale warning for a worktree that is genuinely ahead"),
//! which are precisely the assertions a shell implementation makes easy to
//! satisfy by accident.
//!
//! # Exit codes
//!
//! **0, always** — including when git itself fails. Every git invocation in
//! the retired shell was `|| true` or `2>/dev/null`, and the call sites read
//! nothing back: this is advisory repair, and a repo that cannot be fetched
//! from must not block worktree creation. There is no second code to define,
//! so there is nothing for a caller to branch on and nothing for a stale
//! daemon's clap to imitate.
//!
//! **A missing binary means the block does not run at all** — the shell
//! wrapper skips it on an empty `$_WT_DAEMON_BIN`, which is the pre-#6095 /
//! pre-#6257 behaviour: a branch keeps whatever upstream it had, and a stale
//! worktree is preserved without the warning. That is a lost *diagnosis*, not
//! a lost *file*: nothing here creates, deletes or resets anything, and the
//! `git pull --ff-only` that #6100 is really about is a human's later command,
//! not this script's. Contrast [`super::reset`], where the degradation had to
//! be argued down to a specific refusal code because the caller branches on
//! it.
//!
//! # Behaviour deliberately preserved from the shell
//!
//! - **Every message is gated on `--quiet`** (the shell's
//!   `if [[ "$JSON_OUTPUT" != "true" ]]`), while **every git side effect runs
//!   regardless**. Under `--json` the retired code still fetched and still
//!   corrected the upstream; it only stopped narrating. Suppressing the repair
//!   along with the narration would be a behaviour change.
//! - **Command substitution captures stdout whatever the exit code.** The
//!   shell wrote `$(git … 2>/dev/null || true)`, so a git that fails *after*
//!   writing to stdout still contributes its output. This matters for a real
//!   git behaviour: `git rev-parse origin/<branch>` on a missing ref echoes
//!   the argument back on **stdout** and fails, so the shell's `wt_origin_tip`
//!   would be the literal string `origin/<branch>`, not empty. See
//!   [`git_stdout_lossy`].
//! - **The upstream comparison treats "no upstream" and "wrong upstream"
//!   identically for the repair, and differently only for the message** —
//!   `print_warning "… was tracking '<x>' - correcting …"` vs.
//!   `print_info "… has no upstream - setting …"`.
//! - **An unpushed branch is never given a fabricated upstream.** The whole
//!   body sits behind `git show-ref --verify --quiet
//!   refs/remotes/origin/<branch>`; with no such ref, nothing runs. This is
//!   the retained suite's Test 4.
//! - **The drift report fires only on a strict ancestor.** `HEAD != tip` **and**
//!   `merge-base --is-ancestor HEAD tip`. A worktree that is *ahead* (unpushed
//!   local commits) or *diverged* is not drift, and warning about it was the
//!   false positive the retained suite's Test 2 pins.
//! - **The report is warn-only.** It never pulls, resets or checks anything
//!   out; the remediation is printed for a human to run.
//! - **`--uncommitted` is passed in, not recomputed.** The caller decided
//!   "preserve vs. reset" from a `git status --porcelain` taken *before* the
//!   fetch, and the hint block it selects here must agree with that decision.
//!   Re-reading the working tree inside this process could disagree with the
//!   branch the caller is about to take, which is how a "resolve before
//!   building on it" hint ends up attached to the wrong outcome.
//! - **The worktree path is quoted into the hints exactly as passed.** It is
//!   `$WORKTREE_PATH`, already absolute by construction at both call sites,
//!   and is never re-resolved or canonicalised — a path containing a space
//!   (#7858's class) reaches the message whole because it is one argv element
//!   here rather than an unquoted word in a shell.
//! - **Hint alignment is byte-exact.** The three-line uncommitted block pads
//!   its comments into a column; that padding is part of the retained output
//!   and is reproduced literally rather than recomputed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Which of the two retired blocks to run.
///
/// The arms differ in exactly two observable ways — the noun in the two
/// upstream messages, and whether the drift report runs — so they share one
/// body rather than one file with two copies of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arm {
    /// `worktree.sh`'s "local branch already exists, reusing it" arm
    /// (#6095/#6100). Subject noun `Branch`; no drift report — there is no
    /// checkout yet to be stale.
    LocalBranch,
    /// `worktree.sh`'s "worktree directory exists and is registered with git"
    /// fast path (#6257/#6291). Subject noun `Worktree branch`, plus the
    /// behind-the-pushed-tip drift report.
    RegisteredWorktree,
}

impl Arm {
    /// The noun the two upstream messages open with.
    fn subject(self) -> &'static str {
        match self {
            Arm::LocalBranch => "Branch",
            Arm::RegisteredWorktree => "Worktree branch",
        }
    }
}

/// Everything the check needs, once assembled by the shell wrapper.
pub struct Options {
    /// The directory git runs in (`git -C`). The main workspace root for
    /// [`Arm::LocalBranch`]; the worktree itself for
    /// [`Arm::RegisteredWorktree`], where it is *also* the path quoted into
    /// the drift hints.
    pub repo: PathBuf,
    /// `$BRANCH_NAME`.
    pub branch: String,
    /// Which retired block this is.
    pub arm: Arm,
    /// Print nothing. Passed by `worktree.sh` in `--json` mode. The git side
    /// effects still run — see the module docs.
    pub quiet: bool,
    /// `$ISSUE_NUMBER`, quoted into the `snapshot` hint. Only read by
    /// [`Arm::RegisteredWorktree`].
    pub issue: String,
    /// The caller's own `git status --porcelain` verdict, taken before the
    /// fetch — see the module docs for why this is passed in rather than
    /// recomputed. Only read by [`Arm::RegisteredWorktree`].
    pub uncommitted: bool,
}

/// Run the check. Always returns 0 — see the module docs.
pub fn run(opts: &Options) -> i32 {
    let out = Reporter { quiet: opts.quiet };
    let repo = opts.repo.as_path();
    let branch = opts.branch.as_str();
    let tracking = format!("origin/{branch}");

    // `git fetch origin "$BRANCH_NAME" 2>/dev/null || true`
    git_discard(repo, &["fetch", "origin", branch]);

    // `if git show-ref --verify --quiet "refs/remotes/origin/$BRANCH_NAME"`.
    // Everything below is inside this guard: with no remote branch of this
    // name there is nothing to point at and nothing to be behind, and
    // fabricating an upstream for a never-pushed branch is the defect Test 4
    // of the retained suite pins.
    if git_status_code(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ],
    ) != 0
    {
        return 0;
    }

    correct_upstream(repo, branch, &tracking, opts.arm, &out);

    if opts.arm == Arm::RegisteredWorktree {
        report_drift(repo, branch, &tracking, opts, &out);
    }

    0
}

/// The half both arms share: point the local branch at its own remote branch
/// when it is not already.
fn correct_upstream(repo: &Path, branch: &str, tracking: &str, arm: Arm, out: &Reporter) {
    // `current_upstream="$(git rev-parse --abbrev-ref "$BRANCH_NAME@{u}" 2>/dev/null || true)"`
    let current =
        git_stdout_lossy(repo, &["rev-parse", "--abbrev-ref", &format!("{branch}@{{u}}")]);
    if current == tracking {
        return;
    }

    if current.is_empty() {
        out.info(&format!(
            "{} '{branch}' has no upstream - setting it to '{tracking}'",
            arm.subject()
        ));
    } else {
        out.warning(&format!(
            "{} '{branch}' was tracking '{current}' - correcting to '{tracking}'",
            arm.subject()
        ));
    }

    // `git branch --set-upstream-to=… "$BRANCH_NAME" 2>/dev/null || true`
    git_discard(repo, &["branch", &format!("--set-upstream-to={tracking}"), branch]);
}

/// The [`Arm::RegisteredWorktree`]-only half: warn when this checkout's HEAD is
/// a **strict ancestor** of the branch's pushed tip (#6257). Warn-only — it
/// prints the remediation rather than running it.
fn report_drift(repo: &Path, branch: &str, tracking: &str, opts: &Options, out: &Reporter) {
    let head = git_stdout_lossy(repo, &["rev-parse", "HEAD"]);
    let tip = git_stdout_lossy(repo, &["rev-parse", tracking]);

    if head.is_empty() || tip.is_empty() || head == tip {
        return;
    }
    // `git merge-base --is-ancestor "$wt_head_sha" "$wt_origin_tip"` — being
    // BEHIND, not merely different. Ahead or diverged is not drift.
    if git_status_code(repo, &["merge-base", "--is-ancestor", &head, &tip]) != 0 {
        return;
    }

    let path = opts.repo.display();
    out.warning(&format!(
        "Worktree HEAD ({head}) is behind the pushed tip of branch '{branch}' ({tip}) - this worktree may be stale"
    ));
    if opts.uncommitted {
        out.warning(
            "Worktree also has uncommitted changes - resolve before evaluating/building on it:",
        );
        out.info(&format!(
            "  ./.loom/scripts/worktree.sh snapshot {} --include-untracked   # save WIP",
            opts.issue
        ));
        out.info(&format!(
            "  git -C {path} checkout -- .                                       # clear tracked working-tree drift"
        ));
        out.info(&format!(
            "  git -C {path} pull --ff-only                                      # resync to {tracking}"
        ));
    } else {
        out.info(&format!("  git -C {path} pull --ff-only   # resync to {tracking}"));
    }
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// `$(git -C "$dir" … 2>/dev/null || true)`.
///
/// **Stdout is returned whatever the exit code**, because that is what command
/// substitution does — the `|| true` only stops `set -e`, it does not discard
/// what was already written. The case where this is load-bearing rather than
/// pedantic: `git rev-parse origin/<branch>` on a ref that does not exist
/// prints the argument back on stdout (`origin/<branch>`) and *then* fails, so
/// the retired shell's `wt_origin_tip` held that literal string rather than
/// the empty value a success-only helper would report. An empty string is
/// returned only when git wrote nothing, or could not be spawned at all.
fn git_stdout_lossy(dir: &Path, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\n', '\r'])
                .to_string()
        })
        .unwrap_or_default()
}

/// `git -C "$dir" … >/dev/null 2>&1; echo $?` — a git that cannot be spawned
/// answers 127, matching bash's own report for that failure.
fn git_status_code(dir: &Path, args: &[&str]) -> i32 {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(127, |s| s.code().unwrap_or(127))
}

/// `git -C "$dir" … 2>/dev/null || true` where the result is not read.
///
/// **stderr is discarded; stdout is INHERITED**, because the retired shell
/// redirected only stderr on these two calls. That is not a technicality:
/// `git branch --set-upstream-to=…` prints `branch 'X' set up to track
/// 'origin/X'.` on **stdout**, so that confirmation line was part of
/// `worktree.sh`'s output on every correction — and, unlike the `print_*`
/// lines above it, it was **not** suppressed under `--json` (the shell's
/// `if [[ "$JSON_OUTPUT" != "true" ]]` wrapped the messages, not the git
/// command). Under `--json` the script has already pointed fd 1 at stderr for
/// the whole process, which is precisely what kept that line out of the JSON
/// document; inheriting fd 1 here reproduces both behaviours at once.
/// `git fetch` writes its own progress to stderr and so contributes nothing
/// here, but is routed identically for the same reason.
fn git_discard(dir: &Path, args: &[&str]) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// The two message levels this block uses, each gated on `--quiet` and each on
/// **stdout** — `print_warning` and `print_info` both `echo` without `>&2`,
/// and in `--json` mode `worktree.sh` has already rerouted fd 1 to stderr for
/// the whole process, so writing to stdout here is what preserves the
/// stdout-purity contract (#3546) rather than breaking it.
struct Reporter {
    quiet: bool,
}

const YELLOW: &str = "\x1b[1;33m";
const BLUE: &str = "\x1b[0;34m";
const NC: &str = "\x1b[0m";

impl Reporter {
    fn info(&self, msg: &str) {
        if !self.quiet {
            println!("{BLUE}ℹ {msg}{NC}");
        }
    }

    fn warning(&self, msg: &str) {
        if !self.quiet {
            println!("{YELLOW}⚠ {msg}{NC}");
        }
    }
}

#[cfg(test)]
mod tests;
