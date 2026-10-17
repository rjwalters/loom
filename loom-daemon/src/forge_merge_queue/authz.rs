//! Fail-closed merge-queue authorization protocol (#10256, Phase B1 of #9978).
//!
//! GitHub merges a queued PR on its own schedule, independent of Loom's
//! labels. Checking approval once and polling for revocation on a later tick
//! cannot satisfy "revoked before it can merge". This module defines the
//! protocol that makes revocation enforceable on the forge side, and models it
//! against the [`QueueApi`] seam so it can be exercised with a fake forge.
//!
//! # The protocol
//!
//! 1. **Authority is a grant, not a label.** A [`Grant`] binds one PR to one
//!    approved head SHA. It lives in a [`GrantStore`]. No grant means no
//!    authority; the default state is deny.
//! 2. **The required check is the enforcement point.** Queue mode requires a
//!    `merge_group` status check ([`REQUIRED_CHECK_CONTEXT`]). Its body is
//!    [`merge_group_check`]. It reports success only if a grant exists for the
//!    PR's *current* head **and** the *live* facts ([`AuthzFacts`]) still pass
//!    [`evaluate`]. Anything undeterminable (store outage, forge error,
//!    missing fact) is a failure ([`CheckConclusion::Failure`]). A required
//!    check that fails, errors, or never reports blocks the merge, so an
//!    outage of the daemon, the store, or the forge blocks merges rather than
//!    allowing them.
//! 3. **Externally initiated changes need no cooperation from Loom.** A human
//!    removing the approval label, adding a hold, or pushing a new head
//!    changes the live facts; the check reads the facts each time it runs.
//! 4. **Loom-owned transitions revoke first.** [`revoke_then_dequeue`] deletes
//!    the grant before it asks the forge to dequeue, and reports whether the
//!    transition (verdict invalidation, reviewing claim, `loom:operator`) may
//!    proceed ([`Revocation::safe_to_transition`]). A failed dequeue after a
//!    confirmed revoke still leaves the PR unable to pass the check.
//! 5. **Enqueue re-validates after the write.** [`authorize_and_enqueue`]
//!    evaluates, grants, posts `success` for [`REQUIRED_CHECK_CONTEXT`] on
//!    the approved head (GitHub only enqueues a PR whose required checks are
//!    satisfied on its head, #12496), enqueues pinned to that head, then
//!    evaluates again; a revocation that raced the enqueue is rolled back
//!    immediately rather than at the next tick.
//!
//! # What this does and does not prove
//!
//! The protocol closes every revocation that *completes before the check
//! evaluates*, including outages and a failed dequeue. It cannot close the
//! window between the check passing on the merge-group commit and GitHub
//! performing the merge: GitHub offers no atomic "evaluate then merge" hook,
//! and no Loom-side step can sit inside that interval. The test
//! `known_gap_revocation_after_check_pass_can_still_merge` pins the gap so it
//! cannot be forgotten. Because of it, [`INVARIANT_FULLY_DEMONSTRATED`] is
//! `false`; the operator accepted the gap for the Phase C pilot (#10257), which
//! enabled queue execution ([`super::QUEUE_EXECUTION_ENABLED`]).
//!
//! [`merge_group_check`] covers one PR; a merge group can carry several.
//! [`super::group_authz`] evaluates every member, concludes only after the
//! other required checks, and re-fails passed groups on revocation, which
//! narrows (but does not close) the window above.

use std::collections::BTreeMap;
use std::fmt;

use super::group_authz::{status_for, GroupConclusion, StatusApi, StatusState};
use super::mode::MergeMode;
use super::ops::{self, DequeueOutcome, EnqueueOutcome, QueueApi, QueueError};

/// The status-check context a repository must require on its merge queue.
pub const REQUIRED_CHECK_CONTEXT: &str = "loom/merge-authorization";

/// `true` only when the revocation-before-merge invariant holds without a
/// residual window. It does not (see the module docs), so Phase B must not
/// enable queue execution.
pub const INVARIANT_FULLY_DEMONSTRATED: bool = false;

/// A fact observed from the forge or Loom state. `Unknown` is never treated
/// as permissive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact<T> {
    Known(T),
    Unknown(String),
}

