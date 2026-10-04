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
//!   otherwise Curator's starred queue (Curator Priority 0 skips only
//!   `loom:blocked` and the claim/park labels), which re-checks and promotes.
//! - [`StaleAction::CuratorHandoff`] — nothing is cited anywhere: remove
//!   `loom:blocked` **and** `loom:issue` (so a Builder cannot take an issue
//!   whose block may be real but undocumented) and post a handoff comment.
//!   Curator's starred queue picks it up and either names the blocker and
//!   re-blocks it (it then lands `blocked-by`), re-blocks it with none named
//!   (now the operator is asked: a Curator pass failed to name a blocker), or
//!   curates and re-promotes it (the star is the approval).
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
//! The label write is first and is a no-op when repeated; the comment is
//! posted only when no trusted comment already carries its marker, so two
//! hosts racing one pass cost at most one duplicate. The markers are also
//! the gate's memory: once present, a repeat of the same stale block is an
//! operator ask, never a second unblock (see [`super::landing`]).

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

/// The unblock comment.
#[must_use]
pub fn unblock_comment(cleared: &[String], key: &str, host: &str, approved: bool) -> String {
    let next = if approved {
        "It keeps `loom:issue`, so it is ready for a Builder again."
    } else {
        "Curator re-checks it next (starred issues come first) and promotes it."
    };
    format!(
        "{}\n**Unblocked** — this starred issue carried `loom:blocked`, but every blocker it \
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
///
/// # Errors
/// A forge read or write failed; the next pass retries (the issue is still
/// `loom:blocked` if the label write failed, and the comment is deduped by
/// its marker if only the comment failed).
pub fn apply(
    forge: &mut dyn StarForge,
    issue: u32,
    labels: &[String],
    action: &StaleAction,
    host: &str,
) -> Result<()> {
    let has = |l: &str| labels.iter().any(|x| x == l);
    forge.remove_label(issue, BLOCKED_LABEL)?;
    let (marker, body) = match action {
        StaleAction::Unblock { cleared, key } => {
            (unblocked_marker(key), unblock_comment(cleared, key, host, has(ISSUE_LABEL)))
        }
        StaleAction::CuratorHandoff => {
            if has(ISSUE_LABEL) {
                forge.remove_label(issue, ISSUE_LABEL)?;
            }
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
