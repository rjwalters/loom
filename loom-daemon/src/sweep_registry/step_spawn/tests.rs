//! Step spawn (#11906): the launch, the role's resolvers, the claim
//! contract, the timeout reap and the end-of-step capture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::capture::{
    emit_step_capture, launch_rejection_of, parse_outcome_line, read_outcome_line,
    release_class_fallback, release_class_of_line, step_failure, step_outcome_projection,
    tick_outcome, StepCapture, StepEnd, StepOutcomeClass, StepOutcomeLine,
};
use super::lease::{claim_args, claim_verdict, ClaimRun};
use super::timeout::timed_out;
use super::*;
use crate::observability::ops::capture::capture;
use crate::role_runner::RoleTickOutcome;
use crate::sweep_registry::test_support::{assert_child_wrote, fixture_registry, write_config};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use tempfile::tempdir;

fn cancel_all(reg: &mut SweepRegistry) {
    let ids: Vec<String> = reg.entries.keys().cloned().collect();
    for id in ids {
        let _ = reg.cancel(&id, Duration::from_millis(50));
    }
}

fn step_span_reason(spans: &[crate::telemetry::trace::SpanRecord]) -> Vec<(String, String)> {
    spans
        .iter()
        .filter(|s| s.name.as_str() == "loom.dispatch.step")
        .map(|s| {
            (
                s.attributes["loom.dispatch.admission_result"].clone(),
                s.attributes["loom.dispatch.reason"].clone(),
            )
        })
        .collect()
}

// ---- spawn ----------------------------------------------------------------

/// A Judge step launches `/loom:judge N` through the workspace's spawn
/// binary, is tracked as a step holding the PR lock, and emits its span.
#[test]
#[serial]
fn a_step_launches_its_roles_prompt_and_is_tracked_as_a_step() {
    let dir = tempdir().unwrap();
    let (mut reg, record) = fixture_registry(dir.path());
    let request = StepSpawnRequest {
        draw_attempt: 2,
        age_source: "events".into(),
        ..StepSpawnRequest::new(9_100, StepRole::Judge)
    };
    let (outcome, signals) = capture(|| reg.spawn_step(&request));
    let outcome = outcome.unwrap();
    assert!(outcome.sweep_id.starts_with("step-judge-9100-"), "{}", outcome.sweep_id);
    let recorded = assert_child_wrote(&record, &format!("LOOM_SWEEP_ID={}", outcome.sweep_id));
    assert!(recorded.contains("arg: /loom:judge 9100"), "{recorded}");
    // A PR step's item is a PR, never an issue claim (`build-gate.sh`).
    assert!(recorded.contains("LOOM_SWEEP_CLAIM_OWNED=unset"), "{recorded}");
    let info = &reg.entries[&outcome.sweep_id];
    assert_eq!(
        info.kind,
        SweepKind::Step {
            item: 9_100,
            role: StepRole::Judge
        }
    );
    assert!(reg.config.locks_dir().join("pr-9100").exists(), "a PR step holds pr-<N>");
    let spans = &signals.spans;
    let span = spans
        .iter()
        .find(|s| s.name.as_str() == "loom.dispatch.step")
        .unwrap();
    assert_eq!(span.attributes["loom.step.draw_attempt"], "2");
    assert_eq!(span.attributes["loom.step.age_source"], "events");
    assert_eq!(step_span_reason(spans), vec![("spawned".into(), "ok".into())]);
    cancel_all(&mut reg);
}

/// Champion's two steps take the flag their targeted mode parses.
#[test]
#[serial]
fn a_champion_merge_step_runs_the_targeted_pr_mode() {
    let dir = tempdir().unwrap();
    let (mut reg, record) = fixture_registry(dir.path());
    let outcome = reg
        .spawn_step(&StepSpawnRequest::new(9_101, StepRole::ChampionMerge))
        .unwrap();
    let recorded = assert_child_wrote(&record, &format!("LOOM_SWEEP_ID={}", outcome.sweep_id));
    assert!(recorded.contains("arg: /loom:champion --pr 9101"), "{recorded}");
    cancel_all(&mut reg);
}

/// The role's resolvers: `roleModels` / `roleEfforts` for that role, never
/// the sweep's model.
#[test]
#[serial]
fn a_step_runs_with_its_roles_model_and_effort() {
    let dir = tempdir().unwrap();
    let (mut reg, record) = fixture_registry(dir.path());
    write_config(
        dir.path(),
        r#"{"autonomous":{"roleRunner":{"roleModels":{"doctor":"claude-doctor-pin"},"roleEfforts":{"doctor":"high"}}}}"#,
    );
    let outcome = reg
        .spawn_step(&StepSpawnRequest::new(9_102, StepRole::Doctor))
        .unwrap();
    let recorded = assert_child_wrote(&record, &format!("LOOM_SWEEP_ID={}", outcome.sweep_id));
    assert!(recorded.contains("arg: --model\narg: claude-doctor-pin"), "{recorded}");
    assert!(recorded.contains("arg: --effort\narg: high"), "{recorded}");
    assert_eq!(reg.entries[&outcome.sweep_id].model.as_deref(), Some("claude-doctor-pin"));
    cancel_all(&mut reg);
}

/// An item whose lock another live run holds is refused before any write,
/// and the refusal is a span with its reason.
#[test]
#[serial]
fn a_held_item_is_refused_as_claimed() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    crate::sweep_registry::test_support::write_lock_owner(
        &reg,
        9_103,
        "sweep-issue-9103-1",
        std::process::id(),
    );
    let (result, signals) =
        capture(|| reg.spawn_step(&StepSpawnRequest::new(9_103, StepRole::Curator)));
    let refusal = result.unwrap_err().downcast::<StepRefusal>().unwrap();
    assert_eq!(refusal.reason, "claimed");
    assert!(reg.entries.is_empty());
    assert_eq!(step_span_reason(&signals.spans), vec![("refused".into(), "claimed".into())]);
}

/// The step child's own environment: its lease file (so its leases are
/// reaped), and the Curator's curation provenance.
#[test]
fn a_step_child_lists_its_leases_and_a_curator_names_its_launch() {
    let root = Path::new("/ws");
    let env = |cmd: &Command, key: &str| {
        cmd.get_envs()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned())
    };
    let mut cmd = Command::new("true");
    apply_step_env(&mut cmd, root, "step-curator-5-1", StepRole::Curator, Some("opus"), None);
    assert_eq!(
        env(&cmd, crate::pr_lease_reap::LEASE_FILE_ENV),
        Some(
            crate::pr_lease_reap::path_for(root, "step-curator-5-1")
                .display()
                .to_string()
        )
    );
    assert_eq!(
        env(&cmd, crate::host_identity::LEASE_HOST_ID_ENV),
        Some(crate::host_identity::lease_host_identity()),
        "the daemon's lease host id, so a containered session owns its lease"
    );
    assert_eq!(
        env(&cmd, crate::host_identity::HOST_ID_ENV),
        None,
        "the telemetry host id is left alone (#11530)"
    );
    assert_eq!(env(&cmd, "LOOM_CURATION_MODEL").as_deref(), Some("opus"));
    assert_eq!(env(&cmd, "LOOM_CURATION_RUNTIME").as_deref(), Some("unknown"));
    let mut cmd = Command::new("true");
    apply_step_env(&mut cmd, root, "step-judge-5-1", StepRole::Judge, Some("opus"), None);
    assert_eq!(env(&cmd, "LOOM_CURATION_MODEL"), None);
}

#[test]
fn a_pool_or_runtime_gate_refuses_with_its_own_reason() {
    assert_eq!(refusal_of(&RoleTickOutcome::NoTokenPool).reason, "no_token_pool");
    assert_eq!(refusal_of(&RoleTickOutcome::Failure("x".into())).reason, "admission_refused");
}

/// #14026 acceptance criterion 3, at the refusal boundary: a preference-path
/// exhaustion longer than the marker's 160-char cap reaches the step refusal
/// — the text `begin_step_spawn` logs after its `sweep_registry:` prefix —
/// as ONE line carrying the full untruncated reason, never the multi-line
/// operator diagnostic.
#[test]
fn a_runtime_rejected_refusal_carries_the_one_line_candidate_rendering() {
    let obligation = "owned account lease plus durable workspace preparation for model \
                      claude/sonnet on profile zai-metered is still outstanding (#13916)";
    let rejection = crate::runtime_admission::RuntimeRejection {
        role: "curator".into(),
        runtime: String::new(),
        source: crate::runtime_admission::RuntimeSource::Preference,
        unmet_capabilities: vec![],
        reason: format!(
            "No runtime in the preference list can serve role \"curator\" right now\n  \
             codex -> not-admitted(unmet capabilities: worktreeIsolation; {obligation})"
        ),
        exhausted_summary_line: Some(
            format!(
                "runtime_rejected role=curator: codex -> not-admitted(unmet capabilities: \
             worktreeIsolation; {obligation})"
            )
            .into_boxed_str(),
        ),
    };
    let refusal = refusal_of(&RoleTickOutcome::RuntimeRejected(rejection));
    assert_eq!(refusal.reason, "runtime_rejected");
    assert!(!refusal.detail.contains('\n'), "one line: {}", refusal.detail);
    assert!(refusal.detail.contains(obligation), "{}", refusal.detail);
    assert!(!refusal.detail.contains('\u{2026}'), "{}", refusal.detail);
    // A rejection without the one-line rendering (the static path) falls back
    // to its own reason, byte-identical to the pre-#14026 refusal text.
    let static_rejection = crate::runtime_admission::RuntimeRejection {
        role: "builder".into(),
        runtime: "codex".into(),
        source: crate::runtime_admission::RuntimeSource::RoleConfig,
        unmet_capabilities: vec!["worktreeIsolation".into()],
        reason: "unmet capabilities: worktreeIsolation".into(),
        exhausted_summary_line: None,
    };
    let refusal = refusal_of(&RoleTickOutcome::RuntimeRejected(static_rejection));
    assert_eq!(refusal.detail, "unmet capabilities: worktreeIsolation");
}

