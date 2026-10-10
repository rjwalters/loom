//! The landing classifier: one starred issue's facts in, one
//! [`Landing`] out. Pure; every forge read happens in [`super::collect`].
//!
//! # Precedence
//!
//! The first matching rule wins. Operator-only states come first so an issue
//! that is both parked and, say, in review is reported as what actually
//! holds it:
//!
//! 1. the repo is not managed by this host → `needs-operator(unmanaged-repo)`;
//! 2. `loom:operator-decision` on the issue or its PR →
//!    `needs-operator(operator-decision)`; any other operator-only label →
//!    `needs-operator(operator-only)`;
//! 3. `loom:operator` on its PR (Champion's merge-risk / critical-file hold)
//!    → `needs-operator(merge-risk-hold)`;
//! 4. the forge refused the merge of its approved PR →
//!    `needs-operator(merge-refused)`;
//! 5. `loom:blocked` (blockers named in the body **or** a trusted comment): an
//!    open same-repo blocker → `blocked-by` (every open same-repo blocker
//!    inherits the star); only a
//!    cross-repo blocker → `needs-operator(blocked-cross-repo)` (stars are not
//!    inherited across repos, so nothing else would move it), except, at level 2
//!    or above, into a managed repo → `blocked-by` (#10307: the blocker inherits
//!    the level there); otherwise the
//!    block is stale (#10151) → `stale-block`, which the pass resolves itself
//!    ([`StaleAction`]): every cited blocker closed → unblock; none cited →
//!    hand to Curator. Only when that already happened once and the issue is
//!    blocked again with still no open blocker → `needs-operator(blocked-unnamed)`.
//!    An inherited row is never handed to Curator: the walk turns that into
//!    `needs-operator(blocked-unnamed)` at once ([`withhold_inherited_handoff`]);
//! 6. its repo's `main` is red and it has not been dispatched → `blocked-by`
//!    the red-main fix;
//! 7. an open PR → `changes-requested` / `mergeable` (`merging` with a live
//!    sweep) / `in-review` by the PR's labels;
//! 8. `loom:building`, a live sweep, or a peer claim → `building`;
//! 9. this host's token pool is exhausted and no peer claimed it →
//!    `needs-operator(pools-exhausted)`; a capacity-style deferral →
//!    `no-capacity`, carrying the structured [`CapacityWait`] (gate, binding
//!    cap term, queue position; #10214);
//! 10. `loom:issue` → `ready`; anything else → `curating`.

use crate::types::{AskKind, CapacityWait, LandingStage, OperatorAsk};

/// The Champion hold on a PR (merge-risk / critical-file).
pub const HOLD_LABEL: &str = "loom:operator";
/// The decision sub-kind.
pub const DECISION_LABEL: &str = "loom:operator-decision";
/// Operator-only labels other than [`DECISION_LABEL`]: the hard park and its
/// sub-kinds, and the capability park.
pub const OPERATOR_ONLY_LABELS: &[&str] = &[
    "loom:operator-only",
    "loom:operator-blocked",
    "loom:operator-mechanical",
    "loom:needs-capability",
];
/// The blocked park.
pub const BLOCKED_LABEL: &str = "loom:blocked";

/// PR labels that put the next move on Doctor.
const DOCTOR_PR_LABELS: &[&str] = &[
    "loom:changes-requested",
    "loom:merge-conflict",
    "loom:ci-failure",
    "loom:treating",
];

/// One issue (or PR) as the forge listed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemFacts {
    pub number: u32,
    pub labels: Vec<String>,
    pub body: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub open: bool,
}

impl ItemFacts {
    fn has(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

/// A forge refusal to merge an approved PR ([`super::refusal`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRefusal {
    /// A fixed, templated description of the refusal class, e.g. "merge
    /// commits are not allowed (HTTP 405)".
    pub reason: &'static str,
    /// The forge's own words, bounded and inert ([`super::refusal::raw_excerpt`]).
    pub raw: String,
    /// The **open** incident issue tied to the refusal (named in the refusal
    /// comment, or carrying its failure signature), when there is one.
    pub incident: Option<u32>,
}

/// The open PR driving a starred issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFacts {
    pub item: ItemFacts,
    pub refusal: Option<MergeRefusal>,
}

