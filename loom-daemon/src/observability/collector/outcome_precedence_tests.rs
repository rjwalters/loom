//! Issue #9477: which of the two `sweep.outcome` emit paths wins.
//!
//! The backend ingests `sweep.outcome` with `INSERT OR IGNORE` against a
//! partial `UNIQUE(kind, sweep_id)` index, so for a given `sweep_id` exactly
//! ONE of the two paths that write it survives:
//!
//! - this live event-bus path, which builds a deliberately thin record
//!   ([`super::terminal_records`]: `config: {}`, `pr_number: None`,
//!   `phase_durations: []`);
//! - the reaper's durable outcome journal, drained by
//!   [`crate::observability::backfill`] — the rich record.
//!
//! The decision this module pins: **the journal wins**, implemented by
//! [`super::suppress_journal_owned_outcome`] dropping the live copy for every
//! `sweep_id` the journal also covers. A future change to either emit path
//! that silently flips it back to "whatever reaches the queue first" has to
//! delete a named test to do it.
//!
//! Driven through [`super::correlation::map_envelopes`] — the same entry point
//! [`super::handle_event`] calls — rather than the inner pure mapping, so the
//! *adopted-across-restart* correlation (#8720's registry evidence) is covered
//! by the precedence rule too, not just the dispatched-here one.
//!
//! In its own sibling module so `collector.rs`'s existing test file does not
//! grow (`scripts/check-file-size-budget.sh`).

use super::*;
use crate::sweep_registry::TrackedSweepIdentity;
use crate::telemetry::TelemetryRecord;

const REPO: &str = "rjwalters/loom";

fn exited(issue: u32) -> Event {
    Event::SweepExited {
        issue,
        exit_code: Some(0),
        duration_sec: 120,
        no_progress: false,
        death_class: None,
        repo: None,
    }
}

fn crashed(issue: u32) -> Event {
    Event::SweepCrashed {
        issue,
        checkpoint_phase: Some("builder".to_string()),
        classification: Some("execution-error".to_string()),
        death_class: None,
        repo: None,
    }
}

/// Map `event` exactly as `handle_event` does — correlation, then the #9477
/// suppression pass — and hand back what would reach the export queue.
fn queued(
    event: &Event,
    issue: u32,
    dispatches: &mut HashMap<DispatchKey, DispatchState>,
    evidence: &dyn Fn() -> Option<TrackedSweepIdentity>,
) -> Vec<TelemetryEnvelope> {
    let root = tempfile::tempdir().expect("tempdir");
    let mut envelopes = correlation::map_envelopes(
        event,
        issue,
        REPO,
        RepoVisibility::Private,
        root.path(),
        "host-test",
        dispatches,
        evidence,
    );
    suppress_journal_owned_outcome(&mut envelopes);
    envelopes
}

fn no_evidence() -> Option<TrackedSweepIdentity> {
    None
}

fn outcome_ids(envelopes: &[TelemetryEnvelope]) -> Vec<String> {
    envelopes
        .iter()
        .filter_map(|envelope| match &envelope.record {
            TelemetryRecord::SweepOutcome(record) => Some(record.sweep_id.clone()),
            _ => None,
        })
        .collect()
}

fn completed_ids(envelopes: &[TelemetryEnvelope]) -> Vec<String> {
    envelopes
        .iter()
        .filter_map(|envelope| match &envelope.record {
            TelemetryRecord::SweepCompleted(record) => Some(record.sweep_id.clone()),
            _ => None,
        })
        .collect()
}

fn dispatched(issue: u32, sweep_id: &str) -> HashMap<DispatchKey, DispatchState> {
    let mut dispatches = HashMap::new();
    dispatches.insert(
        (REPO.to_owned(), issue),
        DispatchState {
            sweep_id: sweep_id.to_string(),
            started_at: Utc::now() - chrono::Duration::seconds(120),
            trace_context: None,
        },
    );
    dispatches
}

