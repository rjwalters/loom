//! Judge's projection (#12362): one PR's review verdict, from the write
//! `post-verdict.sh` made (`forge verdict-labels`, journaled as
//! `JournalEntry::Applied { by: "verdict-labels" }`), else from the step's
//! `LOOM_STEP_OUTCOME` line. Telemetry only: nothing here is read back by
//! routing, claims or the outcome ledger.
//!
//! Evidence, strongest first: the newest verdict write on the PR, then the
//! step's own outcome line. A role-runner tick that wrote no verdict projects
//! nothing (change-only, like Champion); an idle tick is counted by
//! `role_tick.outcome.found_work` instead.

use super::{Event, Evidence, Projection, Projector};
use crate::step_dispatch::outcome_ledger::OUTCOME_LINE_PREFIX;

/// The Judge reason set.
pub const REASON_SET: &str = "judge_v1";

/// The journal `by` of a verdict write.
pub const VERDICT_BY: &str = super::super::pick_journal::VERDICT_LABELS_BY;

/// The two verdicts a review writes.
pub const VERDICT_REASONS: [&str; 2] = ["approved", "changes_requested"];

pub(super) const PROJECTOR: Projector = Projector {
    step_name,
    project,
    outcome_word,
};

fn step_name(_is_pr: bool) -> &'static str {
    "judge_pr"
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

/// The `class reason` of the last `LOOM_STEP_OUTCOME role=judge` line for
/// `item`, verbatim (`noop already_judged`).
fn outcome_word(text: &str, _is_pr: bool, item: u32) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let rest = line.trim().strip_prefix(OUTCOME_LINE_PREFIX)?;
        let field = |key: &str| {
            rest.split_whitespace()
                .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        };
        (field("role")? == "judge" && field("target")?.parse::<u32>().ok()? == item).then(|| {
            format!(
                "{} {}",
                field("class").unwrap_or_default(),
                field("reason").unwrap_or("unclassified")
            )
        })
    })
}

fn project(e: &Evidence) -> Option<Projection> {
    if let Some((action, head_sha, at)) = e.events.iter().rev().find_map(verdict_of) {
        return Some(Projection {
            class: "verdict",
            reason: action.to_string(),
            reason_set: REASON_SET,
            head_sha: head_sha.map(str::to_string),
            at: Some(at),
            wrote: true,
            ..Projection::default()
        });
    }
    if !e.step_path {
        return None;
    }
    let word = e.outcome.as_deref().unwrap_or_default();
    let (class, reason) = word.split_once(' ').unwrap_or((word, ""));
    let class = match class {
        "noop" => "noop",
        "refused" => "refused",
        "failed" => "failed",
        // A `verdict` line with no verdict write behind it is not one.
        _ => {
            return Some(Projection {
                class: "failed",
                reason: "unclassified".to_string(),
                reason_set: REASON_SET,
                ..Projection::default()
            });
        }
    };
    Some(Projection {
        class,
        reason: if reason.is_empty() {
            "unclassified"
        } else {
            reason
        }
        .to_string(),
        reason_set: REASON_SET,
        ..Projection::default()
    })
}
