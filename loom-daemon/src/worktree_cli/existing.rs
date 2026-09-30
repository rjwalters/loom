//! `worktree.sh`'s **"the worktree directory already exists"** arm — the whole
//! decision, from *is git even aware of this directory?* to *preserve it, or
//! reset it in place?* (#8195 slice 12, epic #7810).
//!
//! # The defect: the last unanchored `git worktree list | grep -q`
//!
//! The arm opened with the registration probe
//!
//! ```text
//! if git worktree list | grep -q "$WORKTREE_PATH"; then
//! ```
//!
//! which is wrong in **both** directions, and slice 10 already retired its twin
//! on the `--sparse`/`--full` re-configure arm (where the same expression
//! decided whether to write a [`super::sentinel`] into a directory git did not
//! know about):
//!
//! - **False negative — a LIVE worktree read as unregistered.** `git worktree
//!   list` prints **symlink-resolved** paths; `$WORKTREE_PATH` is built by
//!   string concatenation from the caller's cwd and is not resolved. On any host
//!   where the repo is reached through a symlink — every macOS checkout under
//!   `/tmp`, which is a symlink to `/private/tmp`, and therefore every hermetic
//!   shell suite's fixture — the substring is absent and a perfectly live
//!   worktree took the `else` arm: *"Directory exists but is not a registered
//!   worktree"*, exit 1, advising `rm -rf` on a worktree that may hold
//!   uncommitted work. A re-invocation (Doctor resuming a Builder's worktree) is
//!   the common case, not an edge one.
//! - **False positive — an unregistered directory read as registered.** The
//!   match is an unanchored **substring**, so `…/worktrees/issue-4` matches the
//!   line for a registered `…/worktrees/issue-44`, and (with `grep`, not `grep
//!   -F`) the needle is a regular expression: a `+` or `.` in a repo path is a
//!   metacharacter. A false positive runs the preserve/reset path against a
//!   directory git knows nothing about — `git -C <dir>` there resolves to the
//!   **parent repo**, so the drift check and the reset target are computed for
//!   the main workspace's branch, and the arm ends by writing a
//!   `.loom-managed` sentinel into crash debris, which is precisely the
//!   authorization `rm -rf` tooling looks for (#3334).
//!
//! Same class as the defects slices 5, 8, 10 and 11 retired: **a path compared
//! textually instead of physically.** [`super::cleanup::is_registered_in`] is
//! the one canonicalizing predicate in this codebase — the orphan guard's own,
//! whose false answer is the #7849 `rm -rf` — and this module asks it rather
//! than adding a third opinion.
//!
//! # Why the whole arm, and not just the predicate
//!
//! Because a delegation costs shell lines and `worktree.sh` is frozen: moving
//! the predicate alone would have added a wrapper and a fallback worth more
//! lines than the two it retired, leaving the script no smaller and the epic no
//! further along. The arm is also genuinely one decision — the predicate's
//! answer selects between two terminal outcomes that share the reference
//! ([`super::stale_ref`]), the sentinel back-fill (#3548) and the message set —
//! so splitting it would have left the halves able to disagree, which is the
//! failure mode this epic exists to remove.
//!
//! What stays in `worktree.sh` is the one thing only it can do: nothing. The
//! `cd` slice 11 left behind has no counterpart here, so this arm moves whole,
//! including the `git fetch` + [`super::reset`] call that used to reach
//! `lib/worktree-race-rescue.sh` by name. That wrapper is untouched and still
//! the seam `test-worktree-race-rescue.sh` drives; this module calls the same
//! [`super::reset::run`] it delegates to, with the caller's `$$`/`$BASHPID`
//! passed through as `ignore_pids` exactly as the wrapper passes them — the
//! liveness probe (#7463) must not count the invoking shell as a foreign holder.
//!
//! # The reset is the part to be careful about
//!
//! This arm contains the script's stale-worktree `git reset --hard`. Three
//! properties are preserved deliberately and each has a test:
//!
//! 1. **The verdict's reference is the reset's target.** Both come from one
//!    [`super::stale_ref::resolve`] call, so the *"0 commits ahead of X"*
//!    message and the ref actually reset to cannot drift apart (#8287 — judging
//!    staleness against the base while a live `origin/<branch>` carries the
//!    PR's only commits is the #8147/#8190 incident).
//! 2. **The uncommitted-changes reading is taken ONCE.** The shell took
//!    `git status --porcelain` *before* the upstream check's fetch and passed the
//!    string to that check while separately re-testing it for the verdict, so the
//!    remediation hint and the preserve/reset decision could not disagree. Here
//!    there is one `let` and no convention to forget.
//! 3. **The reset is still gated by the #6334 rescue guard**, which re-derives
//!    every risk signal immediately before the destructive step and refuses
//!    rather than discarding. Nothing here pre-empts it: a fetch failure or any
//!    non-zero from it lands on *"Could not reset stale worktree (continuing to
//!    use as-is)"* and exit 0, which is a worktree left alone.
//!
//! # Exit codes, and why `1` is the only refusal
//!
//! `0` — the worktree is usable; the caller exits 0 (preserved, reset, or reset
//! refused-and-left-alone, all three of which the shell reported as exit 0).
//! `1` — the directory is not a registered worktree; the caller exits 1. There
//! is no third code: every "could not decide" inside the registered arm already
//! has a documented landing (a fetch that fails, a reference that will not
//! resolve) and none of them is a reason to refuse a worktree.
//!
//! On a host with no daemon (or one predating this subcommand) `worktree.sh`
//! answers the predicate itself, in one physical comparison, and always
//! **preserves**: it never resets. That degradation is the safe direction by
//! construction — the diagnosis and the stale-worktree hygiene are lost, never a
//! file — and it is why this delegation is safe where slice 1's lock delegation
//! was not (#8226).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::wip::Out;

