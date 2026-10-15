//! The self-heal of a checkout that only Loom's own stale install blocks
//! (#11425). Real git in temp dirs, as in the parent module.
//!
//! The clock is injected: a stale operation or lock is one a pass run at
//! `late()` finds, a recent one is found by a pass run at `Utc::now()`.

use std::process::Stdio;

use super::*;

const META: &str =
    r#"{"installed_files": [".loom/scripts/spawn.sh", ".claude/commands/loom/a.md", "CLAUDE.md"]}"#;

fn late() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(2)
}

/// A fixture whose origin and host both have install metadata naming three
/// Loom-managed files, and a non-Loom `README.md`.
fn loom_fixture() -> Fixture {
    let fx = Fixture::new();
    fx.push(".claude/commands/loom/a.md", "a\n");
    fx.push("CLAUDE.md", "claude\n");
    fx.push(".loom/install-metadata.json", META);
    git(&fx.host, &["pull", "--quiet", "--ff-only", "origin", "main"]);
    fx
}

fn pass_at(fx: &Fixture, knobs: &Knobs, memory: &mut Memory, now: DateTime<Utc>) -> CheckoutPass {
    let clock = move || now;
    let env = Env {
        write: knobs.write,
        held: &|| knobs.held,
        gate_in_flight: &*knobs.gate,
        hold: &|_| (!knobs.updating).then(MoveHold::free),
        network: knobs.network,
        backing_off: &|_| false,
        confirmed: &|_, _| None,
        breaker_open: &|| false,
        budget: None,
        elapsed: &|| Duration::ZERO,
        clock: &clock,
    };
    run(&env, std::slice::from_ref(&fx.host), memory)
}

fn heal(fx: &Fixture, now: DateTime<Utc>) -> CheckoutPass {
    pass_at(fx, &Knobs::default(), &mut Memory::default(), now)
}

fn rescue_refs(fx: &Fixture) -> Vec<String> {
    let out = git(
        &fx.host,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/loom/rescue/",
        ],
    );
    out.lines().map(str::to_string).collect()
}

fn stashes(fx: &Fixture) -> String {
    git(&fx.host, &["stash", "list"])
}

fn kinds(pass: &CheckoutPass) -> Vec<&'static str> {
    pass.transitions.iter().map(|t| t.kind).collect()
}

fn healed(state: CheckoutState) -> bool {
    matches!(state, CheckoutState::FastForwarded | CheckoutState::Current)
}

/// Everything "not touched" means: files, index, HEAD, refs, stashes, and the
/// operation and lock markers.
fn fingerprint(fx: &Fixture) -> (Snapshot, String, String, Vec<bool>) {
    let gd = fx.host.join(".git");
    let markers = [
        "rebase-merge",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "index.lock",
    ]
    .iter()
    .map(|m| gd.join(m).exists())
    .collect();
    (snapshot(&fx.host), git(&fx.host, &["for-each-ref"]), stashes(fx), markers)
}

/// Run a pass at `now` and require that it changed nothing and ended in a
/// state other than a repaired one. Returns the state.
fn assert_untouched(fx: &Fixture, now: DateTime<Utc>) -> CheckoutState {
    assert_untouched_with(fx, &Knobs::default(), now)
}

fn assert_untouched_with(fx: &Fixture, knobs: &Knobs, now: DateTime<Utc>) -> CheckoutState {
    let before = fingerprint(fx);
    let found = pass_at(fx, knobs, &mut Memory::default(), now);
    // A fetch moves origin/main in the clone and nothing else, so compare
    // with the remote-tracking refs left out.
    let after = fingerprint(fx);
    assert_eq!(before.0, after.0, "files, index and HEAD are unchanged");
    assert_eq!((before.2, before.3), (after.2, after.3), "stashes and markers are unchanged");
    assert!(rescue_refs(fx).is_empty(), "no rescue ref was made");
    assert!(
        found
            .transitions
            .iter()
            .all(|t| !["repaired", "rescue-ref", "lock-removed"].contains(&t.kind)),
        "{:?}",
        kinds(&found)
    );
    only(&found).state
}

