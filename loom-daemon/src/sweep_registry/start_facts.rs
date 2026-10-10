//! A sweep's dispatch-time facts (Issue #11280): model, effort, runtime and
//! attempt lineage, put on `sweep.global.dispatch` (so on `sweep.started`)
//! and kept for `fleet.state`'s running rows. No forge read: the repo slug
//! comes from the cache the park-label guard already filled, and the lineage
//! from this host's outcome journal, exactly as `sweep.outcome` derives it.

use super::outcome_journal::lineage;
use super::*;
use crate::runtime_preference::DispatchAdmission;
use crate::telemetry::SweepStartFacts;

impl SweepRegistry {
    /// The start facts of every sweep this daemon dispatched and still holds.
    pub(crate) fn start_facts_snapshot(&self) -> HashMap<SweepId, SweepStartFacts> {
        self.start_facts.clone()
    }

    /// `(attempt_index, previous_sweep_id, trigger)` for `sweep_id`, from the
    /// same journal and derivation its `sweep.outcome` will use.
    fn attempt_lineage(
        &self,
        issue: u32,
        sweep_id: &str,
    ) -> Option<(u32, Option<String>, &'static str)> {
        let slug = if self.config.skip_label_flip {
            repo_slug_from_env()
        } else {
            self.resolve_owner_repo()
                .map(|(owner, repo)| format!("{owner}/{repo}"))
        }?;
        // `None` on an unreadable journal: unknown lineage is omitted, never
        // published as a known first attempt.
        let prior = sweep_outcomes::try_read_all_sweep_outcomes(
            &self.config.resolve_outcome_telemetry_path(),
        )?;
        lineage::derive_lineage(&prior, &slug, issue, sweep_id)
    }

    /// Emit `sweep.global.dispatch` for the entry just recorded under
    /// `sweep_id`. An `Issue` dispatch also records and carries its start
    /// facts; `model_source` is `None` for a `PrSet` dispatch, which has none.
    pub(super) fn emit_dispatch(
        &mut self,
        sweep_id: &SweepId,
        admission: &DispatchAdmission,
        story_points: Option<u32>,
        model_source: Option<&'static str>,
    ) {
        let Some(info) = self.entries.get(sweep_id) else {
            return;
        };
        let (kind, model, effort) = (info.kind.clone(), info.model.clone(), info.effort.clone());
        let runtime = admission.admitted.as_ref().map(|a| a.runtime.clone());
        let start = match (&kind, model_source) {
            (SweepKind::Issue(issue), Some(source)) => {
                let lineage = self.attempt_lineage(*issue, sweep_id);
                let facts = SweepStartFacts {
                    model,
                    effort,
                    model_source: Some(source.to_string()),
                    runtime: runtime.clone(),
                    attempt_index: lineage.as_ref().map(|l| l.0),
                    previous_sweep_id: lineage.as_ref().and_then(|l| l.1.clone()),
                    trigger: lineage.map(|l| l.2.to_string()),
                };
                self.start_facts.insert(sweep_id.clone(), facts.clone());
                Some(Box::new(facts))
            }
            _ => None,
        };
        self.emit_event(Event::SweepGlobalDispatch {
            sweep_id: sweep_id.clone(),
            kind,
            runtime,
            runtime_source: admission.admitted.as_ref().map(|a| a.source.clone()),
            // Stamped by `emit_event` -> `set_repo_if_absent` (#4201).
            repo: None,
            story_points,
            start,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "start_facts_tests.rs"]
mod tests;
