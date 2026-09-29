//! Unit tests for the in-worktree predicate (#8195 slice 11).
//!
//! Every case runs against a **real throwaway repo with a real linked
//! worktree**, because the whole decision is a question about what git reports
//! for `--git-dir` / `--git-common-dir` from a given directory, and a mock of
//! git would be a mock of the thing under test. The four positions a caller can
//! actually stand in — the primary clone, a subdirectory of it, a linked
//! worktree, a subdirectory of one — are the grammar: the retired shell was
//! constant-true across all four, so "it answers correctly *here*" is not
//! evidence for any of the others.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-wt-check-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("LC_ALL", "C")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A repo with one commit on `main` and a linked worktree for
/// `feature/issue-42` at `.loom/worktrees/issue-42`, laid out exactly the way
/// `worktree.sh` lays one out.
struct Fixture {
    repo: PathBuf,
    worktree: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let root = tmpdir(tag);
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    git(&repo, &["init", "-q", "--initial-branch=main", "."]);
    fs::write(repo.join("f"), "hi\n").expect("write f");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    fs::create_dir_all(repo.join("sub/deeper")).expect("mkdir sub");
    let worktree = repo.join(".loom/worktrees/issue-42");
    fs::create_dir_all(worktree.parent().expect("parent")).expect("mkdir worktrees");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature/issue-42",
            worktree.to_str().expect("utf8"),
            "main",
        ],
    );
    fs::create_dir_all(worktree.join("nested")).expect("mkdir nested");
    Fixture { repo, worktree }
}

// ---------------------------------------------------------------------------
// The predicate, in all four positions
// ---------------------------------------------------------------------------

#[test]
fn primary_clone_root_is_not_a_worktree() {
    let f = fixture("primary-root");
    let loc = locate(&f.repo);
    assert!(
        !loc.linked_worktree,
        "the primary clone's root must not read as a linked worktree — this is \
         the answer the retired shell got wrong, and it is what made \
         `worktree.sh --check`'s exit 1 unreachable"
    );
    assert_eq!(loc.main_workspace, None);
    assert_eq!(loc.worktree_path, None);
}

#[test]
fn primary_clone_subdirectory_is_not_a_worktree() {
    // The position where git answers `--git-common-dir` as `../.git` and
    // `--git-dir` as an ABSOLUTE path. The retired comparison read that
    // relative answer against an absolute `<toplevel>/.git` and said "worktree".
    let f = fixture("primary-sub");
    assert!(!locate(&f.repo.join("sub")).linked_worktree);
    assert!(!locate(&f.repo.join("sub/deeper")).linked_worktree);
}

#[test]
fn linked_worktree_is_a_worktree_and_names_the_main_workspace() {
    let f = fixture("linked");
    let loc = locate(&f.worktree);
    assert!(loc.linked_worktree);
    assert_eq!(
        loc.main_workspace.as_deref(),
        Some(f.repo.as_path()),
        "the main workspace is the parent of the common git dir"
    );
    assert_eq!(loc.worktree_path.as_deref(), Some(f.worktree.to_str().expect("utf8")));
    assert_eq!(loc.branch.as_deref(), Some("feature/issue-42"));
}

#[test]
fn subdirectory_of_a_linked_worktree_still_resolves_to_the_worktree() {
    let f = fixture("linked-sub");
    let loc = locate(&f.worktree.join("nested"));
    assert!(loc.linked_worktree);
    assert_eq!(loc.main_workspace.as_deref(), Some(f.repo.as_path()));
    assert_eq!(
        loc.worktree_path.as_deref(),
        Some(f.worktree.to_str().expect("utf8")),
        "`--show-toplevel` is the worktree root, not the subdirectory"
    );
}

#[test]
fn outside_any_repository_is_not_a_worktree() {
    let root = tmpdir("outside");
    let loc = locate(&root);
    assert!(
        !loc.linked_worktree,
        "git answering nothing is never evidence OF a linked worktree"
    );
    assert_eq!(loc.main_workspace, None);
}

// ---------------------------------------------------------------------------
// The two hazards the physical comparison exists for
// ---------------------------------------------------------------------------

#[test]
fn a_repo_reached_through_a_symlink_answers_the_same_as_the_real_path() {
    // Slice 10 found this same logical-path comparison refusing a live worktree
    // in a repo reached through a symlink. Both sides are canonicalized here,
    // so the symlinked spelling must produce identical answers.
    let f = fixture("symlink");
    let link = f.repo.parent().expect("parent").join("link-to-repo");
    if std::os::unix::fs::symlink(&f.repo, &link).is_err() {
        return; // no symlink support — nothing to assert
    }
    assert!(!locate(&link).linked_worktree, "the symlinked primary clone is still primary");
    let via_link = link.join(".loom/worktrees/issue-42");
    let loc = locate(&via_link);
    assert!(loc.linked_worktree, "the symlinked worktree path is still a worktree");
    assert_eq!(
        loc.main_workspace.as_deref(),
        Some(f.repo.as_path()),
        "the main workspace resolves to the real path, so the caller's `cd` lands \
         in one canonical place however the repo was spelled"
    );
}

