//! Proactive landing-order planning for overlapping PRs (#9686 — the first
//! release of the #9063 epic: automatic ordering, not combinations).
//!
//! # The problem
//!
//! When several PRs touch the same area, repairing each against every
//! intermediate main tip sends the same work through Doctor and Judge
//! repeatedly, and Champion merges whichever approved PR it reaches first —
//! possibly the wrong one of an ordered pair (the #9378 incident). Ordering
//! was previously agreed in prose, which no tool reads.
//!
//! # What the pass does, per tick per repo
//!
//! - **Trigger.** Repos with ≤2 open PRs are a byte-identical no-op (the
//!   trigger counts ALL open PRs, including drafts and held ones — only the
//!   *ordering* excludes them).
//! - **Plan.** Eligible open PRs (not draft, no hold label, no agent
//!   mid-flight, pinnable head) are grouped into connected components of the
//!   shared-changed-file graph. Shared filenames are evidence of possible
//!   overlap, never proof of semantic compatibility — the pass only orders,
//!   it never merges, vouches, or combines. Within a component, existing
//!   trusted `loom:sequence` markers and base-branch stacking are
//!   authoritative constraints; everything else orders ready-first (#10371,
//!   [`ready`]: approved PRs ahead of non-approved ones), then oldest-first
//!   within a tier (stable tiebreak on PR number), except that a starred
//!   (`loom:operator-priority`) placeable PR goes before an unstarred one of
//!   its tier and a stalled no-verdict PR goes last (#10060). A constraint
//!   cycle skips the whole component for that tick — a half-rewritten order
//!   is worse than a deferred one.
//! - **Apply.** Edges are a DAG over DIRECT overlap (#10060): a follower
//!   waits only for its nearest earlier member that shares a changed file
//!   with it (or that it is stacked on), never for a PR it reaches only
//!   through a third PR — so the marker's "changes files #N also changes" is
//!   always true. No edge is written behind a predecessor that is not ready
//!   to land (#10371). The follower gets `loom:sequenced` (#9378's durable gate)
//!   and a trusted `source=pass` marker pinning both heads and the plan id.
//!   Followers that already carry a trusted marker are never re-planned —
//!   a human's or another pass's "after" wins over the computed order.
//! - **Release / replan.** Every open holder is re-evaluated with #9378's
//!   [`evaluate`]: predecessor landed at the recorded head ⇒ release;
//!   predecessor closed unmerged ⇒ release with that reason; any moved head
//!   ⇒ the stale hold is voided (label off, replan note) and the next tick
//!   re-derives a fresh, correctly-pinned plan. Soft (`source=pass`) holds
//!   on approved followers expire when the predecessor has been quiet for
//!   `LOOM_MERGE_SEQUENCE_MAX_AGE_HOURS` (default 72) — a scheduling
//!   preference must not starve mergeable work. Hard holds (no `source=`,
//!   the human-authored shape) never auto-expire: expiring a semantic
//!   dependency into merge permission is exactly the failure #9063 forbids.
//! - **Stalled heads (#10060, [`stall`]).** A predecessor quiet for
//!   `LOOM_MERGE_SEQUENCE_STALL_HOURS` (default 12) while on a human hold
//!   (`loom:operator` …) or without a `loom:pr` verdict is a stalled head:
//!   soft holds on approved followers behind it release early, and the chain
//!   is escalated once ("operator needed" on the head PR, naming the action
//!   and the approved PRs it holds). Hard holds still never release.
//! - **Not-ready release (#10371, [`ready`]).** A soft hold whose predecessor
//!   loses `loom:pr`, or gains `loom:changes-requested` / `loom:ci-failure` /
//!   `loom:blocked`, is released on the next tick; hard holds never are.
//! - **No-overlap release (#10077, [`overlap`]).** A soft in-flight hold
//!   whose two PRs share no changed file (a transitive-only edge recorded
//!   before #10060) is released; any failed read or unknown keeps it.
//! - **Defer repairs.** The review-conflict pass
//!   (`super::review_conflict`) consults this module's `defer_base_repair`:
//!   a base-conflicting review-queue PR whose sequencing predecessor is
//!   still open at the recorded head is NOT flagged for Doctor — the
//!   predecessor landing will move the base again, so the repair would be
//!   redundant. The deferral is this pass's own marker on the PR and ends on
//!   its own: once the predecessor lands (or the hold releases for any
//!   reason), the follower is an ordinary conflicting PR and the next tick
//!   flags it. The last needed repair always still runs.
//!
//! # What it never does
//!
//! No merge, no branch mutation, no verdict or CI result is fabricated or
//! carried forward (#9416 owns proven equivalence), no substantive Judge
//! rejection is suppressed (only THIS pass's base-conflict auto-flag
//! consults the sequence state), and independent PRs are untouched. Every
//! write is idempotent: an identical marker already on the PR suppresses the
//! repeat comment, so competing daemons converge instead of spamming.
//!
//! Kill switch: [`MERGE_SEQUENCE_ENABLED_ENV`] (default ON), nested inside
//! the master `LOOM_STALE_CLAIM_RECONCILE` switch like the review-conflict
//! pass. Read-only inspection: `loom-daemon merge-pr sequence-plan`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::gh_call;
use crate::merge_pr::sequence::{
    evaluate, fetch_predecessor, fetch_trusted_bodies, marker_text, parse, KeepReason,
    PredecessorState, SequenceMarker, Verdict,
};
/// Kill switch for this pass (`0`/`false`/`no`/`off` disables). Defaults ON.
pub const MERGE_SEQUENCE_ENABLED_ENV: &str = "LOOM_MERGE_SEQUENCE_RECONCILE";

/// Soft holds expire when the predecessor has been quiet (its `updatedAt`
/// has not moved) for this many hours AND the follower is Judge-approved.
/// Any predecessor activity — even a comment — keeps the order fresh; the
/// bound exists so a stalled ordering cannot starve mergeable work forever.
pub const MERGE_SEQUENCE_MAX_AGE_ENV: &str = "LOOM_MERGE_SEQUENCE_MAX_AGE_HOURS";
const DEFAULT_MAX_AGE_HOURS: f64 = 72.0;

// `LOOM_MERGE_SEQUENCE_STALL_HOURS` (default 12, #10060) is the companion
// bound to `LOOM_MERGE_SEQUENCE_MAX_AGE_HOURS` above: a chain head quiet this
// long on a human hold / without a verdict releases soft holds on approved
// followers and is escalated once. Defined in `stall`.
#[path = "merge_sequence_stall.rs"]
pub mod stall;
pub use stall::{stall_hours, MERGE_SEQUENCE_STALL_ENV};

