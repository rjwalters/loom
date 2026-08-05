//! Local-journal backfill for `sweep.completed` / `sweep.outcome` telemetry
//! (Issue #5084).
//!
//! ## Problem
//!
//! [`super::collector`]'s live event-bus pipeline is the *only* path that
//! ever pushes a `sweep.completed`/`sweep.outcome` envelope onto the export
//! [`super::queue::DurableQueue`], and it can only attach the correct
//! `sweep_id` to a terminal `Event::SweepExited`/`Event::SweepCrashed` via an
//! in-memory `issue -> sweep_id` map populated by observing that sweep's own
//! `Event::SweepGlobalDispatch` earlier in **this process's** lifetime
//! ([`super::collector::DispatchState`]). A sweep **adopted across a daemon
//! restart** was dispatched by the previous process — this process never saw
//! its dispatch event — so the terminal event's mapping falls back to a
//! synthesized `unknown-issue-{N}` id (see
//! [`super::collector::map_event_to_records`]). The record can still be
//! pushed and exported, but under the wrong id, so any query keyed on the
//! real `sweep_id` (the dashboard's `sweep:<sweepId>` Durable Object deletion
//! included) finds nothing — the silent under-count and leaked `sweep:` entry
//! #5084 documents.
//!
//! ## Fix: the local outcome-telemetry journal is the queue of record
//!
//! [`crate::sweep_outcomes::append_outcome_telemetry`] already durably
//! records one `sweep.outcome` envelope, carrying the *correct* `sweep_id`
//! (read straight off the registry entry the reaper is holding, never from
//! the event-bus dispatch-tracking map), for **every** terminal transition —
//! restart-adopted or not — independent of whether the live event-bus
//! collector even observed the matching dispatch. This module treats that
//! journal as the export queue-of-record: a backfill pass reads every
//! envelope not yet attempted, synthesizes the paired `sweep.completed`
//! record the journal does not separately store (see
//! [`synthesize_completed`]), and pushes both onto the
//! [`super::queue::DurableQueue`] for the existing sender/exporter to drain —
//! the same path a live-observed terminal event already uses, so no new
//! egress code is needed.
//!
//! This is deliberately more general than "fix restart adoption": *any* lost
//! export (a transport failure, a queue drop, a daemon killed before its own
//! collector task got scheduled) self-heals on the next backfill pass, since
//! the local journal is unconditionally written by the reaper's terminal
//! transition and never depends on the export pipeline succeeding.
//!
//! ## Idempotency
//!
//! A daemon-side cursor ([`BackfillState`]) tracks the newest `emitted_at`
//! already handed to the queue, persisted to disk so a restart does not
//! re-walk the whole journal — but this is an efficiency optimization, **not**
//! the correctness mechanism: a crash between enqueueing a batch and
//! persisting the advanced cursor re-offers the same records on the next
//! pass. True idempotency (never double-counted in the backend) is the
//! backend's job — the `records` table's partial unique index on
//! `(kind, sweep_id)` for `sweep.completed`/`sweep.outcome` plus
//! `INSERT OR IGNORE` (see `dashboard/migrations`) — so a re-sent record is
//! silently absorbed rather than duplicated, however many times a backfill
//! pass re-offers it.
//!
//! ## Measurability
//!
//! [`pending_count`] answers "how many local outcome records has this daemon
//! not yet attempted to export" — the AC's "N local outcomes not yet in the
//! backend" gauge — without a round trip to the backend (this daemon has no
//! query-back channel to it). It performs the same computation
//! [`run_backfill_pass`] uses to decide what to enqueue, exposed separately
//! for `loom-daemon sweep-outcomes --pending-export`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::sweep_outcomes;
use crate::telemetry::{self, TelemetryEnvelope, TelemetryRecord};
use crate::workspace_pool::WorkspacePool;

use super::queue::DurableQueue;

/// Env var overriding the backfill cursor state path (test seam), mirroring
/// [`crate::sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV`].
pub const BACKFILL_STATE_PATH_ENV: &str = "LOOM_OBSERVABILITY_BACKFILL_STATE_PATH";

/// Default filename under `<workspace_root>/.loom/logs/`.
pub const BACKFILL_STATE_FILENAME: &str = "observability-backfill-state.json";

