//! `check-stale-blocked` — the fleet-wide re-check of every open `loom:blocked`
//! issue (issue #8927, items 3 + 4 of that issue's own fix list).
//!
//! # The gap this closes
//!
//! `loom:blocked` is applied once and never re-examined. Nothing pays the cost
//! of removing it, and a blocked issue is skipped by `/loom:sweep` and by
//! Champion's promotion lane, so a label whose cause has evaporated removes an
//! issue from every queue indefinitely. In the incident that filed #8927 three
//! issues had been suppressed for ~11 months: two on blockers that had closed
//! (one of them *the day after* the block was applied), and one carrying no
//! stated blocker at all.
//!
//! # Why this is not a second parser
//!
//! The *mechanical* check already exists. [`crate::dep_recheck`] knows how to
//! read an issue's cited blockers in all three shapes Loom writes them —
//! a `## Dependencies` checklist item ([`named`]), a prose `Blocked by #N` /
//! `Depends on #N` / `Requires #N` / `**Epic** #N` reference in the body or a
//! non-bot comment ([`extract`] + [`premise`]), and a linked closing PR
//! ([`recheck`]) — and it resolves each one's live state. What was missing was
//! only a *trigger*: `curator.md` runs that check opportunistically, when a
//! Curator pass happens to land on a specific issue, and both of its discovery
//! queries explicitly exclude `loom:blocked`.
//!
//! So this module deliberately owns **no** reference vocabulary of its own. It
//! enumerates the population, calls `dep_recheck`'s existing extraction and
//! state resolution per issue, and classifies the three answers. `curator.md`'s
//! own "Why not just scan … directly (#4963)" note records what happens when a
//! second ad hoc parser is written over this exact data.
//!
//! # Advisory, always
//!
//! The caller is `/loom:sweep`'s pre-wave advisory block, alongside
//! `check-host-sleep.sh`, `check-main-freshness.sh` and
//! `check-quarantine-stashes.sh`. Every one of those is read-only and exits 0
//! unconditionally, and this one matches: a forge read that fails is reported
//! as *unevaluated*, never as clear and never as an error, and no label is ever
//! written. It reports; a human or a later Curator pass decides.
//!
//! # Pull requests are in scope too (#8925)
//!
//! The first version of this check enumerated `gh issue list --label
//! loom:blocked`, which never returns a pull request — so a **parked PR** was
//! not merely skipped by a gate, it was never a candidate. Five open PRs were
//! parked that way for up to 183 hours, one of them an urgent security PR
//! (#8314) waiting on a blocker (#8322) that was itself urgent and ready.
//!
//! A parked PR is classified by the same [`classify`] on the same
//! [`Evidence`], with two PR-specific additions:
//!
//! - [`Evidence::declared`] — the blockers a **park record**
//!   ([`crate::park_record`]) names in the body. A park recorded only in prose,
//!   in a comment, is the second half of #8925's defect; [`undeclared`] is what
//!   reports it.
//! - [`Evidence::self_block`] — the parked PR's *own* superseding block, from
//!   [`park_self_block`]. A PR whose cited blockers have all cleared but which
//!   cannot proceed anyway is [`Verdict::Superseded`], never
//!   [`Verdict::Stale`] — the PR-side transposition of the #4634/#7267
//!   superseding-block gate.

use crate::dep_recheck::{named, premise, recheck};

/// Which kind of artifact a finding is about.
///
/// Carried rather than inferred: the two populations come from different
/// enumerations (`gh issue list` / `gh pr list`) and the remedy differs — an
/// unparked issue re-enters the curation/approval flow, an unparked PR re-enters
/// the review lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Artifact {
    Issue,
    Pr,
}

impl Artifact {
    /// The word used in the report and in `--json`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Artifact::Issue => "issue",
            Artifact::Pr => "PR",
        }
    }
}

/// Labels on a **parked PR itself** that supersede any cleared dependency.
///
/// The PR-side transposition of `guide.md`'s `has_superseding_block` (#4634,
/// extended #7267), and the transposition is where the care is:
///
/// - `loom:operator` / `loom:operator-only` belong here. A human decision is
///   pending on this exact artifact, so reporting it as ready to unpark invites
///   an agent to walk over that decision.
/// - `loom:changes-requested`, `loom:review-requested`, `loom:ci-failure`
///   deliberately do **not**. On an *issue*, an open linked PR carrying
///   `loom:changes-requested` is a superseding block because it means the
///   implementation is not done. On the *PR*, that same label is its **normal
///   lane** — removing `loom:blocked` hands it straight back to Doctor, which is
///   exactly the desired outcome for PR #8314 (`loom:blocked` +
///   `loom:changes-requested` + `loom:ci-failure`). Folding it in here would
///   re-create the stall this check exists to surface.
/// - `loom:blocked` itself is obviously excluded — it is the label under
///   evaluation.
const PR_SELF_BLOCK_LABELS: [&str; 2] = ["loom:operator", "loom:operator-only"];

