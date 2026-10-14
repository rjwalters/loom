//! Merge-queue lifecycle: guarded handoff, reconciliation, revocation for
//! Loom-owned transitions, and merge confirmation (#10256, Phase B2).
//!
//! Everything here is written against three seams — [`QueueApi`],
//! [`LifecycleForge`] and [`EventSink`] — so the state machine runs against
//! a fake forge in `lifecycle_tests.rs`. Production wiring is in
//! [`super::gh_lifecycle`].
//!
//! # Direct mode is untouched
//!
//! Every entry point returns before its first forge call when
//! `champion.mergeMode` is `direct` (the default): [`reconcile_pr`] →
//! [`Reconciled::Direct`], [`revoke_for_transition`] → `None`, [`sweep`] →
//! empty, [`handoff`] → the `NOT_QUEUE_MODE` gate refusal.
//!
//! # Enqueue is not merge
//!
//! [`handoff`] only enqueues; nothing here closes an issue, deletes a branch
//! or a worktree, or records success. Those wait for [`reconcile_pr`] to see
//! GitHub report the PR **merged** ([`Reconciled::Merged`]); issue closure
//! itself is GitHub's `Closes #N` on that merge, and worktree cleanup is the
//! daemon reaper's merged-PR pass (#4876).

use chrono::{DateTime, Utc};

use super::authz::{
    authorize_and_enqueue, merge_group_check, revoke_then_dequeue, CheckConclusion, DenyReason,
    HandoffError, Revocation, REQUIRED_CHECK_CONTEXT,
};
use super::events::{secs_between, EventKind, EventSink, QueueEvent};
use super::forge::{read_facts, LifecycleForge};
use super::grants::{reason_token, CommentGrantStore, GrantRecord};
use super::mode::MergeMode;
use super::ops::{self, DequeueOutcome, EnqueueOutcome, PrState, QueueApi, QueueError};
use super::removal::{classify, drop_prose, route, RemovalKind};

/// Everything a lifecycle step needs.
pub struct Ctx<'a> {
    pub mode: MergeMode,
    pub execution_enabled: bool,
    pub queue: &'a dyn QueueApi,
    pub forge: &'a dyn LifecycleForge,
    pub events: &'a dyn EventSink,
    pub now: DateTime<Utc>,
}

impl Ctx<'_> {
    fn at(&self) -> String {
        self.now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }
    fn store(&self, reason: &str) -> CommentGrantStore<'_> {
        CommentGrantStore::new(self.forge, reason, self.now)
    }
    /// Telemetry never fails a lifecycle step; a write error is logged.
    fn emit(&self, ev: &QueueEvent) -> bool {
        match self.events.record(ev) {
            Ok(new) => new,
            Err(e) => {
                log::warn!("merge-queue: could not record {} telemetry: {e}", ev.key());
                false
            }
        }
    }
    pub(super) fn removed_event(
        &self,
        pr: u32,
        rec: &GrantRecord,
        reason: String,
        raw: Option<String>,
    ) {
        if let GrantRecord::Live { sha, nonce, at } = rec {
            let now = self.at();
            self.emit(&QueueEvent {
                kind: EventKind::Removed,
                pr,
                nonce: *nonce,
                head: Some(sha.clone()),
                reason: Some(reason),
                raw_reason: raw,
                enqueued_at: Some(at.clone()),
                enqueue_to_event_secs: secs_between(Some(at), &now),
                at: now,
                merged_after_revocation: false,
            });
        }
    }
}

