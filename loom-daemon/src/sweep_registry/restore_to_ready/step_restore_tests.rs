//! Per-role step restore (#11898): one test per row of design §5's table,
//! plus the carve-outs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::os::unix::fs::PermissionsExt;

fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

/// A fake `gh` whose issue-signals read reports `state` and `labels` (quoted
/// JSON list body), recording every invocation.
fn fake_gh(dir: &Path, state: &str, labels: &str) -> (PathBuf, PathBuf) {
    let fake = dir.join("fake-gh.sh");
    let log = dir.join("gh-calls.log");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  case "$*" in
    *".pull_request != null"*) echo "false" ;;
    *".state"*) printf '{{"body":"","state":"{state}","closed_at":null,"labels":[{labels}]}}' ;;
    *) echo "[]" ;;
  esac
  exit 0
fi
exit 0
"#,
        log = log.display(),
    );
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    (fake, log)
}

fn registry_with(fake_gh: &Path, dir: &Path) -> SweepRegistry {
    let mut config = SweepRegistryConfig::new(dir.to_path_buf());
    config.gh_bin = Some(fake_gh.to_path_buf());
    config.skip_label_flip = false;
    SweepRegistry::new(config)
}

/// Run one role's restore against an open item carrying its claim, and
/// return the gh calls.
fn restore_once(role: StepRole, item: u32) -> String {
    let dir = tempfile::tempdir().unwrap();
    let claim = role.claim_label().unwrap_or("loom:pr");
    let (gh, log) = fake_gh(dir.path(), "open", &format!("\"{claim}\""));
    let registry = registry_with(&gh, dir.path());
    let release = registry.restore_step_claim(item, role, "step-x").unwrap();
    assert_eq!(release, ClaimRelease::Released);
    std::fs::read_to_string(log).unwrap_or_default()
}

#[test]
#[serial_test::serial]
fn curator_step_restores_curating_to_triage() {
    let calls = restore_once(StepRole::Curator, 501);
    assert!(
        calls.contains("issue edit 501 --remove-label loom:curating --add-label loom:triage"),
        "{calls}"
    );
}

#[test]
#[serial_test::serial]
fn champion_promote_step_restores_evaluating_to_curated() {
    let calls = restore_once(StepRole::ChampionPromote, 502);
    assert!(
        calls.contains("issue edit 502 --remove-label loom:evaluating --add-label loom:curated"),
        "{calls}"
    );
}

#[test]
#[serial_test::serial]
fn judge_step_restores_reviewing_to_review_requested() {
    let calls = restore_once(StepRole::Judge, 503);
    assert!(
        calls.contains(
            "pr edit 503 --remove-label loom:reviewing --add-label loom:review-requested"
        ),
        "{calls}"
    );
}

#[test]
#[serial_test::serial]
fn doctor_step_restores_treating_to_changes_requested() {
    let calls = restore_once(StepRole::Doctor, 504);
    assert!(
        calls.contains(
            "pr edit 504 --remove-label loom:treating --add-label loom:changes-requested"
        ),
        "{calls}"
    );
}

#[test]
#[serial_test::serial]
fn builder_step_restores_building_to_issue_through_the_shared_release() {
    let calls = restore_once(StepRole::Builder, 505);
    assert!(
        calls.contains("issue edit 505 --remove-label loom:building --add-label loom:issue"),
        "{calls}"
    );
}

/// The Builder restore holds `loom:building` while the issue's PR is open,
/// even with the sweep path's off-switch set.
#[test]
#[serial_test::serial]
fn builder_step_with_an_open_pr_keeps_building_even_with_the_veto_switched_off() {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = fake_gh(dir.path(), "open", "\"loom:building\"");
    let registry = registry_with(&gh, dir.path());
    registry.record_open_pr_memo(506, OpenPrProbe::Open(9506));
    let prior = std::env::var(crate::release_veto::OPEN_PR_VETO_ENV).ok();
    std::env::set_var(crate::release_veto::OPEN_PR_VETO_ENV, "0");
    let release = registry.restore_step_claim(506, StepRole::Builder, "step-x");
    match prior {
        Some(v) => std::env::set_var(crate::release_veto::OPEN_PR_VETO_ENV, v),
        None => std::env::remove_var(crate::release_veto::OPEN_PR_VETO_ENV),
    }
    assert_eq!(release.unwrap(), ClaimRelease::HeldByOpenPr(9506));
    let calls = std::fs::read_to_string(log).unwrap_or_default();
    assert!(!calls.contains("edit 506"), "{calls}");
}

#[test]
#[serial_test::serial]
fn merge_step_writes_no_label() {
    let calls = restore_once(StepRole::ChampionMerge, 507);
    assert!(calls.is_empty(), "{calls}");
}

#[test]
fn plan_holds_a_frozen_item_and_keeps_vetoes_and_advanced_items_forward() {
    use StepReleasePlan::{Nothing, RemoveClaim, Restore};
    let judge = StepRole::Judge;
    assert_eq!(plan_step_release(judge, &labels(&["loom:reviewing"]), Some(false)), Restore);
    assert_eq!(plan_step_release(judge, &labels(&["loom:reviewing"]), None), Restore);
    assert_eq!(
        plan_step_release(judge, &labels(&["loom:reviewing", "loom:frozen"]), Some(false)),
        Nothing
    );
    assert_eq!(
        plan_step_release(judge, &labels(&["loom:reviewing", "loom:blocked"]), Some(false)),
        RemoveClaim
    );
    assert_eq!(plan_step_release(judge, &labels(&["loom:reviewing"]), Some(true)), RemoveClaim);
    // The Judge wrote its verdict and died before dropping the claim.
    assert_eq!(
        plan_step_release(judge, &labels(&["loom:reviewing", "loom:pr"]), Some(false)),
        RemoveClaim
    );
    assert_eq!(
        plan_step_release(
            StepRole::Curator,
            &labels(&["loom:curating", "loom:curated"]),
            Some(false)
        ),
        RemoveClaim
    );
    assert_eq!(plan_step_release(StepRole::ChampionMerge, &[], Some(false)), Nothing);
}
