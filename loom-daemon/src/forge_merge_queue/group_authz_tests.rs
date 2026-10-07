//! Fake-GitHub protocol tests for merge-group authorization (#10256, B4).
//!
//! The fake models the merge-queue behaviour these properties depend on:
//!
//! - the group built for the entry at position *k* contains entries `1..=k`;
//! - commit statuses are latest-wins per (commit, context);
//! - GitHub merges the longest prefix whose top group has **every** required
//!   context (`ci` and `loom/merge-authorization`) at success;
//! - a dequeue rebuilds every group (new commits, no statuses).
//!
//! Every assertion is on whether a PR **can merge** in that model, not on
//! which calls were made. The model is an assumption about GitHub; Phase C's
//! live pilot (#10257) is what qualifies it.

#![allow(clippy::unwrap_used)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use super::authz::*;
use super::group_authz::*;
use super::mode::MergeMode;
use super::ops::*;

const HA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HC: &str = "cccccccccccccccccccccccccccccccccccccccc";
const A: u32 = 1;
const B: u32 = 2;
const CI: &str = "ci";

#[derive(Default)]
struct FakeQueue {
    entries: RefCell<Vec<Member>>,
    generation: Cell<u32>,
    statuses: RefCell<HashMap<(String, String), CheckState>>,
    merged: RefCell<Vec<u32>>,
    facts: RefCell<HashMap<u32, Result<AuthzFacts, String>>>,
    dequeue_fails: Cell<bool>,
    status_down: Cell<bool>,
    groups_down: Cell<bool>,
}

impl FakeQueue {
    /// Queue `prs` in order, each granted and approved at its head.
    fn with(prs: &[(u32, &str)], store: &MemoryGrantStore) -> Self {
        let q = Self::default();
        for (pr, head) in prs {
            q.entries.borrow_mut().push(Member {
                pr: *pr,
                head: (*head).into(),
            });
            q.facts
                .borrow_mut()
                .insert(*pr, Ok(AuthzFacts::approved(head)));
            store
                .put(Grant {
                    pr: *pr,
                    approved_sha: (*head).into(),
                })
                .unwrap();
        }
        q
    }
    fn groups(&self) -> Vec<MergeGroup> {
        let e = self.entries.borrow();
        (0..e.len())
            .map(|k| MergeGroup {
                commit: format!("g{}-{k}-pr{}", self.generation.get(), e[k].pr),
                members: e[..=k].to_vec(),
            })
            .collect()
    }
    fn group_of(&self, pr: u32) -> MergeGroup {
        self.groups()
            .into_iter()
            .find(|g| g.members.last().map(|m| m.pr) == Some(pr))
            .unwrap()
    }
    fn edit(&self, pr: u32, f: impl FnOnce(&mut AuthzFacts)) {
        if let Some(Ok(x)) = self.facts.borrow_mut().get_mut(&pr) {
            f(x);
        }
    }
    fn facts_for(&self, pr: u32) -> Result<AuthzFacts, String> {
        self.facts
            .borrow()
            .get(&pr)
            .cloned()
            .unwrap_or_else(|| Err("unknown PR".into()))
    }
    fn state(&self, commit: &str, ctx: &str) -> CheckState {
        self.statuses
            .borrow()
            .get(&(commit.to_string(), ctx.to_string()))
            .copied()
            .unwrap_or(CheckState::Pending)
    }
    /// The other required CI finishes on `pr`'s group commit.
    fn ci(&self, pr: u32, s: CheckState) {
        let c = self.group_of(pr).commit;
        self.statuses.borrow_mut().insert((c, CI.into()), s);
    }
    /// Loom's check (workflow or daemon) runs on `pr`'s group and posts.
    fn run_authz(&self, store: &dyn GrantStore, pr: u32) -> GroupConclusion {
        let g = self.group_of(pr);
        let others = Ok(vec![(CI.to_string(), self.state(&g.commit, CI))]);
        let c = group_check(store, &g, others, &|p| self.facts_for(p));
        let (st, _) = status_for(&c);
        let st = match st {
            StatusState::Success => CheckState::Success,
            StatusState::Pending => CheckState::Pending,
            StatusState::Failure => CheckState::Failure,
        };
        self.statuses
            .borrow_mut()
            .insert((g.commit, REQUIRED_CHECK_CONTEXT.into()), st);
        c
    }
    /// GitHub: merge the longest prefix whose top group is fully green.
    fn try_merge(&self) -> Vec<u32> {
        let groups = self.groups();
        let top = groups.iter().rposition(|g| {
            self.state(&g.commit, CI) == CheckState::Success
                && self.state(&g.commit, REQUIRED_CHECK_CONTEXT) == CheckState::Success
        });
        let Some(k) = top else {
            return Vec::new();
        };
        let done: Vec<u32> = self
            .entries
            .borrow_mut()
            .drain(..=k)
            .map(|m| m.pr)
            .collect();
        self.merged.borrow_mut().extend(&done);
        self.generation.set(self.generation.get() + 1);
        done
    }
    fn merged(&self, pr: u32) -> bool {
        self.merged.borrow().contains(&pr)
    }
}