// ---- B1: the claim --------------------------------------------------------

/// The pre-claim takes the role-keyed lease with the step, under the step's
/// sweep id, and the renewal start is the same claim watching the child.
#[test]
fn the_pre_claim_is_the_role_keyed_lease_with_the_step() {
    let root = Path::new("/ws");
    let args = claim_args(root, 42, StepRole::ChampionPromote, "step-champion_promote-42-1", None);
    assert_eq!(
        args.join(" "),
        "lease pr claim 42 --role champion --step champion_promote --sweep-id \
         step-champion_promote-42-1 --workspace /ws"
    );
    let renew = claim_args(root, 42, StepRole::Judge, "step-judge-42-1", Some(777));
    assert!(renew.join(" ").starts_with("lease pr attach 42 "), "renew-only");
    assert!(renew.join(" ").contains("--watch-pid 777"));
}

#[test]
fn a_lost_or_unreadable_pre_claim_refuses_the_step() {
    assert_eq!(claim_verdict(&ClaimRun::Exited(0)), Ok(()));
    let reason = |run: ClaimRun| claim_verdict(&run).unwrap_err().reason;
    assert_eq!(reason(ClaimRun::Exited(4)), "claim_lost");
    assert_eq!(reason(ClaimRun::Exited(6)), "backstop_hold");
    // #12105: a Doctor that found another actor active on the PR.
    assert_eq!(reason(ClaimRun::Exited(7)), "peer_active");
    assert_eq!(reason(ClaimRun::Exited(5)), "claim_unverified");
    assert_eq!(reason(ClaimRun::TimedOut), "claim_unverified");
    assert_eq!(reason(ClaimRun::SpawnFailed), "claim_unverified");
    assert_eq!(reason(ClaimRun::Signaled), "claim_unverified");
}

// ---- B3: the timeout ------------------------------------------------------

#[test]
#[serial]
fn each_role_has_its_timeout_and_builder_the_sweeps() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let role_timeout = crate::role_runner::DEFAULT_ROLE_TIMEOUT;
    assert_eq!(step_timeout(root, StepRole::Judge), role_timeout);
    assert_eq!(step_timeout(root, StepRole::Curator), role_timeout);
    std::env::remove_var(super::super::watchdog::STALE_SWEEP_AGE_ENV);
    assert!(step_timeout(root, StepRole::Builder) > role_timeout);
    std::env::set_var(super::super::watchdog::STALE_SWEEP_AGE_ENV, "7200");
    assert_eq!(step_timeout(root, StepRole::Builder), Duration::from_secs(7200));
    std::env::remove_var(super::super::watchdog::STALE_SWEEP_AGE_ENV);
    let now = Utc::now();
    let secs = role_timeout.as_secs() as i64;
    assert!(!timed_out(role_timeout, now - chrono::Duration::seconds(secs - 5), now));
    assert!(timed_out(role_timeout, now - chrono::Duration::seconds(secs + 5), now));
}

/// A fake `gh` answering the issue read with `labels` on an open item, and
/// recording every call.
fn fake_gh(dir: &Path, labels: &str) -> (PathBuf, PathBuf) {
    let fake = dir.join("fake-gh.sh");
    let log = dir.join("gh-calls.log");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  case "$*" in
    *".pull_request != null"*) echo "true" ;;
    *".state"*) printf '{{"body":"","state":"open","closed_at":null,"labels":[{labels}]}}' ;;
    *) echo "[]" ;;
  esac
fi
exit 0
"#,
        log = log.display(),
    );
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    (fake, log)
}

/// B3: a Judge step past its timeout is killed, and its claim is released
/// back to `loom:review-requested` through the step restore; a step still
/// inside its timeout is left alone.
#[test]
#[serial]
fn a_step_past_its_timeout_is_reaped_and_its_claim_restored() {
    let dir = tempdir().unwrap();
    let (gh, calls) = fake_gh(dir.path(), "\"loom:reviewing\"");
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(gh);
    config.skip_label_flip = false;
    config.journal_path = Some(dir.path().join("journal.json"));
    config.outcomes_journal_path = Some(dir.path().join("outcomes.jsonl"));
    config.outcome_telemetry_path = Some(dir.path().join("telemetry.jsonl"));
    let mut reg = SweepRegistry::new(config);

    let insert = |reg: &mut SweepRegistry, item: u32, age: Duration| {
        let child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let id = format!("step-judge-{item}-1");
        let lock = reg.config.locks_dir().join(format!("pr-{item}"));
        std::fs::create_dir_all(&lock).unwrap();
        let owner = LockOwner::new(item, pid, id.as_str());
        std::fs::write(lock.join("owner.json"), serde_json::to_string(&owner).unwrap()).unwrap();
        reg.children.insert(id.clone(), child);
        reg.entries.insert(
            id.clone(),
            SweepInfo {
                sweep_id: id.clone(),
                kind: SweepKind::Step {
                    item,
                    role: StepRole::Judge,
                },
                pid,
                pgid: Some(pid),
                token_name: "unknown".into(),
                runtime: "claude".into(),
                runtime_source: None,
                log_path: reg.compute_step_log_path(item, StepRole::Judge),
                idempotency_key: None,
                started_at: Utc::now() - chrono::Duration::from_std(age).unwrap(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
            },
        );
        (id, pid)
    };
    let past = step_timeout(dir.path(), StepRole::Judge) + Duration::from_secs(60);
    let (late, late_pid) = insert(&mut reg, 9_200, past);
    let (fresh, fresh_pid) = insert(&mut reg, 9_201, Duration::from_secs(60));

    assert_eq!(reg.reap_step_timeouts(), 1);
    assert!(reg.entries[&late].state.is_terminal(), "{:?}", reg.entries[&late].state);
    assert!(
        crate::sweep_registry::test_support::wait_until_dead(late_pid, 10_000),
        "the timed-out step is killed"
    );
    assert!(!reg.config.locks_dir().join("pr-9200").exists(), "its lock is released");
    let gh_calls = std::fs::read_to_string(&calls).unwrap_or_default();
    assert!(
        gh_calls.contains(
            "pr edit 9200 --remove-label loom:reviewing --add-label loom:review-requested"
        ),
        "the claim is restored: {gh_calls}"
    );
    assert!(matches!(reg.entries[&fresh].state, SweepState::Running));
    assert!(!gh_calls.contains("9201"), "a step inside its timeout is untouched");
    assert!(!super::timeout::take_timed_out(&late), "the timeout mark was consumed");
    let _ = reg.cancel(&fresh, Duration::from_millis(50));
    let _ = crate::sweep_registry::test_support::wait_until_dead(fresh_pid, 10_000);
}

// ---- the end: outcome line and capture ------------------------------------

#[test]
fn an_outcome_line_parses_and_the_prompts_template_does_not() {
    let line = parse_outcome_line(
        "done. LOOM_STEP_OUTCOME role=judge target=42 class=verdict reason=approved",
    )
    .unwrap();
    assert_eq!(line.role, "judge");
    assert_eq!(line.target, 42);
    assert_eq!(line.class, StepOutcomeClass::Verdict);
    assert_eq!(line.reason, "approved");
    assert!(parse_outcome_line(
        "`LOOM_STEP_OUTCOME role=judge target=<n> class=<verdict|noop|refused|failed> reason=<token>`"
    )
    .is_none());
}

/// #14168: the three framings runtimes put on the line — bare, backticked,
/// inside a JSON text event (the wrapper's `"}` or literal `\n` escape
/// glued to the last token) — parse to the identical `StepOutcomeLine`.
#[test]
fn the_three_outcome_line_framings_parse_identically() {
    let expected = StepOutcomeLine {
        role: "judge".into(),
        target: 7,
        class: StepOutcomeClass::Verdict,
        reason: "ci_pending".into(),
    };
    let bare = "LOOM_STEP_OUTCOME role=judge target=7 class=verdict reason=ci-pending";
    let backticked = "`LOOM_STEP_OUTCOME role=judge target=7 class=`verdict` reason=`ci-pending`";
    let json_event = concat!(
        r#"{"type":"text","text":"LOOM_STEP_OUTCOME role=judge target=7 "#,
        r#"class=verdict reason=ci-pending\n"}"#,
    );
    for (framing, raw) in [
        ("bare", bare),
        ("backticked", backticked),
        ("json-text-event", json_event),
    ] {
        assert_eq!(parse_outcome_line(raw).unwrap(), expected, "{framing}");
    }
    // A quoted value inside the JSON framing ends at its closing quote, and
    // the JSON keys around it never read as outcome fields.
    let quoted = r#"{"role":"assistant","text":"LOOM_STEP_OUTCOME role=judge target=7 class=verdict reason=ci-pending"}"#;
    assert_eq!(parse_outcome_line(quoted).unwrap(), expected);
}

/// #14168: a recorded `reason` is the reason as written (its `-` folded),
/// truncated to a fixed bound so a runaway line cannot pin an unbounded
/// attribute value.
#[test]
fn a_reason_is_recorded_bounded() {
    let long = "r".repeat(500);
    let line = parse_outcome_line(&format!(
        "LOOM_STEP_OUTCOME role=judge target=1 class=noop reason={long}"
    ))
    .unwrap();
    assert_eq!(line.reason.chars().count(), 64);
    assert!(line.reason.chars().all(|c| c == 'r'));
    let short =
        parse_outcome_line("LOOM_STEP_OUTCOME role=judge target=1 class=noop reason=ci-pending")
            .unwrap();
    assert_eq!(short.reason, "ci_pending", "as written, folded");
}

/// #14168: an unknown class is not a parseable line, so the step records
/// `none` — the countable no-outcome value — on the span and the record.
#[test]
fn an_unknown_class_yields_none() {
    assert!(
        parse_outcome_line("LOOM_STEP_OUTCOME role=judge target=1 class=banana reason=x").is_none()
    );
    assert_eq!(step_outcome_projection(None), ("none".to_string(), String::new()));
    assert_eq!(
        step_outcome_projection(Some(
            &parse_outcome_line("LOOM_STEP_OUTCOME role=judge target=1 class=noop reason=x")
                .unwrap()
        )),
        ("noop".to_string(), "x".to_string())
    );
}

/// #12541: a Champion step writes only its `CHAMPION-STEP` line, so the
/// captain release takes that line's class, not `failed`.
#[test]
fn a_champion_steps_release_class_is_its_champion_line_folded() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("step.log");
    let header = |id: &str| {
        format!("==== loom-daemon dispatch: t sweep_id={id} item=7 step=champion_merge ====\n")
    };
    let role = StepRole::ChampionMerge;
    for (outcome, class) in [
        ("triaged", "verdict"),
        ("merged", "verdict"),
        ("not-eligible", "refused"),
        ("held", "noop"),
        ("requeued", "ci_wait"),
        ("failed", "failed"),
    ] {
        let body = format!("{}CHAMPION-STEP mode=pr item=7 outcome={outcome}\n", header("s-1"));
        std::fs::write(&log, body).unwrap();
        assert_eq!(release_class_fallback(&log, "s-1", role, 7), class, "{outcome}");
    }
    // Another item's line, an earlier run's line, or no line: `failed`.
    let other = format!("{}CHAMPION-STEP mode=pr item=8 outcome=merged\n", header("s-1"));
    std::fs::write(&log, other).unwrap();
    assert_eq!(release_class_fallback(&log, "s-1", role, 7), "failed");
    let earlier = format!(
        "{}CHAMPION-STEP mode=pr item=7 outcome=merged\n{}",
        header("s-1"),
        header("s-2")
    );
    std::fs::write(&log, earlier).unwrap();
    assert_eq!(release_class_fallback(&log, "s-2", role, 7), "failed");
    assert_eq!(release_class_fallback(&dir.path().join("none.log"), "s-1", role, 7), "failed");
}

