//! Tests for the orphan-stack replacement (#12767). Pure decisions on values;
//! the git half on real repositories (a bare "origin", an "author" clone that
//! builds the stack, and the clone Doctor runs in).

use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

// --- pure decisions ----------------------------------------------------------

#[test]
fn the_replaced_marker_round_trips_and_the_newest_wins() {
    assert_eq!(replaced_marker(41), "<!-- loom:orphan-stack replaced by=41 -->");
    let bodies = vec![
        format!("link\n{}", replaced_marker(41)),
        "unrelated".to_string(),
        replaced_marker(57),
    ];
    assert_eq!(replaced_by(&bodies), Some(57));
    assert_eq!(replaced_by(&bodies[..2]), Some(41));
    assert_eq!(
        replaced_by(&["<!-- loom:orphan-stack flagged parent=1 head=abc -->".to_string()]),
        None
    );
    assert_eq!(replaced_by(&["<!-- loom:orphan-stack replaced by=0 -->".to_string()]), None);
    assert_eq!(replaced_by(&["<!-- loom:orphan-stack replaced by=x -->".to_string()]), None);
}

#[test]
fn the_replacement_branch_is_a_function_of_the_child_and_not_a_stack_parent() {
    assert_eq!(replacement_branch(12), "orphan-replace/pr-12");
    assert_eq!(replacement_branch(12), replacement_branch(12));
    assert!(crate::refname::check_refname(&replacement_branch(12)).is_ok());
    assert!(!crate::merge_pr::stacked_children::is_stackable_parent_branch(
        &replacement_branch(12)
    ));
}

#[test]
fn the_original_is_closed_only_after_the_replacement_and_both_links_and_never_over_descendants() {
    assert_eq!(close_decision(None, true, &[]), CloseDecision::NoReplacement);
    assert_eq!(close_decision(Some(9), false, &[]), CloseDecision::LinksMissing);
    assert_eq!(
        close_decision(Some(9), true, &[30, 20, 30]),
        CloseDecision::BlockedByDescendants(vec![20, 30])
    );
    assert_eq!(close_decision(Some(9), true, &[]), CloseDecision::Close);
}

#[test]
fn every_refusal_has_a_token_and_a_message() {
    for r in [
        Refusal::BadSha("x".into()),
        Refusal::MissingHead("h".into()),
        Refusal::ParentNotAncestor,
        Refusal::EmptyRange,
        Refusal::MergeInRange,
        Refusal::Conflict(vec!["a.txt".into()]),
        Refusal::BranchTaken("taken".into()),
    ] {
        assert!(!r.token().is_empty() && !r.message().is_empty(), "{r:?}");
    }
    assert!(Refusal::Conflict(vec!["a.txt".into()])
        .message()
        .contains("`a.txt`"));
}

// --- the git half ------------------------------------------------------------

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "loom@example.com")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "loom@example.com")
        .output()
        .expect("git must spawn")
}

fn run(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) -> String {
    std::fs::write(dir.join(name), body).unwrap();
    run(dir, &["add", "-A"]);
    run(dir, &["commit", "-q", "-m", msg]);
    run(dir, &["rev-parse", "HEAD"])
}

fn configure(dir: &Path) {
    run(dir, &["config", "user.email", "loom@example.com"]);
    run(dir, &["config", "user.name", "Loom Test"]);
    run(dir, &["config", "commit.gpgsign", "false"]);
}

const PARENT: &str = "feature/issue-901";
const CHILD: &str = "feature/issue-902";

