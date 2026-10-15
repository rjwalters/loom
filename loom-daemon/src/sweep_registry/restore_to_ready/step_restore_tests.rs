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
    // A failed read never restores (#11921 review): at most the claim goes.
    assert_eq!(plan_step_release(judge, &labels(&["loom:reviewing"]), None), RemoveClaim);
    assert_eq!(plan_step_release(judge, &[], None), RemoveClaim);
    // The claim is gone: the step finished, or a later role holds the item.
    assert_eq!(plan_step_release(judge, &labels(&["loom:pr"]), Some(false)), Nothing);
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

/// A fake `gh` whose issue-signals read fails (rate-limited), recording calls.
fn fake_gh_unreadable(dir: &Path) -> (PathBuf, PathBuf) {
    let fake = dir.join("fake-gh.sh");
    let log = dir.join("gh-calls.log");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  echo "HTTP 403: API rate limit exceeded" >&2
  exit 1
fi
exit 0
"#,
        log = log.display(),
    );
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    (fake, log)
}

/// Run `role`'s release on `item` whose live labels are `labels` (`None`: the
/// read fails), returning the release and the edits it made.
fn release_with(role: StepRole, item: u32, labels: Option<&str>) -> (ClaimRelease, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let (gh, log) = match labels {
        Some(l) => fake_gh(dir.path(), "open", l),
        None => fake_gh_unreadable(dir.path()),
    };
    let registry = registry_with(&gh, dir.path());
    let release = registry.restore_step_claim(item, role, "step-x").unwrap();
    let edits = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(" edit "))
        .map(str::to_string)
        .collect();
    (release, edits)
}

/// #11921 review: a Judge that exited 0 having written `loom:pr` must not get
/// `loom:review-requested` back when the release read is rate-limited.
#[test]
#[serial_test::serial]
fn a_finished_judge_with_a_failed_read_only_drops_its_own_claim() {
    let (_, edits) = release_with(StepRole::Judge, 601, None);
    assert_eq!(edits, vec!["pr edit 601 --remove-label loom:reviewing".to_string()]);
}

/// A Doctor that finished, then a Judge claimed: the Doctor's release writes
/// nothing (no `loom:changes-requested` on a PR under review).
#[test]
#[serial_test::serial]
fn a_doctor_release_after_a_judge_claimed_writes_nothing() {
    let (release, edits) =
        release_with(StepRole::Doctor, 602, Some(r#""loom:review-requested","loom:reviewing""#));
    assert_eq!(release, ClaimRelease::Untouched);
    assert!(edits.is_empty(), "{edits:?}");
}

/// A Judge that requested changes, then a Doctor claimed: nothing written.
#[test]
#[serial_test::serial]
fn a_judge_release_after_a_doctor_claimed_writes_nothing() {
    let (release, edits) =
        release_with(StepRole::Judge, 603, Some(r#""loom:changes-requested","loom:treating""#));
    assert_eq!(release, ClaimRelease::Untouched);
    assert!(edits.is_empty(), "{edits:?}");
}

/// A Curator that finished, the issue promoted and a Builder claimed: no
/// `loom:triage` beside `loom:building`.
#[test]
#[serial_test::serial]
fn a_curator_release_after_a_builder_claimed_writes_nothing() {
    let (release, edits) = release_with(StepRole::Curator, 604, Some(r#""loom:building""#));
    assert_eq!(release, ClaimRelease::Untouched);
    assert!(edits.is_empty(), "{edits:?}");
}

/// A Builder step that exited 0 (its PR opened, `loom:building` already moved
/// on by the merge path or released): nothing written.
#[test]
#[serial_test::serial]
fn a_finished_builder_step_writes_nothing() {
    let (release, edits) = release_with(StepRole::Builder, 605, Some(r#""loom:curated""#));
    assert_eq!(release, ClaimRelease::Untouched);
    assert!(edits.is_empty(), "{edits:?}");
}

/// A Builder step whose issue cannot be read: no write at all, not even the
/// claim (it may be the open-PR hold).
#[test]
#[serial_test::serial]
fn a_builder_step_with_a_failed_read_writes_nothing() {
    let (release, edits) = release_with(StepRole::Builder, 606, None);
    assert_eq!(release, ClaimRelease::Untouched);
    assert!(edits.is_empty(), "{edits:?}");
}
