//! Attempt **lineage** on `sweep.outcome` (Issue #9444): `attempt_index`,
//! `previous_sweep_id` and `trigger`.
//!
//! Nothing linked consecutive sweeps: the outcome journal is keyed by
//! `sweep_id` only, and dispatch reasons died in log prose — 7,099
//! same-issue sweep pairs landed within five minutes of each other in the
//! #9444 window with nothing saying what triggered the retry. The daemon's
//! own durable outcome journal (`sweep-outcome-telemetry.jsonl`, bounded at
//! `MAX_JOURNAL_BYTES` / 30 days) is exactly "this host's registry, durably":
//! the lineage for a terminal sweep is computable from it at the terminal
//! transition with no new state and no forge calls.
//!
//! Scope note: "this host's journal" is the honest denominator for
//! `attempt_index` — an issue fought across several hosts shows a per-host
//! attempt count, not a fleet one. The `trigger` vocabulary is normative in
//! `telemetry-schema.md`'s rework table; in-sweep rework *events* (rebase,
//! conflict, CI rerun) arrive separately via the marker protocol in
//! [`super::rework`].

use crate::telemetry::{SweepDisposition, SweepOutcomeRecord, SweepResult};

/// One-based count of terminal sweeps for `repo`#`issue` in `prior` (the
/// journal's sweep.outcome records, in append order, excluding the record
/// being built), plus the previous attempt's `sweep_id`, plus the derived
/// dispatch trigger. `None` when the journal could not be read — the caller
/// omits all three fields rather than fabricating a first attempt.
#[must_use]
pub(crate) fn derive_lineage(
    prior: &[SweepOutcomeRecord],
    repo: &str,
    issue: u32,
    sweep_id: &str,
) -> Option<(u32, Option<String>, &'static str)> {
    let matching: Vec<&SweepOutcomeRecord> = prior
        .iter()
        .filter(|record| {
            record.issue == issue
                && record.repo.as_deref() == Some(repo)
                && record.sweep_id != sweep_id
        })
        .collect();
    let attempt_index = u32::try_from(matching.len()).ok()? + 1;
    let previous = matching.last();
    let trigger = match previous {
        None => crate::telemetry::trigger::FIRST,
        Some(prev) => trigger_from_previous(prev),
    };
    Some((attempt_index, previous.map(|prev| prev.sweep_id.clone()), trigger))
}

/// The trigger implied by the previous attempt's terminal state. Old journal
/// lines (pre-#9441) carry no `disposition`; their `result` +
/// `failure_class` still split the environmental/substantive vocabulary the
/// same way the disposition derivation draws it.
#[must_use]
fn trigger_from_previous(prev: &SweepOutcomeRecord) -> &'static str {
    use crate::telemetry::trigger;
    match prev.disposition {
        SweepDisposition::EnvFailure | SweepDisposition::Cancelled => {
            trigger::RETRY_AFTER_ENV_FAILURE
        }
        SweepDisposition::SubstantiveFailure => trigger::RETRY_AFTER_SUBSTANTIVE_FAILURE,
        // A re-dispatch after a landing/no-op/curator disposal is the
        // no-op re-dispatch storm (#9441) — an operator- or policy-level
        // decision this host cannot attribute further.
        SweepDisposition::Landed
        | SweepDisposition::NoopAlreadyDone
        | SweepDisposition::CuratorClosed
        | SweepDisposition::CuratorRescoped => trigger::OPERATOR_REDISPATCH,
        // Pre-#9441 journal lines deserialize as `Unknown`; their
        // result + failure_class still carry the env/substantive split.
        SweepDisposition::Unknown => legacy_trigger(prev),
    }
}

