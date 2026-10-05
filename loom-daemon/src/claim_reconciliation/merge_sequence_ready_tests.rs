//! Fixtures for #10371: ready-first ordering, no holds behind a non-ready
//! head, and the not-ready release of existing soft holds.

use std::collections::BTreeSet;

use chrono::Utc;

use super::super::{hold_action, plan_repo_with, release_comment_body, SEQUENCE_LABEL};
use super::*;
use crate::work_finder::operator_priority::OPERATOR_PRIORITY_LABEL;

fn sha(n: u32) -> String {
    format!("{n:040x}")
}

fn pr(number: u32, created: &str, labels: &[&str]) -> SequencePr {
    SequencePr {
        number,
        created_at: created.to_string(),
        updated_at: created.to_string(),
        head_sha: Some(sha(number)),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
    }
}

/// Every PR in `nums` changes the same file.
fn all_share(nums: &[u32]) -> BTreeMap<u32, BTreeSet<String>> {
    nums.iter()
        .map(|n| (*n, BTreeSet::from(["src/shared.rs".to_string()])))
        .collect()
}

fn edges(prs: &[SequencePr], markers: &BTreeMap<u32, SequenceMarker>) -> Vec<(u32, u32)> {
    let nums: Vec<u32> = prs.iter().map(|p| p.number).collect();
    plan_repo_with(prs, &all_share(&nums), markers, &BTreeSet::new())
        .iter()
        .flat_map(|g| g.edges.iter().map(|e| (e.follower, e.after)))
        .collect()
}

fn order(prs: &[SequencePr]) -> Vec<u32> {
    let nums: Vec<u32> = prs.iter().map(|p| p.number).collect();
    let groups = plan_repo_with(prs, &all_share(&nums), &BTreeMap::new(), &BTreeSet::new());
    assert_eq!(groups.len(), 1, "{groups:?}");
    groups[0].order.clone()
}

/// The marker `follower` carries behind `after`, pinned at both heads.
fn marker(follower: u32, after: u32, source: Option<&str>, plan: &str) -> SequenceMarker {
    SequenceMarker {
        after,
        pred_head: sha(after),
        follower_head: sha(follower),
        plan: plan.into(),
        source: source.map(str::to_string),
    }
}

fn soft(follower: u32, after: u32) -> SequenceMarker {
    marker(follower, after, Some(SOURCE_PASS), "seq-c0ffee00")
}

/// The predecessor read from the pulls API: open at its pinned head, last
/// active `updated`.
fn open_at_pin(n: u32, updated: &str) -> PredecessorState {
    PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(sha(n)),
        updated_at: Some(updated.to_string()),
    }
}

/// Phase 1's decision for `follower`'s hold, with the predecessor's row in
/// this tick's listing taken from `listing` (exactly as the pass does).
fn decide(m: &SequenceMarker, follower: &SequencePr, listing: &[SequencePr]) -> HoldAction {
    let p = open_at_pin(m.after, &Utc::now().to_rfc3339());
    let fh = follower.head_sha.as_deref();
    let base = hold_action(m, Some(&p), fh, follower.has(APPROVED_LABEL), 72.0);
    let head = listing.iter().find(|h| h.number == m.after);
    with_readiness(base, m, Some(&p), follower, head)
}

const CR: &str = "loom:changes-requested";

// --- The predicate ---------------------------------------------------------

#[test]
fn ready_means_approved_with_no_contrary_label() {
    let t = "2026-10-01T00:00:00Z";
    assert!(ready(&pr(1, t, &[APPROVED_LABEL])));
    assert!(!ready(&pr(1, t, &[])), "no verdict");
    assert!(!ready(&pr(1, t, &["loom:review-requested"])));
    for l in NOT_READY_LABELS {
        assert!(!ready(&pr(1, t, &[APPROVED_LABEL, l])), "{l}");
    }
    // A held approved PR is still ready: it lands as soon as its own
    // predecessor does, which is what the order is for.
    assert!(ready(&pr(1, t, &[APPROVED_LABEL, SEQUENCE_LABEL])));
}

