//! The step engine's item model and its state → step classifier (#11895;
//! design: #11894, `docs/design/work-finder-step-dispatch.md` §4.1).
//!
//! # What an item is
//!
//! An **item** is an open issue joined to its linked open PR. The link rules
//! are the open-PR guard's ([`crate::worktree_ops::linked_pr_listing`]): a
//! `Closes` / `Part of` phrase in the PR body, or the Builder branch
//! `feature/issue-N` in this same repo, with a fork PR by a known-untrusted
//! author never linking. An issue joins its **lowest-numbered** linked PR, as
//! the guard answers. A PR that joins no issue (no link, an issue not in the
//! listing, or a second PR on an issue that already joined one) is its own
//! item. A PR linking several issues joins the lowest of them; the others see
//! [`PassReason::SharedPr`], so one PR is never two items.
//!
//! A **step** is one role session that advances one item by one stage. This
//! module answers, for every item, which step its state calls for, or why it
//! has none ([`Verdict::Pass`]). A pass is weight 0 in the draw.
//!
//! # Pure
//!
//! No forge call, no environment read, no clock. Everything the classifier
//! reads arrives in [`Inputs`]; the facts that need the forge (whether a
//! dependency park's blockers have closed, which merged PRs close an issue,
//! whether a PR sits in the merge queue, trusted comment markers) are computed
//! by the caller. The adapters at the bottom of this file derive the
//! body-borne facts with the parsers that already own them, so a caller never
//! re-implements one. The fixture `loom-daemon/tests/fixtures/
//! step-item-classifier.json` pins the table for a later TypeScript twin.
//!
//! # The table (design §4.1)
//!
//! Checked in this order; the first rule that matches decides.
//!
//! | Item state | Verdict |
//! |---|---|
//! | `loom:frozen` (either side) | pass `frozen` |
//! | `loom:operator`, or a retired name that folds into it (either side) | pass `operator` |
//! | another hard exclusion (`external`) | pass `hard_exclusion` |
//! | a claim label (see below) | pass `claimed` |
//! | PR in GitHub's merge queue | pass `queued` |
//! | issue's linked PR is another item's | pass `shared_pr` |
//! | PR `loom:sequenced` | pass `sequenced` |
//! | PR `loom:pr` + `loom:blocked` + trusted Champion hold marker | Champion hold re-evaluation |
//! | PR `loom:changes-requested` + `loom:blocked` + hold marker, Doctor cycle not capped | Doctor |
//! | PR `loom:review-requested` + `loom:blocked` + hold marker | Judge |
//! | any other blocked PR | pass `blocked` |
//! | PR `loom:changes-requested` | Doctor |
//! | PR `loom:review-requested` | Judge |
//! | PR `loom:pr` | Champion merge |
//! | PR with none of those | pass `no_stage` |
//! | issue `loom:epic` | pass `epic` |
//! | issue `loom:issue` + `loom:blocked`, capability-only park, every capability held | Builder (capability lane) |
//! | the same, a capability not held | pass `capability_missing` |
//! | `loom:curated` (or a proposal label) + `loom:blocked`, **dependency park**, every blocker closed | Champion re-evaluate |
//! | any other blocked issue | pass `blocked` |
//! | landed: a merged **closing** PR, issue still open | Curator keep-or-close |
//! | `loom:decision-malformed` | Curator |
//! | `loom:issue` | Builder |
//! | `loom:curated`, `loom:architect`, `loom:hermit`, `loom:auditor` | Champion promote |
//! | `loom:triage`, or no stage label | Curator |
//!
//! Notes on the rules that are easy to get wrong:
//!
//! - **Landed.** Only a merged PR that *closes* the issue lands it. A merged
//!   `Part of #N` PR is a phase of unfinished work, so the issue keeps its
//!   stage (the same rule as [`super::landed_prefilter`]). A landing Curator
//!   already saw and kept (the trusted route marker naming that PR) does not
//!   route it again.
//! - **Park kinds.** Only a *dependency* park — a park record naming a blocker
//!   — is re-evaluable when its blockers close. A quarantine (and the
//!   fast-fail park, which parks through the quarantine, #11854), the PR-less
//!   hold, a Champion hold record and an unstated park name no blocker, so
//!   Champion `--issue`'s "every blocker closed" check would be vacuously true
//!   for them (`champion-targeted.md`, `--issue` step 1). A hold record beside
//!   a dependency record keeps the hold's kind: fail closed.
//! - **Claims.** The registry's `claim` labels. On an item with an open PR the
//!   issue's `loom:building` is **not** a claim: it stays on the issue until
//!   the closing merge (#6199), so reading it as one would pass every PR.
//! - **Markers** come from trusted comments only ([`crate::comment_trust`], as
//!   #11885 reads the revise and verdict markers): the caller passes the
//!   pre-computed [`PullMarkers`] / [`IssueMarkers`], never raw comments.
//!   Champion's promotion verdict marker is deliberately not an input: a
//!   promotion verdict is terminal (#11860), and Champion `--issue` reads the
//!   marker itself to go straight to Step 4's bound. Passing a curated issue
//!   for carrying one would keep it from ever reaching that bound (#4967).
//! - **Retired labels** are read as their replacement through the registry
//!   (`defaults/labels.json`), so an unmigrated `loom:operator-only` item
//!   passes and an unmigrated revise label reads as `loom:triage`.
//! - **Lanes.** A red-main fix keeps its lane ([`Lane::RedMain`], the draw's
//!   priority boost and backoff exemption, [`super::main_red_fix`]); a
//!   capability-only park is [`Lane::Capability`] (propose mode, #6893) and
//!   weight 0 on a host without the capability.
//!
//! Nothing calls this yet: the step engine is dark until a later child of
//! #11894 wires the draw (`#[allow(dead_code)]` is not needed: the items are
//! `pub` in the library crate).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::label_registry::Registry;
use crate::worktree_ops::linked_pr_listing::linked_issues;

