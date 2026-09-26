//! [`TickReport`] and the per-candidate `dispatch()` outcome classification
//! (Issue #8907), split out of `work_finder.rs`, which is frozen by the
//! file-size ratchet. Both tick loops call [`record_dispatch_outcome`], so
//! the single- and multi-workspace ticks cannot drift apart on how a typed
//! refusal is counted, logged or exported.

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::ready_queue;
use crate::sweep_registry::{
    ClaimLockDispatchError, CollisionDispatchError, DispatchBackoffError, LeaseOrderDispatchError,
    LiveClaimDispatchError, OpenPrDispatchError, ParkedIssueDispatchError,
    TokenSelectionDispatchError, WorkspaceCommandsMissingDispatchError,
};
use crate::types::QueueDisposition as Qd;

/// Per-tick outcome counts, for observability and test assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Ready `loom:issue` rows returned by the source this tick.
    pub seen: usize,
    /// Issues for which a **new** sweep was dispatched this tick.
    pub dispatched: usize,
    /// Issues skipped because they carried a [`SKIP_LABELS`](super::SKIP_LABELS) entry — either in
    /// the candidate listing this tick, or at dispatch time when the
    /// dispatch-side [`PARK_LABELS`](super::PARK_LABELS) guard (#4444) found a park label the
    /// listing had not caught yet (`ParkedIssueDispatchError`). Both are the
    /// same reason, so they share one counter rather than splitting a stale-cache
    /// race across `labeled-skip` and `error(s)`.
    pub skipped_labeled: usize,
    /// Issues skipped because a live sweep already exists for them (registry
    /// in-flight set, or an idempotency no-op from `dispatch()`).
    pub skipped_in_flight: usize,
    /// Issues deferred to a future tick because the concurrency cap was reached.
    pub deferred_capacity: usize,
    /// Issues deferred to a future tick because the **per-tick admission cap**
    /// (#4234, `max_admissions_per_tick`) was reached, independent of
    /// `deferred_capacity` — this fires even when `max_concurrent` computes
    /// large enough to admit them (e.g. a token-axis jump), because the ramp
    /// cap deliberately smooths *how fast* new sweeps are admitted rather than
    /// how many may run concurrently. See [`WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV`](super::WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV).
    pub deferred_ramp_cap: usize,
    /// Issues deferred to a future tick because the **saturation admission
    /// brake** (#4903, [`crate::admission_brake`]) held new admissions: the host
    /// is already at/over the configured load-per-core hold threshold, so adding
    /// work would only slow the sweeps already running.
    ///
    /// Deliberately its own counter, not folded into
    /// [`deferred_capacity`](Self::deferred_capacity): the concurrency cap was
    /// *not* reached — the host was. Conflating them would report a token/disk
    /// shortage on a machine whose only problem is that it is already full, and
    /// send an operator to raise a knob that is not binding.
    pub deferred_saturation: usize,
    /// Issues skipped because they are quarantined for repeated insta-crashing
    /// (Issue #3939). Filtered out before the concurrency budget is allocated, so
    /// a quarantined candidate never consumes a shared dispatch slot.
    pub skipped_quarantined: usize,
    /// Issues skipped because their workspace is missing
    /// `.claude/commands/loom/sweep.md` (Issue #4027 guard 2.4, quarantined at
    /// the work-finder level by #6440). Unlike every other `skipped_*`
    /// counter here, this is incremented **once per gated workspace per
    /// tick**, not once per candidate — the whole point is that the finder no
    /// longer calls `dispatch()` (and gets the same
    /// [`WorkspaceCommandsMissingDispatchError`](crate::sweep_registry::WorkspaceCommandsMissingDispatchError)
    /// back, and logs it) once per ready issue in a structurally broken
    /// workspace, every tick, forever.
    pub skipped_workspace_commands_missing: usize,
    /// Issues skipped because they already have an **open** linked PR (Issue
    /// #4123 open-PR dispatch guard). `dispatch()` refuses these with the typed
    /// [`OpenPrDispatchError`](crate::sweep_registry::OpenPrDispatchError); the finder attributes that refusal here rather
    /// than to [`errors`](Self::errors) so a duplicate-work skip is visible and
    /// distinct from a real dispatch failure. Every in-memory dedup signal dies
    /// with the parent sweep, so without this guard an issue whose approved PR is
    /// still open would be re-dispatched the moment its sweep exits.
    pub skipped_pr_open: usize,
    /// Issues skipped because a **peer host** advertised a live soft claim over
    /// the safehouse room (Issue #4028, Phase 1). Counted under its **own**
    /// distinct reason — never folded into [`collisions`](Self::collisions)
    /// (#4085's post-hoc collision *count*) or the label/in-flight skips — so an
    /// operator can see how many dispatches the soft claim actively prevented,
    /// separate from the collisions it did not. Always `0` when
    /// `safehouse.enabled` is false (the dispatcher's `peer_claimed()` is empty).
    pub skipped_peer_claim: usize,
    /// Issues skipped because they are inside a per-issue dispatch-backoff
    /// window after a failed dispatch (Issue #4485) — either filtered out before
    /// the capacity gate via [`WorkDispatcher::backed_off`](super::WorkDispatcher::backed_off), or refused by the
    /// registry's step-2.8 guard with the typed [`DispatchBackoffError`](crate::sweep_registry::DispatchBackoffError).
    /// Attributed here rather than to [`errors`](Self::errors) because a backoff
    /// refusal is a deliberate skip, not a failure.
    pub skipped_backoff: usize,
    /// The subset of [`skipped_backoff`](Self::skipped_backoff) whose window
    /// was armed specifically by the open-PR guard (#4123) refusing dispatch,
    /// rather than a real dispatch failure (Issue #7606) — filtered out
    /// before the capacity gate via [`WorkDispatcher::pr_open_backed_off`](super::WorkDispatcher::pr_open_backed_off).
    /// Mutually exclusive with `skipped_backoff`: a candidate counted here is
    /// never also counted there. Makes the #4485 ladder's steady-state
    /// deferral of a guarded issue visible as its own tally, distinct from
    /// both a generic backoff skip and an active-tick [`skipped_pr_open`](Self::skipped_pr_open)
    /// refusal.
    pub skipped_pr_open_backoff: usize,
    /// Issues skipped because they are inside a no-op re-dispatch cooldown
    /// window (Issue #6670): a sweep self-reported "no actionable delta this
    /// pass" via `RecordNoopRelease` and the cooldown it armed has not yet
    /// elapsed. Filtered out before the capacity gate via
    /// [`WorkDispatcher::noop_cooldown`](super::WorkDispatcher::noop_cooldown), exactly like
    /// [`skipped_quarantined`](Self::skipped_quarantined) /
    /// [`skipped_backoff`](Self::skipped_backoff) — a distinct counter because
    /// this is a **successful, empty** pass, never a crash or a failed
    /// dispatch.
    pub skipped_noop_cooldown: usize,
    /// Issues skipped because a **hard-exclusion rule** applies (Issue #7528) —
    /// one counter covering both halves of that fix:
    ///
    /// 1. the candidate itself carries a
    ///    [`crate::hard_exclusion::HARD_EXCLUSION_LABELS`] entry (`external`
    ///    today), so no role has standing to act on it at all; or
    /// 2. a previous sweep for it already declined on such a rule and the
    ///    reaper's decline cooldown ([`WorkDispatcher::declined`](super::WorkDispatcher::declined)) has not
    ///    elapsed.
    ///
    /// Deliberately its own counter rather than folded into
    /// [`skipped_labeled`](Self::skipped_labeled): a park label says "a human
    /// took this out of the queue", a hard exclusion says "this issue is not
    /// Loom's to work on yet". Conflating them hides an intake backlog inside
    /// the park tally — and an operator watching `labeled-skip` climb has no
    /// way to tell which of the two they are looking at.
    pub skipped_declined: usize,
    /// Issues skipped because they are inside a **PR-less retry window** (Issue
    /// #7972): a previous dispatch claimed the issue, released it, and left no
    /// pull request behind. Filtered out before the capacity gate via
    /// [`WorkDispatcher::prless_retry`](super::WorkDispatcher::prless_retry), exactly like
    /// [`skipped_noop_cooldown`](Self::skipped_noop_cooldown) — and a distinct
    /// counter because it measures a distinct pathology: not a crash
    /// (`skipped_quarantined`), not a failed dispatch (`skipped_backoff`), not
    /// a deliberate empty pass (`skipped_noop_cooldown`), but a **full,
    /// apparently-healthy sweep that produced nothing** and would otherwise be
    /// re-offered on the very next tick.
    pub skipped_prless_retry: usize,
    /// Issues skipped because they self-declared a `<!-- loom:recheck-interval=
    /// <value> -->` marker (Issue #6685) and their own `updatedAt` is still
    /// within that interval — see [`WorkItem::is_within_recheck_interval`](super::WorkItem::is_within_recheck_interval).
    /// Filtered out before the capacity gate, exactly like
    /// [`skipped_noop_cooldown`](Self::skipped_noop_cooldown), but a distinct
    /// counter: this is issue-declared policy, checked independently of and
    /// without reading any `noop_cooldown` state.
    pub skipped_recheck_interval: usize,
    /// Issues skipped because their host-affinity constraint (#7456 —
    /// `loom:host:<id>` label / `<!-- loom:requires-host=<id> -->` body
    /// marker, see [`crate::host_affinity`]) does not name this host.
    /// Checked before the in-flight/capacity gates, exactly like
    /// [`skipped_recheck_interval`](Self::skipped_recheck_interval), and
    /// carries none of a real skip label's state side effects: no claim
    /// flip, no comment, no cooldown/backoff record — this candidate is not
    /// actionable on this host at all, so it is never even attempted.
    pub skipped_host_constraint: usize,
    /// Dispatch attempts that returned an error (logged, non-fatal).
    pub errors: usize,
    /// Cumulative cross-host dispatch collisions observed (Issue #4085, Phase 0
    /// of #4028). Unlike the other counters — which are per-tick tallies — this
    /// is a **monotonic total** read from the dispatcher(s) at tick end, so an
    /// operator watching successive summary lines sees the baseline collision
    /// rate accumulate. Always `0` unless collision detection is enabled
    /// (`LOOM_DETECT_COLLISIONS` / `autonomous.collisionDetection.enabled`).
    pub collisions: u64,
    /// True when at least one workspace was gated this tick because the
    /// main-health gate (Phase C, #3812) had halted its dispatch (`main` was
    /// **verified** red — see [`crate::main_health_gate::GateOutcome`]). `seen`
    /// still reflects the backlog depth of the halted repo(s).
    ///
    /// Derived directly from the shared
    /// [`WorkspaceHealthStates`](crate::main_health_gate::WorkspaceHealthStates)
    /// flags the gate writes (#3974 AC3), so this can never disagree with what
    /// the gate loop reports — including when a repo's forge query fails.
    pub halted: bool,
    /// True when the saturation admission brake (#4903) was engaged for this
    /// tick. Reported separately from
    /// [`deferred_saturation`](Self::deferred_saturation) so "the host was
    /// holding" is visible even when the backlog was empty and nothing was
    /// deferred — otherwise a saturated host with no queued work is
    /// indistinguishable from a healthy idle one, which is the exact reporting
    /// gap #4903 was filed on.
    pub saturation_held: bool,
    /// Candidates deferred THIS TICK because they fell outside this host's
    /// preferred repo slice while the slice still had at least one eligible
    /// in-slice candidate (Issue #6243, [`tick_multi_with_sharding`](super::tick_multi_with_sharding)).
    /// Always `0` when sharding is not configured at the call site
    /// (`preferred_slice: None`) — see `defaults/docs/dispatcher-repo-sharding.md`.
    /// Purely observational (mirrors [`deferred_saturation`](Self::deferred_saturation)'s
    /// shape): these candidates are NOT lost — they remain ready and are
    /// re-evaluated (and, if still out-of-slice with the slice non-empty,
    /// deferred again) on the next tick.
    pub deferred_out_of_slice: usize,
    /// Per-issue outcomes behind the counters above (Issue #8852), recorded
    /// by the multi-workspace tick only. See [`ready_queue`](super::ready_queue).
    pub queue: Vec<ready_queue::TickQueueRow>,
    /// Workspaces whose ready-issue listing failed this tick, so their
    /// backlog is missing from [`Self::queue`] (Issue #8852).
    pub listing_failed: Vec<usize>,
    /// The subset of [`skipped_backoff`](Self::skipped_backoff) that lost the
    /// lease-order tie-break (`LeaseOrderDispatchError`, #6287). A subset, so
    /// the `backoff-skip` tally and `loom-daemon health` are unchanged;
    /// `loom.dispatch.decisions` reports it as `lease_order_lost` instead of
    /// `backoff` (Issue #8907).
    pub refused_lease_order: usize,
    /// The subset of [`errors`](Self::errors) that died at token selection
    /// (`TokenSelectionDispatchError`, #6614) — `token_selection_failed`.
    pub refused_token_selection: usize,
    /// The subset of [`errors`](Self::errors) refused by cross-host collision
    /// enforcement (`CollisionDispatchError`, #5789) — `claim_collision`.
    pub refused_claim_collision: usize,
    /// The subset of [`errors`](Self::errors) refused because the local claim
    /// lock was already held (`ClaimLockDispatchError`) — `claim_lock_held`.
    pub refused_claim_lock: usize,
    /// One entry per `dispatch()` call this tick, in call order (Issue #8907):
    /// exported as `loom.dispatch.admission` child spans of the tick span.
    pub admissions: Vec<Admission>,
    /// Occupied concurrency slots when the tick finished (Issue #8929), or
    /// `None` when the tick returned before measuring it (a halted
    /// single-workspace tick). Feeds `loom.dispatch.idle_slots`.
    pub occupancy: Option<usize>,
}

