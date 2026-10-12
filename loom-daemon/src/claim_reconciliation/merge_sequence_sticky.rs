//! Sequencing state that survives a tree-identical re-date (#10398).
//!
//! # The incident
//!
//! An operator removed `loom:sequenced` from an approved PR whose chain was
//! stuck. The freshness guard (#8248/#8919) then refused the merge, and its
//! documented remedy — a tree-identical re-date commit — moved the PR's head.
//! The next tick read the moved head as a re-plan trigger and put the label
//! back, so every re-date re-armed the hold the operator had just released
//! (one input to the #10163 livelock).
//!
//! # The two rules
//!
//! 1. **A tree-identical follower head move is not a moved pin.** When a
//!    holder's live head differs from the marker's `follower_head` but the
//!    forge proves the two commits carry byte-identical trees (the
//!    `kind=tree` verdict-equivalence test, [`crate::forge_tree_unchanged`] —
//!    reused, not copied), Phase 1 evaluates the hold as if the head had not
//!    moved: the existing hold, its `pred_head` and its plan stay, and no
//!    replan note or new "Landing order recorded" comment is written. The
//!    marker still names the tree it holds, which is the whole reason a moved
//!    head voids a hold.
//! 2. **An operator's release sticks.** When the newest `loom:sequenced` label
//!    event on a PR is a removal by a non-fleet actor, made after the comment
//!    that wrote the PR's newest marker, while that marker is still live (no
//!    pass tombstone after it), the pass records a
//!    sticky release for that (PR, predecessor) pair and plans no edge for
//!    the pair again until the PR's tree changes. A pass-made release always
//!    writes a tombstone and is never read as an operator release, so it
//!    keeps its existing behavior; an edge to a different predecessor (e.g.
//!    an ADR-0023 consolidation reservation) is unaffected.
//!
//! # Fail closed
//!
//! Every unknown keeps today's behavior: an unanswered or negative tree
//! comparison voids/re-plans exactly as before, and an unreadable label
//! history, an event with no actor, a fleet actor, or a removal not provably
//! newer than the live marker's comment records no sticky release. Nothing here weakens the `merge-pr.sh` gate
//! (`merge_pr::labels`): the label is still the only thing the merge path
//! reads, and this module only decides whether the pass puts it back.

use std::borrow::Cow;
use std::path::Path;

use super::{gh_pr, PredecessorState, SequenceEdge, SequenceMarker, SequencePr, SEQUENCE_LABEL};
use crate::claim_reconciliation::gh_call;
use crate::forge_tree_unchanged::{tree_unchanged, verdict_tree_carveout_enabled};
use crate::merge_pr::sequence::{
    html_comment_spans, is_full_sha, marker_text, parse, parse_live, states_or_ends_hold,
};

/// The record prefix: `<!-- loom:sequence operator-released after=N
/// follower_head=<40-hex> -->`. Not a marker and not a tombstone to
/// [`crate::merge_pr::sequence::parse`] / `parse_live`, so it can never be
/// read as a hold or end one.
pub const OPERATOR_RELEASE_PREFIX: &str = "loom:sequence operator-released";

// --- Rule 1: tree-identical follower head moves ---------------------------

/// Do `pinned` and `live` carry the same tree? Equal SHAs trivially do;
/// otherwise only a positive forge proof counts (kill switch:
/// `LOOM_VERDICT_TREE_CARVEOUT`, shared with the verdict carve-out).
pub fn forge_same_tree(gh_bin: &Path, root: &Path, pinned: &str, live: &str) -> bool {
    pinned == live
        || (verdict_tree_carveout_enabled()
            && tree_unchanged(gh_bin, Some(root), pinned, live) == Some(true))
}

