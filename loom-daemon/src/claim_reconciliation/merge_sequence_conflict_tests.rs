//! Fixtures for #10350: the real merge-conflict pair predicate (the shared
//! detector Phase 1 and the stacking path use; no planner consumes it, #12775).

use std::cell::Cell;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use super::super::SequencePr;
use super::*;
use crate::claim_reconciliation::read_cache;

fn sha(n: u32) -> String {
    format!("{n:040x}")
}

fn pr(number: u32, labels: &[&str]) -> SequencePr {
    SequencePr {
        number,
        created_at: format!("2026-10-01T{:02}:00:00Z", number % 24),
        updated_at: "2026-10-01T00:00:00Z".to_string(),
        head_sha: Some(sha(number)),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        // Judge-approved: #10371 drops edges behind a PR that is not ready,
        // and these tests exercise the conflict predicate, not readiness.
        labels: std::iter::once("loom:pr")
            .chain(labels.iter().copied())
            .map(str::to_string)
            .collect(),
    }
}

// --- The checker: cache, budget, fail closed -------------------------------

#[test]
fn an_unknown_verdict_or_missing_head_is_a_conflict() {
    let root = Path::new("/nonexistent/loom-10350-unknown");
    let checker = PairChecker::with_eval(root, 8, |_: &SequencePr, _: &SequencePr| None);
    assert!(checker.conflicts(&pr(1, &[]), &pr(2, &[])), "eval error ⇒ conflict");
    let mut unpinned = pr(3, &[]);
    unpinned.head_sha = None;
    let clean = PairChecker::with_eval(root, 8, |_: &SequencePr, _: &SequencePr| Some(false));
    assert!(clean.conflicts(&pr(1, &[]), &unpinned), "no head ⇒ conflict");
    assert!(!clean.conflicts(&pr(1, &[]), &pr(2, &[])));
}

#[test]
fn over_budget_pairs_are_conflicts_and_are_not_cached() {
    read_cache::set_test_enabled(true);
    let root = Path::new("/nonexistent/loom-10350-budget");
    let calls = Cell::new(0);
    let eval = |_: &SequencePr, _: &SequencePr| {
        calls.set(calls.get() + 1);
        Some(false)
    };
    let checker = PairChecker::with_eval(root, 1, eval);
    assert!(!checker.conflicts(&pr(1, &[]), &pr(2, &[])));
    assert!(checker.conflicts(&pr(1, &[]), &pr(3, &[])), "past the budget ⇒ conflict");
    assert_eq!(calls.get(), 1);
    // Next tick: the over-budget pair is evaluated, the answered one reused.
    let next = PairChecker::with_eval(root, 1, eval);
    assert!(!next.conflicts(&pr(2, &[]), &pr(1, &[])), "cached, order-insensitive");
    assert!(!next.conflicts(&pr(1, &[]), &pr(3, &[])));
    assert_eq!(calls.get(), 2);
    read_cache::set_test_enabled(false);
}

#[test]
fn verdicts_are_cached_per_head_pair_and_a_moved_head_re_evaluates() {
    read_cache::set_test_enabled(true);
    let root = Path::new("/nonexistent/loom-10350-cache");
    let calls = Cell::new(0);
    let eval = |_: &SequencePr, _: &SequencePr| {
        calls.set(calls.get() + 1);
        Some(true)
    };
    let (a, b) = (pr(1, &[]), pr(2, &[]));
    let tick = PairChecker::with_eval(root, 8, eval);
    assert!(tick.conflicts(&a, &b) && tick.conflicts(&b, &a));
    assert_eq!(calls.get(), 1, "once per tick");
    assert!(PairChecker::with_eval(root, 8, eval).conflicts(&a, &b));
    assert_eq!(calls.get(), 1, "reused across ticks");
    let mut moved = b.clone();
    moved.head_sha = Some(sha(99));
    assert!(PairChecker::with_eval(root, 8, eval).conflicts(&a, &moved));
    assert_eq!(calls.get(), 2, "a moved head is a new pair");
    read_cache::set_test_enabled(false);
}

#[test]
fn within_a_tick_a_pair_is_evaluated_once_even_with_the_cache_off() {
    let root = Path::new("/nonexistent/loom-10350-tick");
    let calls = Cell::new(0);
    let eval = |_: &SequencePr, _: &SequencePr| {
        calls.set(calls.get() + 1);
        Some(false)
    };
    let checker = PairChecker::with_eval(root, 8, eval);
    for _ in 0..3 {
        assert!(!checker.conflicts(&pr(1, &[]), &pr(2, &[])));
    }
    assert_eq!(calls.get(), 1);
}

