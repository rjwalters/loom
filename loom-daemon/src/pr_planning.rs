//! Non-preemptive PR preference. Origin is a trusted historical fact; this
//! queue is only a plan. Role-specific live claims, checks and permissions
//! still decide whether a row can advance.
use std::path::Path;

use serde_json::{json, Value};

use crate::comment_trust::TrustPolicy;
use crate::provenance::origin::WorkOrigin;

mod listing;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod workflow_shaped_tests;
pub use listing::{fetch_queue, has_judge_fallback};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PrRole {
    Judge,
    Doctor,
    Champion,
}

impl PrRole {
    /// The role's name, as the role runner spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Judge => "judge",
            Self::Doctor => "doctor",
            Self::Champion => "champion",
        }
    }
}

pub fn prefer_human_prs(root: &Path) -> bool {
    preference(&crate::config_resolver::resolve_effective_config(root))
}

pub fn preference(config: &Value) -> bool {
    config
        .pointer("/planning/preferHumanPrs")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

pub fn has_label(row: &Value, label: &str) -> bool {
    row["labels"].as_array().is_some_and(|labels| {
        labels
            .iter()
            .any(|l| l.as_str().or_else(|| l["name"].as_str()) == Some(label))
    })
}

/// The row's effective operator priority level (#10307): the highest level
/// label it carries, own or inherited (0 = unstarred).
pub fn operator_level(row: &Value) -> u8 {
    operator_level_in(crate::operator_levels::table(), row)
}

/// [`operator_level`] against an explicit level `table`.
pub fn operator_level_in(table: &[crate::operator_levels::PriorityLevel], row: &Value) -> u8 {
    let labels: Vec<&str> = row["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().or_else(|| l["name"].as_str()))
        .collect();
    crate::operator_levels::level_in(table, &labels)
}

fn fallback(row: &Value) -> bool {
    row["labels"].as_array().is_some_and(|labels| {
        labels.iter().all(|l| {
            !l.as_str()
                .or_else(|| l["name"].as_str())
                .unwrap_or("")
                .starts_with("loom:")
        })
    })
}

/// An unlabeled PR a Loom Builder would have opened (#9389): no `loom:*`
/// label, head branch `feature/issue-<N>`, and the head branch lives in the
/// base repository. Such a PR lost its `loom:review-requested` label (or never
/// got one), so Judge discovers it whatever the labeled queue holds.
///
/// PR metadata is untrusted. Same-repo is the forge's own repository identity
/// (`head.repo` vs `base.repo`), never the branch name or `head.label`: a fork
/// can name its branch `feature/issue-7`. A missing identity (deleted fork,
/// reduced row) is not same-repo.
pub fn workflow_shaped(row: &Value) -> bool {
    let issue_branch = row["head"]["ref"]
        .as_str()
        .and_then(|r| r.strip_prefix("feature/issue-"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let (head, base) = (&row["head"]["repo"], &row["base"]["repo"]);
    let same_repo = match (head["id"].as_u64(), base["id"].as_u64()) {
        (Some(h), Some(b)) => h == b,
        (None, None) => match (head["full_name"].as_str(), base["full_name"].as_str()) {
            (Some(h), Some(b)) => !h.is_empty() && h.eq_ignore_ascii_case(b),
            _ => false,
        },
        _ => false,
    };
    fallback(row) && issue_branch && same_repo
}

/// The fallback rows that join a busy labeled queue and open Judge's
/// role-runner gate: trusted interactive ones when preference is on, and
/// [`workflow_shaped`] ones always (a lost-PR safety net, not a preference).
fn joins_busy_queue(row: &Value, prefer: bool, trust: &TrustPolicy) -> bool {
    workflow_shaped(row)
        || (prefer && WorkOrigin::trusted_pr(row, trust) == WorkOrigin::Interactive)
}

/// Stable role baseline followed by one shared preference. Inputs are REST
/// pull objects, newest first, with `mergeable` resolved for Doctor conflicts.
/// Deliberately retain `loom:reviewing` for Judge's existing stale-claim probe;
/// an apparent claim is not evidence that its holder is alive.
pub fn ordered_queue(
    rows: Vec<Value>,
    role: PrRole,
    prefer: bool,
    trust: &TrustPolicy,
) -> Vec<Value> {
    let labeled_review = rows.iter().any(|r| has_label(r, "loom:review-requested"));
    let mut candidates: Vec<_> = rows
        .into_iter()
        .filter(|r| {
            r["state"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("open"))
                && r["draft"] != true
                && r["isDraft"] != true
                && !has_label(r, "loom:blocked")
                && !has_label(r, "loom:operator-only")
        })
        .collect();
    candidates.retain(|r| match role {
        PrRole::Judge => {
            !has_label(r, "loom:operator")
                && !has_label(r, "loom:treating")
                && (has_label(r, "loom:review-requested")
                    || (fallback(r) && (!labeled_review || joins_busy_queue(r, prefer, trust))))
        }
        PrRole::Doctor => {
            !has_label(r, "loom:treating")
                && (has_label(r, "loom:changes-requested")
                    || (has_label(r, "loom:pr")
                        && !has_label(r, "loom:operator")
                        && (r["mergeable"] == false || r["mergeable"] == "CONFLICTING")))
        }
        PrRole::Champion => has_label(r, "loom:pr"),
    });
    // Match existing role tie-breaks: review/repair listing order; approved
    // conflicts before feedback; Champion oldest first. Sorting is stable.
    match role {
        PrRole::Doctor => candidates.sort_by_key(|r| has_label(r, "loom:changes-requested")),
        PrRole::Champion => {
            candidates.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()))
        }
        PrRole::Judge => {}
    }
    candidates.sort_by_key(|r| {
        // Effective level, highest first (#10307): a level-2 row precedes a
        // plain star, which precedes everything unstarred.
        let star = std::cmp::Reverse(operator_level(r));
        let human = !(prefer && WorkOrigin::trusted_pr(r, trust) == WorkOrigin::Interactive);
        // Doctor historically applied stars inside each queue. Disabled
        // preference reproduces that ordering exactly.
        let baseline = if role == PrRole::Doctor && !prefer {
            has_label(r, "loom:changes-requested")
        } else {
            false
        };
        // #9389: a workflow-shaped fallback row admitted to a busy queue only
        // by its shape goes behind every labeled row, so it cannot delay
        // enrolled review work. Interactive fallback keeps its preference.
        let shape_only = role == PrRole::Judge && labeled_review && human && fallback(r);
        (baseline, shape_only, star, human)
    });
    for row in &mut candidates {
        let origin = WorkOrigin::trusted_pr(row, trust);
        let mode = if role == PrRole::Judge && fallback(row) {
            "fallback"
        } else {
            "workflow"
        };
        let level = operator_level(row);
        let reason = if level >= 1 {
            "operator-priority"
        } else if prefer && origin == WorkOrigin::Interactive {
            "interactive"
        } else {
            "ordinary"
        };
        row["origin"] = json!(origin);
        row["mode"] = json!(mode);
        row["priorityReason"] = json!(reason);
        row["operatorPriorityLevel"] = json!(level);
    }
    candidates
}