// --- Acceptance: an approved PR is never held behind an older non-ready one --

#[test]
fn an_approved_pr_overlapping_an_older_changes_requested_pr_is_not_held() {
    let older_cr = pr(9897, "2026-10-01T00:00:00Z", &[CR]);
    let approved = pr(10185, "2026-10-03T00:00:00Z", &[APPROVED_LABEL]);
    let unrelated = pr(1, "2026-09-01T00:00:00Z", &[]);
    let prs = [older_cr.clone(), approved.clone(), unrelated];
    let nums = [9897, 10185];
    let groups = plan_repo_with(&prs, &all_share(&nums), &BTreeMap::new(), &BTreeSet::new());
    assert_eq!(groups.len(), 1, "{groups:?}");
    let g = &groups[0];
    assert_eq!(g.order, vec![10185, 9897], "ready first despite being younger");
    assert!(
        g.edges.iter().all(|e| e.follower != 10185),
        "the approved PR gets no hold: {:?}",
        g.edges
    );

    // The same with the stale-verdict shape (`loom:pr` still on beside the
    // changes-requested label): still not ready, still no hold.
    let both = pr(9897, "2026-10-01T00:00:00Z", &[APPROVED_LABEL, CR]);
    let prs = [both, approved, pr(1, "2026-09-01T00:00:00Z", &[])];
    assert!(edges(&prs, &BTreeMap::new())
        .iter()
        .all(|(f, _)| *f != 10185));
}

#[test]
fn no_edge_is_ever_planned_behind_a_non_ready_predecessor() {
    // Three non-ready PRs: nothing to save, so nothing is held.
    let prs = [
        pr(1, "2026-10-01T00:00:00Z", &[]),
        pr(2, "2026-10-01T01:00:00Z", &["loom:review-requested"]),
        pr(3, "2026-10-01T02:00:00Z", &[APPROVED_LABEL, "loom:ci-failure"]),
    ];
    assert!(edges(&prs, &BTreeMap::new()).is_empty());

    // Mixed: every edge's predecessor is ready.
    let prs = [
        pr(1, "2026-10-01T00:00:00Z", &[CR]),
        pr(2, "2026-10-01T01:00:00Z", &[APPROVED_LABEL]),
        pr(3, "2026-10-01T02:00:00Z", &[]),
        pr(4, "2026-10-01T03:00:00Z", &[APPROVED_LABEL]),
    ];
    let planned = edges(&prs, &BTreeMap::new());
    assert_eq!(planned, vec![(4, 2), (1, 4)], "approved chain; #3 waits for no one");
    let by: BTreeMap<u32, &SequencePr> = prs.iter().map(|p| (p.number, p)).collect();
    assert!(planned.iter().all(|(_, a)| ready(by[a])), "{planned:?}");
}

// --- Acceptance: a convoy behind a non-ready head releases next tick --------

