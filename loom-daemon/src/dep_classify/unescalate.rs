//! The `--check-unescalate` decision — the Rust port of
//! `classify-dependency-block.sh`'s `check_unescalate` (epic #7810, PR 3).
//!
//! Answers: *may this proposal come back off `loom:operator-only`?* It is the
//! self-healing half of #5664 — a proposal parked on a **timing** finding
//! should return to normal evaluation once the timing changes, without a human
//! having to notice.
//!
//! # The safety asymmetry worth stating
//!
//! `check_defer` tolerates an unreadable blocker; this one refuses on it
//! (`unreadable-blocker`). That is not an inconsistency — the two directions
//! carry different risk:
//!
//! - **Deferring** on incomplete information parks work that a later pass will
//!   re-examine. Recoverable.
//! - **Un-escalating** on incomplete information releases a proposal a human
//!   deliberately parked, on the strength of blockers nobody could read.
//!
//! So this path fails closed and the defer path does not.
//!
//! # Idempotency
//!
//! Applying writes a marker comment carrying the blocker fingerprint. A later
//! pass that sees its own marker stops at `already-unescalated` rather than
//! re-posting. That guard keys on the **comment**, which is why write ordering
//! in `apply` is load-bearing — see [`super::apply`].

use super::fingerprint::fingerprint;
use super::state::ClassifiedRefs;

/// Everything the decision needs, already gathered.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub body: String,
    /// Comma-joined label names, as the shell built them.
    pub labels: Vec<String>,
    /// All comment bodies, newline-joined — searched for markers.
    pub comments: String,
    /// The escalation record: an explicit `--findings-file`, else the **last**
    /// comment carrying the escalation marker.
    pub escalation: String,
    pub refs: ClassifiedRefs,
}

/// What `--check-unescalate` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    NoUnescalate {
        reason: &'static str,
    },
    /// Blockers are still open, but the issue declares work independent of
    /// them — release it scoped to that subset.
    Subset {
        still_open: Vec<String>,
        blocker_fingerprint: String,
        subset: String,
    },
    /// Every recorded blocker has closed: the stated reason for the escalation
    /// is gone.
    Cleared {
        cleared: Vec<String>,
        blocker_fingerprint: String,
    },
}

/// Marker text the decision searches for and `apply` writes.
pub struct Markers<'a> {
    pub operator_only_label: &'a str,
    pub cycle_prefix: &'a str,
    pub unescalate_prefix: &'a str,
}

/// Decide whether to un-escalate.
///
/// Order is contract, as in [`super::defer`]. The gates before any forge
/// classification are cheap refusals; the ones after are the substance.
#[must_use]
pub fn decide(inputs: &Inputs, repo: &str, self_node: &str, markers: &Markers<'_>) -> Decision {
    // Not parked at all — nothing to release.
    if !inputs
        .labels
        .iter()
        .any(|l| l == markers.operator_only_label)
    {
        return Decision::NoUnescalate {
            reason: "not-operator-only",
        };
    }

    // A cycle escalation is not a timing finding: waiting never resolves it, so
    // it was never this mechanism's to undo.
    if inputs.comments.contains(markers.cycle_prefix) {
        return Decision::NoUnescalate {
            reason: "cycle-escalation",
        };
    }

    if inputs.escalation.chars().all(char::is_whitespace) {
        return Decision::NoUnescalate {
            reason: "no-escalation-record",
        };
    }

    let findings = super::findings::extract_findings(&inputs.escalation);
    if findings.chars().all(char::is_whitespace) {
        return Decision::NoUnescalate {
            reason: "no-findings",
        };
    }
    if !super::finding::findings_are_dependency_only(&findings) {
        return Decision::NoUnescalate {
            reason: "merits-finding",
        };
    }

    let blockers = super::defer::resolve_blockers(&findings, &inputs.body, repo, self_node);
    if blockers.is_empty() {
        return Decision::NoUnescalate {
            reason: "no-recorded-blocker",
        };
    }

    // THE asymmetry. See the module docs: releasing a human-parked proposal on
    // the strength of blockers nobody could read is the direction that does not
    // recover.
    if !inputs.refs.unknown.is_empty() {
        return Decision::NoUnescalate {
            reason: "unreadable-blocker",
        };
    }

    if !inputs.refs.open.is_empty() {
        let subset = super::subset::extract_startable_subset(&inputs.body);
        if subset.chars().all(char::is_whitespace) {
            return Decision::NoUnescalate {
                reason: "blocker-still-open",
            };
        }

        // Keyed on the OPEN set and prefixed, so a subset carve-out and a
        // blockers-cleared release on the same issue never share a marker.
        let fp = format!("subset-{}", fingerprint(&inputs.refs.open_joined()));
        if already_marked(&inputs.comments, markers.unescalate_prefix, &fp) {
            return Decision::NoUnescalate {
                reason: "already-unescalated",
            };
        }

        return Decision::Subset {
            still_open: inputs.refs.open.clone(),
            blocker_fingerprint: fp,
            subset,
        };
    }

    // Keyed on the RESOLVED set: what cleared is what this release was about.
    let fp = fingerprint(&inputs.refs.resolved_joined());
    if already_marked(&inputs.comments, markers.unescalate_prefix, &fp) {
        return Decision::NoUnescalate {
            reason: "already-unescalated",
        };
    }

    Decision::Cleared {
        cleared: inputs.refs.resolved.clone(),
        blocker_fingerprint: fp,
    }
}

/// Whether a marker for exactly this fingerprint is already present.
///
/// The trailing ` -->` is required: without it, `subset-abc` would match a
/// marker for `subset-abcdef`, and a later, larger blocker set would be
/// mistaken for one already handled.
fn already_marked(comments: &str, prefix: &str, fp: &str) -> bool {
    comments.contains(&format!("{prefix}{fp} -->"))
}

/// Render a decision as stdout plus its exit code, before any `--apply`.
///
/// `STILL_OPEN:` is emitted on the subset path *before* the verdict, matching
/// the shell — the caller sees what remains blocked even when the answer is to
/// release.
#[must_use]
pub fn render(decision: &Decision, unreadable: &[String]) -> (String, i32) {
    let mut out = String::new();
    if !unreadable.is_empty() {
        out.push_str(&format!("UNREADABLE: {}\n", unreadable.join(" ")));
    }
    match decision {
        Decision::NoUnescalate { reason } => {
            // `blocker-still-open` is reached only after STILL_OPEN has already
            // been printed by the caller; the shell emits it the same way.
            out.push_str("NO_UNESCALATE\n");
            out.push_str(&format!("REASON: {reason}\n"));
            (out, 1)
        }
        Decision::Subset {
            still_open,
            blocker_fingerprint,
            ..
        } => {
            out.push_str(&format!("STILL_OPEN: {}\n", still_open.join(" ")));
            out.push_str("UNESCALATE\n");
            out.push_str("SUBSET_CARVEOUT: yes\n");
            out.push_str(&format!("STILL_OPEN_BLOCKERS: {}\n", still_open.join(" ")));
            out.push_str(&format!("BLOCKER_FINGERPRINT: {blocker_fingerprint}\n"));
            (out, 0)
        }
        Decision::Cleared {
            cleared,
            blocker_fingerprint,
        } => {
            out.push_str("UNESCALATE\n");
            out.push_str(&format!("CLEARED_BLOCKERS: {}\n", cleared.join(" ")));
            out.push_str(&format!("BLOCKER_FINGERPRINT: {blocker_fingerprint}\n"));
            (out, 0)
        }
    }
}

#[cfg(test)]
mod tests;