impl QueueApi for FakeQueue {
    fn status(&self, pr: u32) -> Result<PrQueueStatus, QueueError> {
        let entry = self.entries.borrow().iter().find(|m| m.pr == pr).cloned();
        Ok(PrQueueStatus {
            number: pr,
            node_id: format!("PR_{pr}"),
            state: if self.merged(pr) {
                PrState::Merged
            } else {
                PrState::Open
            },
            head_oid: entry.as_ref().map_or_else(String::new, |m| m.head.clone()),
            entry: entry.map(|m| QueueEntry {
                state: "AWAITING_CHECKS".into(),
                position: None,
                head_oid: Some(m.head),
            }),
        })
    }
    fn enqueue(&self, _pr: u32, _n: &str, _h: &str) -> Result<EnqueueAck, QueueError> {
        unreachable!("these tests start with a populated queue")
    }
    fn dequeue(&self, pr: u32, _n: &str) -> Result<DequeueAck, QueueError> {
        if self.dequeue_fails.get() {
            return Err(QueueError::Forge {
                detail: "502".into(),
            });
        }
        self.entries.borrow_mut().retain(|m| m.pr != pr);
        self.generation.set(self.generation.get() + 1);
        Ok(DequeueAck::Dequeued)
    }
}

impl StatusApi for FakeQueue {
    fn post_status(&self, commit: &str, s: StatusState, _d: &str) -> Result<(), String> {
        if self.status_down.get() {
            return Err("statuses api 503".into());
        }
        let s = match s {
            StatusState::Success => CheckState::Success,
            StatusState::Pending => CheckState::Pending,
            StatusState::Failure => CheckState::Failure,
        };
        self.statuses
            .borrow_mut()
            .insert((commit.into(), REQUIRED_CHECK_CONTEXT.into()), s);
        Ok(())
    }
}

fn revoke(q: &FakeQueue, s: &MemoryGrantStore, pr: u32) -> GroupRevocation {
    let groups = || {
        if q.groups_down.get() {
            Err("refs api 503".to_string())
        } else {
            Ok(q.groups())
        }
    };
    revoke_refail_dequeue(MergeMode::Queue, true, q, s, q, &groups, pr)
}

fn hold(q: &FakeQueue, pr: u32) {
    q.edit(pr, |f| f.human_hold = Fact::Known(true));
}

#[test]
fn authorized_group_merges_only_after_ci_then_authz() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    assert!(matches!(q.run_authz(&s, A), GroupConclusion::Pending(_)));
    assert!(q.try_merge().is_empty(), "pending authz blocks");
    q.ci(A, CheckState::Success);
    assert!(q.try_merge().is_empty(), "CI alone is not enough");
    assert_eq!(q.run_authz(&s, A), GroupConclusion::Success);
    assert_eq!(q.try_merge(), vec![A]);
}

#[test]
fn authz_never_concludes_before_other_required_checks() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    for st in [CheckState::Pending, CheckState::Failure] {
        q.ci(A, st);
        assert!(matches!(q.run_authz(&s, A), GroupConclusion::Pending(_)), "{st:?}");
    }
    let g = q.group_of(A);
    let c = group_check(&s, &g, Err("checks api 502".into()), &|p| q.facts_for(p));
    assert!(matches!(c, GroupConclusion::Pending(_)), "unreadable others never pass");
}