#[test]
fn the_outcome_line_is_read_after_this_steps_own_header_only() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("step.log");
    std::fs::write(
        &log,
        "==== loom-daemon dispatch: t sweep_id=step-judge-4-1 item=4 step=judge ====\n\
         LOOM_STEP_OUTCOME role=judge target=4 class=failed reason=crash\n\
         ==== loom-daemon dispatch: t sweep_id=step-judge-4-2 item=4 step=judge ====\n\
         LOOM_STEP_OUTCOME role=judge target=4 class=noop reason=already_judged\n",
    )
    .unwrap();
    assert_eq!(read_outcome_line(&log, "step-judge-4-2").unwrap().class, StepOutcomeClass::Noop);
    assert_eq!(
        read_outcome_line(&log, "step-judge-4-1").unwrap().class,
        StepOutcomeClass::Noop,
        "the earlier run's anchor reads to the end of the log"
    );
    assert!(read_outcome_line(&log, "step-judge-4-3").is_none());
}

#[test]
fn a_launch_rejection_is_spent_only_on_a_genuine_exit_78_without_a_line() {
    let reported = Some("preflight_role_admission");
    let end = StepEnd::Exited(Some(78));
    assert_eq!(launch_rejection_of(end, None, reported), reported);
    let line =
        parse_outcome_line("LOOM_STEP_OUTCOME role=builder target=2 class=failed reason=no_pr");
    assert_eq!(launch_rejection_of(end, line.as_ref(), reported), None);
    assert_eq!(launch_rejection_of(end, None, None), None);
    for other in [
        StepEnd::TimedOut,
        StepEnd::Cancelled,
        StepEnd::Exited(None),
        StepEnd::Exited(Some(1)),
    ] {
        assert_eq!(launch_rejection_of(other, None, reported), None);
    }
}

#[test]
fn a_steps_end_maps_to_the_role_ticks_outcome() {
    let noop = parse_outcome_line("LOOM_STEP_OUTCOME role=c target=1 class=noop reason=x");
    let failed = parse_outcome_line("LOOM_STEP_OUTCOME role=c target=1 class=failed reason=x");
    let verdict = parse_outcome_line("LOOM_STEP_OUTCOME role=c target=1 class=verdict reason=x");
    let judge = StepRole::Judge;
    assert_eq!(
        tick_outcome(judge, StepEnd::Exited(Some(0)), noop.as_ref()),
        RoleTickOutcome::Success
    );
    assert_eq!(
        tick_outcome(judge, StepEnd::Exited(None), noop.as_ref()),
        RoleTickOutcome::Success
    );
    assert_eq!(tick_outcome(judge, StepEnd::Exited(Some(0)), None), RoleTickOutcome::Success);
    for role in [StepRole::Builder, judge] {
        assert_eq!(
            tick_outcome(role, StepEnd::Exited(Some(0)), verdict.as_ref()),
            RoleTickOutcome::Success,
            "{role:?}"
        );
    }
    for (role, end, line) in [
        (judge, StepEnd::Exited(Some(0)), failed.as_ref()),
        (judge, StepEnd::Exited(Some(1)), None),
        (judge, StepEnd::Exited(None), None),
        (judge, StepEnd::TimedOut, noop.as_ref()),
        (judge, StepEnd::Cancelled, None),
        (StepRole::Builder, StepEnd::Exited(Some(0)), noop.as_ref()),
        (StepRole::Builder, StepEnd::Exited(None), noop.as_ref()),
        (StepRole::Builder, StepEnd::Exited(Some(0)), None),
    ] {
        assert!(
            matches!(tick_outcome(role, end, line), RoleTickOutcome::Failure(_)),
            "{role:?} {end:?} {line:?}"
        );
    }
}

/// #12500: a Builder step that exits 0 with no PR is not a success, and its
/// failure text carries the line's reason.
#[test]
fn a_builder_that_opens_no_pr_reports_no_pr_with_its_reason() {
    let noop =
        parse_outcome_line("LOOM_STEP_OUTCOME role=builder target=1 class=noop reason=ci_pending");
    assert_eq!(
        tick_outcome(StepRole::Builder, StepEnd::Exited(Some(0)), noop.as_ref()),
        RoleTickOutcome::Failure("no_pr: ci_pending".to_string())
    );
    let class = |line: Option<&_>| {
        step_failure(StepRole::Builder, StepEnd::Exited(Some(0)), line, false)
            .map(|f| f.class().to_string())
    };
    assert_eq!(class(noop.as_ref()).as_deref(), Some("step-no-pr"));
    assert_eq!(class(None).as_deref(), Some("step-no-outcome-line"));
    assert_eq!(
        step_failure(StepRole::Judge, StepEnd::Exited(Some(0)), noop.as_ref(), false)
            .map(|f| f.class().to_string()),
        None
    );
}

/// #12480: every failing branch of `tick_outcome` has its own closed class,
/// and `step_failure` is `Some` exactly when `tick_outcome` fails.
#[test]
fn a_failed_steps_end_maps_to_its_own_class() {
    let noop = parse_outcome_line("LOOM_STEP_OUTCOME role=c target=1 class=noop reason=x");
    let failed = parse_outcome_line("LOOM_STEP_OUTCOME role=c target=1 class=failed reason=a b");
    let judge = StepRole::Judge;
    let class = |end, line: Option<&_>| {
        step_failure(judge, end, line, false).map(|f| f.class().to_string())
    };
    assert_eq!(class(StepEnd::TimedOut, noop.as_ref()).as_deref(), Some("step-timed-out"));
    assert_eq!(class(StepEnd::Cancelled, None).as_deref(), Some("step-cancelled"));
    assert_eq!(
        class(StepEnd::Exited(Some(0)), failed.as_ref()).as_deref(),
        Some("step-outcome-failed")
    );
    assert_eq!(class(StepEnd::Exited(Some(7)), None).as_deref(), Some("step-exit-nonzero"));
    assert_eq!(
        step_failure(judge, StepEnd::Exited(Some(7)), None, false)
            .unwrap()
            .exit_code(),
        Some(7)
    );
    assert_eq!(class(StepEnd::Exited(None), None).as_deref(), Some("step-status-unknown"));
    assert_eq!(class(StepEnd::Exited(Some(0)), noop.as_ref()), None);
    assert_eq!(class(StepEnd::Exited(None), noop.as_ref()), None);
    for role in [judge, StepRole::Builder] {
        for end in [
            StepEnd::Exited(Some(0)),
            StepEnd::Exited(Some(2)),
            StepEnd::Exited(None),
            StepEnd::TimedOut,
            StepEnd::Cancelled,
        ] {
            for line in [None, noop.as_ref(), failed.as_ref()] {
                assert_eq!(
                    step_failure(role, end, line, false).is_some(),
                    matches!(tick_outcome(role, end, line), RoleTickOutcome::Failure(_)),
                    "{role:?} {end:?} {line:?}"
                );
            }
        }
    }
}