/// `trigger` for a journal line that predates `disposition`.
#[must_use]
fn legacy_trigger(prev: &SweepOutcomeRecord) -> &'static str {
    use crate::telemetry::trigger;
    match prev.result {
        SweepResult::Cancelled => trigger::RETRY_AFTER_ENV_FAILURE,
        SweepResult::Success => trigger::OPERATOR_REDISPATCH,
        SweepResult::Blocked | SweepResult::Failure => {
            let env_class = prev.failure_class.as_deref().is_some_and(|class| {
                class.starts_with("preflight")
                    || class.contains("exhaust")
                    || class == "no-usable-account"
            });
            if env_class {
                trigger::RETRY_AFTER_ENV_FAILURE
            } else {
                trigger::RETRY_AFTER_SUBSTANTIVE_FAILURE
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    fn record(sweep_id: &str, repo: &str, issue: u32, result: SweepResult) -> SweepOutcomeRecord {
        SweepOutcomeRecord {
            story_points: None,
            repo: Some(repo.to_string()),
            visibility: crate::telemetry::RepoVisibility::Private,
            tokens_status: None,
            tokens_status_reason: None,
            issue,
            sweep_id: sweep_id.to_string(),
            model: None,
            effort: None,
            config: Default::default(),
            phase_durations: Vec::new(),
            total_duration_sec: 60,
            result,
            pr_number: None,
            tokens_in: None,
            tokens_out: None,
            lines_added: None,
            lines_deleted: None,
            tokens_by_model: None,
            failure_class: None,
            models_used: None,
            doctor_cycles: None,
            judge_verdicts: None,
            runtime: None,
            provider: None,
            profile: None,
            complexity: None,
            repo_unresolved: false,
            disposition: crate::telemetry::SweepDisposition::Unknown,
            tokens_unattributed: None,
            attempt_index: None,
            previous_sweep_id: None,
            trigger: None,
            rework_events: None,
            pr_numbers: None,
            hw_lines_added: None,
            hw_lines_deleted: None,
            hw_files: None,
            generated_lines: None,
            test_lines: None,
        }
    }

    #[test]
    fn first_attempt() {
        let prior = vec![record("s-1", "o/r", 9, SweepResult::Success)];
        let (index, prev, trigger) = derive_lineage(&prior, "o/r", 8, "s-2").unwrap();
        assert_eq!(index, 1);
        assert_eq!(prev, None);
        assert_eq!(trigger, crate::telemetry::trigger::FIRST);
    }

    #[test]
    fn retry_after_env_failure_names_the_previous_sweep() {
        let mut prev = record("s-1", "o/r", 9, SweepResult::Failure);
        prev.disposition = SweepDisposition::EnvFailure;
        let (index, prev_id, trigger) = derive_lineage(&[prev], "o/r", 9, "s-2").unwrap();
        assert_eq!(index, 2);
        assert_eq!(prev_id.as_deref(), Some("s-1"));
        assert_eq!(trigger, crate::telemetry::trigger::RETRY_AFTER_ENV_FAILURE);
    }

    #[test]
    fn retry_after_substantive_failure_and_legacy_classification() {
        let mut prev = record("s-1", "o/r", 9, SweepResult::Failure);
        prev.disposition = SweepDisposition::SubstantiveFailure;
        let (_, _, trigger) = derive_lineage(&[prev], "o/r", 9, "s-2").unwrap();
        assert_eq!(trigger, crate::telemetry::trigger::RETRY_AFTER_SUBSTANTIVE_FAILURE);

        // A pre-#9441 line: no disposition, but a preflight failure class.
        let mut prev = record("s-1", "o/r", 9, SweepResult::Failure);
        prev.failure_class = Some("preflight-no-cli-start".to_string());
        let (_, _, trigger) = derive_lineage(&[prev], "o/r", 9, "s-2").unwrap();
        assert_eq!(trigger, crate::telemetry::trigger::RETRY_AFTER_ENV_FAILURE);

        // A substantive legacy failure with no class.
        let prev = record("s-1", "o/r", 9, SweepResult::Failure);
        let (_, _, trigger) = derive_lineage(&[prev], "o/r", 9, "s-2").unwrap();
        assert_eq!(trigger, crate::telemetry::trigger::RETRY_AFTER_SUBSTANTIVE_FAILURE);
    }

    #[test]
    fn filters_by_repo_and_issue_and_self() {
        let prior = vec![
            record("s-1", "o/r", 9, SweepResult::Success),
            record("s-2", "o/other", 9, SweepResult::Success),
            record("s-3", "o/r", 8, SweepResult::Success),
        ];
        let (index, prev, _) = derive_lineage(&prior, "o/r", 9, "s-1").unwrap();
        assert_eq!(index, 1, "self and sibling repo/issue rows are excluded");
        assert_eq!(prev, None);
    }
}