/// What one issue's three re-checks found, in `dep_recheck`'s own types.
///
/// Holding the typed values rather than pre-rendered strings is what keeps
/// [`classify`] free of parsing: the renderings and verdicts below all come
/// from `dep_recheck`'s existing functions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    /// The issue's `## Dependencies` checklist entries, each unchecked one
    /// carrying its live state — [`crate::dep_recheck::forge::fetch_named_deps`].
    pub named: Vec<named::Dep>,
    /// The prose-cited references ([`crate::dep_recheck::extract::extract`]),
    /// each with its live state
    /// ([`crate::dep_recheck::forge::fetch_refs`]).
    pub prose: Vec<premise::Ref>,
    /// The PRs declared to close this issue, with state
    /// ([`crate::dep_recheck::forge::fetch_prs`]).
    pub closing: Vec<recheck::Pr>,
    /// The blockers a **park record** names in the artifact body (#8925,
    /// [`crate::park_record::blockers`]).
    ///
    /// Not a fourth reference shape — a park record renders `Blocked by: #N`, so
    /// its blockers already arrive in [`Evidence::prose`] via the existing
    /// extractor. This field records *how* they were declared, which is the only
    /// way to tell a machine-readable park from a prose mention.
    pub declared: Vec<u64>,
    /// For a parked **PR**: its own superseding block, if any
    /// ([`park_self_block`]). Always `None` for an issue, whose superseding-block
    /// question is answered by [`Evidence::closing`] instead.
    pub self_block: Option<String>,
}

/// Whether this artifact's park is documented only in prose.
///
/// True when a blocker reference exists in *some* shape but no park record
/// declares one. That is the #8314 / #8852 shape exactly: a correct, well-reasoned
/// park whose dependency lives in a comment, so the label can never be cleared by
/// anything that was not present when it was applied.
///
/// A [`Verdict::Undocumented`] artifact is deliberately **not** undeclared — it
/// has no reference at all, which is the louder finding and is reported on its
/// own.
#[must_use]
pub fn undeclared(e: &Evidence) -> bool {
    let has_reference = !e.named.is_empty() || !e.prose.is_empty() || !e.closing.is_empty();
    has_reference && e.declared.is_empty()
}

/// A parked PR's own superseding block, as a reportable reason — or `None` when
/// nothing about the PR itself stands in the way.
///
/// See [`PR_SELF_BLOCK_LABELS`] for why the label set is narrow. The merge-state
/// arm reuses #7267's rule verbatim, including its `UNKNOWN`-fails-safe
/// direction: a PR that cannot currently land is reported as held rather than as
/// ready to unpark, because for a read-only advisory the conservative direction
/// costs a section heading and the confident one costs a wrong unpark.
#[must_use]
pub fn park_self_block(pr: &recheck::Pr) -> Option<String> {
    if pr.state != "OPEN" {
        return None;
    }
    if let Some(label) = pr
        .labels
        .iter()
        .find(|l| PR_SELF_BLOCK_LABELS.contains(&l.as_str()))
    {
        return Some(format!("the PR carries `{label}` — a human decision is pending on it"));
    }
    let bad_merge = pr.mergeable == "CONFLICTING"
        || pr.merge_state_status == "DIRTY"
        || pr.merge_state_status == "CONFLICTING";
    if bad_merge {
        return Some(format!(
            "the PR cannot currently land (mergeable={}, mergeStateStatus={})",
            if pr.mergeable.is_empty() {
                "?"
            } else {
                &pr.mergeable
            },
            if pr.merge_state_status.is_empty() {
                "?"
            } else {
                &pr.merge_state_status
            }
        ));
    }
    None
}

