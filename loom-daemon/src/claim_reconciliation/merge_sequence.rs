//! Landing-order planning for stacked PRs (#9686, narrowed by #12775 from
//! "overlapping PRs" to stacked pairs only).
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
//! - **Plan (#12775).** Only STACKED pairs are ordered: eligible open PRs (not
//!   draft, no hold label, no agent mid-flight, pinnable head) form a group
//!   when a follower's base branch is a predecessor's head branch AND the pair
//!   shares a changed file. PRs that merely overlap — however really they
//!   conflict — are never ordered against each other; the planner reads no
//!   `git merge-tree` verdict and no readiness. A stacking cycle skips its
//!   component for that tick.
//! - **Apply.** The follower gets `loom:sequenced` (#9378's durable gate) and a
//!   trusted `source=pass` marker pinning both heads and the plan id.
//!   Followers that already carry a trusted marker are never re-planned —
//!   a human's or another pass's "after" wins.
//! - **Retired automatic holds (#12775).** A recorded `source=pass` hold the
//!   pass itself generated between independent (not stacked) PRs is released
//!   once ([`retired_generated_hold`]), on positive evidence only; hard,
//!   markerless, consolidation, stacked and unreadable holds stay.
//! - **Release / replan.** Every open holder is re-evaluated with #9378's
//!   [`evaluate`]: predecessor landed at the recorded head ⇒ release;
//!   predecessor closed unmerged ⇒ release with that reason; any moved head
//!   ⇒ the stale hold is voided (label off, replan note) and the next tick
//!   re-derives a fresh, correctly-pinned plan — except a follower head move
//!   the forge proves tree-identical (a re-date), which keeps the hold as is;
//!   and an operator's removal of the label is a sticky release for that
//!   pair until the PR's tree changes (#10398, [`sticky`]). Soft
//!   (`source=pass`) holds on approved followers expire when the predecessor
//!   has been quiet for `LOOM_MERGE_SEQUENCE_MAX_AGE_HOURS` (default 72) — a scheduling
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
//!   before #10060) or do not really conflict (#10350) is released; any
//!   failed read or unknown keeps it.
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
//! write is idempotent: the landing comment is upserted by its order key
//! (after PR, both heads — not the churning plan id), duplicates of it are
//! deleted, and one host per workspace runs the pass ([`landing`], #10634),
//! so competing daemons converge instead of spamming.
//!
//! Kill switch: [`MERGE_SEQUENCE_ENABLED_ENV`] (default ON), nested inside
//! the master `LOOM_STALE_CLAIM_RECONCILE` switch like the review-conflict
//! pass. Read-only inspection: `loom-daemon merge-pr sequence-plan`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
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

// Live CI and mergeability: "landable now" feeds `ready` (#12010).
#[path = "merge_sequence_landable.rs"]
pub mod landable;

// Tree-identical re-dates keep a hold; operator releases stick (#10398).
#[path = "merge_sequence_sticky.rs"]
pub mod sticky;

// One landing-order comment per follower, upserted by key (#10634).
#[path = "merge_sequence_landing.rs"]
pub mod landing;
// The real-merge-conflict pair predicate edges require (#10350).
#[path = "merge_sequence_conflict.rs"]
pub mod conflict;

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

/// Each eligible follower's stacked predecessor: the eligible PR whose head
/// branch is the follower's base branch (the lowest number if several share
/// that head). The only relation the pass orders by since #12775.
#[must_use]
pub fn stacked_preds(open_prs: &[SequencePr]) -> BTreeMap<u32, u32> {
    let eligible: Vec<&SequencePr> = open_prs
        .iter()
        .filter(|p| p.eligible_for_ordering() && p.pinnable())
        .collect();
    let mut pred_of = BTreeMap::new();
    for fol in &eligible {
        if fol.base_ref.is_empty() {
            continue;
        }
        if let Some(after) = eligible
            .iter()
            .filter(|o| o.number != fol.number && o.head_ref == fol.base_ref)
            .map(|o| o.number)
            .min()
        {
            pred_of.insert(fol.number, after);
        }
    }
    pred_of
}

