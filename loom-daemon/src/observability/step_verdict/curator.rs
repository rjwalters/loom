//! Curator's projection (#12921): what a Curator step decided on one issue
//! (`curator_step_v1`), a closed, versioned reason set. A new reason is a new
//! version, never a silent addition.
//!
//! Evidence, strongest first: the writes the run journaled (the `loom:curated`
//! label the gh front saw, an `operator-decision apply`, a `park-record
//! apply`), then the step's `LOOM_STEP_OUTCOME role=curator` `class=` /
//! `reason=`. A `verdict` whose `reason=` is none of the named ones reads as
//! `curated`. Nothing journals a close or a decomposition, so those two are
//! projected from the step's own `reason=` token only.
//!
//! | class     | reasons                                          |
//! |-----------|--------------------------------------------------|
//! | `verdict` | `curated`, `rescoped`, `closed`, `decomposed`    |
//! | `refused` | `routed_operator`                                |
//! | `noop`    | `noop`                                           |
//! | `failed`  | `failed`                                         |

use super::{Event, Evidence, Projection, Projector};

/// The Curator step's reason set.
pub const REASON_SET: &str = "curator_step_v1";

/// Every reason of [`REASON_SET`].
pub const REASONS: [&str; 7] = [
    "curated",
    "routed_operator",
    "rescoped",
    "closed",
    "decomposed",
    "noop",
    "failed",
];

const ROLE: &str = "curator";

pub(super) const PROJECTOR: Projector = Projector {
    step_name,
    project,
    outcome_word,
    reason_word,
};

fn step_name(_is_pr: bool) -> &'static str {
    ROLE
}

fn outcome_word(text: &str, _is_pr: bool, item: u32) -> Option<String> {
    super::outcome_field(text, ROLE, item, "class")
}

fn reason_word(text: &str, _is_pr: bool, item: u32) -> Option<String> {
    super::outcome_field(text, ROLE, item, "reason")
}

/// The reason a `verdict` step's `reason=` token names.
fn verdict_reason(token: Option<&str>) -> &'static str {
    match token {
        Some("closed") => "closed",
        Some("rescoped" | "rescope") => "rescoped",
        Some("decomposed" | "split") => "decomposed",
        Some("routed_operator" | "operator" | "escalated") => "routed_operator",
        _ => "curated",
    }
}

fn project(e: &Evidence) -> Option<Projection> {
    if !e.step_path {
        return None;
    }
    let token = e.reason.as_deref().map(|r| r.replace('-', "_"));
    let detail = e.reason.as_deref().map(super::sanitize_detail);
    for event in e.events.iter().rev() {
        let (at, class, reason) = match event {
            Event::Applied { at, action, by }
                if matches!(
                    (action.as_str(), by.as_str()),
                    ("escalated", "operator_decision") | ("blocked", "park_record")
                ) =>
            {
                (at, "refused", "routed_operator")
            }
            Event::Act { at, action } if action == "curated" => (at, "verdict", "curated"),
            _ => continue,
        };
        return Some(Projection {
            wrote: true,
            at: Some(*at),
            detail,
            ..super::projection(class, reason, REASON_SET)
        });
    }
    let p = match e.outcome.as_deref() {
        Some("verdict") => match verdict_reason(token.as_deref()) {
            // A hand-off to the operator is `refused` whichever word named it.
            "routed_operator" => super::projection("refused", "routed_operator", REASON_SET),
            reason => super::projection("verdict", reason, REASON_SET),
        },
        Some("refused") => super::projection("refused", "routed_operator", REASON_SET),
        Some("noop") => super::projection("noop", "noop", REASON_SET),
        _ => super::projection("failed", "failed", REASON_SET),
    };
    Some(Projection { detail, ..p })
}
