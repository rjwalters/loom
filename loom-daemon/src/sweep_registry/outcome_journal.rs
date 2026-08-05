//! Outcome journal (`sweep.outcome` telemetry) + phase-transition sampling.

use super::*;

/// One observed lifecycle-phase transition for a live sweep (Issue #4704):
/// the checkpoint phase marker and the instant [`SweepRegistry::reap_once`]
/// first observed it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PhaseObservation {
    /// The raw checkpoint phase marker (`"curator-done"`, `"judge-rejected"`,
    /// …) exactly as `sweep-checkpoint.sh` wrote it. Normalized to a lifecycle
    /// phase name by [`phase_label`] only at record-build time, so the stored
    /// observation stays faithful to what was on disk.
    phase: String,
    /// When this transition was first observed (reaper tick time, not the
    /// checkpoint's own `updated_at` — see [`SweepRegistry::sample_phase_transition`]).
    at: DateTime<Utc>,
    /// The `pr_number` the checkpoint carried at this observation, when it
    /// carried one (the sweep skill records it from `builder-done` onward).
    /// Capturing it here is what lets a *successful* sweep's outcome record
    /// name its PR without a forge round trip — the checkpoint itself is
    /// deleted on success, before the record is written.
    pr_number: Option<u32>,
}

/// Cap on retained [`PhaseObservation`]s per sweep (Issue #4704). A normal
/// lifecycle records at most ~6 (curator/builder/judge/doctor/judge/merge); the
/// Judge↔Doctor cycle is already bounded by the escalation ladder, so this cap
/// is a defensive backstop against an unbounded loop, not a normal-path limit.
/// Observations past the cap are dropped (the earliest are kept — they are the
/// ones the per-phase breakdown is built from).
pub(crate) const MAX_PHASE_OBSERVATIONS: usize = 32;

/// The normalized lifecycle phase name (see [`phase_label`]) whose completion
/// means the sweep merged — the `Success` signal for the durable
/// `sweep.outcome` record (Issue #4704).
pub(crate) const MERGE_PHASE_LABEL: &str = "merge";

/// Normalize a checkpoint phase marker to the lifecycle phase name the
/// telemetry schema documents (Issue #4704): `"curator-done"` → `"curator"`,
/// `"judge-rejected"` → `"judge"`, `"merge-done"` → `"merge"`. A marker with
/// neither known suffix is passed through unchanged rather than being guessed
/// at, so a future `VALID_PHASES` addition degrades to a readable raw label
/// instead of a truncated one.
#[must_use]
pub(crate) fn phase_label(checkpoint_phase: &str) -> &str {
    for suffix in ["-done", "-rejected"] {
        if let Some(head) = checkpoint_phase.strip_suffix(suffix) {
            return head;
        }
    }
    checkpoint_phase
}

impl SweepRegistry {
    /// Best-effort removal of this registry's sweep-journal entry for
    /// `issue` (Issue #3953). Never load-bearing — a missed removal is pruned
    /// the next time anything touches the journal — so failures are logged
    /// at `debug` and swallowed rather than propagated.
    pub(crate) fn journal_remove_best_effort(&self, issue: u32) {
        let journal_path = match self.config.resolve_journal_path() {
            Ok(p) => p,
            Err(e) => {
                log::debug!("sweep_journal: cannot resolve journal path for #{issue}: {e}");
                return;
            }
        };
        if let Err(e) = sweep_journal::remove_sweep_at(
            &journal_path,
            &self.config.workspace_root.display().to_string(),
            issue,
        ) {
            log::debug!("sweep_journal: failed to remove entry for #{issue}: {e}");
        }
    }

