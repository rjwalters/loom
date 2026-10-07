//! State-machine tests for the queue lifecycle (#10256, Phase B2).
//!
//! The fake forge models what matters about GitHub: a queued entry merges
//! only if the required `loom/merge-authorization` check — evaluated by
//! [`authorize_check`] against the forge's *current* state — succeeds at
//! that moment. Assertions are on whether the PR **merged**, not on whether a
//! dequeue was attempted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::{Cell, RefCell};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::authz::{CheckConclusion, DenyReason, HandoffError, REQUIRED_CHECK_CONTEXT};
use super::events::{EventKind, EventSink, FileEventSink, MemoryEventSink, QueueEvent};
use super::forge::{LifecycleForge, PrSnapshot, RemovalEvent};
use super::grants::{latest_record, GrantRecord};
use super::lifecycle::*;
use super::lifecycle_cli::{render_handoff, render_reconciled, EXIT_QUEUED};
use super::mode::MergeMode;
use super::ops::*;
use super::removal::{classify, RemovalKind};

const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA2: &str = "fedcba9876543210fedcba9876543210fedcba98";
const PR: u32 = 7;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 6, 10, 0, 0).unwrap()
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn verdict(sha: &str) -> String {
    format!("Approved.\n<!-- loom:verdict-sha sha={sha} verdict=approved -->")
}

struct Gh {
    head: RefCell<String>,
    state: Cell<PrState>,
    labels: RefCell<Vec<String>>,
    comments: RefCell<Vec<String>>,
    queued: RefCell<Option<String>>,
    check: RefCell<Option<CheckConclusion>>,
    removals: RefCell<Vec<RemovalEvent>>,
    comments_down: Cell<bool>,
    dequeue_fails: Cell<bool>,
    /// Pushed to the head the instant the enqueue mutation arrives.
    push_during_enqueue: RefCell<Option<String>>,
    calls: Cell<u32>,
}

impl Gh {
    fn new() -> Self {
        Self {
            head: RefCell::new(SHA1.into()),
            state: Cell::new(PrState::Open),
            labels: RefCell::new(vec!["loom:pr".into()]),
            comments: RefCell::new(vec![verdict(SHA1)]),
            queued: RefCell::new(None),
            check: RefCell::new(None),
            removals: RefCell::new(Vec::new()),
            comments_down: Cell::new(false),
            dequeue_fails: Cell::new(false),
            push_during_enqueue: RefCell::new(None),
            calls: Cell::new(0),
        }
    }
    fn tick(&self) {
        self.calls.set(self.calls.get() + 1);
    }
    fn add_label(&self, l: &str) {
        self.labels.borrow_mut().push(l.into());
    }
    fn drop_label(&self, l: &str) {
        self.labels.borrow_mut().retain(|x| x != l);
    }
    fn has_label(&self, l: &str) -> bool {
        self.labels.borrow().iter().any(|x| x == l)
    }
    /// GitHub evaluates the required check on the merge group.
    fn run_check(&self) {
        let h = self.queued.borrow().clone();
        if let Some(h) = h {
            *self.check.borrow_mut() = Some(authorize_check(self, PR, &h, t0()));
        }
    }
    /// GitHub merges an entry only on a passing required check.
    fn try_merge(&self) -> bool {
        let ok = self.queued.borrow().is_some()
            && matches!(&*self.check.borrow(), Some(CheckConclusion::Success));
        if ok {
            self.state.set(PrState::Merged);
            *self.queued.borrow_mut() = None;
        }
        ok
    }
    /// Check then merge, as GitHub does once the other checks are green.
    fn github_merge(&self) -> bool {
        self.run_check();
        self.try_merge()
    }
    fn github_drops(&self, reason: Option<&str>, at: DateTime<Utc>) {
        *self.queued.borrow_mut() = None;
        *self.check.borrow_mut() = None;
        self.removals.borrow_mut().push(RemovalEvent {
            reason: reason.map(str::to_string),
            created_at: ts(at),
        });
    }
    fn count(&self, needle: &str) -> usize {
        self.comments
            .borrow()
            .iter()
            .filter(|c| c.contains(needle))
            .count()
    }
}