/// A blocker named for an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockerRef {
    /// How to show it: `#N` for the same repo, `owner/repo#N` otherwise.
    pub display: String,
    /// The issue number when it is in the same repo (only those can be
    /// looked up and inherit a star).
    pub number: Option<u32>,
    /// Whether it is open. `None` when it could not be read; treated as open
    /// (a blocker we cannot see is not proof the issue is free).
    pub open: Option<bool>,
    /// For a blocker in another repo: whether a workspace on this host
    /// manages that repo. `None` for a same-repo blocker.
    pub cross_repo_managed: Option<bool>,
}

/// What stands between an undispatched issue and a slot on this host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Capacity {
    /// Nothing known to be in the way.
    #[default]
    Available,
    /// This host's token pool is exhausted (the hold's description). The
    /// ask's dedupe key comes from the issue's forge fingerprint, not from
    /// this host's hold, so every host and every re-exhaustion of an issue
    /// that has not moved share one key ([`super::collect`]).
    PoolExhausted { detail: String },
    /// The work finder deferred it on a capacity-style limit: which gate,
    /// what binds it and where the issue stands in the host's starred queue
    /// (#10214).
    Deferred(CapacityWait),
}

/// Everything the classifier needs about one starred issue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StarFacts {
    /// Forge `owner/repo`.
    pub repo: String,
    /// Whether a workspace on this host manages the repo.
    pub managed: bool,
    pub issue: ItemFacts,
    pub pr: Option<PrFacts>,
    /// Blockers named by the issue (read only when it is `loom:blocked`): its
    /// body, plus its trusted comments when the body names no open one.
    pub blockers: Vec<BlockerRef>,
    /// A trusted [`super::stale::HANDOFF_MARKER`] comment exists: the pass
    /// already handed this issue to Curator once over an unnamed block.
    pub curator_handoff: bool,
    /// The keys of trusted [`super::stale::UNBLOCKED_PREFIX`] comments: the
    /// cleared-blocker sets the pass already unblocked this issue over.
    pub unblocked_before: Vec<String>,
    /// The comments of a blocked issue whose body names no open blocker could
    /// not be read: nothing about the block is certain, so no write is made.
    pub comments_unread: bool,
    /// The repo's red-main fix issue, when `main` is verified red.
    pub red_main_fix: Option<u32>,
    /// A live sweep on this host holds the issue.
    pub live_sweep: bool,
    /// A peer host advertised a live claim on it.
    pub peer_claimed: bool,
    pub capacity: Capacity,
    /// This host's id, for the pool-exhaustion ask.
    pub host: String,
}

/// What the pass does about a `stale-block` row (#10151, [`super::stale`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleAction {
    /// Every cited blocker is closed: remove `loom:blocked` and say which.
    /// `key` is the cleared set (`#5,#6`), the comment marker's dedupe key.
    Unblock { cleared: Vec<String>, key: String },
    /// Nothing is cited: hand the issue to Curator to name the blocker or
    /// release it.
    CuratorHandoff,
}

/// The classifier's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landing {
    pub stage: LandingStage,
    pub next_actor: String,
    pub pr: Option<u32>,
    /// `BlockedBy`: the blocker as shown.
    pub blocked_by: Option<String>,
    /// `BlockedBy` / `MergeRefused`: the same-repo issues that inherit the
    /// star, ascending. Every open same-repo blocker inherits, not only the
    /// first one named (#10012 AC 3).
    pub inherits: Vec<u32>,
    pub no_capacity: Option<String>,
    /// `NoCapacity` from a work-finder deferral: the structured wait
    /// (#10214). `None` for the pool-exhaustion grace window.
    pub capacity_wait: Option<CapacityWait>,
    pub ask: Option<OperatorAsk>,
    /// `StaleBlock`: the write the pass makes, or `None` when something else
    /// holds the issue (its open PR is itself parked, or a Builder holds it),
    /// so the row only waits on Curator and the watchdog.
    pub stale: Option<StaleAction>,
}

impl Landing {
    fn stage(stage: LandingStage, next_actor: &str, pr: Option<u32>) -> Self {
        Self {
            stage,
            next_actor: next_actor.to_string(),
            pr,
            blocked_by: None,
            inherits: Vec::new(),
            no_capacity: None,
            capacity_wait: None,
            ask: None,
            stale: None,
        }
    }

    fn operator(kind: AskKind, key_detail: &str, text: String, pr: Option<u32>) -> Self {
        let key = if key_detail.is_empty() {
            kind.as_str().to_string()
        } else {
            format!("{}:{key_detail}", kind.as_str())
        };
        Self {
            ask: Some(OperatorAsk { kind, key, text }),
            ..Self::stage(LandingStage::NeedsOperator, "operator", pr)
        }
    }
}