/// Phase 1's view of a holder: when its live head moved off the marker's
/// `follower_head` by a tree-identical commit, the pinned head stands in for
/// the live one so every Phase-1 decision sees an unmoved follower. Any other
/// shape (unmoved, no head, a changed or unknown tree) is the PR as listed.
pub fn effective_follower<'a>(
    marker: &SequenceMarker,
    pr: &'a SequencePr,
    same_tree: impl FnOnce(&str, &str) -> bool,
) -> Cow<'a, SequencePr> {
    match pr.head_sha.as_deref() {
        Some(live)
            if !live.is_empty()
                && live != marker.follower_head
                && same_tree(&marker.follower_head, live) =>
        {
            log::info!(
                "claim_reconciliation (merge sequence): PR #{} moved {} -> {} with an identical \
                 tree (kind=tree) — hold evaluated at the pinned head, not voided (#10398)",
                pr.number,
                marker.follower_head,
                live
            );
            let mut held = pr.clone();
            held.head_sha = Some(marker.follower_head.clone());
            Cow::Owned(held)
        }
        _ => Cow::Borrowed(pr),
    }
}

/// #10465: the marker re-anchored to the predecessor's live head when that
/// head moved off `marker.pred_head` by a tree-identical commit (the same
/// `kind=tree` proof as the follower rule). `None` when nothing needs
/// re-anchoring or the move is not proven tree-identical (unanswered and
/// negative comparisons both keep today's void/replan). Only an OPEN
/// predecessor is re-anchored; a merged one is judged by [`super::evaluate`].
/// Covers hard (human) and `source=pass` markers alike.
pub fn reanchor_predecessor(
    marker: &SequenceMarker,
    pred: Option<&PredecessorState>,
    same_tree: impl FnOnce(&str, &str) -> bool,
) -> Option<SequenceMarker> {
    let pred = pred.filter(|p| p.open && !p.merged)?;
    let live = pred.head_sha.as_deref().filter(|h| !h.is_empty())?;
    if live == marker.pred_head || !same_tree(&marker.pred_head, live) {
        return None;
    }
    log::info!(
        "claim_reconciliation (merge sequence): predecessor #{} moved {} -> {} with an identical          tree (kind=tree) — hold re-anchored, not voided (#10465)",
        marker.after,
        marker.pred_head,
        live
    );
    Some(SequenceMarker {
        pred_head: live.to_string(),
        ..marker.clone()
    })
}

/// Post the re-anchored marker as the follower's newest marker comment (the
/// newest trusted marker wins, so the hold now pins the live head). No
/// "voided/replanned" note and the label stays. Idempotent: an identical
/// marker line already present suppresses the repeat.
pub(super) fn record_reanchor(
    gh_bin: &Path,
    root: &Path,
    follower: u32,
    marker: &SequenceMarker,
) -> anyhow::Result<()> {
    let body = format!(
        "{}\n**Landing order kept** — #{} was re-dated with a tree-identical commit, so the \
         recorded order after it still holds. The marker now pins its new head.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#10465)*",
        marker_text(marker),
        marker.after
    );
    gh_pr(gh_bin, root, &["comment", &follower.to_string(), "--body", &body])?;
    Ok(())
}

// --- Rule 2: sticky operator releases --------------------------------------

/// The record line for a sticky release of (follower, `after`) at the tree of
/// `follower_head`.
#[must_use]
pub fn record_text(after: u32, follower_head: &str) -> String {
    format!("<!-- {OPERATOR_RELEASE_PREFIX} after={after} follower_head={follower_head} -->")
}

fn parse_record(span: &str) -> Option<(u32, String)> {
    let rest = span.trim().strip_prefix(OPERATOR_RELEASE_PREFIX)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let (mut after, mut head) = (None, None);
    for field in rest.split_whitespace() {
        match field.split_once('=')? {
            ("after", v) => after = v.parse::<u32>().ok().filter(|n| *n > 0),
            ("follower_head", v) => head = is_full_sha(v).then(|| v.to_string()),
            _ => return None,
        }
    }
    Some((after?, head?))
}

/// The newest sticky-release record still in force: `(after, follower_head)`.
/// A later sequencing marker or `released`/`replanned` tombstone supersedes
/// it — the history moved on, so the record no longer describes it.
#[must_use]
pub fn parse_record_in_force(bodies: &[String]) -> Option<(u32, String)> {
    let mut record = None;
    for body in bodies {
        for line in body.lines() {
            for span in html_comment_spans(line) {
                if let Some(r) = parse_record(span) {
                    record = Some(r);
                } else if states_or_ends_hold(span) {
                    record = None;
                }
            }
        }
    }
    record
}