impl QueueApi for Gh {
    fn status(&self, pr: u32) -> Result<PrQueueStatus, QueueError> {
        self.tick();
        Ok(PrQueueStatus {
            number: pr,
            node_id: "PR_node".into(),
            state: self.state.get(),
            head_oid: self.head.borrow().clone(),
            entry: self.queued.borrow().clone().map(|h| QueueEntry {
                state: "AWAITING_CHECKS".into(),
                position: Some(1),
                head_oid: Some(h),
            }),
        })
    }
    fn enqueue(&self, _pr: u32, _n: &str, expected: &str) -> Result<EnqueueAck, QueueError> {
        self.tick();
        if let Some(new) = self.push_during_enqueue.borrow_mut().take() {
            *self.head.borrow_mut() = new;
        }
        if !self.head.borrow().eq_ignore_ascii_case(expected) {
            return Err(QueueError::HeadMismatch {
                pr: PR,
                approved: expected.into(),
                actual: None,
            });
        }
        *self.queued.borrow_mut() = Some(expected.into());
        Ok(EnqueueAck::Enqueued { position: Some(1) })
    }
    fn dequeue(&self, _pr: u32, _n: &str) -> Result<DequeueAck, QueueError> {
        self.tick();
        if self.dequeue_fails.get() {
            return Err(QueueError::Forge {
                detail: "502".into(),
            });
        }
        *self.queued.borrow_mut() = None;
        Ok(DequeueAck::Dequeued)
    }
}

impl LifecycleForge for Gh {
    fn snapshot(&self, pr: u32) -> Result<PrSnapshot, String> {
        self.tick();
        Ok(PrSnapshot {
            number: pr,
            state: self.state.get(),
            head_sha: self.head.borrow().clone(),
            labels: self.labels.borrow().clone(),
            merged_at: (self.state.get() == PrState::Merged)
                .then(|| ts(t0() + Duration::minutes(30))),
        })
    }
    fn trusted_comments(&self, _pr: u32) -> Result<Vec<String>, String> {
        self.tick();
        if self.comments_down.get() {
            return Err("HTTP 502".into());
        }
        Ok(self.comments.borrow().clone())
    }
    fn post_comment(&self, _pr: u32, body: &str) -> Result<(), String> {
        self.tick();
        if self.comments_down.get() {
            return Err("HTTP 502".into());
        }
        self.comments.borrow_mut().push(body.into());
        Ok(())
    }
    fn edit_labels(&self, _pr: u32, add: &[&str], remove: &[&str]) -> Result<(), String> {
        self.tick();
        for l in remove {
            self.drop_label(l);
        }
        for l in add {
            self.add_label(l);
        }
        Ok(())
    }
    fn removals(&self, _pr: u32) -> Result<Vec<RemovalEvent>, String> {
        self.tick();
        Ok(self.removals.borrow().clone())
    }
}

fn ctx<'a>(gh: &'a Gh, ev: &'a dyn EventSink, mode: MergeMode, at: DateTime<Utc>) -> Ctx<'a> {
    Ctx {
        mode,
        execution_enabled: true,
        queue: gh,
        forge: gh,
        events: ev,
        now: at,
    }
}

fn required_ok() -> Result<Vec<String>, String> {
    Ok(vec!["Daemon Checks".into(), REQUIRED_CHECK_CONTEXT.into()])
}

fn hand(gh: &Gh, ev: &dyn EventSink) -> Result<EnqueueOutcome, HandoffFailure> {
    handoff(&ctx(gh, ev, MergeMode::Queue, t0()), PR, SHA1, &required_ok)
}

fn kinds(ev: &MemoryEventSink) -> Vec<EventKind> {
    ev.all().iter().map(|e| e.kind).collect()
}

// ---------------------------------------------------------------- happy path

#[test]
fn enqueue_is_not_merge_and_merge_is_confirmed_once() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    assert!(matches!(hand(&gh, &ev), Ok(EnqueueOutcome::Enqueued { .. })));
    // Handed off: queued, NOT merged, only an `enqueued` record.
    assert_eq!(gh.state.get(), PrState::Open);
    assert_eq!(kinds(&ev), vec![EventKind::Enqueued]);
    assert_eq!(ev.pending().unwrap(), vec![PR]);
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(5));
    assert!(matches!(reconcile_pr(&c, PR), Reconciled::StillQueued { .. }));

    assert!(gh.github_merge());
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(40));
    assert_eq!(
        reconcile_pr(&c, PR),
        Reconciled::Merged {
            recorded: true,
            after_revocation: false
        }
    );
    let merged = ev
        .all()
        .into_iter()
        .find(|e| e.kind == EventKind::Merged)
        .unwrap();
    assert_eq!(merged.enqueue_to_event_secs, Some(30 * 60));
    assert!(ev.pending().unwrap().is_empty());
    // Duplicate event / second pass: no second record.
    assert_eq!(
        reconcile_pr(&c, PR),
        Reconciled::Merged {
            recorded: false,
            after_revocation: false
        }
    );
    assert_eq!(kinds(&ev), vec![EventKind::Enqueued, EventKind::Merged]);
}

