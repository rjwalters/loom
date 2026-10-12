//! `worktree.sh`'s **local-branch reuse arm** — the whole of it (#8195 slice
//! 14, epic #7810).
//!
//! This is the arm `worktree.sh <N>` takes when `refs/heads/feature/issue-N`
//! already exists: the normal state on a host that built an earlier slice of
//! the same issue, and therefore the arm the partial-increment convention
//! (#3667/#3599) runs through most often. Four things happened there, in this
//! order:
//!
//! 1. the `Branch '<b>' already exists - reusing it (…)` warning;
//! 2. the upstream-tracking correction (#6095/#6100) — already Rust since
//!    slice 9, reached here as [`super::upstream`] IN-PROCESS rather than
//!    through the shell wrapper this slice retires;
//! 3. the **already-landed refusal** (#8280): a branch whose tip is an
//!    already-merged PR's head may not be handed back, because the name is
//!    taken locally and this arm cannot silently fall through to a fresh
//!    branch the way its `origin/<b>` sibling does;
//! 4. the base-ref divergence warning (#8280 again).
//!
//! # Why the arm moves as one unit
//!
//! Because the four steps are one ordered contract, and the *order* is the
//! part no reviewer can check from either side alone. Steps 1–2 must run
//! before step 3 so an operator sees which branch is being judged before the
//! refusal names it, and step 2's `git fetch origin -- <branch>` is what makes
//! step 4's history comparison meaningful. Splitting them would have left the
//! shell interleaving a Rust call between two Rust calls purely to preserve
//! print order — the "delegation point that is not a decision boundary" shape
//! #8226 reverted.
//!
//! Folding step 2 in retires `worktree.sh`'s `_worktree_upstream_check`
//! wrapper outright: since slice 12 moved the `registered-worktree` arm into
//! [`super::existing`] (which also reaches [`super::upstream`] in-process),
//! the `local-branch` arm was its only live caller.
//!
//! # The defect this slice fixes
//!
//! The refusal's `--json` document was spliced by hand:
//!
//! ```text
//! echo '{"success": false, …, "branch": "'"$BRANCH_NAME"'", …}'
//! ```
//!
//! `$BRANCH_NAME` is `feature/$CUSTOM_BRANCH` when the caller passed a branch
//! argument — arbitrary operator input — and `git check-ref-format` permits
//! `"` and other JSON metacharacters in a refname. Such a name produced a
//! document that is not JSON at all, so a `--json` consumer piping into `jq`
//! got a parse error where it asked for a refusal reason. Same class as slice
//! 13's `$BASE_BRANCH`, fixed the same way: `serde_json`.
//!
//! # Fail-open is the fixed direction
//!
//! [`Verdict::Unknown`] reuses the branch. That is not conservatism, it is the
//! pinned contract — `test-worktree-stale-merged-branch.sh` Test 5 asserts a
//! forge outage still yields a worktree, because a guard that refused on a
//! failed probe would make every offline clone unable to reuse its own branch.
//! The refusal needs *proof*: a merged PR whose head still equals the tip.
//!
//! # The degenerate-SHA guard is load-bearing
//!
//! A branch tip identical to `origin/<default>`'s current tip is trivially its
//! own ancestor, so [`super::branch_landed`]'s ancestry rung answers `landed`
//! for a brand-new local branch carrying no work yet — which is
//! `worktree.sh`'s own ordinary re-run state. Refusing that would break every
//! second invocation, so the refusal additionally requires the tip to *differ*
//! from the current default tip. A tip that differs is never this degenerate
//! case, whichever rung answered.
//!
//! # Output contract
//!
//! Messages print directly through [`Out`] (the shared reporter that mirrors
//! `print_warning` / `print_error` byte-for-byte, colours included) rather
//! than coming back as records: the shell wrapper is one line and owns no
//! message text. `--json-output true` suppresses every human line — exactly as
//! the retired shell's `if [[ "$JSON_OUTPUT" != "true" ]]` gates did, a
//! suppression and not a reroute — and emits only the refusal document, on
//! **stdout**, which `worktree.sh` redirects to its fd 3 (the caller's real
//! stdout under the #3546 purity contract). That is the dispatch shape
//! [`super::closed_pr_branch`] uses, and the reason `--json-output` is a
//! string rather than a flag: the shell side has to stay a single line under
//! the portable-shell ratchet.
//!
//! | code | meaning |
//! |---|---|
//! | 0 | reuse the local branch |
//! | 1 | refuse — its tip is an already-merged PR's head; a message was printed |
//!
//! The refusal names the worktree still holding the branch, if any (#9319) —
//! see [`holder`]. It stays a diagnosis: nothing is removed or deleted here.
//!
//! 2 is never returned: the shell's own `--help` probe reserves it for "could
//! not run at all".
//!
//! # Paths
//!
//! `repo` is one `OsString` handed whole to `git -C`; nothing here splits,
//! re-quotes or word-splits it, so a main workspace whose path contains a
//! space (#7858's data-loss class) reaches every `git` invocation intact.
//! `loom-daemon/tests/worktree_branch_reuse_differential.rs` builds both sides
//! of its fixture under a directory named `re po` for exactly that reason.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::branch_landed::{self, Verdict};
use super::wip::Out;

