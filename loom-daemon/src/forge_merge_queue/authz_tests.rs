//! Fake-forge protocol tests for the queue authorization protocol (#10256).
//!
//! The fake models GitHub's merge-queue behaviour that matters here: an entry
//! merges only when the required check last concluded `Success` on it. The
//! assertions are on whether the PR *can merge*, not on whether dequeue was
//! called.

#![allow(clippy::unwrap_used)]

use std::cell::{Cell, RefCell};

use super::authz::*;
use super::mode::MergeMode;
use super::ops::*;

const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA2: &str = "fedcba9876543210fedcba9876543210fedcba98";
const PR: u32 = 7;

struct FakeForge {
    head: RefCell<String>,
    queued: RefCell<Option<String>>,
    merged: Cell<bool>,
    facts: RefCell<Result<AuthzFacts, String>>,
    check: RefCell<Option<CheckConclusion>>,
    dequeue_fails: Cell<bool>,
    revoke_on_enqueue: Cell<bool>,
}

impl FakeForge {
    fn new() -> Self {
        Self {
            head: RefCell::new(SHA1.into()),
            queued: RefCell::new(None),
            merged: Cell::new(false),
            facts: RefCell::new(Ok(AuthzFacts::approved(SHA1))),
            check: RefCell::new(None),
            dequeue_fails: Cell::new(false),
            revoke_on_enqueue: Cell::new(false),
        }
    }
    fn read_facts(&self) -> Result<AuthzFacts, String> {
        self.facts.borrow().clone()
    }
    fn edit(&self, f: impl FnOnce(&mut AuthzFacts)) {
        if let Ok(x) = self.facts.borrow_mut().as_mut() {
            f(x);
        }
    }
    fn push_head(&self, sha: &str) {
        *self.head.borrow_mut() = sha.into();
        self.edit(|f| f.head_sha = Fact::Known(sha.into()));
    }
    /// GitHub runs the required check on the merge-group commit.
    fn run_check(&self, store: &dyn GrantStore) {
        if let Some(h) = self.queued.borrow().clone() {
            *self.check.borrow_mut() = Some(merge_group_check(store, PR, &h, self.read_facts()));
        }
    }
    /// GitHub merges an entry only on a passing required check.
    fn try_merge(&self) -> bool {
        let ok = self.queued.borrow().is_some()
            && matches!(&*self.check.borrow(), Some(CheckConclusion::Success));
        if ok {
            self.merged.set(true);
            *self.queued.borrow_mut() = None;
        }
        ok
    }
}

impl QueueApi for FakeForge {
    fn status(&self, pr: u32) -> Result<PrQueueStatus, QueueError> {
        Ok(PrQueueStatus {
            number: pr,
            node_id: "PR_node".into(),
            state: if self.merged.get() {
                PrState::Merged
            } else {
                PrState::Open
            },
            head_oid: self.head.borrow().clone(),
            entry: self.queued.borrow().clone().map(|h| QueueEntry {
                state: "AWAITING_CHECKS".into(),
                position: Some(1),
                head_oid: Some(h),
            }),
        })
    }
    fn enqueue(&self, _pr: u32, _n: &str, expected: &str) -> Result<EnqueueAck, QueueError> {
        if !self.head.borrow().eq_ignore_ascii_case(expected) {
            return Err(QueueError::HeadMismatch {
                pr: PR,
                approved: expected.into(),
                actual: None,
            });
        }
        *self.queued.borrow_mut() = Some(expected.into());
        if self.revoke_on_enqueue.get() {
            self.edit(|f| f.human_hold = Fact::Known(true));
        }
        Ok(EnqueueAck::Enqueued { position: Some(1) })
    }
    fn dequeue(&self, _pr: u32, _n: &str) -> Result<DequeueAck, QueueError> {
        if self.dequeue_fails.get() {
            return Err(QueueError::Forge {
                detail: "503".into(),
            });
        }
        *self.queued.borrow_mut() = None;
        Ok(DequeueAck::Dequeued)
    }
}