#[test]
fn handoff_is_idempotent_for_a_queued_head() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    hand(&gh, &ev).unwrap();
    assert!(matches!(hand(&gh, &ev), Ok(EnqueueOutcome::AlreadyQueued { .. })));
    assert_eq!(gh.count("loom:merge-queue-grant"), 1, "no second grant");
    assert_eq!(kinds(&ev), vec![EventKind::Enqueued], "no duplicate telemetry");
}

// ------------------------------------------------------- direct mode / gates

#[test]
fn direct_mode_makes_no_forge_call_anywhere() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    let c = ctx(&gh, &ev, MergeMode::Direct, t0());
    assert_eq!(reconcile_pr(&c, PR), Reconciled::Direct);
    assert!(revoke_for_transition(&c, PR, "stale-verdict").is_none());
    assert!(sweep(&c).unwrap().is_empty());
    let called = Cell::new(false);
    let r = handoff(&c, PR, SHA1, &|| {
        called.set(true);
        required_ok()
    });
    assert_eq!(r, Err(HandoffFailure::Gate(QueueError::NotQueueMode)));
    assert_eq!(gh.calls.get(), 0);
    assert!(!called.get(), "no ruleset read in direct mode");
}

#[test]
fn dormant_build_refuses_before_any_call() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    let mut c = ctx(&gh, &ev, MergeMode::Queue, t0());
    c.execution_enabled = false;
    let r = handoff(&c, PR, SHA1, &required_ok);
    assert_eq!(r, Err(HandoffFailure::Gate(QueueError::ExecutionDormant)));
    assert_eq!(gh.calls.get(), 0);
    assert_eq!(render_handoff(PR, SHA1, &r).code, 4);
}

#[test]
fn refuses_when_the_ruleset_does_not_require_the_authorization_check() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    let c = ctx(&gh, &ev, MergeMode::Queue, t0());
    let r = handoff(&c, PR, SHA1, &|| Ok(vec!["Daemon Checks".into()]));
    assert!(matches!(r, Err(HandoffFailure::AuthzCheckNotRequired { .. })));
    assert!(gh.queued.borrow().is_none());
    assert_eq!(gh.count("loom:merge-queue-grant"), 0);
    let r = handoff(&c, PR, SHA1, &|| Err("rate limited".into()));
    assert!(matches!(r, Err(HandoffFailure::Preflight(_))));
    assert_eq!(render_handoff(PR, SHA1, &r).code, 3);
}

// --------------------------------------------------------------- revocation

#[test]
fn revocation_before_enqueue_writes_nothing_and_cannot_merge() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    gh.add_label("loom:reviewing");
    assert!(matches!(hand(&gh, &ev), Err(HandoffFailure::Authz(HandoffError::Denied(_)))));
    assert_eq!(gh.count("loom:merge-queue-grant"), 0);
    // Even a hand-enqueue by a human cannot pass the check without a grant.
    *gh.queued.borrow_mut() = Some(SHA1.into());
    assert!(!gh.github_merge());
}

#[test]
fn missing_verdict_marker_denies() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    gh.comments.borrow_mut().clear();
    assert!(matches!(hand(&gh, &ev), Err(HandoffFailure::Authz(HandoffError::Denied(_)))));
}

#[test]
fn head_race_during_enqueue_rolls_back_and_cannot_merge() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    *gh.push_during_enqueue.borrow_mut() = Some(SHA2.into());
    let r = hand(&gh, &ev);
    assert!(matches!(
        r,
        Err(HandoffFailure::Authz(HandoffError::Enqueue(QueueError::HeadMismatch { .. })))
    ));
    let rec = latest_record(&gh.comments.borrow(), PR);
    assert!(matches!(rec, GrantRecord::Revoked { .. }), "grant rolled back: {rec:?}");
    *gh.queued.borrow_mut() = Some(SHA2.into());
    assert!(!gh.github_merge(), "new head has no grant");
}