// Releasing recorded holds between PRs that share no changed file, and the
// per-tick changed-files cache both phases share (#10077).
#[path = "merge_sequence_overlap.rs"]
pub mod overlap;

// Ready-first ordering and the not-ready release (#10371).
#[path = "merge_sequence_ready.rs"]
pub mod ready;

/// The durable hold label this pass applies (defined by #9378).
pub const SEQUENCE_LABEL: &str = "loom:sequenced";

/// The trigger: repositories with MORE THAN TWO open PRs are planned; two or
/// fewer keep byte-identical behavior. Counts ALL open PRs (the issue is
/// explicit that drafts and held PRs count toward the trigger while staying
/// ineligible for ordering).
pub const TRIGGER_OPEN_PRS: usize = 2;

/// An agent is actively working the PR — never plan around it.
const IN_FLIGHT_LABELS: [&str; 2] = ["loom:reviewing", "loom:treating"];

/// `source=pass` — the marker this pass writes. Soft: expiry-eligible.
pub const SOURCE_PASS: &str = "pass";

/// Is this pass enabled? See [`MERGE_SEQUENCE_ENABLED_ENV`].
#[must_use]
pub fn merge_sequence_enabled() -> bool {
    match std::env::var(MERGE_SEQUENCE_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// The soft-hold expiry bound, in hours.
#[must_use]
pub fn max_age_hours() -> f64 {
    std::env::var(MERGE_SEQUENCE_MAX_AGE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|h| *h > 0.0)
        .unwrap_or(DEFAULT_MAX_AGE_HOURS)
}

/// An open PR as the planning core sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencePr {
    pub number: u32,
    /// RFC3339; lexicographic order is chronological order.
    pub created_at: String,
    /// RFC3339; moves on ANY predecessor activity — the staleness signal.
    pub updated_at: String,
    pub head_sha: Option<String>,
    pub head_ref: String,
    pub base_ref: String,
    pub draft: bool,
    pub labels: Vec<String>,
}

impl SequencePr {
    #[must_use]
    pub fn has(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }

    /// Ordering eligibility: drafts and held PRs stay out of the plan (they
    /// count toward the trigger only), and an agent mid-flight is left alone.
    #[must_use]
    pub fn eligible_for_ordering(&self) -> bool {
        if self.draft {
            return false;
        }
        if super::VERDICT_HOLD_LABELS.iter().any(|l| self.has(l)) {
            return false;
        }
        !IN_FLIGHT_LABELS.iter().any(|l| self.has(l))
    }

    /// Pinnable: a head SHA the marker can pin. An unpinnable PR cannot be
    /// re-checked later, so it is never ordered.
    #[must_use]
    pub fn pinnable(&self) -> bool {
        self.head_sha.as_deref().is_some_and(|s| !s.is_empty())
    }
}

impl From<&super::open_pr_listing::RestPull> for SequencePr {
    /// One row of the REST open-PR listing (#10349).
    fn from(r: &super::open_pr_listing::RestPull) -> Self {
        Self {
            number: r.number,
            created_at: r.created_at.clone().unwrap_or_default(),
            updated_at: r.updated_at.clone().unwrap_or_default(),
            head_sha: r.head_sha.clone(),
            head_ref: r.head_ref.clone().unwrap_or_default(),
            base_ref: r.base_ref.clone().unwrap_or_default(),
            draft: r.draft,
            labels: r.labels.clone(),
        }
    }
}

/// The planner's view of the open-PR listing: the newest
/// [`super::MAX_ISSUES_PER_WORKSPACE`] rows (the listing is newest first and
/// pages further, for the review-conflict pass's sake).
#[must_use]
pub fn sequence_prs(rows: &[super::open_pr_listing::RestPull]) -> Vec<SequencePr> {
    let cap = usize::try_from(super::MAX_ISSUES_PER_WORKSPACE).unwrap_or(usize::MAX);
    rows.iter().take(cap).map(SequencePr::from).collect()
}

/// Why a component/edge exists — recorded on the plan, consumed by telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeReason {
    /// Shared changed filenames inside the component.
    SharedFiles,
    /// The follower's base branch is the predecessor's head branch (stacked).
    StackedBase,
}

/// One planned ordering edge: `follower` waits for `after` (chain order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceEdge {
    pub follower: u32,
    pub after: u32,
    /// The predecessor head to pin (`pred_head=`).
    pub pred_head: String,
    /// The follower head to pin (`follower_head=`).
    pub follower_head: String,
    /// Deterministic plan id shared by every edge of one component.
    pub plan: String,
    pub reason: EdgeReason,
}

/// A planned component: ordered members plus the chain edges to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceGroup {
    pub plan: String,
    /// Member PR numbers in landing order (oldest-first absent constraints).
    pub order: Vec<u32>,
    pub edges: Vec<SequenceEdge>,
}

/// Deterministic plan id for one component: `seq-` + 8 hex of SHA-256 over
/// the sorted `number:head` member lines. Same members at same heads ⇒ same
/// id across daemons and versions ⇒ apply is idempotent (a re-run recognizes
/// its own marker instead of posting a duplicate).
#[must_use]
pub fn plan_id(members: &[(u32, &str)]) -> String {
    let mut lines: Vec<String> = members.iter().map(|(n, h)| format!("{n}:{h}")).collect();
    lines.sort();
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("seq-{}", &digest[..8])
}

/// Connected components of the shared-file overlap graph over `eligible`.
///
/// `files` maps PR number → its changed paths. PRs without an entry (a
/// failed files read) are singletons — a failed read must not widen a group
/// by pretending to know the files.
#[must_use]
pub fn overlap_components(
    eligible: &[&SequencePr],
    files: &BTreeMap<u32, BTreeSet<String>>,
) -> Vec<Vec<u32>> {
    let nums: Vec<u32> = eligible.iter().map(|p| p.number).collect();
    let mut adj: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (i, a) in eligible.iter().enumerate() {
        for b in &eligible[i + 1..] {
            let shared = files
                .get(&a.number)
                .zip(files.get(&b.number))
                .is_some_and(|(fa, fb)| fa.iter().any(|f| fb.contains(f)));
            if shared {
                adj.entry(a.number).or_default().push(b.number);
                adj.entry(b.number).or_default().push(a.number);
            }
        }
    }
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut out = Vec::new();
    for start in &nums {
        if seen.contains(start) {
            continue;
        }
        let mut component = Vec::new();
        let mut queue = VecDeque::from([*start]);
        seen.insert(*start);
        while let Some(n) = queue.pop_front() {
            component.push(n);
            for next in adj.get(&n).into_iter().flatten() {
                if seen.insert(*next) {
                    queue.push_back(*next);
                }
            }
        }
        out.push(component);
    }
    out
}