/// Everything the arm needs, all of it already resolved by `worktree.sh`.
pub struct Options {
    /// `$WORKTREE_PATH` — the existing directory under decision. Quoted into
    /// messages **exactly as given** (unresolved), matching the shell.
    pub worktree: PathBuf,
    /// `$WORKTREE_REPO_ROOT` — the main workspace. The registration probe's
    /// `git -C` target, because the retired `git worktree list` ran with cwd
    /// there (the script auto-navigates out of any worktree first).
    pub repo: PathBuf,
    /// `$ISSUE_NUMBER`, verbatim — it reaches the sentinel and the *"Run again:
    /// pnpm worktree <N>"* hint as a string, leading zeros included.
    pub issue: String,
    /// `$BRANCH_NAME` — the worktree's branch, for [`super::stale_ref`] and the
    /// sentinel's `# Branch: ` line.
    pub branch: String,
    /// `$DEFAULT_BRANCH` — the bare default-branch name.
    pub default_branch: String,
    /// `$BASE_REF` — the staleness reference when no live unmerged
    /// `origin/<branch>` applies.
    pub base_ref: String,
    /// `$BASE_DISPLAY` — how `base_ref` is spelled to a human.
    pub base_display: String,
    /// `$BASE_BRANCH` — the `--base` override, empty when absent. Only the
    /// pre-reset `git fetch origin -- "${BASE_BRANCH:-$DEFAULT_BRANCH}"` reads it.
    pub base_branch: String,
    /// `--json` mode: suppress every message the shell gated on
    /// `[[ "$JSON_OUTPUT" != "true" ]]`, and route the ungated ones to stderr
    /// (where the shell's own fd-1-to-stderr redirection put them).
    pub quiet: bool,
    /// The calling shell's `$$` / `$BASHPID`, forwarded to the reset guard's
    /// liveness probe so it does not count the invoker as a foreign holder.
    pub ignore_pids: Vec<u32>,
}

/// Which terminal outcome the arm reached. Returned by [`decide`] so the tests
/// can assert the decision without reading messages back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The directory is not a registered worktree — exit 1.
    Unregistered,
    /// Commits ahead, or uncommitted changes: existing work preserved.
    Preserved,
    /// Stale, and the reset succeeded.
    Reset,
    /// Stale, but the fetch failed or the #6334 guard refused: left as-is.
    ResetFailed,
    /// The `.loom-managed` sentinel could not be written — the one refusal the
    /// registered arm can still reach. See [`write_sentinel`].
    SentinelFailed,
}

