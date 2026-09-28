//! The pre-merge merge-ordering guard (#3747 stacked-PR v2 item 2, reshaped by
//! #7982; a slice of #8191).
//!
//! # The race it closes
//!
//! Loom's own recommended repository setting — `delete_branch_on_merge: true`,
//! applied by `setup-repository-settings.sh` — makes GitHub delete
//! `feature/issue-<parent>` **synchronously, inside the merge API call**. The
//! post-merge stacked reconcile (`reconcile-stack.sh`, item 1) then wants to run
//! `git rebase --onto <default> <parent-branch> <child-branch>`, which needs
//! `<parent-branch>` to still resolve as a ref. It no longer does, so the
//! post-merge pass races the repo's own settings and loses.
//!
//! #7982's answer is the reason this guard is interesting: rather than *assert*
//! the postcondition a later step needs (hard-block the parent merge), it
//! **establishes** it — the parent's tip is already known at merge time, so the
//! guard pins it to `refs/loom/parent/<branch>` (a plain, non-per-worktree ref
//! every worktree of the repo can see, which `reconcile-stack.sh` falls back to)
//! and proceeds with a loud warning. The hard block survives for exactly one
//! case: the tip could not be pinned at all, which is when the original #3747
//! failure is genuinely still reachable.
//!
//! # Why the shell could not keep it
//!
//! `merge-pr.sh` is frozen by the file-size ratchet, and #7982 said so in its
//! own comment: the pin/warn/block decision was "written densely on purpose" to
//! land at a net code-line saving, because a sibling `.sh` was not an available
//! remedy. A guard whose next fix has to be smaller than its last is the trap
//! #8191 exists to break. The dense form also concentrated the fragility: one
//! compound `if` chained `cat-file -e`, `fetch`, `cat-file -e` and `update-ref`
//! with `&&`, and four operator-facing messages interpolated seven variables
//! each. Both are what this module takes over.
//!
//! # What stays in the shell, and why
//!
//! - **The `FORGE_TYPE == github` gate.** `merge-pr.sh` already knows which
//!   forge it is talking to, and the retained suite asserts the pin flag is
//!   GitHub-gated by reading the guard body
//!   (`test-merge-pr-auto-queued-stacked-children.sh`). Nothing is gained by
//!   moving a fact the caller already holds.
//! - **`STACKED_CHILDREN_JSON` / `STACKED_CHILDREN_PIN_WRITTEN`.** They are
//!   read by later shell steps this port has not reached yet (the post-merge
//!   reconcile and the #8010 item-3 re-pin), so this verb reports them as
//!   records and the stub assigns them.
//! - **The item-3 merge-time re-pin.** It is top-level script code that runs
//!   much later, after `$MERGE_PRECONDITION_SHA` is freshly read; it is not part
//!   of this function and is not in this slice.
//!
//! # Fail-open, argued
//!
//! Every *review* gate in this family fails closed
//! ([`super::labels`], [`super::stale_checks`], [`super::loom_pr_guard`]):
//! "could not check" must never read as "checked, fine" when the question is
//! whether this tree may merge. This guard is not that kind of question. It
//! establishes a postcondition for a later, best-effort cleanup step, so a verb
//! that cannot answer costs **one manual `reconcile-stack.sh` invocation against
//! a SHA the warning itself prints** — while failing closed would stop *every*
//! merge on a host whose daemon lags one release, including the overwhelming
//! majority that have no stacked children at all.
//!
//! It is also safe by construction rather than only by judgement: the
//! fail-CLOSED `verdict-contradiction` gate runs on the same binary ~230 lines
//! further down the same script, so a daemon too old for this verb cannot reach
//! the merge anyway. The fail-open here changes which refusal the operator sees,
//! not whether an unverified tree can merge.
//!
//! That asymmetry is also why a **forge read failure** is a skip rather than a
//! refusal, exactly as the retired shell's `|| echo '[]'` had it: no answer from
//! `gh pr list` is indistinguishable from "no children", and the population it
//! is wrong about is "repos with stacked PRs mid-flight", not "repos".

use std::path::Path;
use std::process::Command;

/// The ref namespace `reconcile-stack.sh` reads its parent-tip fallback from.
/// A plain ref (not `refs/worktree/…`) on purpose: every worktree of the repo
/// must see it, since the merge and the later rebase need not happen in the
/// same one.
pub const PIN_NAMESPACE: &str = "refs/loom/parent/";

