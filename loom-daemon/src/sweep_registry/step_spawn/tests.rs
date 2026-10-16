//! Step spawn (#11906): the launch, the role's resolvers, the claim
//! contract, the timeout reap and the end-of-step capture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::capture::{
    emit_step_capture, parse_outcome_line, read_outcome_line, tick_outcome, StepCapture, StepEnd,
    StepOutcomeClass,
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
                overflow: false,
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
fn a_steps_end_maps_to_the_role_ticks_outcome() {
    let noop = parse_outcome_line("LOOM_STEP_OUTCOME role=c target=1 class=noop reason=x");
    let failed = parse_outcome_line("LOOM_STEP_OUTCOME role=c target=1 class=failed reason=x");
    assert_eq!(tick_outcome(StepEnd::Exited(Some(0)), noop.as_ref()), RoleTickOutcome::Success);
    assert_eq!(tick_outcome(StepEnd::Exited(None), noop.as_ref()), RoleTickOutcome::Success);
    for (end, line) in [
        (StepEnd::Exited(Some(0)), failed.as_ref()),
        (StepEnd::Exited(Some(1)), None),
        (StepEnd::Exited(None), None),
        (StepEnd::TimedOut, noop.as_ref()),
        (StepEnd::Cancelled, None),
    ] {
        assert!(
            matches!(tick_outcome(end, line), RoleTickOutcome::Failure(_)),
            "{end:?} {line:?}"
        );
    }
}

/// Capture parity with role ticks (#11877): a step's end writes the same
/// `role_tick.outcome` record a role tick of that role writes.
#[test]
#[serial]
fn a_steps_end_writes_the_role_ticks_outcome_record() {
    let dir = tempdir().unwrap();
    std::env::remove_var(crate::sweep_outcomes::ROLE_TICK_TELEMETRY_JOURNAL_PATH_ENV);
    emit_step_capture(&StepCapture {
        root: dir.path().to_path_buf(),
        role: StepRole::ChampionMerge,
        sweep_id: "step-champion_merge-8-1".into(),
        started_at: Utc::now() - chrono::Duration::seconds(30),
        outcome: RoleTickOutcome::Success,
        step_outcome: Some(("noop".into(), "already_judged".into())),
        model: "opus".into(),
        effort: "high".into(),
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
