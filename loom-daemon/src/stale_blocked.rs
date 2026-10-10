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

use crate::dep_recheck::{extract, named, premise, recheck};

pub mod batch;
pub mod budget;
pub mod hold;
pub mod notify;
pub mod release;
pub mod release_gh;
pub mod release_outcome;
pub mod release_task;
pub mod unnamed;

// The release pass's per-artifact verdicts and their SigNoz export (#10752).
pub mod release_items;
pub mod release_telemetry;

/// Which kind of artifact a finding is about.
///
/// Carried rather than inferred: the two populations come from different
/// enumerations (`gh issue list` / `gh pr list`) and the remedy differs — an
/// unparked issue re-enters the curation/approval flow, an unparked PR re-enters
/// the review lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// ([`crate::dep_recheck::forge::fetch_refs`]). For an issue, read with the
    /// `## Dependencies` checklist lines masked
    /// ([`named::mask_checklist_lines`], #9274): a checklist line is judged
    /// once, by the checklist rule, never also as prose.
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
    pub declared: Vec<crate::park_record::BlockerRef>,
    /// The park-record blockers that name **another repository**
    /// (`OWNER/REPO#N`, #10443), each with its state read in its own repo.
    /// Kept apart from [`Evidence::prose`] because that list is numbers-only and
    /// is resolved against the local repo — a cross-repo number there would be
    /// read as a different local artifact.
    pub remote: Vec<RemoteRef>,
    /// Unchecked `## Dependencies` lines that `named::parse_entries` cannot read
    /// (no `#N` directly after the box, or no ref at all). They are unmet
    /// prerequisites that must not drop out of the count (#9274).
    pub unparsed_unchecked: usize,
    /// For a parked **PR**: its own superseding block, if any
    /// ([`park_self_block`]). Always `None` for an issue, whose superseding-block
    /// question is answered by [`Evidence::closing`] instead.
    pub self_block: Option<String>,
    /// A body park record that names **no** blocker but states a reason
    /// (`<!-- loom:park Blocked by: (unstated) by=… reason="…" -->`, #10558).
    /// Such a hold is documented by its reason, so with no numbered reference
    /// [`classify`] reports [`Verdict::HeldWithReason`], not
    /// [`Verdict::Undocumented`]. Read from the body only.
    pub held: Option<Held>,
}

/// The stated-reason park record behind [`Verdict::HeldWithReason`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held {
    /// `by=` of the record, if written.
    pub by: Option<String>,
    /// The non-empty `reason="…"`.
    pub reason: String,
    /// `at=` as written, if any: [`hold::documents_current_block`] compares it
    /// to the latest `loom:blocked` application.
    pub at: Option<String>,
}

/// A cross-repo park-record blocker with its live state (#10443).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteRef {
    /// `OWNER/REPO`.
    pub repo: String,
    pub number: i64,
    /// `OPEN` / `CLOSED` / `MERGED`.
    pub state: String,
}

impl RemoteRef {
    fn render(&self) -> String {
        format!("{}#{}:{}", self.repo, self.number, self.state)
    }
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
    let has_reference = !e.named.is_empty() || !e.prose.is_empty() || !e.remote.is_empty();
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
    /// `loom:blocked` with no parseable blocker reference. Scope (#9274): a
    /// `## Dependencies` checklist and a park record are read from the **body
    /// only**; prose phrases (`Blocked by #N`, ...) from the body and non-fleet
    /// comments. A linked closing PR is not a blocker reference (#8927's item
    /// 4, and its #180 evidence row). Not
    /// verifiable or clearable by anyone who was not present when it was
    /// applied — a defect on its own terms, independent of whether the block
    /// is real.
    Undocumented,
    /// No numbered blocker, but a body park record states why the artifact is
    /// held (#10558). Documented by its reason, so not [`Verdict::Undocumented`];
    /// the record itself is the idempotency key for anything that reviews it.
    HeldWithReason {
        /// `by=` of the record, if written.
        by: Option<String>,
        /// The stated reason.
        reason: String,
    },
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
    /// Every parseable unchecked `## Dependencies` entry is satisfied, but a
    /// box is still unticked or an unchecked line could not be parsed (#9274).
    /// An unchecked box is unmet until a human confirms its whole condition, so
    /// this is never folded into [`Verdict::Stale`].
    Unticked {
        /// The satisfied entries' references, e.g. `#187`.
        resolved_refs: Vec<String>,
        /// Unchecked lines no parser could read.
        unparsed: usize,
    },
    /// A blocker is cited and is still open. The expected, non-event case.
    StillBlocked,
}

/// Whether a forge state string means the blocker is satisfied.
///
/// `MERGED` (a PR) and `CLOSED` (an issue; a PR closed without merging is
/// `CLOSED_UNMERGED`, see `batch::parse_ref_state`) are satisfied. A
/// closed-unmerged PR is abandoned work and is NOT satisfied (#9274), nor is
/// `OPEN` or any empty/unrecognised string: this check must never manufacture a
/// stale verdict out of a state it did not understand.
///
/// This is deliberately local: `named::verdict` / `premise::compute` feed
/// Curator's dep-recheck `CONCLUSION_HASH` and keep their own rule.
#[must_use]
pub fn resolved(state: &str) -> bool {
    state == "MERGED" || state == "CLOSED"
}

/// Count unchecked (`- [ ]` / `* [ ]`) lines in the body's `## Dependencies`
/// section, parseable or not.
#[must_use]
pub(crate) fn unchecked_lines(body: &str) -> usize {
    named::dependencies_section(body)
        .lines()
        .filter(|l| named::is_unchecked_box(l))
        .count()
}