/// `issue` or `pr-<n>`: which artifact carries an operator label.
fn target(on_pr: Option<u32>) -> String {
    on_pr.map_or_else(|| "issue".to_string(), |n| format!("pr-{n}"))
}

fn where_(repo: &str, issue: u32, on_pr: Option<u32>) -> String {
    match on_pr {
        Some(p) => format!("{repo}#{issue} (via its PR #{p})"),
        None => format!("{repo}#{issue}"),
    }
}

/// Classify one starred issue.
#[must_use]
pub fn classify(f: &StarFacts) -> Landing {
    let n = f.issue.number;
    let pr_num = f.pr.as_ref().map(|p| p.item.number);
    if !f.managed {
        return Landing::operator(
            AskKind::UnmanagedRepo,
            "",
            format!(
                "{}#{n} is starred, but no Loom workspace on this host manages {}: register \
                 the repo on a Loom host (`loom-daemon workspace add <path>`) or unstar it.",
                f.repo, f.repo
            ),
            None,
        );
    }

    // 2. Operator labels, issue first, then its PR.
    let pr_item = f.pr.as_ref().map(|p| &p.item);
    let decision_on = if f.issue.has(DECISION_LABEL) {
        Some(None)
    } else if pr_item.is_some_and(|p| p.has(DECISION_LABEL)) {
        Some(pr_num)
    } else {
        None
    };
    if let Some(on) = decision_on {
        return Landing::operator(
            AskKind::OperatorDecision,
            &target(on),
            format!(
                "{} is starred but parked on `loom:operator-decision`: make the decision the \
                 thread asks for, then remove the label so agents can continue.",
                where_(&f.repo, n, on)
            ),
            pr_num,
        );
    }
    let only_on = OPERATOR_ONLY_LABELS
        .iter()
        .find(|l| f.issue.has(l))
        .map(|l| (*l, None))
        .or_else(|| {
            pr_item.and_then(|p| {
                OPERATOR_ONLY_LABELS
                    .iter()
                    .find(|l| p.has(l))
                    .map(|l| (*l, pr_num))
            })
        });
    if let Some((label, on)) = only_on {
        return Landing::operator(
            AskKind::OperatorOnly,
            &target(on),
            format!(
                "{} is starred but carries `{label}`, so no agent will act on it: do what it \
                 asks, or remove the label if an agent may proceed.",
                where_(&f.repo, n, on)
            ),
            pr_num,
        );
    }

    // 3–4. The approved PR is held or refused.
    if let Some(pr) = &f.pr {
        let p = pr.item.number;
        if pr.item.has(HOLD_LABEL) {
            return Landing::operator(
                AskKind::MergeRiskHold,
                &format!("pr-{p}"),
                format!(
                    "{}#{n}: PR #{p} is held by Champion's merge-risk / critical-file hold \
                     (`loom:operator`). Review and merge it yourself, or remove `loom:operator` \
                     to let Champion merge it.",
                    f.repo
                ),
                Some(p),
            );
        }
        if let Some(refusal) = &pr.refusal {
            let tail = refusal.incident.map_or_else(
                || format!(" No open incident issue tracks it; the forge said: `{}`", refusal.raw),
                |i| format!(" Tracked in #{i}, which inherits the star."),
            );
            let mut landing = Landing::operator(
                AskKind::MergeRefused,
                &format!("pr-{p}"),
                format!(
                    "{}#{n}: PR #{p} is approved but GitHub refuses the merge: {}. A repo admin \
                     must reconcile the repo's merge settings (branch ruleset vs. allowed merge \
                     methods), then Champion can merge it.{tail}",
                    f.repo, refusal.reason
                ),
                Some(p),
            );
            landing.inherits = refusal
                .incident
                .filter(|i| *i != n && *i != p)
                .into_iter()
                .collect();
            return landing;
        }
    }

    // 5. Blocked.
    if f.issue.has(BLOCKED_LABEL) {
        let open = |b: &&BlockerRef| b.open != Some(false);
        if let Some(b) = f
            .blockers
            .iter()
            .filter(open)
            .find(|b| b.cross_repo_managed.is_none())
        {
            let mut landing =
                Landing::stage(LandingStage::BlockedBy, &format!("blocker {}", b.display), pr_num);
            landing.blocked_by = Some(b.display.clone());
            let mut inherits: Vec<u32> = f
                .blockers
                .iter()
                .filter(open)
                .filter(|b| b.cross_repo_managed.is_none())
                .filter_map(|b| b.number)
                .filter(|m| *m != n)
                .collect();
            inherits.sort_unstable();
            inherits.dedup();
            landing.inherits = inherits;
            return landing;
        }
        if let Some(b) = f.blockers.iter().find(open) {
            let d = &b.display;
            // #10307: level >= 2 crosses into managed repos (the blocker
            // inherits the level as a label), so it is an agent wait, not an
            // operator ask. The plain star still does not cross.
            if b.cross_repo_managed == Some(true)
                && crate::operator_levels::level(&f.issue.labels) >= 2
            {
                let mut landing =
                    Landing::stage(LandingStage::BlockedBy, &format!("blocker {d}"), pr_num);
                landing.blocked_by = Some(d.clone());
                return landing;
            }
            let text = if b.cross_repo_managed == Some(true) {
                format!(
                    "{}#{n} is starred but blocked by {d} in another repo. Stars are not \
                     inherited across repos, so {d} keeps its own queue position: star {d} so \
                     it lands first, or remove the blocker.",
                    f.repo
                )
            } else {
                format!(
                    "{}#{n} is starred but blocked by {d} in a repo this host can't act on: get \
                     {d} resolved (or managed by a Loom host and starred), then remove \
                     `loom:blocked`.",
                    f.repo
                )
            };
            let mut landing = Landing::operator(AskKind::BlockedCrossRepo, d, text, pr_num);
            landing.blocked_by = Some(d.clone());
            return landing;
        }
        return stale_block(f, pr_num);
    }

    let dispatched =
        f.pr.is_some() || f.issue.has(crate::work_finder::BUILDING_LABEL) || f.live_sweep;

    // 6. Red main blocks anything not yet dispatched.
    if let Some(fix) = f.red_main_fix.filter(|fix| *fix != n && !dispatched) {
        let mut landing = Landing::stage(LandingStage::BlockedBy, &format!("blocker #{fix}"), None);
        landing.blocked_by = Some(format!("#{fix}"));
        landing.inherits = vec![fix];
        return landing;
    }

    // 7. PR stages.
    if let Some(pr) = &f.pr {
        let p = pr.item.number;
        if DOCTOR_PR_LABELS.iter().any(|l| pr.item.has(l)) {
            return Landing::stage(LandingStage::ChangesRequested, "doctor", Some(p));
        }
        if pr.item.has("loom:pr") {
            if f.live_sweep {
                return Landing::stage(LandingStage::Merging, "champion", Some(p));
            }
            return Landing::stage(LandingStage::Mergeable, "champion", Some(p));
        }
        return Landing::stage(LandingStage::InReview, "judge", Some(p));
    }

    // 8. Claimed.
    if dispatched || f.peer_claimed {
        return Landing::stage(LandingStage::Building, "builder", None);
    }

    // 9. Capacity.
    match &f.capacity {
        Capacity::PoolExhausted { detail } => {
            return Landing::operator(
                AskKind::PoolsExhausted,
                "",
                format!(
                    "{}#{n} is starred and waiting, but the token pool on host `{}` is \
                     exhausted ({detail}) and no other host has claimed it: add a token or \
                     capacity, or let another host take it.",
                    f.repo, f.host
                ),
                None,
            );
        }
        Capacity::Deferred(wait) => {
            let mut landing = Landing::stage(LandingStage::NoCapacity, "work-finder", None);
            landing.no_capacity = Some(wait.summary());
            landing.capacity_wait = Some(wait.clone());
            return landing;
        }
        Capacity::Available => {}
    }

    // 10. Not yet dispatched.
    if f.issue.has("loom:issue") {
        return Landing::stage(LandingStage::Ready, "work-finder", None);
    }
    Landing::stage(LandingStage::Curating, "curator", None)
}