/// Order one component's members: constraint edges first (trusted markers
/// among members, stacked bases), then oldest-first with a stable number
/// tiebreak. `None` on a constraint cycle — the caller skips the component
/// whole rather than applying a partial order.
#[must_use]
pub fn order_component(members: &[&SequencePr], constraints: &[(u32, u32)]) -> Option<Vec<u32>> {
    order_component_with(members, constraints, &BTreeSet::new())
}

/// [`order_component`] with the placeable-set ranking: among members that
/// are placeable right now (no unplaced constraint predecessor), ready
/// members go first (#10371, [`ready::tier`]); within a tier a `stalled`
/// member goes last and a starred one goes first (#10060), then
/// oldest-first. A rank never overrides a constraint edge; with every member
/// in one tier, no star and no stall the order is exactly the oldest-first one.
#[must_use]
pub fn order_component_with(
    members: &[&SequencePr],
    constraints: &[(u32, u32)],
    stalled: &BTreeSet<u32>,
) -> Option<Vec<u32>> {
    let nums: BTreeSet<u32> = members.iter().map(|p| p.number).collect();
    let mut succ: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut indegree: BTreeMap<u32, usize> = members.iter().map(|p| (p.number, 0)).collect();
    for (follower, after) in constraints {
        if nums.contains(follower) && nums.contains(after) && follower != after {
            succ.entry(*after).or_default().push(*follower);
            indegree.entry(*follower).and_modify(|d| *d += 1);
        }
    }
    let by_number: BTreeMap<u32, &SequencePr> = members.iter().map(|p| (p.number, *p)).collect();
    let mut ready: Vec<u32> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut ordered = Vec::with_capacity(members.len());
    while !ready.is_empty() {
        // Among the currently placeable: ready before not ready, then
        // not-stalled before stalled, starred before unstarred, then
        // oldest-first — a stable total order independent of listing order.
        let rank =
            |p: &SequencePr| (ready::tier(p), stalled.contains(&p.number), !stall::starred(p));
        ready.sort_by(|a, b| {
            let (pa, pb) = (by_number[a], by_number[b]);
            (rank(pa), &pa.created_at, pa.number).cmp(&(rank(pb), &pb.created_at, pb.number))
        });
        let n = ready.remove(0);
        ordered.push(n);
        for next in succ.get(&n).into_iter().flatten() {
            let d = indegree.get_mut(next).expect("edge target is a member");
            *d -= 1;
            if *d == 0 {
                ready.push(*next);
            }
        }
    }
    (ordered.len() == members.len()).then_some(ordered)
}

/// Plan edges for one ordered component — a DAG over direct overlap (#10060).
///
/// Each follower waits for its NEAREST earlier member (latest in `order`)
/// that directly shares a changed file with it or whose head branch is its
/// base; a follower with no such member gets no edge. Two members linked
/// only through a third PR's files are never ordered against each other.
/// Followers with no pinnable head SHA yield no edge: an unpinned hold
/// cannot be re-checked safely. Edges carry [`EdgeReason::SharedFiles`]
/// except where the follower's base branch IS the predecessor's head branch
/// ([`EdgeReason::StackedBase`]).
#[must_use]
pub fn plan_group(
    plan: &str,
    order: &[u32],
    by_number: &BTreeMap<u32, SequencePr>,
    files: &BTreeMap<u32, BTreeSet<String>>,
) -> SequenceGroup {
    let shares = |a: u32, b: u32| {
        files
            .get(&a)
            .zip(files.get(&b))
            .is_some_and(|(fa, fb)| fa.iter().any(|f| fb.contains(f)))
    };
    let mut edges = Vec::new();
    for (i, &follower) in order.iter().enumerate() {
        let Some(fol) = by_number.get(&follower) else {
            continue;
        };
        let Some(&after) = order[..i].iter().rev().find(|&&a| {
            shares(a, follower)
                || by_number
                    .get(&a)
                    .is_some_and(|p| fol.base_ref == p.head_ref)
        }) else {
            continue;
        };
        let Some(pred) = by_number.get(&after) else {
            continue;
        };
        let (Some(pred_head), Some(follower_head)) =
            (pred.head_sha.as_deref(), fol.head_sha.as_deref())
        else {
            continue;
        };
        edges.push(SequenceEdge {
            follower: fol.number,
            after,
            pred_head: pred_head.to_string(),
            follower_head: follower_head.to_string(),
            plan: plan.to_string(),
            reason: if fol.base_ref == pred.head_ref {
                EdgeReason::StackedBase
            } else {
                EdgeReason::SharedFiles
            },
        });
    }
    SequenceGroup {
        plan: plan.to_string(),
        order: order.to_vec(),
        edges,
    }
}

/// The full plan for one repo's open set. Pure: the caller fetched listing +
/// files + trusted markers, and gets back the groups to apply. Constraints
/// come from `markers` (trusted, by follower) and from base-branch stacking
/// among members. Components of size 1 are not groups. Stalled no-verdict
/// PRs are judged against the wall clock and [`stall_hours`].
#[must_use]
pub fn plan_repo(
    open_prs: &[SequencePr],
    files: &BTreeMap<u32, BTreeSet<String>>,
    markers: &BTreeMap<u32, SequenceMarker>,
) -> Vec<SequenceGroup> {
    let stalled = stall::stalled_for_ordering(open_prs, Utc::now(), stall_hours());
    plan_repo_with(open_prs, files, markers, &stalled)
}

