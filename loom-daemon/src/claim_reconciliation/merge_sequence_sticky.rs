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
//!    event on a PR is a removal by a non-fleet actor while the PR's newest
//!    marker is still live (no pass tombstone after it), the pass records a
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
//! history, an event with no actor, or a fleet actor records no sticky
//! release. Nothing here weakens the `merge-pr.sh` gate
//! (`merge_pr::labels`): the label is still the only thing the merge path
//! reads, and this module only decides whether the pass puts it back.

use std::borrow::Cow;
use std::path::Path;

use super::{gh_pr, SequenceEdge, SequenceMarker, SequencePr, SEQUENCE_LABEL};
use crate::claim_reconciliation::gh_call;
use crate::forge_tree_unchanged::{tree_unchanged, verdict_tree_carveout_enabled};
use crate::merge_pr::sequence::{html_comment_spans, is_full_sha, parse_live, states_or_ends_hold};

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

/// Is the newest `loom:sequenced` label event in `events` (a paginated
/// issue-timeline body) a removal by an actor `is_fleet` does not claim?
/// `Some(false)` for no such event, a newest `labeled`, or a fleet actor;
/// `None` when the body does not parse or the newest event names no actor —
/// unknown never makes a release sticky.
pub fn operator_unlabeled(events: &[u8], is_fleet: impl Fn(&str) -> bool) -> Option<bool> {
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
    Some(!is_fleet(&actor))
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
    operator_unlabeled_now: impl FnOnce() -> Option<bool>,
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
    match operator_unlabeled_now() {
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
        || {
            let fleet = crate::forge_identity::FleetLogins::for_root(root);
            operator_unlabeled(&fetch_timeline(gh_bin, root, edge.follower)?, |l| fleet.contains(l))
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