#[test]
fn a_convoy_of_approved_prs_behind_a_non_approved_head_releases_on_the_next_tick() {
    // The 2026-10-05 chain: #10365 → #10308 → #10228 → #10205 → #10185 →
    // #9897, every link approved and soft-held, the head changes-requested.
    let head = pr(9897, "2026-09-30T00:00:00Z", &[CR]);
    let chain = [10185, 10205, 10228, 10308, 10365];
    let mut listing = vec![head.clone()];
    for (i, n) in chain.iter().enumerate() {
        let created = format!("2026-10-0{}T00:00:00Z", i + 1);
        listing.push(pr(*n, &created, &[APPROVED_LABEL, SEQUENCE_LABEL]));
    }
    let after = |i: usize| if i == 0 { 9897 } else { chain[i - 1] };

    // Tick N: the link holding onto the non-ready head is released; every
    // other link waits for an APPROVED predecessor, the order that saves
    // rebases among ready work (it drains as each lands).
    let decisions: Vec<HoldAction> = chain
        .iter()
        .enumerate()
        .map(|(i, n)| decide(&soft(*n, after(i)), &listing[i + 1], &listing))
        .collect();
    assert_eq!(decisions[0], HoldAction::ReleaseNotReady);
    assert!(decisions[1..].iter().all(|d| *d == HoldAction::HoldSoft), "{decisions:?}");

    // Before #10371 every link held, waiting on the changes-requested head.
    let p = open_at_pin(9897, &Utc::now().to_rfc3339());
    assert_eq!(
        hold_action(&soft(10185, 9897), Some(&p), Some(&sha(10185)), true, 72.0),
        HoldAction::HoldSoft
    );

    // Tick N+1: re-planned from scratch (no holds), the approved work comes
    // first, nothing waits on the head, and the head itself goes last.
    let unheld: Vec<SequencePr> = listing
        .iter()
        .map(|p| {
            let mut p = p.clone();
            p.labels.retain(|l| l != SEQUENCE_LABEL);
            p
        })
        .collect();
    let mut want = chain.to_vec();
    want.push(9897);
    assert_eq!(order(&unheld), want);
    let replanned = edges(&unheld, &BTreeMap::new());
    assert!(replanned.iter().all(|(_, a)| *a != 9897), "{replanned:?}");
    assert!(
        !replanned.iter().any(|(f, _)| *f == 10185),
        "the convoy's head is free: {replanned:?}"
    );
}

#[test]
fn a_hold_releases_when_the_predecessor_loses_its_verdict_or_gains_a_blocking_label() {
    let follower = pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let m = soft(20, 10);
    let at = |labels: &[&str]| vec![pr(10, "2026-10-01T00:00:00Z", labels), follower.clone()];
    assert_eq!(decide(&m, &follower, &at(&[APPROVED_LABEL])), HoldAction::HoldSoft);
    assert_eq!(decide(&m, &follower, &at(&[])), HoldAction::ReleaseNotReady, "lost loom:pr");
    for l in NOT_READY_LABELS {
        assert_eq!(
            decide(&m, &follower, &at(&[APPROVED_LABEL, l])),
            HoldAction::ReleaseNotReady,
            "{l}"
        );
    }
    // An unapproved follower is released too: the order has no subject.
    let draft_like = pr(20, "2026-10-02T00:00:00Z", &[SEQUENCE_LABEL]);
    assert_eq!(decide(&m, &draft_like, &at(&[CR])), HoldAction::ReleaseNotReady);
    let body = release_comment_body(&m, HoldAction::ReleaseNotReady);
    assert!(body.contains("loom:sequence released plan=seq-c0ffee00"), "{body}");
    assert!(body.contains("#10 is not ready"), "{body}");
}

#[test]
fn a_verdict_flip_re_plans_the_order_on_the_next_tick() {
    // #10 (older) was approved, #20 was planned behind it. #10 then got
    // changes requested: tick N releases #20, tick N+1 puts #20 first and
    // never re-holds it behind #10.
    let follower = pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let flipped = pr(10, "2026-10-01T00:00:00Z", &[CR]);
    let listing = [
        flipped.clone(),
        follower.clone(),
        pr(1, "2026-09-01T00:00:00Z", &[]),
    ];
    assert_eq!(decide(&soft(20, 10), &follower, &listing), HoldAction::ReleaseNotReady);
    let next = [
        flipped.clone(),
        pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL]),
        pr(30, "2026-10-03T00:00:00Z", &[]),
    ];
    assert_eq!(order(&next), vec![20, 10, 30]);
    assert!(!edges(&next, &BTreeMap::new()).contains(&(20, 10)));
    // Stable across ticks: the same input re-plans identically.
    assert_eq!(edges(&next, &BTreeMap::new()), edges(&next, &BTreeMap::new()));
}

// --- Acceptance: hard markers are unchanged ---------------------------------

