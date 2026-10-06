//! Fleet-wide, SigNoz-sourced in-sweep history (#9758).
//!
//! Pure: no daemon, no network, no clock. The "backend" is [`FileRows`] over
//! rows in exactly the shape `OUTCOMES_SQL` returns as `JSONEachRow` (64-bit
//! integers quoted, `loom.phase_durations` as the JSON-encoded string SigNoz
//! stores), or a scripted reader that fails on demand.

use super::fleet::{bytes, fleet_snapshot, input, write_snapshot};
use super::{as_of, subject};
use crate::eta::config::HistoryScopeMode;
use crate::eta::explanation::HistoryScope;
use crate::eta::fleet::{self, FORGE_HOST};
use crate::eta::fleet_signoz::{self, reject, SignozSnapshot, SIGNOZ_SNAPSHOT_SCHEMA};
use crate::eta::fleet_signoz_refresh::{
    fetch, refresh, ClickhouseHttp, FileRows, Limits, PageQuery, ReadError, SignozRead, SignozStop,
};
use crate::eta::heuristics::{FinishV1, LandV1};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{
    AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic, NoEstimateReason, Stage,
};
use crate::telemetry::{PhaseDuration, SweepOutcomeRecord, TelemetryEnvelope, TelemetryRecord};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use std::path::Path;

const REPO: &str = "rjwalters/loom";
const HOSTS: [&str; 3] = ["loom-worker-1", "loom-worker-2", "robb-studio"];

// -- fixtures -------------------------------------------------------------

fn ns(at: DateTime<Utc>) -> String {
    at.timestamp_nanos_opt().unwrap().to_string()
}

/// One row as the outcomes query returns it.
fn row(
    record_id: &str,
    host: &str,
    sweep_id: &str,
    result: &str,
    event: DateTime<Utc>,
    knowable: DateTime<Utc>,
    phases: &[(&str, i64)],
) -> String {
    let list: Vec<serde_json::Value> = phases
        .iter()
        .map(|(phase, d)| json!({"phase": phase, "duration_sec": d, "attempt": 1}))
        .collect();
    let total: i64 = phases.iter().map(|(_, d)| d).sum();
    json!({
        "record_id": record_id,
        "host_id": host,
        "repo": REPO,
        "sweep_id": sweep_id,
        "result": result,
        "total_duration_sec": total.to_string(),
        "phase_durations": serde_json::to_string(&list).unwrap(),
        "event_time_ns": ns(event),
        "knowable_time_ns": ns(knowable),
    })
    .to_string()
}

/// The full in-sweep lifecycle `finish-v1` walks, varied per sweep.
fn lifecycle(i: i64) -> Vec<(&'static str, i64)> {
    vec![
        ("curator", 300 + 10 * i),
        ("builder", 1800 + 60 * i),
        ("judge", 600 + 20 * i),
        ("doctor", 900 + 30 * i),
        ("merge", 120 + i),
    ]
}

/// Twelve successful sweeps spread over three hosts, a day apart, each
/// knowable one minute after its event time.
fn fleet_rows() -> Vec<String> {
    (0..12_i64)
        .map(|i| {
            let event = as_of() - Duration::days(30 - i);
            row(
                &format!("rec-{i:02}"),
                HOSTS[usize::try_from(i).unwrap() % HOSTS.len()],
                &format!("sweep-issue-{}-1", 9000 + i),
                "success",
                event,
                event + Duration::minutes(1),
                &lifecycle(i),
            )
        })
        .collect()
}

fn limits(page_size: u32) -> Limits {
    Limits {
        page_size,
        max_pages: 100,
    }
}

fn snapshot_of(rows: &[String], page_size: u32) -> SignozSnapshot {
    let mut reader = FileRows::parse(&rows.join("\n")).unwrap();
    fetch(REPO, &mut reader, as_of(), limits(page_size))
        .expect("a complete walk")
        .0
}

fn write_signoz(root: &Path, snapshot: &SignozSnapshot) -> std::path::PathBuf {
    let path = fleet_signoz::signoz_path(root, &snapshot.repo);
    fleet_signoz::write(&path, snapshot).unwrap();
    path
}