// ----------------------------------------------------------------------------
// Repaired
// ----------------------------------------------------------------------------

#[test]
fn a_stale_resync_commit_diverged_from_a_resynced_origin_is_repaired() {
    let fx = loom_fixture();
    let stranded = {
        fx.commit_locally(".loom/scripts/spawn.sh");
        fx.head()
    };
    let tip = fx.push(".loom/scripts/spawn.sh", "resynced\n");

    let found = heal(&fx, Utc::now());

    assert!(healed(only(&found).state), "{found:?}");
    assert_eq!(fx.head(), tip);
    let refs = rescue_refs(&fx);
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert!(refs[0].ends_with(&stranded), "the rescue ref is at the old HEAD: {refs:?}");
    let k = kinds(&found);
    assert!(k.contains(&"rescue-ref") && k.contains(&"repaired"), "{k:?}");
    let line = found
        .transitions
        .iter()
        .find(|t| t.kind == "repaired")
        .unwrap();
    assert!(line.line.contains(fx.host.to_str().unwrap()), "{}", line.line);
}

#[test]
fn unpushed_loom_only_commits_are_repaired() {
    let fx = loom_fixture();
    fx.commit_locally("CLAUDE.md");
    let stranded = fx.head();

    let found = heal(&fx, Utc::now());

    assert_eq!(only(&found).state, CheckoutState::Current);
    assert_eq!(fx.head(), fx.origin_tip());
    assert!(rescue_refs(&fx)[0].ends_with(&stranded));
}