#[test]
fn no_evaluation_starts_after_the_shared_deadline_and_holds_are_retained() {
    let root = Path::new("/nonexistent/loom-10350-deadline");
    let now = Rc::new(Cell::new(Duration::ZERO));
    let clock = Rc::clone(&now);
    let deadline = Deadline::with_clock(Duration::from_secs(100), move || clock.get());
    let calls = Cell::new(0);
    // Each evaluation "takes" 40s of the injected clock and finds a clean merge.
    let eval = |_: &SequencePr, _: &SequencePr| {
        calls.set(calls.get() + 1);
        now.set(now.get() + Duration::from_secs(40));
        Some(false)
    };
    let checker = PairChecker::with_deadline(root, 256, deadline, eval);
    assert!(!checker.conflicts(&pr(1, &[]), &pr(2, &[])), "t=0");
    assert!(!checker.conflicts(&pr(1, &[]), &pr(3, &[])), "t=40");
    assert!(!checker.conflicts(&pr(1, &[]), &pr(4, &[])), "t=80, still inside");
    // t=120: spent. Later pairs launch nothing and keep their edge/hold.
    assert!(checker.conflicts(&pr(1, &[]), &pr(5, &[])), "past the deadline ⇒ conflict");
    assert!(checker.conflicts(&pr(2, &[]), &pr(3, &[])), "past the deadline ⇒ conflict");
    assert_eq!(calls.get(), 3, "no evaluation after the deadline");
    // An answer already in hand is still served after the deadline.
    assert!(!checker.conflicts(&pr(4, &[]), &pr(1, &[])));
    assert_eq!(calls.get(), 3);
}

#[test]
fn a_spent_deadline_launches_no_git_and_clamps_the_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("lib.rs"), "pub mod a;\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "base"]);
    let head = git(dir, &["rev-parse", "HEAD"]);
    let spent = Deadline::with_clock(Duration::from_secs(1), || Duration::from_secs(5));
    assert!(spent.expired());
    // Present locally, yet a spent deadline answers "not available" without
    // running even `git cat-file`.
    let mut p = pr(1, &[]);
    p.head_sha = Some(head.clone());
    assert!(!ensure_head(dir, &p, &spent));
    assert!(ensure_head(dir, &p, &Deadline::unbounded()));
    assert_eq!(merge_tree_within(dir, &head, &head, &spent), None);
    // A nearly-spent deadline clamps the timeout to what is left.
    let nearly = Deadline::with_clock(Duration::from_secs(30), || {
        Duration::from_secs(30) - Duration::from_millis(1)
    });
    assert_eq!(nearly.remaining(), Duration::from_millis(1));
}

#[test]
fn fetch_that_spends_the_deadline_launches_no_further_git() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin");
    let clone = tmp.path().join("clone");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&clone).unwrap();
    git(&origin, &["init", "-q", "-b", "main"]);
    std::fs::write(origin.join("lib.rs"), "pub mod a;\n").unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-q", "-m", "base"]);
    let head = git(&origin, &["rev-parse", "HEAD"]);
    git(&origin, &["update-ref", "refs/pull/1/head", &head]);
    git(&clone, &["init", "-q", "-b", "main"]);
    git(&clone, &["remote", "add", "origin", origin.to_str().unwrap()]);
    // Every git launch consults the deadline exactly once. The clock reads
    // "fresh" for the cat-file and the fetch, then "spent" from then on.
    let reads = Rc::new(Cell::new(0));
    let r = Rc::clone(&reads);
    let deadline = Deadline::with_clock(Duration::from_secs(100), move || {
        r.set(r.get() + 1);
        if r.get() <= 2 {
            Duration::ZERO
        } else {
            Duration::from_secs(200)
        }
    });
    let mut p = pr(1, &[]);
    p.head_sha = Some(head);
    assert!(!ensure_head(&clone, &p, &deadline), "post-fetch check fails closed");
    assert_eq!(reads.get(), 3, "cat-file, fetch, then the spent re-check; nothing after");
    let refs = git(&clone, &["for-each-ref", "refs/loom"]);
    assert!(refs.is_empty(), "no private ref is left behind: {refs}");
}

