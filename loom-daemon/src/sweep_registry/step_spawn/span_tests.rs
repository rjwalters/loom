//! A step's `loom.sweep` span is closed by its end, with its real outcome
//! (#12253).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::observability::lifecycle::roll_interruption::{note_stopped, RollDisposition};
use crate::sweep_registry::test_support::fixture_registry;
use crate::sweep_registry::{SweepInfo, SweepKind, SweepState};
use crate::telemetry::trace::{
    journal::Journal, store::TraceStore, SpanName, SpanRecord, SpanStatus,
};
use chrono::Utc;
use std::process::Command;

fn line(class: StepOutcomeClass) -> StepOutcomeLine {
    StepOutcomeLine {
        role: "judge".into(),
        target: 7,
        class,
        reason: "r".into(),
    }
}

fn close_of(end: StepEnd, line: Option<&StepOutcomeLine>) -> (String, TraceAttributes) {
    let outcome = super::super::capture::tick_outcome(end, line);
    sweep_close(end, line, &outcome, StepRole::Judge, Some("opus"), None)
}

#[test]
fn a_clean_exit_closes_as_success_with_its_role_and_exit_code() {
    let (result, close) = close_of(StepEnd::Exited(Some(0)), None);
    assert_eq!(result, "success");
    assert_eq!(close["loom.role"], "judge");
    assert_eq!(close["loom.exit_code"], "0");
    assert_eq!(close["loom.configured_model"], "opus");
    assert!(!close.contains_key("loom.effort"), "an unknown effort is absent");
    assert!(!close.contains_key("loom.failure_class"));
}

#[test]
fn every_failed_end_names_its_class() {
    let failed = line(StepOutcomeClass::Failed);
    let verdict = line(StepOutcomeClass::Verdict);
    for (end, line, class, code) in [
        (StepEnd::TimedOut, None, "timeout-ceiling", None),
        (StepEnd::Cancelled, Some(&verdict), "cancelled", None),
        (StepEnd::Exited(Some(0)), Some(&failed), "step-outcome-failed", Some("0")),
        (StepEnd::Exited(Some(3)), None, "exit-3", Some("3")),
        (StepEnd::Exited(None), None, "exit-unobserved", None),
    ] {
        let (result, close) = close_of(end, line);
        assert_eq!(result, "failure", "{end:?}");
        assert_eq!(close["loom.failure_class"], class, "{end:?}");
        assert_eq!(close.get("loom.exit_code").map(String::as_str), code, "{end:?}");
        assert!(!close["loom.status_message"].is_empty(), "{end:?}");
    }
}

#[test]
fn an_exit_with_unknown_status_and_an_outcome_line_is_a_success() {
    let verdict = line(StepOutcomeClass::Verdict);
    let (result, close) = close_of(StepEnd::Exited(None), Some(&verdict));
    assert_eq!(result, "success");
    assert!(!close.contains_key("loom.exit_code"));
}

fn traced(root: &Path) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(
        root.join(".loom/config.json"),
        r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#,
    )
    .unwrap();
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn insert_step(reg: &mut SweepRegistry, item: u32, role: StepRole, pid: u32) -> String {
    let sweep_id = format!("step-{role}-{item}-1");
    reg.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Step { item, role },
            pid,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: reg.compute_step_log_path(item, role),
            idempotency_key: None,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: Some("opus".into()),
            effort: Some("high".into()),
            depends_on: None,
            repo: None,
            overflow: false,
        },
    );
    sweep_id
}

/// Open the step's `loom.sweep` root as the spawn path does, owned by `pid`.
fn open_sweep_span(root: &Path, sweep_id: &str, pid: u32) -> Journal {
    let span = lifecycle::begin(
        root,
        sweep_id,
        SpanName::Sweep,
        lifecycle::attributes(&[("loom.sweep_id", sweep_id)]),
    )
    .expect("tracing is on");
    drop(span);
    lifecycle::child_spawned(root, sweep_id, pid);
    Journal::for_context(&TraceStore::new(root).path(root, sweep_id))
}

fn drained(journal: &Journal) -> Vec<SpanRecord> {
    let mut spans = Vec::new();
    journal
        .drain(|s| {
            spans.push(s);
            Ok(())
        })
        .unwrap();
    spans
}

