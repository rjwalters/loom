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

use crate::dep_recheck::{named, premise, recheck};

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
        Verdict::StillBlocked
    } else {
        Verdict::Stale(reasons)
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
