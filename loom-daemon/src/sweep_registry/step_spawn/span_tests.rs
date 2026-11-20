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
    let outcome = super::super::capture::tick_outcome(StepRole::Judge, end, line);
    sweep_close(
        end,
        line,
        &outcome,
        StepRole::Judge,
        None,
        Some("opus"),
        None,
        None,
        None,
        false,
        None,
    )
}

/// #12957: a sized step's close carries its points; an unsized one has no
/// attribute, never `0`.
#[test]
fn a_sized_steps_close_carries_its_story_points() {
    let outcome = super::super::capture::tick_outcome(StepRole::Judge, StepEnd::TimedOut, None);
    let close = |points| {
        sweep_close(
            StepEnd::TimedOut,
            None,
            &outcome,
            StepRole::Judge,
            None,
            None,
            None,
            points,
            None,
            false,
            None,
        )
        .1
    };
    assert_eq!(close(Some(13))["loom.story_points"], "13");
    assert!(!close(None).contains_key("loom.story_points"));
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

/// #14168: the step's own word on its work rides the close — class and
/// reason for a parsed line, and `none` with no reason when the step wrote
/// no parseable line, so "no outcome" is countable rather than an absence.
#[test]
fn a_close_carries_the_steps_outcome_class_and_reason() {
    let verdict = line(StepOutcomeClass::Verdict);
    let (_, with_line) = close_of(StepEnd::Exited(Some(0)), Some(&verdict));
    assert_eq!(with_line["loom.step.outcome.class"], "verdict");
    assert_eq!(with_line["loom.step.outcome.reason"], "r");
    let (_, without) = close_of(StepEnd::Exited(Some(0)), None);
    assert_eq!(without["loom.step.outcome.class"], "none");
    assert!(
        !without.contains_key("loom.step.outcome.reason"),
        "no line, no reason: the absence is not a fabricated empty token"
    );
}

/// #14168: the registry's runtime for the step rides the close, so
/// "outcomes by runtime, model and role" is one query over the span. An
/// unknown runtime is absent, never a fabricated name.
#[test]
fn a_close_carries_the_runtime_that_ran_the_step() {
    let outcome = super::super::capture::tick_outcome(StepRole::Judge, StepEnd::TimedOut, None);
    let close = |runtime| {
        sweep_close(
            StepEnd::TimedOut,
            None,
            &outcome,
            StepRole::Judge,
            runtime,
            None,
            None,
            None,
            None,
            false,
            None,
        )
        .1
    };
    assert_eq!(close(Some("codex"))["loom.runtime"], "codex");
    assert!(!close(None).contains_key("loom.runtime"));
    assert!(!close(Some("")).contains_key("loom.runtime"));
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
    assert!(!sweep.attributes.contains_key("loom.story_points"), "an unsized step");
}

/// #12957: the points the step was spawned with reach its `loom.sweep` close
/// through the registry, past the export allowlist.
#[test]
fn a_sized_steps_end_closes_its_sweep_span_with_its_points() {
    let dir = tempfile::tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    traced(dir.path());
    let pid = dead_pid();
    let id = insert_step(&mut reg, 12_957, StepRole::Doctor, pid);
    reg.step_points.insert(id.clone(), 8);
    let journal = open_sweep_span(dir.path(), &id, pid);

    reg.end_step(&id, 12_957, StepRole::Doctor, Utc::now(), StepEnd::Exited(Some(0)));

    let spans = drained(&journal);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].attributes["loom.story_points"], "8");
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

/// Open the step's `loom.sweep` root plus the `loom.phase` / `loom.role_attempt`
/// that `sweep-checkpoint begin` leaves open under it (#14407).
fn open_step_with_attempt(root: &Path, sweep_id: &str, pid: u32, role: &str) -> Journal {
    let span = lifecycle::begin(
        root,
        sweep_id,
        SpanName::Sweep,
        lifecycle::attributes(&[("loom.sweep_id", sweep_id)]),
    )
    .expect("tracing is on");
    let attrs = lifecycle::attributes(&[
        ("loom.role", role),
        ("loom.timing_source", lifecycle::CHECKPOINT_BEGIN_SOURCE),
    ]);
    let phase = span.child(SpanName::Phase, attrs.clone()).unwrap();
    phase.child(SpanName::RoleAttempt, attrs).unwrap();
    drop(span);
    lifecycle::child_spawned(root, sweep_id, pid);
    Journal::for_context(&TraceStore::new(root).path(root, sweep_id))
}