/// Resolve the default backfill cursor path: [`BACKFILL_STATE_PATH_ENV`]
/// override (non-empty), else
/// `<workspace_root>/.loom/logs/observability-backfill-state.json`.
#[must_use]
pub fn default_backfill_state_path(workspace_root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var(BACKFILL_STATE_PATH_ENV) {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    workspace_root
        .join(".loom")
        .join("logs")
        .join(BACKFILL_STATE_FILENAME)
}

/// Persisted backfill progress cursor: the newest `emitted_at` of a
/// telemetry-journal envelope this daemon has already handed to the export
/// queue. `None` means "nothing backfilled yet" (a fresh workspace, or a
/// missing/corrupt state file — see [`load_state`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BackfillState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_backfilled_emitted_at: Option<DateTime<Utc>>,
}

/// Load `path`'s cursor, defaulting to "nothing backfilled yet" on a
/// missing/corrupt file — matches every other journal reader's soft-fail
/// contract in this crate (a corrupt cursor degrades to "reprocess
/// everything", which the backend's idempotent ingest absorbs harmlessly,
/// rather than crashing the collector).
#[must_use]
pub fn load_state(path: &Path) -> BackfillState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

/// Persist `state` to `path` atomically (temp file + rename, mirroring
/// [`crate::observability::queue::persist_to`]), creating parent directories
/// as needed. Best-effort: a write failure is logged and swallowed — the next
/// pass simply re-offers already-queued records, which the backend's
/// idempotent ingest absorbs harmlessly.
pub fn save_state(path: &Path, state: &BackfillState) {
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            log::warn!(
                "observability: could not create backfill state dir {}: {error}",
                parent.display()
            );
            return;
        }
    }
    let json = match serde_json::to_string(state) {
        Ok(json) => json,
        Err(error) => {
            log::warn!("observability: failed to serialize backfill state: {error}");
            return;
        }
    };
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    if let Err(error) = std::fs::write(&temporary, json) {
        log::warn!(
            "observability: failed to write backfill state {}: {error}",
            temporary.display()
        );
        return;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        log::warn!(
            "observability: failed to persist backfill state to {}: {error}",
            path.display()
        );
    }
}

/// The telemetry-journal envelopes not yet handed to the export queue per
/// `state`'s cursor, in journal (chronological) order.
#[must_use]
fn pending_envelopes(
    telemetry_journal_path: &Path,
    state: &BackfillState,
) -> Vec<TelemetryEnvelope> {
    sweep_outcomes::read_all_outcome_telemetry(telemetry_journal_path)
        .into_iter()
        .filter(|envelope| match state.last_backfilled_emitted_at {
            Some(cursor) => envelope.emitted_at > cursor,
            None => true,
        })
        .collect()
}

/// How many local outcome-telemetry records under `workspace_root` have not
/// yet been attempted for export (AC "the gap is measurable"). Reads the
/// on-disk cursor fresh each call — cheap relative to its callers' cadence (a
/// CLI invocation, or once per [`super::SNAPSHOT_INTERVAL`]).
#[must_use]
pub fn pending_count(workspace_root: &Path) -> usize {
    let telemetry_path = sweep_outcomes::default_outcome_telemetry_path(workspace_root);
    let state_path = default_backfill_state_path(workspace_root);
    let state = load_state(&state_path);
    pending_envelopes(&telemetry_path, &state).len()
}

/// Synthesize the paired `sweep.completed` record a `sweep.outcome` envelope
/// implies. The telemetry journal only ever stores the outcome shape (see the
/// `sweep_outcomes` module doc for why they are separate journals), so the
/// completed record has to be rebuilt here rather than read back verbatim.
/// `completed_at` reuses the envelope's own `emitted_at`:
/// `SweepRegistry::append_outcome_telemetry_journal` stamps the envelope with
/// `Utc::now()` synchronously inside the same terminal-transition call that
/// determined the outcome, so the two are the same instant to journal-write
/// precision.
fn synthesize_completed(
    envelope: &TelemetryEnvelope,
    outcome: &telemetry::SweepOutcomeRecord,
) -> TelemetryEnvelope {
    TelemetryEnvelope {
        schema_version: envelope.schema_version,
        emitted_at: envelope.emitted_at,
        host_id: envelope.host_id.clone(),
        record: TelemetryRecord::SweepCompleted(telemetry::SweepCompletedRecord {
            repo: outcome.repo.clone(),
            visibility: outcome.visibility,
            issue: outcome.issue,
            sweep_id: outcome.sweep_id.clone(),
            completed_at: envelope.emitted_at,
            result: outcome.result,
        }),
    }
}