/// A `finish-v1` input: a sweep in its Curator phase, nothing elapsed.
fn finish_input() -> EstimateInput {
    EstimateInput {
        current: CurrentState::At(CurrentStage {
            stage: Stage::SweepCurator,
            entered_at: Some(as_of()),
            age_sec: 0,
            age_source: AgeSource::LabelEvent,
            rework_rounds: 0,
            episode_entered_at: None,
        }),
        ..input()
    }
}

/// A reader that answers from `rows` but fails (or returns garbage) on page
/// `fail_on`.
struct Scripted {
    inner: FileRows,
    page: u32,
    fail_on: u32,
    garbage: bool,
}

impl SignozRead for Scripted {
    fn page(&mut self, query: &PageQuery) -> Result<String, ReadError> {
        self.page += 1;
        if self.page == self.fail_on {
            if self.garbage {
                return Ok("<html>502 Bad Gateway</html>".to_string());
            }
            return Err(ReadError::Unavailable("connection reset".to_string()));
        }
        self.inner.page(query)
    }
}

// -- attribution and source admission -------------------------------------

#[test]
fn every_sample_keeps_its_recording_host_and_the_signoz_source() {
    let history = snapshot_of(&fleet_rows(), 5).stage_samples(&Default::default());
    assert_eq!(history.scope, HistoryScope::Fleet);
    assert!(history
        .stages
        .iter()
        .all(|s| s.source == SampleSource::SignozOutcome));
    let hosts: std::collections::BTreeSet<&str> =
        history.stages.iter().map(|s| s.host.as_str()).collect();
    assert_eq!(hosts.into_iter().collect::<Vec<_>>(), HOSTS.to_vec());
    assert!(history.stages.iter().all(|s| s.host != FORGE_HOST));
    // The in-sweep phases the forge cannot see are here…
    for stage in [Stage::SweepCurator, Stage::SweepBuilder] {
        assert_eq!(history.stages.iter().filter(|s| s.stage == stage).count(), 12);
    }
    // …and so is the merge share, from successful sweeps.
    assert_eq!(history.paths.len(), 12);
    assert!(history.paths.iter().all(|p| p.merged_in_sweep));
    // Verdicts are the forge snapshot's to supply, never counted twice.
    assert!(history.verdicts.is_empty());
}

#[test]
fn a_sweep_outcome_filter_admits_signoz_samples_and_a_stage_journal_filter_does_not() {
    use SampleSource::{ForgeTimeline, SignozOutcome, StageJournal, SweepOutcome};
    assert!(SweepOutcome.admits(SignozOutcome));
    assert!(SignozOutcome.admits(SignozOutcome));
    assert!(!StageJournal.admits(SignozOutcome));
    assert!(!ForgeTimeline.admits(SignozOutcome));
    assert!(!SignozOutcome.admits(SweepOutcome));
    assert_eq!(SignozOutcome.journal(), "signoz:sweep.outcome");

    let history = snapshot_of(&fleet_rows(), 50).stage_samples(&Default::default());
    let via_outcome = history.select(REPO, Stage::SweepBuilder, as_of(), &[SweepOutcome]);
    let selection = via_outcome.expect("a SweepOutcome filter reads the SigNoz half");
    assert_eq!(selection.by_source.keys().collect::<Vec<_>>(), vec!["signoz:sweep.outcome"]);
    assert_eq!(selection.by_host.len(), HOSTS.len());
    assert!(history
        .select(REPO, Stage::SweepBuilder, as_of(), &[StageJournal])
        .is_none());
}

// -- dedupe ------------------------------------------------------------------

