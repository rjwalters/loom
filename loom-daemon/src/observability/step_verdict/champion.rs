//! Champion's projections (#12027): its merge-gate decisions
//! (`champion_merge_v1`), its promotion verdicts (`champion_promotion_v1`)
//! and its PR triage (`champion_triage_v1`, #12499), each a closed, versioned
//! reason set. A new
//! reason is a new version, never a silent addition.
//!
//! Evidence, strongest first: the write the gh front journaled (`merged`,
//! `promoted`), then the markers Champion's comments opened with, then the
//! daemon subcommands' own writes (`park-record`, `operator-decision`), then
//! the step's `CHAMPION-STEP … outcome=` word.

use super::{Event, Evidence, Projection, Projector, StoreOp};

/// The merge gate's reason set.
pub const MERGE_REASON_SET: &str = "champion_merge_v1";

/// The promotion verdict's reason set.
pub const PROMOTION_REASON_SET: &str = "champion_promotion_v1";

/// The PR triage's reason set (#12499): a `--pr` step on an unlabelled PR.
pub const TRIAGE_REASON_SET: &str = "champion_triage_v1";

/// Every reason of [`TRIAGE_REASON_SET`]: the PR went to review (`verdict`),
/// or was blocked or sent to the operator (`refused`).
pub const TRIAGE_REASONS: [&str; 3] = ["review_requested", "blocked", "operator"];

/// Every reason of [`MERGE_REASON_SET`].
pub const MERGE_REASONS: [&str; 14] = [
    "merged",
    "ci_failing",
    "conflict",
    "label",
    "size",
    "critical_file",
    "stale",
    "unknown",
    "ci_pending",
    "held",
    "head_moved",
    "not_eligible",
    "failed",
    "unclassified",
];

/// Every reason of [`PROMOTION_REASON_SET`].
pub const PROMOTION_REASONS: [&str; 9] = [
    "promoted",
    "not_obviously_beneficial",
    "needs_operator",
    "premise_false",
    "blocked_dependency",
    "evaluating_claimed",
    "not_eligible",
    "failed",
    "unclassified",
];

/// The refusal reasons a marker can name: the `pr_refused` gauge's `reason`
/// values (`unknown`: a `champion:reject` criterion this version does not
/// name).
pub const GAUGE_REASONS: [&str; 7] = [
    "ci_failing",
    "conflict",
    "label",
    "size",
    "critical_file",
    "stale",
    "unknown",
];

/// The proposal labels a promotion verdict answers.
pub const PROPOSAL_LABELS: [&str; 4] = [
    "loom:curated",
    "loom:architect",
    "loom:hermit",
    "loom:auditor",
];

/// The step-end line Champion ends every mode with.
const STEP_LINE: &str = "CHAMPION-STEP";

pub(super) const PROJECTOR: Projector = Projector {
    step_name,
    project,
    outcome_word,
};

fn step_name(is_pr: bool) -> &'static str {
    if is_pr {
        "champion_pr"
    } else {
        "champion_issue"
    }
}

/// The `outcome=` word of the last `CHAMPION-STEP` line in `text` for this
/// mode and item, verbatim (`head-moved`, `not-eligible`, …).
fn outcome_word(text: &str, is_pr: bool, item: u32) -> Option<String> {
    let mode = if is_pr { "pr" } else { "issue" };
    text.lines().rev().find_map(|line| {
        let rest = line.trim().strip_prefix(STEP_LINE)?;
        let field = |key: &str| {
            rest.split_whitespace()
                .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        };
        (field("mode")? == mode && field("item")?.parse::<u32>().ok()? == item)
            .then(|| field("outcome").map(str::to_string))
            .flatten()
    })
}

/// A marker's inner tokens: `<!-- champion:reject:ci-status:a,b -->` →
/// `["champion", "reject", "ci-status", "a,b"]`. The fourth part is kept
/// verbatim (further `:` and spaces: check names have them); the kind and the
/// third part end at the first space (`champion:ac-hold pr=1 sha=…`).
fn marker_parts(marker: &str) -> Option<Vec<&str>> {
    let inner = marker.strip_prefix("<!-- ")?;
    // A marker cut at the journal's bound has lost its ` -->`.
    let inner = inner.strip_suffix(" -->").unwrap_or(inner);
    let mut parts: Vec<&str> = inner.splitn(4, ':').collect();
    for part in parts.iter_mut().take(3) {
        *part = part.split_whitespace().next().unwrap_or_default();
    }
    Some(parts)
}

fn projection(class: &'static str, reason: &str, set: &'static str) -> Projection {
    Projection {
        class,
        reason: reason.to_string(),
        reason_set: set,
        ..Projection::default()
    }
}

/// The merge-gate refusal a marker states: `(criterion_key, reason_key,
/// reason)`.
#[must_use]
pub fn merge_marker(marker: &str) -> Option<(String, String, &'static str)> {
    let parts = marker_parts(marker)?;
    match parts.as_slice() {
        ["champion", "reject", criterion, rest @ ..] => {
            let reason = match *criterion {
                "ci-status" => "ci_failing",
                "merge-conflict" => "conflict",
                "label-check" => "label",
                "size-check" => "size",
                _ => "unknown",
            };
            let key = rest.first().copied().unwrap_or_default();
            Some(((*criterion).to_string(), key.to_string(), reason))
        }
        ["champion", "critical-file-hold"] => {
            Some(("critical-file".to_string(), String::new(), "critical_file"))
        }
        ["champion", "stale-pr-notice", rest @ ..] => {
            Some(("recency-check".to_string(), rest.join(":"), "stale"))
        }
        _ => None,
    }
}