/// Run one backfill pass for a single workspace root: enqueue every pending
/// `sweep.outcome` record (paired with a [`synthesize_completed`]d
/// `sweep.completed`) onto `queue`, then advance and persist the cursor.
/// Returns the number of `sweep.outcome` records processed (the number of
/// envelopes pushed onto `queue` is twice this, for the paired `completed` +
/// `outcome` records).
pub fn run_backfill_pass(workspace_root: &Path, queue: &DurableQueue) -> usize {
    let telemetry_path = sweep_outcomes::default_outcome_telemetry_path(workspace_root);
    let state_path = default_backfill_state_path(workspace_root);
    let mut state = load_state(&state_path);
    let pending = pending_envelopes(&telemetry_path, &state);
    if pending.is_empty() {
        return 0;
    }
    let mut newest = state.last_backfilled_emitted_at;
    for envelope in &pending {
        match &envelope.record {
            TelemetryRecord::SweepOutcome(outcome) => {
                queue.push(synthesize_completed(envelope, outcome));
                queue.push(envelope.clone());
            }
            // Forward-compatible: the telemetry journal today only ever
            // carries `sweep.outcome` (see `read_all_sweep_outcomes`'s doc),
            // but a future kind sharing the file should still be exported by
            // this backfill path rather than silently dropped.
            _ => queue.push(envelope.clone()),
        }
        newest =
            Some(newest.map_or(envelope.emitted_at, |current| current.max(envelope.emitted_at)));
    }
    state.last_backfilled_emitted_at = newest;
    save_state(&state_path, &state);
    log::info!(
        "observability: backfilled {} local outcome record(s) for {} onto the export queue",
        pending.len(),
        workspace_root.display()
    );
    pending.len()
}