#[test]
fn overlapping_pages_and_redeliveries_count_once_by_record_id() {
    let mut rows = fleet_rows();
    // A retried delivery is the same envelope: identical row, same record id.
    rows.push(rows[3].clone());
    rows.push(rows[7].clone());
    // One page sees both copies of each.
    let mut reader = FileRows::parse(&rows.join("\n")).unwrap();
    let (snapshot, report) = fetch(REPO, &mut reader, as_of(), limits(50)).unwrap();
    assert_eq!(report.stop, SignozStop::Complete);
    assert_eq!(report.rows, 14);
    assert_eq!(report.stats.duplicate_records, 2);
    assert_eq!(snapshot.outcomes.len(), 12);
    // Page sizes that put a copy on either side of a boundary (the keyset then
    // skips the identical copy) or both on one page: always the same twelve.
    for page_size in [1, 2, 3, 4, 5] {
        let mut reader = FileRows::parse(&rows.join("\n")).unwrap();
        let (paged, report) = fetch(REPO, &mut reader, as_of(), limits(page_size)).unwrap();
        assert_eq!(report.rows as usize - report.stats.duplicate_records, 12, "{page_size}");
        assert_eq!(paged, snapshot, "page size {page_size}");
    }

    // A repeated refresh of the same window is the same snapshot.
    assert_eq!(snapshot, snapshot_of(&fleet_rows(), 7));
}

#[test]
fn two_records_for_one_sweep_keep_the_first_to_become_knowable() {
    let event = as_of() - Duration::days(3);
    let first = row("aaa", HOSTS[0], "sweep-x", "success", event, event, &lifecycle(1));
    // Re-serialised by a newer build: another record id, later knowable-at.
    let later = row(
        "000",
        HOSTS[0],
        "sweep-x",
        "success",
        event,
        event + Duration::hours(2),
        &lifecycle(9),
    );
    // A different host's sweep with the same id is a different sweep.
    let other = row("bbb", HOSTS[1], "sweep-x", "success", event, event, &lifecycle(2));
    let (snapshot, report) = {
        let mut reader = FileRows::parse(&[later, first, other].join("\n")).unwrap();
        fetch(REPO, &mut reader, as_of(), limits(10)).unwrap()
    };
    assert_eq!(report.stats.duplicate_sweeps, 1);
    let kept: Vec<(&str, &str)> = snapshot
        .outcomes
        .iter()
        .map(|o| (o.host.as_str(), o.record_id.as_str()))
        .collect();
    assert_eq!(kept, vec![(HOSTS[0], "aaa"), (HOSTS[1], "bbb")]);
}

// -- malformed records -------------------------------------------------------