/// Hold added while CI is still running, daemon down, nobody revokes: the
/// final evaluation happens after CI and sees the hold.
#[test]
fn hold_during_ci_with_daemon_down_blocks_merge() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.run_authz(&s, A); // early run: pending
    hold(&q, A); // a human acts; Loom never hears about it
    q.ci(A, CheckState::Success);
    assert!(matches!(q.run_authz(&s, A), GroupConclusion::Failure(_)));
    assert!(q.try_merge().is_empty());
    assert!(!q.merged(A));
}

#[test]
fn check_runner_outage_leaves_entry_pending_not_merged() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.run_authz(&s, A); // pending; then the runner/daemon is gone
    q.ci(A, CheckState::Success);
    assert!(q.try_merge().is_empty(), "GitHub waits (then times out); never merges");
}

/// The grouped-merge hole: B's group carries A. A single-PR check on B
/// passes; the group check evaluates A too.
#[test]
fn revoked_pr_cannot_ride_a_later_group() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA), (B, HB)], &s);
    // Isolate the group check: neither the dequeue nor the re-fail lands.
    q.dequeue_fails.set(true);
    q.status_down.set(true);
    hold(&q, A);
    let r = revoke(&q, &s, A);
    assert!(r.revocation.dequeue.is_err(), "A is still queued, inside B's group");
    assert!(!r.passed_checks_withdrawn());
    q.status_down.set(false);
    // The B1 single-PR body would authorize B's group:
    let single = merge_group_check(&s, B, HB, q.facts_for(B));
    assert!(single.is_success(), "pins the hole the group check closes");
    q.ci(A, CheckState::Success);
    q.ci(B, CheckState::Success);
    let c = q.run_authz(&s, B);
    assert!(
        matches!(&c, GroupConclusion::Failure(w) if w.iter().all(|(pr, _)| *pr == A)),
        "{c:?}"
    );
    assert!(q.try_merge().is_empty());
    assert!(!q.merged(A) && !q.merged(B));
}

/// GitHub holds a fully green group (minimum group size / wait time). A
/// revocation in that interval re-fails it even though the dequeue fails.
#[test]
fn revocation_after_pass_refails_the_green_group() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.ci(A, CheckState::Success);
    assert!(q.run_authz(&s, A).is_success());
    q.dequeue_fails.set(true);
    hold(&q, A);
    let r = revoke(&q, &s, A);
    assert!(r.passed_checks_withdrawn() && r.safe_to_transition(), "{r}");
    assert!(q.try_merge().is_empty());
    assert!(!q.merged(A));
}

#[test]
fn revocation_refails_every_group_that_contains_the_pr() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA), (B, HB), (3, HC)], &s);
    for pr in [A, B, 3] {
        q.ci(pr, CheckState::Success);
        assert!(q.run_authz(&s, pr).is_success());
    }
    q.dequeue_fails.set(true);
    let r = revoke(&q, &s, B);
    assert_eq!(r.refailed.as_ref().unwrap().len(), 2, "B's group and #3's");
    // A's own group does not contain B and may still merge on its own.
    assert_eq!(q.try_merge(), vec![A]);
    assert!(!q.merged(B) && !q.merged(3));
}

#[test]
fn successful_dequeue_rebuilds_groups_without_the_pr() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA), (B, HB)], &s);
    hold(&q, A);
    let r = revoke(&q, &s, A);
    assert_eq!(r.revocation.dequeue, Ok(DequeueOutcome::Dequeued));
    q.ci(B, CheckState::Success);
    assert!(q.run_authz(&s, B).is_success(), "B is unaffected once A is out");
    assert_eq!(q.try_merge(), vec![B]);
    assert!(!q.merged(A));
}

#[test]
fn duplicate_revocation_is_idempotent_and_still_blocks() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.ci(A, CheckState::Success);
    q.run_authz(&s, A);
    q.dequeue_fails.set(true);
    let a = revoke(&q, &s, A);
    let b = revoke(&q, &s, A);
    assert!(a.passed_checks_withdrawn() && b.passed_checks_withdrawn());
    assert!(q.try_merge().is_empty());
    // A re-run of the check after the revocation re-confirms the failure.
    assert!(matches!(q.run_authz(&s, A), GroupConclusion::Failure(_)));
    assert!(q.try_merge().is_empty());
}

