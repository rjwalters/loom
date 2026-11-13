//! Judge's projection: what a Judge step did to one PR (`judge_step_v1`,
//! #13274), a closed, versioned reason set; a new reason is a new version,
//! never a silent addition. Plus the review verdict itself (`judge_v1`,
//! #12362) when the run made one. Telemetry only: nothing here is read back by
//! routing, claims or the outcome ledger.
//!
//! Evidence, strongest first: the newest of a verdict write on the PR
//! (`post-verdict.sh` through `forge verdict-labels`, journaled as
//! `JournalEntry::Applied { by: "verdict-labels" }`, #12362) and a CI refusal
//! the run journaled (`post-verdict.sh` exit 5 / 6 through
//! `forge journal-ci-refusal`, #10486); then, on a step, its
//! `LOOM_STEP_OUTCOME role=judge` `class=` / `reason=`. A role-runner tick that
//! wrote no verdict projects nothing (change-only, like Champion); an idle tick
//! is counted by `role_tick.outcome.found_work` instead. The head is the one
//! the write or refusal named; absent without either.
//!
//! `judge_step_v1`:
//!
//! | class     | reasons                      |
//! |-----------|------------------------------|
//! | `verdict` | `reviewed`                   |
//! | `noop`    | `no_change`                  |
//! | `refused` | `ci_red`, `ci_incomplete`, `declined` |
//! | `failed`  | `failed`                     |
//!
//! `judge_v1` (a verdict write): class `verdict`, reason `approved` or
//! `changes_requested`.

use super::{Event, Evidence, Projection, Projector};

/// The Judge step's reason set.
pub const REASON_SET: &str = "judge_step_v1";

/// The reason set of a verdict the run wrote (#12362).
pub const VERDICT_REASON_SET: &str = "judge_v1";

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

/// The journal `by` of a verdict write.
pub const VERDICT_BY: &str = super::super::pick_journal::VERDICT_LABELS_BY;

/// The two verdicts a review writes.
pub const VERDICT_REASONS: [&str; 2] = ["approved", "changes_requested"];

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

/// The verdict a journal entry states: `(action, head_sha, at)`.
fn verdict_of(event: &Event) -> Option<(&str, Option<&str>, chrono::DateTime<chrono::Utc>)> {
    match event {
        Event::Applied {
            at,
            action,
            by,
            head_sha,
        } if by == VERDICT_BY && VERDICT_REASONS.contains(&action.as_str()) => {
            Some((action.as_str(), head_sha.as_deref(), *at))
        }
        _ => None,
    }
}

fn project(e: &Evidence) -> Option<Projection> {
    let written = e.events.iter().rev().find_map(verdict_of);
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
    // A verdict written after the last refusal is the newest evidence.
    if let Some((action, head_sha, at)) = written {
        if refusal.as_ref().is_none_or(|(r_at, ..)| at >= *r_at) {
            return Some(Projection {
                head_sha: head_sha.map(str::to_string),
                at: Some(at),
                wrote: true,
                ..super::projection("verdict", action, VERDICT_REASON_SET)
            });
        }
    }
    if !e.step_path {
        return None;
    }
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