#[test]
fn malformed_records_are_rejected_by_reason_and_never_admitted() {
    let event = as_of() - Duration::days(2);
    let good = row("good", HOSTS[0], "s-good", "success", event, event, &lifecycle(0));
    let mutate = |id: &str, f: &dyn Fn(&mut serde_json::Value)| {
        let mut value: serde_json::Value = serde_json::from_str(&good).unwrap();
        value["record_id"] = json!(id);
        value["sweep_id"] = json!(format!("s-{id}"));
        f(&mut value);
        value.to_string()
    };
    let rows = vec![
        good.clone(),
        mutate("", &|_| {}),
        mutate("no-host", &|v| v["host_id"] = json!("")),
        mutate("no-sweep", &|v| v["sweep_id"] = json!(null)),
        mutate("other-repo", &|v| v["repo"] = json!("someone/else")),
        mutate("weird-result", &|v| v["result"] = json!("merged")),
        mutate("no-total", &|v| v["total_duration_sec"] = json!(null)),
        mutate("no-phases", &|v| v["phase_durations"] = json!(null)),
        mutate("bad-phases", &|v| {
            v["phase_durations"] = json!("[{\"phase\":\"builder\",\"duration_sec\":-5}]");
        }),
        mutate("garbled-phases", &|v| v["phase_durations"] = json!("not json")),
        mutate("no-event", &|v| v["event_time_ns"] = json!("0")),
        mutate("no-knowable", &|v| v["knowable_time_ns"] = json!("0")),
        mutate("knowable-early", &|v| {
            v["knowable_time_ns"] = json!(ns(event - Duration::seconds(1)));
        }),
    ];
    // The rows the query would return (`other-repo` would be filtered by the
    // query itself; feed it straight to the parser instead).
    let parse = |line: &str| fleet_signoz::parse_row(line, REPO).unwrap().1;
    let reasons: Vec<&str> = rows
        .iter()
        .skip(1)
        .map(|line| match parse(line) {
            fleet_signoz::ParsedRow::Rejected(reason) => reason,
            fleet_signoz::ParsedRow::Outcome(o) => panic!("admitted {}", o.record_id),
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            reject::MISSING_RECORD_ID,
            reject::MISSING_HOST,
            reject::MISSING_SWEEP_ID,
            reject::REPO_MISMATCH,
            reject::UNKNOWN_RESULT,
            reject::MISSING_TOTAL_DURATION,
            reject::PHASES_ABSENT,
            reject::PHASES_INVALID,
            reject::PHASES_INVALID,
            reject::MISSING_EVENT_TIME,
            reject::MISSING_KNOWABLE_TIME,
            reject::KNOWABLE_BEFORE_EVENT,
        ]
    );

    // Through a walk, a malformed record is counted and the walk completes
    // with only the admissible one.
    let in_repo: Vec<String> = rows
        .iter()
        .filter(|l| !l.contains("someone/else"))
        .cloned()
        .collect();
    let mut reader = FileRows::parse(&in_repo.join("\n")).unwrap();
    let (snapshot, report) = fetch(REPO, &mut reader, as_of(), limits(3)).unwrap();
    assert_eq!(snapshot.outcomes.len(), 1);
    assert_eq!(snapshot.outcomes[0].record_id, "good");
    assert_eq!(report.rejected.values().sum::<usize>(), 10);
    assert_eq!(report.rejected[reject::PHASES_INVALID], 2);
}

#[test]
fn a_row_that_is_not_an_object_is_an_invalid_response_not_a_rejection() {
    assert!(fleet_signoz::parse_row("[1,2]", REPO).is_err());
    assert!(fleet_signoz::parse_row("{\"record_id\":\"x\"}", REPO).is_err());
    assert!(fleet_signoz::parse_row("{\"knowable_time_ns\":\"1\"}", REPO).is_err());
}

// -- failure keeps the last valid snapshot ---------------------------------------

fn assert_kept(root: &Path, before: &[u8], stop: SignozStop, report_stop: SignozStop) {
    assert_eq!(report_stop, stop);
    let after = std::fs::read(fleet_signoz::signoz_path(root, REPO)).unwrap();
    assert_eq!(after, before, "a {stop:?} walk must leave the published file untouched");
}

#[test]
fn a_failed_page_keeps_the_last_valid_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let published = snapshot_of(&fleet_rows()[..6], 50);
    let before = std::fs::read(write_signoz(dir.path(), &published)).unwrap();

    let mut reader = Scripted {
        inner: FileRows::parse(&fleet_rows().join("\n")).unwrap(),
        page: 0,
        fail_on: 2,
        garbage: false,
    };
    let report = refresh(dir.path(), REPO, &mut reader, as_of(), limits(4));
    assert!(!report.promoted);
    assert_eq!(report.detail.as_deref(), Some("connection reset"));
    assert_kept(dir.path(), &before, SignozStop::Unavailable, report.stop);
}

#[test]
fn an_invalid_response_keeps_the_last_valid_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let before =
        std::fs::read(write_signoz(dir.path(), &snapshot_of(&fleet_rows()[..6], 50))).unwrap();
    let mut reader = Scripted {
        inner: FileRows::parse(&fleet_rows().join("\n")).unwrap(),
        page: 0,
        fail_on: 3,
        garbage: true,
    };
    let report = refresh(dir.path(), REPO, &mut reader, as_of(), limits(4));
    assert_kept(dir.path(), &before, SignozStop::InvalidResponse, report.stop);
}

