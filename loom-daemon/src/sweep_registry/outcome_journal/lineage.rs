//! Attempt **lineage** for a terminal sweep (Issue #9444): `attempt_index`,
//! `previous_sweep_id` and `trigger`, computed from this host's own durable
//! `sweep.outcome` telemetry journal.
//!
//! # Why the journal, and not new state
//!
//! The registry already writes every terminal sweep to
//! `.loom/logs/sweep-outcome-telemetry.jsonl` before this function needs it,
//! bounded at `MAX_JOURNAL_BYTES` / `MAX_JOURNAL_AGE_DAYS` (see
//! [`crate::sweep_outcomes`]). That file *is* "this host's registry, durably":
//! it survives a daemon restart, it is already keyed by `repo` + `issue` +
//! `sweep_id`, and it is already read on other paths. So the lineage is a
//! **derivation** — no counter to keep, no dispatch-time plumb to thread
//! through four entry points (`dispatch_sweep`, the epic supervisor, the work
//! finder, the role runner), and no forge call.
//!
//! # Two honest limits, stated rather than papered over
//!
//! - **The denominator is this host.** An issue fought across several hosts
//!   reports each host's own attempt count. A fleet-wide index is a window
//!   function on the backend, where every host's records sit in one table;
//!   synthesizing one here from a partial view would be wrong in a way nothing
//!   downstream could detect. `previous_sweep_id` inherits the same scope.
//! - **Retention bounds the count.** An issue whose earlier attempts have
//!   aged out of the 30-day window reports a lower `attempt_index` than its
//!   true lifetime one. That is the same bound every other reader of this
//!   journal already lives with, and it is far better than the alternative of
//!   an unbounded per-issue counter file nothing prunes.
//!
//! A journal that cannot be read yields **no lineage at all** — all three
//! fields absent — rather than a fabricated first attempt. "This is attempt 1"
//! and "nobody counted" must stay distinguishable.

use crate::telemetry::{self, AttemptLineage, PriorAttempt, ReworkEvent, SweepOutcomeRecord};