/// #13966: a step whose provider feedback said its api-key seat was
/// exhausted carries the exhaustion class instead of its exit shape — the
/// launch never had a working credential — but a cancelled step keeps the
/// operator's own class, and a successful end stays successful.
#[test]
fn a_provider_exhausted_step_carries_the_exhaustion_class() {
    let class = |exhausted, end, line: Option<&_>| {
        step_failure(StepRole::Builder, end, line, exhausted).map(|f| f.class().to_string())
    };
    assert_eq!(
        class(true, StepEnd::Exited(Some(1)), None).as_deref(),
        Some("step-provider-exhausted")
    );
    // The exit code still rides `exit_code`, never the class.
    assert_eq!(
        step_failure(StepRole::Builder, StepEnd::Exited(Some(1)), None, true)
            .unwrap()
            .exit_code(),
        Some(1)
    );
    // Even a line the session did write, and a timeout, reclass to it.
    let failed = parse_outcome_line("LOOM_STEP_OUTCOME role=builder target=1 class=failed");
    assert_eq!(
        class(true, StepEnd::Exited(Some(1)), failed.as_ref()).as_deref(),
        Some("step-provider-exhausted")
    );
    assert_eq!(class(true, StepEnd::TimedOut, None).as_deref(), Some("step-provider-exhausted"));
    // A cancel is the operator's act; a Judge's clean exit is not a failure
    // (a Builder's clean exit without a line is one, #12500, and reclasses).
    assert_eq!(class(true, StepEnd::Cancelled, None).as_deref(), Some("step-cancelled"));
    let judge_class = |exhausted, end| {
        super::capture::step_failure(StepRole::Judge, end, None, exhausted)
            .map(|f| f.class().to_string())
    };
    assert_eq!(judge_class(true, StepEnd::Exited(Some(0))), None);
    assert_eq!(
        class(true, StepEnd::Exited(Some(0)), None).as_deref(),
        Some("step-provider-exhausted")
    );
    // Without the evidence the shapes are unchanged.
    assert_eq!(
        class(false, StepEnd::Exited(Some(1)), None).as_deref(),
        Some("step-exit-nonzero")
    );
}

/// #13966: the bridge-to-end hand-off is a take-once read of a bounded map.
#[test]
fn a_noted_provider_exhaustion_is_taken_once() {
    let id = "step-builder-13966-take-once";
    assert!(!super::capture::take_step_provider_exhaustion(id), "nothing noted yet");
    super::capture::note_step_provider_exhaustion(id);
    assert!(super::capture::take_step_provider_exhaustion(id), "the end reads it once");
    assert!(!super::capture::take_step_provider_exhaustion(id), "and it is gone");
}

/// Capture parity with role ticks (#11877): a step's end writes the same
/// `role_tick.outcome` record a role tick of that role writes.
#[test]
#[serial]
fn a_steps_end_writes_the_role_ticks_outcome_record() {
    let dir = tempdir().unwrap();
    std::env::remove_var(crate::sweep_outcomes::ROLE_TICK_TELEMETRY_JOURNAL_PATH_ENV);
    emit_step_capture(&StepCapture {
        runtime: None,
        root: dir.path().to_path_buf(),
        role: StepRole::ChampionMerge,
        item: 8,
        sweep_id: "step-champion_merge-8-1".into(),
        trace_context: None,
        started_at: Utc::now() - chrono::Duration::seconds(30),
        outcome: RoleTickOutcome::Success,
        step_outcome: Some(("noop".into(), "already_judged".into())),
        failure: None,
        model: "opus".into(),
        effort: "high".into(),
        stage: "champion_merge".into(),
        log_path: dir.path().join("step-champion_merge-8.log"),
    });
    let journal = crate::sweep_outcomes::default_role_tick_telemetry_path(dir.path());
    let records = crate::sweep_outcomes::read_all_role_tick_outcomes(&journal);
    assert_eq!(records.len(), 1, "{journal:?}");
    assert_eq!(records[0].role, "champion");
    assert_eq!(records[0].model.as_deref(), Some("opus"));
    assert_eq!(records[0].source.as_deref(), Some(crate::telemetry::ROLE_TICK_SOURCE_STEP));
    assert_eq!(records[0].sweep_id.as_deref(), Some("step-champion_merge-8-1"));
    // #12362: the class and reason `tick_outcome` collapses ride additively.
    assert_eq!(records[0].step_class.as_deref(), Some("noop"));
    assert_eq!(records[0].step_reason.as_deref(), Some("already_judged"));
    assert_eq!(records[0].result, crate::telemetry::RoleTickResult::Success);
}

/// #14168: a step that wrote no parseable outcome line records
/// `step_class="none"` on its `role_tick.outcome` record — "no outcome" is
/// countable, not an absence — with no `step_reason`.
#[test]
#[serial]
fn a_step_with_no_outcome_line_records_none_on_its_record() {
    let dir = tempdir().unwrap();
    std::env::remove_var(crate::sweep_outcomes::ROLE_TICK_TELEMETRY_JOURNAL_PATH_ENV);
    emit_step_capture(&StepCapture {
        runtime: None,
        root: dir.path().to_path_buf(),
        role: StepRole::Judge,
        item: 9,
        sweep_id: "step-judge-9-1".into(),
        trace_context: None,
        started_at: Utc::now() - chrono::Duration::seconds(30),
        outcome: RoleTickOutcome::Success,
        step_outcome: Some(step_outcome_projection(None)),
        failure: None,
        model: "opus".into(),
        effort: "high".into(),
        stage: "judge".into(),
        log_path: dir.path().join("step-judge-9.log"),
    });
    let journal = crate::sweep_outcomes::default_role_tick_telemetry_path(dir.path());
    let records = crate::sweep_outcomes::read_all_role_tick_outcomes(&journal);
    assert_eq!(records.len(), 1, "{journal:?}");
    assert_eq!(records[0].step_class.as_deref(), Some("none"));
    assert_eq!(records[0].step_reason.as_deref(), None);
}

/// Review item 4: a step found dead at startup runs the step end too, so the
/// leases it listed are reaped and its lease file is not left behind.
#[test]
#[serial]
fn a_step_found_dead_at_startup_runs_its_end() {
    let dir = tempdir().unwrap();
    let mut reg =
        crate::sweep_registry::test_support::fixture_registry_with_lease_gh(dir.path(), "", 0);
    let id = "step-judge-31-1";
    let file = crate::pr_lease_reap::path_for(dir.path(), id);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, format!("31 judge host-a {id}\n")).unwrap();
    reg.restore_dead_step_at_startup(31, StepRole::Judge, id, Utc::now());
    assert!(
        crate::sweep_registry::test_support::wait_for_condition(30_000, || !file.exists()),
        "the dead step's lease file is reaped and removed"
    );
}

/// Only a Builder step's child is told the issue claim is its invoker's:
/// every other step holds the role-keyed lease alone.
#[test]
#[serial]
fn only_a_builder_step_names_its_item_as_the_owned_claim() {
    let dir = tempdir().unwrap();
    let (mut reg, record) = fixture_registry(dir.path());
    let builder = reg
        .spawn_step(&StepSpawnRequest::new(9_105, StepRole::Builder))
        .unwrap();
    let recorded = assert_child_wrote(&record, &format!("LOOM_SWEEP_ID={}", builder.sweep_id));
    assert!(recorded.contains("LOOM_SWEEP_CLAIM_OWNED=9105"), "{recorded}");
    let curator = reg
        .spawn_step(&StepSpawnRequest::new(9_106, StepRole::Curator))
        .unwrap();
    let recorded = assert_child_wrote(&record, &format!("LOOM_SWEEP_ID={}", curator.sweep_id));
    // This launch's own record: from its `argv:` line to its sweep id.
    let block = recorded[..recorded
        .find(&format!("LOOM_SWEEP_ID={}", curator.sweep_id))
        .unwrap()]
        .rsplit("argv:")
        .next()
        .unwrap_or_default();
    assert!(block.contains("LOOM_SWEEP_CLAIM_OWNED=unset"), "{block}");
    cancel_all(&mut reg);
}

// ---- workspace role command (#12048) ----------------------------------------

/// A non-hermetic registry refuses a step whose `/loom:<role>` command is not
/// installed, before any lock; once the file exists that refusal stops.
#[test]
#[serial]
fn a_step_without_its_role_command_is_refused_before_any_lock() {
    let dir = tempdir().unwrap();
    let (mut reg, _record) = fixture_registry(dir.path());
    reg.config.skip_label_flip = false;
    let request = StepSpawnRequest::new(9_200, StepRole::Judge);

    let (result, signals) = capture(|| reg.spawn_step(&request));
    let refusal = result.unwrap_err();
    let refusal = refusal.downcast_ref::<StepRefusal>().unwrap();
    assert_eq!(refusal.reason, "workspace_commands_missing");
    assert!(!reg.config.locks_dir().join("pr-9200").exists(), "no lock may be left behind");
    assert!(reg.entries.is_empty());
    assert_eq!(
        step_span_reason(&signals.spans),
        vec![("refused".into(), "workspace_commands_missing".into())]
    );

    // A second refusal records the span again but does not WARN again.
    assert!(reg.missing_command_warned.contains("judge"));
    let _ = reg.spawn_step(&request).unwrap_err();
    assert_eq!(reg.missing_command_warned.len(), 1);

    let commands = dir.path().join(".claude").join("commands").join("loom");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(commands.join("judge.md"), "# judge").unwrap();
    // The non-hermetic fixture has no credential pool, so the step may still be
    // refused further on; the guard owns only that it is no longer refused for
    // the missing command and that its WARN re-arms.
    if let Err(err) = reg.spawn_step(&request) {
        let refusal = err.downcast_ref::<StepRefusal>().unwrap();
        assert_ne!(refusal.reason, "workspace_commands_missing");
    }
    assert!(reg.missing_command_warned.is_empty(), "the WARN re-arms once the file is seen");
    cancel_all(&mut reg);
}

/// Champion's promote and merge steps share `champion.md`.
#[test]
fn champion_steps_resolve_the_same_command_file() {
    let dir = tempdir().unwrap();
    let config = SweepRegistryConfig::new(dir.path().to_path_buf());
    assert_eq!(
        config.role_command_path(StepRole::ChampionPromote),
        config.role_command_path(StepRole::ChampionMerge)
    );
    assert!(config.role_command_missing(StepRole::ChampionMerge));
}

// ---- #12133: the Builder claim is claim-then-verify and fails closed ------

const BUILDER_ITEM: u32 = 12133;
const PEER_LEASE: &str = "<!-- loom:lease host=peer-host sweep=sweep-peer -->";