#[test]
fn external_hold_while_checks_complete_blocks_the_merge() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    hand(&gh, &ev).unwrap();
    // A human adds a hold; Loom is not running. Checks then complete.
    gh.add_label("loom:operator");
    assert!(!gh.github_merge());
    // Within one tick the daemon dequeues and records the revocation.
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(2));
    assert!(matches!(reconcile_pr(&c, PR), Reconciled::Continue(Some(_))));
    assert!(gh.queued.borrow().is_none());
    assert_eq!(kinds(&ev), vec![EventKind::Enqueued, EventKind::Removed]);
}

#[test]
fn each_external_change_blocks_without_loom() {
    type Change = fn(&Gh);
    let changes: [(&str, Change); 5] = [
        ("label removed", |g| g.drop_label("loom:pr")),
        ("review claim", |g| g.add_label("loom:reviewing")),
        ("contradiction", |g| g.add_label("loom:changes-requested")),
        ("hold", |g| g.add_label("loom:blocked")),
        ("head push", |g| *g.head.borrow_mut() = SHA2.into()),
    ];
    for (what, change) in changes {
        let (gh, ev) = (Gh::new(), MemoryEventSink::default());
        hand(&gh, &ev).unwrap();
        change(&gh);
        assert!(!gh.github_merge(), "{what} must block the merge");
    }
}

#[test]
fn loom_transition_revokes_before_and_survives_a_failed_dequeue() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    hand(&gh, &ev).unwrap();
    gh.dequeue_fails.set(true);
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(1));
    let rev = revoke_for_transition(&c, PR, "stale-verdict").unwrap();
    assert!(rev.dequeue.is_err());
    assert!(rev.safe_to_transition(), "grant revoke confirmed");
    assert!(transition_line(&rev).contains("dequeue not confirmed"));
    // Still queued (dequeue failed), and still cannot merge.
    assert!(gh.queued.borrow().is_some());
    assert!(!gh.github_merge());
}

#[test]
fn duplicate_revocation_events_post_and_record_once() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    hand(&gh, &ev).unwrap();
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(1));
    for _ in 0..3 {
        assert!(revoke_for_transition(&c, PR, "stale-verdict")
            .unwrap()
            .safe_to_transition());
    }
    assert_eq!(gh.count("loom:merge-queue-revoke"), 1);
    assert_eq!(kinds(&ev), vec![EventKind::Enqueued, EventKind::Removed]);
}

// ---------------------------------------------------------------- outages

#[test]
fn outage_blocks_the_merge_and_reconcile_fails_closed() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    hand(&gh, &ev).unwrap();
    gh.comments_down.set(true);
    assert!(!gh.github_merge(), "an unreadable grant store fails the check");
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(1));
    let r = reconcile_pr(&c, PR);
    assert!(matches!(r, Reconciled::Undetermined(_)));
    assert_eq!(render_reconciled(PR, &r).code, 3);
    // A Loom transition during the outage cannot confirm the revoke, but the
    // dequeue still lands, so the transition may proceed.
    let rev = revoke_for_transition(&c, PR, "stale-verdict").unwrap();
    assert!(rev.grant_revoked.is_err());
    assert!(rev.safe_to_transition());
    assert!(gh.queued.borrow().is_none());
}

#[test]
fn restart_neither_loses_nor_duplicates_state() {
    let dir = tempfile::tempdir().unwrap();
    let gh = Gh::new();
    {
        let ev = FileEventSink::for_root(dir.path());
        handoff(&ctx(&gh, &ev, MergeMode::Queue, t0()), PR, SHA1, &required_ok).unwrap();
    }
    // "Daemon down": GitHub merges in the meantime.
    assert!(gh.github_merge());
    {
        // A fresh sink = a restarted daemon; the grant lives on the forge.
        let ev = FileEventSink::for_root(dir.path());
        assert_eq!(ev.pending().unwrap(), vec![PR]);
        let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::hours(1));
        let rows = sweep(&c).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].1, Reconciled::Merged { .. }));
        assert!(ev.pending().unwrap().is_empty());
    }
    // A second restart sees the same event again: no duplicate record.
    let ev = FileEventSink::for_root(dir.path());
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::hours(2));
    assert_eq!(
        reconcile_pr(&c, PR),
        Reconciled::Merged {
            recorded: false,
            after_revocation: false
        }
    );
    let log =
        std::fs::read_to_string(dir.path().join(".loom/logs/merge-queue-events.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2, "{log}");
}

// ------------------------------------------------------------------ drops

fn drop_with(reason: Option<&str>) -> (Gh, MemoryEventSink, Reconciled) {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    hand(&gh, &ev).unwrap();
    gh.github_drops(reason, t0() + Duration::minutes(3));
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(4));
    let r = reconcile_pr(&c, PR);
    (gh, ev, r)
}

