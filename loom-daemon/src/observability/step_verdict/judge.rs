//! Judge's projection (#13274): what a Judge step did to one PR
//! (`judge_step_v1`), a closed, versioned reason set. A new reason is a new
//! version, never a silent addition.
//!
//! Evidence, strongest first: a CI refusal the run journaled
//! (`post-verdict.sh` exit 5 / 6 through `forge journal-ci-refusal`, #10486),
//! then the step's `LOOM_STEP_OUTCOME role=judge` `class=` / `reason=`. The
//! head is the one the refusal named; absent without a refusal.
//!
//! | class     | reasons                      |
//! |-----------|------------------------------|
//! | `verdict` | `reviewed`                   |
//! | `noop`    | `no_change`                  |
//! | `refused` | `ci_red`, `ci_incomplete`, `declined` |
//! | `failed`  | `failed`                     |

use super::{Event, Evidence, Projection, Projector};

/// The Judge step's reason set.
pub const REASON_SET: &str = "judge_step_v1";

/// Every reason of [`REASON_SET`].
pub const REASONS: [&str; 6] = [
    "reviewed",
    "no_change",
    "ci_red",
    "ci_incomplete",
    "declined",
    "failed",
];

/// The marker `forge journal-ci-refusal` journals; the class follows the
/// final colon.
pub const REFUSAL_MARKER_PREFIX: &str = "<!-- judge:ci-refused:";

const ROLE: &str = "judge";

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
    // The latest refusal wins: a later approval on the same head posts, and
    // its own step end has no refusal.
    let refusal = e.events.iter().rev().find_map(|event| match event {
        Event::Marker {
            at,
            marker,
            head_sha,
        } => {
            let class = marker
                .strip_prefix(REFUSAL_MARKER_PREFIX)?
                .split_whitespace()
                .next()?;
            let reason = REASONS
                .iter()
                .find(|r| **r == class && r.starts_with("ci_"))?;
            Some((*at, *reason, head_sha.clone()))
        }
        _ => None,
    });
    if let Some((at, reason, head_sha)) = refusal {
        return Some(Projection {
            head_sha,
            at: Some(at),
            ..super::projection("refused", reason, REASON_SET)
        });
    }
    let p = match e.outcome.as_deref() {
        Some("verdict") => super::projection("verdict", "reviewed", REASON_SET),
        Some("noop") => super::projection("noop", "no_change", REASON_SET),
        Some("refused") => super::projection("refused", "declined", REASON_SET),
        _ => super::projection("failed", "failed", REASON_SET),
    };
    Some(Projection {
        detail: e.reason.as_deref().map(super::sanitize_detail),
        ..p
    })
}