fn builder_claim(reg: &mut SweepRegistry, sweep: &str) -> std::result::Result<(), StepRefusal> {
    // A Builder's claim is a label flip and a lease comment: it has no
    // `lease pr claim` facts to carry (#12183).
    reg.pre_claim_step(BUILDER_ITEM, StepRole::Builder, sweep)
        .map(|claim| assert!(claim.is_none(), "a Builder claim carries no facts"))
}

/// Make the fake `gh`'s `arm` (e.g. the comment read) exit 1 straight away.
fn break_gh_arm(ws: &Path, arm: &str) {
    let fake = ws.join("fake-gh.sh");
    let script = std::fs::read_to_string(&fake).unwrap();
    assert!(script.contains(arm), "harness arm changed shape");
    std::fs::write(&fake, script.replacen(arm, &format!("{arm}exit 1\n"), 1)).unwrap();
}

const COMMENT_READ_ARM: &str = "if [[ \"$1\" == \"api\" && \"$*\" == *\"/comments\"* ]]; then\n";
const COMMENT_WRITE_ARM: &str = "if [[ \"$1\" == \"issue\" && \"$2\" == \"comment\" ]]; then\n";

/// The one claimant, with no peer lease anywhere, spawns.
#[test]
#[serial]
fn a_sole_builder_claim_is_verified_and_proceeds() {
    let dir = tempdir().unwrap();
    let (mut reg, ..) =
        crate::sweep_registry::test_support::lease_order_dispatch_registry(dir.path(), &[]);
    assert_eq!(builder_claim(&mut reg, "step-builder-1-1"), Ok(()));
}

/// A peer's earlier live lease wins: the label flip alone no longer claims.
#[test]
#[serial]
fn a_builder_claim_behind_a_peer_lease_is_lost() {
    let dir = tempdir().unwrap();
    let (mut reg, ..) = crate::sweep_registry::test_support::lease_order_dispatch_registry(
        dir.path(),
        &[PEER_LEASE],
    );
    assert_eq!(
        builder_claim(&mut reg, "step-builder-1-1")
            .unwrap_err()
            .reason,
        "claim_lost"
    );
}

/// Two hosts racing one item on one forge comment list: exactly one claim.
#[test]
#[serial]
fn two_hosts_racing_one_item_claim_it_once() {
    use crate::sweep_registry::test_support::lease_order_dispatch_registry;
    let (dir_a, dir_b) = (tempdir().unwrap(), tempdir().unwrap());
    let (mut reg_a, _, _, store) = lease_order_dispatch_registry(dir_a.path(), &[]);
    let (mut reg_b, _, _, store_b) = lease_order_dispatch_registry(dir_b.path(), &[]);
    let fake_b = dir_b.path().join("fake-gh.sh");
    let script = std::fs::read_to_string(&fake_b).unwrap();
    std::fs::write(
        &fake_b,
        script.replace(&store_b.display().to_string(), &store.display().to_string()),
    )
    .unwrap();

    let results = [
        builder_claim(&mut reg_a, "step-builder-1-1"),
        builder_claim(&mut reg_b, "step-builder-1-2"),
    ];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{results:?}");
    assert_eq!(results[1].as_ref().unwrap_err().reason, "claim_lost");
}

/// An unreadable forge is not "no peer": the step is refused, not spawned.
#[test]
#[serial]
fn an_unreadable_lease_order_refuses_the_builder_claim() {
    let dir = tempdir().unwrap();
    let (mut reg, ..) =
        crate::sweep_registry::test_support::lease_order_dispatch_registry(dir.path(), &[]);
    break_gh_arm(dir.path(), COMMENT_READ_ARM);
    assert_eq!(
        builder_claim(&mut reg, "step-builder-1-1")
            .unwrap_err()
            .reason,
        "claim_unverified"
    );
}

/// A lease comment that cannot be written leaves no mutex: refused.
#[test]
#[serial]
fn an_unwritable_lease_comment_refuses_the_builder_claim() {
    let dir = tempdir().unwrap();
    let (mut reg, ..) =
        crate::sweep_registry::test_support::lease_order_dispatch_registry(dir.path(), &[]);
    break_gh_arm(dir.path(), COMMENT_WRITE_ARM);
    assert_eq!(
        builder_claim(&mut reg, "step-builder-1-1")
            .unwrap_err()
            .reason,
        "claim_unverified"
    );
}

/// The lease is written but never reads back (stale read): refused.
#[test]
#[serial]
fn a_lease_that_never_reads_back_refuses_the_builder_claim() {
    let dir = tempdir().unwrap();
    let (mut reg, _, _, store) =
        crate::sweep_registry::test_support::lease_order_dispatch_registry(dir.path(), &[]);
    std::fs::remove_file(&store).unwrap();
    std::fs::create_dir(&store).unwrap(); // appends fail; the read finds nothing
    assert_eq!(
        builder_claim(&mut reg, "step-builder-1-1")
            .unwrap_err()
            .reason,
        "claim_unverified"
    );
}

// ---- #12822: a refused Builder claim rolls back what it wrote -------------

/// The refusal's `loom.dispatch.claim_rollback` token.
fn rollback_of(refusal: &StepRefusal) -> Option<&'static str> {
    refusal.claim.as_deref().and_then(|c| c.rollback)
}

/// The fake forge's stored comment bodies (first lines).
fn stored_comments(store: &Path) -> String {
    std::fs::read_to_string(store).unwrap_or_default()
}