impl<T> Fact<T> {
    fn known(&self) -> Option<&T> {
        match self {
            Fact::Known(v) => Some(v),
            Fact::Unknown(_) => None,
        }
    }
}

/// Live state the authorization decision is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthzFacts {
    /// The PR's current head commit.
    pub head_sha: Fact<String>,
    /// The Judge's approval label (`loom:pr`) is present.
    pub approved_label: Fact<bool>,
    /// The verdict marker's SHA still equals the current head.
    pub verdict_current: Fact<bool>,
    /// A Judge or Doctor currently holds a reviewing/treating claim.
    pub reviewing_claim: Fact<bool>,
    /// A hold present (`loom:operator` / `loom:blocked` / `loom:frozen`, or a
    /// retired name for one, #11795).
    pub human_hold: Fact<bool>,
    /// `loom:changes-requested` (or another contradiction) present.
    pub contradiction: Fact<bool>,
}

impl AuthzFacts {
    /// Every fact known, none denying, head `sha`.
    #[must_use]
    pub fn approved(sha: &str) -> Self {
        Self {
            head_sha: Fact::Known(sha.to_string()),
            approved_label: Fact::Known(true),
            verdict_current: Fact::Known(true),
            reviewing_claim: Fact::Known(false),
            human_hold: Fact::Known(false),
            contradiction: Fact::Known(false),
        }
    }
}

/// Why authorization was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    HeadMoved {
        approved: String,
        actual: String,
    },
    ApprovalLabelRevoked,
    StaleVerdict,
    ReviewingClaim,
    HumanHold,
    Contradiction,
    /// A required fact could not be determined; the name of the fact.
    Undeterminable(&'static str),
    /// No grant exists for this PR.
    NoGrant,
    /// The grant covers a different head.
    GrantForOtherHead {
        granted: String,
        actual: String,
    },
    /// The grant store could not be read.
    StoreUnavailable(String),
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DenyReason::HeadMoved { approved, actual } => {
                write!(f, "head moved from approved {approved} to {actual}")
            }
            DenyReason::ApprovalLabelRevoked => write!(f, "approval label no longer present"),
            DenyReason::StaleVerdict => write!(f, "verdict does not cover the current head"),
            DenyReason::ReviewingClaim => write!(f, "a review/treating claim is active"),
            DenyReason::HumanHold => write!(f, "a human hold is present"),
            DenyReason::Contradiction => write!(f, "a contradicting label is present"),
            DenyReason::Undeterminable(what) => write!(f, "could not determine {what}"),
            DenyReason::NoGrant => write!(f, "no authorization grant exists"),
            DenyReason::GrantForOtherHead { granted, actual } => {
                write!(f, "grant covers {granted}, head is {actual}")
            }
            DenyReason::StoreUnavailable(d) => write!(f, "authorization store unavailable: {d}"),
        }
    }
}

/// Decision on live facts. There is no "maybe": undeterminable facts are
/// reported as denials with [`DenyReason::Undeterminable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDecision {
    Authorized,
    Denied(Vec<DenyReason>),
}

impl AuthzDecision {
    #[must_use]
    pub fn is_authorized(&self) -> bool {
        matches!(self, AuthzDecision::Authorized)
    }
}

/// Decide whether the PR may be (or stay) queued at `approved_sha`.
/// Fail-closed: every fact must be known and non-denying.
#[must_use]
pub fn evaluate(facts: &AuthzFacts, approved_sha: &str) -> AuthzDecision {
    let mut why = Vec::new();
    match facts.head_sha.known() {
        Some(h) if h.eq_ignore_ascii_case(approved_sha) => {}
        Some(h) => why.push(DenyReason::HeadMoved {
            approved: approved_sha.to_string(),
            actual: h.clone(),
        }),
        None => why.push(DenyReason::Undeterminable("head")),
    }
    // (fact, name, deny-when, reason)
    let flags: [(&Fact<bool>, &'static str, bool, DenyReason); 5] = [
        (&facts.approved_label, "approval label", false, DenyReason::ApprovalLabelRevoked),
        (&facts.verdict_current, "verdict freshness", false, DenyReason::StaleVerdict),
        (&facts.reviewing_claim, "reviewing claim", true, DenyReason::ReviewingClaim),
        (&facts.human_hold, "human hold", true, DenyReason::HumanHold),
        (&facts.contradiction, "contradicting labels", true, DenyReason::Contradiction),
    ];
    for (fact, name, deny_when, reason) in flags {
        match fact.known() {
            Some(v) if *v == deny_when => why.push(reason),
            Some(_) => {}
            None => why.push(DenyReason::Undeterminable(name)),
        }
    }
    if why.is_empty() {
        AuthzDecision::Authorized
    } else {
        AuthzDecision::Denied(why)
    }
}

/// Authority to merge one PR at one head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub pr: u32,
    pub approved_sha: String,
}