#[test]
fn a_steps_end_closes_its_sweep_span_with_the_real_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    traced(dir.path());
    let pid = dead_pid();
    let id = insert_step(&mut reg, 12_253, StepRole::Judge, pid);
    let journal = open_sweep_span(dir.path(), &id, pid);
    assert_eq!(journal.active().unwrap().len(), 1);

    reg.end_step(&id, 12_253, StepRole::Judge, Utc::now(), StepEnd::Exited(Some(0)));

    assert!(journal.active().unwrap().is_empty(), "the step's end closed the span");
    let spans = drained(&journal);
    assert_eq!(spans.len(), 1);
    let sweep = &spans[0];
    assert_eq!(sweep.name, SpanName::Sweep);
    assert_eq!(sweep.status, SpanStatus::Ok);
    assert_eq!(sweep.attributes["loom.result"], "success");
    assert_eq!(sweep.attributes["loom.role"], "judge");
    assert_eq!(sweep.attributes["loom.configured_model"], "opus");
    assert_eq!(sweep.attributes["loom.effort"], "high");
    assert!(!sweep.attributes.contains_key("loom.recovered"));
    assert_ne!(sweep.attributes["loom.result"], "process_lost");
}

#[test]
fn a_failed_step_closes_as_failure_with_its_class() {
    let dir = tempfile::tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    traced(dir.path());
    let pid = dead_pid();
    let id = insert_step(&mut reg, 12_254, StepRole::Builder, pid);
    let journal = open_sweep_span(dir.path(), &id, pid);

    reg.end_step(&id, 12_254, StepRole::Builder, Utc::now(), StepEnd::Exited(Some(2)));

    let spans = drained(&journal);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].status, SpanStatus::Error);
    assert_eq!(spans[0].attributes["loom.result"], "failure");
    assert_eq!(spans[0].attributes["loom.failure_class"], "exit-2");
    assert_eq!(spans[0].attributes["loom.exit_code"], "2");
}

#[test]
fn a_step_a_roll_stopped_closes_as_interrupted_by_roll() {
    let dir = tempfile::tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    traced(dir.path());
    let pid = dead_pid();
    let id = insert_step(&mut reg, 12_255, StepRole::Doctor, pid);
    let journal = open_sweep_span(dir.path(), &id, pid);
    note_stopped(Some(pid), "item-12255", RollDisposition::Paused);

    reg.end_step(&id, 12_255, StepRole::Doctor, Utc::now(), StepEnd::Cancelled);

    let spans = drained(&journal);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].status, SpanStatus::Unset);
    assert_eq!(spans[0].attributes["loom.result"], "interrupted_by_roll");
    assert!(!spans[0].attributes.contains_key("loom.failure_class"));
}

fn write_lock(root: &Path, dir: &str, item: u32, sweep_id: &str, pid: u32, pgid: Option<u32>) {
    let lock = root.join(".loom/locks").join(dir);
    std::fs::create_dir_all(&lock).unwrap();
    let mut owner = LockOwner::new(item, pid, sweep_id);
    owner.pgid = pgid;
    std::fs::write(lock.join("owner.json"), serde_json::to_string(&owner).unwrap()).unwrap();
}

#[test]
fn a_step_runs_while_its_lock_names_it_and_its_leader_lives() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let me = std::process::id();
    write_lock(root, "pr-41", 41, "step-judge-41-1", me, None);
    write_lock(root, "issue-40", 40, "step-builder-40-1", me, None);
    assert!(step_still_running(root, "step-judge-41-1"));
    assert!(step_still_running(root, "step-builder-40-1"));
    // A PR step's lock is `pr-<N>`; another sweep's lock is not this step's.
    assert!(!step_still_running(root, "step-judge-40-1"));
    assert!(!step_still_running(root, "step-builder-40-2"));
    // Not a step, or no lock at all.
    assert!(!step_still_running(root, "sweep-issue-40-1"));
    assert!(!step_still_running(root, "step-curator-99-1"));
}

#[cfg(unix)]
#[test]
fn a_step_with_a_dead_leader_runs_only_while_its_group_has_members() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let dead = dead_pid();
    write_lock(root, "issue-42", 42, "step-builder-42-1", dead, None);
    assert!(!step_still_running(root, "step-builder-42-1"), "the step is gone");

    // A dead leader whose group still has a member: a wrapper retry still
    // running the session after the leader exited.
    let mut leader = Command::new("sh")
        .args(["-c", "sleep 30 >/dev/null 2>&1 & exit 0"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id();
    leader.wait().unwrap();
    write_lock(root, "issue-43", 43, "step-builder-43-1", pgid, Some(pgid));
    assert!(step_still_running(root, "step-builder-43-1"));
    // SAFETY: `pgid` is the group this test just created; signal 9 to it.
    unsafe { libc::kill(-i32::try_from(pgid).unwrap(), libc::SIGKILL) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while step_still_running(root, "step-builder-43-1") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!step_still_running(root, "step-builder-43-1"), "the group is gone");
}