fn handoff(f: &FakeForge, store: &MemoryGrantStore) -> Result<EnqueueOutcome, HandoffError> {
    authorize_and_enqueue(MergeMode::Queue, true, f, store, &|| f.read_facts(), PR, SHA1)
}

fn revoke(f: &FakeForge, store: &MemoryGrantStore) -> Revocation {
    revoke_then_dequeue(MergeMode::Queue, true, f, store, PR)
}

#[test]
fn enqueue_then_checks_then_merge() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    assert!(matches!(handoff(&f, &s), Ok(EnqueueOutcome::Enqueued { .. })));
    f.run_check(&s);
    assert!(f.try_merge());
    assert!(f.merged.get());
}

#[test]
fn queued_pr_without_a_grant_cannot_merge() {
    // e.g. someone enqueued by hand, outside Loom.
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    *f.queued.borrow_mut() = Some(SHA1.into());
    f.run_check(&s);
    assert!(!f.try_merge());
}

#[test]
fn revocation_before_enqueue_writes_nothing() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    f.edit(|x| x.approved_label = Fact::Known(false));
    let e = handoff(&f, &s).unwrap_err();
    assert_eq!(e, HandoffError::Denied(vec![DenyReason::ApprovalLabelRevoked]));
    assert!(f.queued.borrow().is_none());
    assert_eq!(s.get(PR).unwrap(), None);
}

#[test]
fn gate_refusal_writes_nothing() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    for (mode, enabled, code) in [
        (MergeMode::Direct, true, "NOT_QUEUE_MODE"),
        (MergeMode::Queue, false, "EXECUTION_DORMANT"),
    ] {
        let e =
            authorize_and_enqueue(mode, enabled, &f, &s, &|| f.read_facts(), PR, SHA1).unwrap_err();
        match e {
            HandoffError::Gate(q) => assert_eq!(q.code(), code),
            other => panic!("{other:?}"),
        }
    }
    assert!(f.queued.borrow().is_none());
    assert_eq!(s.get(PR).unwrap(), None);
}

#[test]
fn head_race_between_read_and_enqueue_is_rejected_and_unauthorized() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    // Facts say SHA1, but the real head already moved (stale read).
    *f.head.borrow_mut() = SHA2.into();
    let e = handoff(&f, &s).unwrap_err();
    assert!(matches!(e, HandoffError::Enqueue(QueueError::HeadMismatch { .. })), "{e:?}");
    assert_eq!(s.get(PR).unwrap(), None, "grant rolled back");
    assert!(f.queued.borrow().is_none());
}

#[test]
fn head_move_after_enqueue_fails_the_check() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    f.push_head(SHA2); // force-push; the queue entry still carries SHA1
    f.run_check(&s);
    assert!(!f.try_merge());
    let c = f.check.borrow().clone().unwrap();
    assert!(
        matches!(c, CheckConclusion::Failure(ref w) if w.iter().any(|r| matches!(r, DenyReason::HeadMoved{..}))),
        "{c:?}"
    );
}

#[test]
fn revocation_during_enqueue_is_rolled_back_in_the_same_call() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    f.revoke_on_enqueue.set(true);
    let e = handoff(&f, &s).unwrap_err();
    match e {
        HandoffError::RevokedDuringEnqueue { why, revocation } => {
            assert_eq!(why, vec![DenyReason::HumanHold]);
            assert!(revocation.safe_to_transition());
        }
        other => panic!("{other:?}"),
    }
    assert!(f.queued.borrow().is_none());
    f.run_check(&s);
    assert!(!f.try_merge());
}

#[test]
fn revocation_while_checks_pending_blocks_merge() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    let r = revoke(&f, &s);
    assert!(r.safe_to_transition() && !r.already_merged());
    f.run_check(&s);
    assert!(!f.try_merge());
}