/// #14407: a step-dispatched attempt on codex whose end was observed carries
/// the step's result and runtime on its attempt and phase, not `exit_unobserved`.
#[test]
fn a_steps_end_gives_its_attempt_and_phase_the_result_and_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    traced(dir.path());
    let pid = dead_pid();
    let id = insert_step(&mut reg, 14_407, StepRole::Builder, pid);
    reg.entries.get_mut(&id).unwrap().runtime = "codex".into();
    let journal = open_step_with_attempt(dir.path(), &id, pid, "builder");
    assert_eq!(journal.active().unwrap().len(), 3);

    reg.end_step(&id, 14_407, StepRole::Builder, Utc::now(), StepEnd::Exited(Some(2)));

    assert!(journal.active().unwrap().is_empty());
    let spans = drained(&journal);
    let sweep = spans.iter().find(|s| s.name == SpanName::Sweep).unwrap();
    for name in [SpanName::RoleAttempt, SpanName::Phase] {
        let s = spans.iter().find(|s| s.name == name).unwrap();
        assert_eq!(s.attributes["loom.result"], sweep.attributes["loom.result"], "{name:?}");
        assert_eq!(s.attributes["loom.result"], "failure", "{name:?}");
        assert_eq!(s.attributes["loom.runtime"], "codex", "{name:?}");
        assert_eq!(s.attributes["loom.engine"], "codex", "{name:?}");
        assert_eq!(s.attributes["loom.timing_source"], "terminal_observed", "{name:?}");
    }
}

