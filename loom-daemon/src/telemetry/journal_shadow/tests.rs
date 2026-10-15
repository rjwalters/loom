//! Unit tests for the J2 telemetry shadow streams (#11348). Every workspace
//! and journal root is a fresh `TempDir`; enablement is forced per thread
//! ([`force`]), never through the process environment, so parallel tests in
//! this crate cannot see each other's configuration.

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

use super::compare::{compare_stream, fingerprint, normalize};
use super::verify::{verify_concern, ConcernOptions, REPORT_STREAM, VERIFY_CURSOR};
use super::*;
use crate::forge_merge_queue::events::{EventKind, EventSink, FileEventSink, QueueEvent};
use crate::journal::{Journal, JournalOptions, Position, ReadRecord};

thread_local! {
    static FORCED: RefCell<Option<Option<PathBuf>>> = const { RefCell::new(None) };
}

/// The test seam [`ShadowConfig::resolve`] consults first.
pub(crate) fn forced_root() -> Option<Option<PathBuf>> {
    FORCED.with(|f| f.borrow().clone())
}

/// Resets the forced configuration on drop.
pub(crate) struct Forced;

impl Drop for Forced {
    fn drop(&mut self) {
        FORCED.with(|f| *f.borrow_mut() = None);
    }
}

/// Force shadowing on at `root` (`Some`) or off (`None`) for this thread.
pub(crate) fn force(root: Option<&Path>) -> Forced {
    FORCED.with(|f| *f.borrow_mut() = Some(root.map(Path::to_path_buf)));
    Forced
}

pub(crate) fn records(root: &Path, stream: TelemetryStream) -> Vec<ReadRecord> {
    Journal::open(root)
        .stream(stream.name())
        .unwrap()
        .reader(Position::START)
        .map(Result::unwrap)
        .collect()
}

fn legacy_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn append_line(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{value}").unwrap();
}

fn shell_envelope(pr: u64) -> Value {
    json!({
        "schema_version": 1,
        "emitted_at": "2026-10-11T03:00:00Z",
        "emitted_at_epoch": 1_791_687_600,
        "host_id": "host-test",
        "record": {"kind": "guide.docs_maintenance", "repo": "o/r", "pr_number": pr,
                   "duration_sec": null, "files_changed": ["README.md"]}
    })
}

/// Legacy write + shadow, the way a shell producer does it.
fn shell_produce(ws: &Path, value: &Value) {
    let legacy = ws.join(".loom/logs/guide-docs-telemetry.jsonl");
    after_legacy(
        ws,
        TelemetryStream::Shell,
        || -> std::io::Result<()> {
            append_line(&legacy, value);
            Ok(())
        },
        |()| Some(ShadowRecord::envelope(value.clone())),
    )
    .unwrap();
}

// 1. Enablement --------------------------------------------------------------

#[test]
fn enablement_is_off_unless_truthy() {
    let ws = Path::new("/ws");
    assert_eq!(ShadowConfig::from_values(None, None, ws), None);
    for off in ["", "0", "false", "off", "no", "nonsense"] {
        assert_eq!(ShadowConfig::from_values(Some(off), None, ws), None, "{off}");
    }
    for on in ["1", "true", "YES", " on "] {
        assert_eq!(
            ShadowConfig::from_values(Some(on), None, ws),
            Some(ShadowConfig::at("/ws/.loom/state/journal/telemetry")),
            "{on}"
        );
    }
    assert_eq!(
        ShadowConfig::from_values(Some("1"), Some("/var/j"), ws),
        Some(ShadowConfig::at("/var/j"))
    );
    // A relative override disables rather than writing somewhere surprising.
    assert_eq!(ShadowConfig::from_values(Some("1"), Some("rel/j"), ws), None);
}