mod holder;

/// Everything the arm needs, assembled by the shell wrapper.
pub struct Options {
    /// The main workspace root every `git` call runs in (`$WORKTREE_REPO_ROOT`).
    pub repo: PathBuf,
    /// `$BRANCH_NAME` — the local branch under question. The caller has
    /// already confirmed `refs/heads/<branch>` exists.
    pub branch: String,
    /// `$ISSUE_NUMBER`, quoted into the messages and the JSON document.
    pub issue: String,
    /// `$DEFAULT_BRANCH` — what "landed" is measured against.
    pub default_branch: String,
    /// `$BASE_REF` — the ref the divergence warning compares history against
    /// (`origin/<default>`, or a `--base` override).
    pub base_ref: String,
    /// `$BASE_DISPLAY` — how that ref is spelled to a human.
    pub base_display: String,
    /// `$JSON_OUTPUT`, as a string — see the module doc.
    pub json_output: String,
}

impl Options {
    /// `$JSON_OUTPUT == "true"`.
    #[must_use]
    pub fn json(&self) -> bool {
        self.json_output == "true"
    }
}

/// What the arm concluded, with the rung that decided it. A value rather than
/// a bare exit code so the decision table is testable without a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Hand the branch back — no merged-PR proof, or the degenerate tip.
    Reuse,
    /// The tip is an already-merged PR's head (#8280).
    Refuse,
}

/// The whole refusal rule, over already-resolved inputs. Pure — no git, no
/// forge.
///
/// `tip_is_current_default` is the degenerate-SHA guard from the module doc:
/// `true` suppresses the refusal even on a `Landed` verdict.
#[must_use]
pub fn decide(verdict: Verdict, tip_is_current_default: bool) -> Decision {
    if verdict == Verdict::Landed && !tip_is_current_default {
        Decision::Refuse
    } else {
        Decision::Reuse
    }
}

/// Run the arm against the real forge. Returns the process exit code.
pub fn run(opts: &Options) -> i32 {
    run_with(opts, &|branch| branch_landed::forge_probe(&opts.repo, branch))
}

/// [`run`] with the forge round-trip injected — the seam a test uses instead
/// of a network, a `gh`, or a `loom-daemon`.
pub fn run_with(opts: &Options, forge: &dyn Fn(&str) -> branch_landed::ForgeProbe) -> i32 {
    let json = opts.json();
    let out = Out::new(json);
    let repo = opts.repo.as_path();

    // Step 1. `print_warning "Branch '…' already exists - reusing it (…)"`,
    // gated on `$JSON_OUTPUT != "true"` in the retired shell.
    if !json {
        out.warning(&format!(
            "Branch '{}' already exists - reusing it (for a fresh branch instead, pass a custom name: ./.loom/scripts/worktree.sh {} <custom-branch-name>)",
            opts.branch, opts.issue
        ));
    }

    // Step 2. The slice-9 upstream-tracking correction, in-process. `repo` is
    // the main workspace (cwd was the main workspace at the retired call
    // site); `uncommitted` is `false` because the `local-branch` arm never
    // read it — there is no checkout yet to hold uncommitted changes.
    let _ = super::upstream::run(&super::upstream::Options {
        repo: opts.repo.clone(),
        branch: opts.branch.clone(),
        arm: super::upstream::Arm::LocalBranch,
        quiet: json,
        issue: opts.issue.clone(),
        uncommitted: false,
    });

    // Step 3. The #8280 already-landed refusal.
    let answer =
        branch_landed::probe_with(repo, &opts.branch, Some(&opts.default_branch), "", forge);
    if decide(answer.verdict, tip_is_current_default(repo, opts)) == Decision::Refuse {
        report_refusal(opts, &out, answer.pr_number.as_deref());
        return 1;
    }

    // Step 4. The base-ref divergence warning. `$JSON_OUTPUT != "true"` gated
    // the whole statement, including the `git merge-base` call — so this is an
    // early skip rather than a suppressed print.
    if !json && !contains_base_history(repo, opts) {
        out.warning(&format!(
            "Branch '{}' has diverged from {} (does not contain all of its history) - reusing it as-is; rebase or delete it if that is not what you want",
            opts.branch, opts.base_display
        ));
    }
    0
}