/// [`plan_repo`] with an explicit stalled set (deterministic for tests).
#[must_use]
pub fn plan_repo_with(
    open_prs: &[SequencePr],
    files: &BTreeMap<u32, BTreeSet<String>>,
    markers: &BTreeMap<u32, SequenceMarker>,
    stalled: &BTreeSet<u32>,
) -> Vec<SequenceGroup> {
    if open_prs.len() <= TRIGGER_OPEN_PRS {
        return Vec::new();
    }
    let eligible: Vec<&SequencePr> = open_prs
        .iter()
        .filter(|p| p.eligible_for_ordering() && p.pinnable())
        .collect();
    let by_number: BTreeMap<u32, SequencePr> =
        open_prs.iter().map(|p| (p.number, p.clone())).collect();
    let mut groups = Vec::new();
    for component in overlap_components(&eligible, files) {
        if component.len() < 2 {
            continue;
        }
        let mut constraints: Vec<(u32, u32)> = Vec::new();
        for n in &component {
            if let Some(m) = markers.get(n) {
                constraints.push((*n, m.after));
            }
            if let Some(p) = by_number.get(n) {
                for other in &component {
                    if other != n
                        && by_number
                            .get(other)
                            .is_some_and(|o| p.base_ref == o.head_ref)
                    {
                        constraints.push((*n, *other));
                    }
                }
            }
        }
        let members: Vec<&SequencePr> = component.iter().filter_map(|n| by_number.get(n)).collect();
        let Some(order) = order_component_with(&members, &constraints, stalled) else {
            log::info!(
                "merge_sequence: component {component:?} has a constraint cycle — skipped this tick"
            );
            continue;
        };
        let member_pins: Vec<(u32, &str)> = order
            .iter()
            .filter_map(|n| {
                by_number
                    .get(n)
                    .and_then(|p| p.head_sha.as_deref())
                    .map(|h| (*n, h))
            })
            .collect();
        let id = plan_id(&member_pins);
        let mut group = plan_group(&id, &order, &by_number, files);
        ready::drop_edges_behind_unready(&mut group, &by_number);
        groups.push(group);
    }
    groups
}

/// The marker an edge implies: `source=pass`, pinned heads, the group plan.
#[must_use]
pub fn edge_marker(edge: &SequenceEdge) -> SequenceMarker {
    SequenceMarker {
        after: edge.after,
        pred_head: edge.pred_head.clone(),
        follower_head: edge.follower_head.clone(),
        plan: edge.plan.clone(),
        source: Some(SOURCE_PASS.to_string()),
    }
}

// --- Phase 1: hold evaluation -------------------------------------------

/// What Phase 1 wants to do with one existing `loom:sequenced` hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldAction {
    /// Leave the label; the ordering is being honored (soft).
    HoldSoft,
    /// Leave the label; a human-authored hold never auto-expires.
    HoldHard,
    /// Predecessor landed at the recorded head: release.
    Release,
    /// Predecessor closed unmerged: release, naming the reason.
    ReleaseDissolved,
    /// A pinned head moved: the stale hold is VOID — label off, replan note;
    /// the next tick re-derives a fresh plan from current heads.
    VoidAndReplan,
    /// Soft hold on an approved follower whose predecessor went quiet past
    /// the bound: release so mergeable work is not starved.
    Expire,
    /// Soft hold on an approved follower whose predecessor is a stalled head
    /// (human hold / no verdict, quiet past the stall bound — #10060): release
    /// so ready work lands first. Decided by [`stall::hold_action_with_stall`].
    ReleaseStalled,
    /// Soft hold between two PRs that share no changed file (a transitive-only
    /// edge from before #10060): release. Decided by [`overlap::with_no_overlap`].
    ReleaseNoOverlap,
    /// Soft hold whose predecessor is not ready to land (no `loom:pr`, or a
    /// changes-requested / ci-failure / blocked label): release (#10371).
    /// Decided by [`ready::with_readiness`].
    ReleaseNotReady,
}

/// Pure Phase-1 decision for one hold.
///
/// `follower_head` is the follower's LIVE head SHA from this tick's listing —
/// never the marker's own recorded `follower_head`, which would make the
/// follower-moved check in [`evaluate`] vacuous. It is checked first, per
/// `evaluate`'s contract: a marker that no longer describes this tree is void
/// whatever the predecessor did, so a moved follower voids the hold even when
/// the predecessor is unreadable (the listing itself is the positive signal).
///
/// `follower_head` is `None` when the listing carried no head, and `pred` is
/// `None` when the predecessor could not be read — both fail closed: hold
/// (the release needs a positive signal, the #9378 asymmetry).
#[must_use]
pub fn hold_action(
    marker: &SequenceMarker,
    pred: Option<&PredecessorState>,
    follower_head: Option<&str>,
    follower_has_loom_pr: bool,
    max_age_hours: f64,
) -> HoldAction {
    let hard = marker.source.as_deref() != Some(SOURCE_PASS);
    let hold = if hard {
        HoldAction::HoldHard
    } else {
        HoldAction::HoldSoft
    };
    let Some(follower_head) = follower_head.filter(|h| !h.is_empty()) else {
        return hold;
    };
    if follower_head != marker.follower_head {
        return HoldAction::VoidAndReplan;
    }
    let Some(pred) = pred else {
        return hold;
    };
    match evaluate(marker, pred, follower_head) {
        Verdict::Clear => HoldAction::Release,
        Verdict::Dissolved => HoldAction::ReleaseDissolved,
        Verdict::Keep(KeepReason::InFlight) => {
            if hard {
                HoldAction::HoldHard
            } else if follower_has_loom_pr
                && pred_quiet_hours(pred).is_some_and(|age| age > max_age_hours)
            {
                HoldAction::Expire
            } else {
                HoldAction::HoldSoft
            }
        }
        Verdict::Keep(_) => HoldAction::VoidAndReplan,
    }
}

/// Hours since the predecessor's last activity, or `None` when unknown —
/// unknown never expires.
#[must_use]
pub fn pred_quiet_hours(pred: &PredecessorState) -> Option<f64> {
    let updated = pred.updated_at.as_deref()?;
    let then = chrono::DateTime::parse_from_rfc3339(updated).ok()?;
    let now = Utc::now();
    Some((now.timestamp_millis() as f64 - then.timestamp_millis() as f64) / 3_600_000.0)
}

// --- Comments -----------------------------------------------------------

/// The apply comment body. Carries the marker (the machine state) and a
/// concise human explanation.
#[must_use]
pub fn apply_comment_body(marker: &SequenceMarker, reason: EdgeReason) -> String {
    let why = match reason {
        EdgeReason::StackedBase => format!("its base branch is #{}'s head branch", marker.after),
        EdgeReason::SharedFiles => format!("it changes files #{} also changes", marker.after),
    };
    format!(
        "**Landing order recorded** — this PR overlaps other open work\n\n\
         Planned by the merge-sequencing pass (#9686): this PR lands AFTER #{}, because {why}. \
         Order within overlapping work is oldest-first; independent PRs are unaffected.\n\n\
         While the `loom:sequenced` label is present, `merge-pr.sh` refuses to merge this PR \
         (the #9378 gate). The label clears mechanically when #{} lands at the recorded head \
         — or by re-evaluation if it closes or moves. This is a scheduling preference: it \
         suppresses only redundant base-conflict repairs while #{} is in flight, never a \
         genuine review finding.\n\n\
         {}\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#9686, plan {})*",
        marker.after,
        marker.after,
        marker.after,
        marker_text(marker),
        marker.plan
    )
}