    /// Append one line to the durable terminal-outcomes journal (Issue #4644)
    /// for a single-issue sweep's terminal transition. Called at every site
    /// that already emits a terminal `SweepExited`/`SweepCrashed` bus event —
    /// the reaper's dead-child handling in [`reap_once`](Self::reap_once) and
    /// the operator/watchdog-initiated [`finish_cancel`](Self::finish_cancel)
    /// — so the record outlives both the in-memory registry's ~1h GC window
    /// and the in-memory-only event bus.
    ///
    /// Best-effort by contract, matching [`journal_remove_best_effort`](Self::journal_remove_best_effort):
    /// a write failure is logged and swallowed, never allowed to block
    /// reaping. Deliberately independent of the bus emission's own success —
    /// the two are separate side effects of the same terminal transition (see
    /// the `sweep_outcomes` module doc).
    // One parameter per journaled field; they come from four different points
    // in the reaper's terminal branches, so bundling them into a struct would
    // only move the same argument list to the construction site.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_outcome_journal(
        &self,
        issue: u32,
        sweep_id: &str,
        outcome: &str,
        exit_code: Option<i32>,
        death_class: Option<String>,
        duration_sec: i64,
        result: telemetry::SweepResult,
    ) {
        let path = self.config.resolve_outcomes_journal_path();
        let token_name = self
            .entries
            .get(sweep_id)
            .map(|info| info.token_name.clone())
            .unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string());
        let record = sweep_outcomes::OutcomeRecord {
            timestamp: Utc::now(),
            repo: self.config.workspace_root.display().to_string(),
            issue,
            sweep_id: sweep_id.to_string(),
            outcome: outcome.to_string(),
            exit_code,
            death_class,
            token_name,
            duration_sec,
        };
        if let Err(e) = sweep_outcomes::append_outcome(&path, &record) {
            log::warn!(
                "sweep_outcomes: failed to append terminal-outcome journal entry for issue \
                 #{issue} ({sweep_id}) at {}: {e} — best-effort, not blocking reap (#4644)",
                path.display()
            );
        }
        // Durable `sweep.outcome` telemetry record (Issue #4704, absorbs
        // #4137) — a SEPARATE journal from the one above (see the
        // `sweep_outcomes` module doc for why), carrying model/config/result
        // detail the #4644 journal was never meant to hold. Independent
        // best-effort side effect: never allowed to block reaping.
        self.append_outcome_telemetry_journal(issue, sweep_id, duration_sec, result);
    }

    /// Append one `sweep.outcome` telemetry record (Issue #4704, absorbs
    /// #4137) for `sweep_id`'s terminal transition, wrapped in a
    /// [`telemetry::TelemetryEnvelope`] and appended to the journal
    /// [`sweep_outcomes::append_outcome_telemetry`] manages. Called only from
    /// [`append_outcome_journal`](Self::append_outcome_journal) so every
    /// #4644 terminal-outcome line has a paired telemetry line.
    ///
    /// Best-effort like its sibling: a write failure is logged and swallowed.
    /// Everything except the repo slug + visibility tag is derived from state
    /// this registry already holds (the entry, plus the phase/PR observations
    /// it sampled while the sweep ran), so a terminal transition costs no forge
    /// round trip beyond the one cached `owner/repo` + visibility lookup — and
    /// even that is skipped entirely (never shelling to `gh`) when
    /// `skip_label_flip` is set, matching every other real-forge probe in this
    /// file. A skipped lookup still writes the record, with best-effort
    /// (workspace-path `repo`, private `visibility`) values.
    pub(crate) fn append_outcome_telemetry_journal(
        &self,
        issue: u32,
        sweep_id: &str,
        duration_sec: i64,
        result: telemetry::SweepResult,
    ) {
        let info = self.entries.get(sweep_id);
        let model = info.and_then(|i| i.model.clone());
        let effort = info.and_then(|i| i.effort.clone());
        let runtime = info.map(|i| i.runtime.clone());
        let token_name = info
            .map(|i| i.token_name.clone())
            .unwrap_or_else(|| UNKNOWN_TOKEN_NAME.to_string());
        let latest_phase = info.and_then(|i| i.latest_phase.clone());
        let started_at = info.map(|i| i.started_at);

        let mut config = BTreeMap::new();
        if let Some(runtime) = runtime {
            config.insert("runtime".to_string(), runtime);
        }
        config.insert("token_account".to_string(), token_name);
        // Issue #4809: attribute this sweep to the model-cost A/B experiment's
        // arm from its OWN dispatched model — the same inference the #3725
        // harvest already applies to `observe`-mode records
        // (`sweep_experiment::infer_arm_from_model`), reused here so a
        // `experiment`-mode dispatch (which forces exactly an opus/sonnet-family
        // model — see `resolve_autonomous_dispatch_model`) is attributed
        // identically without needing a separate stored "explicit arm" field.
        // Kept in the free-form `config` map (not a new top-level schema field)
        // so this is additive per #4703 with no schema-version bump. Inference
        // stays faithful under #4827's per-issue complexity stratification: the
        // stratum only shifts WHICH arm an issue lands in, never the arm →
        // model mapping this reads back.
        if let Some(arm) =
            crate::script_helpers::sweep_experiment::infer_arm_from_model(model.as_deref())
        {
            config.insert("arm".to_string(), arm.to_string());
        }

        // Per-phase breakdown from the transition history this registry
        // sampled while the sweep was alive (see `sample_phase_transition`).
        // Fallback (history empty — e.g. a sweep reconstructed after a daemon
        // restart, or one that died before the first reaper tick): a single
        // best-effort entry naming the last known phase, attributed the whole
        // duration. Empty when neither is available, never a fabricated phase.
        let phase_durations = started_at
            .map(|started_at| self.phase_durations_for(sweep_id, started_at))
            .unwrap_or_default();
        let phase_durations = if phase_durations.is_empty() {
            latest_phase
                .map(|phase| {
                    vec![telemetry::PhaseDuration {
                        phase: phase_label(&phase).to_string(),
                        duration_sec,
                    }]
                })
                .unwrap_or_default()
        } else {
            phase_durations
        };

        // The sweep's own PR, taken from the checkpoint values sampled while it
        // ran (free), falling back to the checkpoint still on disk for a sweep
        // that died before any sampling tick. Deliberately NOT a
        // `probe_open_linked_pr` GraphQL round trip: that would add one forge
        // call per terminal transition on the reaper's hot path (the exact
        // pressure the fleet rate-limit work exists to relieve), and it answers
        // a weaker question — "does the issue have any open linked PR" rather
        // than "which PR did this sweep produce".
        let pr_number = self.sampled_pr_number(sweep_id).or_else(|| {
            let started_at = started_at?;
            let checkpoint = self
                .config
                .checkpoint_dir()
                .join(format!("issue-{issue}.json"));
            checkpoint_written_by_run(&checkpoint, started_at)
                .then(|| read_checkpoint_pr_number(&checkpoint))
                .flatten()
        });

        let (repo, visibility) = if self.config.skip_label_flip {
            (
                self.config.workspace_root.display().to_string(),
                telemetry::RepoVisibility::Private,
            )
        } else {
            let repo_slug = self
                .resolve_owner_repo()
                .map(|(owner, repo)| format!("{owner}/{repo}"));
            let visibility = repo_slug
                .as_deref()
                .map(telemetry::visibility::derive_visibility)
                .unwrap_or(telemetry::RepoVisibility::Private);
            let repo =
                repo_slug.unwrap_or_else(|| self.config.workspace_root.display().to_string());
            (repo, visibility)
        };

        // Lines added/deleted (Issue #5357): prefer the value this registry
        // already sampled while the sweep was alive (see
        // `sample_phase_transition`'s opportunistic snapshot) — it is the
        // only source that survives a `--merge`-mode sweep's own synchronous
        // `merge-pr.sh` worktree cleanup, which can run and complete inside
        // the sweep's process *before* this reaper-side terminal transition
        // ever fires. Falls back to a live probe for a sweep that died
        // before any sampling tick but whose worktree still exists now (the
        // common Builder-only-dispatch case). Never a forge call either way
        // — both paths are local `git` subprocess invocations. Omitted
        // entirely (not `Some((0, 0))`) when neither source has anything.
        let (lines_added, lines_deleted) = self
            .sampled_loc(sweep_id)
            .or_else(|| self.probe_worktree_loc(issue))
            .map_or((None, None), |(added, deleted)| (Some(added), Some(deleted)));

        // Tokens in/out (Issue #5357): summed straight from the sweep's own
        // Claude Code transcripts, split into input/output axes (raw, NOT
        // cost-weighted — see the schema doc and `tokens_in`'s own doc for
        // why). Bounded to this run's own wall-clock window so a
        // re-dispatched issue's earlier runs are never folded in. Best
        // effort: any failure (no project dir, no matching session, pruned
        // logs) degrades to `None`, never a fabricated `0`.
        let (tokens_in, tokens_out) = started_at
            .and_then(|started_at| {
                let projects_dir = crate::transcript_tokens::claude_projects_dir()?;
                crate::transcript_tokens::sum_sweep_tokens_split(
                    &projects_dir,
                    &self.config.workspace_root,
                    issue,
                    Some((started_at, Utc::now())),
                )
            })
            .map_or((None, None), |(tin, tout)| (Some(tin), Some(tout)));

        let outcome_record = telemetry::SweepOutcomeRecord {
            repo,
            visibility,
            issue,
            sweep_id: sweep_id.to_string(),
            model,
            effort,
            config,
            phase_durations,
            total_duration_sec: duration_sec,
            result,
            pr_number,
            tokens_in,
            tokens_out,
            lines_added,
            lines_deleted,
        };
        let envelope = telemetry::TelemetryEnvelope::new(
            host_identity(),
            telemetry::TelemetryRecord::SweepOutcome(outcome_record),
        );
        let path = self.config.resolve_outcome_telemetry_path();
        if let Err(e) = sweep_outcomes::append_outcome_telemetry(&path, &envelope) {
            log::warn!(
                "sweep_outcomes: failed to append sweep.outcome telemetry record for issue \
                 #{issue} ({sweep_id}) at {}: {e} — best-effort, not blocking reap (#4704)",
                path.display()
            );
        }
    }

    // ------------------------------------------------------------------------
    // Lifecycle-phase transition sampling (Issue #4704)
    // ------------------------------------------------------------------------

    /// Record a lifecycle-phase transition for a **live** sweep when the
    /// on-disk checkpoint has advanced since the last observation (Issue
    /// #4704). Called once per live entry per [`reap_once`](Self::reap_once)
    /// tick — which includes the read-path reaps (`ListSweeps` /
    /// `GetSweepStatus` → [`reap_liveness`](Self::reap_liveness)), so the
    /// effective sampling resolution is at worst the 30s reaper timer and
    /// usually finer.
    ///
    /// Sampling — rather than reading a history off disk — is required because
    /// `sweep-checkpoint.sh` *overwrites* the checkpoint at every phase
    /// boundary and the sweep skill **deletes** it on success: the file is a
    /// point-in-time value that is gone precisely when a successful sweep's
    /// record is written. The #4009 freshness guard
    /// ([`checkpoint_written_by_run`]) still applies, so a checkpoint left by
    /// an earlier dispatch of the same issue is never misattributed to this
    /// run.
    ///
    /// The timestamp recorded is the *observation* instant, not the
    /// checkpoint's own `updated_at`: the daemon only ever reads the phase
    /// string (`read_checkpoint_phase`), and an observation time is the honest
    /// upper bound on when the transition happened given a polled source.
    ///
    /// The checkpoint's `pr_number` is captured on the same read (it is written
    /// by the sweep skill from `builder-done` onward), which is what lets the
    /// terminal record carry the sweep's own PR **without** a forge round trip
    /// — including for a successful sweep, whose checkpoint is deleted before
    /// the record is written.
    ///
    /// # Bus emission (Issue #4863)
    ///
    /// Every *newly observed* transition also publishes [`Event::SweepPhase`]
    /// on the attached bus — the only production emit site for that variant, and
    /// therefore the only source of the `sweep.phase` telemetry record the
    /// observability collector maps it to. It lives **here**, rather than in
    /// [`overlay_live_phase`](Self::overlay_live_phase), precisely because this
    /// function already dedupes against `phase_history`: the checkpoint is
    /// re-read on every tick, so an unguarded emit would republish the same
    /// phase for the whole time a sweep sits in it and flood the bus (and D1).
    /// The published `phase` is the **normalized** lifecycle name
    /// ([`phase_label`] — `"curator-done"` → `"curator"`), matching the
    /// `sweep.phase` schema's documented `curator|builder|judge|doctor|merge`
    /// vocabulary, while the stored [`PhaseObservation`] keeps the raw marker.
    pub(crate) fn sample_phase_transition(
        &mut self,
        sweep_id: &str,
        kind: &SweepKind,
        started_at: DateTime<Utc>,
    ) {
        let SweepKind::Issue(issue) = kind else {
            return;
        };
        let checkpoint = self
            .config
            .checkpoint_dir()
            .join(format!("issue-{issue}.json"));
        if !checkpoint_written_by_run(&checkpoint, started_at) {
            return;
        }
        let Some(phase) = read_checkpoint_phase(&checkpoint) else {
            return;
        };

        // Opportunistic LOC snapshot (Issue #5357): runs on EVERY tick the
        // checkpoint is valid, not just on a phase transition, so the most
        // recent diffstat is always cached before a `--merge`-mode sweep's
        // own synchronous worktree cleanup can remove it out from under the
        // terminal-transition write. A failed probe (worktree gone, no
        // mainline ref resolves yet) simply leaves the last successfully
        // sampled value in place rather than clearing it.
        if let Some(loc) = self.probe_worktree_loc(*issue) {
            self.sampled_loc.insert(sweep_id.to_string(), loc);
        }

        let pr_number = read_checkpoint_pr_number(&checkpoint);
        let history = self.phase_history.entry(sweep_id.to_string()).or_default();
        if history.last().map(|o| o.phase.as_str()) == Some(phase.as_str()) {
            // Unchanged phase since the previous tick — the common case. Still
            // absorb a `pr_number` that appeared after the transition was first
            // observed, so a PR opened mid-phase is not lost.
            if let Some(last) = history.last_mut() {
                if last.pr_number.is_none() {
                    last.pr_number = pr_number;
                }
            }
            return;
        }
        if history.len() >= MAX_PHASE_OBSERVATIONS {
            return;
        }
        history.push(PhaseObservation {
            phase: phase.clone(),
            at: Utc::now(),
            pr_number,
        });
        // Genuine transition — publish it (see the "Bus emission" doc section
        // above). Ordered after the history push so the dedupe state is already
        // committed even if a subscriber panics on delivery.
        self.emit_event(Event::SweepPhase {
            issue: *issue,
            phase: phase_label(&phase).to_string(),
            pr_number: pr_number.and_then(|n| i32::try_from(n).ok()),
            repo: None, // stamped by emit_event (#3929)
        });
    }

    /// The most recent `pr_number` observed on this sweep's checkpoint (Issue
    /// #4704), or `None` when the sweep never reached a phase that records one.
    /// Free (no forge call) — see [`sample_phase_transition`](Self::sample_phase_transition).
    pub(crate) fn sampled_pr_number(&self, sweep_id: &str) -> Option<u32> {
        self.phase_history
            .get(sweep_id)?
            .iter()
            .rev()
            .find_map(|o| o.pr_number)
    }

    /// The most recently opportunistically-sampled `(lines_added,
    /// lines_deleted)` for `sweep_id` (Issue #5357), or `None` when it was
    /// never successfully sampled — see [`sample_phase_transition`](Self::sample_phase_transition)'s
    /// per-tick snapshot and [`probe_worktree_loc`](Self::probe_worktree_loc)'s
    /// live-fallback sibling in [`append_outcome_telemetry_journal`](Self::append_outcome_telemetry_journal).
    pub(crate) fn sampled_loc(&self, sweep_id: &str) -> Option<(i64, i64)> {
        self.sampled_loc.get(sweep_id).copied()
    }

    /// Best-effort local diffstat for issue `N`'s worktree against its
    /// mainline merge base (Issue #5357) — never a forge call, and `None`
    /// (not a fabricated `0`) when the worktree is absent or the probe
    /// fails for any reason (no mainline ref resolves, not a git repo,
    /// `git` unavailable). Delegates to [`crate::git_utils::diff_stat_against_mainline`];
    /// this wrapper only resolves the worktree path from the issue number.
    pub(crate) fn probe_worktree_loc(&self, issue: u32) -> Option<(i64, i64)> {
        let wt = self.worktree_path(issue);
        if !wt.exists() {
            return None;
        }
        crate::git_utils::diff_stat_against_mainline(&wt)
    }

    /// Whether this sweep was ever observed completing the Merge phase (Issue
    /// #4704) — the strongest available "this sweep merged" signal, and the
    /// [`telemetry::SweepResult::Success`] the schema documents ("merged, or
    /// otherwise reached its successful terminal state").
    ///
    /// Load-bearing because exit codes are not always available: an entry with
    /// no retained `Child` handle (reconstructed after a daemon restart) is
    /// reaped via the `kill(pid, 0)` probe, which yields no code at all — so a
    /// merged sweep would otherwise be recorded as a failure purely because its
    /// exit status was unobservable.
    pub(crate) fn sampled_reached_merge(&self, sweep_id: &str) -> bool {
        self.phase_history.get(sweep_id).is_some_and(|history| {
            history
                .iter()
                .any(|o| phase_label(&o.phase) == MERGE_PHASE_LABEL)
        })
    }

    /// Build the `sweep.outcome` record's per-phase breakdown from the
    /// transition history sampled for `sweep_id` (Issue #4704).
    ///
    /// The checkpoint markers are phase *completions* (`curator-done` means the
    /// Curator phase finished), so the duration attributed to a phase is the
    /// interval ending at its own observation and beginning at the previous
    /// observation — or at `started_at` for the first. A phase that appears
    /// twice in one lifecycle (the Judge↔Doctor cycle) yields two entries, in
    /// lifecycle order, which is more faithful than collapsing them.
    ///
    /// The trailing in-flight segment — from the last observed completion to
    /// the terminal transition — is deliberately **not** emitted: the daemon
    /// does not know which phase the sweep was in, and inventing one would be a
    /// fabricated label. So the entries sum to at most `total_duration_sec`,
    /// never more.
    pub(crate) fn phase_durations_for(
        &self,
        sweep_id: &str,
        started_at: DateTime<Utc>,
    ) -> Vec<telemetry::PhaseDuration> {
        let Some(history) = self.phase_history.get(sweep_id) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(history.len());
        let mut prev = started_at;
        for observation in history {
            // `max(0)` guards against a clock step between observations; a
            // negative duration on the wire would be nonsense to any consumer.
            let duration_sec = (observation.at - prev).num_seconds().max(0);
            out.push(telemetry::PhaseDuration {
                phase: phase_label(&observation.phase).to_string(),
                duration_sec,
            });
            prev = observation.at;
        }
        out
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    /// AC: the durable outcomes journal is written on BOTH terminal shapes —
    /// a checkpoint-less `Exited` death (this test) and a checkpoint-present
    /// `Crashed` death (the exit-78 test above, and the dedicated crashed-shape
    /// test below) — at the same emit sites as the existing bus events.
    #[test]
    fn outcome_journal_records_exited_terminal_with_no_checkpoint() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());

        insert_dead_running_with_log(&mut registry, 4645, 0, "agent-2", "clean run, no markers\n");
        registry.reap_once();

        let path = registry.config().resolve_outcomes_journal_path();
        let records = sweep_outcomes::read_all(&path);
        let record = records
            .iter()
            .find(|r| r.issue == 4645)
            .expect("Exited terminal outcome must be journaled");
        assert_eq!(record.outcome, "exited");
        assert_eq!(record.repo, registry.config().workspace_root.display().to_string());
    }

    /// AC: the journal entry outlives `reap_once`'s in-memory GC of terminal
    /// entries (the ~1h `TERMINAL_RETENTION_SECS` window) — it is the whole
    /// point of a *durable* journal that it survives past the point the
    /// in-memory registry has forgotten the sweep ever existed.
    #[test]
    fn outcome_journal_entry_survives_reap_once_gc() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        let sweep_id = insert_dead_running_with_log(
            &mut registry,
            4646,
            0,
            "agent-3",
            "==== loom-daemon dispatch: sweep-issue-4646-0 ====\nToken selection failed:\n",
        );
        registry.reap_once();

        // Force this entry's terminal timestamp far enough in the past that
        // the NEXT tick's GC pass drops it from the in-memory registry.
        if let Some(info) = registry.entries.get_mut(&sweep_id) {
            match &mut info.state {
                SweepState::Exited { at, .. } | SweepState::Crashed { at } => {
                    *at = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS + 60);
                }
                other => panic!("expected a terminal state, got {other:?}"),
            }
        }
        registry.reap_once();
        assert!(
            !registry.entries.contains_key(&sweep_id),
            "in-memory entry should be GC'd by the retention-window pass"
        );

        let path = registry.config().resolve_outcomes_journal_path();
        let records = sweep_outcomes::read_all(&path);
        assert!(
            records.iter().any(|r| r.issue == 4646),
            "the durable journal entry must survive reap_once's in-memory GC"
        );
    }

    // ------------------------------------------------------------------------
    // `sweep.outcome` telemetry journal (Issue #4704, absorbs #4137)
    // ------------------------------------------------------------------------

    /// AC: every terminal transition that journals a #4644 [`OutcomeRecord`]
    /// ALSO journals a paired `sweep.outcome` telemetry record carrying
    /// model/config/result/refs the narrower #4644 record does not — the
    /// crashed shape here, `Failure`.
    #[test]
    fn telemetry_outcome_records_crashed_terminal_as_failure() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        let sweep_id = insert_dead_running_with_log(
            &mut registry,
            4704,
            0,
            "agent-4",
            "==== loom-daemon dispatch: sweep-issue-4704-0 ====\nToken selection failed:\n",
        );
        if let Some(info) = registry.entries.get_mut(&sweep_id) {
            info.model = Some("opus".to_string());
            info.effort = Some("high".to_string());
            info.latest_phase = Some("builder".to_string());
        }
        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records
            .iter()
            .find(|r| r.issue == 4704)
            .expect("sweep.outcome telemetry record must be journaled on a crash");
        assert_eq!(record.result, telemetry::SweepResult::Failure);
        assert_eq!(record.sweep_id, "sweep-issue-4704-0");
        assert_eq!(record.model.as_deref(), Some("opus"));
        assert_eq!(record.effort.as_deref(), Some("high"));
        assert_eq!(record.config.get("token_account").map(String::as_str), Some("agent-4"));
        // Issue #4809: an opus-family model is attributed to Arm A in the
        // free-form `config` map — additive, no schema-version bump.
        assert_eq!(record.config.get("arm").map(String::as_str), Some("A"));
        assert!(record.total_duration_sec >= 0);
        // skip_label_flip is set in the fixture registry, so the forge-probe
        // fields fall back to their private-safe / workspace-path defaults
        // rather than shelling out — never `Public`, never a stray PR number.
        assert_eq!(record.visibility, telemetry::RepoVisibility::Private);
        assert_eq!(record.pr_number, None);
    }

    /// AC ("result"): a sweep observed completing the Merge phase is
    /// classified `Success` — the schema's "merged" terminal state — even
    /// though its exit status was never captured (the kill-probe reap path
    /// yields no exit code) and its checkpoint was deleted on success.
    #[test]
    fn telemetry_outcome_records_merged_lifecycle_as_success() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 4705;

        let sweep_id =
            insert_dead_running_with_log(&mut registry, issue, 0, "agent-5", "clean run\n");
        let started_at = Utc::now() - Duration::from_secs(600);
        {
            let info = registry.entries.get_mut(&sweep_id).unwrap();
            info.model = Some("sonnet".to_string());
            info.started_at = started_at;
        }

        for phase in ["builder-done", "judge-done", "merge-done"] {
            write_checkpoint_with_mtime(&registry, issue, phase, SystemTime::now());
            registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
        }
        // The sweep skill deletes the checkpoint after a successful merge.
        std::fs::remove_file(
            registry
                .config
                .checkpoint_dir()
                .join(format!("issue-{issue}.json")),
        )
        .unwrap();

        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records
            .iter()
            .find(|r| r.issue == issue)
            .expect("sweep.outcome telemetry record must be journaled on a merged lifecycle");
        assert_eq!(record.result, telemetry::SweepResult::Success);
        assert_eq!(record.model.as_deref(), Some("sonnet"));
        // Issue #4809: sonnet-family attributes to Arm B.
        assert_eq!(record.config.get("arm").map(String::as_str), Some("B"));
        assert_eq!(
            record
                .phase_durations
                .iter()
                .map(|p| p.phase.as_str())
                .collect::<Vec<_>>(),
            vec!["builder", "judge", "merge"]
        );
    }

    /// Issue #4809: a model that is neither arm (or no model at all) carries
    /// NO `arm` key in the outcome record's `config` map — never a fabricated
    /// attribution.
    #[test]
    fn telemetry_outcome_records_no_arm_for_an_unattributable_model() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());

        let sweep_id =
            insert_dead_running_with_log(&mut registry, 4809, 0, "agent-15", "clean run\n");
        if let Some(info) = registry.entries.get_mut(&sweep_id) {
            info.model = Some("claude-haiku-5".to_string());
        }
        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == 4809).unwrap();
        assert_eq!(record.config.get("arm"), None);
    }

    /// A checkpoint-less death whose exit status was never observed is NOT
    /// success: an unobservable exit is not evidence that anything landed.
    #[test]
    fn telemetry_outcome_records_unverified_exit_as_failure() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());

        insert_dead_running_with_log(&mut registry, 4714, 0, "agent-14", "no markers\n");
        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == 4714).unwrap();
        assert_eq!(record.result, telemetry::SweepResult::Failure);
    }

    /// AC: an operator/watchdog-initiated cancel is telemetry-classified
    /// `Cancelled` — distinct from both `Success` and `Failure`.
    #[test]
    fn telemetry_outcome_records_cancel_as_cancelled() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());

        let kind = SweepKind::Issue(4706);
        let started_at = Utc::now();
        let sweep_id = "sweep-issue-4706-test".to_string();
        registry.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                pgid: None,
                sweep_id: sweep_id.clone(),
                kind: kind.clone(),
                pid: 2_147_483_640,
                token_name: "agent-6".into(),
                runtime: "claude".into(),
                runtime_source: None,
                log_path: registry.compute_log_path(4706),
                idempotency_key: None,
                started_at,
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: Some("opus".to_string()),
                effort: None,
                depends_on: None,
                repo: None,
            },
        );

        registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records
            .iter()
            .find(|r| r.issue == 4706)
            .expect("sweep.outcome telemetry record must be journaled on cancel");
        assert_eq!(record.result, telemetry::SweepResult::Cancelled);
        assert_eq!(record.model.as_deref(), Some("opus"));
    }

    /// AC: the telemetry journal entry survives `reap_once`'s in-memory GC of
    /// terminal entries, mirroring [`outcome_journal_entry_survives_reap_once_gc`]
    /// for the #4644 journal.
    #[test]
    fn telemetry_outcome_entry_survives_reap_once_gc() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        let sweep_id = insert_dead_running_with_log(
            &mut registry,
            4707,
            0,
            "agent-7",
            "==== loom-daemon dispatch: sweep-issue-4707-0 ====\nToken selection failed:\n",
        );
        registry.reap_once();

        if let Some(info) = registry.entries.get_mut(&sweep_id) {
            match &mut info.state {
                SweepState::Exited { at, .. } | SweepState::Crashed { at } => {
                    *at = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS + 60);
                }
                other => panic!("expected a terminal state, got {other:?}"),
            }
        }
        registry.reap_once();
        assert!(!registry.entries.contains_key(&sweep_id));

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        assert!(
            records.iter().any(|r| r.issue == 4707),
            "the durable telemetry journal entry must survive reap_once's in-memory GC"
        );
    }

    /// AC: with neither a sampled transition history nor a known
    /// `latest_phase`, `phase_durations` is empty rather than fabricating a
    /// phase name.
    #[test]
    fn telemetry_outcome_phase_durations_empty_without_a_known_phase() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());

        insert_dead_running_with_log(&mut registry, 4708, 0, "agent-8", "clean run\n");
        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == 4708).unwrap();
        assert!(record.phase_durations.is_empty());
    }

    /// AC (#4704 "per-phase durations"): the persisted record carries a REAL
    /// per-phase breakdown built from the transitions the registry sampled
    /// while the sweep was alive — one entry per observed phase completion, in
    /// lifecycle order, with the checkpoint markers normalized to lifecycle
    /// names and each duration measured from the previous observation.
    #[test]
    fn telemetry_outcome_phase_durations_come_from_sampled_transitions() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 4709;

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-9", "log\n");
        // Backdate the dispatch so the first phase's measured duration is
        // unambiguously non-zero (it runs from `started_at` to the first
        // observation).
        let started_at = Utc::now() - Duration::from_secs(300);
        registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

        // Two phase completions observed by (simulated) reaper ticks while the
        // sweep was still alive — exactly what `reap_once` does per tick.
        write_checkpoint_with_mtime(&registry, issue, "curator-done", SystemTime::now());
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
        write_checkpoint_with_mtime(&registry, issue, "judge-rejected", SystemTime::now());
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
        // A repeat tick with an unchanged checkpoint must NOT add an entry.
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);

        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records
            .iter()
            .find(|r| r.issue == issue)
            .expect("terminal transition must journal a sweep.outcome record");
        assert_eq!(
            record
                .phase_durations
                .iter()
                .map(|p| p.phase.as_str())
                .collect::<Vec<_>>(),
            vec!["curator", "judge"],
            "markers normalize to lifecycle names, in lifecycle order, deduped per tick"
        );
        assert!(
            record.phase_durations[0].duration_sec >= 299,
            "first phase spans dispatch -> first observation, got {}",
            record.phase_durations[0].duration_sec
        );
        let sum: i64 = record.phase_durations.iter().map(|p| p.duration_sec).sum();
        assert!(
            sum <= record.total_duration_sec,
            "the unattributed trailing in-flight segment means phases sum to at most the total \
             ({sum} > {})",
            record.total_duration_sec
        );
    }

    /// AC ("…and refs"): the record's `pr_number` comes from the checkpoint
    /// value sampled while the sweep ran — no forge round trip — and survives
    /// the checkpoint being deleted, which is what a *successful* sweep does
    /// before its record is written.
    #[test]
    fn telemetry_outcome_pr_number_comes_from_the_sampled_checkpoint() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 4713;

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-13", "log\n");
        let started_at = Utc::now() - Duration::from_secs(120);
        registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        let cp_path = cp_dir.join(format!("issue-{issue}.json"));
        std::fs::write(
            &cp_path,
            format!(r#"{{"phase":"builder-done","issue":{issue},"pr_number":8123}}"#),
        )
        .unwrap();
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
        // The sweep skill deletes the checkpoint on success — the sampled
        // observation must survive that.
        std::fs::remove_file(&cp_path).unwrap();

        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == issue).unwrap();
        assert_eq!(record.pr_number, Some(8123));
    }

    // --- Work-output capture: tokens_in/tokens_out, lines_added/lines_deleted (#5357) ---

    /// `git init` a worktree at `registry`'s `issue-<N>` path with `main` at
    /// one commit and a checked-out `feature` branch carrying one more
    /// commit: `new.txt` (+2 lines) and one appended `README.md` line — a
    /// deterministic `(3, 0)` diffstat against `main`'s merge base.
    fn seed_worktree_with_diff(registry: &SweepRegistry, issue: u32) -> PathBuf {
        let wt = registry.worktree_path(issue);
        std::fs::create_dir_all(&wt).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(&wt)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed in {}", wt.display());
        };
        git(&["init", "-q", "--initial-branch=main"]);
        git(&["config", "user.email", "loom@example.com"]);
        git(&["config", "user.name", "Loom Test"]);
        std::fs::write(wt.join("README.md"), "line1\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "seed"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(wt.join("README.md"), "line1\nline2\n").unwrap();
        std::fs::write(wt.join("new.txt"), "a\nb\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "work"]);
        wt
    }

    /// AC: LOC survives a `--merge`-mode sweep's own synchronous worktree
    /// cleanup, because it was opportunistically sampled (and cached) while
    /// the worktree was still live — the whole reason `sampled_loc` exists
    /// rather than only ever live-probing at outcome-write time.
    #[test]
    fn telemetry_outcome_lines_survive_worktree_removal_after_sampling() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 5357;

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-53", "log\n");
        let started_at = Utc::now() - Duration::from_secs(60);
        registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

        let wt = seed_worktree_with_diff(&registry, issue);
        write_checkpoint_with_mtime(&registry, issue, "builder-done", SystemTime::now());
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
        assert_eq!(
            registry.sampled_loc(&sweep_id),
            Some((3, 0)),
            "the tick must have sampled the diff"
        );

        // Simulate a self-merging sweep's own `merge-pr.sh` cleanup racing
        // ahead of this reaper's terminal-transition write.
        std::fs::remove_dir_all(&wt).unwrap();

        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == issue).unwrap();
        assert_eq!(record.lines_added, Some(3));
        assert_eq!(record.lines_deleted, Some(0));
    }

    /// AC: a sweep that died before any reap tick sampled it (no live process
    /// handle, reconstructed after a daemon restart, etc.) still gets its LOC
    /// via the outcome-write-time fallback probe, as long as the worktree
    /// still exists then — the common Builder-only-dispatch case the curator
    /// pass confirmed.
    #[test]
    fn telemetry_outcome_lines_fall_back_to_a_live_probe_without_prior_sampling() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 5358;

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-54", "log\n");
        registry.entries.get_mut(&sweep_id).unwrap().started_at =
            Utc::now() - Duration::from_secs(60);
        seed_worktree_with_diff(&registry, issue);
        assert_eq!(
            registry.sampled_loc(&sweep_id),
            None,
            "never sampled — no checkpoint was written"
        );

        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == issue).unwrap();
        assert_eq!(record.lines_added, Some(3));
        assert_eq!(record.lines_deleted, Some(0));
    }

    /// AC: no worktree, never sampled ⇒ the LOC fields are omitted entirely,
    /// never a fabricated `(0, 0)`.
    #[test]
    fn telemetry_outcome_omits_lines_when_the_worktree_never_existed() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 5359;

        insert_dead_running_with_log(&mut registry, issue, 0, "agent-55", "log\n");
        registry.reap_once();

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == issue).unwrap();
        assert_eq!(record.lines_added, None);
        assert_eq!(record.lines_deleted, None);
    }

    /// AC: `tokens_in`/`tokens_out` are aggregated from the sweep's own
    /// matched Claude Code transcripts, split by axis rather than combined —
    /// input includes both cache counters, output is `output_tokens` alone.
    #[test]
    #[serial]
    fn telemetry_outcome_tokens_come_from_matched_transcripts() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 5360;

        let config_dir = dir.path().join("claude-config");
        std::fs::create_dir_all(config_dir.join("projects")).unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-56", "log\n");
        registry.entries.get_mut(&sweep_id).unwrap().started_at =
            Utc::now() - Duration::from_secs(60);

        let project = config_dir
            .join("projects")
            .join(crate::transcript_tokens::project_slug(&registry.config.workspace_root));
        std::fs::create_dir_all(project.join("uuid-1/subagents")).unwrap();
        let head = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\
             \"<command-name>/loom:sweep</command-name>\\n\
             <command-args>{issue} --claim-owned {issue}</command-args>\"}}}}\n"
        );
        let usage = |input: u32, output: u32, read: u32, create: u32| {
            format!(
                "{{\"message\":{{\"usage\":{{\"input_tokens\":{input},\
                 \"output_tokens\":{output},\"cache_read_input_tokens\":{read},\
                 \"cache_creation_input_tokens\":{create}}}}}}}\n"
            )
        };
        std::fs::write(project.join("uuid-1.jsonl"), format!("{head}{}", usage(10, 20, 300, 40)))
            .unwrap();
        std::fs::write(project.join("uuid-1/subagents/agent-bld.jsonl"), usage(1, 2, 30, 4))
            .unwrap();

        registry.reap_once();

        std::env::remove_var("CLAUDE_CONFIG_DIR");

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == issue).unwrap();
        // input = (10+300+40) + (1+30+4) = 350 + 35; output = 20 + 2.
        assert_eq!(record.tokens_in, Some(385));
        assert_eq!(record.tokens_out, Some(22));
    }

    /// AC: a sweep with no attributable transcript (pruned logs, or none
    /// ever written) omits both token fields — never a fabricated `0`.
    #[test]
    #[serial]
    fn telemetry_outcome_omits_tokens_when_no_transcript_is_attributable() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 5361;

        let config_dir = dir.path().join("claude-config");
        std::fs::create_dir_all(config_dir.join("projects")).unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-57", "log\n");
        registry.entries.get_mut(&sweep_id).unwrap().started_at =
            Utc::now() - Duration::from_secs(60);

        registry.reap_once();

        std::env::remove_var("CLAUDE_CONFIG_DIR");

        let path = registry.config().resolve_outcome_telemetry_path();
        let records = sweep_outcomes::read_all_sweep_outcomes(&path);
        let record = records.iter().find(|r| r.issue == issue).unwrap();
        assert_eq!(record.tokens_in, None);
        assert_eq!(record.tokens_out, None);
    }

    /// A checkpoint left behind by an EARLIER dispatch of the same issue must
    /// never be sampled as this run's progress — the #4009 freshness guard
    /// (`checkpoint_written_by_run`) applies to phase sampling exactly as it
    /// does to every other checkpoint read in this file.
    #[test]
    fn phase_sampling_ignores_a_checkpoint_from_an_earlier_dispatch() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 4710;

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-10", "log\n");
        let started_at = Utc::now();
        registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

        // Checkpoint written an hour BEFORE this run started.
        write_checkpoint_with_mtime(
            &registry,
            issue,
            "builder-done",
            SystemTime::now() - Duration::from_secs(3600),
        );
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);

        assert!(
            !registry.phase_history.contains_key(&sweep_id),
            "a stale checkpoint must not seed this run's phase history"
        );
    }

    /// Retained phase observations are capped so a pathological Judge<->Doctor
    /// loop cannot grow the per-sweep history without bound.
    #[test]
    fn phase_history_is_capped() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 4711;

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-11", "log\n");
        let started_at = Utc::now() - Duration::from_secs(10);
        registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

        for i in 0..(MAX_PHASE_OBSERVATIONS * 2) {
            // Alternate so every tick looks like a genuine transition.
            let phase = if i.is_multiple_of(2) {
                "judge-rejected"
            } else {
                "doctor-done"
            };
            write_checkpoint_with_mtime(&registry, issue, phase, SystemTime::now());
            registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
        }

        assert_eq!(
            registry.phase_history.get(&sweep_id).map(Vec::len),
            Some(MAX_PHASE_OBSERVATIONS)
        );
    }

    /// The per-SweepId phase history is pruned alongside entry GC, so it
    /// cannot accumulate across many dispatches.
    #[test]
    fn phase_history_is_pruned_with_terminal_entry_gc() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let issue = 4712;

        let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-12", "log\n");
        let started_at = Utc::now() - Duration::from_secs(30);
        registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
        write_checkpoint_with_mtime(&registry, issue, "curator-done", SystemTime::now());
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
        assert!(registry.phase_history.contains_key(&sweep_id));

        registry.reap_once();
        if let Some(info) = registry.entries.get_mut(&sweep_id) {
            match &mut info.state {
                SweepState::Exited { at, .. } | SweepState::Crashed { at } => {
                    *at = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS + 60);
                }
                other => panic!("expected a terminal state, got {other:?}"),
            }
        }
        registry.reap_once();

        assert!(!registry.entries.contains_key(&sweep_id));
        assert!(
            !registry.phase_history.contains_key(&sweep_id),
            "phase history must be pruned with the GC'd entry"
        );
    }

    /// Checkpoint markers normalize to the lifecycle phase names the telemetry
    /// schema documents; an unknown marker passes through unchanged rather
    /// than being truncated at an arbitrary separator.
    #[test]
    fn phase_label_normalizes_checkpoint_markers() {
        assert_eq!(phase_label("curator-done"), "curator");
        assert_eq!(phase_label("builder-done"), "builder");
        assert_eq!(phase_label("judge-done"), "judge");
        assert_eq!(phase_label("judge-rejected"), "judge");
        assert_eq!(phase_label("doctor-done"), "doctor");
        assert_eq!(phase_label("merge-done"), "merge");
        // No known suffix ⇒ passed through verbatim.
        assert_eq!(phase_label("some-future-phase"), "some-future-phase");
        assert_eq!(phase_label("curator"), "curator");
    }

    // ------------------------------------------------------------------------
    // `Event::SweepPhase` bus emission (Issue #4863)
    // ------------------------------------------------------------------------

    /// Drive `n` reaper ticks and drain every `Event::SweepPhase` the registry
    /// published, returning the phase strings in emission order.
    async fn drain_phase_events(sub: &mut crate::event_bus::Subscription) -> Vec<String> {
        let mut phases = Vec::new();
        while let Ok(recv) = tokio::time::timeout(Duration::from_millis(250), sub.recv()).await {
            match recv {
                Ok(Event::SweepPhase { phase, .. }) => phases.push(phase),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        phases
    }

    /// AC (#4863): a live sweep advancing through phases publishes exactly ONE
    /// `sweep.issue.{N}.phase` event per transition — and republishes nothing
    /// on the ticks where it sits in the same phase. The dedupe matters as much
    /// as the emission: `reap_once` re-reads the checkpoint every tick (every
    /// 30s, plus every read-path reap), so an unguarded emit would republish
    /// the same phase for the sweep's entire residence in it.
    #[tokio::test]
    async fn phase_transitions_publish_one_sweep_phase_event_each() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let bus = Arc::new(crate::event_bus::EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let issue = 4863;
        let started_at = Utc::now() - Duration::from_secs(60);
        // A live-looking pid, so `reap_once` samples the phase without reaping
        // the entry — the real shape of a mid-flight sweep.
        let _sweep_id = insert_running_at(&mut registry, issue, 0, started_at);

        write_checkpoint_with_mtime(&registry, issue, "curator-done", SystemTime::now());
        registry.reap_once();
        // Two more ticks with the checkpoint unchanged: the sweep is still in
        // the same phase, so these must publish nothing.
        registry.reap_once();
        registry.reap_once();
        write_checkpoint_with_mtime(&registry, issue, "builder-done", SystemTime::now());
        registry.reap_once();
        registry.reap_once();

        assert_eq!(
            drain_phase_events(&mut sub).await,
            vec!["curator".to_string(), "builder".to_string()],
            "one event per transition, none while the sweep sits in a phase"
        );
    }

    /// AC (#4863): the emitted `phase` agrees with the `sweep.phase` schema's
    /// vocabulary (`curator|builder|judge|doctor|merge`) rather than carrying
    /// the raw checkpoint marker (`"judge-rejected"`), which is what the
    /// dashboard's `SweepPhaseName` renders.
    #[tokio::test]
    async fn emitted_phase_uses_the_schema_lifecycle_vocabulary() {
        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let bus = Arc::new(crate::event_bus::EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let issue = 4864;
        let started_at = Utc::now() - Duration::from_secs(60);
        insert_running_at(&mut registry, issue, 0, started_at);

        for marker in ["judge-rejected", "doctor-done", "merge-done"] {
            write_checkpoint_with_mtime(&registry, issue, marker, SystemTime::now());
            registry.reap_once();
        }

        assert_eq!(
            drain_phase_events(&mut sub).await,
            vec![
                "judge".to_string(),
                "doctor".to_string(),
                "merge".to_string()
            ]
        );
    }

    /// AC (#4863), end-to-end: a REAL phase transition — not a hand-built
    /// `Event::SweepPhase` fixture — produces a `sweep.phase` record on the
    /// durable telemetry queue via the observability collector's own
    /// event→record mapping. This is the link that was missing: every piece of
    /// the pipeline below was already correct and unit-tested, but nothing
    /// upstream ever published the event that feeds it.
    #[tokio::test]
    async fn a_real_phase_transition_reaches_the_telemetry_queue() {
        use crate::observability::queue::DurableQueue;
        use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

        let dir = tempdir().unwrap();
        let (mut registry, _rec) = fixture_registry(dir.path());
        let bus = Arc::new(crate::event_bus::EventBus::new());
        registry.set_event_bus(bus.clone());
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let issue = 4865;
        let started_at = Utc::now() - Duration::from_secs(60);
        let sweep_id = insert_running_at(&mut registry, issue, 0, started_at);

        write_checkpoint_with_mtime(&registry, issue, "builder-done", SystemTime::now());
        registry.reap_once();

        let event = loop {
            let recv = tokio::time::timeout(Duration::from_secs(5), sub.recv())
                .await
                .expect("a phase transition must publish an event");
            match recv.unwrap() {
                ev @ Event::SweepPhase { .. } => break ev,
                _ => continue,
            }
        };
        // The registry stamps its owning workspace root (#3929) on the way out.
        match &event {
            Event::SweepPhase { repo, .. } => assert_eq!(
                repo.as_deref(),
                Some(
                    registry
                        .config()
                        .workspace_root
                        .display()
                        .to_string()
                        .as_str()
                )
            ),
            other => panic!("expected SweepPhase, got {other:?}"),
        }

        // Same mapping the collector task applies to every bus event, with the
        // dispatch state a live daemon would already hold for this sweep.
        let mut dispatches = HashMap::new();
        crate::observability::collector::map_event_to_records(
            &Event::SweepGlobalDispatch {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(issue),
                runtime: None,
                runtime_source: None,
                repo: None,
            },
            issue,
            "rjwalters/loom",
            RepoVisibility::Public,
            &mut dispatches,
        );
        let records = crate::observability::collector::map_event_to_records(
            &event,
            issue,
            "rjwalters/loom",
            RepoVisibility::Public,
            &mut dispatches,
        );

        let queue = DurableQueue::open(dir.path().join("telemetry-queue.jsonl"), 64);
        for record in records {
            queue.push(TelemetryEnvelope::new("host-test", record));
        }

        assert_eq!(queue.len(), 1, "exactly one sweep.phase record queued");
        match &queue.peek_batch(1)[0].record {
            TelemetryRecord::SweepPhase(r) => {
                assert_eq!(r.issue, issue);
                assert_eq!(r.phase, "builder");
                assert_eq!(r.sweep_id, sweep_id);
                assert_eq!(r.repo, "rjwalters/loom");
            }
            other => panic!("expected a SweepPhase record on the queue, got {other:?}"),
        }
    }
}
