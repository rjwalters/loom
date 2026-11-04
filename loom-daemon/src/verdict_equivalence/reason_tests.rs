//! Failure reasons and `--repo` pinning for `forge verdict-equivalent` (#13146).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::tests::fake_gh;
use super::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

const REVIEWED: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";

#[test]
fn base_ref_failure_names_exit_status_and_stderr() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("log");
    let gh = fake_gh(dir.path(), &log, &[]);
    let why = pr_base_ref_checked(&gh, Some(dir.path()), 7).unwrap_err();
    assert!(why.contains("status 1"), "{why}");
    assert!(why.contains("fake-gh: no route"), "{why}");
}

#[test]
fn base_ref_unparseable_and_unsafe_are_distinct_reasons() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("log");
    let gh = fake_gh(dir.path(), &log, &[("pr view", "not json")]);
    let why = pr_base_ref_checked(&gh, Some(dir.path()), 7).unwrap_err();
    assert!(why.contains("unparseable"), "{why}");

    let gh = fake_gh(dir.path(), &log, &[("pr view", r#"{"baseRefName":"-x"}"#)]);
    let why = pr_base_ref_checked(&gh, Some(dir.path()), 7).unwrap_err();
    assert!(why.contains("unsafe base ref"), "{why}");
}

#[test]
fn indeterminate_assessment_carries_the_underlying_reasons() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("log");
    let gh = fake_gh(dir.path(), &log, &[]);
    let a = assess_with(true, true, &gh, Some(dir.path()), 7, REVIEWED, HEAD);
    assert_eq!(a.equivalence, Equivalence::Indeterminate);
    let note = a.unavailable_note().unwrap();
    assert!(note.contains("tree: forge compare"), "{note}");
    assert!(note.contains("fake-gh: no route"), "{note}");
    assert!(note.contains("base branch from the forge: gh exited with status 1"), "{note}");
    assert!(note.contains(BASE_UNREAD_TOKEN), "{note}");
}

#[test]
fn pin_repo_rejects_a_malformed_repo_and_none_is_a_no_op() {
    assert!(pin_repo(None).is_ok());
    for bad in ["", "owner", "a/b/c", "../x", "a/..", "a b/c"] {
        assert!(pin_repo(Some(bad)).is_err(), "{bad}");
    }
}

#[test]
#[serial]
fn pinned_repo_reaches_gh_as_repo_flag_and_gh_repo_env() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("log");
    let bin = dir.path().join("env-gh.sh");
    std::fs::write(
        &bin,
        format!(
            "#!/usr/bin/env bash\nprintf 'GH_REPO=%s ARGS=%s\\n' \"$GH_REPO\" \"$*\" >> \"{}\"\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let prior = std::env::var_os("LOOM_REPO");
    pin_repo(Some("acme/widgets")).unwrap();
    let _ = pr_base_ref_checked(&bin, Some(dir.path()), 7);
    let _ = gh_api_checked("verdict.compare", &bin, Some(dir.path()), "repos/{owner}/{repo}/x");
    match prior {
        Some(v) => std::env::set_var("LOOM_REPO", v),
        None => std::env::remove_var("LOOM_REPO"),
    }

    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("--repo acme/widgets"), "{logged}");
    assert_eq!(logged.matches("GH_REPO=acme/widgets").count(), 2, "{logged}");
}
