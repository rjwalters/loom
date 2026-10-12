//! Resolving a stale `loom:blocked` on a starred issue (#10151).
//!
//! Star-liveness used to send every starred `loom:blocked` issue with no
//! **open** named blocker straight to the operator (`blocked-unnamed`), even
//! when every blocker it cited had closed and the block was simply stale.
//! 2AMLogic/2am#2127 was escalated one minute after its last cited blocker
//! merged and then sat blocked for ~16.5h until a human removed the label;
//! an operator session later hand-fixed 22 of 24 such starred blocks.
//!
//! The classifier ([`super::landing`]) now lands those rows on `stale-block`
//! with a [`StaleAction`], and this module carries it out:
//!
//! - [`StaleAction::Unblock`] — every cited blocker is closed: remove
//!   `loom:blocked` and post one comment naming the closed blockers. The issue
//!   re-enters its normal lane: `ready` when it already carries `loom:issue`,
//!   otherwise Curator — its starred queue for a starred issue (Curator
//!   Priority 0 skips only `loom:blocked` and the claim/park labels), its
//!   usual queue for an inherited row, which carries no star label.
//! - [`StaleAction::CuratorHandoff`] — nothing is cited anywhere: remove
//!   `loom:issue` (so a Builder cannot take an issue whose block may be real
//!   but undocumented) and then `loom:blocked`, and post a handoff comment.
//!   Curator's starred queue picks it up and either names the blocker and
//!   re-blocks it (it then lands `blocked-by`), re-blocks it with none named
//!   (now the operator is asked: a Curator pass failed to name a blocker), or
//!   curates and re-promotes it (the star is the approval).
//!
//! # Starred rows only hand off
//!
//! The handoff is for a **directly starred** issue only. An inherited row (a
//! blocker or child the walk reached, carrying no `loom:operator-priority`)
//! is never handed off: Curator's Priority 0 query is label-based, so it would
//! never reach the starred queue the comment promises, and Curator may not
//! re-add `loom:issue` to an unstarred issue — the handoff would just withdraw
//! a human's approval from an issue nobody starred. The walk turns such a row
//! back into the `blocked-unnamed` operator ask instead
//! ([`super::landing::withhold_inherited_handoff`]), and [`apply`] refuses
//! the handoff for it as a backstop. An inherited row may still be
//! **unblocked** (that only removes a `loom:blocked` every cited blocker of
//! which is closed), and its comment says it inherits the star rather than
//! calling it starred.
//!
//! # Never touched
//!
//! `loom:building` and `loom:operator-priority` are never added or removed.
//! A row a Builder or an open PR holds gets no handoff, and a stale block
//! whose own PR is parked gets no unblock (the classifier leaves
//! [`super::landing::Landing::stale`] empty for both).
//!
//! # Idempotence
//!
//! The label writes come first and are no-ops when repeated; the comment is
//! posted only when no trusted comment already carries its marker, so two
//! hosts racing one pass cost at most one duplicate. The markers are also
//! the gate's memory: once present, a repeat of the same stale block is an
//! operator ask, never a second unblock (see [`super::landing`]).
//!
//! `loom:blocked` is always the **last** label removed, because removing it is
//! what ends the pass's retries (rule 5 no longer matches). Any failure before
//! it leaves the issue blocked, so the next pass rebuilds the same action and
//! finishes it; the handoff's `loom:issue` removal comes first so a failure
//! never leaves the issue unblocked **and** still approved.

use anyhow::Result;

use super::forge::{ForgeComment, StarForge};
use super::landing::{StaleAction, BLOCKED_LABEL};

/// The marker of the Curator handoff comment.
pub const HANDOFF_MARKER: &str = "<!-- loom:operator-priority-curator-handoff -->";
/// The marker prefix of an unblock comment; the key is the cleared set.
pub const UNBLOCKED_PREFIX: &str = "<!-- loom:operator-priority-unblocked key=";
/// The approval label the handoff withdraws (Curator re-promotes a star).
pub const ISSUE_LABEL: &str = "loom:issue";

/// The marker for an unblock over the cleared set `key` (`#5,#6`).
#[must_use]
pub fn unblocked_marker(key: &str) -> String {
    format!("{UNBLOCKED_PREFIX}{key} -->")
}

/// Whether `body` is one of the liveness pass's own comments (or a loom-ui
/// intent audit): never read for blocker references.
#[must_use]
pub fn is_liveness_comment(body: &str) -> bool {
    body.contains(super::escalate::MARKER_PREFIX)
        || body.contains(HANDOFF_MARKER)
        || body.contains(UNBLOCKED_PREFIX)
        || body.contains(super::intents::INTENT_MARKER_PREFIX)
}

/// What a blocked issue's comments say, from trusted authors only
/// ([`super::trust`]): an outsider can neither name a blocker (which would
/// inherit the star) nor forge a marker (which would skip a step).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommentFacts {
    /// Bodies to read blocker references from (the pass's own comments
    /// excluded).
    pub bodies: Vec<String>,
    /// A handoff marker is present.
    pub handoff: bool,
    /// The keys of the unblock markers present.
    pub unblocked: Vec<String>,
}

