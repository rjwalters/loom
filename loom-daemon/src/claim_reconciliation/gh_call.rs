//! The one way the claim-reconciliation pass family spawns `gh` (#10089).
//!
//! Every site here used to hand-build a `Command::new(gh_bin)`, so none of
//! its calls reached [`crate::forge_call_stats`] and the breaker booked them
//! as "external". Routing them through [`GhInvocation`] counts each under a
//! stable `claim.*` / `verdict.*` / `sequence.*` operation name, and gives
//! every call a deadline (they were unbounded `.output()`s).
//!
//! The facade supplies what each site applied by hand: the root's
//! cross-owner `GH_CONFIG_DIR` (from the working directory) and `LOOM_REPO`
//! as `GH_REPO` (the #8263 contract for `gh api`). `gh issue|pr` sites keep
//! their `--repo` flag too ([`loom_repo_flag`]).

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::cmd_out::CmdOutcome;
use crate::forge_call_stats::{ops, ForgeOp};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Per-call deadline. Generous: a paginated comments walk on a long PR is
/// several requests.
pub(crate) const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// A facade invocation running `gh_bin` (a test stub, or bare `gh` in
/// production — resolved through the facade's ladder) in `root`.
pub(crate) fn inv(
    op: &'static str,
    intent: AccessIntent,
    gh_bin: &Path,
    root: &Path,
) -> GhInvocation {
    build(op, intent, gh_bin, root, GH_TIMEOUT)
}

/// [`inv`] with its own deadline instead of [`GH_TIMEOUT`].
pub(crate) fn read_within(
    op: &'static str,
    gh_bin: &Path,
    root: &Path,
    timeout: Duration,
) -> GhInvocation {
    build(op, AccessIntent::Read, gh_bin, root, timeout)
}

fn build(
    op: &'static str,
    intent: AccessIntent,
    gh_bin: &Path,
    root: &Path,
    timeout: Duration,
) -> GhInvocation {
    let inv = GhInvocation::new(Operation::new(op), intent, GhTarget::None, timeout)
        .program(gh_bin)
        .current_dir(root);
    match forge_op_for(op) {
        Some(fop) => inv.forge_op(fop),
        None => inv,
    }
}

/// The inventoried forge operation (#9831) each telemetry name built through
/// this module — or the sweep registry's `gh_inv` — serves, so its
/// `forge_call_stats` row is not `unknown`. One table for both families keeps
/// a name's mapping in exactly one place.
///
/// `None` is deliberate, not debt, for: GraphQL issue views (`claim.labels`,
/// `claim.issue_labels`, `claim.issue_state`, `model.issue_body*`) and
/// PR-by-label listings (`claim.pr_list_claimed`, `verdict.pr_list`,
/// `*.pr_list`, `snapshot.*`) — the inventory has no single-issue-view or
/// PR-list-by-label row — plus `star.api` (a generic `gh api` passthrough).
/// `heal.issue_view` is absent because it is an ETag'd REST read that names
/// its own `issue.view-state` op (#10507); the `*.pr_view` dispatch arms are
/// gone (#10507).
#[must_use]
pub(crate) fn forge_op_for(op: &str) -> Option<ForgeOp> {
    Some(match op {
        "claim.pr_timeline"
        | "quarantine.issue_timeline"
        | "guard.open_pr_timeline"
        | "outcome.label_timeline"
        | "restore.label_timeline"
        | "sequence.label_timeline" => ops::TIMELINE_READ,
        "guard.open_pr_graphql" => ops::PR_CLOSING_ISSUE_REFERENCES,
        "claim.lease_comments"
        | "guard.lease_comments"
        | "claim.pr_activity_comments"
        | "verdict.pr_comments"
        | "quarantine.issue_comments"
        | "sequence.trusted_bodies"
        | "chain_lock.comments"
        | "roster.comments" => ops::COMMENT_LIST,
        "claim.pr_labels" | "sequence.predecessor" | "chain_lock.pr_base" => ops::PR_VIEW_STATE,
        "park_hold.issue_view" => ops::ISSUE_VIEW_STATE,
        "park_hold.issue_body" => ops::ISSUE_EDIT_BODY,
        "intake.list_open" | "quarantine.issue_list" => ops::ISSUE_LIST,
        "claim.issue_reclaim"
        | "claim.pr_add_label"
        | "claim.pr_reclaim"
        | "quarantine.issue_release"
        | "heal.issue_add_label"
        | "intake.add_triage"
        | "intake.remove_triage"
        | "verdict.clear_labels"
        | "verdict.requeue_unanchored"
        | "sequence.pr_edit"
        | "review_conflict.pr_edit"
        | "guard.flip_building"
        | "quarantine.label"
        | "quarantine.release"
        | "restore.label"
        | "prless.hold_label"
        | "park_hold.issue_labels" => ops::ISSUE_EDIT_LABELS,
        "verdict.anchor_comment"
        | "verdict.reanchor_comment"
        | "verdict.stale_comment"
        | "verdict.requeue_unanchored_comment"
        | "sequence.pr_comment"
        | "review_conflict.pr_comment"
        | "guard.lease_comment"
        | "guard.lease_yield_comment"
        | "quarantine.comment"
        | "watchdog.gaveup_comment"
        | "watchdog.stale_comment"
        | "outcome.writeback_comment"
        | "prless.comment" => ops::COMMENT_CREATE,
        "roster.delete" | "roster.patch" | "sequence.comment_patch" | "sequence.comment_delete" => {
            ops::COMMENT_EDIT_DELETE
        }
        _ => return None,
    })
}

/// A read ([`AccessIntent::Read`]) — the common case.
pub(crate) fn read(op: &'static str, gh_bin: &Path, root: &Path) -> GhInvocation {
    inv(op, AccessIntent::Read, gh_bin, root)
}

/// A read pinned to the writer: a read-back of a forge fact this daemon just
/// wrote itself, where a reader App could still lag the write (W4-C).
pub(crate) fn read_own_write(op: &'static str, gh_bin: &Path, root: &Path) -> GhInvocation {
    read(op, gh_bin, root).writer_identity()
}

/// A write ([`AccessIntent::Write`]).
pub(crate) fn write(op: &'static str, gh_bin: &Path, root: &Path) -> GhInvocation {
    inv(op, AccessIntent::Write, gh_bin, root)
}

/// `["--repo", $LOOM_REPO]` when the override is set, else nothing — for
/// `gh issue|pr` only (`gh api` has no `--repo`, #8263).
pub(crate) fn loom_repo_flag() -> Vec<String> {
    std::env::var("LOOM_REPO")
        .map(|repo| vec!["--repo".to_string(), repo])
        .unwrap_or_default()
}

/// Run `inv`: the process's output when `gh` ran (any exit status), an
/// error when it could not be started, collected, or outlived its deadline.
pub(crate) fn output(inv: GhInvocation) -> Result<Output> {
    let op = inv.operation().as_str();
    match inv.run() {
        CmdOutcome::Ran(out) => Ok(out),
        CmdOutcome::Unavailable(u) => Err(anyhow!("failed to invoke gh ({op}): {u}")),
    }
}

/// [`output`], `None` on any failure or non-zero exit — the fail-open shape
/// of the best-effort probes.
pub(crate) fn ok_stdout(inv: GhInvocation) -> Option<Vec<u8>> {
    output(inv)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
}

/// The trimmed stderr of a failed run, for error messages.
pub(crate) fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}
