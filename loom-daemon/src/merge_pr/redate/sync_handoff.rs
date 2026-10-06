//! The automated base-sync handoff for an exhausted re-date budget (#10388,
//! addendum B3).
//!
//! When a re-date chain has spent its whole budget and the #8248 guard still
//! blocks, the old behaviour was an immediate `loom:operator` hold. That parks
//! a Judge-approved PR on a human although nothing in it needs a decision: the
//! base simply moved faster than CI. Before escalating, the remedy now spends
//! up to `champion.redateSyncHandoffs` (default 1; env
//! `LOOM_REDATE_SYNC_HANDOFFS`, env > config > default) **base syncs** per PR:
//!
//! 1. `PUT /repos/{nwo}/pulls/{n}/update-branch` (the same call `merge-pr.sh`'s
//!    `forge_update_branch` makes). It is a real, non-tree-identical merge
//!    commit, so it starts a fresh re-date chain, and the Judge verdict carries
//!    through `verdict_equivalence`'s `clean-merge-of-base`. No LLM session.
//! 2. If the forge reports a merge conflict, the PR gets `loom:merge-conflict`
//!    — Doctor's Priority-1 queue performs the real rebase. Not
//!    `loom:operator`, and never `loom:changes-requested` (that label is
//!    Judge's verdict, addendum A4).
//! 3. Only when the handoffs are spent and the chain exhausts again does the
//!    unchanged `escalate()` run.
//!
//! Handoff state is a trusted-author marker (#9548) read from the listing the
//! caller already filtered; this module does no comment fetch of its own.

use serde_json::Value;
use std::path::Path;

use super::{gh_api_body, gh_api_with, post_comment, read_nonempty, RemedyOutcome};

/// Default sync handoffs per PR.
pub const DEFAULT_HANDOFFS: u32 = 1;
/// Upper clamp: the bound is the point, a typo must not make it meaningless.
pub const MAX_HANDOFFS: u32 = 5;
/// Env override (beats config).
pub const HANDOFFS_ENV: &str = "LOOM_REDATE_SYNC_HANDOFFS";
/// Config key.
pub const HANDOFFS_CONFIG_KEY: &str = "champion.redateSyncHandoffs";
/// Doctor's Priority-1 queue label.
pub const CONFLICT_LABEL: &str = "loom:merge-conflict";

/// Resolved handoff allowance. `0` disables the handoff (today's behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncConfig {
    pub handoffs: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            handoffs: DEFAULT_HANDOFFS,
        }
    }
}

impl SyncConfig {
    /// env > config > default; an unparseable value at a tier falls through.
    /// Unlike the budget, `0` is valid (it turns the handoff off).
    #[must_use]
    pub fn resolve(env: Option<&str>, config: &Value) -> Self {
        let clamp = |n: u64| u32::try_from(n.min(u64::from(MAX_HANDOFFS))).unwrap_or(MAX_HANDOFFS);
        let handoffs = env
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or_else(|| {
                crate::config_resolver::get_path(config, HANDOFFS_CONFIG_KEY)
                    .and_then(Value::as_u64)
            })
            .map_or(DEFAULT_HANDOFFS, clamp);
        Self { handoffs }
    }

    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        let effective = crate::config_resolver::resolve_effective_config(root);
        Self::resolve(std::env::var(HANDOFFS_ENV).ok().as_deref(), &effective)
    }
}

const SYNC_PREFIX: &str = "<!-- loom:stale-check-sync head=";

/// The marker recorded after a successful sync: the head synced FROM and the
/// handoff's 1-based position for this PR.
#[must_use]
pub fn sync_marker(head: &str, n: u32) -> String {
    format!("{SYNC_PREFIX}{head} n={n} -->")
}

/// The once-per-head marker on the conflict notice.
#[must_use]
pub fn conflict_marker(head: &str) -> String {
    format!("<!-- loom:stale-check-sync-conflict head={head} -->")
}

/// Handoffs already spent on this PR: trusted markers across ALL heads (the
/// sync itself moves the head, so a per-head count would never accumulate).
/// `bodies` is the TRUSTED listing's bodies joined.
#[must_use]
pub fn handoffs_used(bodies: &str) -> u32 {
    u32::try_from(bodies.matches(SYNC_PREFIX).count()).unwrap_or(u32::MAX)
}