/// Does `body` carry `marker` in one of its single-line HTML comments?
fn carries_marker(body: &str, marker: &SequenceMarker) -> bool {
    body.lines().any(|line| {
        html_comment_spans(line)
            .into_iter()
            .any(|span| parse(&[format!("<!--{span}-->")]).as_ref() == Some(marker))
    })
}

fn timestamp(v: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(v.pointer("/created_at")?.as_str()?).ok()
}

/// When a timeline comment last took its current body: the later of its
/// `created_at` and `updated_at`. The #10634 landing upsert PATCHes a new
/// marker into an existing comment, so `created_at` alone back-dates it
/// (Judge, PR #10651). An absent `updated_at` (or JSON `null`) means never
/// edited; a present but unparseable one is an unknown (`None`).
fn comment_timestamp(v: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let created = timestamp(v)?;
    match v.pointer("/updated_at") {
        None | Some(serde_json::Value::Null) => Some(created),
        Some(u) => {
            let updated = chrono::DateTime::parse_from_rfc3339(u.as_str()?).ok()?;
            Some(created.max(updated))
        }
    }
}

/// Is the newest `loom:sequenced` label event in `events` (a paginated
/// issue-timeline body) a removal by an actor `is_fleet` does not claim,
/// made AFTER the comment that wrote `marker`?
///
/// A carrying comment is dated by the later of its `created_at` and
/// `updated_at`: the landing upsert (#10634) edits a marker into an existing
/// comment in place, and the marker is only as old as that edit.
///
/// `Some(false)` for no such event, a newest `labeled`, a fleet actor, or a
/// removal no newer than the marker comment (it released an earlier hold,
/// not this one: e.g. the pass wrote this marker but its `--add-label`
/// failed, so the newest label event is a stale removal). `None` when the
/// body does not parse, the newest event names no actor, or either timestamp
/// is missing or unparseable (including a marker comment absent from the
/// timeline) — unknown never makes a release sticky. When several timeline
/// comments carry the marker the newest one counts; the timeline is not
/// trust-filtered, so a copied marker can only move the bar later.
pub fn operator_unlabeled(
    events: &[u8],
    marker: &SequenceMarker,
    is_fleet: impl Fn(&str) -> bool,
) -> Option<bool> {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for page in serde_json::Deserializer::from_slice(events).into_iter::<serde_json::Value>() {
        match page.ok()? {
            serde_json::Value::Array(items) => rows.extend(items),
            _ => return None,
        }
    }
    let str_at =
        |v: &serde_json::Value, p: &str| v.pointer(p).and_then(|s| s.as_str()).map(str::to_string);
    let mut label_events: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|e| {
            matches!(str_at(e, "/event").as_deref(), Some("labeled" | "unlabeled"))
                && str_at(e, "/label/name").as_deref() == Some(SEQUENCE_LABEL)
        })
        .collect();
    // Stable: same-second events keep the forge's (chronological) order.
    label_events.sort_by_key(|e| str_at(e, "/created_at").unwrap_or_default());
    let Some(newest) = label_events.last() else {
        return Some(false);
    };
    if str_at(newest, "/event").as_deref() != Some("unlabeled") {
        return Some(false);
    }
    let actor = str_at(newest, "/actor/login").filter(|l| !l.trim().is_empty())?;
    if is_fleet(&actor) {
        return Some(false);
    }
    let removed_at = timestamp(newest)?;
    let mut marker_at = None;
    for c in rows.iter().filter(|e| {
        str_at(e, "/event").as_deref() == Some("commented")
            && str_at(e, "/body").is_some_and(|b| carries_marker(&b, marker))
    }) {
        // Any carrying comment with an unreadable timestamp is an unknown.
        let at = comment_timestamp(c)?;
        marker_at = Some(marker_at.map_or(at, |m: chrono::DateTime<_>| m.max(at)));
    }
    Some(removed_at > marker_at?)
}

/// What the sticky-release check found for one planned edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorRelease {
    /// No sticky release applies: plan the edge as before.
    None,
    /// A record already covers this pair at this tree: skip the edge.
    Recorded,
    /// An operator release was just detected: skip the edge, and the caller
    /// records it at the carried pinned `follower_head`.
    Detected(String),
}