/// One open child PR still targeting the parent branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    /// The child PR number.
    pub number: i64,
    /// The child PR's head branch, carried through to the post-merge reconcile.
    pub head_ref_name: String,
}

/// The guard's decision *before* any pin is attempted.
///
/// Split this way so the pure part is decidable without touching git: the
/// caller resolves [`Outcome::NeedsPin`] by calling [`establish_pin`] and then
/// [`pinned_message`] or [`blocked_message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not a stackable parent branch, or no open child PR targets it. No
    /// message, no ref, no record — byte-for-byte the retired no-op.
    Skip,
    /// `--allow-stacked-children`: the operator asserts the children are
    /// reconciled. Warns, and deliberately does **not** pin (see the flag's
    /// note on [`Outcome::NeedsPin`]).
    Bypass(String),
    /// `--dry-run`: report the would-be outcome, write nothing.
    DryRun(String),
    /// Open children exist and a pin must be attempted. The bypass returns
    /// above without reaching here, which is why `STACKED_CHILDREN_PIN_WRITTEN`
    /// must gate on the pin actually being written and not on children merely
    /// being found (the #8010 follow-up Judge caught).
    NeedsPin,
}

/// The strict, closed allow-list of recognized Builder branch prefixes:
/// `feature/issue-<N>` (worktree.sh's default) and `feature/harness-ops-<N>`
/// (2AMLogic/harness-ops's Builder convention). The ONE definition shared by
/// [`is_stackable_parent_branch`] (this module's pre-merge gate) and
/// [`super::reconcile::issue_from_branch`] (the post-merge plan and
/// child-issue derivation), so the two decisions can never disagree on which
/// parent branches are stackable (2AMLogic/2am#1298, #1396). Before #1298, a
/// `feature/harness-ops-<N>` parent wrote no `refs/loom/parent/<branch>` pin,
/// recorded no `STACKED_CHILDREN` snapshot, and was skipped by the post-merge
/// plan too, so every harness-ops parent merge silently stranded its open
/// children (harness-ops#283, #356).
///
/// Deliberately a short, closed allow-list — not a configurable
/// naming-convention system, and not widened to an unconditional `feature/`
/// prefix.
pub const STACKABLE_PARENT_PREFIXES: [&str; 2] = ["feature/issue-", "feature/harness-ops-"];

/// `^feature/(issue|harness-ops)-([0-9]+)$` — see [`STACKABLE_PARENT_PREFIXES`].
/// Only a parent PR on a recognized Builder branch can have Loom-style stacked
/// children. Still strict/anchored: `feature/issue-100-extra`,
/// `feature/issue-100/sub`, `feature/harness-ops-350-extra`,
/// `feature/harness-ops-350/sub`, and `feature/other-350` do not match.
#[must_use]
pub fn is_stackable_parent_branch(branch: &str) -> bool {
    for prefix in STACKABLE_PARENT_PREFIXES {
        if let Some(rest) = branch.strip_prefix(prefix) {
            return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
        }
    }
    false
}

/// The pin ref for `branch`, e.g. `refs/loom/parent/feature/issue-100`.
#[must_use]
pub fn pin_ref(branch: &str) -> String {
    format!("{PIN_NAMESPACE}{branch}")
}

/// Parse `gh pr list --json number,headRefName` output into children.
///
/// Lenient in exactly the direction the retired shell was: every read there was
/// `jq … 2>/dev/null || echo 0` / `|| echo ''`, so output jq could not make
/// sense of meant "no children" and the guard skipped. Preserved — see the
/// module's fail-open argument.
///
/// A row whose `number` is a numeric *string* is accepted, because the retired
/// `(.number|tostring)` rendered one into a working command and dropping it
/// would lose a real child.
///
/// A row with no usable `number` at all is dropped, and that **is** a divergence
/// from the retired shell, which counted it and rendered it as the literal
/// `null` — `#null` in the child list, `reconcile-stack.sh null <branch>` as the
/// operator's paste-ready command. It is recorded and asserted in
/// `tests/merge_pr_stacked_children_differential.rs`. The direction is the one
/// this guard already accepts everywhere else: `gh pr list --json number`
/// cannot return a null `number` (non-nullable in the schema), so reaching it
/// means the response is not one the guard can read — the same trigger class as
/// the `|| echo '[]'` read failure, with the same bounded cost of one manual
/// `reconcile-stack.sh`. Shipping an unusable `null` command was not the safer
/// alternative; it was the same loss with worse output.
#[must_use]
pub fn parse_children(json: &str) -> Vec<Child> {
    let Ok(serde_json::Value::Array(rows)) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let raw = row.get("number")?;
            let number = raw
                .as_i64()
                .or_else(|| raw.as_str().and_then(|s| s.parse::<i64>().ok()))?;
            let head_ref_name = row
                .get("headRefName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(Child {
                number,
                head_ref_name,
            })
        })
        .collect()
}