#[test]
fn dirty_loom_paths_are_stashed_and_repaired_and_untracked_files_survive() {
    let fx = loom_fixture();
    write(&fx.host.join(".loom/scripts/spawn.sh"), "edited\n");
    write(&fx.host.join(".claude/commands/loom/a.md"), "staged\n");
    git(&fx.host, &["add", ".claude/commands/loom/a.md"]);
    fs::remove_file(fx.host.join("CLAUDE.md")).unwrap();
    write(&fx.host.join("notes.txt"), "mine\n");
    write(&fx.host.join("deep/dir/scratch.bin"), "bytes\n");
    let tip = fx.push("README.md", "two\n");
    // The clone already holds the remote's head: the step saw it on an earlier
    // pass, which is how dirt becomes a blocker.
    git(&fx.host, &["fetch", "--quiet", "origin"]);

    let found = heal(&fx, Utc::now());

    assert!(healed(only(&found).state), "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "?? deep/\n?? notes.txt");
    let list = stashes(&fx);
    assert!(list.contains("loom-rescue-"), "{list}");
    let saved = git(&fx.host, &["stash", "show", "--name-only", "stash@{0}"]);
    for path in [
        ".loom/scripts/spawn.sh",
        ".claude/commands/loom/a.md",
        "CLAUDE.md",
    ] {
        assert!(saved.contains(path), "{saved}");
    }
    assert_eq!(rescue_refs(&fx).len(), 1);
    assert_eq!(fs::read_to_string(fx.host.join("notes.txt")).unwrap(), "mine\n");
    assert_eq!(fs::read_to_string(fx.host.join("deep/dir/scratch.bin")).unwrap(), "bytes\n");
}

/// A host with a Loom-only merge left in progress (HEAD on the branch).
fn merge_in_progress(fx: &Fixture) {
    fx.commit_locally(".loom/scripts/spawn.sh");
    fx.push(".loom/scripts/spawn.sh", "resynced\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    let merged = git_in(&fx.host, &["merge", "--no-edit", "origin/main"]);
    assert!(!merged.status.success(), "the merge must conflict");
    assert!(fx.host.join(".git/MERGE_HEAD").exists());
}

#[test]
fn a_stale_loom_only_merge_is_aborted_and_repaired() {
    let fx = loom_fixture();
    merge_in_progress(&fx);

    let found = heal(&fx, late());

    assert!(healed(only(&found).state), "{found:?}");
    assert_eq!(fx.head(), fx.origin_tip());
    assert!(!fx.host.join(".git/MERGE_HEAD").exists());
    assert_eq!(rescue_refs(&fx).len(), 1);
    assert!(kinds(&found).contains(&"abort"));
}

#[test]
fn a_stale_loom_only_rebase_is_aborted_and_repaired() {
    let fx = loom_fixture();
    fx.commit_locally(".loom/scripts/spawn.sh");
    let original = fx.head();
    let tip = fx.push(".loom/scripts/spawn.sh", "resynced\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    let rebased = git_in(&fx.host, &["rebase", "origin/main"]);
    assert!(!rebased.status.success(), "the rebase must conflict");
    assert!(fx.host.join(".git/rebase-merge").exists());

    let found = heal(&fx, late());

    assert!(healed(only(&found).state), "{found:?}");
    assert_eq!(fx.head(), tip);
    assert!(!fx.host.join(".git/rebase-merge").exists());
    let refs = rescue_refs(&fx).join("\n");
    assert!(refs.contains(&original), "the branch tip is kept: {refs}");
}

#[test]
fn a_stale_empty_index_lock_is_removed() {
    let fx = loom_fixture();
    fx.push("README.md", "two\n");
    write(&fx.host.join(".git/index.lock"), "");

    let found = heal(&fx, late());

    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert!(!fx.host.join(".git/index.lock").exists());
    let t = found
        .transitions
        .iter()
        .find(|t| t.kind == "lock-removed")
        .expect("emitted");
    assert!(
        t.line.contains("index.lock") && t.line.contains(fx.host.to_str().unwrap()),
        "{}",
        t.line
    );
    assert!(rescue_refs(&fx).is_empty(), "a lock alone needs no reset");
}

// ----------------------------------------------------------------------------
// Not touched
// ----------------------------------------------------------------------------

#[test]
fn a_commit_touching_a_non_loom_path_is_not_touched() {
    let fx = loom_fixture();
    fx.commit_locally("src/feature.rs");
    fx.push("README.md", "two\n");
    assert_eq!(assert_untouched(&fx, late()), CheckoutState::Diverged);
}

#[test]
fn a_mixed_commit_is_not_touched() {
    let fx = loom_fixture();
    write(&fx.host.join("CLAUDE.md"), "x\n");
    write(&fx.host.join("src/feature.rs"), "fn f() {}\n");
    git(&fx.host, &["add", "-A"]);
    git(&fx.host, &["commit", "--quiet", "-m", "mixed"]);
    fx.push("README.md", "two\n");
    assert_eq!(assert_untouched(&fx, late()), CheckoutState::Diverged);
}

#[test]
fn a_non_loom_dirty_file_is_not_touched_even_beside_loom_ones() {
    let fx = loom_fixture();
    write(&fx.host.join("CLAUDE.md"), "edited\n");
    write(&fx.host.join("README.md"), "mine\n");
    fx.push("src/a.rs", "fn a() {}\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    assert_eq!(assert_untouched(&fx, late()), CheckoutState::Dirty);
}

#[test]
fn dirt_on_a_checkout_that_is_not_behind_is_left_to_the_operator() {
    let fx = loom_fixture();
    write(&fx.host.join("CLAUDE.md"), "my local edit\n");
    assert_eq!(assert_untouched(&fx, late()), CheckoutState::Current);
}

#[test]
fn missing_or_unparseable_install_metadata_repairs_nothing() {
    let plain = Fixture::new();
    plain.commit_locally(".loom/scripts/spawn.sh");
    plain.push("README.md", "two\n");
    assert_eq!(assert_untouched(&plain, late()), CheckoutState::Diverged);

    let broken = Fixture::new();
    broken.push(".loom/install-metadata.json", "{ not json");
    git(&broken.host, &["pull", "--quiet", "--ff-only", "origin", "main"]);
    broken.commit_locally(".loom/scripts/spawn.sh");
    broken.push("README.md", "two\n");
    assert_eq!(assert_untouched(&broken, late()), CheckoutState::Diverged);
}

#[test]
fn a_recent_operation_is_not_touched() {
    let fx = loom_fixture();
    merge_in_progress(&fx);
    assert_eq!(assert_untouched(&fx, Utc::now()), CheckoutState::MidOperation);
}

#[test]
fn an_operation_with_a_non_loom_commit_is_not_touched() {
    let fx = loom_fixture();
    fx.commit_locally("src/feature.rs");
    fx.commit_locally(".loom/scripts/spawn.sh");
    fx.push(".loom/scripts/spawn.sh", "resynced\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    let rebased = git_in(&fx.host, &["rebase", "origin/main"]);
    assert!(!rebased.status.success());
    assert!(assert_untouched(&fx, late()) != CheckoutState::FastForwarded);
    assert!(fx.host.join(".git/rebase-merge").exists());
}

#[test]
fn a_live_git_process_in_the_repo_blocks_the_repair() {
    let fx = loom_fixture();
    merge_in_progress(&fx);
    write(&fx.host.join(".git/index.lock"), "");
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(&fx.host)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();

    let state = assert_untouched(&fx, late());

    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(state, CheckoutState::MidOperation);
}

#[test]
fn a_young_or_non_empty_index_lock_is_not_touched() {
    let fx = loom_fixture();
    fx.push("README.md", "two\n");
    write(&fx.host.join(".git/index.lock"), "");
    assert_untouched(&fx, Utc::now());
    write(&fx.host.join(".git/index.lock"), "held\n");
    assert_untouched(&fx, late());
    assert_eq!(fs::read_to_string(fx.host.join(".git/index.lock")).unwrap(), "held\n");
}

#[test]
fn an_untracked_file_where_origin_writes_one_is_not_touched() {
    let fx = loom_fixture();
    fx.commit_locally("CLAUDE.md");
    fx.push("notes.txt", "from origin\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    write(&fx.host.join("notes.txt"), "mine\n");
    assert_untouched(&fx, late());
    assert_eq!(fs::read_to_string(fx.host.join("notes.txt")).unwrap(), "mine\n");
}

#[test]
fn a_rescue_ref_that_cannot_be_made_leaves_the_checkout_unmodified() {
    let fx = loom_fixture();
    fx.commit_locally("CLAUDE.md");
    fx.push("README.md", "two\n");
    // A file where the ref's directory would be: `update-ref` cannot create it.
    write(&fx.host.join(".git/refs/loom"), "in the way\n");
    let before = fingerprint(&fx);

    let found = heal(&fx, Utc::now());

    assert!(kinds(&found).contains(&"repair-failed"), "{:?}", kinds(&found));
    assert_eq!(fingerprint(&fx).0.files, before.0.files);
    assert_ne!(fx.head(), fx.origin_tip(), "nothing was reset");
    // The planted file also breaks the pass's own fetch; either way it ends there.
    assert!(matches!(
        only(&found).state,
        CheckoutState::Diverged | CheckoutState::FetchFailed
    ));
}

#[test]
fn no_write_gate_lets_a_repair_write_anything() {
    let gates: Vec<(&str, Knobs)> = vec![
        (
            "autoApply off",
            Knobs {
                write: false,
                ..Knobs::default()
            },
        ),
        (
            "host held",
            Knobs {
                held: Some("draining"),
                ..Knobs::default()
            },
        ),
        (
            "gate in flight",
            Knobs {
                gate: Box::new(|_| true),
                ..Knobs::default()
            },
        ),
        (
            "self-update",
            Knobs {
                updating: true,
                ..Knobs::default()
            },
        ),
    ];
    for (name, knobs) in gates {
        let fx = loom_fixture();
        fx.commit_locally("CLAUDE.md");
        fx.push("README.md", "two\n");
        write(&fx.host.join(".git/index.lock"), "");
        let state = assert_untouched_with(&fx, &knobs, late());
        assert_ne!(state, CheckoutState::FastForwarded, "{name}");
    }
}

// ----------------------------------------------------------------------------
// Rescue refs and escalation
// ----------------------------------------------------------------------------

#[test]
fn old_rescue_refs_are_pruned_and_everything_else_is_kept() {
    let fx = loom_fixture();
    let head = fx.head();
    let recent = (Utc::now() - chrono::Duration::days(1))
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    for name in [
        "refs/loom/rescue/20200101T000000Z".to_string(),
        "refs/loom/rescue/20200101T000000Z-branch".to_string(),
        format!("refs/loom/rescue/{recent}"),
        "refs/loom/rescue/not-a-time".to_string(),
        "refs/loom/other/20200101T000000Z".to_string(),
        "refs/heads/20200101T000000Z".to_string(),
    ] {
        git(&fx.host, &["update-ref", &name, &head]);
    }

    let found = heal(&fx, Utc::now());

    assert_eq!(
        kinds(&found)
            .iter()
            .filter(|k| **k == "rescue-pruned")
            .count(),
        2
    );
    let left = git(
        &fx.host,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/loom/",
            "refs/heads/",
        ],
    );
    assert!(!left.contains("rescue/20200101T000000Z"), "{left}");
    for kept in [
        recent.as_str(),
        "not-a-time",
        "refs/loom/other/",
        "refs/heads/2020",
    ] {
        assert!(left.contains(kept), "{kept} in {left}");
    }
}

#[test]
fn a_hold_that_cannot_be_repaired_is_escalated_once() {
    let fx = loom_fixture();
    write(&fx.host.join("README.md"), "mine\n");
    fx.push("src/a.rs", "fn a() {}\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    let t = Utc::now();
    let mut memory = Memory::default();
    let knobs = Knobs::default();
    let asks = |pass: &CheckoutPass| {
        pass.transitions
            .iter()
            .filter(|t| t.kind == "escalated")
            .count()
    };

    let first = pass_at(&fx, &knobs, &mut memory, t);
    let soon = pass_at(&fx, &knobs, &mut memory, t + chrono::Duration::minutes(30));
    let due = pass_at(&fx, &knobs, &mut memory, t + chrono::Duration::minutes(150));
    let again = pass_at(&fx, &knobs, &mut memory, t + chrono::Duration::minutes(180));

    assert_eq!((asks(&first), asks(&soon), asks(&due), asks(&again)), (0, 0, 1, 0));
    let ask = due
        .transitions
        .iter()
        .find(|t| t.kind == "escalated")
        .unwrap();
    assert_eq!(ask.level, Level::Error);
    for needle in [
        fx.host.to_str().unwrap(),
        "dirty",
        "git -C",
        "reset --hard origin/main",
    ] {
        assert!(ask.line.contains(needle), "{needle} in {}", ask.line);
    }
}

// ----------------------------------------------------------------------------
// #11989: surfaces `installed_files` does not list, and a detached HEAD
// ----------------------------------------------------------------------------

/// Far enough ahead that a detached HEAD has sat past
/// `managed::detached_after()`'s six-hour default.
fn much_later() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(7)
}

/// A local resync-shaped commit writing surfaces a release added after the
/// last full install: none of them is in `installed_files`.
fn commit_unlisted_surfaces(fx: &Fixture) {
    for path in [
        ".agents/skills/loom-new/SKILL.md",
        ".loom/scripts/tests/test-new.sh",
        ".loom/docs/new.md",
    ] {
        write(&fx.host.join(path), "resynced locally\n");
    }
    git(&fx.host, &["add", "-A"]);
    git(
        &fx.host,
        &[
            "commit",
            "--quiet",
            "-m",
            "chore: resync installed Loom surfaces",
        ],
    );
}

#[test]
fn a_stranded_resync_commit_on_surfaces_installed_files_misses_is_repaired() {
    let fx = loom_fixture();
    commit_unlisted_surfaces(&fx);
    let stranded = fx.head();
    let tip = fx.push(".loom/scripts/spawn.sh", "resynced\n");

    let found = heal(&fx, Utc::now());

    assert!(healed(only(&found).state), "{found:?}");
    assert_eq!(fx.head(), tip);
    assert!(rescue_refs(&fx)[0].ends_with(&stranded));
}

#[test]
fn a_surface_path_pinned_in_resync_ignore_is_not_touched() {
    let fx = loom_fixture();
    fx.push(".loom/resync-ignore", "# deliberate\nscripts/tests/test-new.sh\n");
    git(&fx.host, &["pull", "--quiet", "--ff-only", "origin", "main"]);
    commit_unlisted_surfaces(&fx);
    fx.push("README.md", "two\n");
    assert_eq!(assert_untouched(&fx, late()), CheckoutState::Diverged);
}

#[test]
fn a_non_loom_skill_or_command_is_not_counted_as_a_surface() {
    let fx = loom_fixture();
    write(&fx.host.join(".agents/skills/custom/SKILL.md"), "mine\n");
    write(&fx.host.join(".claude/commands/repo/x.md"), "mine\n");
    git(&fx.host, &["add", "-A"]);
    git(
        &fx.host,
        &[
            "commit",
            "--quiet",
            "-m",
            "chore: resync installed Loom surfaces",
        ],
    );
    fx.push("README.md", "two\n");
    assert_eq!(assert_untouched(&fx, late()), CheckoutState::Diverged);
}

/// The host's checkout detached at its current HEAD, with origin one commit
/// ahead and the clone already holding it.
fn detached_behind(fx: &Fixture) -> String {
    git(&fx.host, &["checkout", "--quiet", "--detach", "HEAD"]);
    let tip = fx.push("README.md", "two\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    tip
}

#[test]
fn a_detached_head_left_behind_for_hours_is_put_back_on_the_branch() {
    let fx = loom_fixture();
    let tip = detached_behind(&fx);

    let found = heal(&fx, much_later());

    assert!(healed(only(&found).state), "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(
        git(&fx.host, &["rev-parse", "--abbrev-ref", "main@{upstream}"]),
        "origin/main",
        "the branch tracks origin again"
    );
    assert!(kinds(&found).contains(&"reattached"), "{:?}", kinds(&found));
}

#[test]
fn a_detached_head_with_loom_only_commits_is_put_back_and_rescued() {
    let fx = loom_fixture();
    git(&fx.host, &["checkout", "--quiet", "--detach", "HEAD"]);
    commit_unlisted_surfaces(&fx);
    let stranded = fx.head();
    let tip = fx.push("README.md", "two\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);

    let found = heal(&fx, much_later());

    assert!(healed(only(&found).state), "{found:?}");
    assert_eq!(fx.head(), tip);
    assert_eq!(git(&fx.host, &["symbolic-ref", "--short", "HEAD"]), "main");
    let refs = rescue_refs(&fx);
    assert!(refs.iter().any(|r| r.ends_with(&stranded)), "{refs:?}");
}

#[test]
fn a_recently_detached_head_is_not_touched() {
    let fx = loom_fixture();
    detached_behind(&fx);
    assert_eq!(assert_untouched(&fx, late()), CheckoutState::WrongBranch);
}

#[test]
fn a_detached_head_at_origins_tip_is_not_touched() {
    let fx = loom_fixture();
    fx.push("README.md", "two\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    git(&fx.host, &["checkout", "--quiet", "--detach", "origin/main"]);
    assert_eq!(assert_untouched(&fx, much_later()), CheckoutState::WrongBranch);
}

#[test]
fn a_detached_head_with_a_non_loom_commit_is_not_touched() {
    let fx = loom_fixture();
    git(&fx.host, &["checkout", "--quiet", "--detach", "HEAD"]);
    fx.commit_locally("src/feature.rs");
    fx.push("README.md", "two\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    assert_eq!(assert_untouched(&fx, much_later()), CheckoutState::WrongBranch);
}

#[test]
fn a_checkout_on_another_branch_is_not_this_repair() {
    let fx = loom_fixture();
    git(&fx.host, &["checkout", "--quiet", "-b", "pr6597"]);
    fx.push("README.md", "two\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    assert_eq!(assert_untouched(&fx, much_later()), CheckoutState::WrongBranch);
}