/// `[[ "$(git rev-parse "$BRANCH_NAME")" == "$(git rev-parse "origin/$DEFAULT_BRANCH")" ]]`
/// — the degenerate-SHA guard.
///
/// Both operands are compared as git resolved them, including the case where
/// one or both fail to resolve: the retired shell compared two empty strings
/// as equal (and so suppressed the refusal), and an unresolvable tip is not
/// evidence of anything.
fn tip_is_current_default(repo: &Path, opts: &Options) -> bool {
    let tip = git_stdout(repo, &["rev-parse", &opts.branch]);
    let default_tip = git_stdout(repo, &["rev-parse", &format!("origin/{}", opts.default_branch)]);
    tip == default_tip
}

/// `git merge-base --is-ancestor "$BASE_REF" "$BRANCH_NAME"` — does the branch
/// contain all of the base ref's history?
fn contains_base_history(repo: &Path, opts: &Options) -> bool {
    git_ok(repo, &["merge-base", "--is-ancestor", &opts.base_ref, &opts.branch])
}

/// The refusal, in whichever of the two output modes the caller is in — an
/// if/else in the retired shell, so exactly one of the two is emitted.
///
/// Both modes name the worktree still holding the branch, when there is one
/// (#9319): `git branch -D` refuses a held branch, so the bare remedy is
/// wrong in exactly that state. See [`holder`].
fn report_refusal(opts: &Options, out: &Out, pr_number: Option<&str>) {
    let held_by = holder::find(&opts.repo, &opts.branch);
    if opts.json() {
        out.json_line(&refusal_document(opts, pr_number, held_by.as_ref()).to_string());
        return;
    }
    Out::error(&refusal_message(opts, pr_number, held_by.as_ref()));
}

/// The `--json` refusal document. `heldByWorktree` is additive (#9319): the
/// holding worktree's path, present only when a worktree holds the branch.
///
/// Absent rather than `null` with no holder, so that document is the retired
/// shell's key for key — the differential test compares the two as values —
/// just as the human line is its line byte for byte. A consumer reading
/// `.heldByWorktree` sees `null` either way. The other five keys and their
/// types never change.
fn refusal_document(
    opts: &Options,
    pr_number: Option<&str>,
    held_by: Option<&holder::Holder>,
) -> serde_json::Value {
    let mut doc = serde_json::json!({
        "success": false,
        "error": "branch-already-landed",
        "issueNumber": json_issue(&opts.issue),
        "branch": opts.branch,
        "prNumber": json_pr(pr_number),
    });
    if let Some(h) = held_by {
        doc["heldByWorktree"] = h.path.to_string_lossy().into_owned().into();
    }
    doc
}

/// The human refusal line. With no holder it is the retired shell's line,
/// byte for byte — the differential test pins that.
fn refusal_message(
    opts: &Options,
    pr_number: Option<&str>,
    held_by: Option<&holder::Holder>,
) -> String {
    let pr = pr_number
        .filter(|n| !n.is_empty())
        .map(|n| format!(" (already-merged PR #{n})"))
        .unwrap_or_default();
    let delete_and_rerun = format!(
        "git branch -D {branch} && ./.loom/scripts/worktree.sh {issue}",
        branch = opts.branch,
        issue = opts.issue,
    );
    let remedy = held_by.map_or_else(
        || format!("Delete it and re-run: {delete_and_rerun}"),
        |h| h.remedy(&opts.default_branch, &delete_and_rerun),
    );
    format!(
        "Local branch '{branch}' has already landed on {base}{pr} - refusing to reuse it. {remedy}",
        branch = opts.branch,
        base = opts.base_display,
    )
}

/// `"issueNumber": '"$ISSUE_NUMBER"'` — unquoted in the retired shell, so a
/// JSON **number**. `worktree.sh` has already refused a non-numeric issue
/// number by this point, so the fallback is unreachable in practice; it emits
/// a string rather than invalid JSON, because no document this process writes
/// may fail to parse.
fn json_issue(issue: &str) -> serde_json::Value {
    issue
        .parse::<u64>()
        .map(serde_json::Value::from)
        .unwrap_or_else(|_| serde_json::Value::from(issue))
}

/// `"prNumber": '"${BRANCH_LANDED_PR_NUMBER:-null}"'` — also unquoted, so a
/// bare number or the literal `null`. A non-numeric answer from the forge
/// becomes `null` rather than a quoted string, so the field's type never
/// changes under an existing consumer — the rule
/// [`super::closed_pr_branch`] states for the same field.
fn json_pr(number: Option<&str>) -> serde_json::Value {
    match number {
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => n
            .parse::<u64>()
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
        _ => serde_json::Value::Null,
    }
}

fn git_ok(repo: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
