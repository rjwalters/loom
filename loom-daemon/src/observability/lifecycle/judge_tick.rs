//! What a launched Judge role tick did, as `loom.found_work`,
//! `loom.noop_reason`, `loom.judge_verdict` and `loom.head_sha` on its
//! `loom.role_attempt` span and `found_work` / `noop_reason` / `verdicts` on
//! its `role_tick.outcome` (#12362). Telemetry only.
//!
//! Every input is local: the tick's pick journal (the `Applied` entry
//! `forge verdict-labels` appends, and the claim handoff / prompt PR), and the
//! tail of the role's own log. No forge call, no label read.

use std::path::Path;

use super::super::pick_journal::{self, JournalEntry};
use crate::telemetry::JudgeVerdictRecord;

/// `loom.found_work`.
pub const FOUND_WORK: &str = "loom.found_work";
/// `loom.noop_reason`.
pub const NOOP_REASON: &str = "loom.noop_reason";
/// `loom.head_sha`.
pub const HEAD_SHA: &str = "loom.head_sha";
/// `loom.judge_verdict`.
pub const JUDGE_VERDICT: &str = "loom.judge_verdict";

/// The reason an unassigned tick that won no claim and wrote no verdict
/// reports when its session named none.
pub const NO_CLAIM_NO_VERDICT: &str = "no_claim_no_verdict";

/// The journal `by` of a verdict write (`forge verdict-labels`).
const VERDICT_BY: &str = pick_journal::VERDICT_LABELS_BY;

/// How much of the role log's tail is searched for the session's outcome line.
const MAX_LOG_TAIL: usize = 64 * 1024;

/// One launched Judge tick's observed result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeTick {
    /// A claim was won (or the prompt named a PR) or a verdict was written.
    pub found_work: bool,
    /// Why no work was found; set exactly when `found_work` is `false`.
    pub noop_reason: Option<String>,
    /// One entry per PR a verdict was written on, in PR order.
    pub verdicts: Vec<JudgeVerdictRecord>,
    /// The PR of the newest verdict write.
    last_pr: Option<u32>,
}

impl JudgeTick {
    /// The newest verdict written, if any.
    #[must_use]
    pub fn latest(&self) -> Option<&JudgeVerdictRecord> {
        let pr = self.last_pr?;
        self.verdicts.iter().find(|v| v.pr_number == pr)
    }

    /// The PR the tick wrote its newest verdict on.
    #[must_use]
    pub fn verdict_pr(&self) -> Option<u32> {
        self.last_pr
    }
}

/// The tick's result from its journal entries, the PR it claimed (or was
/// assigned), and the session's own outcome reason.
#[must_use]
pub fn from_entries(
    entries: &[JournalEntry],
    claimed_pr: Option<u64>,
    session_reason: Option<String>,
) -> JudgeTick {
    let mut verdicts: Vec<JudgeVerdictRecord> = Vec::new();
    let mut last_pr = None;
    for entry in entries {
        let JournalEntry::Applied {
            number,
            action,
            by,
            head_sha,
            ..
        } = entry
        else {
            continue;
        };
        if by != VERDICT_BY || !matches!(action.as_str(), "approved" | "changes_requested") {
            continue;
        }
        let record = JudgeVerdictRecord {
            pr_number: *number,
            head_sha: head_sha.clone(),
            verdict: action.clone(),
        };
        // Newest write per PR wins (a superseded approval is re-written).
        verdicts.retain(|v| v.pr_number != *number);
        verdicts.push(record);
        last_pr = Some(*number);
    }
    verdicts.sort_by_key(|v| v.pr_number);
    let found_work = claimed_pr.is_some() || !verdicts.is_empty();
    JudgeTick {
        found_work,
        noop_reason: (!found_work)
            .then(|| session_reason.unwrap_or_else(|| NO_CLAIM_NO_VERDICT.to_string())),
        verdicts,
        last_pr,
    }
}

/// The `reason=` of the last `LOOM_STEP_OUTCOME role=judge` line in `log`,
/// searched only after the last `# LOOM_RUNTIME_RESOLVED` marker so an earlier
/// tick's line is never read as this one's. `None` when there is no marker or
/// no such line.
#[must_use]
pub fn session_reason(log: &str) -> Option<String> {
    let start = log.rfind(crate::launch_record::RUNTIME_RESOLVED_MARKER)?;
    let section = &log[start..];
    let mut cut = section.len().saturating_sub(MAX_LOG_TAIL);
    while !section.is_char_boundary(cut) {
        cut += 1;
    }
    section[cut..].lines().rev().find_map(|line| {
        let rest = line
            .trim()
            .strip_prefix(crate::step_dispatch::outcome_ledger::OUTCOME_LINE_PREFIX)?;
        let field = |key: &str| {
            rest.split_whitespace()
                .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        };
        (field("role")? == "judge")
            .then(|| field("reason").map(str::to_string))
            .flatten()
            .filter(|r| !r.is_empty())
    })
}

/// Observe the Judge tick that just ran for `root`: its journal is peeked, not
/// taken (the drain after the span closes still needs it).
#[must_use]
pub fn observe(root: &Path, claimed_pr: Option<u64>) -> JudgeTick {
    let entries = pick_journal::peek(root)
        .map(|j| j.entries)
        .unwrap_or_default();
    let log = crate::role_runner::role_log_path(&root.join(".loom").join("logs"), "judge");
    let reason = std::fs::read_to_string(log)
        .ok()
        .and_then(|text| session_reason(&text));
    from_entries(&entries, claimed_pr, reason)
}

#[cfg(test)]
#[path = "judge_tick/tests.rs"]
mod tests;