// ============================================================================
// Labels the table names
// ============================================================================

const TRIAGE: &str = "loom:triage";
const CURATED: &str = "loom:curated";
const READY: &str = "loom:issue";
const BUILDING: &str = super::BUILDING_LABEL;
const REVIEW_REQUESTED: &str = "loom:review-requested";
const CHANGES_REQUESTED: &str = "loom:changes-requested";
const APPROVED: &str = "loom:pr";
const SEQUENCED: &str = "loom:sequenced";
const FROZEN: &str = "loom:frozen";
const DECISION_MALFORMED: &str = "loom:decision-malformed";

// ============================================================================
// Inputs
// ============================================================================

/// Everything one classification reads, for one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inputs {
    /// The open issues.
    #[serde(default)]
    pub issues: Vec<IssueInput>,
    /// The open pull requests.
    #[serde(default)]
    pub pulls: Vec<PullInput>,
    /// This repository's lanes.
    #[serde(default)]
    pub repo: RepoInput,
    /// The host the draw runs on.
    #[serde(default)]
    pub host: HostInput,
}

/// One open issue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueInput {
    pub number: u32,
    /// The labels as listed; retired names are folded here, not by the caller.
    #[serde(default)]
    pub labels: Vec<String>,
    /// The kind of `loom:blocked` park the body declares ([`park_kind`]).
    #[serde(default)]
    pub park: ParkKind,
    /// A [`ParkKind::Dependency`] park whose every named blocker is a closed
    /// issue or a merged PR (the caller reads the forge). Ignored for every
    /// other kind.
    #[serde(default)]
    pub blockers_closed: bool,
    /// The capabilities a [`ParkKind::Capability`] park declares
    /// ([`declared_capabilities`]). ANDed.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Merged PRs that reference this issue, and how.
    #[serde(default)]
    pub merged_links: Vec<MergedLink>,
    /// Markers read from the issue's trusted comments.
    #[serde(default)]
    pub markers: IssueMarkers,
    /// The issue is a red-main fix the lane admits: it carries the
    /// `loom:main-red-fix` marker, filed by a trusted author when it is not
    /// yet `loom:issue` ([`super::main_red_fix`], #10118).
    #[serde(default)]
    pub red_fix: bool,
}