/// Re-serialize the children as the compact array later shell steps read
/// (`STACKED_CHILDREN_JSON`, consumed with `jq 'length'` and
/// `jq -r '.[] | "\(.number)\t\(.headRefName)"'`).
///
/// Re-serialized rather than passed through: `gh`'s own bytes may be
/// pretty-printed across lines, and the record protocol this verb speaks is
/// line-oriented. Only the two fields the consumer reads are carried, so the
/// record cannot smuggle an unbounded PR body into a `<<<` here-string.
#[must_use]
pub fn children_json(children: &[Child]) -> String {
    let rows: Vec<serde_json::Value> = children
        .iter()
        .map(|c| serde_json::json!({"number": c.number, "headRefName": c.head_ref_name}))
        .collect();
    serde_json::Value::Array(rows).to_string()
}

/// `#501, #502` — the operator-facing child list.
#[must_use]
pub fn child_list(children: &[Child]) -> String {
    children
        .iter()
        .map(|c| format!("#{}", c.number))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One ready-to-paste `reconcile-stack.sh` invocation per child, two-space
/// indented, reused verbatim by the warn and the block message so the operator
/// never has to assemble the command themselves.
#[must_use]
pub fn reconcile_commands(children: &[Child], parent_branch: &str) -> String {
    if children.is_empty() {
        // The retired `|| echo` fallback for a jq failure. Unreachable from
        // `decide`, which skips on an empty list, but kept so a caller that
        // renders a message for no children still emits a usable command shape
        // rather than an empty block.
        return format!("  ./.loom/scripts/reconcile-stack.sh <child-pr> {parent_branch}");
    }
    children
        .iter()
        .map(|c| format!("  ./.loom/scripts/reconcile-stack.sh {} {parent_branch}", c.number))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Everything the messages interpolate.
pub struct Inputs<'a> {
    /// `$PR_NUMBER` — the parent PR being merged.
    pub pr_number: &'a str,
    /// `$PR_BRANCH` — the parent branch children target.
    pub branch: &'a str,
    /// `$PR_HEAD_SHA` — the tip to pin.
    pub head_sha: &'a str,
    /// The open children discovered by a live forge query.
    pub children: &'a [Child],
    /// `--allow-stacked-children`.
    pub allow_stacked_children: bool,
    /// `--dry-run`: never mutate a local ref.
    pub dry_run: bool,
}

/// The guard's decision up to (not including) the pin attempt.
#[must_use]
pub fn decide(inputs: &Inputs<'_>) -> Outcome {
    if !is_stackable_parent_branch(inputs.branch) || inputs.children.is_empty() {
        return Outcome::Skip;
    }
    let count = inputs.children.len();
    let list = child_list(inputs.children);

    // Checked BEFORE the dry-run branch, as the retired shell had it: the
    // bypass is the operator's assertion about the world, and reporting the
    // pin-or-block path under --dry-run when the operator has already opted out
    // of it would describe a run that will not happen.
    if inputs.allow_stacked_children {
        return Outcome::Bypass(format!(
            "Merge-ordering guard: --allow-stacked-children set; proceeding despite {count} open stacked child PR(s) ({list}) targeting '{}' (operator asserts they are reconciled)",
            inputs.branch
        ));
    }

    if inputs.dry_run {
        return Outcome::DryRun(format!(
            "[dry-run] {count} open stacked child PR(s) ({list}) still target '{}'. A real run would pin the parent tip to {} and proceed with a warning naming each child, or hard-block if the tip could not be pinned. No ref was written.",
            inputs.branch,
            pin_ref(inputs.branch)
        ));
    }

    Outcome::NeedsPin
}

/// The warning for a pin that was written: the merge proceeds, and every child
/// gets its unblock command.
#[must_use]
pub fn pinned_message(inputs: &Inputs<'_>) -> String {
    format!(
        "Merge-ordering guard: PR #{}'s branch '{}' still has {} open stacked child PR(s) ({}) targeting it. Pinned the parent tip to {} ({}) so reconcile-stack.sh can still resolve '{}' after the merge deletes it (#3747 item 2, #7982). Proceeding with the merge — reconcile each child once this has landed:\n{}",
        inputs.pr_number,
        inputs.branch,
        inputs.children.len(),
        child_list(inputs.children),
        pin_ref(inputs.branch),
        inputs.head_sha,
        inputs.branch,
        reconcile_commands(inputs.children, inputs.branch),
    )
}

/// The refusal for a tip that could not be pinned — the one case where #3747's
/// original race is still reachable.
#[must_use]
pub fn blocked_message(inputs: &Inputs<'_>) -> String {
    format!(
        "Merge blocked: PR #{}'s branch '{}' still has {} open stacked child PR(s) ({}) targeting it, and its tip ({}) could not be pinned to {} — a detached or unreadable parent. Merging now would race the repo's delete_branch_on_merge setting: GitHub deletes '{}' synchronously during the merge, before the child PR(s) can be rebased/retargeted onto the default branch — leaving reconcile-stack.sh's rebase unable to resolve the parent branch ref (#3747 item 2). Reconcile each child first (from a clean checkout), then re-run this merge:\n{}\nOr, if you have already verified/reconciled them, re-run with --allow-stacked-children to bypass this guard.",
        inputs.pr_number,
        inputs.branch,
        inputs.children.len(),
        child_list(inputs.children),
        inputs.head_sha,
        pin_ref(inputs.branch),
        inputs.branch,
        reconcile_commands(inputs.children, inputs.branch),
    )
}

/// `gh pr list --repo <repo> --base <branch> --state open --json
/// number,headRefName`, as raw stdout.
///
/// A **live** forge query, never the ephemeral daemon `SweepRegistry`: terminal
/// registry entries are garbage-collected ~1h after transition and the registry
/// only exists while `loom-daemon` is running, but this guard also runs from
/// Champion's cron and from an interactive `/loom:sweep` merge. Uncached `gh`,
/// so the answer is "as of right now".
///
/// Returns `[]` on any failure — see the module's fail-open argument.
#[must_use]
pub fn discover_open_children(gh: &str, repo: &str, branch: &str) -> String {
    let out = Command::new(gh)
        .args(["pr", "list", "--repo", repo, "--base", branch, "--state"])
        .args(["open", "--json", "number,headRefName"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => "[]".to_string(),
    }
}

/// Establish the postcondition `reconcile-stack.sh` needs: `head_sha` readable
/// from `repo_root` **and** named by the pin ref.
///
/// The object check is not ceremony — a ref pointing at an object this repo does
/// not have satisfies "did we write a ref" while being useless to a later
/// rebase. So: verify the object locally, fetch the branch once if it is absent,
/// re-verify, then write. All three failing means a detached or unreadable
/// parent, which is the one case the guard still refuses on.
///
/// One deliberate simplification of the retired `{ cat-file || fetch; } &&
/// cat-file && update-ref` chain: there, a FAILING `fetch` short-circuited the
/// whole chain, so the second `cat-file` never ran and a block was declared
/// without re-asking whether the object had arrived. Here only the re-verify
/// decides, and the fetch's own exit status is ignored. The two answers can
/// differ on exactly one state — `fetch` reports failure yet the object is
/// present afterwards, which needs a concurrent writer of the same object store
/// — and there the port's answer is the correct one: the object is readable, so
/// the pin is usable and refusing the merge would be refusing on a
/// postcondition that in fact holds.
///
/// `true` iff the ref now names the commit.
pub fn establish_pin(repo_root: &Path, branch: &str, head_sha: &str) -> bool {
    if !has_commit(repo_root, head_sha) {
        // Best-effort single fetch, exactly as the retired shell's
        // `git fetch --quiet origin "$PR_BRANCH"`: its failure is not
        // interesting on its own, only whether the object is present after it.
        let _ = git(repo_root, &["fetch", "--quiet", "origin", branch]);
        if !has_commit(repo_root, head_sha) {
            return false;
        }
    }
    git(repo_root, &["update-ref", &pin_ref(branch), head_sha])
}

fn has_commit(repo_root: &Path, sha: &str) -> bool {
    git(repo_root, &["cat-file", "-e", &format!("{sha}^{{commit}}")])
}

fn git(repo_root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests;
