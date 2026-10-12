#![allow(clippy::unwrap_used)]
//! #11263: a run a planned roll ended closes as `interrupted_by_roll`; an
//! unplanned death still closes as `process_lost`.

use super::*;
use crate::telemetry::trace::{SpanName, SpanStatus, TraceContext};
use std::process::Command;

/// A pid that is provably gone: a child that has exited and been reaped.
fn dead_pid() -> u32 {
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// A journal holding one span, started at `started_at`, whose owner and
/// supervisor are both `pid`.
fn orphan(dir: &Path, pid: u32, started_at: DateTime<Utc>) -> Journal {
    let journal = Journal::for_context(&dir.join("execution.json"));
    let context = TraceContext::root(true);
    journal
        .start(context.clone(), None, SpanName::RoleAttempt, started_at, TraceAttributes::new())
        .unwrap();
    journal.set_owner(&context, pid).unwrap();
    journal.set_supervisor(&context, pid).unwrap();
    journal
}

fn item(
    id: &str,
    pid: u32,
    disposition: &str,
    status: &str,
    reason: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id, "kind": "role_run", "repo": "/r", "disposition": disposition,
        "status": status, "reason": reason, "pid": pid,
        "stopped_at": Utc::now().to_rfc3339(),
    })
}

/// Write a v1 manifest whose pause began at `paused_at` to `dir/name`.
fn write_manifest(dir: &Path, name: &str, paused_at: DateTime<Utc>, items: Vec<serde_json::Value>) {
    let manifest = serde_json::json!({
        "schema_version": 1, "manifest_id": "rp-test", "phase": "paused",
        "written_by": {"version": "0.19.1"},
        "roll": {"to_version": "0.19.2", "pause_started_at": paused_at.to_rfc3339(), "max_age_secs": 900},
        "items": items,
    });
    std::fs::write(dir.join(name), manifest.to_string()).unwrap();
}

fn closed(journal: &Journal) -> Vec<crate::telemetry::trace::SpanRecord> {
    assert!(journal.active().unwrap().is_empty(), "recovery closed the journal");
    let mut spans = Vec::new();
    journal
        .drain(|s| {
            spans.push(s);
            Ok(())
        })
        .unwrap();
    spans
}