#[test]
fn a_separate_git_dir_checkout_is_not_a_worktree() {
    // `git init --separate-git-dir` leaves `.git` as a FILE in the working
    // tree, exactly like a linked worktree does — so the tempting
    // `[[ -f .git ]]` shortcut would call this a worktree and navigate to the
    // wrong directory. `--git-dir` == `--git-common-dir` here, so the
    // predicate this module uses says "primary", correctly.
    let root = tmpdir("separate-git-dir");
    let work = root.join("work");
    let gitdir = root.join("elsewhere.git");
    fs::create_dir_all(&work).expect("mkdir work");
    let out = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("--initial-branch=main")
        .arg(format!("--separate-git-dir={}", gitdir.display()))
        .arg(&work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git init");
    if !out.status.success() {
        return; // git too old for --separate-git-dir on init
    }
    assert!(work.join(".git").is_file(), "fixture precondition: .git is a file");
    assert!(!locate(&work).linked_worktree);
}

// ---------------------------------------------------------------------------
// The `--check` verb's two output shapes
// ---------------------------------------------------------------------------

#[test]
fn report_exits_1_in_the_primary_clone_and_0_in_a_worktree() {
    let f = fixture("report");
    assert_eq!(report(&f.repo), 1, "the primary clone is not a worktree");
    assert_eq!(report(&f.worktree), 0);
}

// ---------------------------------------------------------------------------
// The create path's record stream
// ---------------------------------------------------------------------------

#[test]
fn no_records_at_all_when_not_in_a_worktree() {
    let f = fixture("records-primary");
    let loc = locate(&f.repo);
    assert!(records(&loc, false).is_empty());
    assert!(records(&loc, true).is_empty());
}

#[test]
fn the_record_stream_replays_the_retired_banner_in_order() {
    let f = fixture("records-order");
    let loc = locate(&f.worktree);
    let got: Vec<(&str, String)> = records(&loc, false)
        .into_iter()
        .map(|(l, t)| (l.token(), t))
        .collect();
    let want = vec![
        (
            "WARNING",
            "Currently in a worktree, auto-navigating to main workspace...".to_string(),
        ),
        ("BLANK", String::new()),
        ("PLAIN", "Current worktree:".to_string()),
        ("PLAIN", format!("  Path: {}", f.worktree.display())),
        ("PLAIN", "  Branch: feature/issue-42".to_string()),
        ("BLANK", String::new()),
        ("INFO", format!("Found main workspace: {}", f.repo.display())),
        ("IN_WORKTREE", "true".to_string()),
        ("MAIN_WORKSPACE", f.repo.display().to_string()),
    ];
    assert_eq!(got, want);
}

#[test]
fn quiet_emits_only_the_two_data_records() {
    // `--json` mode: the retired shell wrapped every one of those messages in
    // `if [[ "$JSON_OUTPUT" != "true" ]]`, so the port must print none of them.
    let f = fixture("records-quiet");
    let loc = locate(&f.worktree);
    let got: Vec<&str> = records(&loc, true).iter().map(|(l, _)| l.token()).collect();
    assert_eq!(got, vec!["IN_WORKTREE", "MAIN_WORKSPACE"]);
}

#[test]
fn every_record_is_a_single_line_with_exactly_one_tab() {
    // The wrapper reads this with `IFS=$'\t' read -r level text`, which splits
    // on the FIRST tab and would silently swallow a second field; an embedded
    // newline would be read as a whole extra record.
    let f = fixture("records-shape");
    for (level, text) in records(&locate(&f.worktree), false) {
        assert!(!level.token().is_empty());
        assert!(!text.contains('\n'), "record text must be one line: {text:?}");
        assert!(!text.contains('\t'), "record text must not contain a tab: {text:?}");
    }
}

#[test]
fn a_worktree_whose_path_contains_a_space_survives_the_record_stream() {
    // #7858's class. A worktree path with a space was truncated by every
    // `awk '{print $2}'` in this script's history; here it must arrive at the
    // shell whole, which is what lets the wrapper `cd` into it.
    let root = tmpdir("spacey");
    let repo = root.join("re po");
    fs::create_dir_all(&repo).expect("mkdir repo");
    git(&repo, &["init", "-q", "--initial-branch=main", "."]);
    fs::write(repo.join("f"), "hi\n").expect("write f");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let wt = repo.join("work trees/issue 42");
    fs::create_dir_all(wt.parent().expect("parent")).expect("mkdir");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature/issue-42",
            wt.to_str().expect("utf8"),
            "main",
        ],
    );
    let loc = locate(&wt);
    assert!(loc.linked_worktree);
    assert_eq!(loc.main_workspace.as_deref(), Some(repo.as_path()));
    let recs = records(&loc, true);
    assert_eq!(
        recs.iter()
            .find(|(l, _)| *l == Level::MainWorkspace)
            .map(|(_, t)| t.as_str()),
        Some(repo.to_str().expect("utf8")),
        "the main workspace arrives whole, spaces included"
    );
}