// --- Real git --------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Commit `lib.rs` = `body` on a new branch `name` off `main`; its head SHA.
fn branch(dir: &Path, name: &str, body: &str) -> String {
    git(dir, &["checkout", "-q", "-b", name, "main"]);
    std::fs::write(dir.join("lib.rs"), body).unwrap();
    git(dir, &["commit", "-q", "-am", name]);
    let head = git(dir, &["rev-parse", "HEAD"]);
    git(dir, &["checkout", "-q", "main"]);
    head
}

#[test]
fn a_conflict_confined_to_changelog_md_is_not_an_ordering_conflict() {
    let tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    assert!(only_changelog_conflicts(&format!("{tree}\nCHANGELOG.md\n")));
    assert!(!only_changelog_conflicts(&format!("{tree}\nCHANGELOG.md\nsrc/lib.rs\n")));
    assert!(!only_changelog_conflicts(&format!("{tree}\nsrc/CHANGELOG.md\n")));
    // No path list (or none readable) fails closed.
    assert!(!only_changelog_conflicts(&format!("{tree}\n")));
    assert!(!only_changelog_conflicts(""));
}

#[test]
fn real_merge_tree_ignores_a_same_spot_changelog_conflict_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("CHANGELOG.md"), "# Log\n").unwrap();
    std::fs::write(dir.join("lib.rs"), "a\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "base"]);
    let edit = |name: &str, log: &str, lib: &str| {
        git(dir, &["checkout", "-q", "-b", name, "main"]);
        std::fs::write(dir.join("CHANGELOG.md"), log).unwrap();
        std::fs::write(dir.join("lib.rs"), lib).unwrap();
        git(dir, &["commit", "-q", "-am", name]);
        let head = git(dir, &["rev-parse", "HEAD"]);
        git(dir, &["checkout", "-q", "main"]);
        head
    };
    let a = edit("a", "# Log\n- a\n", "a\n");
    let b = edit("b", "# Log\n- b\n", "a\n");
    let c = edit("c", "# Log\n- c\n", "c\n");
    let d = edit("d", "# Log\n- d\n", "d\n");
    if merge_tree_conflicts(dir, &a, &a).is_none() {
        eprintln!("git without `merge-tree --write-tree`: skipping");
        return;
    }
    assert_eq!(merge_tree_conflicts(dir, &a, &b), Some(false), "CHANGELOG-only");
    assert_eq!(merge_tree_conflicts(dir, &c, &d), Some(true), "CHANGELOG + lib.rs");
}

#[test]
fn real_merge_tree_distinguishes_clean_mod_appends_from_same_line_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("lib.rs"), "pub mod a;\npub mod c;\npub mod e;\npub mod g;\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "base"]);
    let b = branch(dir, "add-b", "pub mod a;\npub mod b;\npub mod c;\npub mod e;\npub mod g;\n");
    let f = branch(dir, "add-f", "pub mod a;\npub mod c;\npub mod e;\npub mod f;\npub mod g;\n");
    let bb = branch(dir, "add-bb", "pub mod a;\npub mod bb;\npub mod c;\npub mod e;\npub mod g;\n");
    if merge_tree_conflicts(dir, &b, &f).is_none() {
        eprintln!("git without `merge-tree --write-tree`: the check fails closed; skipping");
        return;
    }
    assert_eq!(merge_tree_conflicts(dir, &b, &f), Some(false));
    assert_eq!(merge_tree_conflicts(dir, &b, &bb), Some(true));
    // git reports a missing object as exit 1 on some versions: either way it
    // is never a clean merge.
    assert_ne!(merge_tree_conflicts(dir, &b, &"0".repeat(40)), Some(false), "missing object");

    let at = |n: u32, head: &str| {
        let mut p = pr(n, &[]);
        p.head_sha = Some(head.to_string());
        p
    };
    let prs = [at(1, &b), at(2, &f), at(3, &bb)];
    let checker = live(dir);
    assert!(!checker.conflicts(&prs[0], &prs[1]), "clean mod appends do not conflict");
    assert!(checker.conflicts(&prs[0], &prs[2]), "the same-line pair conflicts");
    // A head that cannot be resolved (no remote to fetch from) fails closed.
    let ghost = at(4, &"1".repeat(40));
    assert!(checker.conflicts(&prs[0], &ghost));
}
