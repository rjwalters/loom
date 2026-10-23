//! Fake-forge protocol tests for the queue authorization protocol (#10256).
//!
//! The fake models GitHub's merge-queue behaviour that matters here: an entry
//! merges only when the required check last concluded `Success` on it. The
//! assertions are on whether the PR *can merge*, not on whether dequeue was
//! called.

#![allow(clippy::unwrap_used)]

use std::cell::{Cell, RefCell};

use super::authz::*;
use super::group_authz::{StatusApi, StatusState};
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
    /// Latest `loom/merge-authorization` status per commit.
    head_status: RefCell<std::collections::HashMap<String, StatusState>>,
    status_fails: Cell<bool>,
    /// The writer credential lacks `statuses: write` ([`StatusApi::can_post_statuses`]).
    cannot_post: Cell<bool>,
    /// GitHub only enqueues a PR whose required check is green on its head.
    require_head_status: Cell<bool>,
    dequeue_fails: Cell<bool>,
    revoke_on_enqueue: Cell<bool>,
    enqueue_fails: Cell<bool>,
}

impl FakeForge {
    fn new() -> Self {
        Self {
            head: RefCell::new(SHA1.into()),
            queued: RefCell::new(None),
            merged: Cell::new(false),
            facts: RefCell::new(Ok(AuthzFacts::approved(SHA1))),
            check: RefCell::new(None),
            head_status: RefCell::new(std::collections::HashMap::new()),
            status_fails: Cell::new(false),
            cannot_post: Cell::new(false),
            require_head_status: Cell::new(true),
            dequeue_fails: Cell::new(false),
            revoke_on_enqueue: Cell::new(false),
            enqueue_fails: Cell::new(false),
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

impl StatusApi for FakeForge {
    fn post_status(&self, commit: &str, s: StatusState, _d: &str) -> Result<(), String> {
        if self.status_fails.get() {
            return Err("statuses api 503".into());
        }
        self.head_status.borrow_mut().insert(commit.into(), s);
        Ok(())
    }
    fn can_post_statuses(&self) -> Result<(), String> {
        if self.cannot_post.get() {
            return Err("installation has `statuses: read`".into());
        }
        Ok(())
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
        if self.enqueue_fails.get() {
            return Err(QueueError::Forge {
                detail: "502 transient".into(),
            });
        }
        let green = self.head_status.borrow().get(expected) == Some(&StatusState::Success);
        if self.require_head_status.get() && !green {
            return Err(QueueError::Forge {
                detail: "Required status check \"loom/merge-authorization\" is expected".into(),
            });
        }
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
    authorize_and_enqueue(MergeMode::Queue, true, f, store, f, &|| f.read_facts(), PR, SHA1)
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
        let e = authorize_and_enqueue(mode, enabled, &f, &s, &f, &|| f.read_facts(), PR, SHA1)
            .unwrap_err();
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

#[test]
fn handoff_posts_success_on_the_approved_head_before_enqueue() {
    // `FakeForge::enqueue` refuses unless the head is green, like GitHub.
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    assert!(matches!(handoff(&f, &s), Ok(EnqueueOutcome::Enqueued { .. })));
    assert_eq!(f.head_status.borrow().get(SHA1), Some(&StatusState::Success));
    assert_eq!(f.head_status.borrow().len(), 1, "only the pinned head is written");
}

#[test]
fn enqueue_refused_without_the_head_status_is_the_bug() {
    // Without the head write the fake forge refuses, as loom-ui#4826 saw.
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    f.require_head_status.set(true);
    let e = authorize_and_enqueue(
        MergeMode::Queue,
        true,
        &f,
        &s,
        &NoStatus,
        &|| f.read_facts(),
        PR,
        SHA1,
    )
    .unwrap_err();
    assert!(matches!(e, HandoffError::Enqueue(_)), "{e:?}");
    assert_eq!(s.get(PR).unwrap(), None);
}

struct NoStatus;
impl StatusApi for NoStatus {
    fn post_status(&self, _: &str, _: StatusState, _: &str) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn status_write_failure_aborts_before_enqueue_and_revokes_the_grant() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    f.status_fails.set(true);
    let e = handoff(&f, &s).unwrap_err();
    assert!(matches!(e, HandoffError::HeadStatus(_)), "{e:?}");
    assert!(f.queued.borrow().is_none());
    assert_eq!(s.get(PR).unwrap(), None, "grant revoked");
}

#[test]
fn rolled_back_handoff_withdraws_the_head_success() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    f.revoke_on_enqueue.set(true);
    assert!(handoff(&f, &s).is_err());
    assert_eq!(f.head_status.borrow().get(SHA1), Some(&StatusState::Failure));
}

#[test]
fn transient_enqueue_failure_leaves_pending_not_failure_and_retry_enqueues() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    f.enqueue_fails.set(true);
    let e = handoff(&f, &s).unwrap_err();
    assert!(matches!(e, HandoffError::Enqueue(_)), "{e:?}");
    assert_eq!(s.get(PR).unwrap(), None, "grant revoked");
    assert_eq!(f.head_status.borrow().get(SHA1), Some(&StatusState::Pending));
    f.enqueue_fails.set(false);
    assert!(matches!(handoff(&f, &s), Ok(EnqueueOutcome::Enqueued { .. })));
}

#[test]
fn a_new_head_never_inherits_the_old_success() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    handoff(&f, &s).unwrap();
    f.push_head(SHA2);
    assert_eq!(f.head_status.borrow().get(SHA2), None);
    assert!(handoff_at(&f, &s, SHA2).is_err(), "SHA2 needs its own grant and status");
}

fn handoff_at(
    f: &FakeForge,
    store: &MemoryGrantStore,
    sha: &str,
) -> Result<EnqueueOutcome, HandoffError> {
    // Facts still approve SHA1 only: evaluation denies a moved head.
    authorize_and_enqueue(MergeMode::Queue, true, f, store, f, &|| f.read_facts(), PR, sha)
}

// ---- #12579: idempotent handoff and the writer preflight

#[test]
fn queued_head_with_a_live_grant_is_a_noop_even_for_a_writer_that_cannot_post() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    assert!(matches!(handoff(&f, &s), Ok(EnqueueOutcome::Enqueued { .. })));
    f.cannot_post.set(true);
    f.status_fails.set(true);
    f.head_status.borrow_mut().clear();
    // The grant was written by another identity; its case differs.
    s.put(Grant {
        pr: PR,
        approved_sha: SHA1.to_ascii_uppercase(),
    })
    .unwrap();
    assert!(matches!(handoff(&f, &s), Ok(EnqueueOutcome::AlreadyQueued { .. })));
    assert!(f.queued.borrow().is_some(), "still queued");
    assert!(s.get(PR).unwrap().is_some(), "grant not revoked");
    assert!(f.head_status.borrow().is_empty(), "no status written");
}

#[test]
fn writer_that_cannot_post_refuses_before_any_grant_or_revoke() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    f.cannot_post.set(true);
    let e = handoff(&f, &s).unwrap_err();
    assert!(
        matches!(&e, HandoffError::WriterCannotPostStatuses(d) if d.contains("statuses")),
        "{e:?}"
    );
    assert_eq!(s.get(PR).unwrap(), None, "no grant written");
    assert!(f.head_status.borrow().is_empty(), "no status written");
}