/// A merged PR that references an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedLink {
    pub pr: u32,
    pub kind: LinkKind,
}

/// How a merged PR references an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// In the issue's closes-graph (a closing keyword or a Development-sidebar
    /// link), merged after the issue was last reopened.
    Closes,
    /// A partial-increment reference (`Part of #N`, `Contributes to #N`), or a
    /// closing merge older than the issue's last reopen.
    PartOf,
}

/// Facts from an issue's **trusted** comments ([`crate::comment_trust`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueMarkers {
    /// Champion's revise marker for the current body stands unanswered
    /// (#11860): a send-back Curator answers at its Priority 0. Carried on
    /// the item for the draw; it does not change the step.
    #[serde(default)]
    pub send_back: bool,
    /// Merged PRs whose landed route marker Curator already kept
    /// ([`super::landed_prefilter::route_marker`]): their landing is not
    /// routed again.
    #[serde(default)]
    pub landed_kept: Vec<u32>,
}

/// The kind of `loom:blocked` park an issue carries, from its body park
/// records ([`crate::park_record`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkKind {
    /// No park record (or not blocked).
    #[default]
    None,
    /// A record naming a blocker: the one kind Champion re-evaluates.
    Dependency,
    /// The insta-crash quarantine, and the fast-fail park that parks through
    /// it with the same record (#3939, #11854).
    Quarantine,
    /// The daemon's PR-less retry hold (#7972).
    PrlessHold,
    /// A Champion merge-risk or critical-file hold record (#11861).
    ChampionHold,
    /// A capability-only park: capability markers, no named blocker, no
    /// decision block (#6893).
    Capability,
    /// A record that names no blocker and no reason the daemon knows.
    Unstated,
}

/// One open pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullInput {
    pub number: u32,
    #[serde(default)]
    pub labels: Vec<String>,
    /// The PR description, for its link phrases.
    #[serde(default)]
    pub body: Option<String>,
    /// The head branch name.
    #[serde(default)]
    pub head_ref: Option<String>,
    /// The head lives in this repository (not a fork).
    #[serde(default)]
    pub same_repo: bool,
    /// The author is known and untrusted (the H14 rule, #9548).
    #[serde(default)]
    pub author_untrusted: bool,
    /// The PR is in GitHub's merge queue: it is never drawn again for merge.
    #[serde(default)]
    pub queued: bool,
    /// Markers read from the PR's trusted comments.
    #[serde(default)]
    pub markers: PullMarkers,
}

/// Facts from a PR's **trusted** comments ([`PullMarkers::from_trusted`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullMarkers {
    /// A Champion hold stands ([`crate::champion_hold::standing_hold`]).
    #[serde(default)]
    pub champion_hold: bool,
    /// The Doctor-cycle cap was hit after the latest hold
    /// ([`crate::champion_hold::doctor_cycle_capped`]).
    #[serde(default)]
    pub doctor_cycle_capped: bool,
}

/// One repository's lanes this tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInput {
    /// `main` counts as red ([`super::RedMainLane::is_red`]).
    #[serde(default)]
    pub main_red: bool,
}

/// The host the draw runs on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInput {
    /// The capabilities this worker holds
    /// ([`crate::capability::held_capabilities`]).
    #[serde(default)]
    pub held_capabilities: BTreeSet<String>,
}

// ============================================================================
// Output
// ============================================================================

/// One item and its verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The issue, when the item has one.
    pub issue: Option<u32>,
    /// The linked open PR, when the item has one.
    pub pr: Option<u32>,
    pub state: ItemState,
    pub verdict: Verdict,
    pub lane: Lane,
    /// [`IssueMarkers::send_back`].
    pub send_back: bool,
}