/// The release comment (predecessor landed / dissolved / expired).
#[must_use]
pub fn release_comment_body(marker: &SequenceMarker, action: HoldAction) -> String {
    let why = match action {
        HoldAction::Release => {
            format!("#{} merged at the recorded head — this PR may land now", marker.after)
        }
        HoldAction::ReleaseDissolved => {
            format!("#{} closed without merging — the ordering's subject is gone", marker.after)
        }
        HoldAction::Expire => format!(
            "#{} has been quiet past the {:.0}h soft-ordering bound while this PR is approved — \
             the scheduling preference expired rather than starve mergeable work",
            marker.after,
            max_age_hours()
        ),
        HoldAction::ReleaseStalled => format!(
            "#{} has stalled past the {:.0}h bound (on a human hold, or without a verdict) while \
             this PR is approved — ready work lands first and #{} is rebased afterwards",
            marker.after,
            stall_hours(),
            marker.after
        ),
        HoldAction::ReleaseNoOverlap => overlap::release_reason(marker),
        HoldAction::ReleaseNotReady => ready::release_reason(marker),
        _ => "released".to_string(),
    };
    format!(
        "<!-- loom:sequence released plan={} -->\n\
         **Sequencing hold released**: {why}. The `loom:sequenced` label is removed; normal \
         merge rules apply from here.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#9686)*",
        marker.plan
    )
}

/// The void note posted when a hold's pins no longer describe reality.
pub const REPLAN_NOTE_BODY: &str =
    "**Sequencing hold voided and re-planned** — a pinned head moved \
     since the last plan, so the old hold no longer describes this tree. The label is removed; \
     the pass re-derives the landing order from the current heads on its next tick, and if the \
     overlap is gone this PR simply proceeds unordered.\n\n\
     <!-- loom:sequence replanned -->\n\n\
     ---\n\
     *Automated by loom-daemon claim reconciliation (#9686)*";

/// The repair-deferral marker: written by the review-conflict pass when a
/// base conflict is deferred behind an open predecessor instead of flagged.
pub const DEFER_REPAIR_MARKER_FMT: &str =
    "<!-- loom:sequence defer-repair plan=%PLAN% pred=%PRED% -->";

/// The deferral comment body.
#[must_use]
pub fn defer_comment_body(marker: &SequenceMarker) -> String {
    format!(
        "<!-- loom:sequence defer-repair plan={} pred={} -->\n\
         **Base-conflict repair deferred** — this PR is sequenced behind #{} (plan {}), which is \
         still open at the recorded head. Rebasing now would be redundant: #{} landing will move \
         the base again. The conflict is re-checked every tick; when #{} lands (or the hold \
         releases for any reason), a still-conflicting tree is flagged for repair as usual. A \
         genuine review finding is NOT deferred — only this automated base-conflict routing is.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#9686)*",
        marker.plan, marker.after, marker.after, marker.after, marker.after, marker.after
    )
}

/// True when a base-conflict repair should be DEFERRED rather than flagged:
/// the follower's newest trusted marker describes a predecessor that is
/// still open at the recorded head (the ordinary in-flight ordering state).
/// Any replan-shaped state (moved heads, landed, dissolved) does NOT defer —
/// the hold needs attention, and a conflict flag beside it is information,
/// not churn.
///
/// `follower_head` is the follower's LIVE head — the one the caller just
/// found base-conflicting — so a marker written against an older tree of the
/// follower never defers a repair of the tree actually there now.
#[must_use]
pub fn defer_base_repair(
    marker: &SequenceMarker,
    pred: &PredecessorState,
    follower_head: &str,
) -> bool {
    matches!(evaluate(marker, pred, follower_head), Verdict::Keep(KeepReason::InFlight))
}

/// Evaluate whether the review-conflict pass should DEFER flagging `number`:
/// `Ok(Some(marker))` defers behind the marker's open predecessor,
/// `Ok(None)` means flag normally. A comment-read failure is `Err` — the
/// caller flags anyway (fail toward repair, never toward suppression); a
/// missing marker or an unreadable predecessor is `Ok(None)`, since an
/// unverifiable ordering state must not suppress a repair either.
///
/// `follower_head` is the live head the caller's conflict decision was made
/// at (see [`defer_base_repair`]).
pub fn defer_flag_decision(
    gh_bin: &Path,
    root: &Path,
    number: u32,
    follower_head: &str,
) -> Result<Option<SequenceMarker>> {
    let bin = gh_bin.to_string_lossy().to_string();
    let Some(bodies) = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", number) else {
        anyhow::bail!("could not read trusted comments on PR #{number}");
    };
    let Some(marker) = parse(&bodies) else {
        return Ok(None);
    };
    let Some(pred) = fetch_predecessor(&bin, root, "{owner}/{repo}", marker.after) else {
        return Ok(None);
    };
    Ok(defer_base_repair(&marker, &pred, follower_head).then_some(marker))
}

/// Record a deferral on the PR — idempotent: the exact defer-marker comment
/// already present suppresses the repeat, so the tick cannot spam.
pub(crate) fn defer_repair(
    gh_bin: &Path,
    root: &Path,
    number: u32,
    marker: &SequenceMarker,
) -> Result<()> {
    let bin = gh_bin.to_string_lossy().to_string();
    let bodies = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", number).unwrap_or_default();
    let want =
        format!("<!-- loom:sequence defer-repair plan={} pred={} -->", marker.plan, marker.after);
    if bodies.iter().any(|b| b.contains(&want)) {
        return Ok(());
    }
    let n = number.to_string();
    let body = defer_comment_body(marker);
    gh_pr(gh_bin, root, &["comment", &n, "--body", &body])?;
    Ok(())
}

// --- Counters -----------------------------------------------------------