/// The comment recorded after a successful sync, carrying the per-PR cycle
/// count (addendum D).
#[must_use]
pub fn sync_comment_body(
    pr: &str,
    head: &str,
    n: u32,
    max: u32,
    spent: u32,
    budget: u32,
) -> String {
    let short = &head[..head.len().min(7)];
    format!(
        "{}\n**Automated base sync after re-date exhaustion (#10388), {n} of {max}**\n\n\
PR #{pr}'s #8248 required-check-freshness block survived {spent} of {budget} tree-identical \
re-dates at head `{short}`. Before holding it for a human, the base branch was merged into the \
PR branch (`update-branch`). That is a real merge commit, so it starts a fresh re-date chain, \
and the standing Judge verdict carries across a clean merge of the base. No review session was \
spent. This is sync {n} of {max} for this PR; if the chain exhausts again, it escalates to \
`{}`.",
        sync_marker(head, n),
        super::HOLD_LABEL
    )
}

/// The conflict notice.
#[must_use]
pub fn conflict_comment_body(pr: &str, head: &str) -> String {
    let short = &head[..head.len().min(7)];
    format!(
        "{}\n**Base sync hit a merge conflict (#10388)**\n\n\
PR #{pr} at head `{short}` exhausted its re-date budget, and merging the base branch into it \
conflicts. Applied `{CONFLICT_LABEL}` so Doctor rebases it; no `{}` hold is applied.",
        conflict_marker(head),
        super::HOLD_LABEL
    )
}

fn is_conflict(err: &str) -> bool {
    err.to_ascii_lowercase().contains("merge conflict")
}

/// Run the handoff. `None` means the sync could not be attempted or failed for
/// a reason that is not a conflict; the caller then falls back to today's
/// `escalate()` so the PR is never left unflagged.
#[allow(clippy::too_many_arguments)]
pub(super) fn attempt(
    gh: &str,
    nwo: &str,
    branch: &str,
    pr: &str,
    head: &str,
    trusted_bodies: &str,
    used: u32,
    max: u32,
    spent: u32,
    budget: u32,
) -> Option<RemedyOutcome> {
    let expected = format!("expected_head_sha={head}");
    match gh_api_with(
        gh,
        &[
            "-X",
            "PUT",
            &format!("repos/{nwo}/pulls/{pr}/update-branch"),
            "-f",
            &expected,
        ],
    ) {
        Ok(_) => {
            let n = used + 1;
            let body = sync_comment_body(pr, head, n, max, spent, budget);
            if let Err(e) = post_comment(gh, nwo, pr, &body) {
                return Some(RemedyOutcome::Failed(format!(
                    "update-branch for PR #{pr} succeeded, but recording it failed ({e}); \
post {} by hand or a second sync may run",
                    sync_marker(head, n)
                )));
            }
            Some(RemedyOutcome::SyncedBase {
                head: head.to_string(),
                n,
                max,
            })
        }
        Err(e) if is_conflict(&e) => {
            if let Err(e) = gh_api_body(
                gh,
                &[&format!("repos/{nwo}/issues/{pr}/labels")],
                Some(&format!("{{\"labels\":[\"{CONFLICT_LABEL}\"]}}")),
            ) {
                return Some(RemedyOutcome::Failed(format!(
                    "could not apply {CONFLICT_LABEL} to PR #{pr}: {e}"
                )));
            }
            let notice_posted = !trusted_bodies.contains(&conflict_marker(head));
            if notice_posted {
                if let Err(e) = post_comment(gh, nwo, pr, &conflict_comment_body(pr, head)) {
                    return Some(RemedyOutcome::Failed(format!(
                        "applied {CONFLICT_LABEL} but could not post the notice: {e}"
                    )));
                }
            }
            Some(RemedyOutcome::SyncConflict { notice_posted })
        }
        Err(e) => {
            // The head was checked before the comment read, so a concurrent push can
            // make the forge reject our `expected_head_sha`. That is the
            // `HeadMoved` retry path (the new head gets a fresh chain), not an
            // exhausted budget: re-read the live head before falling back.
            match read_nonempty(
                gh,
                &[
                    &format!("repos/{nwo}/git/refs/heads/{branch}"),
                    "--jq",
                    ".object.sha",
                ],
                &format!("ref heads/{branch} resolved to an empty sha"),
            ) {
                Ok(current) if current != head => Some(RemedyOutcome::HeadMoved { current }),
                Ok(_) => {
                    eprintln!(
                        "Note: base sync for PR #{pr} failed ({e}); falling back to the operator hold (#10388)"
                    );
                    None
                }
                Err(re) => Some(RemedyOutcome::Failed(format!(
                    "base sync for PR #{pr} failed ({e}) and the head could not be re-read ({re})"
                ))),
            }
        }
    }
}

#[cfg(test)]
#[path = "sync_handoff_tests.rs"]
mod tests;