#[test]
fn conflict_drop_routes_to_doctor_with_the_verbatim_reason() {
    let (gh, ev, r) = drop_with(Some("Merge conflict with the base branch"));
    assert!(matches!(
        r,
        Reconciled::Dropped {
            kind: RemovalKind::Conflict,
            route_error: None,
            ..
        }
    ));
    assert_eq!(render_reconciled(PR, &r).code, EXIT_QUEUED);
    assert!(!gh.has_label("loom:pr") && gh.has_label("loom:changes-requested"));
    assert_eq!(gh.count("\"Merge conflict with the base branch\""), 1);
    let removed: Vec<QueueEvent> = ev
        .all()
        .into_iter()
        .filter(|e| e.kind == EventKind::Removed)
        .collect();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].reason.as_deref(), Some("conflict"));
    assert_eq!(removed[0].enqueue_to_event_secs, Some(4 * 60));
    // The next pass does not re-route or re-comment.
    gh.add_label("loom:pr");
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(9));
    assert_eq!(reconcile_pr(&c, PR), Reconciled::Continue(None));
    assert_eq!(gh.count("loom:merge-queue-revoke"), 1);
}

#[test]
fn check_failure_and_timeout_use_their_existing_routes() {
    let (gh, _, r) = drop_with(Some("Required status check \"Daemon Checks\" failed"));
    assert!(matches!(
        r,
        Reconciled::Dropped {
            kind: RemovalKind::CheckFailure,
            ..
        }
    ));
    assert!(gh.has_label("loom:changes-requested"));
    let (gh, _, r) = drop_with(Some("Timed out waiting for status checks"));
    assert!(matches!(
        r,
        Reconciled::Dropped {
            kind: RemovalKind::Timeout,
            ..
        }
    ));
    assert!(gh.has_label("loom:pr"), "timeout re-queues next pass");
    // Re-handoff after a timeout is a new generation.
    let ev = MemoryEventSink::default();
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(10));
    assert!(handoff(&c, PR, SHA1, &required_ok).is_ok());
    assert_eq!(gh.count("loom:merge-queue-grant"), 2);
}

#[test]
fn unknown_or_missing_reason_is_reported_unknown_not_guessed() {
    for reason in [
        None,
        Some(""),
        Some("The merge commit could not be created"),
    ] {
        let (gh, _, r) = drop_with(reason);
        assert!(
            matches!(
                r,
                Reconciled::Dropped {
                    kind: RemovalKind::Unknown,
                    ..
                }
            ),
            "{reason:?} → {r:?}"
        );
        assert!(gh.has_label("loom:operator"));
        assert!(gh.has_label("loom:pr"), "no Doctor route on a guess");
        assert_eq!(gh.count("Classified as **unknown**"), 1);
    }
}

#[test]
fn a_removal_older_than_the_grant_is_not_this_drop() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    gh.removals.borrow_mut().push(RemovalEvent {
        reason: Some("Merge conflict".into()),
        created_at: ts(t0() - Duration::hours(1)),
    });
    hand(&gh, &ev).unwrap();
    *gh.queued.borrow_mut() = None; // entry vanished with no new removal event
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(1));
    assert!(matches!(reconcile_pr(&c, PR), Reconciled::Continue(Some(_))));
    assert!(gh.has_label("loom:pr"), "an old conflict is not re-routed");
    assert!(matches!(
        latest_record(&gh.comments.borrow(), PR),
        GrantRecord::Revoked { ref reason, .. } if reason == "no-queue-entry"
    ));
}

// -------------------------------------------------------------- known gap