impl Outcome {
    /// The process exit code — see the module docs.
    #[must_use]
    pub fn code(&self) -> i32 {
        match self {
            Outcome::Unregistered | Outcome::SentinelFailed => 1,
            Outcome::Preserved | Outcome::Reset | Outcome::ResetFailed => 0,
        }
    }
}

/// Run the arm: print what the shell printed, do what the shell did, return its
/// exit code.
pub fn run(opts: &Options) -> i32 {
    decide(opts, &|req| perform_reset_with(opts, req)).code()
}

/// [`run`] with the destructive step injected — the seam the unit tests use to
/// drive both reset outcomes without a real `git reset --hard`, and the reason
/// the message set can be asserted around a reset that did not happen.
///
/// `reset` is handed the same [`Options`] and the resolved target ref; it
/// returns `true` when the worktree ended up reset (the shell's `git fetch && …
/// reset_or_rescue` succeeding as a pair).
pub fn decide(opts: &Options, reset: &dyn Fn(&ResetRequest) -> bool) -> Outcome {
    let out = Reporter {
        out: Out::new(opts.quiet),
        quiet: opts.quiet,
    };
    let shown = opts.worktree.to_string_lossy().to_string();

    // `if git worktree list | grep -q "$WORKTREE_PATH"` — see the module docs
    // for both directions this used to get wrong. The probe is the orphan
    // guard's own canonicalizing predicate, asked in the main workspace.
    if !super::cleanup::is_registered_in(Some(&opts.repo), &opts.worktree) {
        // NOT gated on `--json`: the shell's `print_error`/`print_info`/`echo`
        // here were unconditional, and in `--json` mode its fd 1 was already
        // stderr, which is what [`Out::new(quiet)`] reproduces.
        Out::error("Directory exists but is not a registered worktree");
        out.plain_always("");
        out.info_always("To fix this:");
        out.plain_always(&format!("  1. Remove the directory: rm -rf {shown}"));
        out.plain_always(&format!("  2. Run again: pnpm worktree {}", opts.issue));
        return Outcome::Unregistered;
    }

    // `local_uncommitted=$(git -C "$WORKTREE_PATH" status --porcelain 2>/dev/null)`
    // — ONE reading, taken here (after the registration proof, before the drift
    // check's fetch) and consumed by BOTH the drift report's remediation hint and
    // the verdict below, so the two cannot disagree. The shell achieved that by
    // passing the string to the one and re-testing it for the other; here there
    // is one value and no convention to forget.
    let uncommitted = has_uncommitted(&opts.worktree);

    // `_worktree_upstream_check registered-worktree "$WORKTREE_PATH" "$local_uncommitted"`
    // — the #6257/#6291 arm: correct this checkout's upstream tracking and report
    // drift against the branch's own pushed tip. Its exit code was discarded by
    // the shell (`|| true`) and is discarded here for the same reason: a repo that
    // cannot be fetched from must not block a worktree being handed over. It runs
    // BEFORE the reference is resolved because that resolution reads
    // `origin/<branch>`, which this call's fetch is what refreshes.
    let _ = super::upstream::run(&super::upstream::Options {
        repo: opts.worktree.clone(),
        branch: opts.branch.clone(),
        arm: super::upstream::Arm::RegisteredWorktree,
        quiet: opts.quiet,
        issue: opts.issue.clone(),
        uncommitted,
    });

    // ONE resolution of the reference, feeding both the verdict below and the
    // reset target (#8287). `ahead`/`behind` are measured against whichever
    // reference it chose, never against `$BASE_REF` regardless.
    let resolved = super::stale_ref::resolve(&super::stale_ref::Options {
        worktree: opts.worktree.clone(),
        branch: opts.branch.clone(),
        default_branch: opts.default_branch.clone(),
        base_ref: opts.base_ref.clone(),
        base_display: opts.base_display.clone(),
    });

    if resolved.ahead > 0 || uncommitted {
        // Real work — preserve it, after back-filling the sentinel so a resumed
        // worktree that lost its marker stays cleanup-eligible (#3548).
        if let Err(err) = write_sentinel(opts) {
            return err;
        }
        out.info("Worktree is registered with git");
        if resolved.ahead > 0 {
            out.info(&format!(
                "Worktree has {} commit(s) ahead of {} - preserving existing work",
                resolved.ahead, resolved.display
            ));
        } else if uncommitted {
            out.info("Worktree has uncommitted changes - preserving existing work");
        }
        out.blank();
        out.info(&format!("To use this worktree: cd {shown}"));
        return Outcome::Preserved;
    }

    // Stale: 0 commits ahead of the chosen reference, no uncommitted changes.
    // Reset in place rather than removing (which would corrupt a caller's cwd).
    out.warning(&format!(
        "Stale worktree detected (0 commits ahead, {} behind {}, no uncommitted changes)",
        resolved.behind, resolved.display
    ));
    out.info(&format!("Resetting worktree in place to {}...", resolved.display));
    // Written BEFORE the reset attempt and on both of its outcomes, exactly as
    // the shell did: the worktree remains usable either way (#3548).
    if let Err(err) = write_sentinel(opts) {
        return err;
    }

    let did_reset = reset(&ResetRequest {
        target_ref: resolved.reference.clone(),
        display: resolved.display.clone(),
    });

    if did_reset {
        out.success(&format!("Stale worktree reset to {}", resolved.display));
        out.blank();
        out.info(&format!("To use this worktree: cd {shown}"));
        Outcome::Reset
    } else {
        out.warning("Could not reset stale worktree (continuing to use as-is)");
        out.blank();
        out.info(&format!("To use this worktree: cd {shown}"));
        Outcome::ResetFailed
    }
}