/// What this issue's `loom:blocked` label looks like now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// At least one cited blocker has resolved. Carries one human-readable
    /// line per triggering signal — an issue can be stale in more than one
    /// way at once, and which signal fired is the whole value of the report.
    Stale(Vec<String>),
    /// `loom:blocked` with no parseable blocker reference anywhere in the body
    /// or comments (#8927's item 4, and its #180 evidence row). Not
    /// verifiable or clearable by anyone who was not present when it was
    /// applied — a defect on its own terms, independent of whether the block
    /// is real.
    Undocumented,
    /// Every cited blocker has resolved, but the artifact itself still cannot
    /// proceed (#8925) — today only reachable for a PR, via
    /// [`park_self_block`]. Reported in its own section rather than folded into
    /// [`Verdict::Stale`]: the park's *stated grounds* have moved, so a human
    /// should see it, but the #4634/#7267 gate says it is not ready to unpark.
    Superseded {
        /// Why the stated grounds have moved — the same lines
        /// [`Verdict::Stale`] would have carried.
        cleared: Vec<String>,
        /// What supersedes them.
        block: String,
    },
    /// A blocker is cited and is still open. The expected, non-event case.
    StillBlocked,
}

/// Whether a forge state string means "no longer open".
///
/// `MERGED` and `CLOSED`-without-merging both count as resolved, matching
/// [`named::verdict`]'s own rule and `curator.md`'s "When Dependencies
/// Complete". Anything else — including an empty or unrecognised string — is
/// treated as still open, because this check must never manufacture a stale
/// verdict out of a state it did not understand.
#[must_use]
fn resolved(state: &str) -> bool {
    state == "MERGED" || state == "CLOSED"
}

/// Classify one issue's evidence.
///
/// Order matters only for the undocumented case: "no reference at all" is
/// answered first, because with nothing cited there is nothing whose staleness
/// could be assessed.
///
/// A *mixed* reference set — one blocker closed, another still open — is
/// reported as [`Verdict::Stale`], not `StillBlocked`. That is
/// [`premise::compute`]'s existing rule ("`stale-premise` iff **any**
/// reference is no longer OPEN") applied consistently to the other two shapes,
/// and it is the right direction for an advisory: the issue's stated grounds
/// have partially moved, which is worth a human's ten seconds. Suppressing it
/// until the *last* blocker cleared is how #178 stayed invisible for eleven
/// months.
#[must_use]
pub fn classify(e: &Evidence) -> Verdict {
    let has_named = !e.named.is_empty();
    let has_prose = !e.prose.is_empty();
    let has_closing = !e.closing.is_empty();

    if !has_named && !has_prose && !has_closing {
        return Verdict::Undocumented;
    }

    let mut reasons = Vec::new();

    // (a) The `## Dependencies` checklist. `named::verdict` is `clear` iff no
    // unchecked entry is still OPEN, so `clear` with a non-empty checklist is
    // exactly "every stated prerequisite is resolved or ticked".
    if has_named && named::verdict(&e.named) == "clear" {
        reasons.push(format!(
            "every `## Dependencies` checklist entry is resolved or ticked: {}",
            one_line(&named::deps_lines(&e.named))
        ));
    }

    // (b) Prose `Blocked by #N` / `Depends on #N` / `Requires #N` / `**Epic**
    // #N` references, from the body and every non-bot comment.
    if has_prose && premise::compute(&e.prose).verdict == "stale-premise" {
        reasons.push(format!(
            "a cited blocker is no longer open: {}",
            one_line(&premise::refs_lines(&e.prose))
        ));
    }

    // (c) A linked closing PR. `recheck::verdict` is deliberately NOT consulted
    // here: it answers "is this PR blocked" (open + a superseding label or a
    // conflict), which is a different question. What makes the ISSUE's block
    // stale is the PR no longer being open at all.
    if has_closing && e.closing.iter().all(|p| resolved(&p.state)) {
        reasons.push(format!(
            "every linked closing PR is merged or closed: {}",
            one_line(&recheck::blockers(&e.closing))
        ));
    }

    if reasons.is_empty() {
        return Verdict::StillBlocked;
    }

    // (d) The #4634/#7267 superseding-block gate, PR side (#8925). Applied LAST,
    // over an already-computed stale reason set, so the gate can only ever
    // downgrade "ready to unpark" to "held" — never manufacture a stale verdict
    // and never suppress the evidence that produced one.
    match &e.self_block {
        Some(block) => Verdict::Superseded {
            cleared: reasons,
            block: block.clone(),
        },
        None => Verdict::Stale(reasons),
    }
}

/// Flatten a `dep_recheck` multi-line rendering onto one reportable line.
///
/// The renderings are newline-joined because they feed a hash; a warning block
/// wants one line per issue, not one per reference.
fn one_line(rendered: &str) -> String {
    rendered
        .lines()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