/// Counters for one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MergeSequenceStats {
    pub checked: usize,
    pub groups: usize,
    pub applied: usize,
    pub voided: usize,
    pub released: usize,
    pub expired: usize,
    /// Soft holds released behind a stalled head (#10060).
    pub stall_released: usize,
    /// Soft holds released because the two PRs share no file (#10077).
    pub overlap_released: usize,
    /// Soft holds released because the predecessor is not ready (#10371).
    pub not_ready_released: usize,
    /// Stalled-chain escalations posted this tick (#10060).
    pub escalated: usize,
    pub held: usize,
}

// --- Forge reads --------------------------------------------------------

/// Run `gh pr <args…>` in `root` with the per-root credential and `LOOM_REPO`
/// applied — the same invocation shape as the review-conflict pass.
fn gh_pr(gh_bin: &Path, root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let inv = match args.first().copied() {
        Some("view") => gh_call::read("sequence.pr_view", gh_bin, root),
        Some("comment") => gh_call::write("sequence.pr_comment", gh_bin, root),
        _ => gh_call::write("sequence.pr_edit", gh_bin, root),
    };
    let out = gh_call::output(inv.args(["pr"]).args(args).args(gh_call::loom_repo_flag()))?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "gh pr {} failed in {}: {}",
            args.first().copied().unwrap_or_default(),
            root.display(),
            gh_call::stderr(&out)
        ));
    }
    Ok(out.stdout)
}

fn list_open_prs(gh_bin: &Path, root: &Path) -> Result<Vec<SequencePr>> {
    Ok(sequence_prs(&super::open_pr_listing::list_open_prs(gh_bin, root)?))
}

/// Changed paths for one PR. `None` on any failure: a failed read shrinks
/// the plan (singleton) rather than fabricating overlap. #10089: a PR's
/// files are a function of its head and base, so a found answer is reused
/// until either moves (this was one GraphQL view per eligible PR per tick).
fn changed_files(gh_bin: &Path, root: &Path, pr: &SequencePr) -> Option<BTreeSet<String>> {
    let key = super::read_cache::key(root, pr.number, &pr.base_ref, pr.head_sha.as_deref());
    super::read_cache::CHANGED_FILES.get_or(key, || fetch_changed_files(gh_bin, root, pr.number))
}

fn fetch_changed_files(gh_bin: &Path, root: &Path, number: u32) -> Option<BTreeSet<String>> {
    #[derive(Debug, Deserialize)]
    struct Row {
        path: String,
    }
    #[derive(Debug, Deserialize)]
    struct Files {
        #[serde(default)]
        files: Vec<Row>,
    }
    let stdout = gh_pr(gh_bin, root, &["view", &number.to_string(), "--json", "files"]).ok()?;
    let parsed: Files = serde_json::from_slice(&stdout).ok()?;
    Some(parsed.files.into_iter().map(|Row { path }| path).collect())
}

/// A holder's newest trusted marker (`Some(None)`: a manual hold), `None`
/// when the comments read failed. #10089: reused until the holder's
/// `updatedAt` moves — a new marker comment or label event bumps it.
fn holder_marker(bin: &str, root: &Path, pr: &SequencePr) -> Option<Option<SequenceMarker>> {
    let key = super::read_cache::key(root, pr.number, "hold-marker", Some(&pr.updated_at));
    super::read_cache::HOLD_MARKER.get_or(key, || {
        fetch_trusted_bodies(bin, root, "{owner}/{repo}", pr.number).map(|b| parse(&b))
    })
}

/// The predecessor's live state. #10089: while it is in this tick's open
/// listing, reused until its `updatedAt` or head moves; once it leaves the
/// listing (merged or closed) there is no key, so the read is always live.
fn predecessor(
    bin: &str,
    root: &Path,
    open: &[SequencePr],
    after: u32,
) -> Option<PredecessorState> {
    let key = open.iter().find(|p| p.number == after).and_then(|p| {
        let version = format!("{}@{}", p.updated_at, p.head_sha.as_deref()?);
        super::read_cache::key(root, after, "predecessor", Some(&version))
    });
    super::read_cache::PREDECESSOR
        .get_or(key, || fetch_predecessor(bin, root, "{owner}/{repo}", after))
}

/// The newest trusted marker per PR, for PRs carrying the hold label.
///
/// `known` holds this tick's already-completed reads (#10089): Phase 1 walks
/// every holder's comments, and the hold label is the filter here too, so
/// re-walking them doubled the pass's largest per-PR read. Reuse is exact,
/// not a cache: Phase 1's own writes (release / replan notes) carry no
/// parseable marker, so a fresh read would yield the same newest marker. A
/// holder whose Phase 1 read failed is absent from `known` and is re-read.
fn fetch_markers(
    gh_bin: &Path,
    root: &Path,
    prs: &[&SequencePr],
    known: &BTreeMap<u32, Option<SequenceMarker>>,
) -> BTreeMap<u32, SequenceMarker> {
    let bin = gh_bin.to_string_lossy().to_string();
    let mut out = BTreeMap::new();
    for pr in prs.iter().filter(|p| p.has(SEQUENCE_LABEL)) {
        let marker = match known.get(&pr.number) {
            Some(m) => m.clone(),
            None => fetch_trusted_bodies(&bin, root, "{owner}/{repo}", pr.number)
                .and_then(|b| parse(&b)),
        };
        if let Some(m) = marker {
            out.insert(pr.number, m);
        }
    }
    out
}

// --- Read-only plan (shared by the pass and the dry-run verb) ------------

/// A read-only plan report for one repo: what the pass WOULD do this tick.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanReport {
    pub open_prs: usize,
    pub groups: Vec<SequenceGroup>,
    /// Edges already satisfied by an identical marker — no write would occur.
    pub already_planned: usize,
    pub holders: usize,
    /// Edges suppressed because the follower already carries an ordering
    /// state (trusted marker or hold label) — reported so the dry-run's
    /// "what would the pass write" matches the live pass edge for edge
    /// (Judge re-review of #9707).
    pub skipped_held: usize,
    /// Existing holds Phase 1 would release as no-overlap (#10077):
    /// `(follower, predecessor)`.
    pub would_release_no_overlap: Vec<(u32, u32)>,
}

