//! Doctor's projection (#12921): what a Doctor step did to one PR
//! (`doctor_step_v1`), a closed, versioned reason set. A new reason is a new
//! version, never a silent addition.
//!
//! Evidence, strongest first: the writes the run journaled (an
//! `operator-decision apply`, a `park-record apply`), then the step's
//! `LOOM_STEP_OUTCOME role=doctor` `class=` / `reason=`. The head comes from
//! the last journaled comment marker that named one, else it is absent.
//! `doctor_cycles` has no journaled signal today, so the record omits it.
//!
//! | class     | reasons                  |
//! |-----------|--------------------------|
//! | `verdict` | `fixed_pushed`           |
//! | `noop`    | `no_change`              |
//! | `refused` | `escalated`, `gave_up`   |
//! | `failed`  | `failed`                 |

use super::{Event, Evidence, Projection, Projector};

/// The Doctor step's reason set.
pub const REASON_SET: &str = "doctor_step_v1";

/// Every reason of [`REASON_SET`].
pub const REASONS: [&str; 5] = [
    "fixed_pushed",
    "no_change",
    "escalated",
    "gave_up",
    "failed",
];

const ROLE: &str = "doctor";

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

fn project(e: &Evidence) -> Option<Projection> {
    if !e.step_path {
        return None;
    }
    let head_sha = e.events.iter().rev().find_map(|event| match event {
        Event::Marker { head_sha, .. } => head_sha.clone(),
        _ => None,
    });
    let with_head = |p: Projection| Projection {
        head_sha: head_sha.clone(),
        ..p
    };
    // A journaled hand-off to a human is the step's own write: it wins.
    let handed_off = e.events.iter().rev().find_map(|event| match event {
        Event::Applied { at, action, by }
            if matches!(
                (action.as_str(), by.as_str()),
                ("escalated", "operator_decision") | ("blocked", "park_record")
            ) =>
        {
            Some(*at)
        }
        _ => None,
    });
    if let Some(at) = handed_off {
        return Some(with_head(Projection {
            wrote: true,
            at: Some(at),
            ..super::projection("refused", "escalated", REASON_SET)
        }));
    }
    let token = e.reason.as_deref().map(|r| r.replace('-', "_"));
    let p = match e.outcome.as_deref() {
        Some("verdict") => super::projection("verdict", "fixed_pushed", REASON_SET),
        Some("noop") => super::projection("noop", "no_change", REASON_SET),
        Some("refused") if token.as_deref() == Some("gave_up") => {
            super::projection("refused", "gave_up", REASON_SET)
        }
        Some("refused") => super::projection("refused", "escalated", REASON_SET),
        _ => super::projection("failed", "failed", REASON_SET),
    };
    Some(with_head(Projection {
        detail: e.reason.as_deref().map(super::sanitize_detail),
        ..p
    }))
}