#[test]
fn the_page_ceiling_never_publishes_a_partial_window() {
    let dir = tempfile::tempdir().unwrap();
    let mut reader = FileRows::parse(&fleet_rows().join("\n")).unwrap();
    let report = refresh(
        dir.path(),
        REPO,
        &mut reader,
        as_of(),
        Limits {
            page_size: 2,
            max_pages: 3,
        },
    );
    assert_eq!(report.stop, SignozStop::PageLimit);
    assert!(!report.promoted);
    assert!(!fleet_signoz::signoz_path(dir.path(), REPO).exists());
}

#[test]
fn a_page_that_does_not_advance_the_cursor_is_an_invalid_response() {
    struct Stuck(String);
    impl SignozRead for Stuck {
        fn page(&mut self, _: &PageQuery) -> Result<String, ReadError> {
            Ok(self.0.clone())
        }
    }
    let rows = fleet_rows();
    let mut reader = Stuck(rows[..2].join("\n"));
    let report = fetch(REPO, &mut reader, as_of(), limits(2)).unwrap_err();
    assert_eq!(report.stop, SignozStop::InvalidResponse);
}

#[test]
fn a_complete_walk_publishes_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let mut reader = FileRows::parse(&fleet_rows().join("\n")).unwrap();
    let report = refresh(dir.path(), REPO, &mut reader, as_of(), limits(5));
    assert_eq!(report.stop, SignozStop::Complete);
    assert!(report.promoted);
    let read = fleet_signoz::read(&fleet_signoz::signoz_path(dir.path(), REPO)).unwrap();
    assert_eq!(read.schema, SIGNOZ_SNAPSHOT_SCHEMA);
    assert_eq!(Some(read.snapshot_id.clone()), report.snapshot_id);
    assert_eq!(read, snapshot_of(&fleet_rows(), 5));
    // The forge loader never parses a SigNoz file as one of its own.
    assert!(fleet::load_all(dir.path()).is_empty());
    assert_eq!(fleet_signoz::load_all(dir.path()), vec![read]);
}

#[test]
fn an_unknown_schema_is_a_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let mut snapshot = snapshot_of(&fleet_rows(), 50);
    snapshot.schema = "eta-fleet-signoz-snapshot/v0".to_string();
    let path = write_signoz(dir.path(), &snapshot);
    assert!(fleet_signoz::read(&path).is_none());
}

// -- knowable-at: replay ----------------------------------------------------------

#[test]
fn samples_are_observed_at_knowable_at_and_a_replay_excludes_the_rest() {
    let event = as_of() - Duration::days(5);
    let knowable = event + Duration::hours(6);
    let rows: Vec<String> = (0..8_i64)
        .map(|i| {
            row(
                &format!("late-{i}"),
                HOSTS[0],
                &format!("s-{i}"),
                "success",
                event + Duration::seconds(i),
                knowable + Duration::seconds(i),
                &lifecycle(i),
            )
        })
        .collect();
    let history = snapshot_of(&rows, 50).stage_samples(&Default::default());
    assert!(history
        .stages
        .iter()
        .all(|s| s.observed_at >= knowable && s.observed_at < knowable + Duration::seconds(8)));
    let sources = [SampleSource::SweepOutcome];
    // After every event but before any row was knowable: nothing.
    let between = event + Duration::hours(1);
    assert!(history
        .select(REPO, Stage::SweepCurator, between, &sources)
        .is_none());
    assert!(history.merge_share(REPO, between).is_none());
    // Exactly at the first knowable instant: still nothing (strictly before).
    assert!(history
        .select(REPO, Stage::SweepCurator, knowable, &sources)
        .is_none());
    // Once all are knowable, all are read.
    let after = knowable + Duration::minutes(1);
    let selection = history
        .select(REPO, Stage::SweepCurator, after, &sources)
        .unwrap();
    assert_eq!(selection.sorted.len(), 8);
}