fn gh_edit_calls(ws: &Path) -> String {
    std::fs::read_to_string(ws.join("gh-invocations.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("issue edit"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Behind a peer's lease the step yields its own lease and leaves the peer's
/// lease and the label (the winner's claim) alone.
#[test]
#[serial]
fn a_claim_lost_builder_yields_its_own_lease_and_keeps_the_peers_label() {
    let dir = tempdir().unwrap();
    let (mut reg, _, _, store) = crate::sweep_registry::test_support::lease_order_dispatch_registry(
        dir.path(),
        &[PEER_LEASE],
    );
    let refusal = builder_claim(&mut reg, "step-builder-1-1").unwrap_err();
    assert_eq!(refusal.reason, "claim_lost");
    assert_eq!(rollback_of(&refusal), Some("lease_yielded"));
    let comments = stored_comments(&store);
    assert!(
        comments.contains("loom:lease-yield host=") && comments.contains("sweep=step-builder-1-1"),
        "{comments}"
    );
    assert!(comments.contains("earliest_sweep=sweep-peer"), "{comments}");
    assert!(comments.contains(PEER_LEASE), "the peer's lease is untouched: {comments}");
    assert!(
        !gh_edit_calls(dir.path()).contains("--remove-label loom:building"),
        "the winner's label stays: {}",
        gh_edit_calls(dir.path())
    );
}

/// An unreadable order yields the step's lease and, failing closed, leaves
/// the label.
#[test]
#[serial]
fn an_unverified_builder_claim_yields_its_lease_and_keeps_the_label() {
    let dir = tempdir().unwrap();
    let (mut reg, _, _, store) =
        crate::sweep_registry::test_support::lease_order_dispatch_registry(dir.path(), &[]);
    break_gh_arm(dir.path(), COMMENT_READ_ARM);
    let refusal = builder_claim(&mut reg, "step-builder-1-1").unwrap_err();
    assert_eq!(refusal.reason, "claim_unverified");
    assert_eq!(rollback_of(&refusal), Some("lease_yielded"));
    assert!(stored_comments(&store).contains("loom:lease-yield host="));
    assert!(!gh_edit_calls(dir.path()).contains("--remove-label loom:building"));
}

/// A lease that could not be written has nothing to yield; with no peer lease
/// live the label this step flipped is restored.
#[test]
#[serial]
fn an_unwritable_builder_lease_restores_the_label_when_no_peer_is_live() {
    let dir = tempdir().unwrap();
    let (mut reg, ..) =
        crate::sweep_registry::test_support::lease_order_dispatch_registry(dir.path(), &[]);
    break_gh_arm(dir.path(), COMMENT_WRITE_ARM);
    let refusal = builder_claim(&mut reg, "step-builder-1-1").unwrap_err();
    assert_eq!(refusal.reason, "claim_unverified");
    assert_eq!(rollback_of(&refusal), Some("label_restored"));
    assert!(gh_edit_calls(dir.path()).contains("--remove-label loom:building"));
}

/// The same unwritable lease behind a live peer lease keeps the label.
#[test]
#[serial]
fn an_unwritable_builder_lease_keeps_the_label_behind_a_peer() {
    let dir = tempdir().unwrap();
    let (mut reg, ..) = crate::sweep_registry::test_support::lease_order_dispatch_registry(
        dir.path(),
        &[PEER_LEASE],
    );
    break_gh_arm(dir.path(), COMMENT_WRITE_ARM);
    let refusal = builder_claim(&mut reg, "step-builder-1-1").unwrap_err();
    assert_eq!(rollback_of(&refusal), Some("none"));
    assert!(!gh_edit_calls(dir.path()).contains("--remove-label loom:building"));
}

/// A yield that cannot be posted is recorded `failed`; the refusal is still
/// the original one.
#[test]
#[serial]
fn a_failed_rollback_still_returns_the_original_refusal() {
    let dir = tempdir().unwrap();
    let (mut reg, ..) = crate::sweep_registry::test_support::lease_order_dispatch_registry(
        dir.path(),
        &[PEER_LEASE],
    );
    // The lease write succeeds; the yield (the next comment write) fails.
    let fake = dir.path().join("fake-gh.sh");
    let script = std::fs::read_to_string(&fake).unwrap();
    std::fs::write(
        &fake,
        script.replacen(
            COMMENT_WRITE_ARM,
            &format!("{COMMENT_WRITE_ARM}if [[ \"$*\" == *lease-yield* ]]; then exit 1; fi\n"),
            1,
        ),
    )
    .unwrap();
    let refusal = builder_claim(&mut reg, "step-builder-1-1").unwrap_err();
    assert_eq!(refusal.reason, "claim_lost");
    assert_eq!(rollback_of(&refusal), Some("failed"));
}

// ---- #14482: a terminal Builder yields only its own issue lease -----------

mod builder_terminal_cleanup {
    use super::*;
    use crate::forge_check_claim::{self as guard, BranchLeg, ClaimEvidence};
    use crate::worktree_ops::gh::MergedBranchPrProbe;

    const MINE: &str = "step-builder-12133-terminal";

    /// Extend the existing claim fixture with persisted labels and yield reads.
    /// The cleanup and admission readers consume the same forge state.
    fn fixture(ws: &Path) -> (SweepRegistry, PathBuf) {
        let (reg, _, _, store) =
            crate::sweep_registry::test_support::lease_order_dispatch_registry(ws, &[]);
        std::fs::write(ws.join("labels.json"), r#"["loom:issue"]"#).unwrap();
        let fake = ws.join("fake-gh.sh");
        let script = std::fs::read_to_string(&fake)
            .unwrap()
            .replace("[[ \"$line\" != *'\"body\":\"<!-- loom:lease host='* ]] && continue\n", "");
        let prefix = format!(
            r#"#!/usr/bin/env bash
labels='{root}/labels.json'
if [[ "$1 $2" == 'api graphql' && -f '{root}/open-pr' ]]; then
  echo '{{"data":{{"repository":{{"issue":{{"closedByPullRequestsReferences":{{"nodes":[{{"number":999,"state":"OPEN"}}]}}}}}}}}}}'
  exit 0
fi
if [[ "$1 $2" == 'issue edit' ]]; then
  printf '%s\n' "$*" >> '{root}/gh-invocations.log'
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --remove-label) shift; jq --arg l "$1" 'map(select(. != $l))' "$labels" > "$labels.next"; mv "$labels.next" "$labels" ;;
      --add-label) shift; jq --arg l "$1" '. + [$l] | unique' "$labels" > "$labels.next"; mv "$labels.next" "$labels" ;;
    esac
    shift
  done
  exit 0
fi
if [[ "$1 $2" == 'issue view' ]]; then
  [[ -f '{root}/unreadable' ]] && exit 1
  jq '{{state:"OPEN",url:"https://github.com/rjwalters/loom/issues/12133",labels:map({{name:.}})}}' "$labels"
  exit 0
fi
if [[ "$1" == api && "$*" == *closed_at* ]]; then
  [[ -f '{root}/unreadable' ]] && exit 1
  jq '{{state:"open",body:"",closed_at:null,labels:.}}' "$labels"
  exit 0
fi
if [[ "$1" == api && "$2" == */issues/12133 && "$*" == *'.pull_request != null'* ]]; then echo false; exit 0; fi
"#,
            root = ws.display(),
        );
        std::fs::write(fake, prefix + &script).unwrap();
        (reg, store)
    }

    /// The public check-claim verdict, using its real label/lease readers and
    /// the registry's linked-PR probe. This fixture has no merged PR or branch.
    fn admission(reg: &SweepRegistry) -> guard::Verdict {
        let root = &reg.config.workspace_root;
        let gh = reg.resolved_gh();
        guard::decide(
            BUILDER_ITEM,
            &ClaimEvidence {
                open_pr: reg.probe_open_linked_pr(BUILDER_ITEM),
                merged_pr: Some(MergedBranchPrProbe::NoneMerged),
                labels: Some(guard::read_claim_labels(&gh, root, BUILDER_ITEM)),
                lease: Some(guard::read_freshest_live_lease(
                    &gh,
                    root,
                    BUILDER_ITEM,
                    Utc::now(),
                    15.0,
                )),
                branch: Some(BranchLeg::Absent),
            },
            false,
        )
    }

    fn finish(reg: &mut SweepRegistry) {
        reg.release_dead_step(
            MINE,
            BUILDER_ITEM,
            StepRole::Builder,
            Utc::now(),
            StepEnd::Exited(Some(1)),
        );
    }

    #[test]
    #[serial]
    fn terminal_builder_is_immediately_admissible_and_cleanup_is_idempotent() {
        let dir = tempdir().unwrap();
        let (mut reg, store) = fixture(dir.path());
        builder_claim(&mut reg, MINE).unwrap();
        assert_eq!(admission(&reg).stdout, "BUILDING");
        // Retain a real exited child, using the existing reaper fixture.
        let old_id = crate::sweep_registry::test_support::insert_clean_exit_running(
            &mut reg,
            BUILDER_ITEM,
            1,
        );
        let mut entry = reg.entries.remove(&old_id).unwrap();
        entry.sweep_id = MINE.into();
        entry.kind = SweepKind::Step {
            item: BUILDER_ITEM,
            role: StepRole::Builder,
        };
        entry.log_path = reg.compute_step_log_path(BUILDER_ITEM, StepRole::Builder);
        reg.entries.insert(MINE.into(), entry);
        let child = reg.children.remove(&old_id).unwrap();
        reg.children.insert(MINE.into(), child);
        assert_eq!(reg.reap_once(), 1);
        assert!(reg.entries[MINE].state.is_terminal());
        let verdict = admission(&reg);
        assert_eq!(verdict.code, guard::EX_SAFE_TO_CLAIM, "{verdict:?}");
        let comments = stored_comments(&store);
        assert!(comments
            .contains(&format!("loom:lease-yield host={} sweep={MINE}", reg.published_host_id())));
        finish(&mut reg);
        assert_eq!(stored_comments(&store), comments, "repeat cleanup posts no duplicate yield");
        assert_eq!(reg.reap_once(), 0);
        assert_eq!(admission(&reg).code, guard::EX_SAFE_TO_CLAIM);
    }

    #[test]
    #[serial]
    fn won_builder_claim_spawn_failure_yields_its_lease() {
        let dir = tempdir().unwrap();
        let (mut reg, _) = fixture(dir.path());
        builder_claim(&mut reg, MINE).unwrap();
        reg.unwind_step_claim(BUILDER_ITEM, StepRole::Builder, MINE, true);
        let verdict = admission(&reg);
        assert_eq!(verdict.code, guard::EX_SAFE_TO_CLAIM, "{verdict:?}");
    }

    #[test]
    #[serial]
    fn terminal_builder_never_yields_a_newer_peer_lease() {
        let dir = tempdir().unwrap();
        let (mut reg, store) = fixture(dir.path());
        builder_claim(&mut reg, MINE).unwrap();
        let original = stored_comments(&store);
        let peer = original
            .lines()
            .next()
            .unwrap()
            .replace(MINE, "new-peer")
            .replace(&reg.published_host_id(), "peer-host");
        std::fs::write(&store, format!("{original}{peer}\n")).unwrap();
        finish(&mut reg);
        let verdict = admission(&reg);
        assert_eq!(verdict.code, 0, "{verdict:?}");
        assert!(verdict.stdout.ends_with(" new-peer"), "{verdict:?}");
        assert!(!stored_comments(&store).contains("lease-yield host=peer-host sweep=new-peer"));
    }

    #[test]
    #[serial]
    fn failed_terminal_yield_keeps_the_fresh_lease_effective() {
        let dir = tempdir().unwrap();
        let (mut reg, store) = fixture(dir.path());
        builder_claim(&mut reg, MINE).unwrap();
        break_gh_arm(dir.path(), COMMENT_WRITE_ARM);
        finish(&mut reg);
        let verdict = admission(&reg);
        assert_eq!(verdict.code, 0, "{verdict:?}");
        assert!(verdict.stdout.ends_with(MINE), "{verdict:?}");
        assert!(!stored_comments(&store).contains("loom:lease-yield"));
        let calls = std::fs::read_to_string(dir.path().join("gh-invocations.log")).unwrap();
        assert!(
            calls
                .lines()
                .any(|l| l.starts_with("issue comment") && l.contains("loom:lease-yield")),
            "the failed write was attempted"
        );
        assert!(
            !reg.yield_finished_builder_lease(BUILDER_ITEM, MINE, "exited"),
            "the failed yield is reported, not treated as success"
        );
    }

    #[test]
    #[serial]
    fn unreadable_terminal_lease_history_is_not_a_successful_yield() {
        let dir = tempdir().unwrap();
        let (mut reg, store) = fixture(dir.path());
        builder_claim(&mut reg, MINE).unwrap();
        break_gh_arm(dir.path(), COMMENT_READ_ARM);
        assert!(!reg.yield_finished_builder_lease(BUILDER_ITEM, MINE, "exited"));
        finish(&mut reg);
        assert!(!stored_comments(&store).contains("loom:lease-yield"));
        assert_eq!(admission(&reg).code, guard::EX_PROBE_FAILED);
    }

    #[test]
    #[serial]
    fn an_untrusted_yield_does_not_suppress_terminal_cleanup() {
        let dir = tempdir().unwrap();
        let (mut reg, store) = fixture(dir.path());
        builder_claim(&mut reg, MINE).unwrap();
        let original = stored_comments(&store);
        let mut forged: serde_json::Value =
            serde_json::from_str(original.lines().next().unwrap()).unwrap();
        forged["user"] = serde_json::json!({"login":"outsider", "type":"User"});
        forged["body"] =
            format!("<!-- loom:lease-yield host={} sweep={MINE} -->", reg.published_host_id())
                .into();
        std::fs::write(&store, format!("{original}{forged}\n")).unwrap();
        finish(&mut reg);
        assert_eq!(stored_comments(&store).matches("loom:lease-yield").count(), 2);
        assert_eq!(admission(&reg).code, guard::EX_SAFE_TO_CLAIM);
    }

    #[test]
    #[serial]
    fn terminal_cleanup_preserves_open_pr_hold_and_unreadable_vetoes() {
        for (veto, spawn_failed) in ["open-pr", "loom:frozen", "loom:operator", "unreadable"]
            .into_iter()
            .flat_map(|veto| [(veto, false), (veto, true)])
        {
            let dir = tempdir().unwrap();
            let (mut reg, _) = fixture(dir.path());
            builder_claim(&mut reg, MINE).unwrap();
            match veto {
                "open-pr" => std::fs::write(dir.path().join("open-pr"), "").unwrap(),
                "unreadable" => std::fs::write(dir.path().join("unreadable"), "").unwrap(),
                label => std::fs::write(
                    dir.path().join("labels.json"),
                    format!(r#"["loom:building","{label}"]"#),
                )
                .unwrap(),
            }
            if spawn_failed {
                reg.unwind_step_claim(BUILDER_ITEM, StepRole::Builder, MINE, true);
            } else {
                finish(&mut reg);
            }
            let labels = std::fs::read_to_string(dir.path().join("labels.json")).unwrap();
            assert!(!labels.contains("loom:issue"), "{veto}: {labels}");
            assert_ne!(admission(&reg).code, guard::EX_SAFE_TO_CLAIM, "{veto}");
        }
    }
}

// ---- #12450: the captain's claim ------------------------------------------

mod captain {
    use super::*;
    use crate::fleet_dispatch::proto::{CLAIM_PATH, REFUSE_PATH, RELEASE_PATH};
    use crate::fleet_dispatch::test_server::TestServer;
    use serde_json::json;

    fn enable(root: &Path, url: &str) {
        write_config(root, &json!({"fleetDispatch": {"enabled": true, "url": url}}).to_string());
    }

    fn server(claim: serde_json::Value) -> TestServer {
        TestServer::start(move |path, _| {
            if path == CLAIM_PATH {
                (200, claim.clone())
            } else {
                (200, json!({"server_epoch": "e1"}))
            }
        })
    }

    fn held() -> TestServer {
        server(json!({"server_epoch":"e1","granted":false,"reason":"held","held_by":"peer",
            "retry_after_s":30}))
    }

    fn granted() -> TestServer {
        server(json!({"server_epoch":"e1","granted":true,"assignment_id":"a-12450","ttl_s":1200}))
    }

    /// A non-hermetic Builder registry whose fake `gh` logs every call.
    fn forge_registry(ws: &Path, leases: &[&str]) -> (SweepRegistry, PathBuf) {
        let (reg, gh_log, ..) =
            crate::sweep_registry::test_support::lease_order_dispatch_registry(ws, leases);
        let commands = ws.join(".claude").join("commands").join("loom");
        std::fs::write(commands.join("builder.md"), "# builder").unwrap();
        (reg, gh_log)
    }

    fn gh_calls(log: &Path) -> String {
        std::fs::read_to_string(log).unwrap_or_default()
    }

    /// A refused claim is a pass with ZERO forge calls: no landed check, no
    /// label flip, no lease comment, no lock left behind. Both spawn paths.
    #[test]
    #[serial]
    fn a_captain_refusal_makes_no_forge_call() {
        crate::fleet_dispatch::worker::reset_for_test();
        let dir = tempdir().unwrap();
        let (mut reg, gh_log) = forge_registry(dir.path(), &[]);
        let server = held();
        enable(dir.path(), &server.url());
        let request = StepSpawnRequest::new(BUILDER_ITEM, StepRole::Builder);
        let (result, signals) = capture(|| reg.spawn_step(&request));
        let refusal = result.unwrap_err().downcast::<StepRefusal>().unwrap();
        assert_eq!(refusal.reason, "captain_held");
        assert_eq!(gh_calls(&gh_log), "", "a refusal makes no forge call");
        assert!(reg.entries.is_empty());
        assert!(!reg
            .config
            .locks_dir()
            .join(format!("issue-{BUILDER_ITEM}"))
            .exists());
        assert_eq!(
            step_span_reason(&signals.spans),
            vec![("refused".into(), "captain_held".into())]
        );
        assert_eq!(server.requests_to(CLAIM_PATH).len(), 1);

        let registry = Arc::new(Mutex::new(reg));
        let refusal = SweepRegistry::spawn_step_unlocked(&registry, &request)
            .unwrap_err()
            .downcast::<StepRefusal>()
            .unwrap();
        assert_eq!(refusal.reason, "captain_held");
        assert_eq!(gh_calls(&gh_log), "", "nor on the unlocked path");
        assert!(server.requests_to(REFUSE_PATH).is_empty(), "nothing was granted to give back");
    }

    /// A grant runs today's path unchanged: the forge is asked as before.
    /// A refusal further on gives the grant back with its own reason, and
    /// names the assignment on the step span.
    #[test]
    #[serial]
    fn a_grant_runs_todays_path_and_a_later_refusal_gives_it_back() {
        crate::fleet_dispatch::worker::reset_for_test();
        let dir = tempdir().unwrap();
        // A peer's earlier lease: today's forge claim loses the race.
        let (mut reg, gh_log) = forge_registry(dir.path(), &[PEER_LEASE]);
        // A per-repo credential pool, so the launch resolves on any host and
        // the step reaches the forge pre-claim (without one, a host with no
        // pool is refused `no_token_pool` before the claim is ever made).
        crate::sweep_registry::test_support::seed_token_pool(dir.path(), "agent-1");
        let server = granted();
        enable(dir.path(), &server.url());
        let request = StepSpawnRequest::new(BUILDER_ITEM, StepRole::Builder);
        let (result, signals) = capture(|| reg.spawn_step(&request));
        let refusal = result.unwrap_err().downcast::<StepRefusal>().unwrap();
        assert_ne!(gh_calls(&gh_log), "", "a grant runs the forge path");
        assert_eq!(refusal.assignment_id.as_deref(), Some("a-12450"));
        let refused = server.wait_for(REFUSE_PATH, 1);
        assert_eq!(refused.len(), 1, "{:?}", server.requests());
        assert_eq!(refused[0]["assignment_id"], "a-12450");
        // The peer's lease wins the forge order: the literal token, so a
        // refusal that gave the grant back under some other reason fails.
        assert_eq!(refusal.reason, "claim_lost");
        assert_eq!(refused[0]["reason"], "claim_lost");
        let span = signals
            .spans
            .iter()
            .find(|s| s.name.as_str() == "loom.dispatch.step")
            .unwrap();
        assert_eq!(span.attributes["loom.assignment.id"], "a-12450");
    }

    /// A granted step that spawns carries the assignment on its span and
    /// releases it at its end.
    #[test]
    #[serial]
    fn a_granted_step_names_its_assignment_and_releases_it() {
        crate::fleet_dispatch::worker::reset_for_test();
        let dir = tempdir().unwrap();
        let (mut reg, _) = fixture_registry(dir.path());
        let server = granted();
        enable(dir.path(), &server.url());
        let (outcome, signals) =
            capture(|| reg.spawn_step(&StepSpawnRequest::new(9_450, StepRole::Judge)));
        let outcome = outcome.unwrap();
        let span = signals
            .spans
            .iter()
            .find(|s| s.name.as_str() == "loom.dispatch.step")
            .unwrap();
        assert_eq!(span.attributes["loom.dispatch.admission_result"], "spawned");
        assert_eq!(span.attributes["loom.assignment.id"], "a-12450");
        let _ = reg.end_step(
            &outcome.sweep_id,
            9_450,
            StepRole::Judge,
            Utc::now(),
            StepEnd::Exited(Some(0)),
        );
        let released = server.wait_for(RELEASE_PATH, 1);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0]["assignment_id"], "a-12450");
        assert_eq!(released[0]["outcome"], "failed", "no outcome line reads as failed");
        cancel_all(&mut reg);
    }

    /// #12493: the flag set only in the daemon workspace's host-local tier
    /// reaches a step whose registry is another repo: the step asks the
    /// captain, and carries the grant. With the host tier off, nothing asks.
    #[test]
    #[serial]
    fn the_host_tier_flag_reaches_a_step_in_another_repo() {
        crate::fleet_dispatch::worker::reset_for_test();
        let (host, dir) = (tempdir().unwrap(), tempdir().unwrap());
        let (mut reg, _) = fixture_registry(dir.path());
        let server = granted();
        let local = host.path().join(crate::config_resolver::LOCAL_CONFIG_REL);
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(
            &local,
            json!({"fleetDispatch": {"enabled": true, "url": server.url()}}).to_string(),
        )
        .unwrap();
        crate::fleet_dispatch::worker::set_host_root(host.path());
        let (outcome, signals) =
            capture(|| reg.spawn_step(&StepSpawnRequest::new(9_452, StepRole::Judge)));
        outcome.unwrap();
        let claims = server.requests_to(CLAIM_PATH);
        assert_eq!(claims.len(), 1, "the host's flag must reach this repo's claim");
        assert_eq!(claims[0]["item"], 9_452);
        let span = signals
            .spans
            .iter()
            .find(|s| s.name.as_str() == "loom.dispatch.step")
            .unwrap();
        assert_eq!(span.attributes["loom.assignment.id"], "a-12450");
        cancel_all(&mut reg);

        std::fs::write(
            &local,
            json!({"fleetDispatch": {"enabled": false, "url": server.url()}}).to_string(),
        )
        .unwrap();
        let (outcome, _) =
            capture(|| reg.spawn_step(&StepSpawnRequest::new(9_453, StepRole::Judge)));
        outcome.unwrap();
        assert_eq!(server.requests_to(CLAIM_PATH).len(), 1, "host tier off: no new claim");
        cancel_all(&mut reg);
        crate::fleet_dispatch::worker::reset_for_test();
    }

    /// With the flag off the step spawns exactly as before: no server call.
    #[test]
    #[serial]
    fn with_the_flag_off_the_server_is_never_asked() {
        crate::fleet_dispatch::worker::reset_for_test();
        let dir = tempdir().unwrap();
        let (mut reg, _) = fixture_registry(dir.path());
        let server = held();
        write_config(
            dir.path(),
            &json!({"fleetDispatch": {"enabled": false, "url": server.url()}}).to_string(),
        );
        let (outcome, signals) =
            capture(|| reg.spawn_step(&StepSpawnRequest::new(9_451, StepRole::Judge)));
        outcome.unwrap();
        assert!(server.requests().is_empty());
        let span = signals
            .spans
            .iter()
            .find(|s| s.name.as_str() == "loom.dispatch.step")
            .unwrap();
        assert!(!span.attributes.contains_key("loom.assignment.id"));
        cancel_all(&mut reg);
    }
}

#[test]
fn the_late_seat_busy_release_requires_the_matching_exit_and_outcome() {
    use super::capture::seat_busy_exit_reason;
    use crate::session_exec::refusal::{REFUSAL_EXIT_CODE, SEAT_BUSY_REASON};

    let busy =
        parse_outcome_line("LOOM_STEP_OUTCOME role=judge target=7 class=refused reason=seat_busy");
    let refused = StepEnd::Exited(Some(REFUSAL_EXIT_CODE));
    let classified = |role, item, end| seat_busy_exit_reason(role, item, end, busy.as_ref());
    assert_eq!(classified(StepRole::Judge, 7, refused), Some(SEAT_BUSY_REASON));
    assert_eq!(classified(StepRole::Judge, 7, StepEnd::Exited(Some(1))), None);
    assert_eq!(classified(StepRole::Judge, 7, StepEnd::TimedOut), None);
    assert_eq!(classified(StepRole::Judge, 8, refused), None);
    assert_eq!(classified(StepRole::Curator, 7, refused), None);
}

/// #12551: a step that ended waiting on CI releases as `ci_wait`, not `noop`.
#[test]
fn a_ci_pending_noop_line_releases_as_ci_wait() {
    let rel = |l: &str| release_class_of_line(&parse_outcome_line(l).unwrap());
    let line = |class: &str, reason: &str| {
        format!("LOOM_STEP_OUTCOME role=judge target=7 class={class} reason={reason}")
    };
    assert_eq!(rel(&line("noop", "ci_pending")), "ci_wait");
    assert_eq!(rel(&line("noop", "already_judged")), "noop");
    assert_eq!(rel(&line("verdict", "approved")), "verdict");
    assert_eq!(rel(&line("failed", "ci_pending")), "failed");
}

/// #13500: the Judge writes `reason=ci-pending`, the spelling of its own
/// `lease pr release --skip-reason`. The parser folds `-` to `_`, so the
/// line is the contract's `ci_pending` and releases as `ci_wait`, not as a
/// `noop` that climbs the captain's 30 m / 2 h / 6 h / 24 h ladder.
#[test]
fn a_hyphenated_ci_pending_line_is_the_same_token_and_releases_as_ci_wait() {
    let parsed =
        parse_outcome_line("LOOM_STEP_OUTCOME role=judge target=7 class=noop reason=ci-pending")
            .unwrap();
    assert_eq!(parsed.reason, "ci_pending");
    assert_eq!(release_class_of_line(&parsed), "ci_wait");
    let other = parse_outcome_line(
        "LOOM_STEP_OUTCOME role=curator target=7 class=refused reason=not-a-work-item-hold-digest",
    )
    .unwrap();
    assert_eq!(other.reason, "not_a_work_item_hold_digest");
    assert_eq!(release_class_of_line(&other), "refused");
}

/// #12729: a Champion `requeued` holds as `ci_wait` whatever gate requeued it;
/// a `skipped` stop stays on the no-progress ladder.
#[test]
fn a_champion_requeued_noop_line_releases_as_ci_wait_for_any_gate() {
    let rel = |l: &str| release_class_of_line(&parse_outcome_line(l).unwrap());
    let line = |reason: &str| {
        format!("LOOM_STEP_OUTCOME role=champion target=7 class=noop reason={reason}")
    };
    assert_eq!(rel(&line("requeued")), "ci_wait");
    assert_eq!(rel(&line("skipped")), "noop");
}

/// #13176: a Builder that opens a PR ends on the prompt's `## Completion`
/// section. If that section does not tell it to write the outcome line, the
/// step exits 0 with no line and is recorded `no-outcome-line`.
#[test]
fn builders_completion_section_requires_a_parseable_outcome_line() {
    let prompt = include_str!("../../../../defaults/.claude/commands/loom/builder.md");
    let completion = &prompt[prompt.rfind("\n## Completion").expect("Completion section")..];
    let line = completion
        .lines()
        .filter_map(|l| l.trim().strip_prefix('`').and_then(|l| l.strip_suffix('`')))
        .find(|l| l.starts_with("LOOM_STEP_OUTCOME role=builder"))
        .expect("Completion names the builder outcome line");
    let concrete = line.replace("<n>", "13176");
    let parsed = parse_outcome_line(&concrete).expect("the template parses once <n> is filled");
    assert_eq!(parsed.class, StepOutcomeClass::Verdict);
    assert_eq!(parsed.reason, "pr_opened");
    assert_eq!(parsed.target, 13176);
}

/// #13521: the capture-side class splits the same way, and only for it.
#[test]
fn a_no_line_builder_that_opened_a_pr_has_its_own_step_class() {
    let base = step_failure(StepRole::Builder, StepEnd::Exited(Some(0)), None, false).unwrap();
    let split = super::capture::no_outcome_line_with_pr(base);
    assert_eq!(split.class(), "step-no-outcome-line-pr-opened");
    assert_eq!(split.exit_code(), Some(0));
    let other = step_failure(StepRole::Builder, StepEnd::Exited(Some(3)), None, false).unwrap();
    assert_eq!(super::capture::no_outcome_line_with_pr(other).class(), "step-exit-nonzero");
}

// ---- selection-time model fallback (#13873) -------------------------------

/// #13873, end-to-end on the step engine: with `roleModels.judge = fable`,
/// `modelFallback.enabled` and every pool account bad-marked
/// `[model-class:fable]`, a Judge step launches `/loom:judge` on the `opus`
/// rung — and its `loom.dispatch.step` span records
/// `model_configured=fable`, `model_launched=opus`,
/// `model_fallback_from=fable` / `_reason=no-account-for-class`.
#[test]
#[serial]
fn a_step_falls_back_a_model_rung_when_the_configured_class_is_exhausted() {
    let dir = tempdir().unwrap();
    let (mut reg, record) = fixture_registry(dir.path());
    write_config(
        dir.path(),
        r#"{"autonomous": {"roleRunner": {"roleModels": {"judge": "fable"}},
            "modelFallback": {"enabled": true}}}"#,
    );
    // The fixture workspace is not a git worktree, so its per-repo pool is
    // the one selection resolves: two accounts, every one fable-marked —
    // fable has nothing to select, opus still does.
    let tokens = dir.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens).unwrap();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let mut marks = String::new();
    for account in ["alpha", "beta"] {
        std::fs::write(tokens.join(format!("{account}.token")), "sk-ant-oat01-fake").unwrap();
        marks.push_str(&format!(
            "{now} {account} exhausted: hit your Fable limit [model-class:fable]\n"
        ));
    }
    std::fs::write(tokens.join(".bad_tokens"), marks).unwrap();

    let request = StepSpawnRequest::new(9_101, StepRole::Judge);
    let (outcome, signals) = capture(|| reg.spawn_step(&request));
    let outcome = outcome.unwrap();
    let recorded = assert_child_wrote(&record, "--model");
    let lines: Vec<&str> = recorded.lines().collect();
    let idx = lines
        .iter()
        .position(|l| *l == "arg: --model")
        .expect("--model pinned");
    assert_eq!(lines[idx + 1], "arg: opus", "launched model must be the rung: {recorded}");

    let span = signals
        .spans
        .iter()
        .find(|s| s.name.as_str() == "loom.dispatch.step")
        .unwrap();
    assert_eq!(span.attributes["loom.step.model_configured"], "fable");
    assert_eq!(span.attributes["loom.step.model_launched"], "opus");
    assert_eq!(span.attributes["loom.step.model_fallback_from"], "fable");
    assert_eq!(span.attributes["loom.step.model_fallback_reason"], "no-account-for-class");
    assert_eq!(span.attributes["loom.dispatch.admission_result"], "spawned");
    cancel_all(&mut reg);
    let _ = outcome;
}