#[test]
fn a_dispatched_sweeps_clean_exit_yields_no_live_outcome_record() {
    // The reaper journaled the rich record under this exact `sweep_id`
    // synchronously, before it published this event. Emitting the thin copy
    // here is what made the rich one lose the `INSERT OR IGNORE` race.
    let mut dispatches = dispatched(9477, "sweep-issue-9477-100");

    let envelopes = queued(&exited(9477), 9477, &mut dispatches, &no_evidence);

    assert!(
        outcome_ids(&envelopes).is_empty(),
        "the durable outcome journal owns sweep.outcome for a correlated sweep_id"
    );
    assert_eq!(
        completed_ids(&envelopes),
        vec!["sweep-issue-9477-100".to_string()],
        "sweep.completed is the live-moment record and must still be emitted"
    );
}

#[test]
fn a_dispatched_sweeps_crash_yields_no_live_outcome_record_either() {
    // Both terminal shapes journal before they publish, so both defer.
    let mut dispatches = dispatched(9478, "sweep-issue-9478-200");

    let envelopes = queued(&crashed(9478), 9478, &mut dispatches, &no_evidence);

    assert!(outcome_ids(&envelopes).is_empty());
    assert_eq!(completed_ids(&envelopes), vec!["sweep-issue-9478-200".to_string()]);
}

#[test]
fn a_sweep_adopted_across_a_restart_defers_to_the_journal_too() {
    // #8720: this process never saw the dispatch, but the owning registry
    // still knows the real id — and that is precisely the id the reaper is
    // journaling under right now, so the collision is the same one.
    let mut dispatches = HashMap::new();
    let evidence = || {
        Some(TrackedSweepIdentity {
            sweep_id: "sweep-issue-9479-300".to_string(),
            started_at: Utc::now() - chrono::Duration::seconds(600),
        })
    };

    let envelopes = queued(&exited(9479), 9479, &mut dispatches, &evidence);

    assert!(
        outcome_ids(&envelopes).is_empty(),
        "registry-adopted identity is a real journal-owned sweep_id, not a fallback"
    );
    assert_eq!(completed_ids(&envelopes), vec!["sweep-issue-9479-300".to_string()]);
}

#[test]
fn an_uncorrelated_terminal_transition_still_emits_under_the_synthesized_id() {
    // No tracked dispatch and no registry evidence. The reaper journals this
    // sweep under its OWN real id, never under `unknown-issue-{N}`, so the
    // two records cannot collide at ingest — suppressing here would drop a
    // row nothing else replaces.
    let mut dispatches = HashMap::new();

    let envelopes = queued(&exited(9480), 9480, &mut dispatches, &no_evidence);

    assert_eq!(
        outcome_ids(&envelopes),
        vec!["unknown-issue-9480".to_string()],
        "the synthesized-id fallback is the one shape the journal never covers"
    );
}

#[test]
fn suppression_leaves_a_non_terminal_events_records_alone() {
    // `sweep.phase` carries no `SweepOutcome` variant at all, so the retain's
    // catch-all arm must be a true no-op — pinned so a future refactor of the
    // match cannot start dropping other kinds.
    let mut dispatches = dispatched(9481, "sweep-issue-9481-400");
    let root = tempfile::tempdir().expect("tempdir");
    let event = Event::SweepPhase {
        issue: 9481,
        phase: "builder".to_string(),
        pr_number: None,
        repo: None,
    };
    let mut envelopes = correlation::map_envelopes(
        &event,
        9481,
        REPO,
        RepoVisibility::Private,
        root.path(),
        "host-test",
        &mut dispatches,
        &no_evidence,
    );
    let before = envelopes.clone();

    suppress_journal_owned_outcome(&mut envelopes);

    assert_eq!(envelopes, before);
    assert!(!envelopes.is_empty(), "a phase event does produce a record");
}

#[test]
fn the_mapping_itself_still_produces_the_outcome_record_it_always_did() {
    // Suppression is a separate pass on purpose: `map_event_to_records` stays
    // a complete, pure event -> records mapping, so the terminal-record tests
    // (and `backfill`'s own shape expectations) keep describing one thing.
    let records = map_event_to_records(
        &exited(9482),
        9482,
        REPO,
        RepoVisibility::Private,
        &mut dispatched(9482, "sweep-issue-9482-500"),
    );
    assert!(
        records
            .iter()
            .any(|record| matches!(record, TelemetryRecord::SweepOutcome(_))),
        "the pure mapping is unchanged; only handle_event's post-pass filters"
    );
}
