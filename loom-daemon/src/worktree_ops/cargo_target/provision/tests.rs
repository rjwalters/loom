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
#[cfg(unix)]
fn realish_resolves_a_symlinked_prefix_even_when_the_leaf_does_not_exist_yet() {
    // The root cause behind `an_unredirected_host_is_a_no_op_even_when_enabled`
    // flaking on macOS (issue #9194): `std::fs::canonicalize` fails outright
    // on a path whose leaf does not exist yet, so the old `realish` fell back
    // to the RAW path for the not-yet-created side of a comparison while the
    // EXISTING side resolved through a symlinked prefix (macOS's `/var` ->
    // `/private/var`) — silently breaking the `starts_with` containment check
    // this module relies on. Exercise that geometry directly, with an
    // explicit symlink rather than relying on the host's own tmp layout, so
    // the regression holds on every unix host this module runs on, not just
    // one whose ambient tmp happens to be symlinked.
    let tmp = tempfile::tempdir().expect("tempdir");
    let real = tmp.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // `link/child` does not exist yet.
    let via_link = realish(&link.join("child"));
    let via_real = real.canonicalize().unwrap().join("child");
    assert_eq!(
        via_link, via_real,
        "a not-yet-existing child of a symlinked prefix must resolve consistently with \
         the same child under the symlink's real target"
    );

    // And the containment check this exists to protect must see it: a
    // not-yet-existing path under the resolved worktree is still recognised
    // as living inside it.
    let worktree_real = realish(&link);
    assert!(
        via_link.starts_with(&worktree_real),
        "{via_link:?} should be recognised as inside {worktree_real:?}"
    );
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

// ---------------------------------------------------------------------------
// spawn_target_dir — the delivery half, called from `worker_spawn::run`
// ---------------------------------------------------------------------------
//
// Every case goes through `spawn_target_dir_with` + `shared()`, never the
// process-global `CARGO_TARGET_DIR`: the happy path needs a REDIRECTED host
// (`planned_dir` refuses when cargo's output would already land inside the
// worktree, so an un-redirected tempdir answers `None` for the right reason and
// proves nothing about the wiring), and setting that env var to arrange one is
// the global-env race this module's header warns about.

#[test]
fn a_claim_owning_spawn_on_an_opted_in_redirected_host_gets_its_own_target_dir() {
    let (tmp, repo, _wt) = fixture(741);
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    let dir =
        with_opt_in(Some("1"), || spawn_target_dir_with(&repo, Some("741"), false, &shared(&root)))
            .expect("a claim-owning spawn on a redirected host gets a directory");

    assert_eq!(dir, root.join("wt").join("issue-741"));
    // Created, not merely named: a CARGO_TARGET_DIR cargo cannot write to would
    // fail every build in the sweep.
    assert!(dir.is_dir(), "{} was named but not created", dir.display());
}

#[test]
fn a_spawn_that_owns_no_claim_keeps_the_hosts_own_cargo_resolution() {
    // A role-runner tick or an operator's interactive spawn: no single worktree
    // to attribute a target dir to, so the host's resolution is left alone even
    // with the feature on and a redirected host.
    let (tmp, repo, _wt) = fixture(742);
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    assert_eq!(
        with_opt_in(Some("1"), || spawn_target_dir_with(&repo, None, false, &shared(&root))),
        None
    );
}

#[test]
fn the_copy_re_executed_inside_a_containment_container_does_not_re_derive() {
    // It already received the outer spawn's answer as an explicit
    // `-e CARGO_TARGET_DIR=…`; re-deriving would resolve against a cargo
    // configuration the container may not have been given.
    let (tmp, repo, _wt) = fixture(743);
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    assert_eq!(
        with_opt_in(Some("1"), || spawn_target_dir_with(&repo, Some("743"), true, &shared(&root))),
        None
    );
}

#[test]
fn the_spawn_seam_is_off_by_default_and_a_no_op_outside_a_cargo_repo() {
    let (tmp, repo, _wt) = fixture(744);
    let root = tmp.path().join("shared");
    std::fs::create_dir_all(&root).unwrap();

    // Feature off (the default) — nothing, however redirected the host is.
    assert_eq!(
        with_opt_in(None, || spawn_target_dir_with(&repo, Some("744"), false, &shared(&root))),
        None
    );

    // Opted in, but the workspace carries no manifest: nothing here ever builds
    // with cargo, so there is nothing to isolate.
    let bare = tmp.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(
        with_opt_in(Some("1"), || spawn_target_dir_with(&bare, Some("744"), false, &shared(&root))),
        None
    );
}
