//! Fixtures A-E of Issue #9282, plus the shapes that bound them.
//!
//! Every fixture is a real repository built with real `git`: the whole
//! question is what `rebase`, `--remerge-diff` and `merge-tree` actually
//! compute, so a stub would test the stub. Hermetic by construction: a temp
//! directory per test, no remote, no network, and git isolated from the
//! host's system and global configuration through [`ENV`], which the verb's
//! own git children receive too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::path::Path;
use std::process::Command;
use tempfile::{tempdir, TempDir};

/// Environment for every git child, the fixture's and the verb's alike.
const ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_EDITOR", "true"),
    ("GIT_AUTHOR_NAME", "Fixture"),
    ("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
    ("GIT_COMMITTER_NAME", "Fixture"),
    ("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
];

fn raw(repo: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "advice.mergeConflict=false",
        ])
        .args(args)
        .envs(ENV.iter().copied())
        .output()
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run git and require success.
#[track_caller]
fn g(repo: &Path, args: &[&str]) -> String {
    let (ok, out) = raw(repo, args);
    assert!(ok, "git {args:?} failed in {}", repo.display());
    out
}

fn write(repo: &Path, rel: &str, content: &str) {
    std::fs::write(repo.join(rel), content).unwrap();
}

/// Stage everything and commit; returns the new commit.
#[track_caller]
fn commit(repo: &Path, msg: &str) -> String {
    g(repo, &["add", "-A"]);
    g(repo, &["commit", "-q", "-m", msg]);
    g(repo, &["rev-parse", "HEAD"])
}

/// A repository on `main` with one commit holding `files`.
fn repo_with(files: &[(&str, &str)]) -> TempDir {
    let dir = tempdir().unwrap();
    g(dir.path(), &["init", "-q", "-b", "main"]);
    for (rel, content) in files {
        write(dir.path(), rel, content);
    }
    commit(dir.path(), "base");
    dir
}

fn check(repo: &Path, base: &str, old: &str, new: &str) -> Verdict {
    assess_with(repo, ENV, base, old, new)
}

#[track_caller]
fn assert_clean(v: &Verdict) {
    assert!(matches!(v, Verdict::Clean(_)), "expected clean, got {v:?}");
}

const TEN_LINES: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";

// ---------------------------------------------------------------------------
// Fixture A: a hand-resolved merge replaced by a flattened rebase
// ---------------------------------------------------------------------------