/// Outcome of [`reconcile_pr`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    /// `champion.mergeMode=direct`: nothing read, nothing written.
    Direct,
    /// Nothing queued on Loom's authority: run the normal guards and, if
    /// they pass, [`handoff`]. The note says what was cleaned up, if anything.
    Continue(Option<String>),
    /// Queued and still authorized — leave it; the required check guards it.
    StillQueued { head: String },
    /// GitHub confirmed the merge. `recorded` is false for a duplicate.
    Merged {
        recorded: bool,
        after_revocation: bool,
    },
    /// GitHub dropped it; the reason was verified (or reported unknown),
    /// commented, and routed.
    Dropped {
        kind: RemovalKind,
        raw: Option<String>,
        route_error: Option<String>,
    },
    /// A read or write needed for a safe answer failed. Fail closed: the
    /// caller must not merge or enqueue on this pass.
    Undetermined(String),
}

fn why_token(why: &[DenyReason]) -> String {
    let first = why.first().map_or("denied", |r| match r {
        DenyReason::HeadMoved { .. } => "head-moved",
        DenyReason::ApprovalLabelRevoked => "approval-revoked",
        DenyReason::StaleVerdict => "stale-verdict",
        DenyReason::ReviewingClaim => "reviewing-claim",
        DenyReason::HumanHold => "human-hold",
        DenyReason::Contradiction => "contradiction",
        DenyReason::Undeterminable(_) => "undeterminable",
        DenyReason::NoGrant => "no-grant",
        DenyReason::GrantForOtherHead { .. } => "grant-for-other-head",
        DenyReason::StoreUnavailable(_) => "store-unavailable",
    });
    first.to_string()
}

fn join(why: &[DenyReason]) -> String {
    why.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse(ts: &str) -> Option<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(ts).ok()
}

/// Reconcile one PR against the queue. See [`Reconciled`] for the outcomes.
#[must_use]
pub fn reconcile_pr(ctx: &Ctx<'_>, pr: u32) -> Reconciled {
    if ctx.mode == MergeMode::Direct {
        return Reconciled::Direct;
    }
    let status = match ctx.queue.status(pr) {
        Ok(s) => s,
        Err(e) => return Reconciled::Undetermined(format!("queue status: {e}")),
    };
    let store = ctx.store("reconcile");
    let rec = match store.record(pr) {
        Ok(r) => r,
        Err(e) => return Reconciled::Undetermined(format!("grant record: {e}")),
    };
    match status.state {
        PrState::Merged => confirm_merge(ctx, pr, &rec),
        PrState::Closed => {
            if !matches!(rec, GrantRecord::Live { .. }) {
                return Reconciled::Continue(None);
            }
            let prose = "The PR was closed without merging.";
            match store.revoke_with(pr, "pr-closed", prose) {
                Ok(_) => {
                    ctx.removed_event(pr, &rec, "revoked-pr-closed".into(), None);
                    Reconciled::Continue(Some("closed PR: authorization revoked".into()))
                }
                Err(e) => Reconciled::Undetermined(format!("revoke on close: {e}")),
            }
        }
        PrState::Open => match status.entry {
            Some(entry) => {
                let head = entry.head_oid.unwrap_or_else(|| status.head_oid.clone());
                match merge_group_check(&store, pr, &head, read_facts(ctx.forge, pr)) {
                    CheckConclusion::Success => Reconciled::StillQueued { head },
                    CheckConclusion::Failure(why) => {
                        let s = ctx.store(&format!("revoked-{}", why_token(&why)));
                        let rev =
                            revoke_then_dequeue(ctx.mode, ctx.execution_enabled, ctx.queue, &s, pr);
                        if rev.already_merged() {
                            return confirm_merge(ctx, pr, &rec);
                        }
                        if !rev.safe_to_transition() {
                            return Reconciled::Undetermined(format!(
                                "queued PR is no longer authorized ({}) and neither the revoke \
                                 nor the dequeue was confirmed: {rev:?}",
                                join(&why)
                            ));
                        }
                        ctx.removed_event(pr, &rec, format!("revoked-{}", why_token(&why)), None);
                        Reconciled::Continue(Some(format!(
                            "queued PR was no longer authorized ({}); revoked and dequeued",
                            join(&why)
                        )))
                    }
                }
            }
            None => match &rec {
                GrantRecord::Live { at, .. } => reconcile_drop(ctx, &store, pr, &rec, at),
                _ => Reconciled::Continue(None),
            },
        },
    }
}