#[test]
fn a_snapshot_holds_nothing_knowable_after_its_own_as_of() {
    let future = as_of() + Duration::hours(1);
    let mut rows = fleet_rows();
    rows.push(row("future", HOSTS[0], "s-f", "success", future, future, &lifecycle(0)));
    // The query bounds `observed_timestamp <= until`; the builder enforces it
    // again for a reader that does not.
    let snapshot = snapshot_of(&rows, 50);
    assert!(snapshot
        .outcomes
        .iter()
        .all(|o| o.knowable_at <= snapshot.as_of));
    let outcome = fleet_signoz::parse_row(&rows[12], REPO).unwrap().1;
    let fleet_signoz::ParsedRow::Outcome(outcome) = outcome else {
        panic!("well-formed")
    };
    let (built, stats) =
        SignozSnapshot::build(REPO, SignozSnapshot::window_start(as_of()), as_of(), vec![outcome]);
    assert!(built.outcomes.is_empty());
    assert_eq!(stats.outside_window, 1);
}

// -- determinism ------------------------------------------------------------------

#[test]
fn shuffled_input_yields_the_same_snapshot_id_order_and_bytes() {
    let rows = fleet_rows();
    let mut shuffled = rows.clone();
    shuffled.reverse();
    shuffled.swap(1, 7);
    let a = snapshot_of(&rows, 3);
    let b = snapshot_of(&shuffled, 11);
    assert_eq!(a.snapshot_id, b.snapshot_id);
    assert_eq!(a, b);
    assert_eq!(serde_json::to_vec_pretty(&a).unwrap(), serde_json::to_vec_pretty(&b).unwrap());
    // Content-derived: a different window is a different id.
    let mut reader = FileRows::parse(&rows.join("\n")).unwrap();
    let later = fetch(REPO, &mut reader, as_of() + Duration::seconds(1), limits(50))
        .unwrap()
        .0;
    assert_ne!(later.snapshot_id, a.snapshot_id);
}

// -- the headline: fleet scope answers finish-v1 -----------------------------------

#[test]
fn finish_v1_answers_at_fleet_scope_on_a_host_with_zero_local_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(dir.path(), &fleet_snapshot());

    // Before #9758: the forge half alone cannot see inside a sweep.
    let forge_only =
        fleet::apply_scope(HistoryScopeMode::Fleet, dir.path(), StageSamples::default());
    let refused = FinishV1.estimate(&finish_input(), &forge_only);
    assert_eq!(refused.no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));

    // With the SigNoz half cached, the same empty local history answers.
    write_signoz(dir.path(), &snapshot_of(&fleet_rows(), 50));
    let local = StageSamples::default();
    assert!(local.stages.is_empty() && local.outcome_keys.is_empty());
    let history = fleet::apply_scope(HistoryScopeMode::Fleet, dir.path(), local);
    let answered = FinishV1.estimate(&finish_input(), &history);
    assert_eq!(answered.no_estimate_reason, None, "{answered:?}");
    let record = answered.history.as_ref().unwrap();
    assert_eq!(record.scope, HistoryScope::Fleet);
    assert!(record
        .samples_by_source
        .contains_key("signoz:sweep.outcome"));
    for host in HOSTS {
        assert!(record.samples_by_host.contains_key(host), "{record:?}");
    }
}

#[test]
fn two_hosts_at_the_same_snapshots_produce_byte_identical_output_with_the_signoz_half() {
    let host_a = tempfile::tempdir().unwrap();
    let host_b = tempfile::tempdir().unwrap();
    write_snapshot(host_a.path(), &fleet_snapshot());
    write_snapshot(host_b.path(), &fleet_snapshot());
    // Host A reads the window in small pages, host B in one, from a shuffled
    // export: the same snapshot either way.
    let path_a = write_signoz(host_a.path(), &snapshot_of(&fleet_rows(), 2));
    let mut shuffled = fleet_rows();
    shuffled.reverse();
    let path_b = write_signoz(host_b.path(), &snapshot_of(&shuffled, 500));
    assert_eq!(std::fs::read(path_a).unwrap(), std::fs::read(path_b).unwrap());

    // Host B also has its own local outcomes; at `fleet` scope they must not
    // move the answer by a byte.
    let mut local_b = StageSamples::default();
    local_b.push_envelopes(&[local_envelope("host-b", "sweep-local-1", 4000)]);
    let history_a = fleet::apply_scope(HistoryScopeMode::Fleet, host_a.path(), Default::default());
    let history_b = fleet::apply_scope(HistoryScopeMode::Fleet, host_b.path(), local_b);
    for (a, b) in [
        (
            FinishV1.estimate(&finish_input(), &history_a),
            FinishV1.estimate(&finish_input(), &history_b),
        ),
        (LandV1.estimate(&input(), &history_a), LandV1.estimate(&input(), &history_b)),
    ] {
        assert_eq!(a.history, b.history, "identical HistoryRecord");
        assert_eq!(bytes(&a), bytes(&b), "identical estimate output");
        assert!(a.result.is_some());
    }
}