/// Compute the plan report without writing anything.
///
/// # Errors
/// Forge listing or files reads.
pub fn plan_report(gh_bin: &Path, root: &Path) -> Result<PlanReport> {
    let open = list_open_prs(gh_bin, root)?;
    let mut report = PlanReport {
        open_prs: open.len(),
        ..PlanReport::default()
    };
    if open.len() <= TRIGGER_OPEN_PRS {
        return Ok(report);
    }
    let eligible: Vec<&SequencePr> = open
        .iter()
        .filter(|p| p.eligible_for_ordering() && p.pinnable())
        .collect();
    let holder_numbers: BTreeSet<u32> = eligible
        .iter()
        .filter(|p| p.has(SEQUENCE_LABEL))
        .map(|p| p.number)
        .collect();
    report.holders = holder_numbers.len();
    let mut cache = overlap::TickFiles::default();
    report.would_release_no_overlap = overlap::would_release(gh_bin, root, &open, &mut cache);
    for pr in &eligible {
        cache.get_or_fetch(pr, |p| changed_files(gh_bin, root, p));
    }
    let files = cache.known(eligible.iter().map(|p| p.number));
    let markers = fetch_markers(gh_bin, root, &eligible, &BTreeMap::new());
    report.groups = plan_repo(&open, &files, &markers);
    for g in &mut report.groups {
        g.edges.retain(|e| {
            if markers.contains_key(&e.follower) || holder_numbers.contains(&e.follower) {
                report.skipped_held += 1;
                return false;
            }
            true
        });
    }
    for g in &report.groups {
        for e in &g.edges {
            if markers
                .get(&e.follower)
                .is_some_and(|m| marker_text(m) == marker_text(&edge_marker(e)))
            {
                report.already_planned += 1;
            }
        }
    }
    Ok(report)
}

// --- Writes -------------------------------------------------------------

/// Label off first, then comment — a released hold whose comment fails is
/// still released (a lost transcript line is recoverable; an on-PR hold that
/// outlives its subject is not).
fn release_hold(gh_bin: &Path, root: &Path, number: u32, body: &str) -> Result<()> {
    let n = number.to_string();
    gh_pr(gh_bin, root, &["edit", &n, "--remove-label", SEQUENCE_LABEL])?;
    gh_pr(gh_bin, root, &["comment", &n, "--body", body])?;
    Ok(())
}

/// Does the PR carry the sequencing label right now? Live read — the label
/// and the marker are checked independently so a partial write is healed
/// per-side on the next tick.
fn has_sequence_label(gh_bin: &Path, root: &Path, number: u32) -> Result<bool> {
    let stdout = gh_pr(
        gh_bin,
        root,
        &[
            "view",
            &number.to_string(),
            "--json",
            "labels",
            "--jq",
            ".labels[].name",
        ],
    )?;
    let text = String::from_utf8_lossy(&stdout);
    Ok(text.lines().any(|l| l.trim() == SEQUENCE_LABEL))
}

/// Apply one planned edge: marker comment + label, idempotent PER SIDE.
/// Returns `Ok(false)` only when both sides already agree — the convergence
/// guard that makes competing daemons settle without duplicate comments.
///
/// ORDER IS THE SAFETY PROPERTY (Judge re-review of #9707): the marker
/// comment goes FIRST, the label second. A label without a marker is the one
/// unrecoverable shape — Phase 1 reads "label, no marker" as a manual hold
/// no pass owns releasing — so the write order must never create it. The
/// inverse partial failure (marker posted, label write fails) leaves the PR
/// un-gated for at most one tick and is healed here on the next run: each
/// side is checked independently, so the follow-up adds the missing label
/// without re-posting the comment.
fn apply_edge(
    gh_bin: &Path,
    root: &Path,
    edge: &SequenceEdge,
    marker: &SequenceMarker,
) -> Result<bool> {
    let bin = gh_bin.to_string_lossy().to_string();
    let bodies =
        fetch_trusted_bodies(&bin, root, "{owner}/{repo}", edge.follower).unwrap_or_default();
    let marker_present = bodies.iter().any(|b| b.contains(&marker_text(marker)));
    let label_present = has_sequence_label(gh_bin, root, edge.follower)?;
    if marker_present && label_present {
        return Ok(false);
    }
    let n = edge.follower.to_string();
    if !marker_present {
        let body = apply_comment_body(marker, edge.reason);
        gh_pr(gh_bin, root, &["comment", &n, "--body", &body])?;
    }
    if !label_present {
        gh_pr(gh_bin, root, &["edit", &n, "--add-label", SEQUENCE_LABEL])?;
    }
    Ok(true)
}

// --- The pass ------------------------------------------------------------

/// Run the pass over one workspace `root`. Best effort: any `gh` failure is
/// logged at `warn` and contributes nothing, mirroring the review-conflict
/// pass's posture.
pub fn reconcile_merge_sequences(gh_bin: &Path, root: &Path) -> MergeSequenceStats {
    reconcile_merge_sequences_with(gh_bin, root, None)
}