fn confirm_merge(ctx: &Ctx<'_>, pr: u32, rec: &GrantRecord) -> Reconciled {
    let (nonce, head, enq_at, after_revocation) = match rec {
        GrantRecord::None => {
            return Reconciled::Merged {
                recorded: false,
                after_revocation: false,
            }
        }
        GrantRecord::Live { sha, nonce, at } => {
            (*nonce, Some(sha.clone()), Some(at.clone()), false)
        }
        GrantRecord::Revoked {
            nonce,
            sha,
            granted_at,
            ..
        } => (*nonce, sha.clone(), granted_at.clone(), true),
    };
    if after_revocation {
        log::warn!(
            "merge-queue: PR #{pr} merged although its newest authorization record is a \
             revocation — the documented pass-to-merge window (#10256)"
        );
    }
    let merged_at = ctx
        .forge
        .snapshot(pr)
        .ok()
        .and_then(|s| s.merged_at)
        .unwrap_or_else(|| ctx.at());
    let recorded = ctx.emit(&QueueEvent {
        kind: EventKind::Merged,
        pr,
        nonce,
        head,
        reason: None,
        raw_reason: None,
        enqueue_to_event_secs: secs_between(enq_at.as_deref(), &merged_at),
        enqueued_at: enq_at,
        at: merged_at,
        merged_after_revocation: after_revocation,
    });
    Reconciled::Merged {
        recorded,
        after_revocation,
    }
}

fn reconcile_drop(
    ctx: &Ctx<'_>,
    store: &CommentGrantStore<'_>,
    pr: u32,
    rec: &GrantRecord,
    granted_at: &str,
) -> Reconciled {
    let removals = match ctx.forge.removals(pr) {
        Ok(r) => r,
        Err(e) => return Reconciled::Undetermined(format!("queue removal timeline: {e}")),
    };
    let granted = parse(granted_at);
    let drop = removals
        .iter()
        .rev()
        .find(|r| match (parse(&r.created_at), granted) {
            (Some(when), Some(g)) => when >= g,
            _ => false,
        });
    let Some(ev) = drop else {
        // A live grant but no entry and no removal since: the enqueue never
        // landed (a failed call or a crash between grant and enqueue).
        let prose = "No merge-queue entry exists for this grant and GitHub recorded no removal \
                     since it was written, so the enqueue never took effect. The grant is \
                     revoked; the next pass re-runs every guard before any new handoff.";
        return match store.revoke_with(pr, "no-queue-entry", prose) {
            Ok(_) => {
                ctx.removed_event(pr, rec, "revoked-no-queue-entry".into(), None);
                Reconciled::Continue(Some("stale grant with no queue entry revoked".into()))
            }
            Err(e) => Reconciled::Undetermined(format!("revoke stale grant: {e}")),
        };
    };
    let kind = classify(ev.reason.as_deref());
    let prose = drop_prose(kind, ev.reason.as_deref(), Some(&ev.created_at));
    // Revoke (with the verified reason) BEFORE any label transition. The
    // revoke marker is also the dedup point: a later pass sees `Revoked` and
    // never re-routes or re-comments this drop.
    if let Err(e) = store.revoke_with(pr, &format!("dropped-{}", kind.as_str()), &prose) {
        return Reconciled::Undetermined(format!("revoke after drop: {e}"));
    }
    let r = route(kind);
    let route_error = if r.add.is_empty() && r.remove.is_empty() {
        None
    } else {
        ctx.forge.edit_labels(pr, &r.add, &r.remove).err()
    };
    ctx.removed_event(pr, rec, kind.as_str().to_string(), ev.reason.clone());
    Reconciled::Dropped {
        kind,
        raw: ev.reason.clone(),
        route_error,
    }
}

