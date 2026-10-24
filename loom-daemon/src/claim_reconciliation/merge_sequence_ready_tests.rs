//! Fixtures for #10371: ready-first ordering, no holds behind a non-ready
//! head, and the not-ready release of existing soft holds.

use chrono::Utc;

use super::super::{hold_action, release_comment_body, SEQUENCE_LABEL};
use super::*;

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

// --- Acceptance: a convoy behind a non-ready head releases next tick --------

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
    let body = release_comment_body(&m, HoldAction::ReleaseNotReady, None);
    assert!(body.contains("loom:sequence released plan=seq-c0ffee00"), "{body}");
    assert!(body.contains("#10 is not ready"), "{body}");
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

// --- #10465: readiness is transitive ----------------------------------------

#[test]
fn a_soft_hold_behind_a_sequenced_non_landable_pr_is_released_but_a_pin_is_kept() {
    let c = pr(3, "2026-10-01T00:00:00Z", &[]);
    let b = pr(2, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let a = pr(1, "2026-10-03T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let markers = BTreeMap::from([(2, soft(2, 3))]);
    let listing = demote_unlandable(&[c, b, a.clone()], &markers);
    assert!(listing[1].has(NOT_LANDABLE_LABEL));
    assert_eq!(decide(&soft(1, 2), &a, &listing), HoldAction::ReleaseNotReady);
    // A human `after=` pin (no `source=pass`) is still honored.
    let pin = marker(1, 2, None, "seq-c0ffee00");
    assert_eq!(decide(&pin, &a, &listing), HoldAction::HoldHard);
}

#[test]
fn operator_parked_predecessors_are_not_ready() {
    let t = "2026-10-01T00:00:00Z";
    assert!(!ready(&pr(1, t, &[APPROVED_LABEL, "loom:operator"])));
}

// --- #12595: blocked predecessor releases; hold_reason names why ---------------

#[test]
fn a_blocked_predecessor_releases_its_follower_at_an_unchanged_head_but_not_a_moved_one() {
    let follower = pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let m = soft(20, 10);
    let listing = vec![
        pr(10, "2026-10-01T00:00:00Z", &[APPROVED_LABEL, "loom:blocked"]),
        follower.clone(),
    ];
    assert_eq!(decide(&m, &follower, &listing), HoldAction::ReleaseNotReady);
    // The predecessor's head moved off the recorded pin: the readiness rule
    // needs the in-flight signal, so this hold is left to the other rules.
    let moved = PredecessorState {
        head_sha: Some(sha(99)),
        ..open_at_pin(10, &Utc::now().to_rfc3339())
    };
    let base = hold_action(&m, Some(&moved), follower.head_sha.as_deref(), true, 72.0);
    assert_ne!(
        with_readiness(base, &m, Some(&moved), &follower, listing.first()),
        HoldAction::ReleaseNotReady
    );
}

#[test]
fn hold_reason_names_the_first_condition_keeping_a_hold() {
    let now = Utc::now().to_rfc3339();
    let follower = pr(20, "2026-10-02T00:00:00Z", &[APPROVED_LABEL, SEQUENCE_LABEL]);
    let p = open_at_pin(10, &now);
    let ready_head = pr(10, "2026-10-01T00:00:00Z", &[APPROVED_LABEL]);
    let why = |m: &SequenceMarker, pred: Option<&PredecessorState>, h: Option<&SequencePr>| {
        hold_reason(m, pred, &follower, h)
    };
    assert!(why(&marker(20, 10, None, "x"), Some(&p), None).contains("human hold"));
    assert!(
        why(&marker(20, 10, Some(SOURCE_PASS), "cons-1"), Some(&p), None).contains("consolidation")
    );
    assert!(why(&soft(20, 10), Some(&p), None).contains("outside this tick's listing"));
    assert!(why(&soft(20, 10), None, Some(&ready_head)).contains("not read open"));
    assert!(why(&soft(20, 10), Some(&p), Some(&ready_head)).contains("is ready"));
    let mut stacked = follower.clone();
    stacked.base_ref = ready_head.head_ref.clone();
    assert!(hold_reason(&soft(20, 10), Some(&p), &stacked, Some(&ready_head)).contains("stacked"));
    let blocked = pr(10, "2026-10-01T00:00:00Z", &[APPROVED_LABEL, "loom:blocked"]);
    assert!(why(&soft(20, 10), Some(&p), Some(&blocked)).contains("not ready"));
}