/// The grant store failed (daemon outage, I/O). Never a permissive answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where grants live. Implementations must make [`GrantStore::revoke`]
/// durable before returning `Ok`.
pub trait GrantStore {
    /// # Errors
    ///
    /// The store could not answer.
    fn get(&self, pr: u32) -> Result<Option<Grant>, StoreError>;
    /// # Errors
    ///
    /// The store could not record the grant.
    fn put(&self, grant: Grant) -> Result<(), StoreError>;
    /// Idempotent: revoking a missing grant is `Ok`.
    ///
    /// # Errors
    ///
    /// The store could not record the revocation.
    fn revoke(&self, pr: u32) -> Result<(), StoreError>;
}

/// Conclusion reported to the forge for [`REQUIRED_CHECK_CONTEXT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckConclusion {
    Success,
    Failure(Vec<DenyReason>),
}

impl CheckConclusion {
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, CheckConclusion::Success)
    }
}

/// Body of the required `merge_group` check. `facts` is the *live* read (an
/// `Err` is an outage); `merge_group_head` is the PR head the merge-group
/// commit was built from.
#[must_use]
pub fn merge_group_check(
    store: &dyn GrantStore,
    pr: u32,
    merge_group_head: &str,
    facts: Result<AuthzFacts, String>,
) -> CheckConclusion {
    let grant = match store.get(pr) {
        Ok(Some(g)) => g,
        Ok(None) => return CheckConclusion::Failure(vec![DenyReason::NoGrant]),
        Err(e) => return CheckConclusion::Failure(vec![DenyReason::StoreUnavailable(e.0)]),
    };
    if !grant.approved_sha.eq_ignore_ascii_case(merge_group_head) {
        return CheckConclusion::Failure(vec![DenyReason::GrantForOtherHead {
            granted: grant.approved_sha,
            actual: merge_group_head.to_string(),
        }]);
    }
    let facts = match facts {
        Ok(f) => f,
        Err(e) => return CheckConclusion::Failure(vec![DenyReason::StoreUnavailable(e)]),
    };
    match evaluate(&facts, &grant.approved_sha) {
        AuthzDecision::Authorized => CheckConclusion::Success,
        AuthzDecision::Denied(why) => CheckConclusion::Failure(why),
    }
}

/// Why [`authorize_and_enqueue`] did not leave the PR queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffError {
    /// Refused by the dormant/direct gate; nothing was written.
    Gate(QueueError),
    /// Facts denied before anything was written.
    Denied(Vec<DenyReason>),
    /// The grant could not be recorded; nothing was enqueued.
    Store(StoreError),
    /// The writer credential cannot post the head status (or that could not
    /// be established), so the handoff would roll back. Refused before any
    /// grant was written or revoked; another identity's grant and queue entry
    /// are untouched (#12579). The detail names the missing permission.
    WriterCannotPostStatuses(String),
    /// The `success` status for [`REQUIRED_CHECK_CONTEXT`] could not be
    /// posted on the approved head; the grant was revoked and nothing was
    /// enqueued. GitHub only enqueues a PR whose required checks are
    /// satisfied on its head, so the enqueue could not have succeeded.
    HeadStatus(String),
    /// The enqueue failed; the grant was revoked.
    Enqueue(QueueError),
    /// A revocation raced the enqueue; the grant was revoked and the
    /// `revocation` records the rollback.
    RevokedDuringEnqueue {
        why: Vec<DenyReason>,
        revocation: Revocation,
    },
}