// -- augment ----------------------------------------------------------------------

/// A local `sweep.outcome` envelope with this module's lifecycle.
fn local_envelope(host: &str, sweep_id: &str, builder_sec: i64) -> TelemetryEnvelope {
    let mut envelope = super::history_a_envelopes()
        .into_iter()
        .find(|e| matches!(e.record, TelemetryRecord::SweepOutcome(_)))
        .expect("history-a has a sweep.outcome");
    let TelemetryRecord::SweepOutcome(record) = &mut envelope.record else {
        unreachable!()
    };
    let record: &mut SweepOutcomeRecord = record;
    record.repo = Some(REPO.to_string());
    record.sweep_id = sweep_id.to_string();
    record.total_duration_sec = 400 + builder_sec + 60;
    record.judge_verdicts = None;
    record.phase_durations = vec![
        PhaseDuration::new("curator", 400),
        PhaseDuration::new("builder", builder_sec),
        PhaseDuration::new("merge", 60),
    ];
    envelope.host_id = host.to_string();
    envelope.emitted_at = as_of() - Duration::days(2);
    envelope
}

#[test]
fn a_local_outcome_also_exported_to_signoz_contributes_once() {
    let dir = tempfile::tempdir().unwrap();
    // The fleet exported HOSTS[0]'s sweep 9000; this host *is* HOSTS[0] and
    // journalled the same sweep itself.
    write_signoz(dir.path(), &snapshot_of(&fleet_rows(), 50));
    let mut local = StageSamples::default();
    local.push_envelopes(&[local_envelope(HOSTS[0], "sweep-issue-9000-1", 7777)]);
    assert!(local
        .outcome_keys
        .contains(&(HOSTS[0].to_string(), "sweep-issue-9000-1".to_string())));

    let merged = fleet::apply_scope(HistoryScopeMode::Augment, dir.path(), local);
    let builders: Vec<(&str, SampleSource, i64)> = merged
        .stages
        .iter()
        .filter(|s| s.stage == Stage::SweepBuilder)
        .map(|s| (s.host.as_str(), s.source, s.duration_sec))
        .collect();
    // Twelve sweeps, not thirteen: the shared one is read from the journal.
    assert_eq!(builders.len(), 12, "{builders:?}");
    assert!(builders.contains(&(HOSTS[0], SampleSource::SweepOutcome, 7777)));
    assert!(!builders
        .iter()
        .any(|(_, source, d)| *source == SampleSource::SignozOutcome && *d == 1800));
    assert_eq!(merged.paths.len(), 12);

    // At `fleet` scope nothing is excluded: the SigNoz copy is the evidence.
    let fleet_only =
        fleet::apply_scope(HistoryScopeMode::Fleet, dir.path(), StageSamples::default());
    assert_eq!(
        fleet_only
            .stages
            .iter()
            .filter(|s| s.stage == Stage::SweepBuilder)
            .count(),
        12
    );
}

