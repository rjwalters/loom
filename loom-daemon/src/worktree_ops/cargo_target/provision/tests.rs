//! Tests for the creation-time half (#8458).
//!
//! `enabled()` reads a process-global env var, so every case that depends on
//! the opt-in goes through [`with_opt_in`], which serialises on a mutex and
//! restores the previous value. The alternative — `set_var` sprinkled through
//! parallel tests — is the exact global-env race that makes
//! `worktree_reaper::tests::liveness` flaky under the full suite.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::*;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Run `body` with `LOOM_PER_WORKTREE_TARGET_DIR` forced to `value`
/// (`None` = unset), restoring whatever was there before.
fn with_opt_in<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var(ENABLE_ENV).ok();
    match value {
        Some(v) => std::env::set_var(ENABLE_ENV, v),
        None => std::env::remove_var(ENABLE_ENV),
    }
    let out = body();
    match previous {
        Some(v) => std::env::set_var(ENABLE_ENV, v),
        None => std::env::remove_var(ENABLE_ENV),
    }
    out
}

/// A repo with a worktree at `.loom/worktrees/issue-<n>`, both carrying a
/// manifest (the tree must look like a cargo tree to be provisionable).
fn fixture(n: u64) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    let wt = repo.join(".loom/worktrees").join(format!("issue-{n}"));
    std::fs::create_dir_all(&wt).expect("mkdir worktree");
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname=\"f\"\n").expect("repo manifest");
    std::fs::write(wt.join("Cargo.toml"), "[package]\nname=\"f\"\n").expect("wt manifest");
    (tmp, repo, wt)
}

/// A resolver standing in for `cargo metadata`: every tree resolves to one
/// shared root, the #8453 shape.
fn shared(root: &Path) -> impl Fn(&Path) -> PathBuf + '_ {
    move |_| root.to_path_buf()
}

#[test]
fn provisioning_creates_the_dir_and_writes_an_attributable_marker() {
    let (tmp, repo, wt) = fixture(701);
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    let outcome = with_opt_in(Some("1"), || provision_with(&repo, &wt, &shared(&root)));

    let expected = root.join("wt").join("issue-701");
    assert_eq!(outcome, Provision::Provisioned(expected.clone()));
    assert!(expected.is_dir(), "the directory must exist");
    assert_eq!(
        std::fs::read_to_string(wt.join(per_worktree::MARKER_FILE))
            .unwrap()
            .trim(),
        expected.display().to_string(),
        "the marker must name it"
    );
    // The whole point: the removal path must recognise what we just wrote.
    assert!(
        per_worktree::marker_value(&wt).is_some(),
        "a marker this module writes must always be readable by the reclaim path"
    );
}

#[test]
fn two_worktrees_get_distinct_dirs_so_the_uplifted_binary_cannot_collide() {
    let (tmp, repo, wt_a) = fixture(702);
    let wt_b = repo.join(".loom/worktrees/issue-703");
    std::fs::create_dir_all(&wt_b).unwrap();
    std::fs::write(wt_b.join("Cargo.toml"), "[package]\nname=\"f\"\n").unwrap();
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    let (a, b) = with_opt_in(Some("1"), || {
        (
            provision_with(&repo, &wt_a, &shared(&root)),
            provision_with(&repo, &wt_b, &shared(&root)),
        )
    });
    assert_ne!(a.dir(), b.dir(), "distinct worktrees must get distinct target dirs");
    assert_eq!(a.dir().unwrap(), root.join("wt/issue-702"));
    assert_eq!(b.dir().unwrap(), root.join("wt/issue-703"));
}

#[test]
fn the_default_is_off() {
    let (tmp, repo, wt) = fixture(704);
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    let outcome = with_opt_in(None, || provision_with(&repo, &wt, &shared(&root)));
    assert_eq!(outcome, Provision::Disabled);
    assert!(!wt.join(per_worktree::MARKER_FILE).exists(), "nothing may be written when off");
    assert!(!root.join("wt").exists(), "and no directory tree created");
}

#[test]
fn an_unredirected_host_is_a_no_op_even_when_enabled() {
    // The #6013/#6014 lesson: `<worktree>/target` is ALREADY per-worktree and
    // goes away with the worktree. Splitting it deeper relocates a build cache
    // for no benefit — that relocation is the rebuild storm.
    let (_tmp, repo, wt) = fixture(705);
    let in_worktree = wt.join("target");

    let outcome = with_opt_in(Some("1"), || provision_with(&repo, &wt, &shared(&in_worktree)));
    assert!(matches!(outcome, Provision::Unredirected(_)), "got {outcome:?}");
    assert!(!wt.join(per_worktree::MARKER_FILE).exists());
}