impl Item {
    /// The step, or `None` for a pass.
    #[must_use]
    pub fn step(&self) -> Option<Step> {
        match self.verdict {
            Verdict::Step(step) => Some(step),
            Verdict::Pass(_) => None,
        }
    }

    /// The number the step's role is invoked on (`/loom:<role> N`): the PR for
    /// a PR step, else the issue. `None` for a pass.
    #[must_use]
    pub fn target(&self) -> Option<u32> {
        let step = self.step()?;
        if step.acts_on_pr() {
            self.pr
        } else {
            self.issue
        }
    }
}

/// What the item's labels say it is, before the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Unlabeled,
    Triage,
    DecisionMalformed,
    Curated,
    Ready,
    Landed,
    Blocked,
    Epic,
    ReviewRequested,
    ChangesRequested,
    Approved,
    Held,
    Sequenced,
    Queued,
    Claimed,
    Operator,
    Frozen,
    Excluded,
    SharedPr,
    NoStage,
}

/// A step, or why there is none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Step(Step),
    Pass(PassReason),
}

/// One role session that advances an item by one stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Curator,
    CuratorKeepOrClose,
    ChampionPromote,
    ChampionReevaluate,
    Builder,
    Judge,
    Doctor,
    ChampionMerge,
    ChampionHoldReevaluate,
}

impl Step {
    /// The role that runs the step.
    #[must_use]
    pub fn role(self) -> &'static str {
        match self {
            Step::Curator | Step::CuratorKeepOrClose => "curator",
            Step::ChampionPromote
            | Step::ChampionReevaluate
            | Step::ChampionMerge
            | Step::ChampionHoldReevaluate => "champion",
            Step::Builder => "builder",
            Step::Judge => "judge",
            Step::Doctor => "doctor",
        }
    }

    /// The step works the item's PR rather than its issue.
    #[must_use]
    pub fn acts_on_pr(self) -> bool {
        matches!(
            self,
            Step::Judge | Step::Doctor | Step::ChampionMerge | Step::ChampionHoldReevaluate
        )
    }
}

/// Why an item has no step this tick (weight 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassReason {
    Frozen,
    Operator,
    HardExclusion,
    Claimed,
    Queued,
    SharedPr,
    Sequenced,
    Blocked,
    CapabilityMissing,
    Epic,
    NoStage,
}

/// The dispatch lane an item rides in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    #[default]
    Normal,
    /// A red-main fix on a red repo ([`super::main_red_fix`]).
    RedMain,
    /// A capability-only park worked in propose mode (#6893).
    Capability,
}

// ============================================================================
// Join and classify
// ============================================================================

/// Join `inputs`' PRs to their issues and classify every item: issue items in
/// issue order, then PR-only items in PR order.
#[must_use]
pub fn classify(inputs: &Inputs) -> Vec<Item> {
    let open: BTreeSet<u32> = inputs.issues.iter().map(|i| i.number).collect();
    // Each issue's lowest-numbered linked PR, as the open-PR guard answers.
    let mut issue_pr: BTreeMap<u32, &PullInput> = BTreeMap::new();
    for pr in &inputs.pulls {
        for n in linked_issues(
            pr.body.as_deref(),
            pr.head_ref.as_deref(),
            pr.same_repo,
            pr.author_untrusted,
        ) {
            if !open.contains(&n) {
                continue;
            }
            let slot = issue_pr.entry(n).or_insert(pr);
            if pr.number < slot.number {
                *slot = pr;
            }
        }
    }
    // A PR joins the lowest issue that picked it; the others share it.
    let mut pr_owner: BTreeMap<u32, u32> = BTreeMap::new();
    for (&issue, pr) in &issue_pr {
        pr_owner.entry(pr.number).or_insert(issue);
    }

    let mut issues: Vec<&IssueInput> = inputs.issues.iter().collect();
    issues.sort_by_key(|i| i.number);
    let mut pulls: Vec<&PullInput> = inputs.pulls.iter().collect();
    pulls.sort_by_key(|p| p.number);

    let mut items = Vec::with_capacity(issues.len() + pulls.len());
    for issue in issues {
        let pr = issue_pr.get(&issue.number).copied();
        let shared = pr.is_some_and(|p| pr_owner.get(&p.number) != Some(&issue.number));
        items.push(classify_item(Some(issue), pr, shared, inputs));
    }
    for pr in pulls {
        if !pr_owner.contains_key(&pr.number) {
            items.push(classify_item(None, Some(pr), false, inputs));
        }
    }
    items
}