/// Why [`handoff`] did not leave the PR queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffFailure {
    /// Direct mode or dormant build; nothing was read or written.
    Gate(QueueError),
    /// The ruleset could not be read.
    Preflight(String),
    /// The ruleset does not require [`REQUIRED_CHECK_CONTEXT`], so a
    /// revocation would not be enforced forge-side. Refused, never degraded.
    AuthzCheckNotRequired { required: Vec<String> },
    /// The authorization protocol refused or rolled back.
    Authz(HandoffError),
    /// The `Enqueued` record could not be written. `sweep` finds queued PRs
    /// only through it, so an unrecorded entry would be invisible to drop
    /// reconciliation; the handoff was revoked and dequeued (fail closed).
    Telemetry { error: String, revoked: Revocation },
}

impl std::fmt::Display for HandoffFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffFailure::Gate(e) => write!(f, "[{}] {e}", e.code()),
            HandoffFailure::Preflight(e) => write!(f, "[PREFLIGHT_FAILED] {e}"),
            HandoffFailure::AuthzCheckNotRequired { required } => write!(
                f,
                "[AUTHZ_CHECK_NOT_REQUIRED] the merge-queue ruleset does not require \
                 `{REQUIRED_CHECK_CONTEXT}` (required: {}); without it a revoked approval could \
                 still merge, so queue handoff is refused",
                if required.is_empty() {
                    "none".to_string()
                } else {
                    required.join(", ")
                }
            ),
            HandoffFailure::Authz(e) => write!(f, "[AUTHZ_REFUSED] {e:?}"),
            HandoffFailure::Telemetry { error, revoked } => write!(
                f,
                "[TELEMETRY_UNRECORDED] could not record the enqueue ({error}), so the daemon \
                 could not reconcile this PR; the grant was {} and the dequeue {}",
                if revoked.grant_revoked.is_ok() {
                    "revoked"
                } else {
                    "NOT confirmed revoked"
                },
                match &revoked.dequeue {
                    Ok(_) => "confirmed".to_string(),
                    Err(e) => format!("NOT confirmed ({e:?})"),
                },
            ),
        }
    }
}

/// Hand an approved, guard-clean PR to the queue. Callers run every direct
/// guard first (`merge-pr.sh` calls this at the point it would merge).
///
/// `required_checks` lists the ruleset's required contexts; it is consulted
/// only after the execution gate, so a dormant or direct build makes no call.
///
/// # Errors
///
/// [`HandoffFailure`]; on every error the PR is left unauthorized.
pub fn handoff(
    ctx: &Ctx<'_>,
    pr: u32,
    approved_sha: &str,
    required_checks: &dyn Fn() -> Result<Vec<String>, String>,
) -> Result<EnqueueOutcome, HandoffFailure> {
    ops::execution_gate(ctx.mode, ctx.execution_enabled).map_err(HandoffFailure::Gate)?;
    let required = required_checks().map_err(HandoffFailure::Preflight)?;
    if !required.iter().any(|c| c == REQUIRED_CHECK_CONTEXT) {
        return Err(HandoffFailure::AuthzCheckNotRequired { required });
    }
    let store = ctx.store("handoff-rollback");
    let out = authorize_and_enqueue(
        ctx.mode,
        ctx.execution_enabled,
        ctx.queue,
        &store,
        &|| read_facts(ctx.forge, pr),
        pr,
        approved_sha,
    )
    .map_err(HandoffFailure::Authz)?;
    if let Ok(GrantRecord::Live { sha, nonce, at }) = store.record(pr) {
        let ev = QueueEvent {
            kind: EventKind::Enqueued,
            pr,
            nonce,
            head: Some(sha),
            reason: None,
            raw_reason: None,
            enqueued_at: Some(at.clone()),
            at,
            enqueue_to_event_secs: Some(0),
            merged_after_revocation: false,
        };
        if let Err(error) = ctx.events.record(&ev) {
            log::warn!("merge-queue: could not record {} telemetry: {error}", ev.key());
            // `sweep` discovers queued PRs only through this record: fail closed.
            let revoked =
                revoke_then_dequeue(ctx.mode, ctx.execution_enabled, ctx.queue, &store, pr);
            return Err(HandoffFailure::Telemetry { error, revoked });
        }
    }
    Ok(out)
}