#[test]
fn divergence_reports_live_beside_not_inside_the_telemetry_root() {
    let config = ShadowConfig::at("/ws/.loom/state/journal/telemetry");
    assert_eq!(
        config.report_root(),
        PathBuf::from("/ws/.loom/state/journal/telemetry-divergence")
    );
    assert!(TelemetryStream::ALL
        .iter()
        .all(|s| TelemetryStream::from_name(s.name()) == Some(*s)));
    assert_eq!(TelemetryStream::from_name("ci"), None, "T7 is out of scope");
}

// 2. Ordering: legacy first, shadow never fatal ------------------------------

#[test]
fn a_failed_legacy_write_appends_nothing() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("j");
    let _f = force(Some(&root));
    let result: Result<(), &str> = after_legacy(
        tmp.path(),
        TelemetryStream::Sweep,
        || Err("legacy disk full"),
        |()| Some(ShadowRecord::envelope(json!({"record": {"kind": "x"}}))),
    );
    assert_eq!(result, Err("legacy disk full"));
    assert!(!root.exists(), "no journal record after a legacy failure");
}

#[test]
fn a_failed_shadow_append_never_fails_the_legacy_write() {
    let tmp = TempDir::new().unwrap();
    // The journal root is a regular file: every append fails with I/O.
    let root = tmp.path().join("not-a-dir");
    std::fs::write(&root, b"x").unwrap();
    let _f = force(Some(&root));
    let result: Result<u32, String> = after_legacy(
        tmp.path(),
        TelemetryStream::Rework,
        || Ok(7),
        |_| Some(ShadowRecord::envelope(json!({}))),
    );
    assert_eq!(result, Ok(7));
    assert!(matches!(
        shadow_append(tmp.path(), TelemetryStream::Rework, || Some(ShadowRecord::envelope(json!(
            {}
        )))),
        ShadowOutcome::Failed(_)
    ));
}

#[test]
fn disabled_shadowing_creates_nothing_and_builds_nothing() {
    let tmp = TempDir::new().unwrap();
    let _f = force(None);
    let mut built = false;
    let outcome = shadow_append(tmp.path(), TelemetryStream::Sweep, || {
        built = true;
        None
    });
    assert_eq!(outcome, ShadowOutcome::Disabled);
    assert!(!built, "the record is not even serialized while disabled");
    assert!(!tmp.path().join(".loom").exists());
}

// 3. Producers: default-off, enabled, legacy failure -------------------------

fn rework_marker() -> crate::rework_events::Marker<'static> {
    crate::rework_events::Marker {
        issue: 11348,
        kind: crate::rework_events::KINDS[0],
        reason: Some("base moved"),
        classification: None,
        duration_sec: Some(3),
    }
}

#[test]
fn rework_producer_is_default_off() {
    let ws = TempDir::new().unwrap();
    let _f = force(None);
    crate::rework_events::append(ws.path(), &rework_marker()).unwrap();
    assert!(!ws.path().join(".loom/state/journal").exists());
}

#[test]
fn rework_producer_shadows_exactly_the_legacy_line() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    let legacy = crate::rework_events::append(ws.path(), &rework_marker()).unwrap();
    let shadow = records(&root, TelemetryStream::Rework);
    assert_eq!(shadow.len(), 1);
    assert_eq!(shadow[0].envelope.data, legacy_lines(&legacy)[0]);
    assert!(shadow[0].envelope.kind.starts_with("rework."));
    let (cmp, _) = compare_stream(&root, ws.path(), TelemetryStream::Rework);
    assert!(cmp.is_clean() && cmp.matched == 1, "{cmp:?}");
}

#[test]
fn rework_producer_legacy_failures_append_nothing() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    // A vocabulary error is refused before the legacy write.
    let mut bad = rework_marker();
    bad.kind = "not-a-kind";
    assert!(crate::rework_events::append(ws.path(), &bad).is_err());
    // An I/O failure: `.loom/logs` is a file.
    std::fs::create_dir_all(ws.path().join(".loom")).unwrap();
    std::fs::write(ws.path().join(".loom/logs"), b"").unwrap();
    assert!(crate::rework_events::append(ws.path(), &rework_marker()).is_err());
    assert!(!root.join("rework").exists());
}

