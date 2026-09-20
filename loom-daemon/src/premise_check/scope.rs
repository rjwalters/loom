//! Stage 1: is this issue in the gated population?
//!
//! The population is deliberately **narrow** — three triggers, none of which
//! fires on an ordinary bug report. Routine work continues straight to Curator
//! exactly as today, which is both an acceptance criterion of #8396 and the
//! only way a gate like this survives: a gate that fires on everything is a
//! gate every reader learns to wave through.

/// Why an issue is in the gated population. Reported verbatim on stdout, so
/// the caller can say *which* trigger fired without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// An autonomously-filed proposal. This is the population #7855 came from
    /// and the one #8396 names first.
    ProposalLabel(String),
    /// The issue's own text claims it reverses/overrides a documented
    /// decision. Fires regardless of who filed it — see "the human-filed
    /// edge case" below.
    ReversalClaim(&'static str),
    /// An incident report: behaviour the filer *observed* but did not
    /// *diagnose*.
    IncidentReport(String),
}

impl std::fmt::Display for Trigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trigger::ProposalLabel(l) => write!(f, "label:{l}"),
            Trigger::ReversalClaim(p) => write!(f, "reversal-claim:{p}"),
            Trigger::IncidentReport(h) => write!(f, "incident-report:{h}"),
        }
    }
}

/// Labels that mark an autonomously-filed proposal.
pub const PROPOSAL_LABELS: &[&str] = &["loom:architect", "loom:hermit", "loom:auditor"];

/// Heading fragments that make an issue an incident report, matched
/// case-insensitively as substrings of a markdown heading's text.
///
/// DELIBERATELY EXCLUDED, and why: `steps to reproduce`, `expected`, `actual`,
/// `current behaviour`, `symptom`, `description`, `bug`. Those are the
/// ordinary bug-report vocabulary — an ordinary bug report is *also* an
/// observation the filer has not diagnosed, so a vocabulary built from them
/// would pull the entire bug queue into the gate and falsify #8396's
/// "routine bug fixes are demonstrably unaffected" criterion. What separates
/// an incident report is that it narrates a *host- or fleet-level event*, and
/// that is what these fragments name.
pub const INCIDENT_HEADINGS: &[&str] = &[
    "what happened",
    "what went wrong",
    "what i observed",
    "what we observed",
    "observed behaviour",
    "observed behavior",
    "incident",
    "postmortem",
    "post-mortem",
    "outage",
    "timeline",
];

/// Phrases by which an issue declares, in its own words, that it reverses a
/// documented decision. Matched case-insensitively anywhere in title+body.
///
/// DELIBERATELY EXCLUDED bare words: `reverse`, `revert`, `override`,
/// `deliberate`, `documented`, `by design`. Each is ordinary engineering prose
/// here — "revert the commit", "override the default", "documented in
/// CLAUDE.md" — and a bare-substring vocabulary built from them would fire on
/// most of the backlog.
///
/// The last two entries are #7855's own reasoning, named as an anti-pattern in
/// `defaults/docs/label-state-machine.md`. An issue (or a Curator rescoping
/// comment) that reaches for "the filing is the ruling" is exactly the case
/// this gate exists for, so the phrase itself is a trigger.
pub const REVERSAL_CLAIMS: &[&str] = &[
    "reverse the documented",
    "reverses the documented",
    "reversing the documented",
    "reverse a documented",
    "reverses a documented",
    "reversal of the documented",
    "reversal of a documented",
    "reverse the deliberate",
    "reverses the deliberate",
    "reverses a deliberate",
    "remove the deliberate",
    "removes the deliberate",
    "drop the deliberate",
    "drops the deliberate",
    "undo the deliberate",
    "revert the documented",
    "override the documented",
    "overrides the documented",
    "overrule the documented",
    "contradicts the documented",
    "against the documented design",
    "the filing is the ruling",
    "the filing is the approval",
];

/// The gated population, or `None`.
///
/// Trigger order is fixed for determinism, and is strongest-signal-first:
/// a label is a fact about the forge, a reversal claim is the issue's own
/// assertion, and an incident heading is the weakest of the three.
///
/// # The human-filed edge case, resolved on purpose (#8396's Test Plan)
///
/// #8269 scoped this to "autonomously-filed" issues. That is the wrong axis
/// for [`Trigger::ReversalClaim`]: the risk is the *reversal*, not the filer.
/// A human who files "reverse the documented no-auto-restart posture" is
/// stating a preference, not issuing a ruling — the ruling is the operator
/// answering the routed issue, which costs them one comment and produces the
/// written decision the codebase then cites. So `ReversalClaim` fires on
/// every issue regardless of author, while `ProposalLabel` and
/// `IncidentReport` stay scoped to their populations.
#[must_use]
pub fn trigger(title: &str, body: &str, labels: &[String]) -> Option<Trigger> {
    for label in labels {
        if PROPOSAL_LABELS.contains(&label.as_str()) {
            return Some(Trigger::ProposalLabel(label.clone()));
        }
    }

    let haystack = format!("{title}\n{body}").to_lowercase();
    for phrase in REVERSAL_CLAIMS {
        if haystack.contains(phrase) {
            return Some(Trigger::ReversalClaim(phrase));
        }
    }

    for heading in headings(body) {
        let lower = heading.to_lowercase();
        for frag in INCIDENT_HEADINGS {
            if lower.contains(frag) {
                return Some(Trigger::IncidentReport(heading));
            }
        }
    }

    None
}

/// Markdown heading texts, in document order.
///
/// ATX headings only (`#`…`######` followed by a space), and never inside a
/// fenced code block — a shell comment in a fence is not a heading, and a
/// transcript pasted into an incident report is full of them.
#[must_use]
pub fn headings(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            out.push(t[hashes..].trim().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests;