/// PR labels under which the issue's own open PR is parked, so clearing the
/// issue's block would move nothing (the #4492 / #8925 superseding block):
/// the stale block is then left to Curator, never cleared by the pass.
const PARKED_PR_LABELS: &[&str] = &[
    "loom:blocked",
    "loom:changes-requested",
    "loom:merge-conflict",
];

/// The ways to name a blocker so it inherits the star, for the ask.
const NAMING_FORMS: &str = "a `Blocked by #N` / `Depends on owner/repo#N` / `Requires #N` line \
     anywhere in the body, or an unchecked `- [ ] #N` / `- [ ] owner/repo#N` item under \
     `## Dependencies`";

/// Rule 5's tail (#10151): `loom:blocked`, and no open blocker is named in
/// the body or a trusted comment.
///
/// - Every cited same-repo blocker is closed → `stale-block`, unblocked by
///   the pass ([`StaleAction::Unblock`]).
/// - Nothing is cited → `stale-block`, handed to Curator
///   ([`StaleAction::CuratorHandoff`]).
/// - The pass already did that once (its marker is on the issue) and the
///   issue is blocked again on the same evidence → an agent pass has failed
///   to name a blocker: `needs-operator(blocked-unnamed)`. Never a second
///   unblock, so the pass cannot flip-flop with whoever re-applied the label.
///
/// A self-reference is not a blocker, and an unreadable or cross-repo one
/// (`open != Some(false)`) never gets here: it was treated as open above.
fn stale_block(f: &StarFacts, pr_num: Option<u32>) -> Landing {
    let n = f.issue.number;
    let mut closed: Vec<u32> = f
        .blockers
        .iter()
        .filter(|b| b.open == Some(false))
        .filter_map(|b| b.number.filter(|m| *m != n))
        .collect();
    closed.sort_unstable();
    closed.dedup();
    let cleared: Vec<String> = closed.iter().map(|m| format!("#{m}")).collect();
    let key = cleared.join(",");

    if cleared.is_empty() && f.curator_handoff && !f.comments_unread {
        return Landing::operator(
            AskKind::BlockedUnnamed,
            "",
            format!(
                "{}#{n} is starred but `loom:blocked` with no open blocking issue named, and it \
                 is still blocked after the liveness check handed it to Curator to name one: \
                 resolve what blocks it and remove `loom:blocked`, or name the blocker so it \
                 inherits the star — {NAMING_FORMS}.",
                f.repo
            ),
            pr_num,
        );
    }
    if !cleared.is_empty() && f.unblocked_before.contains(&key) {
        return Landing::operator(
            AskKind::BlockedUnnamed,
            &format!("reblocked:{key}"),
            format!(
                "{}#{n} is starred and `loom:blocked` again, though every blocker it cites \
                 ({key}) is closed and the liveness check already unblocked it once: name what \
                 really blocks it so it inherits the star — {NAMING_FORMS} — or remove \
                 `loom:blocked`.",
                f.repo
            ),
            pr_num,
        );
    }

    let mut landing = Landing::stage(LandingStage::StaleBlock, "curator", pr_num);
    landing.stale = if f.comments_unread {
        None
    } else if cleared.is_empty() {
        // A PR or a Builder already holds it: nothing for Curator to release.
        let held = f.pr.is_some() || f.issue.has(crate::work_finder::BUILDING_LABEL);
        (!held).then_some(StaleAction::CuratorHandoff)
    } else {
        let parked =
            f.pr.as_ref()
                .is_some_and(|p| PARKED_PR_LABELS.iter().any(|l| p.item.has(l)));
        (!parked).then_some(StaleAction::Unblock { cleared, key })
    };
    landing
}

/// An inherited row never gets a [`StaleAction::CuratorHandoff`] (#10162
/// review; see [`super::stale`]'s module doc): `classify` cannot tell an
/// inherited row from a starred one, so the walk calls this once it knows
/// the row inherits through `via` from the starred `root`.
///
/// A handoff becomes the `blocked-unnamed` operator ask — what every
/// unnamed block got before #10151, and still the only actor that can move
/// an unstarred blocker no agent will touch. An [`StaleAction::Unblock`]
/// (or no action) is kept as it is.
pub fn withhold_inherited_handoff(f: &StarFacts, landing: &mut Landing, via: u32, root: u32) {
    if landing.stale != Some(StaleAction::CuratorHandoff) {
        return;
    }
    let n = f.issue.number;
    let through = if via == root {
        String::new()
    } else {
        format!(" through #{via}")
    };
    *landing = Landing::operator(
        AskKind::BlockedUnnamed,
        "",
        format!(
            "{}#{n} blocks starred #{root}{through}, so it inherits that star, but it is \
             `loom:blocked` with no open blocking issue named: resolve what blocks it and \
             remove `loom:blocked`, or name the blocker so the star passes on to it — \
             {NAMING_FORMS}.",
            f.repo
        ),
        landing.pr,
    );
}