/// #14407: a registry entry with no known runtime stamps none (never guessed),
/// and a step a roll stopped was not observed to end, so it stays unobserved.
#[test]
fn an_unknown_runtime_is_not_stamped_and_a_roll_stopped_step_stays_unobserved() {
    let dir = tempfile::tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    traced(dir.path());
    let pid = dead_pid();
    let id = insert_step(&mut reg, 14_408, StepRole::Judge, pid);
    let journal = open_step_with_attempt(dir.path(), &id, pid, "judge");
    reg.end_step(&id, 14_408, StepRole::Judge, Utc::now(), StepEnd::Exited(Some(0)));
    let spans = drained(&journal);
    let attempt = spans
        .iter()
        .find(|s| s.name == SpanName::RoleAttempt)
        .unwrap();
    assert_eq!(attempt.attributes["loom.result"], "success");
    assert!(!attempt.attributes.contains_key("loom.runtime"));

    let pid = dead_pid();
    let id = insert_step(&mut reg, 14_409, StepRole::Doctor, pid);
    reg.entries.get_mut(&id).unwrap().runtime = "codex".into();
    let journal = open_step_with_attempt(dir.path(), &id, pid, "doctor");
    note_stopped(Some(pid), "item-14409", RollDisposition::Paused);
    reg.end_step(&id, 14_409, StepRole::Doctor, Utc::now(), StepEnd::Cancelled);
    let spans = drained(&journal);
    for name in [SpanName::RoleAttempt, SpanName::Phase] {
        let s = spans.iter().find(|s| s.name == name).unwrap();
        assert_eq!(s.attributes["loom.result"], "exit_unobserved", "{name:?}");
        assert!(!s.attributes.contains_key("loom.runtime"), "{name:?}");
    }
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

/// #13169: a Builder step's close names the PR it opened; absent otherwise.
#[test]
fn a_close_carries_the_pr_number_only_when_one_is_known() {
    let outcome =
        super::super::capture::tick_outcome(StepRole::Builder, StepEnd::Exited(Some(0)), None);
    let close = |pr| {
        sweep_close(
            StepEnd::Exited(Some(0)),
            None,
            &outcome,
            StepRole::Builder,
            None,
            None,
            None,
            None,
            pr,
            false,
            None,
        )
        .1
    };
    assert_eq!(close(Some(99))["loom.pr_number"], "99");
    assert!(!close(None).contains_key("loom.pr_number"));
}

/// #13521: a clean-exit Builder step with no outcome line closes as
/// `no-outcome-line-pr-opened` when it opened a PR, and as `no-outcome-line`
/// when none is known (an unreadable PR fails closed to the old class).
#[test]
fn a_builder_with_no_line_splits_by_whether_it_opened_a_pr() {
    let outcome =
        super::super::capture::tick_outcome(StepRole::Builder, StepEnd::Exited(Some(0)), None);
    assert!(matches!(outcome, RoleTickOutcome::Failure(_)), "tick outcome is unchanged");
    let class = |pr| {
        sweep_close(
            StepEnd::Exited(Some(0)),
            None,
            &outcome,
            StepRole::Builder,
            None,
            None,
            None,
            None,
            pr,
            false,
            None,
        )
        .1[lifecycle::FAILURE_CLASS]
            .clone()
    };
    assert_eq!(class(Some(7)), "no-outcome-line-pr-opened");
    assert_eq!(class(None), "no-outcome-line");
}

/// #13966: a step whose provider feedback said its api-key seat was
/// exhausted closes as `provider-exhausted`, not its exit shape — unless it
/// was cancelled, where the operator's own act stays the class.
#[test]
fn a_provider_exhausted_step_closes_as_provider_exhausted() {
    let outcome = RoleTickOutcome::Failure("step exited 1".to_string());
    let class = |end, exhausted| {
        sweep_close(
            end,
            None,
            &outcome,
            StepRole::Builder,
            None,
            None,
            None,
            None,
            None,
            exhausted,
            None,
        )
        .1[lifecycle::FAILURE_CLASS]
            .clone()
    };
    assert_eq!(class(StepEnd::Exited(Some(1)), true), "provider-exhausted");
    assert_eq!(class(StepEnd::Exited(Some(1)), false), "exit-1");
    assert_eq!(class(StepEnd::Cancelled, true), "cancelled");
    // A step that hit the limit and then hung to its timeout: the exhaustion
    // wins here as it does on `role_tick.outcome` (`capture::step_failure`).
    assert_eq!(class(StepEnd::TimedOut, true), "provider-exhausted");
    assert_eq!(class(StepEnd::TimedOut, false), "timeout-ceiling");
}

/// #14234: a close with a line and a noted post-state records whether the
/// forge agreed with the class the step claimed.
mod agrees {
    use super::*;
    use crate::step_dispatch::outcome_ledger::PostState;

    const KEY: &str = "loom.step.outcome.agrees";
    const MOVED: PostState = PostState {
        moved: true,
        closed: false,
        claim_held: false,
    };
    const UNMOVED: PostState = PostState {
        moved: false,
        closed: false,
        claim_held: false,
    };

    fn close(
        line: Option<&StepOutcomeLine>,
        post: Option<PostState>,
        exhausted: bool,
    ) -> TraceAttributes {
        let end = StepEnd::Exited(Some(0));
        let outcome = super::super::super::capture::tick_outcome(StepRole::Judge, end, line);
        sweep_close(
            end,
            line,
            &outcome,
            StepRole::Judge,
            None,
            None,
            None,
            None,
            None,
            exhausted,
            post,
        )
        .1
    }

    #[test]
    fn a_verdict_the_forge_shows_agrees() {
        let c = close(Some(&line(StepOutcomeClass::Verdict)), Some(MOVED), false);
        assert_eq!(c[KEY], "true");
    }

    /// The step claimed `verdict`, the item did not move: the ledger says
    /// `failed`, the claim is recorded as a disagreement, and nothing else
    /// on the close changes.
    #[test]
    fn a_verdict_the_forge_does_not_show_disagrees() {
        let verdict = line(StepOutcomeClass::Verdict);
        let c = close(Some(&verdict), Some(UNMOVED), false);
        assert_eq!(c[KEY], "false");
        assert_eq!(c["loom.step.outcome.class"], "verdict");
        let mut without = close(Some(&verdict), None, false);
        let mut with = c;
        with.remove(KEY);
        without.remove(KEY);
        assert_eq!(with, without, "the attribute is the only difference");
    }

    #[test]
    fn a_noop_on_an_unmoved_item_agrees() {
        let c = close(Some(&line(StepOutcomeClass::Noop)), Some(UNMOVED), false);
        assert_eq!(c[KEY], "true");
    }

    #[test]
    fn a_held_claim_disagrees_with_anything_but_failed() {
        let held = PostState {
            claim_held: true,
            ..UNMOVED
        };
        assert_eq!(close(Some(&line(StepOutcomeClass::Verdict)), Some(held), false)[KEY], "false");
        assert_eq!(close(Some(&line(StepOutcomeClass::Failed)), Some(held), false)[KEY], "true");
    }

    #[test]
    fn absent_without_a_line_without_a_post_state_or_on_provider_exhaustion() {
        assert!(!close(None, Some(MOVED), false).contains_key(KEY), "no line");
        let verdict = line(StepOutcomeClass::Verdict);
        assert!(!close(Some(&verdict), None, false).contains_key(KEY), "no post-state");
        assert!(
            !close(Some(&verdict), Some(MOVED), true).contains_key(KEY),
            "provider exhausted"
        );
    }

    /// The close reads the noted post-state without consuming it: the tick's
    /// `take_post` still gets it.
    #[test]
    fn peeking_the_post_state_leaves_it_for_the_tick() {
        use super::super::super::ended::{note_post, peek_post, take_post};
        let root = std::path::Path::new("/tmp/peek-post-14234");
        note_post(root, "step-judge-1-1", Some(MOVED));
        assert_eq!(peek_post(root, "step-judge-1-1"), Some(Some(MOVED)));
        assert_eq!(peek_post(root, "step-judge-1-1"), Some(Some(MOVED)), "peek twice");
        assert_eq!(take_post(root, "step-judge-1-1"), Some(Some(MOVED)));
        assert_eq!(peek_post(root, "step-judge-1-1"), None);
    }
}
