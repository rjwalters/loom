//! #11900: the registry applies the step-outcome ledger's effects. (The
//! windows used to be broadcast to peers over the safehouse claims room; that
//! channel was retired in #13945, so they are this host's own.)

use super::*;
use crate::step_dispatch::outcome_ledger::OutcomeClass;
use crate::sweep_registry::test_support::fixture_registry;
use serial_test::serial;
use std::sync::Arc;
use tempfile::tempdir;

const DAY: u64 = 24 * 3600;

#[derive(Default)]
struct FakeForge {
    body: String,
    writes: usize,
}

impl BodyForge for FakeForge {
    fn body(&mut self, _number: u64) -> Result<String, String> {
        Ok(self.body.clone())
    }
    fn set_body(&mut self, _number: u64, body: &str) -> Result<(), String> {
        self.body = body.to_string();
        self.writes += 1;
        Ok(())
    }
}

/// A forge whose body reads but whose writes fail (#12516): a secondary rate
/// limit on content writes, an oversized body, a permission error.
struct WriteFailingForge {
    body: String,
    attempts: usize,
}

impl BodyForge for WriteFailingForge {
    fn body(&mut self, _number: u64) -> Result<String, String> {
        Ok(self.body.clone())
    }
    fn set_body(&mut self, _number: u64, _body: &str) -> Result<(), String> {
        self.attempts += 1;
        Err("secondary rate limit".into())
    }
}

fn run(item: u32, step: StepRole, class: OutcomeClass, reason: &str, secs: u64) -> StepRun {
    StepRun {
        item,
        step,
        sweep_id: format!("step-{step}-{item}-1"),
        outcome: StepOutcome::new(class, reason),
        ran_for: Duration::from_secs(secs),
        failure_class: None,
    }
}

fn registry(dir: &std::path::Path) -> SweepRegistry {
    fixture_registry(dir).0
}

#[test]
fn a_noop_cools_locally_per_item_step() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let rec = reg.record_step_outcome(
        &run(9, StepRole::Judge, OutcomeClass::Noop, "no_changes_needed", 600),
        &mut FakeForge::default(),
    );
    assert!(rec.cooling_until.is_some());
    let now = Utc::now();
    assert!(reg.step_cooldowns(now).is_cooling(9, StepRole::Judge, now));
    assert!(!reg.step_cooldowns(now).is_cooling(9, StepRole::Doctor, now));
    let until = reg.step_cooldowns(now).until(9, StepRole::Judge).unwrap();
    let left = (until - now).num_seconds();
    assert!((3590..=3600).contains(&left), "{left}");
}

/// #12042: a `ci_pending` no-op waited on CI, not on the item, so it cools
/// for the short refusal window (10 min).
#[test]
fn a_ci_pending_noop_cools_for_the_short_window() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.record_step_outcome(
        &run(9, StepRole::Judge, OutcomeClass::Noop, "ci_pending", 600),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    let until = reg.step_cooldowns(now).until(9, StepRole::Judge).unwrap();
    let left = (until - now).num_seconds();
    assert!((590..=600).contains(&left), "{left}");
}

/// The refusal cooldown is in the draw's query as soon as the step is
/// recorded, before the next tick.
#[test]
fn a_pool_gate_refusal_is_in_the_query_before_the_next_tick() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.record_step_outcome(
        &run(3, StepRole::Curator, OutcomeClass::Refused, "pool_gate", 5),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    assert!(reg
        .step_cooldowns(now)
        .is_cooling(3, StepRole::Curator, now));
}

#[test]
#[serial]
fn landed_arms_the_landed_cooldown_and_a_24h_builder_cooldown() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.record_step_outcome(
        &run(12, StepRole::Curator, OutcomeClass::Verdict, "closed", 300),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    assert!(reg.landed_cooldown_active(12));
    assert!(reg
        .step_cooldowns(now)
        .is_cooling(12, StepRole::Builder, now));
    let until = reg
        .step_cooldowns(now)
        .until(12, StepRole::Builder)
        .unwrap();
    let left = (until - now).num_seconds();
    let day = i64::try_from(DAY).unwrap();
    assert!(left <= day && left > day - 10, "{left}");
}

#[test]
fn a_champion_verdict_clears_the_ladder() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let mut forge = FakeForge {
        body: "Body.".into(),
        ..FakeForge::default()
    };
    reg.record_step_outcome(
        &run(30, StepRole::ChampionMerge, OutcomeClass::Noop, "requeued", 60),
        &mut forge,
    );
    reg.record_step_outcome(
        &run(30, StepRole::ChampionMerge, OutcomeClass::Noop, "held", 60),
        &mut forge,
    );
    assert!(forge.body.contains("loom:reeval"));

    reg.record_step_outcome(
        &run(30, StepRole::ChampionMerge, OutcomeClass::Verdict, "merged", 60),
        &mut forge,
    );
    assert_eq!(forge.body, "Body.", "the ladder is gone");
    let now = Utc::now();
    assert!(!reg
        .step_cooldowns(now)
        .is_cooling(30, StepRole::ChampionMerge, now));
}