/// The full plan for one repo's open set: stacked-base edges only. Pure.
///
/// A follower waits for its [`stacked_preds`] predecessor, provided the pair
/// still shares a changed file per `files` (a stacked pair that touches
/// disjoint files has never been ordered). Nothing else is planned: PRs that
/// merely overlap are never ordered against each other (#12775, #12344), and
/// trusted markers are not consulted because a follower that already carries
/// one is never re-planned. A stacking cycle skips its component whole. A
/// component is a connected set of stacked PRs; size-1 components are not
/// groups. Only PRs [`stacked_preds`] names need an entry in `files`.
#[must_use]
pub fn plan_repo(
    open_prs: &[SequencePr],
    files: &BTreeMap<u32, BTreeSet<String>>,
) -> Vec<SequenceGroup> {
    if open_prs.len() <= TRIGGER_OPEN_PRS {
        return Vec::new();
    }
    let eligible: BTreeMap<u32, &SequencePr> = open_prs
        .iter()
        .filter(|p| p.eligible_for_ordering() && p.pinnable())
        .map(|p| (p.number, p))
        .collect();
    let shares = |a: u32, b: u32| {
        files
            .get(&a)
            .zip(files.get(&b))
            .is_some_and(|(fa, fb)| fa.iter().any(|f| fb.contains(f)))
    };
    let pred_of: BTreeMap<u32, u32> = stacked_preds(open_prs)
        .into_iter()
        .filter(|(f, a)| shares(*f, *a))
        .collect();
    // Components over the stacked links (union by walking to the chain root).
    let mut components: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut cyclic: BTreeSet<u32> = BTreeSet::new();
    for n in pred_of.keys().chain(pred_of.values()) {
        let mut cur = *n;
        let mut seen = BTreeSet::from([cur]);
        while let Some(next) = pred_of.get(&cur) {
            if !seen.insert(*next) {
                cyclic.insert(*next);
                break;
            }
            cur = *next;
        }
        components.entry(cur).or_default().push(*n);
    }
    let mut groups = Vec::new();
    for (root, mut members) in components {
        members.sort_unstable();
        members.dedup();
        if cyclic.contains(&root) || members.iter().any(|m| cyclic.contains(m)) {
            log::info!(
                "merge_sequence: stacked component {members:?} has a cycle — skipped this tick"
            );
            continue;
        }
        let depth = |n: u32| {
            let mut d = 0;
            let mut cur = n;
            while let Some(next) = pred_of.get(&cur) {
                d += 1;
                cur = *next;
            }
            d
        };
        members.sort_by_key(|n| (depth(*n), eligible[n].created_at.clone(), *n));
        let pins: Vec<(u32, &str)> = members
            .iter()
            .filter_map(|n| eligible[n].head_sha.as_deref().map(|h| (*n, h)))
            .collect();
        let plan = plan_id(&pins);
        let edges = members
            .iter()
            .filter_map(|n| {
                let after = *pred_of.get(n)?;
                Some(SequenceEdge {
                    follower: *n,
                    after,
                    pred_head: eligible[&after].head_sha.clone()?,
                    follower_head: eligible[n].head_sha.clone()?,
                    plan: plan.clone(),
                })
            })
            .collect();
        groups.push(SequenceGroup {
            plan,
            order: members,
            edges,
        });
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
    /// A recorded hold this pass generated for PRs that are not stacked: the
    /// automatic ordering is retired (#12775), so release it. Decided by
    /// [`retired_generated_hold`] on positive evidence only.
    ReleaseRetired,
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

/// Is this recorded hold a retired, generated ordering (#12775)? Positive
/// evidence only: [`overlap::release_candidate`] (a trusted `source=pass`
/// marker, no `cons-` consolidation plan, the predecessor named by the marker
/// present in this tick's listing, and the follower's live base not that
/// predecessor's head) AND the follower is not stacked on any other open PR.
/// Hard holds, label-without-marker, consolidation plans, stacked pairs and
/// an unreadable predecessor all fail it and stay held.
#[must_use]
pub fn retired_generated_hold(
    marker: &SequenceMarker,
    follower: &SequencePr,
    open: &[SequencePr],
) -> bool {
    let pred = open.iter().find(|p| p.number == marker.after);
    overlap::release_candidate(marker, follower, pred)
        && !open
            .iter()
            .any(|o| o.number != follower.number && o.head_ref == follower.base_ref)
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
pub fn apply_comment_body(marker: &SequenceMarker) -> String {
    let why = format!("its base branch is #{}'s head branch", marker.after);
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
pub fn release_comment_body(
    marker: &SequenceMarker,
    action: HoldAction,
    head: Option<&SequencePr>,
) -> String {
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
        HoldAction::ReleaseRetired => format!(
            "automatic ordering between PRs that are not stacked has been retired — #{} was only \
             a scheduling preference derived from overlapping changes, not an explicit dependency",
            marker.after
        ),
        HoldAction::ReleaseNoOverlap => overlap::release_reason(marker),
        HoldAction::ReleaseNotReady => ready::release_reason(marker, head),
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
    /// Generated holds released because automatic ordering is retired (#12775).
    pub retired_released: usize,
    /// Stalled-chain escalations posted this tick (#10060).
    pub escalated: usize,
    pub held: usize,
}

// --- Forge reads --------------------------------------------------------

/// Run a `gh pr <comment|edit…>` write in `root` with the per-root credential
/// and `LOOM_REPO` applied — the same invocation shape as the review-conflict
/// pass. There is no read arm: the label side of [`apply_edge`] comes from the
/// tick-start REST listing (#10507), so this pass spends no GraphQL read here.
fn gh_pr(gh_bin: &Path, root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let inv = match args.first().copied() {
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

/// #10382: ETag'd REST `pulls/{n}/files` (was GraphQL `gh pr view --json
/// files`), so a re-read after `CHANGED_FILES` expires is a free `304`.
fn fetch_changed_files(gh_bin: &Path, root: &Path, number: u32) -> Option<BTreeSet<String>> {
    let caller = "claim_reconciliation.pr_files";
    crate::forge_pull_listing::pull_files_cached_as(caller, gh_bin, Some(root), None, number).ok()
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
    /// Suppressed edges whose follower's trusted marker already states this
    /// very order (same `LandingKey`). Disjoint from `skipped_held` (#9771).
    pub already_planned: usize,
    pub holders: usize,
    /// Every other suppressed edge: the follower carries a different order
    /// (marker or hold label) or a sticky operator release (#10398). With
    /// `already_planned`, leaves `groups` holding exactly what the live pass
    /// would write (Judge re-review of #9707).
    pub skipped_held: usize,
    /// Existing holds Phase 1 would release as no-overlap (#10077):
    /// `(follower, predecessor)`.
    pub would_release_no_overlap: Vec<(u32, u32)>,
    /// Generated holds Phase 1 would release as retired (#12775):
    /// `(follower, predecessor)`.
    pub would_release_retired: Vec<(u32, u32)>,
    /// Existing holds and why each is still kept (#12595):
    /// `(follower, predecessor, reason)`. Read-only; nothing is written.
    pub held_because: Vec<(u32, u32, String)>,
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
    let open = landable::tag_live(gh_bin, root, &open, true);
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
    let pairs = conflict::live(root);
    let conflicts = |a: &SequencePr, b: &SequencePr| pairs.conflicts(a, b);
    report.would_release_no_overlap =
        overlap::would_release(gh_bin, root, &open, &mut cache, &conflicts);
    let markers = fetch_markers(gh_bin, root, &eligible, &BTreeMap::new());
    let stacked = stacked_preds(&open);
    for pr in eligible
        .iter()
        .filter(|p| stacked.contains_key(&p.number) || stacked.values().any(|a| *a == p.number))
    {
        cache.get_or_fetch(pr, |p| changed_files(gh_bin, root, p));
    }
    report.groups = plan_repo(&open, &cache.known(stacked.iter().flat_map(|(f, a)| [*f, *a])));
    let bin = gh_bin.to_string_lossy().to_string();
    let open_ready = ready::demote_unlandable(&open, &markers);
    for (n, marker) in &markers {
        let Some(follower) = open.iter().find(|p| p.number == *n) else {
            continue;
        };
        let pred = predecessor(&bin, root, &open, marker.after);
        let head = open_ready.iter().find(|p| p.number == marker.after);
        if retired_generated_hold(marker, follower, &open) {
            report.would_release_retired.push((*n, marker.after));
        }
        let why = ready::hold_reason(marker, pred.as_ref(), follower, head);
        report.held_because.push((*n, marker.after, why));
    }
    for g in &mut report.groups {
        g.edges.retain(|e| {
            // #10634: the order key, not the plan id — the live pass keeps a
            // comment whose key matches, whatever plan id it carries.
            if markers.get(&e.follower).is_some_and(|m| {
                landing::LandingKey::of(m) == landing::LandingKey::of(&edge_marker(e))
            }) {
                report.already_planned += 1;
                return false;
            }
            if markers.contains_key(&e.follower) || holder_numbers.contains(&e.follower) {
                report.skipped_held += 1;
                return false;
            }
            // #10398: a sticky operator release is skipped (read-only here).
            let bodies = fetch_trusted_bodies(&bin, root, "{owner}/{repo}", e.follower);
            if sticky::check(gh_bin, root, e, &open, &bodies.unwrap_or_default())
                != sticky::OperatorRelease::None
            {
                report.skipped_held += 1;
                return false;
            }
            true
        });
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

/// Apply one planned edge: landing comment + label, idempotent PER SIDE.
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
///
/// The comment side is [`landing::plan`] over this follower's fresh trusted
/// comments (#10634): keep, patch in place, or post once, keyed by the order
/// rather than the plan id. The label side comes from the tick-start REST
/// listing (`label_present`, #10507), not a per-follower `gh pr view`: the
/// caller only reaches here for followers that were NOT holders in that
/// listing. The one race — another daemon adds the label between the listing
/// and this write — costs one redundant `--add-label`, which the forge no-ops.
fn apply_edge(
    gh_bin: &Path,
    root: &Path,
    edge: &SequenceEdge,
    plan: &landing::LandingPlan,
    label_present: bool,
) -> Result<bool> {
    let wrote = if plan.write == landing::LandingWrite::Keep && label_present {
        false
    } else {
        landing::write(gh_bin, root, edge.follower, plan)?;
        if !label_present {
            let n = edge.follower.to_string();
            gh_pr(gh_bin, root, &["edit", &n, "--add-label", SEQUENCE_LABEL])?;
        }
        true
    };
    landing::delete_duplicates(gh_bin, root, edge.follower, plan);
    Ok(wrote)
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
    // #10634: one host per workspace plans and posts — the role-runner shard
    // (#6374/#6704) that already makes one host of a sharded fleet own a
    // workspace's role ticks. Unsharded
    // hosts own everything, so a lone daemon is unaffected. Releasing an
    // existing hold is not gated (see `reconcile_merge_sequences_gated`).
    let owned = cfg!(test) || crate::role_shard::decide(root).admits_role_tick();
    reconcile_merge_sequences_gated(gh_bin, root, prefetched, owned)
}

/// [`reconcile_merge_sequences_with`] with the shard verdict supplied
/// (#10634). A host that does not own `root` still runs Phase 1's releases —
/// they are idempotent, and a slice with no live owner (its static-shard host
/// down, a roster yielding everywhere, the owner skipped by write scope) must
/// not keep `loom:sequenced` on PRs that `merge-pr.sh` then refuses (Judge,
/// PR #10651). It does not plan or post: no new landing-order comment, no
/// re-anchor record, no stall escalation. With no hold in the listing it
/// makes no forge call beyond the listing.
pub(super) fn reconcile_merge_sequences_gated(
    gh_bin: &Path,
    root: &Path,
    prefetched: Option<&[super::open_pr_listing::RestPull]>,
    owned: bool,
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
    // #12010: a ready PR that is red or conflicting now is not ready.
    let open = landable::tag_live(gh_bin, root, &open, owned);
    let max_age = max_age_hours();
    let (now, stall_bound) = (Utc::now(), stall_hours());
    let mut stalls = stall::StallLedger::default();
    // One changed-files read per PR per tick, shared by both phases (#10077).
    let mut files = overlap::TickFiles::default();
    // One real-conflict predicate per tick, shared by both phases (#10350).
    let pairs = conflict::live(root);
    let conflicts = |a: &SequencePr, b: &SequencePr| pairs.conflicts(a, b);

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
    // #10465: readiness is transitive across the recorded holds, so every
    // holder's marker is read up front (once; the loop below reuses it).
    let holder_reads: BTreeMap<u32, Option<Option<SequenceMarker>>> = holders
        .iter()
        .map(|p| (p.number, holder_marker(&bin, root, p)))
        .collect();
    let held_markers: BTreeMap<u32, SequenceMarker> = holder_reads
        .iter()
        .filter_map(|(n, m)| Some((*n, m.clone()??)))
        .collect();
    let open_ready = ready::demote_unlandable(&open, &held_markers);
    for pr in &holders {
        let parsed = match holder_reads.get(&pr.number).cloned().flatten() {
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
        // #10398: a tree-identical re-date is evaluated at the pinned head.
        let held = sticky::effective_follower(&marker, pr, |pinned, live| {
            sticky::forge_same_tree(gh_bin, root, pinned, live)
        });
        let pr: &SequencePr = &held;
        let retire = retired_generated_hold(&marker, pr, &open);
        let pred = predecessor(&bin, root, &open, marker.after);
        // #10465: a predecessor head that moved by a tree-identical commit is
        // re-anchored, not voided; the hold is evaluated at the new head.
        let reanchored = sticky::reanchor_predecessor(&marker, pred.as_ref(), |pinned, live| {
            sticky::forge_same_tree(gh_bin, root, pinned, live)
        });
        let marker = reanchored.clone().unwrap_or(marker);
        // The head's labels and freshness come from this tick's listing; a
        // predecessor outside it is never treated as stalled (fail closed).
        let head = open.iter().find(|p| p.number == marker.after);
        let cause = head.and_then(|h| stall::stall_cause(h, now, stall_bound));
        let approved = pr.has("loom:pr");
        let action = if retire {
            HoldAction::ReleaseRetired
        } else {
            stall::hold_action_with_stall(
                &marker,
                pred.as_ref(),
                pr.head_sha.as_deref(),
                approved,
                max_age,
                cause.as_ref(),
            )
        };
        if let (Some(h), Some(c), true) = (head, cause.as_ref(), approved) {
            if matches!(action, HoldAction::ReleaseStalled | HoldAction::HoldHard) {
                let quiet = stall::quiet_hours(h, now).unwrap_or(stall_bound);
                stalls.record(h, c, quiet, pr.number, action == HoldAction::ReleaseStalled);
            }
        }
        // #10371: a soft hold behind a predecessor that is not ready to land
        // (as this tick's listing shows it) is released.
        let ready_head = open_ready.iter().find(|p| p.number == marker.after);
        let action = ready::with_readiness(action, &marker, pred.as_ref(), pr, ready_head);
        // #10077: a soft hold between PRs sharing no file is released.
        let fetch = |p: &SequencePr| changed_files(gh_bin, root, p);
        let (p, open) = (pred.as_ref(), open.as_slice());
        let action =
            overlap::with_no_overlap(action, &marker, p, pr, open, &mut files, fetch, &conflicts);
        let result = match action {
            HoldAction::Release
            | HoldAction::ReleaseDissolved
            | HoldAction::Expire
            | HoldAction::ReleaseStalled
            | HoldAction::ReleaseNoOverlap
            | HoldAction::ReleaseNotReady
            | HoldAction::ReleaseRetired => {
                let body = release_comment_body(&marker, action, ready_head);
                release_hold(gh_bin, root, pr.number, &body).map(|_| action)
            }
            HoldAction::VoidAndReplan => {
                release_hold(gh_bin, root, pr.number, REPLAN_NOTE_BODY).map(|_| action)
            }
            HoldAction::HoldSoft | HoldAction::HoldHard => match reanchored.as_ref() {
                // Re-anchor records are posting: the owner writes them.
                Some(m) if owned => {
                    sticky::record_reanchor(gh_bin, root, pr.number, m).map(|_| action)
                }
                _ => Ok(action),
            },
        };
        match result {
            Ok(HoldAction::Release) => stats.released += 1,
            Ok(HoldAction::ReleaseDissolved) => stats.released += 1,
            Ok(HoldAction::Expire) => stats.expired += 1,
            Ok(HoldAction::ReleaseStalled) => stats.stall_released += 1,
            Ok(HoldAction::ReleaseNoOverlap) => stats.overlap_released += 1,
            Ok(HoldAction::ReleaseNotReady) => stats.not_ready_released += 1,
            Ok(HoldAction::ReleaseRetired) => stats.retired_released += 1,
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

    // #10634: everything below plans or posts — the workspace's owner only.
    if !owned {
        log::debug!(
            "claim_reconciliation (merge sequence): {} is owned by another host's shard — \
             holds evaluated, planning skipped (#10634)",
            root.display()
        );
        return stats;
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
    // Only stacked PRs need their changed files read (#12775).
    let stacked = stacked_preds(&open);
    for pr in eligible
        .iter()
        .filter(|p| stacked.contains_key(&p.number) || stacked.values().any(|a| *a == p.number))
    {
        files.get_or_fetch(pr, |p| changed_files(gh_bin, root, p));
    }
    let plan_files = files.known(stacked.iter().flat_map(|(f, a)| [*f, *a]));
    let markers = fetch_markers(gh_bin, root, &eligible, &read_markers);
    let fleet = crate::forge_identity::FleetLogins::for_root(root);
    for group in plan_repo(&open, &plan_files) {
        stats.groups += 1;
        for edge in group.edges {
            // Never re-plan a follower that already carries an ordering
            // state: a trusted marker (any author — a manual marker without
            // a label is still someone's stated order) or a hold label
            // present at tick start.
            if markers.contains_key(&edge.follower) || holder_numbers.contains(&edge.follower) {
                continue;
            }
            // #10634: one read gives the sticky check its bodies and the
            // landing upsert its comment ids. A failed read skips the edge —
            // "no comment" read from a read that did not happen is a repost.
            let Some(thread) = landing::fetch_thread(gh_bin, root, edge.follower) else {
                log::warn!(
                    "claim_reconciliation (merge sequence): PR #{} in {}: could not read \
                     trusted comments — edge skipped this tick",
                    edge.follower,
                    root.display()
                );
                continue;
            };
            let bodies = landing::bodies(&thread);
            // #10398: an operator's release of this pair sticks while the tree holds.
            match sticky::check(gh_bin, root, &edge, &open, &bodies) {
                sticky::OperatorRelease::None => {}
                found => {
                    if let sticky::OperatorRelease::Detected(head) = &found {
                        if let Err(e) = sticky::record(gh_bin, root, &edge, head, &bodies) {
                            log::warn!(
                                "claim_reconciliation (merge sequence): PR #{}: {e}",
                                edge.follower
                            );
                        }
                    }
                    stats.held += 1;
                    continue;
                }
            }
            let plan = landing::plan(&thread, &edge_marker(&edge), |l| fleet.contains(l));
            // Label side from the tick-start listing (#10507); see `apply_edge`.
            let label_present = open
                .iter()
                .any(|p| p.number == edge.follower && p.has(SEQUENCE_LABEL));
            match apply_edge(gh_bin, root, &edge, &plan, label_present) {
                Ok(true) => {
                    stats.applied += 1;
                    log::info!(
                        "claim_reconciliation (merge sequence): PR #{} in {} now sequenced after \
                         #{} (plan {}, stacked)",
                        edge.follower,
                        root.display(),
                        edge.after,
                        edge.plan
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