fn queue_event(pr: u32) -> QueueEvent {
    QueueEvent {
        kind: EventKind::Merged,
        pr,
        nonce: 2,
        head: Some("abc123".into()),
        reason: None,
        raw_reason: None,
        enqueued_at: Some("2026-10-11T03:00:00Z".into()),
        at: "2026-10-11T03:05:00Z".into(),
        enqueue_to_event_secs: Some(300),
        merged_after_revocation: false,
    }
}

#[test]
fn merge_queue_producer_shadows_once_and_keeps_legacy_dedupe() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    let sink = FileEventSink::for_root(ws.path());
    assert!(sink.record(&queue_event(9)).unwrap());
    assert!(!sink.record(&queue_event(9)).unwrap(), "legacy dedupe");
    let shadow = records(&root, TelemetryStream::MergeQueue);
    assert_eq!(shadow.len(), 1, "a legacy duplicate is not shadowed");
    assert_eq!(shadow[0].envelope.kind, "merge_queue.merged");
    assert_eq!(shadow[0].key(), Some("merged-9-2"));
    let (cmp, _) = compare_stream(&root, ws.path(), TelemetryStream::MergeQueue);
    assert!(cmp.is_clean() && cmp.matched == 1, "{cmp:?}");
}

#[test]
fn merge_queue_producer_is_default_off_and_legacy_failure_appends_nothing() {
    let ws = TempDir::new().unwrap();
    {
        let _f = force(None);
        FileEventSink::for_root(ws.path())
            .record(&queue_event(1))
            .unwrap();
        assert!(!ws.path().join(".loom/state/journal").exists());
    }
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    // The legacy log is a directory: the append fails.
    std::fs::remove_file(ws.path().join(".loom/logs/merge-queue-events.jsonl")).unwrap();
    std::fs::create_dir(ws.path().join(".loom/logs/merge-queue-events.jsonl")).unwrap();
    assert!(FileEventSink::for_root(ws.path())
        .record(&queue_event(2))
        .is_err());
    assert!(!root.join("merge_queue").exists());
}

#[test]
fn experiment_producer_shadows_into_the_owning_workspace() {
    let ws = TempDir::new().unwrap();
    let stats = ws
        .path()
        .join(crate::script_helpers::sweep_experiment::DEFAULT_STATS_FILE);
    let record = json!({"ts": "2026-10-11T03:00:00Z", "issue": 1, "phase": "builder", "arm": null});
    {
        let _f = force(None);
        crate::script_helpers::sweep_experiment::append_record(&record, stats.to_str()).unwrap();
        assert!(!ws.path().join(".loom/state/journal").exists());
    }
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    let record = json!({"ts": "2026-10-11T03:01:00Z", "issue": 2, "phase": "builder", "arm": null});
    crate::script_helpers::sweep_experiment::append_record(&record, stats.to_str()).unwrap();
    let shadow = records(&root, TelemetryStream::Experiment);
    assert_eq!(shadow.len(), 1);
    assert_eq!(shadow[0].envelope.kind, "experiment.builder");
    assert_eq!(workspace_of_loom_file(&stats), ws.path());
    // Legacy holds 2 (one from before enablement): explained, not divergent.
    let (cmp, _) = compare_stream(&root, ws.path(), TelemetryStream::Experiment);
    assert_eq!(cmp.legacy_only_predating_shadow, 1, "{cmp:?}");
    assert!(cmp.is_clean(), "{cmp:?}");
}

#[test]
fn experiment_producer_legacy_failure_appends_nothing() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    let stats = ws.path().join(".loom/stats/sweep-model-stats.jsonl");
    std::fs::create_dir_all(&stats).unwrap(); // a directory: open fails
    let record = json!({"phase": "builder"});
    assert!(
        crate::script_helpers::sweep_experiment::append_record(&record, stats.to_str()).is_err()
    );
    assert!(!root.join("experiment").exists());
}