/// The ladder goes on the forge; locally only up to 24 h,
/// even when the rung is two days.
#[test]
fn the_forge_ladder_is_written_and_its_mirror_is_bounded_at_24h() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let mut forge = FakeForge {
        body:
            "Parked.\n\n<!-- loom:reeval step=champion_promote rung=6 next=2026-01-01T00:00:00Z -->"
                .into(),
        ..FakeForge::default()
    };
    reg.record_step_outcome(
        &run(8, StepRole::ChampionPromote, OutcomeClass::Noop, "blocked", 60),
        &mut forge,
    );
    let written = reeval_ladder::parse(&forge.body);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].rung, 7, "48 h rung (#12513)");
    let now = Utc::now();
    let local = reg
        .step_cooldowns(now)
        .until(8, StepRole::ChampionPromote)
        .unwrap();
    assert!(local <= now + chrono::Duration::hours(24), "{local}");
    assert!(local > now + chrono::Duration::hours(23));
}

#[test]
fn an_issue_step_fast_failure_charges_the_ladder_and_mirrors_its_window() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.record_step_outcome(
        &run(21, StepRole::Builder, OutcomeClass::Failed, "crashed", 30),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    let ladder = reg
        .dispatch_backoff_until(21, now)
        .expect("the #11854 ladder armed");
    assert_eq!(reg.step_cooldowns(now).until(21, StepRole::Builder), Some(ladder));
}

/// Judge review on #11962: a PR step driven well past the fast-fail park
/// threshold never reaches the issue quarantine. No `gh` call (no label, no
/// park record, no comment), no body write, no quarantine entry, no issue
/// ladder; only the ledger's own window. #11969 widens it to
/// Curator and Champion promotion: the park's release restores `loom:issue`,
/// which would let a curated or revised issue skip promotion.
#[test]
fn a_non_builder_step_past_the_fast_fail_threshold_writes_nothing_to_the_forge() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let gh_log = dir.path().join("gh-calls.log");
    let fake_gh = dir.path().join("fake-gh-recording.sh");
    std::fs::write(
        &fake_gh,
        format!("#!/bin/bash\necho \"$*\" >> '{}'\nexit 0\n", gh_log.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    reg.config.gh_bin = Some(fake_gh);
    let mut forge = FakeForge {
        body: "PR body.".into(),
        ..FakeForge::default()
    };
    let steps = [
        StepRole::Curator,
        StepRole::ChampionPromote,
        StepRole::Judge,
        StepRole::Doctor,
        StepRole::ChampionMerge,
    ];
    for step in steps {
        for _ in 0..6 {
            let rec = reg.record_step_outcome(
                &run(77, step, OutcomeClass::Failed, "crashed", 20),
                &mut forge,
            );
            assert_eq!(rec.effect, Effect::FastFailWindow, "{step}");
        }
    }
    assert!(!gh_log.exists(), "no forge call: {:?}", std::fs::read_to_string(&gh_log));
    assert_eq!(forge.writes, 0, "no body write");
    assert_eq!(forge.body, "PR body.");
    assert!(!reg.is_quarantined(77), "never the issue quarantine");
    assert_eq!(reg.dispatch_backoff_until(77, Utc::now()), None, "never the issue ladder");
    let now = Utc::now();
    for step in steps {
        assert!(reg.step_cooldowns(now).is_cooling(77, step, now), "{step}");
    }
}

#[test]
fn claim_lost_is_counted() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.record_step_outcome(
        &run(5, StepRole::Doctor, OutcomeClass::Refused, "claim_lost", 1),
        &mut FakeForge::default(),
    );
    assert_eq!(reg.step_ledger_counters().claim_lost, 1);
}

/// #11969: a second consecutive lost claim arms the 10-min refusal cooldown,
/// in the draw's query.
#[test]
fn a_second_claim_lost_cools_locally() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let lost = run(5, StepRole::Curator, OutcomeClass::Refused, "claim_lost", 1);
    reg.record_step_outcome(&lost, &mut FakeForge::default());
    let rec = reg.record_step_outcome(&lost, &mut FakeForge::default());
    assert_eq!(rec.effect, Effect::Redraw);
    assert!(rec.cooling_until.is_some());
    let now = Utc::now();
    assert!(reg
        .step_cooldowns(now)
        .is_cooling(5, StepRole::Curator, now));
    assert_eq!(reg.step_ledger_counters().claim_lost, 2);
}