/// Classify one issue's evidence.
///
/// Order matters only for the undocumented case: "no reference at all" is
/// answered first, because with nothing cited there is nothing whose staleness
/// could be assessed. A linked closing PR is not a reference (#9274): it answers
/// "what closes this issue", not "what blocks it".
///
/// A *mixed* prose reference set — one blocker satisfied, another still open —
/// is reported as [`Verdict::Stale`], not `StillBlocked`: the issue's stated
/// grounds have partially moved, which is worth a human's ten seconds.
/// Suppressing it until the *last* blocker cleared is how #178 stayed invisible
/// for eleven months. The `## Dependencies` checklist is stricter: an unchecked
/// box is unmet whatever refs its line mentions, so it is Stale only when every
/// box is ticked.
#[must_use]
pub fn classify(e: &Evidence) -> Verdict {
    let has_checklist = !e.named.is_empty() || e.unparsed_unchecked > 0;
    let has_prose = !e.prose.is_empty();
    let has_remote = !e.remote.is_empty();

    if !has_checklist && !has_prose && !has_remote {
        // A reason-only park record documents the hold only when nothing else
        // is cited; an all-unparseable checklist (`unparsed_unchecked > 0`)
        // counts as a checklist and still classifies as `Unticked` (#9274).
        return match &e.held {
            Some(h) => Verdict::HeldWithReason {
                by: h.by.clone(),
                reason: h.reason.clone(),
            },
            None => Verdict::Undocumented,
        };
    }

    let mut reasons = Vec::new();
    let mut unticked: Option<Verdict> = None;

    // (a) The `## Dependencies` checklist.
    if has_checklist {
        let unchecked: Vec<&named::Dep> = e.named.iter().filter(|d| !d.checked).collect();
        let satisfied = |d: &&named::Dep| resolved(d.state.as_deref().unwrap_or(""));
        if unchecked.is_empty() && e.unparsed_unchecked == 0 {
            reasons.push(format!(
                "every `## Dependencies` checklist entry is ticked: {}",
                one_line(&named::deps_lines(&e.named))
            ));
        } else if unchecked.iter().all(satisfied) {
            // Reached only with at least one unchecked line (parseable or not).
            // An empty parseable set is vacuously resolved, so an
            // all-unparseable checklist lands here as `Unticked { [], N }`;
            // any open/unknown parseable ref keeps it `StillBlocked`.
            unticked = Some(Verdict::Unticked {
                resolved_refs: unchecked.iter().map(|d| d.reference()).collect(),
                unparsed: e.unparsed_unchecked,
            });
        }
    }

    // (b) Prose `Blocked by #N` / `Depends on #N` / `Requires #N` / `**Epic**
    // #N` references, from the body and every non-bot comment.
    if e.prose.iter().any(|r| resolved(&r.state)) {
        reasons.push(format!(
            "a cited blocker is no longer open: {}",
            one_line(&premise::refs_lines(&e.prose))
        ));
    }

    // (b') Park-record blockers in another repo (#10443), judged by their own
    // repo's state — same "any no longer open" rule as (b).
    if has_remote && e.remote.iter().any(|r| resolved(&r.state)) {
        let lines: Vec<String> = e.remote.iter().map(RemoteRef::render).collect();
        reasons.push(format!("a cited cross-repo blocker is no longer open: {}", lines.join(", ")));
    }

    if reasons.is_empty() {
        return unticked.unwrap_or(Verdict::StillBlocked);
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

/// Which of `closed` an artifact's body/comments cite as a blocker (issue
/// #9102), in document-independent ascending order, deduplicated.
///
/// Backs the close-triggered re-check (`loom-daemon notify-cleared-blockers`,
/// called from `merge-pr.sh`'s post-merge path). It narrows the open
/// `loom:blocked` population to the artifacts that named a just-closed
/// issue/PR, so the close path neither re-reports blocks that were already
/// stale (the fleet-wide advisory's job) nor pays for a full [`Evidence`]
/// gather on every artifact — this runs on text already fetched, with no
/// state lookups.
///
/// The vocabulary is exactly the one [`Evidence`] is built from, so this can
/// never disagree with [`classify`]: [`extract::extract`]'s prose phrases over
/// the body plus every non-bot comment, and — for an issue only, mirroring
/// `gather`'s own PR arm — [`named::parse_entries`]'s `## Dependencies`
/// checklist. Unchecked entries only: a ticked box is never consulted.
/// Same-repo only: a cross-repo checklist entry (`owner/repo#5`) numerically
/// equal to a closed number is a different artifact.
///
/// A linked closing PR is deliberately not consulted — that answers "what
/// closes *this* issue", the opposite relation from "what does this issue cite
/// as its own blocker".
#[must_use]
pub fn cited_among(kind: Artifact, input: &extract::Input, closed: &[i64]) -> Vec<i64> {
    // A qualified park-record blocker is another repo's artifact; mask it so
    // its number can never match a local closed number (#10443).
    let masked = extract::Input {
        body: crate::park_record::mask_qualified(&input.body),
        comments: input.comments.clone(),
    };
    let mut refs: Vec<i64> =
        extract::extract_with(&masked, &crate::forge_identity::FleetLogins::current())
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
    if kind == Artifact::Issue {
        refs.extend(
            named::parse_entries(&input.body)
                .into_iter()
                .filter(|d| d.repo.is_none() && !d.checked)
                .map(|d| d.number),
        );
    }
    let mut hit: Vec<i64> = closed
        .iter()
        .copied()
        .filter(|n| refs.contains(n))
        .collect();
    hit.sort_unstable();
    hit.dedup();
    hit
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

#[cfg(test)]
mod batch_tests;

#[cfg(test)]
mod budget_tests;

#[cfg(test)]
mod release_tests;

#[cfg(test)]
mod notify_tests;

#[cfg(test)]
mod archived_tests;

#[cfg(test)]
mod unnamed_tests;
