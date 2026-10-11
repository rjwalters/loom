//! Do not re-dispatch an issue whose work has just landed (Issue #11429).
//!
//! Items were dispatched again minutes after a sweep reported
//! `disposition=landed`, and the new sweep ended `substantive_failure` with
//! nothing to do. Two checks refuse that dispatch with the typed
//! [`AlreadyLandedDispatchError`], which the work finder records as the
//! `already_landed` queue disposition and `pick.decision` skip reason:
//!
//! - this host's outcome journal has a `landed` `sweep.outcome` for the issue
//!   inside [`LANDED_REDISPATCH_WINDOW`] (no forge call); and
//! - the 2.5 closed-issue read, which dispatch already makes, finds the issue
//!   closed.
//!
//! A crash resume of the issue's own sweep (`resume_bypass_pr`) is not
//! refused by the journal check: its own landed record is the reason to
//! resume, and the 2.6 open-PR guard already decides that case.

use super::*;
use crate::telemetry::{SweepDisposition, TelemetryEnvelope, TelemetryRecord};

/// How long after a `landed` outcome this host refuses to dispatch the issue.
pub const LANDED_REDISPATCH_WINDOW: Duration = Duration::from_secs(30 * 60);

/// The `pick.decision` skip reason and `loom.dispatch.decisions` reason.
pub const ALREADY_LANDED_REASON: &str = "already_landed";

/// What showed that the issue's work already landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandedEvidence {
    /// The issue is closed on the forge.
    ForgeClosed,
    /// This host's outcome journal has a recent `landed` outcome.
    JournalLanded {
        sweep_id: String,
        pr_number: Option<u32>,
        age_secs: u64,
    },
}

/// Typed refusal: the issue's work already landed (Issue #11429). A
/// deliberate skip, not a dispatch failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlreadyLandedDispatchError {
    pub issue: u32,
    pub evidence: LandedEvidence,
}

impl AlreadyLandedDispatchError {
    /// The bounded queue-row detail: `forge_closed` or `journal_landed`.
    #[must_use]
    pub fn detail(&self) -> &'static str {
        match self.evidence {
            LandedEvidence::ForgeClosed => "forge_closed",
            LandedEvidence::JournalLanded { .. } => "journal_landed",
        }
    }
}

impl std::fmt::Display for AlreadyLandedDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let issue = self.issue;
        match &self.evidence {
            LandedEvidence::ForgeClosed => write!(
                f,
                "refusing to dispatch issue #{issue} ({ALREADY_LANDED_REASON}): it is closed on \
                 the forge (#4088 closed-issue guard, #11429)"
            ),
            LandedEvidence::JournalLanded {
                sweep_id,
                pr_number,
                age_secs,
            } => write!(
                f,
                "refusing to dispatch issue #{issue} ({ALREADY_LANDED_REASON}): sweep {sweep_id} \
                 landed it{} {age_secs}s ago on this host, inside the {}s re-dispatch window \
                 (#11429)",
                pr_number.map_or_else(String::new, |pr| format!(" as PR #{pr}")),
                LANDED_REDISPATCH_WINDOW.as_secs(),
            ),
        }
    }
}

impl std::error::Error for AlreadyLandedDispatchError {}

/// The newest `landed` outcome for `repo`#`issue` in `envelopes` emitted no
/// earlier than `window` before `now`, as `(sweep_id, pr_number, age_secs)`.
/// An outcome stamped after `now` (clock skew) counts as age 0.
#[must_use]
pub(crate) fn recent_landing(
    envelopes: &[TelemetryEnvelope],
    repo: &str,
    issue: u32,
    now: DateTime<Utc>,
    window: Duration,
) -> Option<(String, Option<u32>, u64)> {
    envelopes.iter().rev().find_map(|envelope| {
        let TelemetryRecord::SweepOutcome(record) = &envelope.record else {
            return None;
        };
        if record.issue != issue
            || record.repo.as_deref() != Some(repo)
            || record.disposition != SweepDisposition::Landed
        {
            return None;
        }
        let age = now
            .signed_duration_since(envelope.emitted_at)
            .to_std()
            .unwrap_or_default();
        (age <= window).then(|| (record.sweep_id.clone(), record.pr_number, age.as_secs()))
    })
}

impl SweepRegistry {
    /// Refuse `issue` when this host's journal shows it landed inside
    /// [`LANDED_REDISPATCH_WINDOW`] before `now`. Fails open: an unresolved
    /// repo or an unreadable journal never refuses.
    pub(super) fn refuse_recent_landing(&self, issue: u32, now: DateTime<Utc>) -> Result<()> {
        let Some(repo) = self.journal_repo_slug() else {
            return Ok(());
        };
        let path = self.config.resolve_outcome_telemetry_path();
        let Ok(journal) = std::fs::read_to_string(&path) else {
            return Ok(());
        };
        // Runs on every dispatch attempt: parse only the lines that can match.
        let envelopes: Vec<TelemetryEnvelope> = journal
            .lines()
            .filter(|line| line.contains("\"landed\""))
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let Some((sweep_id, pr_number, age_secs)) =
            recent_landing(&envelopes, &repo, issue, now, LANDED_REDISPATCH_WINDOW)
        else {
            return Ok(());
        };
        let refusal = AlreadyLandedDispatchError {
            issue,
            evidence: LandedEvidence::JournalLanded {
                sweep_id,
                pr_number,
                age_secs,
            },
        };
        log::info!("sweep_registry: {refusal}");
        Err(refusal.into())
    }

    /// Step 2.5: refuse a closed issue as `already_landed`, and a number that
    /// resolves to a pull request as before (#4088/#4504). Fails open.
    pub(super) fn refuse_closed_issue(&self, issue: u32) -> Result<()> {
        match self.guard_issue_gate(issue) {
            Some(IssueGate::Closed) => {
                let refusal = AlreadyLandedDispatchError {
                    issue,
                    evidence: LandedEvidence::ForgeClosed,
                };
                log::info!("sweep_registry: {refusal}");
                Err(refusal.into())
            }
            Some(IssueGate::PullRequest) => Err(anyhow!(
                "refusing to dispatch issue #{issue}: the number resolves to a pull request \
                 rather than an open issue (#4088/#4504 closed-issue guard). A watchdog \
                 re-dispatch must not re-claim a closed/merged issue or a PR number."
            )),
            Some(IssueGate::Open) | None => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "landed_guard_tests.rs"]
mod tests;