/// #12152: a finished step's terminal bus event carries the ledger's
/// classification and the registry's workspace, on its own topic.
#[test]
fn emit_step_ended_publishes_the_ledger_classification() {
    let dir = tempdir().unwrap();
    let mut registry = registry(dir.path());
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let mut sub = bus.subscribe(["step.item"]);
    registry.set_event_bus(Arc::clone(&bus));
    registry.emit_step_ended(&run(
        42,
        StepRole::ChampionMerge,
        OutcomeClass::Verdict,
        "merged",
        90,
    ));
    let event = sub.try_recv().unwrap();
    assert_eq!(event.topic(), "step.item.42.ended");
    let crate::types::Event::StepEnded {
        item,
        role,
        class,
        reason,
        ran_for_sec,
        repo,
    } = event
    else {
        panic!("expected StepEnded, got {event:?}");
    };
    assert_eq!((item, role), (42, StepRole::ChampionMerge));
    assert_eq!((class.as_str(), reason.as_str(), ran_for_sec), ("verdict", "merged", 90));
    assert_eq!(
        repo.as_deref(),
        Some(
            registry
                .config()
                .workspace_root
                .display()
                .to_string()
                .as_str()
        )
    );
    assert!(sub.try_recv().is_err(), "exactly one event per ended step");
}

/// #12251: the registry mirrors its ledger to the workspace's logs directory
/// and a new registry on the same workspace (a daemon restart or roll)
/// reloads it, so the repeat cap holds across the restart and this host's
/// own windows come back.
#[test]
#[serial]
fn the_repeat_cap_and_windows_survive_a_registry_restart() {
    let dir = tempdir().unwrap();
    {
        let mut reg = registry(dir.path());
        for _ in 0..2 {
            let rec = reg.record_step_outcome(
                &run(31, StepRole::Builder, OutcomeClass::Failed, "crashed", 600),
                &mut FakeForge::default(),
            );
            assert!(!rec.repeat_capped);
        }
        reg.hold_landed_builder(32);
    }
    assert!(dir
        .path()
        .join(".loom/logs")
        .join(STEP_LEDGER_FILE)
        .exists());

    let mut reg = registry(dir.path());
    let now = Utc::now();
    assert!(
        reg.step_cooldowns(now)
            .is_cooling(32, StepRole::Builder, now),
        "the landed Builder hold came back"
    );
    let rec = reg.record_step_outcome(
        &run(31, StepRole::Builder, OutcomeClass::Failed, "crashed", 600),
        &mut FakeForge::default(),
    );
    assert!(rec.repeat_capped, "the third run caps across the restart");
}

/// #12516: a forge write that keeps failing does not loop a held item at the
/// first rung. Each failed write holds the first rung's window, and the runs
/// count toward the repeat cap, so the third held verdict in a row cools the
/// item well past 1 h (#12524: it is the cap, not a climb, that does this).
#[test]
fn a_failing_ladder_write_hits_the_cap_on_the_third_run() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let mut forge = WriteFailingForge {
        body: "Held.".into(),
        attempts: 0,
    };
    let mut windows = Vec::new();
    for _ in 0..3 {
        let rec = reg.record_step_outcome(
            &run(60, StepRole::ChampionMerge, OutcomeClass::Noop, "held", 600),
            &mut forge,
        );
        let now = Utc::now();
        windows.push((rec.repeat_capped, rec.cooling_until.unwrap() - now));
    }
    assert_eq!(forge.attempts, 3);
    assert!(reeval_ladder::parse(&forge.body).is_empty(), "nothing written");
    for (capped, left) in &windows[..2] {
        assert!(!capped);
        assert!(*left <= chrono::Duration::hours(1), "{left}");
        assert!(*left > chrono::Duration::minutes(59), "{left}");
    }
    let (capped, left) = windows[2];
    assert!(capped, "the third failed write counts toward the cap");
    assert!(left > chrono::Duration::hours(23), "{left}");
    assert!(left <= chrono::Duration::hours(24), "{left}");
}

/// #12516: the fallback is the next rung after the stored one, not a flat 1 h.
#[test]
fn a_failing_ladder_write_holds_the_next_rung_after_the_stored_one() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    let next = (Utc::now() + chrono::Duration::minutes(30)).to_rfc3339();
    let mut forge = WriteFailingForge {
        body: format!("Held.\n\n<!-- loom:reeval step=champion_merge rung=3 next={next} -->"),
        attempts: 0,
    };
    let rec = reg.record_step_outcome(
        &run(61, StepRole::ChampionMerge, OutcomeClass::Noop, "held", 600),
        &mut forge,
    );
    let left = rec.cooling_until.unwrap() - Utc::now();
    assert!(left > chrono::Duration::minutes(239), "rung 4 = 4 h: {left}");
    assert!(left <= chrono::Duration::hours(4), "{left}");
}