#[test]
fn known_gap_merge_after_revocation_is_flagged_in_telemetry() {
    let (gh, ev) = (Gh::new(), MemoryEventSink::default());
    hand(&gh, &ev).unwrap();
    gh.run_check(); // passes
    gh.dequeue_fails.set(true);
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(1));
    let _ = revoke_for_transition(&c, PR, "stale-verdict");
    assert!(gh.try_merge(), "pass-to-merge window: GitHub merges anyway");
    let c = ctx(&gh, &ev, MergeMode::Queue, t0() + Duration::minutes(2));
    assert!(matches!(
        reconcile_pr(&c, PR),
        Reconciled::Merged {
            after_revocation: true,
            ..
        }
    ));
    assert!(ev.all().iter().any(|e| e.merged_after_revocation));
    const { assert!(!super::authz::INVARIANT_FULLY_DEMONSTRATED) };
    const { assert!(!super::QUEUE_EXECUTION_ENABLED) };
}

// ------------------------------------------------------------ pure pieces

#[test]
fn classify_is_conservative() {
    assert_eq!(classify(Some("merge conflict")), RemovalKind::Conflict);
    assert_eq!(classify(Some("A required check failed")), RemovalKind::CheckFailure);
    assert_eq!(classify(Some("timed out")), RemovalKind::Timeout);
    assert_eq!(classify(Some("the head was updated")), RemovalKind::HeadChanged);
    assert_eq!(classify(Some("dequeued by octocat")), RemovalKind::Unknown);
    assert_eq!(classify(Some("   ")), RemovalKind::Unknown);
    assert_eq!(classify(None), RemovalKind::Unknown);
}

#[test]
fn markers_for_other_prs_are_ignored_and_newest_wins() {
    let b = vec![
        super::grants::grant_body(8, SHA1, 1, "2026-10-06T10:00:00Z"),
        super::grants::grant_body(PR, SHA1, 2, "2026-10-06T10:00:00Z"),
        super::grants::revoke_body(PR, 2, "stale-verdict", "2026-10-06T10:01:00Z", "x"),
        super::grants::grant_body(PR, SHA2, 3, "2026-10-06T10:02:00Z"),
    ];
    assert!(
        matches!(latest_record(&b, PR), GrantRecord::Live { ref sha, nonce: 3, .. } if sha == SHA2)
    );
    assert!(matches!(latest_record(&b[..3], PR), GrantRecord::Revoked { nonce: 2, .. }));
    assert_eq!(latest_record(&b[..1], PR), GrantRecord::None);
}

#[test]
fn file_sink_dedups_across_instances() {
    let dir = tempfile::tempdir().unwrap();
    let e = QueueEvent {
        kind: EventKind::Removed,
        pr: PR,
        nonce: 9,
        head: None,
        reason: Some("timeout".into()),
        raw_reason: None,
        enqueued_at: None,
        at: ts(t0()),
        enqueue_to_event_secs: None,
        merged_after_revocation: false,
    };
    assert!(FileEventSink::for_root(dir.path()).record(&e).unwrap());
    assert!(!FileEventSink::for_root(dir.path()).record(&e).unwrap());
}

#[test]
fn deny_reasons_name_the_check_failure() {
    let gh = Gh::new();
    match authorize_check(&gh, PR, SHA1, t0()) {
        CheckConclusion::Failure(why) => assert_eq!(why, vec![DenyReason::NoGrant]),
        CheckConclusion::Success => panic!("no grant must fail"),
    }
}

#[test]
fn redate_is_refused_in_queue_mode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    let cfg = dir.path().join(".loom/config.json");
    std::fs::write(&cfg, r#"{"champion":{"mergeMode":"direct"}}"#).unwrap();
    assert!(redate_permitted(dir.path()).is_ok());
    std::fs::write(&cfg, r#"{"champion":{"mergeMode":"queue"}}"#).unwrap();
    assert!(redate_permitted(dir.path())
        .unwrap_err()
        .contains("never pushes"));
}

#[test]
fn parses_github_shapes() {
    let pr = r#"{"state":"closed","merged":true,"merged_at":"2026-10-06T11:00:00Z",
        "head":{"sha":"abc"},"labels":[{"name":"loom:pr"}]}"#;
    let s = super::gh_lifecycle::parse_snapshot(PR, pr).unwrap();
    assert_eq!(s.state, PrState::Merged);
    assert_eq!(s.labels, vec!["loom:pr".to_string()]);
    let tl = serde_json::json!({"data":{"repository":{"pullRequest":{"timelineItems":{"nodes":[
        {"reason":"merge conflict","createdAt":"2026-10-06T10:00:00Z"},
        {"reason":null,"createdAt":"2026-10-06T10:05:00Z"}]}}}}});
    let r = super::gh_lifecycle::parse_removals(&tl).unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[1].reason, None);
}