/// Derive this sweep's lineage from `prior` — the journal's `sweep.outcome`
/// records in append order, which may include this sweep's own record if the
/// caller read the journal after appending it.
///
/// `rework` is this attempt's own observed in-sweep rework; it only ever
/// refines the trigger for a predecessor that did **not** fail (see
/// [`telemetry::derive_trigger`]).
///
/// The `repo` filter is by `owner/name` slug. A record whose slug did not
/// resolve (#9442 leaves `repo` absent) cannot be attributed to any repo and is
/// therefore never counted — an under-count is the honest failure direction
/// here, since counting it would risk folding a different repo's attempts of
/// the same issue number into this one's chain.
#[must_use]
pub(crate) fn derive_lineage(
    prior: &[SweepOutcomeRecord],
    repo: &str,
    issue: u32,
    sweep_id: &str,
    rework: &[ReworkEvent],
) -> AttemptLineage {
    let previous = prior
        .iter()
        .filter(|record| {
            record.issue == issue
                && record.repo.as_deref() == Some(repo)
                // Exclude this sweep's own record: the caller may read the
                // journal at any point relative to its own append, and an
                // attempt must never be its own predecessor.
                && record.sweep_id != sweep_id
        })
        .collect::<Vec<_>>();
    let attempt_index = u32::try_from(previous.len())
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1);
    let last = previous.last();
    let trigger = telemetry::derive_trigger(
        last.map(|record| PriorAttempt {
            disposition: record.disposition,
            result: record.result,
            failure_class: record.failure_class.as_deref(),
        })
        .as_ref(),
        rework,
    );
    AttemptLineage {
        attempt_index,
        previous_sweep_id: last.map(|record| record.sweep_id.clone()),
        trigger,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::telemetry::{
        ReworkKind, SweepDisposition, SweepOutcomeRecord, SweepResult, SweepTrigger,
    };

    fn record(sweep_id: &str, repo: Option<&str>, issue: u32) -> SweepOutcomeRecord {
        SweepOutcomeRecord {
            repo: repo.map(ToString::to_string),
            repo_unresolved: repo.is_none(),
            visibility: telemetry::RepoVisibility::Private,
            issue,
            sweep_id: sweep_id.to_string(),
            model: None,
            effort: None,
            config: std::collections::BTreeMap::new(),
            phase_durations: Vec::new(),
            total_duration_sec: 60,
            result: SweepResult::Success,
            disposition: SweepDisposition::Landed,
            pr_number: Some(1),
            tokens_in: None,
            tokens_out: None,
            lines_added: None,
            lines_deleted: None,
            tokens_by_model: None,
            tokens_unattributed: None,
            failure_class: None,
            models_used: None,
            doctor_cycles: None,
            judge_verdicts: None,
            runtime: None,
            provider: None,
            profile: None,
            complexity: None,
            attempt_index: None,
            previous_sweep_id: None,
            trigger: None,
            rework_events: None,
        }
    }

    fn failed(sweep_id: &str, disposition: SweepDisposition) -> SweepOutcomeRecord {
        SweepOutcomeRecord {
            result: SweepResult::Failure,
            disposition,
            pr_number: None,
            failure_class: Some("unclassified:spawn-death".to_string()),
            ..record(sweep_id, Some("o/r"), 42)
        }
    }

    #[test]
    fn an_empty_journal_is_the_first_attempt() {
        let lineage = derive_lineage(&[], "o/r", 42, "s-1", &[]);
        assert_eq!(lineage.attempt_index, 1);
        assert_eq!(lineage.previous_sweep_id, None);
        assert_eq!(lineage.trigger, SweepTrigger::First);
    }

    /// AC3: a retry after a spawn death names its predecessor and reports
    /// `retry_after_env_failure`.
    #[test]
    fn a_retry_after_a_spawn_death_names_its_predecessor() {
        let prior = vec![failed("s-1", SweepDisposition::EnvFailure)];
        let lineage = derive_lineage(&prior, "o/r", 42, "s-2", &[]);
        assert_eq!(lineage.attempt_index, 2);
        assert_eq!(lineage.previous_sweep_id.as_deref(), Some("s-1"));
        assert_eq!(lineage.trigger, SweepTrigger::RetryAfterEnvFailure);
    }

    /// AC1: the predecessor landed a PR and this attempt's PR conflicted with
    /// its moved base ⇒ `trigger = merge_conflict`, classified environmental.
    #[test]
    fn a_conflict_after_a_landing_reports_the_merge_conflict_trigger() {
        let prior = vec![record("s-1", Some("o/r"), 42)];
        let rework = [ReworkEvent::new(ReworkKind::MergeConflict, None, None)];
        let lineage = derive_lineage(&prior, "o/r", 42, "s-2", &rework);
        assert_eq!(lineage.attempt_index, 2);
        assert_eq!(lineage.trigger, SweepTrigger::MergeConflict);
        assert_eq!(lineage.trigger.classification(), Some(telemetry::ReworkClass::Environmental));
    }

    /// The chain follows APPEND order, and only the immediately preceding
    /// attempt decides the trigger.
    #[test]
    fn the_latest_prior_attempt_is_the_predecessor() {
        let prior = vec![
            failed("s-1", SweepDisposition::EnvFailure),
            failed("s-2", SweepDisposition::SubstantiveFailure),
        ];
        let lineage = derive_lineage(&prior, "o/r", 42, "s-3", &[]);
        assert_eq!(lineage.attempt_index, 3);
        assert_eq!(lineage.previous_sweep_id.as_deref(), Some("s-2"));
        assert_eq!(lineage.trigger, SweepTrigger::RetryAfterSubstantiveFailure);
    }

    /// Another issue, another repo, an unresolved-slug record, and this
    /// sweep's own already-appended record are all excluded from the count.
    #[test]
    fn only_this_repo_and_issue_and_never_this_sweep_are_counted() {
        let prior = vec![
            record("s-1", Some("o/r"), 42),
            record("s-2", Some("o/other"), 42),
            record("s-3", Some("o/r"), 43),
            record("s-4", None, 42),
            record("s-5", Some("o/r"), 42),
        ];
        // "s-5" is this sweep's own record, already appended.
        let lineage = derive_lineage(&prior, "o/r", 42, "s-5", &[]);
        assert_eq!(lineage.attempt_index, 2);
        assert_eq!(lineage.previous_sweep_id.as_deref(), Some("s-1"));
    }

    /// The whole point of the field: an issue's attempts form a walkable
    /// chain, with no gaps and no self-links.
    #[test]
    fn successive_attempts_form_an_unbroken_chain() {
        let mut journal: Vec<SweepOutcomeRecord> = Vec::new();
        let mut seen: Vec<(u32, Option<String>)> = Vec::new();
        for n in 1..=5 {
            let sweep_id = format!("s-{n}");
            let lineage = derive_lineage(&journal, "o/r", 42, &sweep_id, &[]);
            seen.push((lineage.attempt_index, lineage.previous_sweep_id.clone()));
            journal.push(failed(&sweep_id, SweepDisposition::EnvFailure));
        }
        assert_eq!(
            seen,
            vec![
                (1, None),
                (2, Some("s-1".into())),
                (3, Some("s-2".into())),
                (4, Some("s-3".into())),
                (5, Some("s-4".into())),
            ]
        );
    }
}