impl TickReport {
    /// This report with no occupancy reading, for comparing an idle tick
    /// against [`TickReport::default`] in tests.
    #[cfg(test)]
    #[must_use]
    pub fn without_occupancy(self) -> Self {
        TickReport {
            occupancy: None,
            ..self
        }
    }
}

/// One `dispatch()` attempt (Issue #8907), exported as a
/// `loom.dispatch.admission` span under the tick's `loom.dispatch.tick` span.
///
/// Equality ignores the timestamps: two ticks that made the same attempts
/// with the same outcomes are the same tick (`TickReport` equality is how the
/// tests pin that two dispatch paths behave identically).
#[derive(Debug, Clone)]
pub struct Admission {
    /// The issue the attempt was for. A span attribute only, never a metric
    /// label.
    pub issue: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// `dispatched`, `in_flight` (idempotency no-op), `refused` (a deliberate,
    /// typed skip) or `error`.
    pub result: &'static str,
    /// The `loom.dispatch.decisions` `reason` the attempt was counted under.
    pub reason: &'static str,
}

impl PartialEq for Admission {
    fn eq(&self, other: &Self) -> bool {
        (self.issue, self.result, self.reason) == (other.issue, other.result, other.reason)
    }
}

impl Eq for Admission {}

/// How one `dispatch()` outcome is counted: queue disposition and detail,
/// plus the admission span's result and reason.
struct Outcome {
    disposition: Qd,
    detail: Option<String>,
    result: &'static str,
    reason: &'static str,
}

