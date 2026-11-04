//! #13054: a Builder `noop` / `failed` step posts its reason once.

use super::*;
use crate::step_dispatch::outcome_ledger::StepOutcome;
use crate::step_role::StepRole;
use crate::sweep_registry::test_support::fixture_registry;
use std::time::Duration;
use tempfile::tempdir;

fn run(class: OutcomeClass, reason: &str, sweep: &str) -> StepRun {
    StepRun {
        item: 9,
        step: StepRole::Builder,
        sweep_id: sweep.into(),
        outcome: StepOutcome::new(class, reason),
        ran_for: Duration::from_secs(600),
        failure_class: None,
    }
}

#[test]
fn a_noop_notice_carries_the_reason_and_the_marker_excerpt() {
    let body = outcome_notice_body(
        &run(OutcomeClass::Noop, "no_changes_needed", "step-builder-9-1"),
        Some("already fixed on main"),
    )
    .expect("a noop gets a notice");
    assert!(body.starts_with(STEP_OUTCOME_COMMENT_MARKER));
    assert!(body.contains("sweep=step-builder-9-1"));
    assert!(body.contains("`no_changes_needed`"));
    assert!(body.contains("already fixed on main"));
}

#[test]
fn a_failed_notice_names_the_reason_and_a_verdict_gets_none() {
    let body = outcome_notice_body(&run(OutcomeClass::Failed, "unclassified", "s"), None)
        .expect("a failed step gets a notice");
    assert!(body.contains("`unclassified`"));
    assert!(outcome_notice_body(&run(OutcomeClass::Verdict, "pr_opened", "s"), None).is_none());
    assert!(outcome_notice_body(&run(OutcomeClass::Refused, "claimed", "s"), None).is_none());
}

#[test]
fn the_excerpt_is_the_first_200_chars_of_the_marker_file() {
    let dir = tempdir().unwrap();
    assert_eq!(marker_excerpt(dir.path()), None);
    std::fs::write(dir.path().join(".no-changes-needed"), "x".repeat(500)).unwrap();
    assert_eq!(marker_excerpt(dir.path()).unwrap().len(), 200);
}

#[test]
fn a_step_is_noticed_once_and_an_empty_sweep_id_never() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    assert!(reg.claim_outcome_notice("step-builder-9-1"));
    assert!(!reg.claim_outcome_notice("step-builder-9-1"));
    assert!(reg.claim_outcome_notice("step-builder-9-2"));
    assert!(!reg.claim_outcome_notice(""));
}

#[test]
fn recording_a_builder_noop_claims_the_notice() {
    struct NoForge;
    impl crate::step_dispatch::reeval_ladder::BodyForge for NoForge {
        fn body(&mut self, _: u64) -> Result<String, String> {
            Ok(String::new())
        }
        fn set_body(&mut self, _: u64, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let noop = run(OutcomeClass::Noop, "no_changes_needed", "step-builder-9-1");
    reg.record_step_outcome(&noop, &mut NoForge);
    assert!(
        reg.step_outcome_notices.contains("step-builder-9-1"),
        "noop must post a comment"
    );
    let failed = run(OutcomeClass::Failed, "unclassified", "step-builder-9-2");
    reg.record_step_outcome(&failed, &mut NoForge);
    assert!(
        reg.step_outcome_notices.contains("step-builder-9-2"),
        "failed must post a comment"
    );
    let mut other = run(OutcomeClass::Noop, "no_fix_needed", "step-doctor-9-1");
    other.step = StepRole::Doctor;
    reg.record_step_outcome(&other, &mut NoForge);
    assert!(!reg.step_outcome_notices.contains("step-doctor-9-1"));
}

#[test]
fn repeated_outcomes_edit_one_comment_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let gh = ws.join("fake-gh.sh");
    // `list` answers with the stored id once a comment exists; POST (issue
    // comment) creates it; PATCH records the edit.
    std::fs::write(
        &gh,
        format!(
            r#"#!/usr/bin/env bash
D="{d}"
case "$*" in
  *"-X PATCH"*) echo "$*" >> "$D/patched" ;;
  "api "*"/comments "*) [ -f "$D/created" ] && echo 4242 ;;
  "issue comment"*) echo x >> "$D/created" ;;
  *) exit 1 ;;
esac
"#,
            d = ws.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = crate::sweep_registry::SweepRegistryConfig::new(ws.to_path_buf());
    config.gh_bin = Some(gh);
    let mut reg = crate::sweep_registry::SweepRegistry::new(config);
    for i in 0..5 {
        reg.note_builder_outcome(&run(OutcomeClass::Failed, "unclassified", &format!("s-{i}")));
    }
    let created = std::fs::read_to_string(ws.join("created")).unwrap();
    assert_eq!(created.lines().count(), 1, "one comment posted, the rest edited");
    let patched = std::fs::read_to_string(ws.join("patched")).unwrap();
    assert_eq!(patched.matches("issues/comments/4242").count(), 4);
    assert!(patched.contains("issues/comments/4242"));
    assert!(patched.contains("sweep=s-4"), "latest outcome retained");
}