#[test]
fn queued_entry_without_a_grant_is_not_dequeued_by_a_writer_that_cannot_post() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    *f.queued.borrow_mut() = Some(SHA1.into());
    f.cannot_post.set(true);
    assert!(matches!(
        handoff(&f, &s).unwrap_err(),
        HandoffError::WriterCannotPostStatuses(_)
    ));
    assert!(f.queued.borrow().is_some(), "the PR stays queued");
}

#[test]
fn grant_for_another_head_is_not_a_noop() {
    let (f, s) = (FakeForge::new(), MemoryGrantStore::default());
    *f.queued.borrow_mut() = Some(SHA1.into());
    s.put(Grant {
        pr: PR,
        approved_sha: "f".repeat(40),
    })
    .unwrap();
    assert!(matches!(handoff(&f, &s), Ok(EnqueueOutcome::AlreadyQueued { .. })));
    assert_eq!(s.get(PR).unwrap().unwrap().approved_sha, SHA1, "re-granted for the head");
}

// ---- #12679: forge.mergeQueue.statusCommand replaces the writer-App gate

mod status_command_route {
    use super::*;
    use crate::forge_merge_queue::gh_lifecycle::GhLifecycleForge;
    use crate::forge_merge_queue::group_authz::revoke_refail_dequeue;

    /// A repo root whose config routes status posts to a recording stub that
    /// exits `exit`. The `gh` binary is a path that cannot run, so any status
    /// call made with the daemon's own token would fail loudly.
    struct Route {
        dir: tempfile::TempDir,
        forge: GhLifecycleForge,
    }