impl Outcome {
    fn new(
        disposition: Qd,
        detail: Option<String>,
        result: &'static str,
        reason: &'static str,
    ) -> Self {
        Outcome {
            disposition,
            detail,
            result,
            reason,
        }
    }
}

/// Count, log and record one `dispatch()` outcome for `issue`, started at
/// `started_at`. Returns the ready-queue disposition and detail for the row;
/// the caller consumes a capacity slot only for [`Qd::Dispatched`].
///
/// Typed refusals are matched by downcast, never by string. Each keeps the
/// counter it always had (so the tick log line and `loom-daemon health` are
/// unchanged); the lease-order, token-selection, collision and claim-lock
/// refusals are additionally tallied in their own subset counters, which
/// `loom.dispatch.decisions` reports under their own reasons (#8907).
pub fn record_dispatch_outcome(
    report: &mut TickReport,
    issue: u32,
    started_at: DateTime<Utc>,
    outcome: &Result<bool>,
) -> (Qd, Option<String>) {
    let counted = classify(report, issue, outcome);
    report.admissions.push(Admission {
        issue,
        started_at,
        ended_at: Utc::now().max(started_at),
        result: counted.result,
        reason: counted.reason,
    });
    (counted.disposition, counted.detail)
}

fn classify(report: &mut TickReport, issue: u32, outcome: &Result<bool>) -> Outcome {
    let e = match outcome {
        Ok(true) => {
            // Issue #7482: the only place the past-tense "dispatched" line is
            // logged — a confirmed new spawn, past every pre-spawn guard.
            log::info!("work_finder: dispatched issue #{issue}");
            report.dispatched += 1;
            return Outcome::new(Qd::Dispatched, None, "dispatched", "dispatched");
        }
        Ok(false) => {
            // Idempotency no-op: a sweep with the same key was already running
            // (label-flip lag). An in-flight skip that consumes no slot.
            report.skipped_in_flight += 1;
            return Outcome::new(Qd::InFlight, None, "in_flight", "in_flight");
        }
        Err(e) => e,
    };
    let why = Some(ready_queue::short_detail(&e.to_string()));
    if let Some(open_pr) = e.downcast_ref::<OpenPrDispatchError>() {
        // Open-PR guard (#4123): a skip, not a failure. #6350: name the PR.
        report.skipped_pr_open += 1;
        log::info!(
            "work_finder: skipping issue #{issue} — it already has an open linked PR #{} \
             (#4123 open-PR guard)",
            open_pr.pr
        );
        let detail = Some(format!("open PR #{}", open_pr.pr));
        Outcome::new(Qd::OpenPr, detail, "refused", "pr_open")
    } else if let Some(parked) = e.downcast_ref::<ParkedIssueDispatchError>() {
        // Park-label guard (#4444): the listing was stale; same reason as the
        // query-side filter, so the same `labeled-skip` counter.
        report.skipped_labeled += 1;
        log::info!(
            "work_finder: skipping issue #{issue} — it carries `{}` on the forge (#4444 \
             park-label guard; the candidate listing was stale)",
            parked.label
        );
        Outcome::new(Qd::Parked, Some(parked.label.to_string()), "refused", "labeled")
    } else if e.downcast_ref::<DispatchBackoffError>().is_some() {
        // Dispatch backoff (#4485), possibly armed mid-tick by a reap.
        report.skipped_backoff += 1;
        log::info!("work_finder: skipping issue #{issue} — {e}");
        Outcome::new(Qd::DispatchBackoff, why, "refused", "backoff")
    } else if e.downcast_ref::<LiveClaimDispatchError>().is_some() {
        // Live-claim guard (#4556): genuinely in flight, but a weaker signal
        // lied (the #4275 duplicate-dispatch signature), hence WARN.
        report.skipped_in_flight += 1;
        log::warn!("work_finder: skipping issue #{issue} — {e}");
        Outcome::new(Qd::InFlight, why, "refused", "in_flight")
    } else if e.downcast_ref::<LeaseOrderDispatchError>().is_some() {
        // Lease-order tie-break loss (#6287). `dispatch()` already armed this
        // issue's backoff (#6350), so it stays on `skipped_backoff`, and is
        // also tallied as `lease_order_lost` (#8907).
        report.skipped_backoff += 1;
        report.refused_lease_order += 1;
        log::info!("work_finder: skipping issue #{issue} — {e}");
        Outcome::new(Qd::DispatchBackoff, why, "refused", "lease_order_lost")
    } else if e
        .downcast_ref::<WorkspaceCommandsMissingDispatchError>()
        .is_some()
    {
        // Defense in depth (#6440): the per-tick snapshot should have caught
        // it; reaching here means the condition appeared mid-tick.
        report.skipped_workspace_commands_missing += 1;
        log::warn!("work_finder: skipping issue #{issue} — {e}");
        Outcome::new(Qd::WorkspaceCommandsMissing, why, "refused", "workspace_commands_missing")
    } else if e.downcast_ref::<TokenSelectionDispatchError>().is_some() {
        // Empty/unusable token pool (#4689, typed by #6614): a real failure,
        // still on `errors`, named because the remedy is the pool. The registry
        // has armed this issue's backoff and the cross-issue pool hold.
        report.errors += 1;
        report.refused_token_selection += 1;
        log::warn!(
            "work_finder: dispatch for issue #{issue} died at token selection — the token pool \
             is empty or every account is bad-marked (#6614): {e}"
        );
        Outcome::new(Qd::DispatchError, why, "error", "token_selection_failed")
    } else if e.downcast_ref::<CollisionDispatchError>().is_some() {
        // Cross-host collision enforcement (#5789): a peer already claimed it.
        report.errors += 1;
        report.refused_claim_collision += 1;
        log::info!("work_finder: skipping issue #{issue} — {e}");
        Outcome::new(Qd::DispatchError, why, "refused", "claim_collision")
    } else if e.downcast_ref::<ClaimLockDispatchError>().is_some() {
        // The local claim lock is already held (#8907).
        report.errors += 1;
        report.refused_claim_lock += 1;
        log::warn!("work_finder: dispatch for issue #{issue} failed: {e}");
        Outcome::new(Qd::DispatchError, why, "refused", "claim_lock_held")
    } else {
        report.errors += 1;
        log::warn!("work_finder: dispatch for issue #{issue} failed: {e}");
        Outcome::new(Qd::DispatchError, why, "error", "error")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tick_report_tests.rs"]
mod tests;