fn role_tick(root: &Path) -> crate::role_tick_telemetry::RoleTickTelemetry {
    crate::role_tick_telemetry::RoleTickTelemetry {
        root: root.to_path_buf(),
        role: "judge".into(),
        started_at: chrono::Utc::now(),
        ended_at: chrono::Utc::now(),
        result: crate::telemetry::RoleTickResult::SkippedNoTokenPool,
        model: None,
        effort: None,
        detail: Some("no pool".into()),
        gated_pool: None,
        failure_class: None,
        preference_tier: None,
        preference_tap: None,
        role_draw: None,
        step_sweep_id: None,
    }
}

#[test]
fn role_tick_producer_shadows_after_legacy_and_is_default_off() {
    let ws = TempDir::new().unwrap();
    {
        let _f = force(None);
        crate::role_tick_telemetry::emit(&role_tick(ws.path()));
        assert!(!ws.path().join(".loom/state/journal").exists());
    }
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    crate::role_tick_telemetry::emit(&role_tick(ws.path()));
    let shadow = records(&root, TelemetryStream::RoleTick);
    assert_eq!(shadow.len(), 1);
    assert_eq!(shadow[0].envelope.kind, "role_tick.outcome");
    let legacy = legacy_lines(&crate::sweep_outcomes::default_role_tick_telemetry_path(ws.path()));
    assert_eq!(legacy.len(), 2, "legacy keeps every record");
    assert_eq!(shadow[0].envelope.data, legacy[1]);
}

#[test]
fn role_tick_producer_legacy_failure_appends_nothing() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    std::fs::create_dir_all(ws.path().join(".loom")).unwrap();
    std::fs::write(ws.path().join(".loom/logs"), b"").unwrap();
    crate::role_tick_telemetry::emit(&role_tick(ws.path()));
    assert!(!root.join("role_tick").exists());
}

// 4. Normalized comparison ---------------------------------------------------

#[test]
fn normalization_ignores_key_order_nulls_and_subsecond_time() {
    let a = json!({"b": 1, "a": {"y": null, "x": "2026-10-11T03:00:00.987654Z"}});
    let b = json!({"a": {"x": "2026-10-11T03:00:00+00:00"}, "b": 1});
    assert_eq!(normalize(&a), normalize(&b));
    assert_eq!(fingerprint(&a), fingerprint(&b));
    assert_ne!(fingerprint(&a), fingerprint(&json!({"b": 2})));
}

#[test]
fn a_record_missing_from_the_journal_after_shadowing_began_is_divergence() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let legacy = ws.path().join(".loom/logs/guide-docs-telemetry.jsonl");
    append_line(&legacy, &shell_envelope(1)); // before enablement
    let _f = force(Some(&root));
    shell_produce(ws.path(), &shell_envelope(2));
    append_line(&legacy, &shell_envelope(3)); // legacy-only, after shadowing began
    let (cmp, _) = compare_stream(&root, ws.path(), TelemetryStream::Shell);
    assert_eq!(cmp.matched, 1);
    assert_eq!(cmp.legacy_only_predating_shadow, 1);
    assert_eq!(cmp.legacy_only_unexplained, 1);
    assert!(!cmp.is_clean());
    assert_eq!(cmp.samples.len(), 1);
    assert_eq!(cmp.samples[0].side, "legacy_only");
    assert_eq!(cmp.samples[0].kind.as_deref(), Some("guide.docs_maintenance"));
}

#[test]
fn journal_only_records_are_explained_only_before_the_legacy_retention_window() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let config = ShadowConfig::at(&root);
    // Journal 1, 2, 3; legacy was pruned down to 2 and has a stray journal-only after.
    for pr in 1..=3 {
        append_with(&config, TelemetryStream::Shell, ShadowRecord::envelope(shell_envelope(pr)))
            .unwrap();
    }
    append_line(&ws.path().join(".loom/logs/guide-docs-telemetry.jsonl"), &shell_envelope(2));
    let (cmp, _) = compare_stream(&root, ws.path(), TelemetryStream::Shell);
    assert_eq!(cmp.matched, 1);
    assert_eq!(cmp.journal_only_predating_legacy, 1, "pr 1: legacy rotated it away");
    assert_eq!(cmp.journal_only_unexplained, 1, "pr 3: legacy never got it");
}