#[test]
fn one_history_record_distinguishes_forge_signoz_and_local_samples() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(dir.path(), &fleet_snapshot());
    write_signoz(dir.path(), &snapshot_of(&fleet_rows(), 50));
    let mut local = StageSamples::default();
    let envelopes: Vec<TelemetryEnvelope> = (0..3)
        .map(|i| local_envelope("this-host", &format!("sweep-local-{i}"), 2000 + i))
        .collect();
    local.push_envelopes(&envelopes);
    let merged = fleet::apply_scope(HistoryScopeMode::Augment, dir.path(), local);
    // `land-v1` from merge_wait reads one stage that all three producers
    // carry: the forge's label timeline, the fleet's and this host's sweeps.
    let at_merge = EstimateInput {
        current: CurrentState::At(CurrentStage {
            stage: Stage::MergeWait,
            entered_at: Some(as_of()),
            age_sec: 0,
            age_source: AgeSource::LabelEvent,
            rework_rounds: 0,
            episode_entered_at: None,
        }),
        subject: subject(),
        ..input()
    };
    let explanation = LandV1.estimate(&at_merge, &merged);
    let record = explanation.history.as_ref().expect("a history record");
    assert_eq!(
        record.samples_by_source.keys().collect::<Vec<_>>(),
        vec![
            "forge:pr-timeline",
            "signoz:sweep.outcome",
            "sweep-outcome-telemetry.jsonl"
        ],
        "{record:?}"
    );
    assert_eq!(record.samples_by_source["sweep-outcome-telemetry.jsonl"], 3);
    assert_eq!(record.samples_by_source["signoz:sweep.outcome"], 12);
    let hosts: Vec<&String> = record.samples_by_host.keys().collect();
    assert!(hosts.contains(&&FORGE_HOST.to_string()));
    assert!(hosts.contains(&&"this-host".to_string()));
    for host in HOSTS {
        assert!(hosts.contains(&&host.to_string()), "{hosts:?}");
    }
}

// -- the HTTP transport, without a network -----------------------------------------

#[test]
fn the_clickhouse_reader_binds_every_query_parameter_and_refuses_non_http() {
    let reader = ClickhouseHttp {
        endpoint: "https://telemetry.example:8443".to_string(),
        user: Some("reader".to_string()),
        credential_file: None,
        timeout: std::time::Duration::from_secs(1),
    };
    let query = PageQuery {
        repo: REPO.to_string(),
        since: as_of() - Duration::days(1),
        until: as_of(),
        after: Some((42, "rec-01".to_string())),
        limit: 7,
    };
    let url = reader.url(&query).unwrap();
    let pairs: std::collections::BTreeMap<String, String> =
        url.query_pairs().into_owned().collect();
    assert_eq!(pairs["param_repo"], REPO);
    assert_eq!(pairs["param_until_ns"], ns(as_of()));
    assert_eq!(pairs["param_since_ns"], ns(as_of() - Duration::days(1)));
    assert_eq!(pairs["param_after_ns"], "42");
    assert_eq!(pairs["param_after_id"], "rec-01");
    assert_eq!(pairs["param_limit"], "7");
    for name in pairs.keys() {
        let bare = name.trim_start_matches("param_");
        assert!(
            crate::eta::fleet_signoz_refresh::OUTCOMES_SQL.contains(&format!("{{{bare}:")),
            "{bare} is bound but not used"
        );
    }
    let file = ClickhouseHttp {
        endpoint: "file:///etc/passwd".to_string(),
        ..reader
    };
    assert!(file.url(&query).is_err());
}

#[cfg(unix)]
#[test]
fn a_credential_file_readable_by_others_is_refused_and_never_echoed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("signoz-read.key");
    std::fs::write(&path, "s3cret-value\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut reader = ClickhouseHttp {
        // Unroutable on purpose: the credential check must fail first.
        endpoint: "http://127.0.0.1:9".to_string(),
        user: Some("reader".to_string()),
        credential_file: Some(path),
        timeout: std::time::Duration::from_millis(200),
    };
    let query = PageQuery {
        repo: REPO.to_string(),
        since: as_of() - Duration::days(1),
        until: as_of(),
        after: None,
        limit: 1,
    };
    let Err(ReadError::Unavailable(why)) = reader.page(&query) else {
        panic!("a group/world-readable credential must be refused");
    };
    assert!(why.contains("readable by group or others"), "{why}");
    assert!(!why.contains("s3cret"), "{why}");
}