/// Read `comments` into [`CommentFacts`].
#[must_use]
pub fn comment_facts(comments: &[ForgeComment], self_login: Option<&str>) -> CommentFacts {
    let mut out = CommentFacts::default();
    for c in comments
        .iter()
        .filter(|c| super::trust::trusted(c, self_login))
    {
        let body = c.body.as_str();
        if body.contains(HANDOFF_MARKER) {
            out.handoff = true;
        }
        if let Some(rest) = body.split(UNBLOCKED_PREFIX).nth(1) {
            if let Some((key, _)) = rest.split_once(" -->") {
                out.unblocked.push(key.trim().to_string());
            }
        }
        if !is_liveness_comment(body) {
            out.bodies.push(c.body.clone());
        }
    }
    out
}

/// The unblock comment. `inherited_via` is the parent an inherited row
/// takes its star through (`None` for a directly starred issue): an
/// inherited row is never called starred, nor promised the starred queue.
#[must_use]
pub fn unblock_comment(
    cleared: &[String],
    key: &str,
    host: &str,
    approved: bool,
    inherited_via: Option<u32>,
) -> String {
    let next = match (approved, inherited_via) {
        (true, _) => "It keeps `loom:issue`, so it is ready for a Builder again.",
        (false, None) => "Curator re-checks it next (starred issues come first) and promotes it.",
        (false, Some(_)) => "It goes back through Curator's usual queue.",
    };
    let what = inherited_via.map_or_else(
        || "this starred issue".to_string(),
        |via| format!("this issue (not starred itself; it inherits the star through #{via})"),
    );
    format!(
        "{}\n**Unblocked** — {what} carried `loom:blocked`, but every blocker it \
         cites is now closed ({}), so the liveness check removed the label. {next}\n\n\
         If something else still holds it, name that blocker in the body and re-apply \
         `loom:blocked`; the check will then ask the operator instead of unblocking it again.\n\n\
         <sub>Posted once by the loom-daemon liveness check on host `{host}` (#10151).</sub>",
        unblocked_marker(key),
        cleared.join(", ")
    )
}

/// The Curator handoff comment.
#[must_use]
pub fn handoff_comment(host: &str) -> String {
    format!(
        "{HANDOFF_MARKER}\n**Handed to Curator** — this starred issue carried `loom:blocked` \
         but names no blocking issue in its body or comments, so nothing would ever clear it. \
         The liveness check removed `loom:blocked` (and `loom:issue`, so no Builder takes it \
         before Curator looks) to put it in Curator's starred queue.\n\n\
         **Curator:** if it is really blocked, name the blocker in the body (a `Blocked by` \
         line or an unchecked `## Dependencies` item) and re-apply `loom:blocked`; otherwise \
         curate and promote it as usual. If it must stay blocked with no nameable blocker, \
         re-apply `loom:blocked` and the operator is asked.\n\n\
         <sub>Posted once by the loom-daemon liveness check on host `{host}` (#10151).</sub>"
    )
}

/// Carry out `action` on `issue` (whose current labels are `labels`).
/// `inherited_via` is `Some(parent)` for an inherited row, which is never
/// handed off (see the module doc): a [`StaleAction::CuratorHandoff`] for one
/// is refused with no write.
///
/// # Errors
/// A forge read or write failed. What the next pass does depends on which:
/// - A label write failed: `loom:blocked` is removed last, so the issue is
///   still blocked, the next pass rebuilds the same action and retries. A
///   handoff that removed `loom:issue` and then failed on `loom:blocked`
///   leaves the issue blocked and unapproved — never unblocked and approved.
/// - Every label write succeeded but reading or posting the comment failed:
///   **no retry** — `loom:blocked` is gone, so rule 5 no longer matches and
///   the action is never rebuilt. The issue is already in its intended lane;
///   only the explanation and the marker (the gate's memory) are missing, so
///   if it is re-blocked on the same evidence the pass treats it as new and
///   runs at most one extra unblock or handoff cycle before it asks.
pub fn apply(
    forge: &mut dyn StarForge,
    issue: u32,
    labels: &[String],
    action: &StaleAction,
    host: &str,
    inherited_via: Option<u32>,
) -> Result<()> {
    let has = |l: &str| labels.iter().any(|x| x == l);
    let (marker, body) = match action {
        StaleAction::Unblock { cleared, key } => {
            forge.remove_label(issue, BLOCKED_LABEL)?;
            let body = unblock_comment(cleared, key, host, has(ISSUE_LABEL), inherited_via);
            (unblocked_marker(key), body)
        }
        StaleAction::CuratorHandoff => {
            if inherited_via.is_some() {
                log::warn!(
                    "star_liveness: refusing a Curator handoff for inherited #{issue}; \
                     only a starred issue is handed off"
                );
                return Ok(());
            }
            // `loom:issue` first: a failure between the two writes must leave
            // the issue blocked, not unblocked and still approved.
            if has(ISSUE_LABEL) {
                forge.remove_label(issue, ISSUE_LABEL)?;
            }
            forge.remove_label(issue, BLOCKED_LABEL)?;
            (HANDOFF_MARKER.to_string(), handoff_comment(host))
        }
    };
    let comments = forge.comments(issue)?;
    let me = forge.self_login();
    let posted = comments
        .iter()
        .any(|c| c.body.contains(&marker) && super::trust::trusted(c, me.as_deref()));
    if !posted {
        forge.post_comment(issue, &body)?;
    }
    Ok(())
}