/// Run a backfill pass across every workspace this daemon knows about:
/// `primary_workspace_root` (the daemon's own managed root, included even if
/// not yet provisioned in `workspace_pool` — e.g. no sweep dispatched there
/// yet in this process) plus every additional repo
/// [`WorkspacePool::provisioned_registries`] currently tracks. Mirrors the
/// same "iterate every provisioned root" pattern
/// [`super::collector::collect_active_sweep_ids`] and
/// [`super::collector::collect_managed_repos`] already use — a multi-repo
/// daemon's backfill must cover every repo's own local journal, not just the
/// primary one (the reported incident spanned repos, not just issues within
/// one).
pub fn run_backfill_pass_all(
    primary_workspace_root: &Path,
    workspace_pool: &WorkspacePool,
    queue: &DurableQueue,
) -> usize {
    let mut roots: Vec<PathBuf> = vec![primary_workspace_root.to_path_buf()];
    for registry in workspace_pool.provisioned_registries() {
        let root = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config()
            .workspace_root
            .clone();
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
        .iter()
        .map(|root| run_backfill_pass(root, queue))
        .sum()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::event_bus::EventBus;
    use serial_test::serial;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn outcome_envelope(issue: u32, sweep_id: &str, host_id: &str) -> TelemetryEnvelope {
        TelemetryEnvelope::new(
            host_id,
            TelemetryRecord::SweepOutcome(telemetry::SweepOutcomeRecord {
                repo: "rjwalters/loom".to_string(),
                visibility: telemetry::RepoVisibility::Public,
                issue,
                sweep_id: sweep_id.to_string(),
                model: Some("opus".to_string()),
                effort: None,
                config: std::collections::BTreeMap::new(),
                phase_durations: Vec::new(),
                total_duration_sec: 42,
                result: telemetry::SweepResult::Success,
                pr_number: Some(100 + issue),
                tokens_in: None,
                tokens_out: None,
                lines_added: None,
                lines_deleted: None,
            }),
        )
    }

    #[test]
    fn pending_envelopes_with_no_cursor_returns_everything() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcome-telemetry.jsonl");
        sweep_outcomes::append_outcome_telemetry(
            &path,
            &outcome_envelope(1, "sweep-issue-1-0", "host-a"),
        )
        .unwrap();
        sweep_outcomes::append_outcome_telemetry(
            &path,
            &outcome_envelope(2, "sweep-issue-2-0", "host-a"),
        )
        .unwrap();

        let pending = pending_envelopes(&path, &BackfillState::default());
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn pending_envelopes_excludes_everything_at_or_before_the_cursor() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sweep-outcome-telemetry.jsonl");
        let first = outcome_envelope(1, "sweep-issue-1-0", "host-a");
        sweep_outcomes::append_outcome_telemetry(&path, &first).unwrap();
        sweep_outcomes::append_outcome_telemetry(
            &path,
            &outcome_envelope(2, "sweep-issue-2-0", "host-a"),
        )
        .unwrap();

        let state = BackfillState {
            last_backfilled_emitted_at: Some(first.emitted_at),
        };
        let pending = pending_envelopes(&path, &state);
        assert_eq!(pending.len(), 1, "only the envelope strictly after the cursor is pending");
        match &pending[0].record {
            TelemetryRecord::SweepOutcome(r) => assert_eq!(r.issue, 2),
            other => panic!("unexpected record {other:?}"),
        }
    }

    #[test]
    fn state_round_trips_through_save_and_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("backfill-state.json");
        assert_eq!(load_state(&path), BackfillState::default(), "missing file -> default state");

        let state = BackfillState {
            last_backfilled_emitted_at: Some(Utc::now()),
        };
        save_state(&path, &state);
        assert_eq!(load_state(&path), state);
    }

    #[test]
    fn load_state_of_a_corrupt_file_degrades_to_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("backfill-state.json");
        std::fs::write(&path, "{ not json }").unwrap();
        assert_eq!(load_state(&path), BackfillState::default());
    }

    // Serialized against the other tests in this module: they mutate the
    // process-wide `BACKFILL_STATE_PATH_ENV` var (leaked for the duration of
    // their body), and `#[serial]` only serializes against other `#[serial]`
    // tests — a non-serial reader can otherwise observe a sibling's leaked
    // override and flake nondeterministically (#5133).
    #[test]
    #[serial]
    fn default_backfill_state_path_is_under_loom_logs() {
        let workspace = Path::new("/workspace/repo");
        let path = default_backfill_state_path(workspace);
        assert_eq!(
            path,
            workspace
                .join(".loom")
                .join("logs")
                .join(BACKFILL_STATE_FILENAME)
        );
    }

    /// AC: a sweep adopted across a daemon restart — no
    /// `Event::SweepGlobalDispatch` ever observed in this process, only the
    /// local journal record — still produces a matching `sweep.completed` +
    /// `sweep.outcome` pair on the export queue, carrying the REAL sweep id
    /// (never the collector's `unknown-issue-{N}` fallback).
    #[test]
    #[serial]
    fn backfill_pushes_a_completed_and_outcome_pair_with_the_real_sweep_id() {
        let dir = tempdir().unwrap();
        std::env::set_var(
            sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV,
            dir.path().join("telemetry.jsonl"),
        );
        std::env::set_var(super::BACKFILL_STATE_PATH_ENV, dir.path().join("cursor.json"));

        sweep_outcomes::append_outcome_telemetry(
            &sweep_outcomes::default_outcome_telemetry_path(dir.path()),
            &outcome_envelope(4989, "sweep-issue-4989-restart-adopted", "robb-studio"),
        )
        .unwrap();

        let queue = DurableQueue::open(dir.path().join("queue.jsonl"), 10);
        let processed = run_backfill_pass(dir.path(), &queue);

        std::env::remove_var(sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV);
        std::env::remove_var(super::BACKFILL_STATE_PATH_ENV);

        assert_eq!(processed, 1);
        let batch = queue.peek_batch(10);
        assert_eq!(batch.len(), 2, "one sweep.completed + one sweep.outcome");
        match &batch[0].record {
            TelemetryRecord::SweepCompleted(r) => {
                assert_eq!(r.sweep_id, "sweep-issue-4989-restart-adopted");
                assert_eq!(r.issue, 4989);
                assert_eq!(r.result, telemetry::SweepResult::Success);
            }
            other => panic!("expected SweepCompleted first, got {other:?}"),
        }
        match &batch[1].record {
            TelemetryRecord::SweepOutcome(r) => {
                assert_eq!(r.sweep_id, "sweep-issue-4989-restart-adopted");
            }
            other => panic!("expected SweepOutcome second, got {other:?}"),
        }
    }

    /// AC: backfill is idempotent at the daemon-side cursor level — a second
    /// pass with no new journal entries enqueues nothing more.
    #[test]
    #[serial]
    fn a_second_pass_with_no_new_entries_enqueues_nothing() {
        let dir = tempdir().unwrap();
        let telemetry_path = dir.path().join("telemetry.jsonl");
        sweep_outcomes::append_outcome_telemetry(
            &telemetry_path,
            &outcome_envelope(1, "sweep-issue-1-0", "host-a"),
        )
        .unwrap();

        std::env::set_var(sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV, &telemetry_path);
        std::env::set_var(super::BACKFILL_STATE_PATH_ENV, dir.path().join("cursor.json"));

        let queue = DurableQueue::open(dir.path().join("queue.jsonl"), 10);
        let first_pass = run_backfill_pass(dir.path(), &queue);
        assert_eq!(first_pass, 1);
        queue.ack(queue.len()); // simulate a successful export drain

        let second_pass = run_backfill_pass(dir.path(), &queue);

        std::env::remove_var(sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV);
        std::env::remove_var(super::BACKFILL_STATE_PATH_ENV);

        assert_eq!(second_pass, 0, "the already-backfilled entry must not be re-enqueued");
        assert!(queue.is_empty());
    }

    /// AC: newly appended journal entries ARE picked up on a subsequent pass
    /// — the cursor advances forward, it does not stall after the first run.
    #[test]
    #[serial]
    fn a_later_pass_picks_up_newly_appended_entries() {
        let dir = tempdir().unwrap();
        let telemetry_path = dir.path().join("telemetry.jsonl");
        std::env::set_var(sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV, &telemetry_path);
        std::env::set_var(super::BACKFILL_STATE_PATH_ENV, dir.path().join("cursor.json"));

        sweep_outcomes::append_outcome_telemetry(
            &telemetry_path,
            &outcome_envelope(1, "sweep-issue-1-0", "host-a"),
        )
        .unwrap();
        let queue = DurableQueue::open(dir.path().join("queue.jsonl"), 10);
        assert_eq!(run_backfill_pass(dir.path(), &queue), 1);
        queue.ack(queue.len());

        sweep_outcomes::append_outcome_telemetry(
            &telemetry_path,
            &outcome_envelope(2, "sweep-issue-2-0", "host-a"),
        )
        .unwrap();
        let second_pass = run_backfill_pass(dir.path(), &queue);

        std::env::remove_var(sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV);
        std::env::remove_var(super::BACKFILL_STATE_PATH_ENV);

        assert_eq!(second_pass, 1);
        assert_eq!(queue.len(), 2);
    }

    /// AC ("measurable"): [`pending_count`] reflects exactly what the next
    /// backfill pass would enqueue, before and after a pass runs.
    #[test]
    #[serial]
    fn pending_count_tracks_the_backfill_cursor() {
        let dir = tempdir().unwrap();
        std::env::set_var(
            sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV,
            dir.path().join("telemetry.jsonl"),
        );
        std::env::set_var(super::BACKFILL_STATE_PATH_ENV, dir.path().join("cursor.json"));

        assert_eq!(pending_count(dir.path()), 0, "no journal yet -> nothing pending");

        sweep_outcomes::append_outcome_telemetry(
            &sweep_outcomes::default_outcome_telemetry_path(dir.path()),
            &outcome_envelope(1, "sweep-issue-1-0", "host-a"),
        )
        .unwrap();
        assert_eq!(pending_count(dir.path()), 1);

        let queue = DurableQueue::open(dir.path().join("queue.jsonl"), 10);
        run_backfill_pass(dir.path(), &queue);
        assert_eq!(pending_count(dir.path()), 0, "a completed pass clears the backlog");

        std::env::remove_var(sweep_outcomes::OUTCOME_TELEMETRY_JOURNAL_PATH_ENV);
        std::env::remove_var(super::BACKFILL_STATE_PATH_ENV);
    }

    /// AC: `run_backfill_pass_all` covers every repo the pool has
    /// provisioned, not just the primary workspace root — a multi-repo
    /// daemon's adopted sweeps can live in a sibling repo's own journal.
    #[tokio::test]
    #[serial]
    async fn run_backfill_pass_all_covers_every_provisioned_workspace() {
        let dir = tempdir().unwrap();
        let primary = dir.path().join("primary");
        let sibling = dir.path().join("sibling");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        sweep_outcomes::append_outcome_telemetry(
            &sweep_outcomes::default_outcome_telemetry_path(&primary),
            &outcome_envelope(1, "sweep-issue-1-0", "host-a"),
        )
        .unwrap();
        sweep_outcomes::append_outcome_telemetry(
            &sweep_outcomes::default_outcome_telemetry_path(&sibling),
            &outcome_envelope(2, "sweep-issue-2-0", "host-a"),
        )
        .unwrap();

        let pool = WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current());
        let registry = crate::sweep_registry::SweepRegistry::new(
            crate::sweep_registry::SweepRegistryConfig::new(sibling.clone()),
        );
        pool.seed(sibling.clone(), Arc::new(std::sync::Mutex::new(registry)));

        let queue = DurableQueue::open(dir.path().join("queue.jsonl"), 10);
        let processed = run_backfill_pass_all(&primary, &pool, &queue);

        assert_eq!(processed, 2, "one outcome from each of the two workspaces");
        assert_eq!(queue.len(), 4, "two completed + two outcome envelopes");
    }
}