/// What [`decide`] asks its injected reset to perform.
pub struct ResetRequest {
    /// The ref to reset to — the reference the verdict was reached against.
    pub target_ref: String,
    /// That reference's human-facing spelling, for a caller that reports it.
    pub display: String,
}

/// `write_loom_sentinel "$WORKTREE_PATH"`.
///
/// The shell's `cat > "$wt/.loom-managed"` ran under `set -e`, so a failed write
/// aborted the whole script with a non-zero status rather than reporting a
/// usable worktree. The same choice here, as the one refusal the registered arm
/// can still reach: a worktree whose sentinel could not be written is one
/// `merge-pr.sh` will later refuse to clean up, and silently continuing is what
/// #3548 was filed about.
fn write_sentinel(opts: &Options) -> Result<(), Outcome> {
    match super::sentinel::write(&opts.worktree, &opts.issue, &opts.branch) {
        Ok(()) => Ok(()),
        Err(err) => {
            Out::error(&format!(
                "Failed writing the Loom sentinel to {}/{}: {err}",
                opts.worktree.to_string_lossy(),
                super::sentinel::FILE_NAME
            ));
            Err(Outcome::SentinelFailed)
        }
    }
}

/// The real destructive step: the shell's
///
/// ```text
/// git -C "$WORKTREE_PATH" fetch origin -- "${BASE_BRANCH:-$DEFAULT_BRANCH}" 2>/dev/null && \
///    loom_worktree_reset_or_rescue "$WORKTREE_PATH" "$stale_ref" "issue-$N-stale-worktree-reset"
/// ```
///
/// The `--` is #9106's end-of-options separator and is load-bearing: without
/// it a `--base` / default-branch name such as `--upload-pack=/tmp/x` is
/// re-parsed by git as a switch (on a path origin, that EXECUTES the payload).
/// With it the name is a refspec, the fetch fails, and no reset is attempted.
/// `worktree.sh` has already refused such names via `check_branch_name` before
/// delegating here; this is the defence in depth behind that check, and
/// `a_dash_prefixed_branch_is_never_a_git_option` pins it.
///
/// Both halves, in order, short-circuiting on the fetch exactly as `&&` did —
/// a fetch that fails means no reset is attempted at all. The fetch key stays
/// the BASE branch (not the chosen reference): when the reference is
/// `origin/<branch>` its own remote was already fetched by the upstream check
/// that ran immediately before this arm, and refreshing the base too is
/// harmless.
///
/// **`-c maintenance.auto=false` (#9620).** Every `git fetch` ends by spawning
/// `git maintenance run --auto --detach`, and the detached child keeps the
/// fetch's cwd, which is this worktree. The #7463 liveness probe that
/// [`super::reset::run`] opens with then counts git's own housekeeping as a
/// foreign live holder and refuses the reset. The shell took two `exec`s to
/// reach its probe, so the child had nearly always exited by then. This port
/// probes in-process microseconds after the fetch, and on Linux's instant
/// `/proc` walk it lost that race often enough to leave stale worktrees
/// unreset. The config form is used rather than `--no-auto-maintenance`
/// because older gits ignore an unknown key but reject an unknown flag, and a
/// rejected fetch here means no reset at all. The probe itself is untouched.
fn perform_reset_with(opts: &Options, req: &ResetRequest) -> bool {
    let fetch_key = if opts.base_branch.is_empty() {
        opts.default_branch.as_str()
    } else {
        opts.base_branch.as_str()
    };
    if !git_ok(
        &opts.worktree,
        &[
            "-c",
            "maintenance.auto=false",
            "fetch",
            "origin",
            "--",
            fetch_key,
        ],
    ) {
        return false;
    }
    super::reset::run(&super::reset::Options {
        worktree: opts.worktree.clone(),
        target_ref: req.target_ref.clone(),
        rescue_label: format!("issue-{}-stale-worktree-reset", opts.issue),
        ignore_pids: opts.ignore_pids.clone(),
    }) == 0
}