    impl Route {
        fn new(exit: i32) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("calls.log");
            let script = format!(
                "echo \"$@\" >> '{}'; case \"$*\" in *--dry-run*) exit {exit};; esac; exit {exit}",
                log.display()
            );
            let cfg = serde_json::json!({"forge": {"mergeQueue": {"statusCommand":
                ["sh", "-c", script, "stub"]}}});
            std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
            std::fs::write(dir.path().join(".loom/config.json"), cfg.to_string()).unwrap();
            let forge = GhLifecycleForge::new("/nonexistent/gh", dir.path(), "o/r").unwrap();
            Self { dir, forge }
        }
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    fn handoff_via(
        f: &FakeForge,
        s: &MemoryGrantStore,
        r: &Route,
    ) -> Result<EnqueueOutcome, HandoffError> {
        f.require_head_status.set(false);
        authorize_and_enqueue(MergeMode::Queue, true, f, s, &r.forge, &|| f.read_facts(), PR, SHA1)
    }

    #[test]
    fn handoff_succeeds_through_the_command_with_the_pr_and_no_gh_status_call() {
        let (f, s, r) = (FakeForge::new(), MemoryGrantStore::default(), Route::new(0));
        // The writer would be refused (`statuses: read`); the command gate ignores it.
        f.cannot_post.set(true);
        assert!(matches!(handoff_via(&f, &s, &r), Ok(EnqueueOutcome::Enqueued { .. })));
        let calls = r.calls();
        assert_eq!(calls.len(), 2, "dry-run probe, then the head success: {calls:?}");
        assert!(calls[0].contains("--dry-run"));
        assert!(calls[1].contains(&format!("--sha {SHA1}")));
        assert!(calls[1].contains("--state success"));
        assert!(calls[1].contains("--context loom/merge-authorization"));
        assert!(calls[1].contains("--repo o/r"));
        assert!(calls[1].contains(&format!("--pr {PR}")));
        assert!(calls[1].contains("--idempotency-key"));
        assert!(f.head_status.borrow().is_empty(), "the fake writer was never used");
    }

    #[test]
    fn rollback_after_a_failed_enqueue_posts_pending_through_the_command() {
        let (f, s, r) = (FakeForge::new(), MemoryGrantStore::default(), Route::new(0));
        f.enqueue_fails.set(true);
        assert!(handoff_via(&f, &s, &r).is_err());
        let calls = r.calls();
        assert!(calls.last().unwrap().contains("--state pending"), "{calls:?}");
        assert!(calls.last().unwrap().contains(&format!("--pr {PR}")));
    }

    #[test]
    fn revocation_after_enqueue_posts_failure_through_the_command() {
        let (f, s, r) = (FakeForge::new(), MemoryGrantStore::default(), Route::new(0));
        f.revoke_on_enqueue.set(true);
        assert!(matches!(
            handoff_via(&f, &s, &r),
            Err(HandoffError::RevokedDuringEnqueue { .. })
        ));
        assert!(r.calls().last().unwrap().contains("--state failure"));
    }

    #[test]
    fn failing_command_refuses_the_gate_before_any_grant() {
        let (f, s, r) = (FakeForge::new(), MemoryGrantStore::default(), Route::new(1));
        let e = handoff_via(&f, &s, &r).unwrap_err();
        assert!(matches!(e, HandoffError::WriterCannotPostStatuses(_)), "{e:?}");
        assert_eq!(s.get(PR).unwrap(), None, "no grant written");
        assert!(f.queued.borrow().is_none(), "nothing enqueued");
    }

    #[test]
    fn head_and_group_revoke_post_failure_through_the_command() {
        use crate::forge_merge_queue::group_authz::{Member, MergeGroup};
        let (f, s, r) = (FakeForge::new(), MemoryGrantStore::default(), Route::new(0));
        let group = MergeGroup {
            commit: "b".repeat(40),
            members: vec![Member {
                pr: PR,
                head: SHA1.into(),
            }],
        };
        let out = revoke_refail_dequeue(
            MergeMode::Queue,
            true,
            &f,
            &s,
            &r.forge,
            &|| Ok(vec![group.clone()]),
            &|| Ok(SHA1.to_string()),
            PR,
        );
        assert!(out.refailed.is_ok() && out.head_failed.is_ok());
        let calls = r.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls.iter().all(|c| c.contains("--state failure")));
        let group_call = calls.iter().find(|c| c.contains(&"b".repeat(40))).unwrap();
        assert!(!group_call.contains("--pr"), "a group commit names no single PR");
        let head_call = calls.iter().find(|c| c.contains(SHA1)).unwrap();
        assert!(head_call.contains(&format!("--pr {PR}")));
    }

    #[test]
    fn malformed_setting_fails_closed_instead_of_falling_back_to_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"forge":{"mergeQueue":{"statusCommand":"post-it"}}}"#,
        )
        .unwrap();
        let forge = GhLifecycleForge::new("/nonexistent/gh", dir.path(), "o/r").unwrap();
        assert!(forge.can_post_statuses().is_err());
        assert!(forge.post_status(SHA1, StatusState::Success, "x").is_err());
    }
}
