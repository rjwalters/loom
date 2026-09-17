//! The `--check-fact-unescalate` decision — the Rust port of
//! `classify-dependency-block.sh`'s `check_fact_unescalate` (epic #7810, PR 3).
//!
//! Releases a proposal parked on findings that a **specific commit** has since
//! resolved. Unlike [`super::unescalate`], which reasons about whether blockers
//! closed, this one demands *evidence*: a resolutions file accounting for every
//! finding, and the commit that did it.
//!
//! # The strictest gate in the script
//!
//! Two counting rules stand between a parked proposal and release, and together
//! they mean **every finding must be individually accounted for**:
//!
//! - `resolutions-mismatch` — the number of resolution lines must *equal* the
//!   number of findings. Not "at least"; equal. A file covering three of five
//!   findings is rejected, and so is one covering seven.
//! - `partial-resolution` — not one line may be `UNRESOLVED:`.
//!
//! Without the equality check a caller could satisfy the gate by resolving one
//! finding and omitting the rest; without the `UNRESOLVED` check it could
//! satisfy it by listing them all as unresolved. Each covers the other's gap,
//! which is why both are preserved rather than folded into one "all resolved"
//! test.
//!
//! The fingerprint keys on the escalation text **and** the commit, so releasing
//! against one commit does not suppress a later release against another.

use super::fingerprint::fact_fingerprint;

/// Everything the decision needs, already gathered.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub labels: Vec<String>,
    pub comments: String,
    /// The last comment carrying the escalation marker.
    pub escalation: String,
    /// Contents of `--resolutions-file`, or `None` when it was not supplied or
    /// does not exist — the shell tests both together.
    pub resolutions: Option<String>,
    /// `--commit`.
    pub commit_sha: Option<String>,
}

/// What `--check-fact-unescalate` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    NoFactUnescalate {
        reason: &'static str,
    },
    FactUnescalate {
        verified_commit: String,
        resolved_count: usize,
        fingerprint: String,
    },
}

/// Marker text this decision reads.
pub struct Markers<'a> {
    pub operator_only_label: &'a str,
    pub cycle_prefix: &'a str,
    pub fact_unescalate_prefix: &'a str,
}

/// Count of resolution lines, and how many of them are unresolved.
///
/// Only lines *starting* with the tokens count, matching the shell's
/// `grep -cE '^(RESOLVED|UNRESOLVED):'` — prose mentioning "RESOLVED" mid-line
/// must not inflate the total, or a narrative file could satisfy the equality
/// gate by accident.
#[must_use]
fn count_resolutions(text: &str) -> (usize, usize) {
    let mut total = 0;
    let mut unresolved = 0;
    for line in text.lines() {
        if line.starts_with("UNRESOLVED:") {
            total += 1;
            unresolved += 1;
        } else if line.starts_with("RESOLVED:") {
            total += 1;
        }
    }
    (total, unresolved)
}

/// Decide whether a commit's evidence releases this proposal.
#[must_use]
pub fn decide(inputs: &Inputs, markers: &Markers<'_>) -> Decision {
    if !inputs
        .labels
        .iter()
        .any(|l| l == markers.operator_only_label)
    {
        return Decision::NoFactUnescalate {
            reason: "not-operator-only",
        };
    }

    if inputs.comments.contains(markers.cycle_prefix) {
        return Decision::NoFactUnescalate {
            reason: "cycle-escalation",
        };
    }

    if inputs.escalation.chars().all(char::is_whitespace) {
        return Decision::NoFactUnescalate {
            reason: "no-escalation-record",
        };
    }

    let findings = super::findings::extract_findings(&inputs.escalation);
    if findings.chars().all(char::is_whitespace) {
        return Decision::NoFactUnescalate {
            reason: "no-findings",
        };
    }

    let Some(resolutions) = inputs.resolutions.as_deref() else {
        return Decision::NoFactUnescalate {
            reason: "missing-resolutions-file",
        };
    };

    // `grep -c '.'` counts non-empty lines.
    let n_findings = findings.lines().filter(|l| !l.is_empty()).count();
    let (n_resolutions, n_unresolved) = count_resolutions(resolutions);

    // EQUALITY, not "at least": see the module docs. Fewer lines means findings
    // went unaccounted for; more means the file describes something else.
    if n_findings != n_resolutions {
        return Decision::NoFactUnescalate {
            reason: "resolutions-mismatch",
        };
    }

    if n_unresolved != 0 {
        return Decision::NoFactUnescalate {
            reason: "partial-resolution",
        };
    }

    let Some(commit) = inputs.commit_sha.as_deref().filter(|c| !c.is_empty()) else {
        return Decision::NoFactUnescalate {
            reason: "missing-commit",
        };
    };

    let fp = fact_fingerprint(&inputs.escalation, commit);
    if inputs
        .comments
        .contains(&format!("{}{fp} -->", markers.fact_unescalate_prefix))
    {
        return Decision::NoFactUnescalate {
            reason: "already-unescalated",
        };
    }

    Decision::FactUnescalate {
        verified_commit: commit.to_string(),
        resolved_count: n_resolutions,
        fingerprint: fp,
    }
}

/// Render a decision as stdout plus its exit code.
#[must_use]
pub fn render(decision: &Decision) -> (String, i32) {
    match decision {
        Decision::NoFactUnescalate { reason } => {
            (format!("NO_FACT_UNESCALATE\nREASON: {reason}\n"), 1)
        }
        Decision::FactUnescalate {
            verified_commit,
            resolved_count,
            fingerprint,
        } => (
            format!(
                "FACT_UNESCALATE\nVERIFIED_COMMIT: {verified_commit}\n\
                 RESOLVED_COUNT: {resolved_count}\nFINGERPRINT: {fingerprint}\n"
            ),
            0,
        ),
    }
}

#[cfg(test)]
mod tests;