/// Classify one joined item. `shared`: the PR is another issue's item.
fn classify_item(
    issue: Option<&IssueInput>,
    pr: Option<&PullInput>,
    shared: bool,
    inputs: &Inputs,
) -> Item {
    let issue_labels = issue
        .map(|i| canonical_labels(&i.labels))
        .unwrap_or_default();
    let pr_labels = pr.map(|p| canonical_labels(&p.labels)).unwrap_or_default();
    let lane = if inputs.repo.main_red && issue.is_some_and(|i| i.red_fix) {
        Lane::RedMain
    } else {
        Lane::Normal
    };
    let (state, verdict) = decide(issue, &issue_labels, pr, &pr_labels, shared, inputs);
    let lane = match verdict {
        Verdict::Step(Step::Builder) if state == ItemState::Blocked => Lane::Capability,
        _ => lane,
    };
    Item {
        issue: issue.map(|i| i.number),
        pr: pr.map(|p| p.number),
        state,
        verdict,
        lane,
        send_back: issue.is_some_and(|i| i.markers.send_back),
    }
}

/// The §4.1 table, in order (module docs).
fn decide(
    issue: Option<&IssueInput>,
    issue_labels: &[String],
    pr: Option<&PullInput>,
    pr_labels: &[String],
    shared: bool,
    inputs: &Inputs,
) -> (ItemState, Verdict) {
    use PassReason as P;
    let pass = |s: ItemState, r: PassReason| (s, Verdict::Pass(r));
    let either = |f: &dyn Fn(&[String]) -> bool| f(issue_labels) || f(pr_labels);

    if either(&|l| has(l, FROZEN)) {
        return pass(ItemState::Frozen, P::Frozen);
    }
    if either(&|l| super::is_operator_hold(l)) {
        return pass(ItemState::Operator, P::Operator);
    }
    if either(&crate::hard_exclusion::is_hard_excluded) {
        return pass(ItemState::Excluded, P::HardExclusion);
    }
    if is_claimed(issue_labels, pr.is_some()) || is_claimed(pr_labels, true) {
        return pass(ItemState::Claimed, P::Claimed);
    }
    if let Some(pr) = pr {
        if pr.queued {
            return pass(ItemState::Queued, P::Queued);
        }
        if shared {
            return pass(ItemState::SharedPr, P::SharedPr);
        }
        return decide_pr(pr, pr_labels);
    }
    let Some(issue) = issue else {
        return pass(ItemState::NoStage, P::NoStage);
    };
    decide_issue(issue, issue_labels, &inputs.host)
}