/// AC: an orphaned span whose run is in a pause manifest finishes as
/// `interrupted_by_roll` with the roll's disposition and item id, not as
/// `process_lost`, and carries no failure class.
#[cfg(unix)]
#[test]
fn an_orphan_the_roll_stopped_closes_interrupted_not_process_lost() {
    for (disposition, status, reason, expected) in [
        ("resume", "paused", None, "paused"),
        ("requeue", "requeued", Some("young-agent-reset"), "requeued"),
        ("requeue", "planned", Some("pause-budget-missed"), "budget-missed"),
    ] {
        let (work, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let pid = dead_pid();
        let opened = Utc::now() - chrono::Duration::minutes(5);
        let journal = orphan(work.path(), pid, opened);
        write_manifest(
            state.path(),
            pause_manifest::MANIFEST_FILE,
            opened + chrono::Duration::minutes(1),
            vec![item("role-judge-1", pid, disposition, status, reason)],
        );
        recover(&journal, &mut Records::at(Some(state.path().to_path_buf())));
        let spans = closed(&journal);
        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        assert_eq!(s.attributes["loom.result"], RESULT, "{expected}");
        assert_eq!(s.attributes[DISPOSITION], expected);
        assert_eq!(s.attributes[ITEM_ID], "role-judge-1");
        assert_eq!(s.attributes["loom.recovered"], "true");
        assert!(!s.attributes.contains_key(role_failure::FAILURE_CLASS), "{expected}");
        assert_eq!(s.status, SpanStatus::Unset);
    }
}

/// Strict membership: no entry, an entry for another pid, a run the pause did
/// not stop (it exited first), a span opened after the pause, and an
/// unreadable manifest all leave today's `process_lost`.
#[cfg(unix)]
#[test]
fn without_a_matching_manifest_entry_an_orphan_is_still_process_lost() {
    let opened = Utc::now() - chrono::Duration::minutes(5);
    let paused_at = opened + chrono::Duration::minutes(1);
    type Setup = fn(&Path, u32, DateTime<Utc>);
    let cases: Vec<(&str, Setup)> = vec![
        ("no manifest", |_, _, _| {}),
        ("another pid", |state, pid, at| {
            write_manifest(
                state,
                pause_manifest::MANIFEST_FILE,
                at,
                vec![item("x", pid + 1, "resume", "paused", None)],
            );
        }),
        ("exited before the safe point", |state, pid, at| {
            write_manifest(
                state,
                pause_manifest::MANIFEST_FILE,
                at,
                vec![item(
                    "x",
                    pid,
                    "resume",
                    "exited",
                    Some("exited-before-safe-point"),
                )],
            );
        }),
        ("opened after the pause", |state, pid, _| {
            let before = Utc::now() - chrono::Duration::minutes(30);
            write_manifest(
                state,
                pause_manifest::MANIFEST_FILE,
                before,
                vec![item("x", pid, "resume", "paused", None)],
            );
        }),
        ("unreadable manifest", |state, _, _| {
            std::fs::write(state.join(pause_manifest::MANIFEST_FILE), "{not json").unwrap();
        }),
    ];
    for (label, setup) in cases {
        let (work, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let pid = dead_pid();
        let journal = orphan(work.path(), pid, opened);
        setup(state.path(), pid, paused_at);
        recover(&journal, &mut Records::at(Some(state.path().to_path_buf())));
        let spans = closed(&journal);
        assert_eq!(spans[0].attributes["loom.result"], "process_lost", "{label}");
        assert_eq!(spans[0].attributes[role_failure::FAILURE_CLASS], "supervisor-lost", "{label}");
        assert!(!spans[0].attributes.contains_key(DISPOSITION), "{label}");
    }
}

/// H5 archives the manifest it finished; recovery that runs after still
/// finds the roll there.
#[cfg(unix)]
#[test]
fn an_archived_manifest_still_counts() {
    let (work, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let pid = dead_pid();
    let opened = Utc::now() - chrono::Duration::minutes(5);
    let journal = orphan(work.path(), pid, opened);
    write_manifest(
        state.path(),
        "roll-pause-manifest.rp-test.done.json",
        opened + chrono::Duration::minutes(1),
        vec![item("sweep-1", pid, "resume", "resumed", None)],
    );
    recover(&journal, &mut Records::at(Some(state.path().to_path_buf())));
    let spans = closed(&journal);
    assert_eq!(spans[0].attributes["loom.result"], RESULT);
    assert_eq!(spans[0].attributes[DISPOSITION], "paused");
}

/// A journal with a live owner is left alone, and the manifests are not even
/// read for it.
#[test]
fn a_live_journal_is_not_recovered_and_reads_no_manifest() {
    let work = tempfile::tempdir().unwrap();
    let journal = orphan(work.path(), std::process::id(), Utc::now());
    let mut records = Records::at(Some(work.path().to_path_buf()));
    recover(&journal, &mut records);
    assert_eq!(journal.active().unwrap().len(), 1);
    assert!(records.loaded.is_none());
}

/// The old binary's path: H4 notes the pid it stops, and the role tick that
/// owned that child reads the note once.
#[test]
fn a_noted_stop_is_read_once_by_the_span_its_child_owns() {
    let work = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&work.path().join("execution.json"));
    let context = TraceContext::root(true);
    let active = journal
        .start(context.clone(), None, SpanName::RoleAttempt, Utc::now(), TraceAttributes::new())
        .unwrap();
    let span = Span { journal, active };
    let pid = 2_000_000_001; // never a real pid: no other test can note it
    span.journal.set_owner(&context, pid).unwrap();
    assert_eq!(stopped(&span), None, "nothing noted");
    note_stopped(Some(pid), "role-champion-1", RollDisposition::BudgetMissed);
    let roll = stopped(&span).expect("the noted stop");
    assert_eq!(roll.disposition, RollDisposition::BudgetMissed);
    assert_eq!(stopped(&span), None, "consumed");

    let mut close = attributes(&[
        ("loom.result", "failure"),
        (role_failure::FAILURE_CLASS, "exit-143"),
        ("loom.admission.reason", "x"),
    ]);
    roll.apply(&mut close);
    assert_eq!(close["loom.result"], RESULT);
    assert_eq!(close[DISPOSITION], "budget-missed");
    assert_eq!(close[ITEM_ID], "role-champion-1");
    assert!(!close.contains_key(role_failure::FAILURE_CLASS));
    assert_eq!(close["loom.admission.reason"], "x", "unrelated attributes stay");
}

#[test]
fn the_resumed_from_note_is_taken_once() {
    assert_eq!(take_resumed_from(), None);
    set_resumed_from("role-judge-7");
    assert_eq!(take_resumed_from().as_deref(), Some("role-judge-7"));
    assert_eq!(take_resumed_from(), None);
}

const COLLECTOR_CONFIG: &str =
    include_str!("../../../../../defaults/observability/collector/config.yaml");

#[test]
fn gateway_collector_keeps_every_roll_attribute() {
    let mut current = String::new();
    let mut keep = std::collections::BTreeSet::new();
    for line in COLLECTOR_CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            current = rest.trim().to_string();
        }
        if current == "span" && trimmed.contains("keep_keys(") {
            keep.extend(trimmed.split('"').skip(1).step_by(2).map(str::to_string));
        }
    }
    for key in KEYS {
        assert!(keep.contains(key), "collector span keep_keys lacks {key}");
    }
}