#[test]
fn two_disjoint_non_empty_sides_are_never_green() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    append_with(
        &ShadowConfig::at(&root),
        TelemetryStream::Shell,
        ShadowRecord::envelope(shell_envelope(1)),
    )
    .unwrap();
    append_line(&ws.path().join(".loom/logs/guide-docs-telemetry.jsonl"), &shell_envelope(2));
    let (cmp, _) = compare_stream(&root, ws.path(), TelemetryStream::Shell);
    assert_eq!(cmp.matched, 0);
    assert_eq!(cmp.legacy_only_unexplained, 1);
    assert_eq!(cmp.journal_only_unexplained, 1);
}

#[test]
fn rotated_legacy_backups_are_compared_too() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    shell_produce(ws.path(), &shell_envelope(1));
    shell_produce(ws.path(), &shell_envelope(2));
    let legacy = ws.path().join(".loom/logs/guide-docs-telemetry.jsonl");
    std::fs::rename(&legacy, legacy.with_file_name("guide-docs-telemetry.jsonl.1")).unwrap();
    shell_produce(ws.path(), &shell_envelope(3));
    let (cmp, _) = compare_stream(&root, ws.path(), TelemetryStream::Shell);
    assert_eq!(cmp.matched, 3, "{cmp:?}");
    assert!(cmp.is_clean());
}

#[test]
fn reports_never_carry_record_content() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    shell_produce(ws.path(), &shell_envelope(1));
    let mut leaked = shell_envelope(2);
    leaked["record"]["repo"] = json!("ghp_SECRETSECRETSECRET");
    append_line(&ws.path().join(".loom/logs/guide-docs-telemetry.jsonl"), &leaked);
    let report = verify_concern(
        &ShadowConfig::at(&root),
        ws.path(),
        &ConcernOptions {
            write_report: true,
            ..ConcernOptions::default()
        },
    );
    assert!(!report.ok);
    let text = serde_json::to_string(&report).unwrap();
    assert!(!text.contains("SECRET"), "{text}");
    let durable = std::fs::read_dir(ShadowConfig::at(&root).report_root().join(REPORT_STREAM))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap_or_default())
        .collect::<String>();
    assert!(durable.contains("journal.divergence"));
    assert!(!durable.contains("SECRET"));
}

// 5. Concern verify ----------------------------------------------------------

#[test]
fn verify_with_no_streams_is_nothing_to_verify_not_green() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let report = verify_concern(&ShadowConfig::at(&root), ws.path(), &ConcernOptions::default());
    assert!(!report.ok);
    assert!(report.nothing_to_verify);
    assert_eq!(report.absent_streams.len(), 6);
}

#[test]
fn verify_clean_records_a_durable_report_outside_the_streams() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    crate::rework_events::append(ws.path(), &rework_marker()).unwrap();
    shell_produce(ws.path(), &shell_envelope(1));
    let config = ShadowConfig::at(&root);
    let options = ConcernOptions {
        write_report: true,
        ..ConcernOptions::default()
    };
    let report = verify_concern(&config, ws.path(), &options);
    assert!(report.ok, "{report:#?}");
    assert_eq!(report.report_seq, Some(1));
    assert_eq!(report.comparisons.len(), 2);
    assert!(report.absent_streams.contains(&"sweep".to_owned()));
    // The report is its own journal, not a telemetry stream.
    assert!(!root.join(REPORT_STREAM).exists());
    let second = verify_concern(&config, ws.path(), &options);
    assert_eq!(second.report_seq, Some(2));
    let kinds: Vec<String> = Journal::open(config.report_root())
        .stream(REPORT_STREAM)
        .unwrap()
        .reader(Position::START)
        .map(|r| r.unwrap().envelope.kind)
        .collect();
    assert_eq!(kinds, ["telemetry.shadow_clean", "telemetry.shadow_clean"]);
}