/// Revoke and dequeue **before** a Loom-owned transition (verdict
/// invalidation, a review claim, an operator hold). `None` in direct mode
/// (nothing read or written). The caller may proceed with its transition
/// when [`Revocation::safe_to_transition`]; even when it may not, the live
/// check still denies the transition's new state.
#[must_use]
pub fn revoke_for_transition(ctx: &Ctx<'_>, pr: u32, reason: &str) -> Option<Revocation> {
    if ctx.mode == MergeMode::Direct {
        return None;
    }
    let token = format!("revoked-{}", reason_token(reason));
    let store = ctx.store(&token);
    let before = store.record(pr).unwrap_or(GrantRecord::None);
    let rev = revoke_then_dequeue(ctx.mode, ctx.execution_enabled, ctx.queue, &store, pr);
    if rev.grant_revoked.is_ok() {
        ctx.removed_event(pr, &before, token, None);
    }
    Some(rev)
}

/// One audit line for a transition comment.
#[must_use]
pub fn transition_line(rev: &Revocation) -> String {
    let dequeue = match &rev.dequeue {
        Ok(DequeueOutcome::Dequeued) => "dequeued".to_string(),
        Ok(DequeueOutcome::NotQueued) => "was not queued".to_string(),
        Ok(DequeueOutcome::AlreadyMerged) => "had ALREADY MERGED".to_string(),
        Err(e) => format!("dequeue not confirmed ({})", e.code()),
    };
    let grant = match &rev.grant_revoked {
        Ok(()) => "authorization revoked".to_string(),
        Err(e) => format!("authorization revoke NOT confirmed ({e})"),
    };
    format!("- **Merge queue**: {grant}; {dequeue} (#10256).")
}

/// Reconcile every PR the event log says is pending. Direct mode: nothing.
///
/// # Errors
///
/// The event log could not be read.
pub fn sweep(ctx: &Ctx<'_>) -> Result<Vec<(u32, Reconciled)>, String> {
    if ctx.mode == MergeMode::Direct {
        return Ok(Vec::new());
    }
    Ok(ctx
        .events
        .pending()?
        .into_iter()
        .map(|pr| (pr, reconcile_pr(ctx, pr)))
        .collect())
}

/// Body of the required `loom/merge-authorization` check for the PR head a
/// merge group was built from. Mode-independent: if a ruleset requires the
/// context, it is enforced.
#[must_use]
pub fn authorize_check(
    forge: &dyn LifecycleForge,
    pr: u32,
    pr_head: &str,
    now: DateTime<Utc>,
) -> CheckConclusion {
    let store = CommentGrantStore::new(forge, "check", now);
    merge_group_check(&store, pr, pr_head, read_facts(forge, pr))
}

/// May the #8508 re-date remedy push for this repository? Never in queue
/// mode: the queue validates against the latest base itself, so a
/// tree-identical push would only move the head and invalidate the verdict.
/// An unreadable mode refuses too.
///
/// # Errors
///
/// The refusal text.
pub fn redate_permitted(root: &std::path::Path) -> Result<(), String> {
    match super::mode::resolve_merge_mode(root) {
        Ok(m) if m.mode == MergeMode::Direct => Ok(()),
        Ok(m) => Err(format!(
            "champion.mergeMode={} (from {}): queue mode never pushes a tree-identical re-date \
             commit; the merge queue tests against the latest base itself (#10256)",
            m.mode.as_str(),
            m.source.as_str()
        )),
        Err(e) => Err(format!("champion.mergeMode could not be resolved: {e}")),
    }
}
