//! The visible reason behind a Builder step that ended without a PR (#13054).
//!
//! A Builder step that ends `noop` or `failed` returns the issue to the
//! draw. Only the `/loom:sweep` orchestrator commented on a no-op, and only
//! when the Builder got far enough to write `.no-changes-needed`; a step run
//! by the step engine, or one cut short, left no trace of why it looped.
//! [`SweepRegistry::note_builder_outcome`] makes the comment a daemon-side
//! guarantee: one marked comment per step (keyed by its sweep id) carrying
//! the outcome's reason token and, when the marker file is still on disk, its
//! first 200 characters. It is a comment only: no label change.

use std::path::Path;

use super::step_ledger::StepRun;
use super::SweepRegistry;
use crate::step_dispatch::outcome_ledger::OutcomeClass;

/// Marker prefix on the comment; the sweep id follows, so a second record of
/// the same step is recognizable.
pub const STEP_OUTCOME_COMMENT_MARKER: &str = "<!-- loom:builder-step-outcome (#13054)";

/// Longest `.no-changes-needed` excerpt quoted.
const EXCERPT_CHARS: usize = 200;

/// Most sweep ids remembered for de-duplication.
const MAX_REMEMBERED: usize = 1024;

/// The first [`EXCERPT_CHARS`] characters of the Builder's no-changes marker
/// in `worktree`, if it is still there and non-empty.
#[must_use]
pub fn marker_excerpt(worktree: &Path) -> Option<String> {
    let text = std::fs::read_to_string(worktree.join(".no-changes-needed")).ok()?;
    let text: String = text.trim().chars().take(EXCERPT_CHARS).collect();
    (!text.is_empty()).then_some(text)
}

/// The comment body for `run`, or `None` when the outcome is not a Builder
/// `noop` / `failed`. Pure.
#[must_use]
pub fn outcome_notice_body(run: &StepRun, excerpt: Option<&str>) -> Option<String> {
    let marker = format!("{STEP_OUTCOME_COMMENT_MARKER} sweep={} -->", run.sweep_id);
    let reason = run
        .failure_class
        .as_deref()
        .unwrap_or(run.outcome.reason.as_str());
    match run.outcome.class {
        OutcomeClass::Noop => {
            let said = excerpt.map_or(String::new(), |e| format!(" Builder's note: {e}"));
            Some(format!(
                "{marker}\n**Builder found no actionable changes this pass** (reason: \
                 `{reason}`).{said} The claim was released without a PR and a no-op cooldown \
                 applies; this issue is parked after repeated no-op passes."
            ))
        }
        OutcomeClass::Failed => Some(format!(
            "{marker}\n**Builder step ended without a PR** (reason: `{reason}`). It left no \
             `.no-changes-needed` conclusion, so this is a failed pass, not a deliberate no-op. \
             The daemon will retry under its failure ladder."
        )),
        _ => None,
    }
}

impl SweepRegistry {
    /// Whether the notice for `sweep_id` is still due, remembering it when so.
    pub(crate) fn claim_outcome_notice(&mut self, sweep_id: &str) -> bool {
        if sweep_id.is_empty() {
            return false;
        }
        if self.step_outcome_notices.len() >= MAX_REMEMBERED {
            self.step_outcome_notices.clear();
        }
        self.step_outcome_notices.insert(sweep_id.to_string())
    }

    /// Post the reason a Builder step ended `noop` / `failed`, once per step.
    /// Best-effort and forge-less in tests.
    pub(crate) fn note_builder_outcome(&mut self, run: &StepRun) {
        if !matches!(run.outcome.class, OutcomeClass::Noop | OutcomeClass::Failed)
            || !self.claim_outcome_notice(&run.sweep_id)
        {
            return;
        }
        let worktree = self
            .config
            .workspace_root
            .join(".loom/worktrees")
            .join(format!("issue-{}", run.item));
        let excerpt = marker_excerpt(&worktree);
        let Some(body) = outcome_notice_body(run, excerpt.as_deref()) else {
            return;
        };
        log::info!(
            "step_ledger: #{} builder step {} ended {} ({})",
            run.item,
            run.sweep_id,
            run.outcome.class.as_str(),
            run.outcome.reason
        );
        if self.config.skip_label_flip {
            return;
        }
        let issue_arg = run.item.to_string();
        let mut comment = vec![
            "issue",
            "comment",
            issue_arg.as_str(),
            "--body",
            body.as_str(),
        ];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        comment.extend(repo_flag.iter().map(String::as_str));
        if let Err(e) = self.gh_write("step_outcome.comment", comment) {
            log::debug!("step_ledger: builder outcome comment for #{} failed: {e}", run.item);
        }
    }
}

#[cfg(test)]
#[path = "step_outcome_notice_tests.rs"]
mod tests;
