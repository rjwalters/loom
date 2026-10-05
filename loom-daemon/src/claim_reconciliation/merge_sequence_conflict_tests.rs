//! Fixtures for #10350: ordering edges require a real merge conflict.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use super::super::overlap::{with_no_overlap, TickFiles};
use super::super::{
    overlap_components_with, plan_group_with, plan_repo_conflicts, plan_repo_with, EdgeReason,
    HoldAction, SequenceGroup, SequenceMarker, SequencePr, SEQUENCE_LABEL,
};
use super::*;
use crate::claim_reconciliation::read_cache;
use crate::merge_pr::sequence::PredecessorState;

const LIB_RS: &str = "loom-daemon/src/lib.rs";

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
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
    }
}

fn all_share(prs: &[SequencePr], path: &str) -> BTreeMap<u32, BTreeSet<String>> {
    prs.iter()
        .map(|p| (p.number, BTreeSet::from([path.to_string()])))
        .collect()
}

fn edges(groups: &[SequenceGroup]) -> Vec<(u32, u32, EdgeReason)> {
    groups
        .iter()
        .flat_map(|g| g.edges.iter().map(|e| (e.follower, e.after, e.reason)))
        .collect()
}

fn no_conflict(_: &SequencePr, _: &SequencePr) -> bool {
    false
}

const NONE: BTreeMap<u32, SequenceMarker> = BTreeMap::new();

// --- Pure planner ----------------------------------------------------------

#[test]
fn a_shared_hot_file_without_a_real_conflict_yields_no_edge() {
    let prs = [pr(1, &[]), pr(2, &[]), pr(3, &[])];
    let f = all_share(&prs, LIB_RS);
    let eligible: Vec<&SequencePr> = prs.iter().collect();
    let comps = overlap_components_with(&eligible, &f, &no_conflict);
    assert_eq!(comps, vec![vec![1], vec![2], vec![3]], "separate components");
    let by: BTreeMap<u32, SequencePr> = prs.iter().map(|p| (p.number, p.clone())).collect();
    let g = plan_group_with("seq-t", &[1, 2, 3], &by, &f, &no_conflict);
    assert!(g.edges.is_empty(), "{:?}", g.edges);
    let groups = plan_repo_conflicts(&prs, &f, &NONE, &BTreeSet::new(), &no_conflict);
    assert!(groups.is_empty(), "{groups:?}");
}

#[test]
fn a_real_conflict_keeps_the_edge_oldest_first() {
    let prs = [pr(3, &[]), pr(1, &[]), pr(2, &[])];
    let f = all_share(&prs, LIB_RS);
    let conflicts = |_: &SequencePr, _: &SequencePr| true;
    let groups = plan_repo_conflicts(&prs, &f, &NONE, &BTreeSet::new(), &conflicts);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].order, vec![1, 2, 3]);
    assert_eq!(
        edges(&groups),
        vec![
            (2, 1, EdgeReason::SharedFiles),
            (3, 2, EdgeReason::SharedFiles)
        ]
    );
    // The predicate-free entry point is the same filename-only plan.
    assert_eq!(groups, plan_repo_with(&prs, &f, &NONE, &BTreeSet::new()));
}

#[test]
fn only_the_conflicting_pair_is_ordered() {
    let prs = [pr(1, &[]), pr(2, &[]), pr(3, &[])];
    let f = all_share(&prs, LIB_RS);
    let conflicts = |a: &SequencePr, b: &SequencePr| {
        let mut p = [a.number, b.number];
        p.sort_unstable();
        p == [1, 3]
    };
    let groups = plan_repo_conflicts(&prs, &f, &NONE, &BTreeSet::new(), &conflicts);
    assert_eq!(edges(&groups), vec![(3, 1, EdgeReason::SharedFiles)]);
}

#[test]
fn a_stacked_base_edge_is_never_dropped_by_the_predicate() {
    let base = pr(1, &[]);
    let mut stacked = pr(2, &[]);
    stacked.base_ref = base.head_ref.clone();
    let prs = [base, stacked, pr(3, &[])];
    let f = all_share(&prs, LIB_RS);
    let groups = plan_repo_conflicts(&prs, &f, &NONE, &BTreeSet::new(), &no_conflict);
    assert_eq!(edges(&groups), vec![(2, 1, EdgeReason::StackedBase)]);
}

/// The 2026-10-05 shape: ~20 PRs that each add a distinct `pub mod` line to
/// `lib.rs`, the oldest a stalled head on a human hold. No follower waits.
#[test]
fn the_2026_10_05_lib_rs_component_is_not_serialized() {
    let mut prs: Vec<SequencePr> = (1..=20).map(|n| pr(n, &["loom:pr"])).collect();
    prs[0].labels = vec!["loom:operator".to_string()];
    let f = all_share(&prs, LIB_RS);
    let stalled = BTreeSet::from([1]);
    let groups = plan_repo_conflicts(&prs, &f, &NONE, &stalled, &no_conflict);
    assert!(groups.is_empty(), "no follower is sequenced: {groups:?}");
    // Before #10350 the same input was one 19-member chain.
    assert_eq!(edges(&plan_repo_with(&prs, &f, &NONE, &stalled)).len(), 18);

    // Holds recorded behind the held head before the fix are released by
    // Phase 1 with the same predicate.
    let pred_state = PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(sha(1)),
        updated_at: None,
    };
    for n in 2..=20 {
        let mut follower = pr(n, &["loom:pr", SEQUENCE_LABEL]);
        follower.created_at = prs[n as usize - 1].created_at.clone();
        let marker = SequenceMarker {
            after: 1,
            pred_head: sha(1),
            follower_head: sha(n),
            plan: "seq-0badc0de".into(),
            source: Some("pass".into()),
        };
        let mut cache = TickFiles::default();
        let fetch = |p: &SequencePr| f.get(&p.number).cloned();
        let ps = Some(&pred_state);
        let action = HoldAction::HoldSoft;
        let decided =
            with_no_overlap(action, &marker, ps, &follower, &prs, &mut cache, fetch, &no_conflict);
        assert_eq!(decided, HoldAction::ReleaseNoOverlap, "follower #{n}");
        // A failed (fail-closed) conflict check keeps it.
        let mut cache = TickFiles::default();
        let fetch = |p: &SequencePr| f.get(&p.number).cloned();
        let kept =
            with_no_overlap(action, &marker, ps, &follower, &prs, &mut cache, fetch, &|_, _| true);
        assert_eq!(kept, HoldAction::HoldSoft, "follower #{n}");
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
    let files = all_share(&prs, "lib.rs");
    let checker = live(dir);
    let conflicts = |x: &SequencePr, y: &SequencePr| checker.conflicts(x, y);
    let groups = plan_repo_conflicts(&prs, &files, &NONE, &BTreeSet::new(), &conflicts);
    assert_eq!(
        edges(&groups),
        vec![(3, 1, EdgeReason::SharedFiles)],
        "only the same-line pair is ordered"
    );
    // A head that cannot be resolved (no remote to fetch from) fails closed.
    let ghost = at(4, &"1".repeat(40));
    assert!(checker.conflicts(&prs[0], &ghost));
}