/// End to end through `role_invocation` (tracing needs the `otlp` feature):
/// a tick whose child H4 stopped closes `interrupted_by_roll`, unset, with no
/// failure; a resumed tick names the run it continues.
#[cfg(feature = "otlp")]
mod role_tick {
    use super::super::super::{
        role_child_exited, role_child_spawned, role_command, role_invocation, RoleTrace,
    };
    use super::*;
    use crate::role_runner::RoleTickOutcome;
    use crate::telemetry::trace::store::TraceStore;

    fn traced_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#,
        )
        .unwrap();
        dir
    }

    fn root_span(root: &Path, trace: &RoleTrace) -> crate::telemetry::trace::SpanRecord {
        let store = TraceStore::new(root);
        let journal = Journal::for_context(&store.path(root, &trace.execution));
        let mut spans = Vec::new();
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap();
        spans
            .into_iter()
            .find(|s| s.context == trace.context)
            .unwrap()
    }

    /// Launch a real child; `stop` notes it stopped by the roll first.
    fn launch(stop: bool) {
        let mut command = Command::new("true");
        role_command(&mut command);
        let mut child = command.spawn().unwrap();
        role_child_spawned(child.id());
        if stop {
            note_stopped(Some(child.id()), "role-judge-9", RollDisposition::Requeued);
        }
        child.wait().unwrap();
        role_child_exited("signal");
    }

    #[test]
    #[serial_test::serial] // `loom.repo` resolution reads the process-global `LOOM_REPO`
    fn a_tick_the_roll_stopped_closes_interrupted_and_reports_no_failure() {
        let dir = traced_root();
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            launch(true);
            RoleTickOutcome::Failure("exited with signal 15".into())
        });
        let trace = trace.unwrap();
        assert!(trace.failure.is_none(), "an interrupted tick is not a failed one");
        let root = root_span(dir.path(), &trace);
        assert_eq!(root.attributes["loom.result"], RESULT);
        assert_eq!(root.attributes[DISPOSITION], "requeued");
        assert_eq!(root.attributes[ITEM_ID], "role-judge-9");
        assert!(!root.attributes.contains_key(role_failure::FAILURE_CLASS));
        assert_eq!(root.status, SpanStatus::Unset);
    }

    #[test]
    #[serial_test::serial]
    fn an_unstopped_failure_still_closes_as_failure() {
        let dir = traced_root();
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            launch(false);
            RoleTickOutcome::Failure("exit 1".into())
        });
        let trace = trace.unwrap();
        let root = root_span(dir.path(), &trace);
        assert_eq!(root.attributes["loom.result"], "failure");
        assert_eq!(root.status, SpanStatus::Error);
        assert!(!root.attributes.contains_key(DISPOSITION));
    }

    /// AC: the resumed run's span carries the link to the interrupted span.
    #[test]
    #[serial_test::serial]
    fn a_resumed_tick_names_the_run_it_continues() {
        let dir = traced_root();
        set_resumed_from("role-judge-9");
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            launch(false);
            RoleTickOutcome::Success
        });
        let root = root_span(dir.path(), &trace.unwrap());
        assert_eq!(root.attributes["loom.result"], "success");
        assert_eq!(root.attributes[RESUMED_FROM], "role-judge-9");
        assert_eq!(take_resumed_from(), None, "the note does not leak to the next tick");
    }
}