/// The PR rows of the table.
fn decide_pr(pr: &PullInput, labels: &[String]) -> (ItemState, Verdict) {
    let step = |s: ItemState, st: Step| (s, Verdict::Step(st));
    if has(labels, SEQUENCED) {
        return (ItemState::Sequenced, Verdict::Pass(PassReason::Sequenced));
    }
    if super::is_blocked_hold(labels) {
        let hold = pr.markers.champion_hold;
        if hold && has(labels, APPROVED) && !has(labels, CHANGES_REQUESTED) {
            return step(ItemState::Held, Step::ChampionHoldReevaluate);
        }
        if hold && !pr.markers.doctor_cycle_capped && crate::champion_hold::doctor_shape(labels) {
            return step(ItemState::Held, Step::Doctor);
        }
        if hold && crate::champion_hold::judge_shape(labels) {
            return step(ItemState::Held, Step::Judge);
        }
        return (ItemState::Blocked, Verdict::Pass(PassReason::Blocked));
    }
    // Verdict-label precedence: `loom:pr` always loses to the other verdicts
    // (Champion's Verdict-State Janitor), so a contradictory PR is never merged.
    if has(labels, CHANGES_REQUESTED) {
        step(ItemState::ChangesRequested, Step::Doctor)
    } else if has(labels, REVIEW_REQUESTED) {
        step(ItemState::ReviewRequested, Step::Judge)
    } else if has(labels, APPROVED) {
        step(ItemState::Approved, Step::ChampionMerge)
    } else {
        (ItemState::NoStage, Verdict::Pass(PassReason::NoStage))
    }
}

/// The issue rows of the table (no open PR).
fn decide_issue(issue: &IssueInput, labels: &[String], host: &HostInput) -> (ItemState, Verdict) {
    let step = |s: ItemState, st: Step| (s, Verdict::Step(st));
    let pass = |s: ItemState, r: PassReason| (s, Verdict::Pass(r));
    if has(labels, super::EPIC_LABEL) {
        return pass(ItemState::Epic, PassReason::Epic);
    }
    if super::is_blocked_hold(labels) {
        if issue.park == ParkKind::Capability && has(labels, READY) {
            let held = !issue.capabilities.is_empty()
                && issue
                    .capabilities
                    .iter()
                    .all(|c| host.held_capabilities.contains(c));
            return if held {
                step(ItemState::Blocked, Step::Builder)
            } else {
                pass(ItemState::Blocked, PassReason::CapabilityMissing)
            };
        }
        if is_proposal(labels) && issue.park == ParkKind::Dependency && issue.blockers_closed {
            return step(ItemState::Blocked, Step::ChampionReevaluate);
        }
        return pass(ItemState::Blocked, PassReason::Blocked);
    }
    if is_landed(issue) {
        return step(ItemState::Landed, Step::CuratorKeepOrClose);
    }
    if has(labels, DECISION_MALFORMED) {
        step(ItemState::DecisionMalformed, Step::Curator)
    } else if has(labels, READY) {
        step(ItemState::Ready, Step::Builder)
    } else if is_proposal(labels) {
        step(ItemState::Curated, Step::ChampionPromote)
    } else if has(labels, TRIAGE) {
        step(ItemState::Triage, Step::Curator)
    } else {
        step(ItemState::Unlabeled, Step::Curator)
    }
}

/// A merged PR closes the issue, and Curator has not already kept that landing.
fn is_landed(issue: &IssueInput) -> bool {
    issue
        .merged_links
        .iter()
        .any(|l| l.kind == LinkKind::Closes && !issue.markers.landed_kept.contains(&l.pr))
}

/// A Champion promotion candidate: `loom:curated`, or a proposal label
/// Champion promotes directly (the registry's `champion_path` labels, less the
/// epic container).
fn is_proposal(labels: &[String]) -> bool {
    has(labels, CURATED)
        || labels.iter().any(|l| {
            l != super::EPIC_LABEL
                && Registry::embedded()
                    .get(l)
                    .is_some_and(|d| d.property("champion_path") == Some(true))
        })
}

/// A live claim among `labels`: any registry `claim` label, except the issue's
/// `loom:building` once a PR is open (it stays until the closing merge).
fn is_claimed(labels: &[String], building_is_not_a_claim: bool) -> bool {
    let claims = Registry::embedded().with_kind("claim");
    labels
        .iter()
        .any(|l| claims.contains(&l.as_str()) && !(building_is_not_a_claim && l == BUILDING))
}