/// #13873 AC: with the block absent, the same exhausted-for-fable pool is
/// byte-identical to today — the step still launches on `fable` (and the
/// real wrapper's selection is what fails later), with no fallback keys on
/// the span.
#[test]
#[serial]
fn a_step_without_the_fallback_block_launches_the_configured_model() {
    let dir = tempdir().unwrap();
    let (mut reg, record) = fixture_registry(dir.path());
    write_config(
        dir.path(),
        r#"{"autonomous": {"roleRunner": {"roleModels": {"judge": "fable"}}}}"#,
    );
    let tokens = dir.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens).unwrap();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let mut marks = String::new();
    for account in ["alpha", "beta"] {
        std::fs::write(tokens.join(format!("{account}.token")), "sk-ant-oat01-fake").unwrap();
        marks.push_str(&format!(
            "{now} {account} exhausted: hit your Fable limit [model-class:fable]\n"
        ));
    }
    std::fs::write(tokens.join(".bad_tokens"), marks).unwrap();

    let request = StepSpawnRequest::new(9_102, StepRole::Judge);
    let (outcome, signals) = capture(|| reg.spawn_step(&request));
    outcome.unwrap();
    let recorded = assert_child_wrote(&record, "--model");
    let lines: Vec<&str> = recorded.lines().collect();
    let idx = lines
        .iter()
        .position(|l| *l == "arg: --model")
        .expect("--model pinned");
    assert_eq!(lines[idx + 1], "arg: fable", "no fallback may fire: {recorded}");
    let span = signals
        .spans
        .iter()
        .find(|s| s.name.as_str() == "loom.dispatch.step")
        .unwrap();
    assert_eq!(span.attributes["loom.step.model_configured"], "fable");
    assert!(!span
        .attributes
        .contains_key("loom.step.model_fallback_from"));
    cancel_all(&mut reg);
}