#[test]
fn hard_markers_never_release_or_expire_behind_a_non_ready_head() {
    let follower = pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let hard = marker(20, 10, None, "manual");
    for labels in [&[][..], &[CR][..], &[APPROVED_LABEL, "loom:blocked"][..]] {
        let listing = [pr(10, "2026-10-01T00:00:00Z", labels), follower.clone()];
        assert_eq!(decide(&hard, &follower, &listing), HoldAction::HoldHard, "{labels:?}");
    }
    // Quiet for years and not ready: a hard hold still never expires.
    let ancient = open_at_pin(10, "2020-01-01T00:00:00Z");
    let base = hold_action(&hard, Some(&ancient), Some(&sha(20)), true, 72.0);
    assert_eq!(base, HoldAction::HoldHard);
    let head = pr(10, "2020-01-01T00:00:00Z", &[CR]);
    assert_eq!(
        with_readiness(base, &hard, Some(&ancient), &follower, Some(&head)),
        HoldAction::HoldHard
    );
    // A non-`pass` source is also hard.
    let other = marker(20, 10, Some("human"), "manual");
    let listing = [head, follower.clone()];
    assert_eq!(decide(&other, &follower, &listing), HoldAction::HoldHard);
}

#[test]
fn reservations_and_every_other_decision_are_left_alone() {
    let follower = pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let head = pr(10, "2026-10-01T00:00:00Z", &[CR]);
    let listing = [head.clone(), follower.clone()];
    // Consolidation reservations keep ADR-0023's contract.
    let cons = marker(20, 10, Some(SOURCE_PASS), "cons-ab12cd34");
    assert_eq!(decide(&cons, &follower, &listing), HoldAction::HoldSoft);
    // Predecessor outside the listing: fail closed.
    assert_eq!(
        decide(&soft(20, 10), &follower, std::slice::from_ref(&follower)),
        HoldAction::HoldSoft
    );
    // Unreadable predecessor or unknown follower head: fail closed.
    let m = soft(20, 10);
    let p = open_at_pin(10, &Utc::now().to_rfc3339());
    let fh = &follower;
    assert_eq!(
        with_readiness(HoldAction::HoldSoft, &m, None, fh, Some(&head)),
        HoldAction::HoldSoft
    );
    let headless = SequencePr {
        head_sha: None,
        ..follower.clone()
    };
    assert_eq!(
        with_readiness(HoldAction::HoldSoft, &m, Some(&p), &headless, Some(&head)),
        HoldAction::HoldSoft
    );
    // A moved predecessor head is the void path, not this one.
    let moved = PredecessorState {
        head_sha: Some(sha(777)),
        ..p.clone()
    };
    assert_eq!(
        with_readiness(HoldAction::HoldSoft, &m, Some(&moved), fh, Some(&head)),
        HoldAction::HoldSoft
    );
    for a in [
        HoldAction::Release,
        HoldAction::ReleaseDissolved,
        HoldAction::VoidAndReplan,
        HoldAction::Expire,
        HoldAction::ReleaseStalled,
        HoldAction::HoldHard,
    ] {
        assert_eq!(with_readiness(a, &m, Some(&p), fh, Some(&head)), a);
    }
}

// --- Ordering inside a tier -------------------------------------------------

#[test]
fn stars_first_and_oldest_first_within_each_readiness_tier() {
    let prs = [
        pr(1, "2026-10-01T00:00:00Z", &[]),
        pr(2, "2026-10-01T01:00:00Z", &[OPERATOR_PRIORITY_LABEL]),
        pr(3, "2026-10-01T02:00:00Z", &[APPROVED_LABEL]),
        pr(4, "2026-10-01T03:00:00Z", &[APPROVED_LABEL, OPERATOR_PRIORITY_LABEL]),
        pr(5, "2026-10-01T04:00:00Z", &[APPROVED_LABEL]),
        pr(6, "2026-10-01T05:00:00Z", &[]),
    ];
    // Ready tier: star #4, then #3, #5 by age. Then the rest: star #2, then
    // #1, #6 by age. A star never lifts a non-ready PR over ready work.
    assert_eq!(order(&prs), vec![4, 3, 5, 2, 1, 6]);
}