#[test]
fn corrupt_and_unknown_major_journal_lines_fail_verify_but_a_torn_tail_does_not() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    shell_produce(ws.path(), &shell_envelope(1));
    let segment = root.join("shell/0000000001.jsonl");
    let options = ConcernOptions::default();

    // Torn tail (a crash mid-append): reported, not failing; the next
    // writer (a restart) repairs it and continues the sequence.
    let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
    file.write_all(b"{\"v\":1,\"stream\":\"sh").unwrap();
    drop(file);
    let report = verify_concern(&ShadowConfig::at(&root), ws.path(), &options);
    assert!(report.ok, "{report:#?}");
    shell_produce(ws.path(), &shell_envelope(2));
    let seqs: Vec<u64> = records(&root, TelemetryStream::Shell)
        .iter()
        .map(|r| r.envelope.seq)
        .collect();
    assert_eq!(seqs, [1, 2]);

    // Schema skew: a newer envelope major is counted, never read as data.
    let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
    file.write_all(b"{\"v\":2,\"seq\":3,\"future\":true}\n")
        .unwrap();
    drop(file);
    let report = verify_concern(&ShadowConfig::at(&root), ws.path(), &options);
    assert!(!report.ok);
    let structural = report.structural.unwrap();
    assert_eq!(structural.streams[0].unknown_major, 1);
    assert!(report.comparisons[0].is_clean(), "comparison skips it");

    // A corrupt complete line.
    let mut file = OpenOptions::new().append(true).open(&segment).unwrap();
    file.write_all(b"not json\n").unwrap();
    drop(file);
    let report = verify_concern(&ShadowConfig::at(&root), ws.path(), &options);
    assert_eq!(report.structural.unwrap().streams[0].corrupt_lines, 1);
}

#[test]
fn rotation_keeps_segments_an_unacknowledged_consumer_still_needs() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let config = ShadowConfig {
        root: root.clone(),
        options: JournalOptions {
            max_segment_bytes: 600,
            lock_retry: Duration::from_millis(200),
            ..JournalOptions::default()
        },
    };
    let legacy = ws.path().join(".loom/logs/guide-docs-telemetry.jsonl");
    for pr in 1..=8 {
        append_line(&legacy, &shell_envelope(pr));
        append_with(&config, TelemetryStream::Shell, ShadowRecord::envelope(shell_envelope(pr)))
            .unwrap();
    }
    let segments = || {
        std::fs::read_dir(root.join("shell"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "jsonl")
            })
            .count()
    };
    let before = segments();
    assert!(before > 2, "small segments rolled: {before}");
    // An exporter that has acknowledged nothing yet.
    let stream = config.journal().stream("shell").unwrap();
    stream
        .cursor("exporter")
        .unwrap()
        .commit(Position::START)
        .unwrap();
    let options = ConcernOptions {
        rotate: true,
        ..ConcernOptions::default()
    };
    let report = verify_concern(&config, ws.path(), &options);
    assert!(report.ok, "{report:#?}");
    assert!(report.rotations[0].deleted_segments.is_empty());
    assert_eq!(segments(), before, "nothing deleted behind an unacknowledged cursor");
    assert!(stream
        .cursor(VERIFY_CURSOR)
        .unwrap()
        .load()
        .unwrap()
        .is_some());
    // Once the exporter acknowledges everything, retention is bounded.
    let end = records(&root, TelemetryStream::Shell).last().unwrap().next;
    stream.cursor("exporter").unwrap().commit(end).unwrap();
    let report = verify_concern(&config, ws.path(), &options);
    assert!(!report.rotations[0].deleted_segments.is_empty());
    // Only the segment holding the cursors (plus a freshly rolled empty one).
    assert!(segments() <= 2 && segments() < before, "{}", segments());
    // The deleted journal records are still in legacy: explained, not divergent.
    let report = verify_concern(&config, ws.path(), &ConcernOptions::default());
    assert!(report.ok, "{report:#?}");
    assert!(report.comparisons[0].legacy_only_predating_shadow > 0);
}