/// The incident. The branch's merge of `main` resolved a textual conflict AND
/// moved a fixture off a colliding coordinate; an independent rebase resolved
/// only the textual conflict. Both trees are clean.
#[test]
fn fixture_a_rebase_that_drops_a_hand_resolution_is_blocked() {
    let dir = repo_with(&[("conf.txt", "value=0\n"), ("fix_a.txt", "coord=5\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "conf.txt", "value=feature\n");
    write(r, "fix_b.txt", "coord=5\n");
    commit(r, "feature: change conf, add a second fixture");
    g(r, &["checkout", "-q", "main"]);
    write(r, "conf.txt", "value=main\n");
    commit(r, "main: change conf");

    // old: merge main in, resolve the conflict, and move the fixture.
    g(r, &["checkout", "-q", "feature"]);
    let (merged_cleanly, _) = raw(r, &["merge", "main"]);
    assert!(!merged_cleanly, "the fixture needs a real textual conflict");
    write(r, "conf.txt", "value=both\n");
    write(r, "fix_b.txt", "coord=6\n");
    let merge = commit(r, "Merge main: keep both, move fix_b off coord 5");

    // new: rebase the pre-merge work, resolving only the textual conflict.
    g(r, &["checkout", "-q", "-b", "rebased", "feature~1"]);
    let (rebased_cleanly, _) = raw(r, &["rebase", "main"]);
    assert!(!rebased_cleanly, "the rebase must hit the same textual conflict");
    write(r, "conf.txt", "value=both\n");
    g(r, &["add", "-A"]);
    g(r, &["rebase", "--continue"]);
    assert_eq!(g(r, &["diff", "--name-only", "feature", "rebased"]), "fix_b.txt");

    let v = check(r, "main", "feature", "rebased");
    assert_eq!(
        v,
        Verdict::ResolutionDiscarded {
            merge: merge.clone(),
            paths: vec!["fix_b.txt".to_string()],
        }
    );

    let args = Args {
        base: "main".into(),
        old: "feature".into(),
        new: "rebased".into(),
    };
    let (code, token, explanation) = render(&v, &args);
    assert_eq!(code, 0, "blocked is exit 0 so a bare `if` fires on it");
    assert_eq!(token, Some(format!("RESOLUTION_DISCARDED {merge}")));
    assert!(explanation.contains("do NOT push"));
    assert!(explanation.contains("A conflict resolution is knowledge"), "states the reason");
    assert!(
        explanation.contains("merge `main` into it instead of rebasing"),
        "names the merge-instead remedy"
    );
    assert!(explanation.contains("fix_b.txt"));
}

/// The silent variant: nothing conflicts textually, so `git rebase` exits 0
/// on its own and simply flattens the merge commit that held the hand edit.
#[test]
fn fixture_a_rebase_that_succeeds_cleanly_still_reverts_the_resolution() {
    let dir = repo_with(&[("fix_a.txt", "coord=5\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "fix_b.txt", "coord=5\n");
    commit(r, "feature: add a second fixture");
    g(r, &["checkout", "-q", "main"]);
    write(r, "other.txt", "main moved\n");
    commit(r, "main: unrelated");

    g(r, &["checkout", "-q", "feature"]);
    g(r, &["merge", "--no-commit", "--no-ff", "main"]);
    write(r, "fix_b.txt", "coord=6\n");
    let merge = commit(r, "Merge main: move fix_b off the colliding coordinate");

    g(r, &["checkout", "-q", "-b", "rebased", "feature"]);
    g(r, &["rebase", "main"]); // exits 0: "succeeded cleanly"
    assert_eq!(std::fs::read_to_string(r.join("fix_b.txt")).unwrap(), "coord=5\n");

    assert_eq!(
        check(r, "main", "feature", "rebased"),
        Verdict::ResolutionDiscarded {
            merge,
            paths: vec!["fix_b.txt".to_string()],
        }
    );
}

/// A rebase that carries the resolution forward by hand is not a discard.
#[test]
fn a_rebase_that_carries_the_resolution_forward_is_clean() {
    let dir = repo_with(&[("fix_a.txt", "coord=5\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "fix_b.txt", "coord=5\n");
    commit(r, "feature: add a second fixture");
    g(r, &["checkout", "-q", "main"]);
    write(r, "other.txt", "main moved\n");
    commit(r, "main: unrelated");
    g(r, &["checkout", "-q", "feature"]);
    g(r, &["merge", "--no-commit", "--no-ff", "main"]);
    write(r, "fix_b.txt", "coord=6\n");
    commit(r, "Merge main: move fix_b");

    g(r, &["checkout", "-q", "-b", "rebased", "feature"]);
    g(r, &["rebase", "main"]);
    write(r, "fix_b.txt", "coord=6\n");
    commit(r, "carry the merge's resolution forward");

    assert_clean(&check(r, "main", "feature", "rebased"));
}

/// The base moved a path the resolution touched, without conflict. Comparing
/// against `<old>` alone would call that a discard; comparing against what a
/// merge of the base would produce does not.
#[test]
fn a_base_that_cleanly_moves_a_resolution_path_is_not_a_discard() {
    let dir = repo_with(&[("conf.txt", TEN_LINES)]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "conf.txt", &TEN_LINES.replace("two", "two-feature"));
    commit(r, "feature: edit line two");
    g(r, &["checkout", "-q", "main"]);
    write(r, "conf.txt", &TEN_LINES.replace("two", "two-main"));
    commit(r, "main: edit line two");
    g(r, &["checkout", "-q", "feature"]);
    assert!(!raw(r, &["merge", "main"]).0);
    write(r, "conf.txt", &TEN_LINES.replace("two", "two-both"));
    commit(r, "Merge main: keep both");

    // main moves again, in a distant region of the same file.
    g(r, &["checkout", "-q", "main"]);
    write(
        r,
        "conf.txt",
        &TEN_LINES
            .replace("two", "two-main")
            .replace("nine", "nine-main"),
    );
    commit(r, "main: edit line nine");

    // new: one commit on main whose tree is exactly "merge main into feature".
    g(r, &["checkout", "-q", "-b", "merged", "feature"]);
    g(r, &["merge", "-q", "--no-edit", "main"]);
    let tree = g(r, &["rev-parse", "merged^{tree}"]);
    let flat = g(r, &["commit-tree", &tree, "-p", "main", "-m", "flattened"]);

    assert_clean(&check(r, "main", "feature", &flat));
}

/// The base conflicts with the resolution itself, so any rebase had to redo
/// it without the merge commit. That is a discard however it was redone.
#[test]
fn a_resolution_path_the_base_conflicts_with_is_a_discard() {
    let dir = repo_with(&[("conf.txt", "value=0\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "conf.txt", "value=feature\n");
    commit(r, "feature: change conf");
    g(r, &["checkout", "-q", "main"]);
    write(r, "conf.txt", "value=main\n");
    commit(r, "main: change conf");
    g(r, &["checkout", "-q", "feature"]);
    assert!(!raw(r, &["merge", "main"]).0);
    write(r, "conf.txt", "value=both\n");
    let merge = commit(r, "Merge main: keep both");

    g(r, &["checkout", "-q", "main"]);
    write(r, "conf.txt", "value=main-again\n");
    let main2 = commit(r, "main: change conf again");
    write(r, "conf.txt", "value=redone\n");
    let redone = commit(r, "stand-in for a rebase that re-resolved conf");
    g(r, &["reset", "-q", "--hard", &main2]);

    assert_eq!(
        check(r, "main", "feature", &redone),
        Verdict::ResolutionDiscarded {
            merge,
            paths: vec!["conf.txt".to_string()],
        }
    );
}

// ---------------------------------------------------------------------------
// Fixture B: an emptied payload
// ---------------------------------------------------------------------------

/// #8298 / #9001: a `merge=ours` path makes the replayed commit empty, the
/// rebase exits 0, and the rebased tree equals the base.
#[test]
fn fixture_b_merge_ours_emptied_install_metadata_commit_is_emptied() {
    let dir = repo_with(&[
        (".gitattributes", "install-metadata.json merge=ours\n"),
        (
            "install-metadata.json",
            "{\n  \"loom_version\": \"1\",\n  \"loom_commit\": \"aaaa\"\n}\n",
        ),
    ]);
    let r = dir.path();
    g(r, &["config", "merge.ours.driver", "true"]);
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(
        r,
        "install-metadata.json",
        "{\n  \"loom_version\": \"1\",\n  \"loom_commit\": \"bbbb\"\n}\n",
    );
    let payload = commit(r, "chore: resync installed Loom surfaces");
    g(r, &["checkout", "-q", "main"]);
    write(
        r,
        "install-metadata.json",
        "{\n  \"loom_version\": \"2\",\n  \"loom_commit\": \"aaaa\"\n}\n",
    );
    commit(r, "chore: bump version");

    g(r, &["checkout", "-q", "-b", "rebased", "feature"]);
    g(r, &["rebase", "main"]); // exits 0, no conflict reported
    assert_eq!(g(r, &["rev-parse", "rebased^{tree}"]), g(r, &["rev-parse", "main^{tree}"]));

    let v = check(r, "main", "feature", "rebased");
    assert_eq!(v, Verdict::Emptied { commit: payload });
    let args = Args {
        base: "main".into(),
        old: "feature".into(),
        new: "rebased".into(),
    };
    let (code, token, explanation) = render(&v, &args);
    assert_eq!((code, token.as_deref()), (0, Some("EMPTIED")));
    assert!(explanation.contains("do NOT push"));
}

/// Fixture B in its plainest form: commits ahead of base, new tree is base.
#[test]
fn fixture_b_new_tree_equal_to_base_is_emptied() {
    let dir = repo_with(&[("a.txt", "a\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "b.txt", "payload\n");
    let payload = commit(r, "feature: payload");
    g(r, &["checkout", "-q", "main"]);
    write(r, "c.txt", "main moved\n");
    commit(r, "main: unrelated");

    assert_eq!(check(r, "main", "feature", "main"), Verdict::Emptied { commit: payload });
}

/// DECISION: a commit the rebase dropped because the base already carries the
/// identical patch is CLEAN. Nothing is lost (the content is in the base), and
/// blocking would strand a branch whose only possible rebase result is this
/// same empty one. Contrast with the `merge=ours` case above, where the base
/// holds a DIFFERENT change to the same path.
#[test]
fn a_commit_dropped_because_the_base_already_has_it_is_clean() {
    let dir = repo_with(&[("a.txt", "a\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "b.txt", "payload\n");
    let payload = commit(r, "feature: payload");
    g(r, &["checkout", "-q", "main"]);
    write(r, "c.txt", "main moved\n");
    commit(r, "main: unrelated");
    g(r, &["cherry-pick", &payload]);

    g(r, &["checkout", "-q", "-b", "rebased", "feature"]);
    g(r, &["rebase", "main"]);
    assert_eq!(g(r, &["rev-parse", "rebased"]), g(r, &["rev-parse", "main"]));

    assert_clean(&check(r, "main", "feature", "rebased"));
}

/// Commits ahead that change nothing are not a payload.
#[test]
fn a_branch_of_only_empty_commits_is_not_emptied() {
    let dir = repo_with(&[("a.txt", "a\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    g(r, &["commit", "-q", "--allow-empty", "-m", "retrigger ci"]);
    g(r, &["checkout", "-q", "main"]);
    write(r, "c.txt", "main moved\n");
    commit(r, "main: unrelated");

    assert_clean(&check(r, "main", "feature", "main"));
}

// ---------------------------------------------------------------------------
// Fixtures C, D, E: the clean shapes
// ---------------------------------------------------------------------------

#[test]
fn fixture_c_ordinary_rebase_of_a_linear_branch_is_clean() {
    let dir = repo_with(&[("a.txt", "a\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "b.txt", "one\n");
    commit(r, "feature: one");
    write(r, "b.txt", "one\ntwo\n");
    commit(r, "feature: two");
    g(r, &["checkout", "-q", "main"]);
    write(r, "c.txt", "main moved\n");
    commit(r, "main: unrelated");

    g(r, &["checkout", "-q", "-b", "rebased", "feature"]);
    g(r, &["rebase", "main"]);

    assert_clean(&check(r, "main", "feature", "rebased"));
}

/// A mechanical merge (empty remerge-diff) is not a reasoned resolution.
#[test]
fn fixture_d_flattening_a_mechanical_merge_is_clean() {
    let dir = repo_with(&[("a.txt", "a\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "b.txt", "payload\n");
    commit(r, "feature: payload");
    g(r, &["checkout", "-q", "main"]);
    write(r, "c.txt", "main moved\n");
    commit(r, "main: unrelated");
    g(r, &["checkout", "-q", "feature"]);
    g(r, &["merge", "-q", "--no-ff", "--no-edit", "main"]);
    assert_eq!(g(r, &["rev-list", "--merges", "--count", "main..feature"]), "1");

    g(r, &["checkout", "-q", "-b", "rebased", "feature"]);
    g(r, &["rebase", "main"]);
    assert_eq!(g(r, &["rev-list", "--merges", "--count", "main..rebased"]), "0");
    assert_eq!(g(r, &["rev-parse", "rebased^{tree}"]), g(r, &["rev-parse", "feature^{tree}"]));

    assert_clean(&check(r, "main", "feature", "rebased"));
}

/// The hand-resolved merge stays in history: the push is a fast-forward.
#[test]
fn fixture_e_fast_forward_over_a_hand_resolved_merge_is_clean() {
    let dir = repo_with(&[("conf.txt", "value=0\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "conf.txt", "value=feature\n");
    commit(r, "feature: change conf");
    g(r, &["checkout", "-q", "main"]);
    write(r, "conf.txt", "value=main\n");
    commit(r, "main: change conf");
    g(r, &["checkout", "-q", "feature"]);
    assert!(!raw(r, &["merge", "main"]).0);
    write(r, "conf.txt", "value=both\n");
    commit(r, "Merge main: keep both");
    g(r, &["branch", "old-head"]);
    write(r, "conf.txt", "value=changed-after-the-merge\n");
    commit(r, "feature: follow-up on top of the merge");

    assert_clean(&check(r, "main", "old-head", "feature"));
    assert_clean(&check(r, "main", "feature", "feature"));
}

// ---------------------------------------------------------------------------
// Unanswered, defaults, and the read-only contract
// ---------------------------------------------------------------------------

#[test]
fn a_nonexistent_old_ref_is_unanswered_not_blocked_and_not_safe() {
    let dir = repo_with(&[("a.txt", "a\n")]);
    let v = check(dir.path(), "main", "origin/no-such-branch", "HEAD");
    let Verdict::Unanswered(reason) = &v else {
        panic!("expected unanswered, got {v:?}");
    };
    assert!(
        reason.contains("--old `origin/no-such-branch`"),
        "names the bad argument: {reason}"
    );

    let args = Args {
        base: "main".into(),
        old: "origin/no-such-branch".into(),
        new: "HEAD".into(),
    };
    let (code, token, explanation) = render(&v, &args);
    assert_eq!(code, EX_UNANSWERED);
    assert!(code != 0 && code != EX_SAFE, "distinct from blocked and from safe");
    assert_eq!(token, None, "nothing on stdout");
    assert!(explanation.starts_with("WARNING:"));
    assert!(explanation.contains("not an all-clear"));
}

#[test]
fn outside_a_repository_is_unanswered() {
    let dir = tempdir().unwrap();
    assert!(matches!(check(dir.path(), "main", "a", "b"), Verdict::Unanswered(_)));
}

/// The CLI defaults: `--old` is the branch's push destination as fetched,
/// `--new` is `HEAD`. Resolved against a local bare "origin".
#[test]
fn the_default_old_is_the_push_destination() {
    let dir = repo_with(&[("a.txt", "a\n")]);
    let r = dir.path();
    let origin = tempdir().unwrap();
    g(origin.path(), &["init", "-q", "--bare", "-b", "main"]);
    g(r, &["remote", "add", "origin", origin.path().to_str().unwrap()]);
    g(r, &["push", "-q", "origin", "main"]);
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "b.txt", "payload\n");
    commit(r, "feature: payload");
    g(r, &["push", "-q", "-u", "origin", "feature"]);
    g(r, &["checkout", "-q", "main"]);
    write(r, "c.txt", "main moved\n");
    commit(r, "main: unrelated");
    g(r, &["push", "-q", "origin", "main"]);
    g(r, &["checkout", "-q", "feature"]);
    g(r, &["reset", "-q", "--hard", "origin/main"]); // an "emptied" local head

    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }
    let args = Cli::parse_from(["x", "--base", "origin/main"]).args;
    assert_eq!((args.old.as_str(), args.new.as_str()), (DEFAULT_OLD, "HEAD"));
    assert!(matches!(check(r, &args.base, &args.old, &args.new), Verdict::Emptied { .. }));

    // How `worktree.sh` leaves a branch: its upstream is the BASE, so `@{push}`
    // does not resolve. The default must find `origin/feature`, never settle
    // for the upstream (which would compare the base with itself: "clean").
    g(r, &["branch", "-q", "--set-upstream-to=origin/main"]);
    assert!(!raw(r, &["rev-parse", "--verify", "--quiet", "@{push}"]).0);
    assert!(matches!(check(r, &args.base, &args.old, &args.new), Verdict::Emptied { .. }));

    // No push destination and no `origin/<branch>`: unanswered, not clean.
    g(r, &["checkout", "-q", "-b", "never-pushed"]);
    assert!(matches!(check(r, &args.base, &args.old, &args.new), Verdict::Unanswered(_)));
}

/// The verb adds nothing to the repository: no object, no ref, no index or
/// working-tree change, even on the paths that re-merge and write trees.
#[test]
fn assessing_leaves_the_repository_untouched() {
    let dir = repo_with(&[("conf.txt", "value=0\n")]);
    let r = dir.path();
    g(r, &["checkout", "-q", "-b", "feature"]);
    write(r, "conf.txt", "value=feature\n");
    commit(r, "feature: change conf");
    g(r, &["checkout", "-q", "main"]);
    write(r, "conf.txt", "value=main\n");
    commit(r, "main: change conf");
    g(r, &["checkout", "-q", "feature"]);
    assert!(!raw(r, &["merge", "main"]).0);
    write(r, "conf.txt", "value=both\n");
    commit(r, "Merge main: keep both");
    g(r, &["checkout", "-q", "main"]);
    write(r, "conf.txt", "value=main-again\n");
    commit(r, "main: change conf again");
    g(r, &["checkout", "-q", "-b", "redone"]);
    write(r, "conf.txt", "value=redone\n");
    commit(r, "re-resolved");

    let snapshot = |r: &Path| {
        (
            g(r, &["count-objects", "-v"]),
            g(r, &["for-each-ref"]),
            g(r, &["rev-parse", "HEAD"]),
            g(r, &["status", "--porcelain"]),
            g(r, &["stash", "list"]),
        )
    };
    let before = snapshot(r);
    assert!(matches!(
        check(r, "main", "feature", "redone"),
        Verdict::ResolutionDiscarded { .. }
    ));
    assert_eq!(snapshot(r), before);
    let leftovers: Vec<_> = std::fs::read_dir(r.join(".git/objects"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("tmp_objdir") || n.starts_with("incoming"))
        .collect();
    assert!(leftovers.is_empty(), "left behind in .git/objects: {leftovers:?}");
}

#[test]
fn a_safe_verdict_exits_one_with_nothing_on_stdout() {
    let args = Args {
        base: "main".into(),
        old: "a".into(),
        new: "b".into(),
    };
    let (code, token, _) = render(&Verdict::Clean("x"), &args);
    assert_eq!((code, token), (EX_SAFE, None));
}