/// Check, grant, post the head status, enqueue, re-check. See the module docs.
///
/// Idempotent: a PR already queued at `approved_sha` under a live grant for the
/// same head reports [`EnqueueOutcome::AlreadyQueued`] before anything is
/// written, so a second pass (possibly from a credential that cannot post
/// statuses) can never revoke the standing grant. Otherwise
/// [`StatusApi::can_post_statuses`] is checked before the first write.
///
/// `read_facts` is called once before any write and once after the enqueue.
/// `statuses` receives the `success` for [`REQUIRED_CHECK_CONTEXT`] on
/// `approved_sha` (the head the grant is pinned to) between the grant and the
/// enqueue, and a best-effort `failure` on every path that rolls the grant
/// back after a successful enqueue (`pending` when the enqueue itself failed).
///
/// # Errors
///
/// [`HandoffError`]; on every error the PR is left unauthorized.
#[allow(clippy::too_many_arguments)] // injected seams (queue, grants, statuses, facts), each distinct
pub fn authorize_and_enqueue(
    mode: MergeMode,
    execution_enabled: bool,
    api: &dyn QueueApi,
    store: &dyn GrantStore,
    statuses: &dyn StatusApi,
    read_facts: &dyn Fn() -> Result<AuthzFacts, String>,
    pr: u32,
    approved_sha: &str,
) -> Result<EnqueueOutcome, HandoffError> {
    ops::execution_gate(mode, execution_enabled).map_err(HandoffError::Gate)?;
    let facts =
        read_facts().map_err(|e| HandoffError::Denied(vec![DenyReason::StoreUnavailable(e)]))?;
    if let AuthzDecision::Denied(why) = evaluate(&facts, approved_sha) {
        return Err(HandoffError::Denied(why));
    }
    if let Some(out) = already_queued(api, store, pr, approved_sha) {
        return Ok(out);
    }
    statuses
        .can_post_statuses()
        .map_err(HandoffError::WriterCannotPostStatuses)?;
    store
        .put(Grant {
            pr,
            approved_sha: approved_sha.to_ascii_lowercase(),
        })
        .map_err(HandoffError::Store)?;
    let (state, desc) = status_for(&GroupConclusion::Success);
    if let Err(e) = statuses.post_status(approved_sha, state, &desc) {
        let _ = store.revoke(pr);
        return Err(HandoffError::HeadStatus(e));
    }
    let out = match ops::enqueue(api, pr, approved_sha) {
        Ok(o) => o,
        Err(e) => {
            // Roll the grant back; a failed rollback is surfaced by the next
            // check, which still re-reads live facts.
            let _ = store.revoke(pr);
            clear_head_status(statuses, pr, approved_sha);
            return Err(HandoffError::Enqueue(e));
        }
    };
    let again = read_facts().map_err(|e| DenyReason::StoreUnavailable(e));
    let verdict = match again {
        Ok(f) => evaluate(&f, approved_sha),
        Err(r) => AuthzDecision::Denied(vec![r]),
    };
    if let AuthzDecision::Denied(why) = verdict {
        let revocation = revoke_then_dequeue(mode, execution_enabled, api, store, pr);
        withdraw_head_status(statuses, pr, approved_sha);
        return Err(HandoffError::RevokedDuringEnqueue { why, revocation });
    }
    Ok(out)
}

/// The PR is open, queued at `approved_sha`, and a live grant covers that same
/// head: nothing to do. Any read that fails answers `None`, so the normal
/// (guarded) path runs instead.
fn already_queued(
    api: &dyn QueueApi,
    store: &dyn GrantStore,
    pr: u32,
    approved_sha: &str,
) -> Option<EnqueueOutcome> {
    let status = api.status(pr).ok()?;
    if status.state != ops::PrState::Open {
        return None;
    }
    let entry = status.entry?;
    let queued_head = entry.head_oid.unwrap_or(status.head_oid);
    if !queued_head.eq_ignore_ascii_case(approved_sha) {
        return None;
    }
    let grant = store.get(pr).ok()??;
    grant
        .approved_sha
        .eq_ignore_ascii_case(approved_sha)
        .then_some(EnqueueOutcome::AlreadyQueued {
            position: entry.position,
        })
}