#[test]
fn a_divergent_stream_is_not_rotated() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let config = ShadowConfig::at(&root);
    append_with(&config, TelemetryStream::Shell, ShadowRecord::envelope(shell_envelope(1)))
        .unwrap();
    let report = verify_concern(
        &config,
        ws.path(),
        &ConcernOptions {
            rotate: true,
            ..ConcernOptions::default()
        },
    );
    assert!(!report.ok);
    assert!(report.rotations.is_empty());
    assert!(!root.join("shell/cursors").exists());
}

// 6. Isolation, concurrency, restart/downgrade -------------------------------

#[test]
fn streams_are_isolated() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let _f = force(Some(&root));
    crate::rework_events::append(ws.path(), &rework_marker()).unwrap();
    shell_produce(ws.path(), &shell_envelope(1));
    shell_produce(ws.path(), &shell_envelope(2));
    assert_eq!(records(&root, TelemetryStream::Rework).len(), 1);
    assert_eq!(records(&root, TelemetryStream::Shell).len(), 2);
    assert_eq!(records(&root, TelemetryStream::Shell)[0].envelope.seq, 1, "own seq space");
    for stream in [TelemetryStream::Sweep, TelemetryStream::MergeQueue] {
        assert!(!root.join(stream.name()).exists());
    }
}

#[test]
fn concurrent_producers_get_a_gapless_sequence() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let config = ShadowConfig {
                root: root.clone(),
                options: JournalOptions {
                    lock_retry: Duration::from_secs(30),
                    ..JournalOptions::default()
                },
            };
            std::thread::spawn(move || {
                for i in 0..25 {
                    let data = json!({"kind": "t", "thread": t, "i": i});
                    append_with(&config, TelemetryStream::Experiment, ShadowRecord::envelope(data))
                        .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let seqs: Vec<u64> = records(&root, TelemetryStream::Experiment)
        .iter()
        .map(|r| r.envelope.seq)
        .collect();
    assert_eq!(seqs, (1..=100).collect::<Vec<_>>());
    let structural = crate::journal::verify(&root, &crate::journal::VerifyOptions::default());
    assert!(structural.ok, "{structural:#?}");
}

#[test]
fn legacy_readers_see_the_same_records_with_shadowing_on_or_off() {
    // Downgrade safety: an old binary reads only the legacy store, so the
    // legacy bytes a producer writes must not depend on shadowing.
    let off = TempDir::new().unwrap();
    let on = TempDir::new().unwrap();
    {
        let _f = force(None);
        crate::rework_events::append(off.path(), &rework_marker()).unwrap();
    }
    {
        let root = ShadowConfig::default_root(on.path());
        let _f = force(Some(&root));
        crate::rework_events::append(on.path(), &rework_marker()).unwrap();
    }
    let strip = |ws: &Path| {
        let mut v = legacy_lines(&crate::rework_events::path(ws));
        v[0].as_object_mut().unwrap().remove("at");
        v
    };
    assert_eq!(strip(off.path()), strip(on.path()));
}

#[test]
fn a_restarted_writer_continues_the_sequence() {
    let ws = TempDir::new().unwrap();
    let root = ShadowConfig::default_root(ws.path());
    append_with(
        &ShadowConfig::at(&root),
        TelemetryStream::Sweep,
        ShadowRecord::envelope(json!({"a": 1})),
    )
    .unwrap();
    // A fresh configuration (a new process) appends after the old one.
    let seq = append_with(
        &ShadowConfig::at(&root),
        TelemetryStream::Sweep,
        ShadowRecord::envelope(json!({"a": 2})),
    )
    .unwrap();
    assert_eq!(seq, 2);
}