#[test]
fn a_constraint_edge_still_beats_readiness() {
    // A trusted marker puts the approved #2 after the non-ready #1: the
    // planner honors it (it never rewrites someone's stated order).
    let prs = [
        pr(1, "2026-10-01T00:00:00Z", &[CR]),
        pr(2, "2026-10-01T01:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]),
        pr(3, "2026-10-01T02:00:00Z", &[APPROVED_LABEL]),
    ];
    let markers = BTreeMap::from([(2, marker(2, 1, None, "manual"))]);
    let groups = plan_repo_with(&prs, &all_share(&[1, 2, 3]), &markers, &BTreeSet::new());
    assert_eq!(groups[0].order, vec![3, 1, 2]);

    // A stacked base likewise: #5 is based on non-ready #4's branch.
    let base = pr(4, "2026-10-01T00:00:00Z", &[]);
    let mut stacked = pr(5, "2026-10-01T01:00:00Z", &[APPROVED_LABEL]);
    stacked.base_ref = base.head_ref.clone();
    let prs = [base, stacked, pr(6, "2026-10-01T02:00:00Z", &[])];
    let groups = plan_repo_with(&prs, &all_share(&[4, 5, 6]), &BTreeMap::new(), &BTreeSet::new());
    assert_eq!(groups[0].order, vec![4, 5, 6]);
    // The stacked edge is a dependency, not a rebase-saving preference: it is
    // kept behind the non-ready base, so the approved #5 cannot land into an
    // unready branch. No shared-files edge is written behind #4.
    let planned: Vec<(u32, u32, EdgeReason)> = groups[0]
        .edges
        .iter()
        .map(|e| (e.follower, e.after, e.reason))
        .collect();
    assert!(planned.contains(&(5, 4, EdgeReason::StackedBase)), "{planned:?}");
    assert!(
        planned
            .iter()
            .all(|(_, a, r)| *a != 4 || *r == EdgeReason::StackedBase),
        "{planned:?}"
    );
}

#[test]
fn a_stacked_soft_hold_stays_held_behind_a_non_ready_base() {
    let base = pr(10, "2026-10-01T00:00:00Z", &[CR]);
    let mut stacked = pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    stacked.base_ref = base.head_ref.clone();
    let m = soft(20, 10);
    let listing = [base.clone(), stacked.clone()];
    assert_eq!(decide(&m, &stacked, &listing), HoldAction::HoldSoft);
    // Unknown branch names cannot prove the pair is unstacked: fail closed,
    // exactly as the #10077 no-overlap release does.
    let mut unknown_base = stacked.clone();
    unknown_base.base_ref = String::new();
    assert_eq!(decide(&m, &unknown_base, &listing), HoldAction::HoldSoft);
    let mut nameless = base.clone();
    nameless.head_ref = String::new();
    let follower = pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    assert_eq!(decide(&m, &follower, &[nameless, follower.clone()]), HoldAction::HoldSoft);
    // The same pair, unstacked, is released (the control).
    assert_eq!(decide(&m, &follower, &[base, follower.clone()]), HoldAction::ReleaseNotReady);
}

#[test]
fn an_all_ready_group_orders_exactly_oldest_first() {
    let prs = [
        pr(3, "2026-10-01T02:00:00Z", &[APPROVED_LABEL]),
        pr(1, "2026-10-01T00:00:00Z", &[APPROVED_LABEL]),
        pr(2, "2026-10-01T01:00:00Z", &[APPROVED_LABEL]),
    ];
    assert_eq!(order(&prs), vec![1, 2, 3]);
    assert_eq!(edges(&prs, &BTreeMap::new()), vec![(2, 1), (3, 2)]);
}