struct Fixture {
    _tmp: tempfile::TempDir,
    origin: PathBuf,
    author: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let author = tmp.path().join("author");
        let repo = tmp.path().join("repo");
        run(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "--initial-branch=main",
                origin.to_str().unwrap(),
            ],
        );
        std::fs::create_dir_all(&author).unwrap();
        run(&author, &["init", "-q", "--initial-branch=main"]);
        configure(&author);
        commit_file(&author, "base.txt", "base\n", "base");
        run(&author, &["remote", "add", "origin", origin.to_str().unwrap()]);
        run(&author, &["push", "-q", "origin", "main"]);
        Self {
            _tmp: tmp,
            origin,
            author,
            repo,
        }
    }

    /// A commit on branch `name` (cut from `from`), pushed, recorded as PR
    /// `pr`'s head ref.
    fn branch(&self, name: &str, from: &str, pr: u32, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "-B", name, from]);
        let sha = commit_file(&self.author, file, body, &format!("{name}: {file}"));
        run(&self.author, &["push", "-q", "-f", "origin", &format!("{name}:{name}")]);
        run(
            &self.author,
            &[
                "push",
                "-q",
                "-f",
                "origin",
                &format!("{name}:refs/pull/{pr}/head"),
            ],
        );
        sha
    }

    fn advance_main(&self, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "main"]);
        let sha = commit_file(&self.author, file, body, &format!("main: {file}"));
        run(&self.author, &["push", "-q", "origin", "main"]);
        sha
    }

    fn clone_repo(&self) {
        run(
            self.origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                self.origin.to_str().unwrap(),
                self.repo.to_str().unwrap(),
            ],
        );
        configure(&self.repo);
    }

    fn origin_rev(&self, rev: &str) -> String {
        run(&self.origin, &["rev-parse", rev])
    }

    fn origin_has_branch(&self, name: &str) -> bool {
        git_out(&self.origin, &["rev-parse", "--verify", "-q", &format!("refs/heads/{name}")])
            .status
            .success()
    }

    fn origin_files(&self, rev: &str) -> Vec<String> {
        run(&self.origin, &["ls-tree", "--name-only", rev])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn origin_count(&self, range: &str) -> usize {
        run(&self.origin, &["rev-list", "--count", range])
            .parse()
            .unwrap()
    }

    fn worktrees(&self) -> usize {
        run(&self.repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    }
}

fn orphan<'a>(parent_head: &'a str, child_head: &'a str) -> Orphan<'a> {
    Orphan {
        parent_pr: 1,
        parent_head,
        child_pr: 2,
        child_head,
    }
}

fn expect_refused(r: Result<impl std::fmt::Debug, Failure>) -> Refusal {
    match r {
        Err(Failure::Refused(r)) => r,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_replacement_carries_only_the_childs_own_commits_and_leaves_the_original_alone() {
    let fx = Fixture::new();
    let a = fx.branch(PARENT, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(CHILD, PARENT, 2, "b.txt", "child\n");
    fx.clone_repo();
    let main_tip = fx.advance_main("m.txt", "main moved\n");
    // The parent's branch is gone; only its PR ref and the child's ancestry remain.
    run(&fx.author, &["push", "-q", "origin", "--delete", PARENT]);

    let got = create(&fx.repo, "main", &orphan(&a, &b)).expect("replacement");

    assert_eq!(got.branch, "orphan-replace/pr-2");
    assert!(!got.reused);
    assert_eq!(got.onto, main_tip, "sits on the freshly fetched default-branch tip");
    assert_eq!(got.commits, vec![b.clone()]);
    assert_eq!(fx.origin_rev(&got.branch), got.head);
    assert_eq!(fx.origin_rev(&format!("{}^", got.branch)), main_tip);
    assert_eq!(fx.origin_count(&format!("main..{}", got.branch)), 1, "one commit of its own");
    let files = fx.origin_files(&got.branch);
    assert!(files.contains(&"b.txt".to_string()) && files.contains(&"m.txt".to_string()));
    assert!(!files.contains(&"a.txt".to_string()), "the rejected parent's change is absent");
    // The rejected parent's commit is not an ancestor of the replacement.
    assert!(!git_out(&fx.origin, &["merge-base", "--is-ancestor", &a, &got.head])
        .status
        .success());
    // The original branch and its PR ref are untouched.
    assert_eq!(fx.origin_rev(CHILD), b);
    assert_eq!(fx.origin_rev("refs/pull/2/head"), b);
    assert_eq!(fx.worktrees(), 1, "the scratch worktree is removed");
    assert_eq!(run(&fx.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
}

#[test]
fn a_retry_reuses_the_branch_and_never_makes_a_second_one() {
    let fx = Fixture::new();
    let a = fx.branch(PARENT, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(CHILD, PARENT, 2, "b.txt", "child\n");
    fx.clone_repo();
    let first = create(&fx.repo, "main", &orphan(&a, &b)).unwrap();
    fx.advance_main("m.txt", "main moved after the first run\n");

    let again = create(&fx.repo, "main", &orphan(&a, &b)).unwrap();

    assert!(again.reused);
    assert_eq!(again.head, first.head);
    assert_eq!(again.branch, first.branch);
    assert_eq!(fx.origin_rev(&first.branch), first.head, "not rewritten");
}

#[test]
fn a_branch_that_is_not_the_child_only_range_is_refused_not_overwritten() {
    let fx = Fixture::new();
    let a = fx.branch(PARENT, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(CHILD, PARENT, 2, "b.txt", "child\n");
    let squatter = fx.branch("orphan-replace/pr-2", "main", 99, "x.txt", "x\n");
    run(&fx.author, &["commit", "-q", "--allow-empty", "-m", "second"]);
    run(&fx.author, &["push", "-q", "origin", "orphan-replace/pr-2"]);
    fx.clone_repo();

    let r = expect_refused(create(&fx.repo, "main", &orphan(&a, &b)));

    assert!(matches!(r, Refusal::BranchTaken(_)), "{r:?}");
    assert_ne!(fx.origin_rev("orphan-replace/pr-2"), squatter, "fixture sanity");
    assert_eq!(fx.origin_rev(CHILD), b);
}

#[test]
fn a_parent_that_is_not_an_ancestor_is_refused() {
    let fx = Fixture::new();
    fx.branch(PARENT, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(CHILD, PARENT, 2, "b.txt", "child\n");
    // The parent was amended after the child branched: PR 1 now points elsewhere.
    let amended = fx.branch("feature/issue-901-amended", "main", 1, "a.txt", "amended\n");
    fx.clone_repo();

    let r = expect_refused(create(&fx.repo, "main", &orphan(&amended, &b)));

    assert_eq!(r, Refusal::ParentNotAncestor);
    assert!(!fx.origin_has_branch("orphan-replace/pr-2"), "nothing pushed");
}

#[test]
fn a_missing_parent_head_is_refused() {
    let fx = Fixture::new();
    fx.branch(PARENT, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(CHILD, PARENT, 2, "b.txt", "child\n");
    fx.clone_repo();
    // refs/pull/1/head fetches fine but does not hold this commit.
    let ghost = "0123456789abcdef0123456789abcdef01234567";

    let r = expect_refused(create(&fx.repo, "main", &orphan(ghost, &b)));

    assert!(matches!(r, Refusal::MissingHead(_)), "{r:?}");
    assert!(!fx.origin_has_branch("orphan-replace/pr-2"));
}

#[test]
fn a_short_or_malformed_sha_is_refused_before_any_git() {
    let fx = Fixture::new();
    let r = expect_refused(verify(&fx.author, &orphan("abc", &"b".repeat(40))));
    assert_eq!(r, Refusal::BadSha("abc".to_string()));
}

#[test]
fn an_empty_range_is_refused() {
    let fx = Fixture::new();
    let a = fx.branch(PARENT, "main", 1, "a.txt", "parent\n");
    // The "child" is exactly the parent's head.
    fx.clone_repo();

    let r = expect_refused(create(&fx.repo, "main", &orphan(&a, &a)));

    assert_eq!(r, Refusal::EmptyRange);
}

#[test]
fn a_merge_commit_in_the_range_is_refused() {
    let fx = Fixture::new();
    let a = fx.branch(PARENT, "main", 1, "a.txt", "parent\n");
    let side = fx.branch("side", "main", 5, "s.txt", "side\n");
    run(&fx.author, &["checkout", "-q", "-B", CHILD, PARENT]);
    commit_file(&fx.author, "b.txt", "child\n", "child");
    run(&fx.author, &["merge", "-q", "--no-ff", "-m", "merge side", &side]);
    let b = run(&fx.author, &["rev-parse", "HEAD"]);
    run(&fx.author, &["push", "-q", "-f", "origin", &format!("{CHILD}:{CHILD}")]);
    run(
        &fx.author,
        &[
            "push",
            "-q",
            "-f",
            "origin",
            &format!("{CHILD}:refs/pull/2/head"),
        ],
    );
    fx.clone_repo();

    let r = expect_refused(create(&fx.repo, "main", &orphan(&a, &b)));

    assert_eq!(r, Refusal::MergeInRange);
}

#[test]
fn a_child_that_depends_on_the_rejected_parent_conflicts_and_nothing_is_pushed() {
    let fx = Fixture::new();
    // The parent creates shared.txt; the child edits it, so on main (which has
    // no shared.txt) the child's change cannot apply.
    let a = fx.branch(PARENT, "main", 1, "shared.txt", "one\n");
    let b = fx.branch(CHILD, PARENT, 2, "shared.txt", "one\ntwo\n");
    fx.clone_repo();

    let r = expect_refused(create(&fx.repo, "main", &orphan(&a, &b)));

    match r {
        Refusal::Conflict(paths) => assert_eq!(paths, vec!["shared.txt".to_string()]),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert!(!fx.origin_has_branch("orphan-replace/pr-2"));
    assert_eq!(fx.worktrees(), 1, "the scratch worktree is removed after a conflict");
    assert_eq!(fx.origin_rev(CHILD), b);
}