fn root_with_mode(mode: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        format!(r#"{{"champion":{{"mergeMode":"{mode}"}}}}"#),
    )
    .unwrap();
    dir
}

#[test]
fn revoke_for_root_is_silent_only_in_confirmed_direct_mode() {
    use super::gh_lifecycle::revoke_for_root;
    let gh = std::path::Path::new("/nonexistent/gh");
    let direct = root_with_mode("direct");
    assert!(revoke_for_root(gh, direct.path(), PR, "stale-verdict").is_none());
}

#[test]
fn revoke_for_root_reports_unresolved_state_instead_of_looking_direct() {
    use super::gh_lifecycle::revoke_for_root;
    let gh = std::path::Path::new("/nonexistent/gh");
    // Queue mode, but the repository cannot be resolved (no usable gh).
    let queue = root_with_mode("queue");
    let line = revoke_for_root(gh, queue.path(), PR, "stale-verdict").expect("must not be silent");
    assert!(line.contains("NOT confirmed"), "{line}");
    // An unreadable mode is unknown, not direct.
    let bad = root_with_mode("bogus");
    let line = revoke_for_root(gh, bad.path(), PR, "stale-verdict").expect("must not be silent");
    assert!(line.contains("NOT confirmed") && line.contains("merge mode"), "{line}");
}

struct FailingSink;
impl EventSink for FailingSink {
    fn record(&self, _ev: &QueueEvent) -> Result<bool, String> {
        Err("disk full".into())
    }
    fn pending(&self) -> Result<Vec<u32>, String> {
        Ok(Vec::new())
    }
}

#[test]
fn handoff_fails_closed_when_the_enqueued_record_cannot_be_written() {
    let gh = Gh::new();
    let r = hand(&gh, &FailingSink);
    match &r {
        Err(HandoffFailure::Telemetry { error, revoked }) => {
            assert!(error.contains("disk full"));
            assert!(revoked.safe_to_transition(), "grant must be revoked: {revoked:?}");
        }
        other => panic!("expected Telemetry failure, got {other:?}"),
    }
    assert!(r
        .as_ref()
        .unwrap_err()
        .to_string()
        .contains("TELEMETRY_UNRECORDED"));
    // The grant is revoked, so the live check denies a merge.
    assert!(!gh.github_merge(), "an unrecorded handoff must not be mergeable");
}

fn step_env(root: &std::path::Path, mode: MergeMode, gh: &str) -> super::Env {
    super::Env {
        forge: crate::forge_cmd::ForgeType::GitHub,
        gh: gh.to_string(),
        default_repo: None,
        mode: Ok(super::mode::ResolvedMergeMode {
            mode,
            source: super::mode::MergeModeSource::Default,
        }),
        execution_enabled: false,
        root: root.to_path_buf(),
    }
}

fn step_cmd() -> super::MergeQueueCmd {
    super::MergeQueueCmd::Step {
        pr: PR,
        approved_sha: SHA1.to_string(),
        repo: None,
    }
}

#[test]
fn step_in_direct_mode_prints_direct_and_touches_nothing() {
    let dir = root_with_mode("direct");
    let env = step_env(dir.path(), MergeMode::Direct, "/nonexistent/gh");
    let r = super::run(&step_cmd(), &env);
    assert_eq!(r.code, 0);
    assert_eq!(r.stdout, vec!["LOOM-MERGE-QUEUE-DIRECT".to_string()]);
}

#[test]
fn step_in_queue_mode_never_looks_direct_when_it_cannot_read_the_forge() {
    // The Champion merges directly only on the DIRECT sentinel; an unreadable
    // forge in queue mode must produce anything else.
    let dir = root_with_mode("queue");
    let env = step_env(dir.path(), MergeMode::Queue, "/nonexistent/gh");
    let r = super::run(&step_cmd(), &env);
    assert_ne!(r.code, 0, "{r:?}");
    assert!(
        r.stdout
            .first()
            .is_none_or(|l| !l.starts_with("LOOM-MERGE-QUEUE-DIRECT")),
        "{r:?}"
    );
}