/// [`reconcile_merge_sequences`] with an optional open-PR listing the
/// review-conflict pass already read on this root this tick and did not
/// write after (#4429 follow-up), which saves this pass its own listing read.
/// `None` lists exactly as before. Either way the plan sees the same
/// [`sequence_prs`] cut of the one REST listing (#10349).
pub(super) fn reconcile_merge_sequences_with(
    gh_bin: &Path,
    root: &Path,
    prefetched: Option<&[super::open_pr_listing::RestPull]>,
) -> MergeSequenceStats {
    let mut stats = MergeSequenceStats::default();
    if !merge_sequence_enabled() {
        return stats;
    }
    let listed = match prefetched {
        Some(rows) => Ok(sequence_prs(rows)),
        None => list_open_prs(gh_bin, root),
    };
    let open = match listed {
        Ok(v) => v,
        Err(e) => {
            log::warn!("claim_reconciliation (merge sequence): {}: {e}", root.display());
            crate::rate_limit_breaker::global_observe_failure(
                &e.to_string(),
                "claim_reconciliation",
            );
            return stats;
        }
    };
    stats.checked = open.len();
    if open.len() <= TRIGGER_OPEN_PRS {
        return stats;
    }
    let max_age = max_age_hours();
    let (now, stall_bound) = (Utc::now(), stall_hours());
    let mut stalls = stall::StallLedger::default();
    // One changed-files read per PR per tick, shared by both phases (#10077).
    let mut files = overlap::TickFiles::default();

    // Phase 1: evaluate every existing hold, oldest first for a stable
    // transcript.
    let holders: Vec<SequencePr> = open
        .iter()
        .filter(|p| p.has(SEQUENCE_LABEL))
        .cloned()
        .collect();
    let bin = gh_bin.to_string_lossy().to_string();
    // Each holder's completed marker read, reused by Phase 2 (#10089).
    let mut read_markers: BTreeMap<u32, Option<SequenceMarker>> = BTreeMap::new();
    for pr in &holders {
        let parsed = match holder_marker(&bin, root, pr) {
            Some(m) => m,
            None => {
                // The label gates merges regardless (#9378); a failed read
                // never releases anything.
                log::warn!(
                    "claim_reconciliation (merge sequence): PR #{} in {}: could not read trusted \
                     comments — hold left in place (fail closed)",
                    pr.number,
                    root.display()
                );
                stats.held += 1;
                continue;
            }
        };
        read_markers.insert(pr.number, parsed.clone());
        let Some(marker) = parsed else {
            // A label with no trusted marker is an operator-held PR (the
            // manual shape #9378 documented): nothing here owns releasing it.
            log::info!(
                "claim_reconciliation (merge sequence): PR #{} in {} carries {} with no trusted \
                 sequence marker — treated as a manual hold",
                pr.number,
                root.display(),
                SEQUENCE_LABEL
            );
            stats.held += 1;
            continue;
        };
        let pred = predecessor(&bin, root, &open, marker.after);
        // The head's labels and freshness come from this tick's listing; a
        // predecessor outside it is never treated as stalled (fail closed).
        let head = open.iter().find(|p| p.number == marker.after);
        let cause = head.and_then(|h| stall::stall_cause(h, now, stall_bound));
        let approved = pr.has("loom:pr");
        let action = stall::hold_action_with_stall(
            &marker,
            pred.as_ref(),
            pr.head_sha.as_deref(),
            approved,
            max_age,
            cause.as_ref(),
        );
        if let (Some(h), Some(c), true) = (head, cause.as_ref(), approved) {
            if matches!(action, HoldAction::ReleaseStalled | HoldAction::HoldHard) {
                let quiet = stall::quiet_hours(h, now).unwrap_or(stall_bound);
                stalls.record(h, c, quiet, pr.number, action == HoldAction::ReleaseStalled);
            }
        }
        // #10371: a soft hold behind a predecessor that is not ready to land
        // (as this tick's listing shows it) is released.
        let action = ready::with_readiness(action, &marker, pred.as_ref(), pr, head);
        // #10077: a soft hold between PRs sharing no file is released.
        let fetch = |p: &SequencePr| changed_files(gh_bin, root, p);
        let action =
            overlap::with_no_overlap(action, &marker, pred.as_ref(), pr, &open, &mut files, fetch);
        let result = match action {
            HoldAction::Release
            | HoldAction::ReleaseDissolved
            | HoldAction::Expire
            | HoldAction::ReleaseStalled
            | HoldAction::ReleaseNoOverlap
            | HoldAction::ReleaseNotReady => {
                release_hold(gh_bin, root, pr.number, &release_comment_body(&marker, action))
                    .map(|_| action)
            }
            HoldAction::VoidAndReplan => {
                release_hold(gh_bin, root, pr.number, REPLAN_NOTE_BODY).map(|_| action)
            }
            HoldAction::HoldSoft | HoldAction::HoldHard => Ok(action),
        };
        match result {
            Ok(HoldAction::Release) => stats.released += 1,
            Ok(HoldAction::ReleaseDissolved) => stats.released += 1,
            Ok(HoldAction::Expire) => stats.expired += 1,
            Ok(HoldAction::ReleaseStalled) => stats.stall_released += 1,
            Ok(HoldAction::ReleaseNoOverlap) => stats.overlap_released += 1,
            Ok(HoldAction::ReleaseNotReady) => stats.not_ready_released += 1,
            Ok(HoldAction::VoidAndReplan) => stats.voided += 1,
            Ok(_) => stats.held += 1,
            Err(e) => {
                stats.held += 1;
                log::warn!(
                    "claim_reconciliation (merge sequence): PR #{} in {}: {e}",
                    pr.number,
                    root.display()
                );
            }
        }
    }

    // Phase 1b: one escalation per stalled chain (#10060), deduped on the
    // head PR's own trusted comments.
    for chain in stalls.chains.iter().filter(|c| c.needs_escalation()) {
        match stall::escalate(gh_bin, root, chain, stall_bound) {
            Ok(true) => stats.escalated += 1,
            Ok(false) => {}
            Err(e) => log::warn!(
                "claim_reconciliation (merge sequence): stalled-chain escalation on PR #{} in \
                 {}: {e}",
                chain.head,
                root.display()
            ),
        }
    }

    // Phase 2: plan the eligible set INCLUDING existing holders (Judge
    // re-review of #9707): a new PR overlapping a held predecessor must be
    // chained behind it, not treated as unrelated for the life of the hold.
    // Holders never receive NEW edges — their holds are Phase 1's to
    // release or void — and a hold voided this tick is re-planned NEXT tick
    // from a fresh listing, so a void can never be replaced by a plan built
    // from the same stale read that voided it.
    let holder_numbers: BTreeSet<u32> = holders.iter().map(|h| h.number).collect();
    let eligible: Vec<&SequencePr> = open
        .iter()
        .filter(|p| p.eligible_for_ordering() && p.pinnable())
        .collect();
    if eligible.len() < 2 {
        return stats;
    }
    for pr in &eligible {
        files.get_or_fetch(pr, |p| changed_files(gh_bin, root, p));
    }
    let files = files.known(eligible.iter().map(|p| p.number));
    let markers = fetch_markers(gh_bin, root, &eligible, &read_markers);
    for group in plan_repo(&open, &files, &markers) {
        stats.groups += 1;
        for edge in group.edges {
            // Never re-plan a follower that already carries an ordering
            // state: a trusted marker (any author — a manual marker without
            // a label is still someone's stated order) or a hold label
            // present at tick start.
            if markers.contains_key(&edge.follower) || holder_numbers.contains(&edge.follower) {
                continue;
            }
            let marker = edge_marker(&edge);
            match apply_edge(gh_bin, root, &edge, &marker) {
                Ok(true) => {
                    stats.applied += 1;
                    log::info!(
                        "claim_reconciliation (merge sequence): PR #{} in {} now sequenced after \
                         #{} (plan {}, {:?})",
                        edge.follower,
                        root.display(),
                        edge.after,
                        edge.plan,
                        edge.reason
                    );
                }
                Ok(false) => stats.held += 1,
                Err(e) => log::warn!(
                    "claim_reconciliation (merge sequence): failed to sequence PR #{} after #{} \
                     in {}: {e}",
                    edge.follower,
                    edge.after,
                    root.display()
                ),
            }
        }
    }
    stats
}

#[cfg(test)]
#[path = "merge_sequence_tests.rs"]
mod tests;