/// #12516: the failed write's window survives a restart (there is no marker
/// to read back), and so do its repeat-cap runs.
#[test]
#[serial]
fn a_failing_ladder_writes_fallback_survives_a_registry_restart() {
    let dir = tempdir().unwrap();
    let mut forge = WriteFailingForge {
        body: "Held.".into(),
        attempts: 0,
    };
    {
        let mut reg = registry(dir.path());
        for _ in 0..2 {
            let rec = reg.record_step_outcome(
                &run(62, StepRole::ChampionMerge, OutcomeClass::Noop, "held", 600),
                &mut forge,
            );
            assert!(!rec.repeat_capped);
        }
    }
    let mut reg = registry(dir.path());
    let now = Utc::now();
    let until = reg
        .step_cooldowns(now)
        .until(62, StepRole::ChampionMerge)
        .expect("the fallback came back");
    assert!(until > now + chrono::Duration::minutes(59), "{until}");
    let rec = reg.record_step_outcome(
        &run(62, StepRole::ChampionMerge, OutcomeClass::Noop, "held", 600),
        &mut forge,
    );
    assert!(rec.repeat_capped, "the third failed write caps across the restart");
}

/// #12251: a Builder refused on a landed issue is held off it for
/// [`LANDED_COOLDOWN`]: Curator's verdict on the item lifts it like any verdict.
#[test]
fn a_landed_builder_hold_is_lifted_by_a_verdict() {
    let dir = tempdir().unwrap();
    let mut reg = registry(dir.path());
    reg.hold_landed_builder(40);
    let now = Utc::now();
    assert!(reg
        .step_cooldowns(now)
        .is_cooling(40, StepRole::Builder, now));
    let until = reg
        .step_cooldowns(now)
        .until(40, StepRole::Builder)
        .unwrap();
    let left = u64::try_from((until - now).num_seconds()).unwrap();
    let secs = LANDED_COOLDOWN.as_secs();
    assert!((secs - 10..=secs).contains(&left), "{left}");

    reg.record_step_outcome(
        &run(40, StepRole::Curator, OutcomeClass::Verdict, "kept", 300),
        &mut FakeForge::default(),
    );
    let now = Utc::now();
    assert!(!reg
        .step_cooldowns(now)
        .is_cooling(40, StepRole::Builder, now));
}

/// #12524: one entry this build cannot decode (a cause a newer daemon added)
/// is skipped; every other entry still loads.
#[test]
#[serial]
fn an_unknown_cause_skips_that_entry_and_loads_the_rest() {
    let dir = tempdir().unwrap();
    let logs = dir.path().join(".loom/logs");
    std::fs::create_dir_all(&logs).unwrap();
    let until = (Utc::now() + chrono::Duration::minutes(30)).to_rfc3339();
    let entry = |item: u32, cause: &str| {
        serde_json::json!({
            "item": item, "step": "builder", "runs": [],
            "until": until, "cause": cause,
        })
    };
    let file = serde_json::json!([
        entry(41, "landed"),
        entry(42, "some_future_cause"),
        entry(43, "landed"),
    ]);
    std::fs::write(logs.join(STEP_LEDGER_FILE), file.to_string()).unwrap();

    let reg = registry(dir.path());
    let now = Utc::now();
    let cooling = reg.step_cooldowns(now);
    assert!(cooling.is_cooling(41, StepRole::Builder, now));
    assert!(!cooling.is_cooling(42, StepRole::Builder, now));
    assert!(cooling.is_cooling(43, StepRole::Builder, now));
}

/// #12524: a file that is not a JSON array is still an empty ledger.
#[test]
#[serial]
fn a_non_array_ledger_file_is_an_empty_ledger() {
    let dir = tempdir().unwrap();
    let logs = dir.path().join(".loom/logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join(STEP_LEDGER_FILE), "{\"item\": 1}").unwrap();
    let reg = registry(dir.path());
    let now = Utc::now();
    assert!(!reg
        .step_cooldowns(now)
        .is_cooling(1, StepRole::Builder, now));
}

/// #12704: a captain's cooldown refusal arms the pair until the hold ends,
/// extend-only and pruned with the ledger; an absent or zero hold arms nothing.
#[test]
#[serial]
fn a_captain_hold_arms_the_pair_until_it_ends() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let now = Utc::now();
    reg.arm_captain_hold(9, StepRole::Judge, 0);
    assert!(!reg.step_cooldowns(now).is_cooling(9, StepRole::Judge, now));
    reg.arm_captain_hold(9, StepRole::Judge, 120);
    let cooling = reg.step_cooldowns(now);
    assert!(cooling.is_cooling(9, StepRole::Judge, now));
    assert!(!cooling.is_cooling(9, StepRole::Doctor, now));
    let later = now + chrono::Duration::seconds(130);
    assert!(!reg
        .step_cooldowns(later)
        .is_cooling(9, StepRole::Judge, later));
}