/// The sticky-release decision for `edge`, given the follower's trusted
/// comment `bodies`, its listing row, a tree comparison and a label-history
/// read (the last two are seams for tests). Reads the label history only for
/// the narrow shape an operator release leaves: a live marker for this pair,
/// no record yet, and the tree still the one the marker pinned.
pub fn decide(
    edge: &SequenceEdge,
    follower: &SequencePr,
    bodies: &[String],
    same_tree: impl Fn(&str, &str) -> bool,
    operator_unlabeled_now: impl FnOnce(&SequenceMarker) -> Option<bool>,
) -> OperatorRelease {
    let Some(live) = follower.head_sha.as_deref().filter(|h| !h.is_empty()) else {
        return OperatorRelease::None;
    };
    if let Some((after, pinned)) = parse_record_in_force(bodies) {
        if after == edge.after && same_tree(&pinned, live) {
            return OperatorRelease::Recorded;
        }
        if after == edge.after {
            // The tree changed since the operator's release: it has ended.
            return OperatorRelease::None;
        }
    }
    // A pass-made release ends the marker with a tombstone, so `parse_live`
    // is `None` and the existing behavior stands.
    let Some(marker) = parse_live(bodies) else {
        return OperatorRelease::None;
    };
    if marker.after != edge.after || !same_tree(&marker.follower_head, live) {
        return OperatorRelease::None;
    }
    match operator_unlabeled_now(&marker) {
        Some(true) => OperatorRelease::Detected(marker.follower_head),
        _ => OperatorRelease::None,
    }
}

/// The follower's label history (issue timeline), or `None` on any failure.
fn fetch_timeline(gh_bin: &Path, root: &Path, number: u32) -> Option<Vec<u8>> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{number}/timeline?per_page=100");
    gh_call::ok_stdout(gh_call::read("sequence.label_timeline", gh_bin, root).args([
        "api",
        &path,
        "--paginate",
    ]))
}

/// [`decide`] against the forge: the `kind=tree` comparison and the
/// follower's label timeline, with the fleet roster for `root`.
pub fn check(
    gh_bin: &Path,
    root: &Path,
    edge: &SequenceEdge,
    open: &[SequencePr],
    bodies: &[String],
) -> OperatorRelease {
    let Some(follower) = open.iter().find(|p| p.number == edge.follower) else {
        return OperatorRelease::None;
    };
    decide(
        edge,
        follower,
        bodies,
        |pinned, live| forge_same_tree(gh_bin, root, pinned, live),
        |marker| {
            let fleet = crate::forge_identity::FleetLogins::for_root(root);
            let events = fetch_timeline(gh_bin, root, edge.follower)?;
            operator_unlabeled(&events, marker, |l| fleet.contains(l))
        },
    )
}

/// The record comment: the machine line plus why the pass stands back.
#[must_use]
pub fn record_comment_body(after: u32, follower_head: &str) -> String {
    format!(
        "{}\n**Sequencing release kept** — `loom:sequenced` was removed from this PR by someone \
         outside the fleet, so the merge-sequencing pass will not order it after #{after} again \
         while this PR's tree is unchanged (a tree-identical re-date does not count). A change \
         to the tree ends the release and the order is re-derived as usual.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#10398)*",
        record_text(after, follower_head)
    )
}

/// Post the record for a [`OperatorRelease::Detected`] release — idempotent:
/// the identical record line already present suppresses the repeat.
pub(super) fn record(
    gh_bin: &Path,
    root: &Path,
    edge: &SequenceEdge,
    follower_head: &str,
    bodies: &[String],
) -> anyhow::Result<()> {
    if bodies
        .iter()
        .any(|b| b.contains(&record_text(edge.after, follower_head)))
    {
        return Ok(());
    }
    let body = record_comment_body(edge.after, follower_head);
    gh_pr(gh_bin, root, &["comment", &edge.follower.to_string(), "--body", &body])?;
    Ok(())
}

#[cfg(test)]
#[path = "merge_sequence_sticky_tests.rs"]
mod tests;