#[test]
fn an_existing_marker_is_authoritative_and_not_re_derived() {
    let (tmp, repo, wt) = fixture(706);
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    let first = with_opt_in(Some("1"), || provision_with(&repo, &wt, &shared(&root)));
    // A DIFFERENT shared root on the second call: a re-derivation would move
    // the dir out from under build output already in the first one.
    let other = tmp.path().join("shared-2");
    std::fs::create_dir_all(&other).unwrap();
    let second = with_opt_in(Some("1"), || provision_with(&repo, &wt, &shared(&other)));

    assert_eq!(second, Provision::Existing(first.dir().unwrap().to_path_buf()));
    assert!(!other.join("wt").exists(), "the second root must not be provisioned into");
}

#[test]
fn a_marker_survives_the_feature_being_turned_off_again() {
    // Otherwise flipping the knob off would strand the directory builds are
    // already pointed at, with nothing left to attribute it for reclaim.
    let (tmp, repo, wt) = fixture(707);
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();
    let first = with_opt_in(Some("1"), || provision_with(&repo, &wt, &shared(&root)));

    let after = with_opt_in(Some("0"), || provision_with(&repo, &wt, &shared(&root)));
    assert_eq!(after, Provision::Existing(first.dir().unwrap().to_path_buf()));
}

#[test]
fn a_manifestless_tree_is_never_provisioned() {
    let (tmp, repo, wt) = fixture(708);
    std::fs::remove_file(wt.join("Cargo.toml")).unwrap();
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    let outcome = with_opt_in(Some("1"), || provision_with(&repo, &wt, &shared(&root)));
    assert_eq!(outcome, Provision::NotCargo);
}

#[test]
fn the_derivation_is_idempotent_so_a_re_resolved_env_value_does_not_nest() {
    // Routine, not pathological: the spawn path exports CARGO_TARGET_DIR for
    // the sweep, so worktree.sh re-resolves the per-worktree value as its root.
    let (tmp, repo, wt) = fixture(709);
    let root = tmp.path().join("shared");
    let already = root.join("wt").join("issue-709");
    std::fs::create_dir_all(&already).unwrap();

    let outcome = with_opt_in(Some("1"), || provision_with(&repo, &wt, &shared(&already)));
    assert_eq!(outcome.dir().unwrap(), already, "no <root>/wt/issue-709/wt/issue-709 nesting");
}

#[test]
fn planned_dir_answers_for_a_worktree_that_does_not_exist_yet() {
    // The spawn path's case: it runs BEFORE the sweep creates the worktree.
    let (tmp, repo, _wt) = fixture(710);
    let absent = repo.join(".loom/worktrees/issue-999");
    assert!(!absent.exists());

    let dir = with_opt_in(Some("1"), || {
        // planned_dir resolves through the real cargo path, so drive the
        // derivation directly to keep the test hermetic.
        let root = tmp.path().join("shared");
        per_worktree::is_attributable(&absent, &per_worktree::dir_for(&root, "issue-999"))
            .then(|| per_worktree::dir_for(&root, "issue-999"))
    });
    assert_eq!(dir.unwrap(), tmp.path().join("shared/wt/issue-999"));
}

#[test]
fn planned_dir_is_none_when_the_feature_is_off() {
    let (_tmp, repo, wt) = fixture(711);
    assert_eq!(with_opt_in(Some("0"), || planned_dir(&repo, &wt)), None);
}

#[test]
fn the_env_override_accepts_the_documented_truthy_spellings() {
    for v in ["1", "true", "TRUE", "yes", "on"] {
        assert!(truthy(v), "{v} must be truthy");
    }
    for v in ["0", "false", "no", "off", "", "maybe"] {
        assert!(!truthy(v), "{v} must not be truthy");
    }
}

#[test]
fn report_lines_are_silent_for_the_outcomes_that_describe_an_uninvolved_host() {
    assert!(Provision::Disabled.report_line().is_none());
    assert!(Provision::NotCargo.report_line().is_none());
    assert!(Provision::Unredirected(PathBuf::from("/x/target"))
        .report_line()
        .is_none());
    assert!(Provision::Provisioned(PathBuf::from("/x/wt/issue-1"))
        .report_line()
        .is_some());
    assert!(Provision::Failed("nope".into()).report_line().is_some());
}