#[test]
fn failed_dequeue_still_cannot_merge() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    f.dequeue_fails.set(true);
    let r = revoke(&f, &s);
    assert!(r.dequeue.is_err());
    assert!(r.grant_revoked.is_ok());
    assert!(r.safe_to_transition(), "grant revoked is enough to block merge");
    assert!(f.queued.borrow().is_some(), "entry remains queued");
    f.run_check(&s);
    assert!(!f.try_merge(), "queued but unauthorized: cannot merge");
}

#[test]
fn both_revoke_and_dequeue_failing_is_not_safe_to_transition() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    f.dequeue_fails.set(true);
    s.set_down(true);
    let r = revoke(&f, &s);
    assert!(!r.safe_to_transition(), "caller must not invalidate/claim yet");
}

#[test]
fn store_outage_blocks_merge() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    s.set_down(true); // daemon outage while checks run
    f.run_check(&s);
    assert!(!f.try_merge());
    // Restart: grant survives, check passes again.
    s.set_down(false);
    f.run_check(&s);
    assert!(f.try_merge());
}

#[test]
fn forge_fact_outage_blocks_merge() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    *f.facts.borrow_mut() = Err("api 502".into());
    f.run_check(&s);
    assert!(!f.try_merge());
}

#[test]
fn undeterminable_fact_denies() {
    let mut facts = AuthzFacts::approved(SHA1);
    facts.human_hold = Fact::Unknown("label read failed".into());
    assert_eq!(
        evaluate(&facts, SHA1),
        AuthzDecision::Denied(vec![DenyReason::Undeterminable("human hold")])
    );
}

#[test]
fn each_external_change_blocks_merge_without_loom_acting() {
    type Edit = fn(&mut AuthzFacts);
    let cases: [(&str, Edit); 5] = [
        ("label removed", |f| f.approved_label = Fact::Known(false)),
        ("stale verdict", |f| f.verdict_current = Fact::Known(false)),
        ("reviewing claim", |f| f.reviewing_claim = Fact::Known(true)),
        ("human hold", |f| f.human_hold = Fact::Known(true)),
        ("contradiction", |f| f.contradiction = Fact::Known(true)),
    ];
    for (name, edit) in cases {
        let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
        handoff(&f, &s).unwrap();
        f.edit(edit); // a human acts; the daemon never hears about it
        f.run_check(&s);
        assert!(!f.try_merge(), "{name}");
    }
}

#[test]
fn duplicate_revocation_events_are_idempotent() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    let a = revoke(&f, &s);
    let b = revoke(&f, &s);
    assert_eq!(a.dequeue, Ok(DequeueOutcome::Dequeued));
    assert_eq!(b.dequeue, Ok(DequeueOutcome::NotQueued));
    assert!(b.safe_to_transition());
}

#[test]
fn revocation_after_merge_reports_already_merged() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    f.run_check(&s);
    assert!(f.try_merge());
    let r = revoke(&f, &s);
    assert!(r.already_merged());
}

#[test]
fn re_review_after_head_move_needs_a_fresh_grant() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    f.push_head(SHA2);
    revoke(&f, &s);
    // Re-approved at SHA2 but the old grant is gone: no authority until a new handoff.
    f.edit(|x| *x = AuthzFacts::approved(SHA2));
    *f.queued.borrow_mut() = Some(SHA2.into());
    f.run_check(&s);
    assert!(!f.try_merge());
}

/// The residual window. The check passed on the merge-group commit; GitHub
/// has not merged yet; Loom revokes. Nothing Loom controls sits inside this
/// interval, so the entry can still merge. This test documents the gap and is
/// why `INVARIANT_FULLY_DEMONSTRATED` is false and the mode stays dormant.
#[test]
fn known_gap_revocation_after_check_pass_can_still_merge() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    f.run_check(&s); // passes
    f.dequeue_fails.set(true); // and the dequeue loses the race / fails
    let r = revoke(&f, &s);
    assert!(r.grant_revoked.is_ok());
    assert!(f.try_merge(), "documented gap: pass-to-merge window");
    const { assert!(!INVARIANT_FULLY_DEMONSTRATED) };
}