#[test]
fn grant_store_outage_fails_the_group() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.ci(A, CheckState::Success);
    s.set_down(true);
    assert!(matches!(q.run_authz(&s, A), GroupConclusion::Failure(_)));
    assert!(q.try_merge().is_empty());
}

#[test]
fn member_head_other_than_grant_fails_the_group() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    s.put(Grant {
        pr: A,
        approved_sha: HB.into(),
    })
    .unwrap();
    q.ci(A, CheckState::Success);
    assert!(matches!(q.run_authz(&s, A), GroupConclusion::Failure(_)));
}

#[test]
fn empty_membership_fails_closed() {
    let s = MemoryGrantStore::default();
    let g = MergeGroup {
        commit: "g".into(),
        members: Vec::new(),
    };
    let c = group_check(&s, &g, Ok(Vec::new()), &|_| Err("x".into()));
    assert!(matches!(c, GroupConclusion::Failure(_)));
}

#[test]
fn refail_works_in_a_dormant_build() {
    // Withdrawing authority needs no execution gate; the dequeue is refused.
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.ci(A, CheckState::Success);
    q.run_authz(&s, A);
    let groups = || Ok(q.groups());
    let r = revoke_refail_dequeue(MergeMode::Queue, false, &q, &s, &q, &groups, A);
    assert_eq!(r.revocation.dequeue, Err(QueueError::ExecutionDormant));
    assert!(r.passed_checks_withdrawn());
    assert!(q.try_merge().is_empty());
}

#[test]
fn queue_ref_parsing() {
    let sha = HA;
    assert_eq!(
        parse_queue_ref(&format!("refs/heads/gh-readonly-queue/main/pr-42-{sha}")),
        Some((42, sha.into()))
    );
    assert_eq!(
        parse_queue_ref(&format!("gh-readonly-queue/release/v1/pr-7-{}", sha.to_uppercase())),
        Some((7, sha.into()))
    );
    for bad in [
        "main",
        "gh-readonly-queue/main/pr-x-aaaa",
        "gh-readonly-queue/main/pr-42-abc",
        "feature/gh-readonly-queue/main/pr-1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert_eq!(parse_queue_ref(bad), None, "{bad}");
    }
    let queue = [
        Member {
            pr: A,
            head: HA.into(),
        },
        Member {
            pr: B,
            head: HB.into(),
        },
    ];
    assert_eq!(members_for(B, &queue).unwrap().len(), 2);
    assert_eq!(members_for(A, &queue).unwrap().len(), 1);
    assert_eq!(members_for(9, &queue), None);
}

/// Residual window 1: the status API is unreachable when the revocation
/// lands after the final evaluation passed, and the dequeue fails too.
/// Nothing Loom controls can withdraw the pass; it is reported, not hidden.
#[test]
fn known_gap_refail_unreachable_after_final_pass() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.ci(A, CheckState::Success);
    assert!(q.run_authz(&s, A).is_success());
    q.status_down.set(true);
    q.dequeue_fails.set(true);
    let r = revoke(&q, &s, A);
    assert!(!r.passed_checks_withdrawn(), "reported: {r}");
    assert_eq!(q.try_merge(), vec![A], "documented gap");
    const { assert!(!INVARIANT_FULLY_DEMONSTRATED) };
}

/// Residual window 2: GitHub's own decide-then-merge interval. Once every
/// required context is green GitHub may commit before any re-fail arrives.
#[test]
fn known_gap_github_merges_before_the_refail_arrives() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.ci(A, CheckState::Success);
    q.run_authz(&s, A);
    assert_eq!(q.try_merge(), vec![A]); // GitHub decided first
    let r = revoke(&q, &s, A);
    assert!(r.already_merged(), "reported as already merged, never as revoked-in-time");
}

#[test]
fn group_discovery_outage_is_reported() {
    let s = MemoryGrantStore::default();
    let q = FakeQueue::with(&[(A, HA)], &s);
    q.groups_down.set(true);
    let r = revoke(&q, &s, A);
    assert!(!r.passed_checks_withdrawn());
    assert!(r.safe_to_transition(), "the grant is revoked; future evaluations deny");
}