/// `local_uncommitted=$(git -C "$wt" status --porcelain 2>/dev/null)`, reduced to
/// the only thing either consumer read: whether it was non-empty.
///
/// A git that cannot answer reads as **no** uncommitted changes, matching the
/// shell's `|| local_uncommitted=""` exactly. That is the direction that lets a
/// stale worktree be reset, so it is not a free choice — it is safe only because
/// the reset is gated by the #6334 guard, which re-derives the tracked-diff state
/// itself immediately before the destructive step and rescues or refuses.
fn has_uncommitted(worktree: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain"])
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| out.status.success() && !out.stdout.is_empty())
}

/// `git -C <dir> <args> >/dev/null 2>/dev/null` — did it exit 0?
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// [`Out`] plus the shell's per-message `[[ "$JSON_OUTPUT" != "true" ]]` gate.
///
/// Two families, deliberately distinguished by name rather than by a parameter,
/// because the retired block gated them differently and a reviewer has to be
/// able to see which is which: the registered arm's messages were gated, the
/// unregistered arm's were not.
struct Reporter {
    out: Out,
    quiet: bool,
}

impl Reporter {
    /// `print_info`, gated.
    fn info(&self, msg: &str) {
        if !self.quiet {
            self.out.info(msg);
        }
    }

    /// `print_warning`, gated.
    fn warning(&self, msg: &str) {
        if !self.quiet {
            self.out.warning(msg);
        }
    }

    /// `print_success`, gated.
    fn success(&self, msg: &str) {
        if !self.quiet {
            self.out.success(msg);
        }
    }

    /// `echo ""`, gated — a blank line, not an empty coloured one.
    fn blank(&self) {
        if !self.quiet {
            self.plain_always("");
        }
    }

    /// `print_info`, UNGATED (the unregistered arm's *"To fix this:"*).
    fn info_always(&self, msg: &str) {
        self.out.info(msg);
    }

    /// A bare `echo`, UNGATED: no icon, no colour, and routed to stderr in
    /// `--json` mode for the same stdout-purity reason as [`Out::info`] —
    /// `worktree.sh` had already pointed fd 1 at stderr there (#3546).
    fn plain_always(&self, msg: &str) {
        if self.quiet {
            eprintln!("{msg}");
        } else {
            println!("{msg}");
        }
    }
}

#[cfg(test)]
mod tests;
