//! #12500: the Builder repeat-cap notice is due once per window.

use super::*;
use crate::step_dispatch::outcome_ledger::{OutcomeClass, StepOutcome};
use crate::step_role::StepRole;
use crate::sweep_registry::step_ledger::StepRun;
use crate::sweep_registry::test_support::fixture_registry;
use chrono::Duration as Chrono;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn the_notice_names_the_cause_count_and_release_time() {
    let until = Utc::now() + Chrono::hours(5);
    let body = cap_notice_body(3, 24, until);
    assert!(body.starts_with(STEP_CAP_COMMENT_MARKER));
    assert!(body.contains("3 Builder runs in the last 24 h"));
    assert!(body.contains(&until.format("%Y-%m-%d %H:%M UTC").to_string()));
}

#[test]
fn a_second_arming_in_the_same_window_posts_nothing() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let now = Utc::now();
    let until = now + Chrono::hours(20);
    assert!(reg.claim_cap_notice(7, until, now));
    assert!(!reg.claim_cap_notice(7, until + Chrono::hours(1), now + Chrono::hours(1)));
    assert!(reg.claim_cap_notice(8, until, now), "another item has its own window");
    assert!(
        reg.claim_cap_notice(7, until + Chrono::hours(30), until + Chrono::hours(1)),
        "a new window after the last one ended"
    );
}

#[test]
fn the_third_builder_run_claims_the_notice_and_the_fourth_does_not() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let run = |class, reason: &str| StepRun {
        item: 9,
        step: StepRole::Builder,
        sweep_id: "step-builder-9-1".into(),
        outcome: StepOutcome::new(class, reason),
        ran_for: Duration::from_secs(600),
        failure_class: None,
    };
    struct NoForge;
    impl crate::step_dispatch::reeval_ladder::BodyForge for NoForge {
        fn body(&mut self, _: u64) -> Result<String, String> {
            Ok(String::new())
        }
        fn set_body(&mut self, _: u64, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    for k in 0..2 {
        let rec = reg.record_step_outcome(&run(OutcomeClass::Failed, "crashed"), &mut NoForge);
        assert!(!rec.repeat_capped, "run {k}");
        assert!(!reg.step_cap_notices.contains_key(&9));
    }
    let rec = reg.record_step_outcome(&run(OutcomeClass::Failed, "crashed"), &mut NoForge);
    assert!(rec.repeat_capped);
    let first = reg
        .step_cap_notices
        .get(&9)
        .copied()
        .expect("notice claimed at the cap");
    let rec = reg.record_step_outcome(&run(OutcomeClass::Failed, "crashed"), &mut NoForge);
    assert!(rec.repeat_capped);
    assert_eq!(reg.step_cap_notices.get(&9), Some(&first), "same window, no second notice");
}