/// The `body-<hash>` a promotion marker names.
fn body_hash(marker: &str) -> Option<String> {
    let parts = marker_parts(marker)?;
    match parts.as_slice() {
        ["champion", "proposal-verdict" | "revise", hash, ..] => {
            hash.strip_prefix("body-").map(str::to_string)
        }
        _ => None,
    }
}

fn project(e: &Evidence) -> Option<Projection> {
    if e.is_pr {
        project_merge(e)
    } else {
        project_promotion(e)
    }
}

fn project_merge(e: &Evidence) -> Option<Projection> {
    let set = MERGE_REASON_SET;
    for event in &e.events {
        if let Event::Act { at, action } = event {
            if action == "merged" {
                return Some(Projection {
                    store: StoreOp::Clear,
                    wrote: true,
                    at: Some(*at),
                    ..projection("verdict", "merged", set)
                });
            }
        }
    }
    for event in e.events.iter().rev() {
        let Event::Marker {
            at,
            marker,
            head_sha,
        } = event
        else {
            continue;
        };
        if let Some((criterion, key, reason)) = merge_marker(marker) {
            return Some(Projection {
                criterion_key: Some(criterion),
                reason_key: Some(key),
                head_sha: head_sha.clone(),
                store: StoreOp::Observe,
                wrote: true,
                at: Some(*at),
                ..projection("refused", reason, set)
            });
        }
    }
    let triage = TRIAGE_REASON_SET;
    let p = match e.outcome.as_deref() {
        Some("triaged") => projection("verdict", "review_requested", triage),
        Some("triage-blocked") => projection("refused", "blocked", triage),
        Some("triage-operator") => projection("refused", "operator", triage),
        Some("merged") => Projection {
            store: StoreOp::Clear,
            ..projection("verdict", "merged", set)
        },
        Some("rejected") => Projection {
            store: StoreOp::Lookup,
            ..projection("refused", "unknown", set)
        },
        Some("requeued") => projection("noop", "ci_pending", set),
        Some("held") => projection("noop", "held", set),
        Some("head-moved") => Projection {
            store: StoreOp::Clear,
            ..projection("refused", "head_moved", set)
        },
        Some("not-eligible") => projection("refused", "not_eligible", set),
        Some("failed") => projection("failed", "failed", set),
        _ if e.step_path => projection("failed", "unclassified", set),
        _ => return None,
    };
    Some(p)
}

fn project_promotion(e: &Evidence) -> Option<Projection> {
    let set = PROMOTION_REASON_SET;
    let hash = e.events.iter().rev().find_map(|event| match event {
        Event::Marker { marker, .. } => body_hash(marker),
        _ => None,
    });
    let with_hash = |p: Projection| Projection {
        body_hash: hash.clone(),
        ..p
    };
    for event in &e.events {
        if let Event::Act { at, action } = event {
            if action == "promoted" {
                return Some(with_hash(Projection {
                    wrote: true,
                    at: Some(*at),
                    ..projection("verdict", "promoted", set)
                }));
            }
        }
    }
    for event in e.events.iter().rev() {
        let (at, reason) = match event {
            Event::Marker { at, marker, .. } => {
                let reason = match marker_parts(marker).as_deref() {
                    Some(["champion", "revise", ..]) => "not_obviously_beneficial",
                    Some(["champion", "proposal-escalated", ..]) => "needs_operator",
                    Some(["champion", "premise-false-closed", ..]) => "premise_false",
                    _ => continue,
                };
                (at, reason)
            }
            Event::Applied { at, action, by } => match (action.as_str(), by.as_str()) {
                ("blocked", "park_record") => (at, "blocked_dependency"),
                ("escalated", "operator_decision") => (at, "needs_operator"),
                _ => continue,
            },
            Event::Act { .. } => continue,
        };
        return Some(with_hash(Projection {
            wrote: true,
            at: Some(*at),
            ..projection("refused", reason, set)
        }));
    }
    let evaluating = e.pre_labels.iter().any(|l| l == "loom:evaluating");
    let p = match e.outcome.as_deref() {
        Some("promoted") => projection("verdict", "promoted", set),
        Some("revise") => projection("refused", "not_obviously_beneficial", set),
        Some("operator") => projection("refused", "needs_operator", set),
        Some("closed") => projection("refused", "premise_false", set),
        Some("blocked") => projection("refused", "blocked_dependency", set),
        Some("not-eligible") if evaluating => projection("noop", "evaluating_claimed", set),
        Some("not-eligible") => projection("noop", "not_eligible", set),
        Some("failed") => projection("failed", "failed", set),
        _ if e.step_path => projection("failed", "unclassified", set),
        _ => return None,
    };
    Some(with_hash(p))
}