/// `labels` with every retired name read as its replacement (dropped when it
/// has none), and the replacement's `also_remove` labels dropped (#11795,
/// #11860), through the registry.
fn canonical_labels(labels: &[String]) -> Vec<String> {
    let reg = Registry::embedded();
    let mut out: Vec<String> = Vec::with_capacity(labels.len());
    let mut drop: BTreeSet<String> = BTreeSet::new();
    for l in labels {
        match reg.get_retired(l) {
            Some(r) => {
                drop.extend(r.also_remove.iter().cloned());
                if let Some(to) = &r.replaced_by {
                    out.push(to.clone());
                }
            }
            None => out.push(l.clone()),
        }
    }
    out.retain(|l| !drop.contains(l));
    out.sort();
    out.dedup();
    out
}

fn has(labels: &[String], label: &str) -> bool {
    labels.iter().any(|l| l == label)
}

// ============================================================================
// Adapters: body- and comment-borne facts, from the parsers that own them
// ============================================================================

/// The kind of park `body` declares for an item carrying `labels`.
///
/// A hold record wins over a dependency record beside it (fail closed: a
/// quarantined issue with a stale `Blocked by:` line is still quarantined).
#[must_use]
pub fn park_kind(labels: &[String], body: Option<&str>) -> ParkKind {
    use crate::capability::{route_mechanical, MechanicalRouting};
    use crate::sweep_registry::park_hold::{self, PRLESS_HOLD_REASON, QUARANTINE_HOLD_REASON};

    let Some(body) = body else {
        return ParkKind::None;
    };
    if matches!(
        route_mechanical(labels, Some(body), &BTreeSet::new()),
        MechanicalRouting::MissingCapabilities { .. }
    ) {
        return ParkKind::Capability;
    }
    let records = crate::park_record::parse(body);
    if records.is_empty() {
        return ParkKind::None;
    }
    let reason_is = |want: &str| {
        records
            .iter()
            .any(|r| park_hold::is_daemon_hold(r) && r.reason.as_deref() == Some(want))
    };
    if reason_is(QUARANTINE_HOLD_REASON) {
        ParkKind::Quarantine
    } else if reason_is(PRLESS_HOLD_REASON) {
        ParkKind::PrlessHold
    } else if crate::champion_hold::body_declares_hold(body) {
        ParkKind::ChampionHold
    } else if records.iter().any(|r| r.blocker.is_some()) {
        ParkKind::Dependency
    } else {
        ParkKind::Unstated
    }
}

/// The capabilities a capability-only park declares, sorted. Empty when the
/// declaration carries an unknown value: fail closed, nothing can hold it.
#[must_use]
pub fn declared_capabilities(body: &str) -> Vec<String> {
    let decl = crate::capability::extract_capability_markers(body);
    if decl.unknown.is_empty() {
        decl.required.into_iter().collect()
    } else {
        Vec::new()
    }
}

impl PullMarkers {
    /// From a PR's **trusted** comment bodies, oldest first.
    #[must_use]
    pub fn from_trusted<C: AsRef<str>>(trusted: &[C]) -> Self {
        Self {
            champion_hold: crate::champion_hold::standing_hold(trusted).is_some(),
            doctor_cycle_capped: crate::champion_hold::doctor_cycle_capped(trusted),
        }
    }
}

impl PullInput {
    /// From an open-PR listing row of `owner_repo`, with the trust facts the
    /// open-PR guard reads (`policy`), merge-queue membership and markers.
    #[must_use]
    pub fn from_rest(
        row: &crate::forge_pull_listing::RestPull,
        owner_repo: &str,
        policy: &crate::comment_trust::TrustPolicy,
        queued: bool,
        markers: PullMarkers,
    ) -> Self {
        let (same_repo, author_untrusted) =
            crate::worktree_ops::linked_pr_listing::link_facts(row, owner_repo, policy);
        Self {
            number: row.number,
            labels: row.labels.clone(),
            body: row.body.clone(),
            head_ref: row.head_ref.clone(),
            same_repo,
            author_untrusted,
            queued,
            markers,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "step_item_tests.rs"]
mod tests;