/// Best-effort `failure` on the PR head after a handoff rolled its grant
/// back, so a head `success` never outlives the grant it vouched for. The
/// grant is already revoked, which is what denies the merge-group check; a
/// failed write here is logged, not an error of the handoff.
pub(super) fn withdraw_head_status(statuses: &dyn StatusApi, pr: u32, head: &str) {
    let desc = format!("authorization for #{pr} revoked");
    if let Err(e) = statuses.post_status(head, StatusState::Failure, &desc) {
        log::warn!("merge-queue: could not withdraw the head status for #{pr}: {e}");
    }
}

/// Best-effort `pending` on the PR head after the enqueue itself failed. The
/// grant is already revoked, so the merge-group check still denies; but unlike
/// [`withdraw_head_status`] this leaves no `failure` for the verdict gate to
/// read as red CI, so re-approving the same head is not routed to Doctor.
pub(super) fn clear_head_status(statuses: &dyn StatusApi, pr: u32, head: &str) {
    let desc = format!("authorization for #{pr} pending re-handoff");
    if let Err(e) = statuses.post_status(head, StatusState::Pending, &desc) {
        log::warn!("merge-queue: could not clear the head status for #{pr}: {e}");
    }
}

/// Result of [`revoke_then_dequeue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    pub grant_revoked: Result<(), StoreError>,
    pub dequeue: Result<DequeueOutcome, QueueError>,
}

impl Revocation {
    /// May a Loom-owned transition (verdict invalidation, reviewing claim,
    /// `loom:operator`) proceed? Only when the PR provably cannot merge by
    /// Loom's authority: the grant is confirmed revoked, or the PR is
    /// confirmed out of the queue. A merged PR is reported separately.
    #[must_use]
    pub fn safe_to_transition(&self) -> bool {
        self.grant_revoked.is_ok()
            || matches!(self.dequeue, Ok(DequeueOutcome::Dequeued | DequeueOutcome::NotQueued))
    }

    /// The PR merged before the revocation took effect.
    #[must_use]
    pub fn already_merged(&self) -> bool {
        matches!(self.dequeue, Ok(DequeueOutcome::AlreadyMerged))
    }
}

/// Revoke the grant, then dequeue. Both are always attempted so one failure
/// never skips the other; idempotent, so duplicate events are safe.
///
/// The dequeue is attempted even in `direct`/dormant builds only through the
/// gate: a refusal is recorded in [`Revocation::dequeue`] but the grant is
/// still revoked.
pub fn revoke_then_dequeue(
    mode: MergeMode,
    execution_enabled: bool,
    api: &dyn QueueApi,
    store: &dyn GrantStore,
    pr: u32,
) -> Revocation {
    let grant_revoked = store.revoke(pr);
    let dequeue = ops::guarded_dequeue(mode, execution_enabled, api, pr);
    Revocation {
        grant_revoked,
        dequeue,
    }
}

/// In-memory [`GrantStore`] (tests, and the dormant default).
#[derive(Debug, Default)]
pub struct MemoryGrantStore {
    inner: std::sync::Mutex<BTreeMap<u32, Grant>>,
    down: std::sync::atomic::AtomicBool,
}

impl MemoryGrantStore {
    /// Simulate a store outage.
    pub fn set_down(&self, down: bool) {
        self.down.store(down, std::sync::atomic::Ordering::SeqCst);
    }
    fn check_up(&self) -> Result<(), StoreError> {
        if self.down.load(std::sync::atomic::Ordering::SeqCst) {
            Err(StoreError("store down".into()))
        } else {
            Ok(())
        }
    }
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, Grant>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl GrantStore for MemoryGrantStore {
    fn get(&self, pr: u32) -> Result<Option<Grant>, StoreError> {
        self.check_up()?;
        Ok(self.map().get(&pr).cloned())
    }
    fn put(&self, grant: Grant) -> Result<(), StoreError> {
        self.check_up()?;
        self.map().insert(grant.pr, grant);
        Ok(())
    }
    fn revoke(&self, pr: u32) -> Result<(), StoreError> {
        self.check_up()?;
        self.map().remove(&pr);
        Ok(())
    }
}
