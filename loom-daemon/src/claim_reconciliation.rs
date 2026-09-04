//! Reconciliation of stale `loom:building` claims across every managed
//! workspace (Issue #3953, acceptance criterion 3/4; promoted from a
//! startup-only pass to a periodic one by Issue #4348).
//!
//! The persisted [`crate::sweep_journal`] gives a fresh daemon (post-restart,
//! post-rate-limit-kill, post-upgrade) a liveness source that survives the
//! in-memory [`crate::sweep_registry::SweepRegistry`] being wiped clean. This
//! module is the consumer that turns that evidence into action: for every
//! registered workspace, list the open `loom:building` issues and decide,
//! per issue, whether the claim is still backed by a live sweep.
//!
//! **The journal (and every other evidence source `decide`/`plan` below
//! consult) is HOST-scoped, not fleet-scoped** — it can only prove a claim
//! dead *on this host*, and is structurally blind to a still-running sweep
//! on a *different* host. Issue #3651's fail-safe ("absent liveness evidence
//! means every claim is treated as ALIVE") was, before Epic #6165 Phase 2
//! (#6286), only *syntactically* satisfied by this evidence: a peer host's
//! claim always looks like "no evidence" to it, which the age rule below
//! eventually ages past regardless of whether the peer's sweep was actually
//! still running. The fleet-scoped fix is the lease record (Issue #6179's
//! `<!-- loom:lease host=... sweep=... -->` marker comment, renewed for the
//! sweep's lifetime by #6180) — every host reads the identical forge-assigned
//! `updated_at` on that one comment, so it is checked as the FINAL gate,
//! after `decide`/`plan` below compute a `Reclaim`, in
//! [`forge::reconcile_workspace`] (see "Lease-record freshness" further down
//! this file). A fresh lease refuses the reclaim regardless of what the
//! host-scoped rules below concluded. This is the SOLE fleet-scoped gate as
//! of Epic #6165 Phase 4 (#6317) — an earlier, now-removed gate additionally
//! consulted whether the peer-claim/safehouse advertisement channel itself
//! looked healthy (Issue #6157); that channel is fleet-scoped in principle
//! but only *eventually consistent* (`peer_claims.rs`'s own "soft claim, not
//! a mutex" framing), so treating its silence — degraded or otherwise — as
//! reclamation-relevant evidence was itself the drift Epic #6165 set out to
//! fix. The lease's forge-assigned `updated_at` needs no receipt at all,
//! which is strictly stronger.
//!
//! ## Decision rule (host-scoped evidence only — see the lease gate above)
//!
//! - A journal entry recording a **live** PID ⇒ [`ReconcileAction::Keep`] —
//!   the claim is genuinely in-flight.
//! - A journal entry recording a **dead** PID ⇒
//!   [`ReconcileAction::Reclaim`]`(`[`ReclaimReason::DeadPid`]`)` — the sweep
//!   behind this claim is provably gone *on this host* (still subject to the
//!   lease gate above before any reclaim actually fires).
//! - **No** journal entry (a manually/externally spawned `/loom:sweep` never
//!   writes one — only [`crate::sweep_registry::SweepRegistry::dispatch`]
//!   does), but the checkpoint→run-registry join described below resolves a
//!   pid ⇒ same live/dead split, reclaiming with
//!   [`ReclaimReason::DeadRunRegistry`] on a dead pid (Issue #4348 — this is
//!   the evidence source that recovers a detached sweep killed by an
//!   external `SIGKILL` while the daemon itself stays up).
//! - **No** evidence at all, on the PERIODIC pass ⇒ reclaim only once the
//!   label has been stale longer than [`resolve_stale_hours`] (`updated_at`
//!   age), otherwise `Keep` ([`ReclaimReason::NoRecordStale`]). This mirrors
//!   the Python tool's label-age grace period philosophy (#3651): absence of
//!   *this host's* evidence is not, by itself, proof of orphanhood — only
//!   *aged* absence is, and even then the lease gate above gets the final
//!   say.
//! - **No** evidence at all, on the STARTUP pass only (Issue #6615) ⇒ reclaim
//!   IMMEDIATELY, not gated on [`resolve_stale_hours`]
//!   ([`ReclaimReason::NoRecordAtStartup`]). A `loom:building` claim this
//!   daemon flipped moments before crashing — between
//!   `sweep_registry::dispatch::begin_issue_dispatch`'s label flip and
//!   `finish_issue_dispatch`'s journal write — has exactly this evidence
//!   shape, and total absence of liveness evidence immediately after a
//!   restart is much stronger proof of that crash window than the identical
//!   absence is mid-steady-state (where the periodic rule above still
//!   protects a manually/externally spawned `/loom:sweep` that has not yet
//!   written a journal entry). `decide`'s/`plan`'s `is_startup` parameter
//!   selects between these two final-fallback rules; every other rule above
//!   it is unconditional (`is_startup` never changes their outcome).
//! - No age evidence either (an issue whose `updatedAt` is somehow
//!   unavailable) ⇒ fail safe to `Keep`.
//!
//! ## Checkpoint → run-registry join (Issue #4348)
//!
//! The in-session `/loom:sweep` path (unlike a daemon dispatch) writes no
//! journal entry, but it DOES register `{run_id, pid, timestamp}` at
//! `.loom/sweep-run/<RUN_ID>.json` (`defaults/scripts/sweep-run-registry.sh
//! new`), and its checkpoint (`.loom/sweep-checkpoint/issue-<N>.json`) carries
//! `task_id` = that same run id. [`resolve_run_registry_pid`] joins the two so
//! a `loom:building` issue with no journal entry still gets an immediate,
//! provable liveness answer instead of waiting out the age-based grace
//! period. Any failure in that join (missing checkpoint, missing/garbled
//! `task_id`, missing/garbled run-registry entry) degrades to `None` —
//! [`decide`] then falls through to the age rule, never treating a failed
//! join as proof of death.
//!
//! ## PR-side claim labels (Issue #4367)
//!
//! `loom:reviewing` (Judge) and `loom:treating` (Doctor) are claim
//! *overlays* on an open PR — they coexist with the PR's underlying state
//! label (`loom:review-requested` / `loom:changes-requested` / `loom:pr`)
//! while work is in progress, and like `loom:building` they can be left
//! behind forever when their holder dies. [`decide_pr`] / [`plan_pr`] mirror
//! [`decide`] / [`plan`]'s pure-function shape and the same union-probe join
//! priority (journal, then checkpoint→run-registry), with two deliberate
//! differences driven by the weaker evidence available on the PR side:
//!
//! - **The join is heuristic, not authoritative.** A PR has no `loom:building`
//!   issue number of its own — [`parse_issue_from_branch`] recovers one only
//!   when `headRefName` matches `feature/issue-<N>` (the convention
//!   `worktree.sh` establishes); any other branch name has no evidence to
//!   join, and [`decide_pr`] falls straight through to the age rule below.
//! - **The age gate applies unconditionally**, even to a dead joined pid —
//!   unlike [`decide`]'s `DeadPid`/`DeadRunRegistry` branches, which reclaim
//!   immediately with no grace period. Because the branch-name join can be
//!   wrong (a Doctor may hold a PR whose sweep record belongs to a different
//!   phase, or the branch may simply not follow the convention), a dead pid
//!   alone is not treated as proof here — [`decide_pr`] only reclaims once
//!   the claim's age is also stale (per-label threshold,
//!   [`resolve_stale_reviewing_minutes`] / [`resolve_stale_treating_minutes`]).
//!   A **live** joined pid still short-circuits to `Keep` unconditionally,
//!   exactly like the issue side — that evidence is trustworthy either way.
//!   A missing age signal fails safe to `Keep`, same rationale as the issue
//!   side's total-absence-of-evidence case.
//! - **Freshness signal (Issue #4618): `claim_labeled_at`, not `updated_at`.**
//!   The age gate prefers [`ClaimedPr::claim_labeled_at`] — the timestamp of
//!   the claim label's own most recent `labeled` timeline event — over
//!   [`ClaimedPr::updated_at`] (the PR's aggregate "last modified"
//!   timestamp). GitHub bumps `updated_at` on ANY comment, including a
//!   Judge/Doctor stand-down comment posted by a *later* pass that declined
//!   to reclaim; using it as the sole freshness signal made the check
//!   self-perpetuating (PR #4614: 3 consecutive stand-down comments kept the
//!   claim looking "recently updated" for 30+ minutes with no actual review
//!   progress, and it was never reclaimed). Posting a comment never
//!   re-applies the label, so `claim_labeled_at` cannot be bumped that way.
//!   `updated_at` remains the fail-open fallback for when the timeline fetch
//!   failed or found nothing.
//! - **Claimant activity also refreshes the anchor (Issue #4638, narrowed by
//!   #6523).** Anchoring solely on `claim_labeled_at` fixed #4618's
//!   self-perpetuating livelock, but it also removed the only protection a
//!   claim previously had when it is *not* pid-joinable (no journal entry, no
//!   run-registry join, and a `headRefName` that doesn't match
//!   `feature/issue-<N>`) — a claimant genuinely reviewing/fixing for 35+
//!   minutes while posting real progress comments would have its claim
//!   reclaimed out from under it, because `claim_labeled_at` itself only moves
//!   on a label re-application. [`decide_pr`]'s age gate therefore anchors on
//!   `max(claim_labeled_at, most_recent_claim_activity_at)`
//!   ([`ClaimedPr::most_recent_claim_activity_at`]).
//!
//!   **What counts as claimant activity (#6523).** #4638 shipped this as "any
//!   comment after the claim that is not a marker-tagged stand-down note",
//!   which is the same conflation `defaults/scripts/claim-staleness.sh`
//!   replaced on the agent side in #6514: a routine Builder post-push status
//!   note, a Champion notice, or any bot comment was read as evidence the
//!   *claimant* was alive. Only a comment carrying this claim's own
//!   claim-activity marker now counts:
//!
//!   ```text
//!   <!-- loom:claim-activity claim=<claim_labeled_at, RFC-3339 seconds, Z> -->
//!   ```
//!
//!   ([`CLAIM_ACTIVITY_MARKER_PREFIX`] / [`claim_activity_marker`], the exact
//!   string `claim-staleness.sh`'s `marker` subcommand prints, matched on the
//!   claim's own labeled-at timestamp so a marker left over from an earlier
//!   claim generation cannot refresh a newer one). Marker-tagged stand-down
//!   comments (`<!-- loom:standdown claim=... -->`,
//!   [`STANDDOWN_MARKER_PREFIX`]) stay excluded as before, so the #4618 fix is
//!   preserved, while a genuinely live, non-pid-joinable claimant that
//!   heartbeats with the marker is still not reclaimed out from under it.
//!
//!   Because the anchor is a `max()` of timestamps rather than a boolean pin,
//!   one heartbeat buys exactly one more staleness window from *its own*
//!   timestamp — it cannot hold a claim indefinitely, matching
//!   `claim-staleness.sh`'s "activity resets the idle clock" rule. And this
//!   narrowing only ever makes the daemon reclaim *sooner*: the per-label age
//!   floors (30m/60m, #4790) are untouched and remain the veto no
//!   comment-activity outcome can bypass.
//!
//! Reclaiming removes only the stale claim label (the state label restores
//! discoverability by itself); as a safety net, if the PR is then left
//! carrying none of `loom:review-requested`/`loom:changes-requested`/`loom:pr`,
//! [`forge::reclaim_pr`] adds `loom:review-requested` so a fresh Judge pass
//! picks it back up. This pass runs from the same [`run_reconciliation_pass`]
//! entry point as the issue-side sweep, under the same
//! [`reconciliation_enabled`] kill switch — no separate wiring needed.
//!
//! ## PR-side verdict labels (Issue #5686)
//!
//! `loom:pr` and `loom:changes-requested` are *terminal verdicts*, not claim
//! overlays — and a verdict is a statement about **a specific tree**, not
//! about a PR. Before #5686 the label outlived the tree: a rebase or
//! force-push could replace every commit the verdict was written about and the
//! label would sit there unchanged. Two failure modes, both observed:
//!
//! - **A stall.** rjwalters/repo#192 (2026-08-08): Judge correctly requested
//!   changes for a failing test; the branch was rebased and force-pushed,
//!   turning CI green; the PR then sat carrying `loom:changes-requested` with
//!   nothing re-queueing it (the label says a verdict was already rendered,
//!   so no Judge reclaims it) until an operator cleared it by hand.
//! - **A stale approval — the dangerous direction.** A `loom:pr` that survives
//!   a force-push lets Champion auto-merge a tree no Judge ever reviewed.
//!
//! Judge now stamps every verdict comment with [`VERDICT_MARKER_PREFIX`]'s
//! marker (`<!-- loom:verdict-sha sha=<head> verdict=... -->`), recording
//! which tree the verdict covers. [`extract_latest_verdict_sha`] /
//! [`decide_verdict`] compare that against the PR's current `headRefOid`;
//! [`forge::reconcile_pr_verdicts`] clears a stale verdict, posts an auditable
//! old->new-SHA comment, and returns the PR to `loom:review-requested`.
//!
//! Three deliberate properties:
//!
//! - **Fail safe on missing evidence.** No marker for the currently-held
//!   verdict kind ⇒ [`VerdictKeepReason::Unverifiable`] ⇒ `Keep`. Every
//!   verdict written before this shipped is in that state, so the pass is
//!   inert on rollout rather than force-clearing the whole queue.
//! - **Any head move invalidates.** No force-push-vs-fast-forward detector:
//!   an appended commit is as much "not the tree that was reviewed" as a
//!   rebase, and telling them apart would not change an answer.
//! - **Holds are respected.** A PR carrying [`VERDICT_HOLD_LABELS`] is left
//!   alone ([`VerdictKeepReason::Held`]) — clearing would silently un-park a
//!   PR an operator (or Champion's capped-PR pass) deliberately held.
//!
//! ### Anchoring an unmarked verdict (Issue #6319)
//!
//! Failing safe on a missing marker is right, but it is not a resting state:
//! an unmarked verdict is *permanently* unverifiable, so it keeps exactly the
//! pre-#5686 hazard (an approval that survives a force-push undetected) for as
//! long as the label sits there. In production the marker is dropped roughly
//! one verdict in four — the marker exists only because judge.md *asks* the
//! model to append it, and prose compliance is not a mechanism.
//!
//! So this pass does not merely observe [`VerdictKeepReason::Unverifiable`],
//! it **remediates** it: [`decide_anchor`] / [`forge::anchor_verdict`] post a
//! marker comment recording the head SHA as of now, which bounds the exposure
//! window to a single reconciliation tick. Anchoring is deliberately *not* a
//! judgment — no label changes, so nothing is approved, rejected, or
//! un-parked; the only thing that changes is that the verdict becomes
//! invalidatable from here on. It cannot reconstruct which tree was actually
//! reviewed (a head move before the anchor is unrecoverable), which is why it
//! is a backstop for judge.md's marker, never a substitute for it.
//!
//! Anchoring never fires on ambiguous evidence: not for a held PR (whose
//! comments are never fetched), and not when the comment scan itself failed
//! ([`VerdictPr::marker_scan_ok`]) — a failed fetch is indistinguishable from
//! "no marker", and anchoring on it would post a fresh comment every tick for
//! the duration of an API outage.
//!
//! This is a *different question* from the claim-side passes above: those ask
//! "is the **reviewer** still alive?", this asks "is the **tree** still the
//! one that was reviewed?". The role-prompt fast paths (judge.md's
//! Stale-Verdict Sweep, doctor.md's Stale-Verdict Check,
//! champion-pr-merge.md's Verdict-State Janitor Part 2) only fire when an
//! agent happens to look at the PR; this pass is the always-on backstop, under
//! its own [`VERDICT_STALENESS_ENABLED_ENV`] kill switch nested inside
//! [`RECONCILE_ENABLED_ENV`].
//!
//! ## Periodic pass (Issue #4348)
//!
//! [`run_reconciliation_pass`] is the single entry point both the
//! daemon-startup call site (`main.rs`) and
//! [`spawn_periodic_reconciliation_task`]'s interval loop invoke — identical
//! behavior at both call sites, gated by the same [`reconciliation_enabled`]
//! kill switch and bounded by the same [`MAX_ISSUES_PER_WORKSPACE`]. Running
//! this on an interval (not just at startup) is what recovers a detached
//! sweep an external `SIGKILL` (or a harness model-switch, the incident that
//! motivated #4348) killed while the daemon kept running — the startup pass
//! alone can never observe that, because the daemon never restarted.
//!
//! ## Defensive stale-lock prune — deferred (Issue #4348)
//!
//! The issue's "Recommended approach" also floated a defensive prune of any
//! `.loom/locks/issue-<N>/` whose `owner.json` records a dead pid (the
//! runtime analogue of [`crate::sweep_registry::SweepRegistry::reconstruct`]'s
//! startup lock cleanup — `acquire_lock` hard-fails on `AlreadyExists`
//! without ever checking owner liveness). It is deliberately **out of scope**
//! for this change: it addresses a different failure mode (blocked
//! *re-dispatch*, not a stuck forge label) than the one #4348 reports, is not
//! covered by any of #4348's acceptance criteria, and the issue's own scope
//! guard pre-authorizes deferring it rather than growing this PR further —
//! see the issue's "Affected Files" note on `sweep_registry.rs`. Tracked as a
//! follow-up rather than bundled here.
//!
//! ## Testability
//!
//! The pure [`decide`] / [`plan`] functions take no forge/PID dependencies —
//! they are fully unit-testable (the run-registry lookup is injected into
//! [`plan`] as a closure, exactly like `is_alive`). The `gh`/label-flip glue
//! lives in [`forge::reconcile_workspace`], which mirrors the
//! `WorkSource`/adapter split established by [`crate::work_finder::forge`]
//! and [`crate::epic_supervisor::forge`] (those adapters are likewise not
//! unit-tested directly — they are thin `Command` wrappers around `gh`).

use chrono::{DateTime, Utc};
use std::path::Path;
use std::time::Duration;

use crate::sweep_journal::{self, JournalEntry, SweepJournal};

/// Env var to disable the startup reconciliation pass entirely (kill switch,
/// not a feature gate — this is corrective crash recovery in the same spirit
/// as the daemon's existing unconditional stale-claim release in
/// `activity::release_stale_claims` and `SweepRegistry::reconstruct`, so it
/// defaults to ON). `0`/`false`/`no`/`off` disables; anything else (including
/// unset) leaves it enabled.
pub const RECONCILE_ENABLED_ENV: &str = "LOOM_STALE_CLAIM_RECONCILE";

/// Env var overriding the no-record staleness threshold, in hours.
pub const STALE_HOURS_ENV: &str = "LOOM_STALE_BUILDING_HOURS";

/// Default no-record staleness threshold: a `loom:building` issue with no
/// journal entry at all must be untouched for this many hours before it is
/// considered abandoned. Deliberately generous — a claim made by a
/// pre-journal daemon, or a CLI-driven sweep with no daemon involvement at
/// all, should get a wide berth before automatic reclaim.
pub const DEFAULT_STALE_BUILDING_HOURS: f64 = 4.0;

/// Bound on how many `loom:building` issues one reconciliation pass inspects
/// per workspace (defense in depth against an unexpectedly huge backlog
/// turning startup into a `gh`-API storm).
pub const MAX_ISSUES_PER_WORKSPACE: u32 = 100;

/// Env var overriding the periodic reconciliation interval, in seconds
/// (Issue #4348). The same pass that used to run only once at daemon startup
/// now also runs on this interval for the life of the daemon, so a
/// `loom:building` claim whose sweep dies mid-run — not just across a daemon
/// restart — is recovered without an operator intervening. Governed by the
/// same [`RECONCILE_ENABLED_ENV`] kill switch as the startup pass.
pub const RECONCILE_INTERVAL_ENV: &str = "LOOM_CLAIM_RECONCILE_INTERVAL_SECS";

/// Default periodic reconciliation interval: 10 minutes. Frequent enough that
/// an externally-`SIGKILL`ed detached sweep (#4348) is recovered well within
/// an operator's "is anything stuck" attention span; infrequent enough that
/// it never becomes a `gh`-API cadence concern (each tick is bounded by
/// [`MAX_ISSUES_PER_WORKSPACE`] `gh issue list` calls per workspace, same as
/// the startup pass).
pub const DEFAULT_RECONCILE_INTERVAL_SECS: u64 = 600;

/// Floor for the periodic reconciliation interval — an operator-supplied
/// value below this is clamped up rather than honored literally, so a typo
/// (e.g. `5` meaning "5 minutes") cannot turn this into a `gh`-API busy loop.
pub const MIN_RECONCILE_INTERVAL_SECS: u64 = 60;

/// Default periodic reconciliation interval when safehouse peer-claims are
/// live (Issue #4431): 30 minutes. With claims advertised at dispatch and
/// re-advertised every reaper tick (`sweep_registry::readvertise_peer_claims`),
/// the fleet room — not the `loom:building` label — is the fast in-flight
/// claim signal, and this pass demotes to a slow *healing* sweep for the
/// divergence cases safehouse cannot see (a host that crashed without
/// retracting, a stranded label from a pre-safehouse dispatch). Hosts without
/// `safehouse.enabled` keep [`DEFAULT_RECONCILE_INTERVAL_SECS`] unchanged.
pub const DEFAULT_SAFEHOUSE_RECONCILE_INTERVAL_SECS: u64 = 1800;

/// Resolve the periodic reconciliation interval from
/// [`RECONCILE_INTERVAL_ENV`], falling back to
/// [`DEFAULT_RECONCILE_INTERVAL_SECS`] for an absent, unparseable, or
/// non-positive value, and always clamped up to
/// [`MIN_RECONCILE_INTERVAL_SECS`] regardless of source.
///
/// Env-only resolution — the safehouse-aware default lives in
/// [`resolve_reconcile_interval_for`], which callers with a workspace root
/// should prefer.
#[must_use]
pub fn resolve_reconcile_interval() -> Duration {
    let secs = std::env::var(RECONCILE_INTERVAL_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_RECONCILE_INTERVAL_SECS);
    Duration::from_secs(secs.max(MIN_RECONCILE_INTERVAL_SECS))
}

/// Resolve the periodic reconciliation interval for a workspace, safehouse-
/// aware (Issue #4431). Precedence, first match wins:
///
/// 1. [`RECONCILE_INTERVAL_ENV`] — the operator's explicit choice, any host.
/// 2. `.loom/config.json → safehouse.claimReconcileIntervalSecs` — a per-repo
///    override of the safehouse-mode cadence (zero/invalid ignored).
/// 3. [`DEFAULT_SAFEHOUSE_RECONCILE_INTERVAL_SECS`] when `safehouse.enabled`
///    resolves true for `root` — peer-claims carry the fast signal, labels
///    demote to a healing cadence.
/// 4. [`DEFAULT_RECONCILE_INTERVAL_SECS`] otherwise — byte-for-byte the
///    pre-#4431 behavior for hosts without safehouse (e.g. robb-pro).
///
/// Always clamped up to [`MIN_RECONCILE_INTERVAL_SECS`] regardless of source.
#[must_use]
pub fn resolve_reconcile_interval_for(root: &Path) -> Duration {
    if let Some(secs) = std::env::var(RECONCILE_INTERVAL_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
    {
        return Duration::from_secs(secs.max(MIN_RECONCILE_INTERVAL_SECS));
    }
    let safehouse_enabled = crate::safehouse::resolve_config(root).enabled;
    let config_override = crate::config_resolver::get_path(
        &crate::config_resolver::resolve_effective_config(root),
        "safehouse",
    )
    .and_then(|block| block.get("claimReconcileIntervalSecs"))
    .and_then(serde_json::Value::as_u64)
    .filter(|&v| v > 0);
    let secs = config_override.unwrap_or(if safehouse_enabled {
        DEFAULT_SAFEHOUSE_RECONCILE_INTERVAL_SECS
    } else {
        DEFAULT_RECONCILE_INTERVAL_SECS
    });
    Duration::from_secs(secs.max(MIN_RECONCILE_INTERVAL_SECS))
}

/// Resolve whether the startup reconciliation pass is enabled.
#[must_use]
pub fn reconciliation_enabled() -> bool {
    match std::env::var(RECONCILE_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Resolve the no-record staleness threshold (hours) from
/// [`STALE_HOURS_ENV`], falling back to [`DEFAULT_STALE_BUILDING_HOURS`] for
/// an absent, unparseable, or non-positive value.
#[must_use]
pub fn resolve_stale_hours() -> f64 {
    std::env::var(STALE_HOURS_ENV)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STALE_BUILDING_HOURS)
}

/// Env var overriding the [`ReclaimReason::ExitedNoProgress`] fast-reclaim
/// grace period, in minutes (Issue #4616).
pub const NO_PROGRESS_GRACE_MINUTES_ENV: &str = "LOOM_NO_PROGRESS_GRACE_MINUTES";

/// Default [`ReclaimReason::ExitedNoProgress`] fast-reclaim grace period: 10
/// minutes. A checkpoint stalled at `curator-done` with no run-registry pid
/// and no open PR is ambiguous between two shapes that look identical the
/// instant a new sweep resumes: a genuinely-orphaned sweep (#4462, stranded
/// ~35 minutes) and a legitimately-resumed Builder retry whose fresh run has
/// not yet linked back to the checkpoint or opened a PR (#4616). Ten minutes
/// comfortably covers normal Builder dispatch startup latency while staying
/// far shorter than [`DEFAULT_STALE_BUILDING_HOURS`], so the #4462 fast-reclaim
/// benefit is preserved for the genuinely-orphaned case.
pub const DEFAULT_NO_PROGRESS_GRACE_MINUTES: f64 = 10.0;

/// Resolve the `ExitedNoProgress` grace period (minutes) from
/// [`NO_PROGRESS_GRACE_MINUTES_ENV`], falling back to
/// [`DEFAULT_NO_PROGRESS_GRACE_MINUTES`] for an absent, unparseable, or
/// non-positive value.
#[must_use]
pub fn resolve_no_progress_grace_minutes() -> f64 {
    std::env::var(NO_PROGRESS_GRACE_MINUTES_ENV)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_NO_PROGRESS_GRACE_MINUTES)
}

/// Env var overriding the `loom:reviewing` staleness threshold, in minutes
/// (Issue #4367). Deliberately the SAME name `.claude/commands/loom/judge.md`
/// already established for its own "Stale `loom:reviewing` Claim Check" — the
/// agent-side fast path and this always-on daemon backstop share one
/// convention rather than drifting into two knobs for the same concept.
pub const STALE_REVIEWING_MINUTES_ENV: &str = "LOOM_STALE_REVIEWING_MINUTES";

/// Default `loom:reviewing` staleness threshold: 30 minutes, matching
/// judge.md's default exactly.
pub const DEFAULT_STALE_REVIEWING_MINUTES: f64 = 30.0;

/// Env var overriding the `loom:treating` staleness threshold, in minutes
/// (Issue #4367).
pub const STALE_TREATING_MINUTES_ENV: &str = "LOOM_STALE_TREATING_MINUTES";

/// Default `loom:treating` staleness threshold: 60 minutes — a Doctor's fix
/// cycle (re-run tests, iterate on feedback) legitimately runs longer than a
/// single Judge review pass.
pub const DEFAULT_STALE_TREATING_MINUTES: f64 = 60.0;

/// Resolve the `loom:reviewing` staleness threshold (minutes) from
/// [`STALE_REVIEWING_MINUTES_ENV`], falling back to
/// [`DEFAULT_STALE_REVIEWING_MINUTES`] for an absent, unparseable, or
/// non-positive value.
#[must_use]
pub fn resolve_stale_reviewing_minutes() -> f64 {
    std::env::var(STALE_REVIEWING_MINUTES_ENV)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STALE_REVIEWING_MINUTES)
}

/// Resolve the `loom:treating` staleness threshold (minutes) from
/// [`STALE_TREATING_MINUTES_ENV`], falling back to
/// [`DEFAULT_STALE_TREATING_MINUTES`] for an absent, unparseable, or
/// non-positive value.
#[must_use]
pub fn resolve_stale_treating_minutes() -> f64 {
    std::env::var(STALE_TREATING_MINUTES_ENV)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STALE_TREATING_MINUTES)
}

// ============================================================================
// Lease-record freshness (Epic #6165 Phase 2, Issue #6286)
// ============================================================================
//
// Phase 1 (#6179, #6180 — merged) writes a `<!-- loom:lease host=<host>
// sweep=<sweep-id> --> ` marker comment on a `loom:building` issue at dispatch
// time and renews it (an idempotent PATCH of the SAME comment, never a new
// one) every ~5 minutes for the sweep's lifetime — see
// `defaults/docs/lease-record.md` / `defaults/docs/lease-renewal.md`. This is
// the first FLEET-scoped liveness source available to reclamation: unlike the
// sweep journal, checkpoint->run-registry join, or label-age grace period
// `decide`/`plan` use above (all HOST-scoped — none of them can see a still-
// running sweep on a *different* host), every host reads the identical
// forge-assigned `updated_at` on the SAME comment, so "is the lease still
// fresh" answers the same way everywhere with no clock-skew correction
// needed. [`lease_is_fresh`] is the pure freshness check; the forge-querying
// fetch lives in [`forge::fetch_freshest_lease_updated_at`] (issue-side) and
// `crate::worktree_ops::gh::freshest_lease_updated_at` (the `recover-orphans`
// CLI path), both consulted as the LAST gate before a reclaim fires — see
// [`forge::reconcile_workspace`] and
// `worktree_ops::orphan_recovery::check_untracked_building`. Epic #6165
// Phase 4 (#6317) removed the peer-claim-coordination-degraded gate that
// used to run alongside this one (Issue #6157) — the lease is now the SOLE
// fleet-scoped reclamation gate.

/// Env var overriding the lease-freshness TTL, in minutes (Epic #6165 Phase
/// 2, Issue #6286).
pub const LEASE_TTL_MINUTES_ENV: &str = "LOOM_LEASE_TTL_MINUTES";

/// Default lease-freshness TTL: 15 minutes = 3x `sweep-lease-renew.sh`'s
/// ~5-minute default renewal interval (`defaults/docs/lease-renewal.md`),
/// giving a live sweep two full missed renewal cycles of slack before its
/// claim is treated as unproven by this evidence source.
pub const DEFAULT_LEASE_TTL_MINUTES: f64 = 15.0;

/// Resolve the lease-freshness TTL (minutes) from [`LEASE_TTL_MINUTES_ENV`],
/// falling back to [`DEFAULT_LEASE_TTL_MINUTES`] for an absent, unparseable,
/// or non-positive value.
#[must_use]
pub fn resolve_lease_ttl_minutes() -> f64 {
    std::env::var(LEASE_TTL_MINUTES_ENV)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_LEASE_TTL_MINUTES)
}

/// Marker prefix identifying a lease-record comment (Issue #6179) — see
/// `defaults/docs/lease-record.md`. Machine readers must locate a lease
/// comment via this literal prefix only (`.starts_with(...)`), and must never
/// parse or depend on the free-form prose that follows it.
pub const LEASE_MARKER_PREFIX: &str = "<!-- loom:lease host=";

/// Is a claim's freshest lease record within `ttl_minutes` of `now`? Pure,
/// total, fully unit-testable.
///
/// Per `defaults/docs/lease-record.md`'s load-bearing design decision, the
/// liveness signal is the lease comment's own forge-assigned `updated_at` —
/// never a timestamp embedded in the marker text — so callers must resolve
/// `lease_updated_at` from comment metadata (`updated_at` on the REST
/// comments endpoint), not from parsing the body.
///
/// Callers must treat the ABSENCE of a lease record (no comment found at
/// all) as "no lease evidence", never as "lease is not fresh" — this
/// function only answers the freshness question for a lease that was
/// actually found; see [`forge::fetch_freshest_lease_updated_at`]'s
/// `Option` contract for the corresponding "no evidence either way" case.
#[must_use]
pub fn lease_is_fresh(
    lease_updated_at: DateTime<Utc>,
    now: DateTime<Utc>,
    ttl_minutes: f64,
) -> bool {
    let age_minutes = (now - lease_updated_at).num_seconds() as f64 / 60.0;
    age_minutes < ttl_minutes
}

/// What the lease probe found for a claim at the moment a reclaim decision
/// was about to fire (Issue #6320).
///
/// Reclaim log lines previously recorded only the [`ReclaimReason`] — the
/// HOST-scoped evidence (dead pid / aged label). That leaves the most
/// important question about a cross-host reclaim unanswerable after the
/// fact: was the claim reclaimed because its lease had genuinely expired, or
/// because it never published one at all? Issue #6320 reports exactly that
/// ambiguity ("stated as a hypothesis rather than a finding: I did not
/// instrument the recovery pass, so *no lease ⇒ judged orphaned* is inferred
/// from the timeline and the absent lease comment, not measured"). Carrying
/// this classification into the log turns that inference into a measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LeaseEvidence {
    /// No lease comment was found on the issue (or the probe failed) — no
    /// fleet-scoped liveness evidence either way. Per
    /// `defaults/docs/lease-record.md`'s reader contract this is NOT
    /// evidence of abandonment; the reclaim, if it fires, rests entirely on
    /// the host-scoped [`ReclaimReason`].
    Absent,
    /// A lease record exists and is within the TTL: positive, fleet-scoped
    /// evidence the holder is alive. The reclaim is refused.
    Fresh { age_minutes: f64 },
    /// A lease record exists but has not been renewed within the TTL — the
    /// holder is presumed gone by the one signal every host shares.
    Stale { age_minutes: f64 },
}

impl std::fmt::Display for LeaseEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => write!(
                f,
                "lease_evidence=absent (no loom:lease comment found — not evidence of \
                 abandonment; this decision rests on host-scoped evidence alone)"
            ),
            Self::Fresh { age_minutes } => {
                write!(f, "lease_evidence=fresh (renewed {age_minutes:.1}m ago)")
            }
            Self::Stale { age_minutes } => {
                write!(f, "lease_evidence=stale (last renewed {age_minutes:.1}m ago, past the TTL)")
            }
        }
    }
}

/// Classify the lease probe's result for logging and for the refuse/proceed
/// branch. Pure, total, fully unit-testable — `None` means the probe found
/// no lease comment (or could not read one).
#[must_use]
pub fn classify_lease_evidence(
    lease_updated_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    ttl_minutes: f64,
) -> LeaseEvidence {
    match lease_updated_at {
        None => LeaseEvidence::Absent,
        Some(updated_at) => {
            let age_minutes = (now - updated_at).num_seconds() as f64 / 60.0;
            if lease_is_fresh(updated_at, now, ttl_minutes) {
                LeaseEvidence::Fresh { age_minutes }
            } else {
                LeaseEvidence::Stale { age_minutes }
            }
        }
    }
}

/// A `loom:building` issue reported by the forge, trimmed to the fields the
/// reconciliation decision needs.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildingIssue {
    pub number: u32,
    /// Parsed `updatedAt` timestamp, when available.
    pub updated_at: Option<DateTime<Utc>>,
}

/// Why a claim is being reclaimed — carried through to the log line so an
/// operator reading the daemon log can tell a dead-process reclaim from a
/// no-record/stale-age reclaim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReclaimReason {
    /// The journal recorded a PID for this issue and it is no longer alive.
    DeadPid { pid: u32 },
    /// No journal entry existed, but the checkpoint→run-registry join
    /// (Issue #4348, [`resolve_run_registry_pid`]) resolved a pid for this
    /// issue's manually/externally spawned sweep, and it is no longer alive.
    /// Reclaimed immediately — no age grace — because the death is provable,
    /// exactly like [`Self::DeadPid`].
    DeadRunRegistry { pid: u32 },
    /// No live/dead pid evidence exists (no journal entry, and the
    /// checkpoint→run-registry join returned `None` — the in-session sweep's
    /// run-registry entry was cleaned up at process exit), BUT a checkpoint
    /// records that the sweep ran and made **no progress toward a PR** — it is
    /// still at the pre-Builder `curator-done` phase with no open linked PR
    /// (Issue #4462), AND the checkpoint's own `timestamp` is older than the
    /// [`resolve_no_progress_grace_minutes`] grace period (Issue #4616). This
    /// is the exit-0/no-progress orphan the age gate would otherwise sit on
    /// for hours: the sweep provably ran (a checkpoint exists), provably
    /// stopped (its run-registry entry is gone — that file is only removed by
    /// `sweep-run-registry.sh cleanup` at sweep end), and produced nothing.
    /// Reclaimed fast, ahead of the age gate, so an in-session sweep that
    /// ended its turn on a transport-failure backoff (the #4462 incident) does
    /// not strand its claim. The grace period exists because the identical
    /// checkpoint/no-pid/no-PR shape is also, for a few minutes, what a
    /// **legitimately-resumed Builder retry** looks like: the checkpoint's
    /// `task_id` is only rewritten when Builder completes, not when a resumed
    /// attempt starts, so a fresh run can be genuinely in-flight under a new,
    /// unlinked run-registry entry while this evidence still reads as "no
    /// progress" (Issue #4616).
    ExitedNoProgress,
    /// No journal record exists, and the issue's `loom:building` label has
    /// been present longer than the staleness threshold.
    NoRecordStale { age_hours: f64 },
    /// Issue #6615: no journal entry, no run-registry pid, AND no no-progress
    /// checkpoint evidence exists for this claim — total absence of liveness
    /// evidence — checked on the daemon-STARTUP reconciliation pass only
    /// ([`decide`]'s `is_startup` parameter), reclaimed immediately without
    /// waiting out [`resolve_stale_hours`]'s multi-hour grace period. A
    /// `loom:building` claim this daemon flipped just before crashing
    /// (`sweep_registry::dispatch::begin_issue_dispatch` flips the label,
    /// then spawns the child and only writes the journal entry in
    /// `finish_issue_dispatch` — a crash inside that window leaves the label
    /// flipped with zero durable evidence) looks, immediately after a
    /// restart, exactly like this. That absence is much stronger evidence of
    /// a dropped mid-spawn dispatch immediately after a restart than the
    /// identical absence is during steady-state operation, where a
    /// manually/externally spawned `/loom:sweep` legitimately has no journal
    /// entry yet and must not be reclaimed early — the periodic pass (this
    /// pass's non-startup sibling, sharing the same [`run_reconciliation_pass`]
    /// entry point) never fires this reason, staying on the
    /// [`Self::NoRecordStale`] age gate exactly as before.
    NoRecordAtStartup,
}

/// The reconciliation decision for one issue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReconcileAction {
    /// No liveness concern — leave the claim alone.
    Keep,
    /// Flip `loom:building` back to `loom:issue`.
    Reclaim(ReclaimReason),
}

/// Pure decision function — no I/O, fully unit-testable. See the module docs
/// for the decision rule.
///
/// `run_registry_pid` is the checkpoint→run-registry join result (Issue
/// #4348, [`resolve_run_registry_pid`]) — consulted only when `journal_entry`
/// is absent, exactly like the age rule below it. Passing `Some(pid)` when a
/// `journal_entry` is also present is harmless: the journal always takes
/// priority (it is the more authoritative, daemon-owned evidence source).
///
/// The exit-0/no-progress evidence [`decide`] consults for its
/// [`ReclaimReason::ExitedNoProgress`] fast reclaim — the stalled
/// checkpoint's own `timestamp`, so the grace period below can be judged
/// against when the sweep last touched the checkpoint, not the (potentially
/// much older, unrelated) `loom:building` label age (Issue #4616).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoProgressEvidence {
    /// The stalled checkpoint's own `timestamp` field.
    pub checkpoint_timestamp: DateTime<Utc>,
}

/// `no_progress` (Issue #4462, refined by #4616) is the caller's
/// exit-0/no-progress evidence, consulted only when BOTH `journal_entry` and
/// `run_registry_pid` are absent (i.e. there is no live/dead pid to reason
/// about). When `Some`, a checkpoint exists showing the sweep ran but stalled
/// at the pre-Builder `curator-done` phase with no open linked PR and no live
/// process. This is fired ahead of the (hours-scale) age gate but only once
/// the checkpoint's own age has cleared `no_progress_grace_minutes` — within
/// the grace window the identical evidence shape is also what a
/// legitimately-just-resumed Builder retry looks like (its fresh run has not
/// yet linked back to the checkpoint or opened a PR), so `decide` `Keep`s
/// instead of reclaiming (Issue #4616). Checked STRICTLY AFTER the
/// journal/run-registry checks: a live pid (journal or run-registry) always
/// wins, so a running sweep is never reclaimed even if `no_progress` is
/// mistakenly `Some`.
///
/// `is_startup` (Issue #6615) selects which fallback applies once every
/// pid/no-progress evidence source above has come back empty: `true` (the
/// daemon-startup pass only) fires [`ReclaimReason::NoRecordAtStartup`]
/// immediately; `false` (the periodic pass, and every other caller) falls
/// through to the pre-existing [`resolve_stale_hours`]-gated age rule,
/// unchanged. See [`ReclaimReason::NoRecordAtStartup`] for the rationale.
#[must_use]
#[allow(clippy::too_many_arguments)] // pure decision seam: each arg is a distinct injected evidence source
pub fn decide(
    issue: &BuildingIssue,
    journal_entry: Option<&JournalEntry>,
    run_registry_pid: Option<u32>,
    no_progress: Option<NoProgressEvidence>,
    no_progress_grace_minutes: f64,
    is_alive: &dyn Fn(u32) -> bool,
    stale_hours: f64,
    now: DateTime<Utc>,
    is_startup: bool,
) -> ReconcileAction {
    if let Some(entry) = journal_entry {
        return if is_alive(entry.pid) {
            ReconcileAction::Keep
        } else {
            ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: entry.pid })
        };
    }

    if let Some(pid) = run_registry_pid {
        return if is_alive(pid) {
            ReconcileAction::Keep
        } else {
            ReconcileAction::Reclaim(ReclaimReason::DeadRunRegistry { pid })
        };
    }

    // No live/dead pid evidence at all. Before the slow age gate, a sweep that
    // provably ran (a checkpoint exists) and provably stopped (its run-registry
    // entry is gone) at the pre-Builder phase with no PR is a NO-PROGRESS
    // candidate (Issue #4462) — but only past its own grace period does that
    // become proof of an orphan rather than a just-resumed retry (Issue
    // #4616).
    if let Some(evidence) = no_progress {
        let age_minutes = (now - evidence.checkpoint_timestamp).num_seconds() as f64 / 60.0;
        if age_minutes >= no_progress_grace_minutes {
            return ReconcileAction::Reclaim(ReclaimReason::ExitedNoProgress);
        }
        // Within the grace window: this is the legitimately-just-resumed-
        // Builder shape (Issue #4616), not (yet) proof of an orphaned sweep.
        // Deliberately Keep rather than falling through to the age rule below
        // — that rule reasons about `issue.updated_at`, which can predate
        // this fresh checkpoint by hours (it is only bumped by forge activity,
        // not by a resumed sweep touching its own checkpoint file), and would
        // wrongly reclaim a genuinely in-flight resumed Builder run.
        return ReconcileAction::Keep;
    }

    // Issue #6615: total absence of liveness evidence (no journal entry, no
    // run-registry pid, no no-progress checkpoint) checked on the STARTUP
    // pass is reclaimed immediately, bypassing the age gate below entirely —
    // see `ReclaimReason::NoRecordAtStartup` for why this is safe only
    // immediately after a restart, not during steady-state operation (the
    // `is_startup == false` case below is byte-for-byte unchanged).
    if is_startup {
        return ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup);
    }

    match issue.updated_at {
        Some(updated) => {
            let age_hours = (now - updated).num_seconds() as f64 / 3600.0;
            if age_hours >= stale_hours {
                ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { age_hours })
            } else {
                ReconcileAction::Keep
            }
        }
        // No evidence of any kind: fail safe, never reclaim on a total
        // absence of information.
        None => ReconcileAction::Keep,
    }
}

/// Live-claim veto (Issue #4556) — downgrade a computed `Reclaim` to `Keep`
/// when the issue still has a **confirmed-live** sweep claim behind it.
///
/// [`decide`]'s liveness evidence is a single recorded PID (the journal entry,
/// or the checkpoint→run-registry join). That PID is the sweep *leader* the
/// daemon spawned, so a leader that has exited — or a journal entry that was
/// never written, or was pruned — makes a still-working sweep look dead. On
/// #4275 that misfire is on the record: at `03:08:15.992Z` this pass reclaimed
/// `loom:building -> loom:issue` on `DeadPid { pid: 2781227 }` while sweep
/// processes for the issue were still alive, re-exposing it to the work-finder
/// and seeding a re-dispatch 37 seconds later.
///
/// The veto consults evidence a dead-leader PID cannot invalidate — a live
/// claim-lock owner, a live machine-level journal record from this repo *or a
/// nested worktree daemon*, or a live `/loom:sweep <N>` process rooted in the
/// workspace ([`crate::live_claim::probe`]).
///
/// Applies to **every** [`ReclaimReason`], not just the dead-PID ones: a live
/// sweep means the claim is legitimate no matter which rule proposed dropping
/// it. Pure and total — the caller supplies the (impure) probe result.
#[must_use]
pub fn apply_live_claim_veto(
    action: ReconcileAction,
    live_claim: Option<&crate::live_claim::LiveClaimEvidence>,
) -> ReconcileAction {
    match (action, live_claim) {
        (ReconcileAction::Reclaim(_), Some(_)) => ReconcileAction::Keep,
        (action, _) => action,
    }
}

/// Plan reconciliation decisions for every `issue` in `issues`, given an
/// already-loaded `journal`. Performs no I/O of its own — `run_registry_pid_for`
/// is the caller's injected checkpoint→run-registry lookup (production passes
/// [`resolve_run_registry_pid`] bound to a workspace root; tests pass a fake
/// closure), called only for an issue with no journal entry, mirroring
/// `decide`'s own priority order.
///
/// IMPORTANT (#3975): `journal` must be the **raw, unpruned** journal.
/// Pruning dead-PID entries before calling this function would erase the
/// exact evidence the `DeadPid` branch of [`decide`] needs to fire its
/// unconditional, immediate reclaim -- see [`forge::reconcile_workspace`]
/// for the caller that got this wrong before #3975.
///
/// `no_progress_for` (Issue #4462, refined by #4616) is the caller's injected
/// exit-0/no-progress lookup, called only for an issue with no journal entry
/// AND no run-registry pid — the same priority ordering [`decide`] enforces,
/// so the (potentially gh-querying) lookup never runs when a live/dead pid
/// already decides the issue. It returns the stalled checkpoint's own
/// `timestamp` (as [`NoProgressEvidence`]) when the no-progress shape holds,
/// `None` otherwise; [`decide`] then gates the fast reclaim on that
/// timestamp's age against `no_progress_grace_minutes`.
///
/// `is_startup` (Issue #6615) is forwarded unchanged to every [`decide`] call
/// this makes — `true` only on the daemon-startup pass, `false` on the
/// periodic pass — selecting the immediate [`ReclaimReason::NoRecordAtStartup`]
/// fallback vs. the pre-existing age-gated [`ReclaimReason::NoRecordStale`]
/// one. See [`decide`]'s doc comment.
#[must_use]
#[allow(clippy::too_many_arguments)] // pure decision seam: each arg is a distinct injected evidence source
pub fn plan(
    repo: &str,
    issues: &[BuildingIssue],
    journal: &SweepJournal,
    run_registry_pid_for: &dyn Fn(u32) -> Option<u32>,
    no_progress_for: &dyn Fn(u32) -> Option<NoProgressEvidence>,
    no_progress_grace_minutes: f64,
    is_alive: &dyn Fn(u32) -> bool,
    stale_hours: f64,
    now: DateTime<Utc>,
    is_startup: bool,
) -> Vec<(u32, ReconcileAction)> {
    issues
        .iter()
        .map(|issue| {
            let entry = sweep_journal::find(journal, repo, issue.number);
            let run_registry_pid = if entry.is_none() {
                run_registry_pid_for(issue.number)
            } else {
                None
            };
            // Mirror decide()'s priority: only consult the no-progress evidence
            // when there is no live/dead pid to reason about at all.
            let no_progress = if entry.is_none() && run_registry_pid.is_none() {
                no_progress_for(issue.number)
            } else {
                None
            };
            (
                issue.number,
                decide(
                    issue,
                    entry,
                    run_registry_pid,
                    no_progress,
                    no_progress_grace_minutes,
                    is_alive,
                    stale_hours,
                    now,
                    is_startup,
                ),
            )
        })
        .collect()
}

// ============================================================================
// PR-side claim labels: loom:reviewing / loom:treating (Issue #4367)
// ============================================================================

/// Which PR-side claim overlay a [`decide_pr`] call is reconciling — drives
/// the forge label name and (via [`resolve_stale_reviewing_minutes`] /
/// [`resolve_stale_treating_minutes`]) which staleness threshold applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrClaimKind {
    /// Judge's working label.
    Reviewing,
    /// Doctor's working label.
    Treating,
}

impl PrClaimKind {
    /// The forge label name for this claim kind.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Reviewing => "loom:reviewing",
            Self::Treating => "loom:treating",
        }
    }

    /// The resolved staleness threshold (minutes) for this claim kind.
    #[must_use]
    pub fn stale_minutes(self) -> f64 {
        match self {
            Self::Reviewing => resolve_stale_reviewing_minutes(),
            Self::Treating => resolve_stale_treating_minutes(),
        }
    }
}

/// Marker (Issue #4636/#4618) tagging a Judge/Doctor "standing down, not
/// stomping" comment — evidence of *no* progress (a later pass declining to
/// reclaim), not genuine activity. A comment containing this substring is
/// excluded from [`ClaimedPr::most_recent_claim_activity_at`] (Issue #4638),
/// mirroring `defaults/scripts/claim-staleness.sh`'s own stand-down exclusion
/// (a substring match, not an exact marker+claim-timestamp match, so any
/// stand-down comment for any claim generation is excluded).
pub const STANDDOWN_MARKER_PREFIX: &str = "<!-- loom:standdown claim=";

/// Marker (Issue #6514, adopted daemon-side by #6523) a **claimant** appends to
/// its own progress comments to prove it is still alive:
///
/// ```text
/// <!-- loom:claim-activity claim=<CLAIMED_AT> -->
/// ```
///
/// This is the single shared definition on the Rust side, and it must stay
/// byte-identical to `ACTIVITY_PREFIX` in `defaults/scripts/claim-staleness.sh`
/// — the agent-side evaluator judge.md / doctor.md / curator.md drive, whose
/// `marker` subcommand prints exactly the string [`claim_activity_marker`]
/// builds. Both sides match on the claim's **own** `labeled`-event timestamp,
/// so a marker left behind by an earlier claim generation can never refresh a
/// later one.
///
/// Distinct from [`STANDDOWN_MARKER_PREFIX`] (a *later* pass declining to
/// reclaim — evidence of no progress) and from [`VERDICT_MARKER_PREFIX`]
/// (which tree a verdict describes).
pub const CLAIM_ACTIVITY_MARKER_PREFIX: &str = "<!-- loom:claim-activity claim=";

/// Render the full claim-activity marker for a claim labeled at `claimed_at` —
/// the exact string `claim-staleness.sh marker` prints, and the substring
/// [`most_recent_claim_activity_at`] requires a comment to contain before it
/// counts as claimant liveness.
///
/// The timestamp is rendered RFC-3339 with second precision and a `Z` suffix,
/// which is how the forge emits a timeline event's `created_at` and therefore
/// how `claim-staleness.sh` (which interpolates that field verbatim, having
/// validated it against `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$`) renders it.
#[must_use]
pub fn claim_activity_marker(claimed_at: DateTime<Utc>) -> String {
    format!(
        "{CLAIM_ACTIVITY_MARKER_PREFIX}{} -->",
        claimed_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// One comment on a claimed PR, trimmed to the two fields the claim-activity
/// scan needs. Constructed by [`forge::fetch_most_recent_claim_activity_at`]
/// from `gh pr view --json comments`, and directly by the unit tests — the
/// predicate itself ([`most_recent_claim_activity_at`]) is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrComment {
    pub created_at: DateTime<Utc>,
    pub body: String,
}

/// The timestamp of the most recent **claimant activity** comment posted after
/// `claimed_at` — the pure predicate behind
/// [`ClaimedPr::most_recent_claim_activity_at`] (Issue #6523).
///
/// A comment counts iff all three hold, mirroring
/// `defaults/scripts/claim-staleness.sh` exactly:
///
/// 1. it was posted strictly after `claimed_at` (the claim's own `labeled`
///    event — anything at or before it belongs to a previous claim
///    generation);
/// 2. it carries [`claim_activity_marker`]`(claimed_at)` — the claim-activity
///    marker for **this** claim, not merely some claim;
/// 3. it is not a stand-down comment ([`STANDDOWN_MARKER_PREFIX`]).
///
/// Condition 3 is redundant against a well-formed stand-down comment (which
/// carries no activity marker) and is kept deliberately: it is the #4618
/// regression guard, and a belt-and-braces exclusion costs nothing if a future
/// stand-down body ever quotes an activity marker verbatim.
///
/// `None` when nothing qualifies — callers then anchor on `claim_labeled_at` /
/// `updated_at` alone, i.e. an unrelated comment does not postpone reclamation
/// at all.
#[must_use]
pub fn most_recent_claim_activity_at(
    comments: &[PrComment],
    claimed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let marker = claim_activity_marker(claimed_at);
    comments
        .iter()
        .filter(|c| c.created_at > claimed_at)
        .filter(|c| !c.body.contains(STANDDOWN_MARKER_PREFIX))
        .filter(|c| c.body.contains(&marker))
        .map(|c| c.created_at)
        .max()
}

/// An open PR carrying a `loom:reviewing`/`loom:treating` claim label,
/// trimmed to the fields the reconciliation decision needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimedPr {
    pub number: u32,
    /// Parsed `updatedAt` timestamp, when available. Kept as a fallback
    /// freshness signal only — see [`Self::claim_labeled_at`] for why it is
    /// no longer the primary one (Issue #4618).
    pub updated_at: Option<DateTime<Utc>>,
    /// Timestamp of the most recent `labeled <claim-label>` timeline event
    /// for this PR's currently-held claim label (Issue #4618), when
    /// resolvable. Unlike [`Self::updated_at`] — GitHub's aggregate
    /// "last modified" timestamp, which a plain comment (including a Judge's
    /// or Doctor's own "standing down, not stomping" note) bumps just as
    /// readily as genuine progress — this timestamp changes ONLY when the
    /// claim label is re-applied (i.e. an actual reclaim). A stand-down
    /// comment can therefore never self-refresh it, which is what makes it
    /// safe to use as the primary age-gate signal in [`decide_pr`]. `None`
    /// when the timeline fetch failed or returned no matching event — callers
    /// then fall back to [`Self::updated_at`], preserving pre-#4618 behavior
    /// for that fail-open case.
    pub claim_labeled_at: Option<DateTime<Utc>>,
    /// Timestamp of the most recent **claimant activity** comment posted since
    /// [`Self::claim_labeled_at`] (Issue #4638, narrowed by #6523), when
    /// resolvable — i.e. the newest comment carrying this claim's own
    /// [`claim_activity_marker`], per [`most_recent_claim_activity_at`].
    /// [`decide_pr`] anchors its age gate on
    /// `max(claim_labeled_at, most_recent_claim_activity_at)`, so a claimant
    /// that is not pid-joinable but is genuinely heartbeating is not reclaimed
    /// out from under it purely because `claim_labeled_at` itself has aged.
    ///
    /// `None` when there is no `claim_labeled_at` to compare against, the
    /// comment fetch failed, or no comment since the claim carried this
    /// claim's activity marker — which now includes the ordinary chatty-PR
    /// case (#6523: a Builder status note, a Champion notice or a stand-down
    /// note is *not* claimant activity and must not postpone reclamation).
    /// Callers then fall back to `claim_labeled_at`/`updated_at` alone,
    /// preserving the #4618 regression guard: a claim with no claimant
    /// heartbeat since must still be reclaimed once stale.
    pub most_recent_claim_activity_at: Option<DateTime<Utc>>,
    /// The PR's head branch name, when available — the only join key to an
    /// issue number this pass has (see [`parse_issue_from_branch`]).
    pub head_ref_name: Option<String>,
}

/// Why a PR-side claim is being reclaimed. Mirrors [`ReclaimReason`] but with
/// a minutes-scale age (PR-side thresholds are minutes, not hours) and no
/// "no record" variant of its own — an unjoined PR that ages out is reported
/// as [`Self::Aged`], the same variant a joined-but-dead pid falls into once
/// it also ages out (see [`decide_pr`]'s unconditional age gate).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrReclaimReason {
    /// The journal recorded a PID for this PR's joined issue and it is no
    /// longer alive, AND the PR has also aged past the staleness threshold.
    DeadPid { pid: u32 },
    /// The checkpoint→run-registry join resolved a pid for this PR's joined
    /// issue and it is no longer alive, AND the PR has also aged past the
    /// staleness threshold.
    DeadRunRegistry { pid: u32 },
    /// No live-pid evidence was available at all (no join, or a join that
    /// resolved to nothing), and the PR has aged past the staleness
    /// threshold.
    Aged { age_minutes: f64 },
}

/// The reconciliation decision for one PR-side claim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrReconcileAction {
    /// No liveness concern — leave the claim label alone.
    Keep,
    /// Remove the claim label (see [`forge::reclaim_pr`] for the safety-net
    /// state-label backfill).
    Reclaim(PrReclaimReason),
}

/// One PR-side claim decided [`PrReconcileAction::Reclaim`], detailed enough
/// for a non-daemon caller to report or apply independently of the periodic
/// daemon backstop's own logging (Issue #6167 — `recover-orphans`'s
/// dry-run/`--recover` CLI contract needs the same detection pass
/// [`forge::reconcile_pr_claims`] already runs, but as data rather than log
/// lines, and without unconditionally mutating the forge in dry-run mode).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrClaimOutcome {
    pub pr_number: u32,
    pub label: &'static str,
    pub reason: PrReclaimReason,
    /// `true` only when the caller passed `recover=true` AND the label
    /// removal actually succeeded. `false` in dry-run mode, or when the `gh`
    /// call failed (already logged at `warn` by
    /// [`forge::reconcile_pr_claims_report`]).
    pub reclaimed: bool,
}

/// Recover the `loom:building` issue number a PR's head branch encodes, when
/// it follows the `feature/issue-<N>` convention `worktree.sh` establishes.
/// `None` for any other branch shape — the caller then has no join key at
/// all and [`decide_pr`] falls straight through to the age rule.
#[must_use]
pub fn parse_issue_from_branch(head_ref_name: &str) -> Option<u32> {
    head_ref_name
        .strip_prefix("feature/issue-")?
        .parse::<u32>()
        .ok()
}

/// Pure decision function for one PR-side claim label — no I/O, fully
/// unit-testable. See the module docs' "PR-side claim labels" section for the
/// decision rule; in short, it mirrors [`decide`]'s union-probe join priority
/// (journal, then checkpoint→run-registry) but — unlike the issue side —
/// applies the age gate **unconditionally**, even to a dead joined pid,
/// because the branch-name join here is heuristic rather than authoritative.
///
/// `run_registry_pid` is the caller's join result, consulted only when
/// `journal_entry` is absent, exactly like [`decide`].
///
/// **Age-gate freshness signal (Issue #4618, refined by #4638, narrowed by
/// #6523)**: the age gate below anchors on
/// `max(pr.claim_labeled_at, pr.most_recent_claim_activity_at)`,
/// falling back to `pr.updated_at` only when BOTH are unavailable.
/// `claim_labeled_at` (the claim label's own most recent `labeled` timeline
/// event) replaced `updated_at` as the primary signal in #4618: before that
/// fix, `updated_at` was the sole signal, and GitHub bumps it on ANY comment
/// — including a Judge/Doctor "standing down, not stomping" comment posted
/// by a later pass declining to reclaim. That made the check perpetually
/// self-refreshing: each stand-down comment satisfied the very freshness
/// test the next pass ran, so a claim could survive past the staleness
/// window once and then never be reclaimed again (PR #4614). But anchoring
/// solely on `claim_labeled_at` also removed the only protection a claim had
/// when it is not pid-joinable: a claimant genuinely working for 35+ minutes
/// while heartbeating would be reclaimed anyway, since `claim_labeled_at`
/// itself only moves on a label re-application (#4638).
/// `most_recent_claim_activity_at` restores that protection — but only for a
/// comment carrying **this claim's** [`claim_activity_marker`] (#6523).
/// A comment by anyone else (a Builder post-push note, a Champion notice, a
/// bot) is not claimant liveness and does not move the anchor at all, which is
/// what makes this side agree with `defaults/scripts/claim-staleness.sh`; a
/// marker-tagged stand-down comment stays excluded on top of that, so the
/// #4618 fix holds. `updated_at` remains the final fail-open fallback for when
/// neither signal is available (same fail-safe posture as the pre-#4618 `None`
/// branch below).
///
/// **This narrowing never weakens the age floor.** It can only make a reclaim
/// happen *sooner*, never sooner than the per-label threshold: every reclaim
/// still has to clear `stale_minutes` (30m reviewing / 60m treating, #4790),
/// which is the protection against the #4618 double-claim race and is applied
/// below regardless of what the comment evidence says. A live joined pid also
/// still short-circuits to `Keep` ahead of the age gate entirely.
#[must_use]
pub fn decide_pr(
    pr: &ClaimedPr,
    journal_entry: Option<&JournalEntry>,
    run_registry_pid: Option<u32>,
    is_alive: &dyn Fn(u32) -> bool,
    stale_minutes: f64,
    now: DateTime<Utc>,
) -> PrReconcileAction {
    // A live joined pid is trustworthy evidence either way -- short-circuit
    // to Keep unconditionally, exactly like the issue side.
    let dead_pid_reason = if let Some(entry) = journal_entry {
        if is_alive(entry.pid) {
            return PrReconcileAction::Keep;
        }
        Some(PrReclaimReason::DeadPid { pid: entry.pid })
    } else if let Some(pid) = run_registry_pid {
        if is_alive(pid) {
            return PrReconcileAction::Keep;
        }
        Some(PrReclaimReason::DeadRunRegistry { pid })
    } else {
        None
    };

    // Age gate applies unconditionally from here on -- whether the evidence
    // was a dead joined pid, or no join at all (non-joinable branch / no
    // journal / no run-registry entry). A dead pid alone is never sufficient
    // proof here: the PR->issue join is heuristic, so only *aged* absence (or
    // aged dead-pid evidence) triggers a reclaim.
    //
    // Anchor on max(claim_labeled_at, most_recent_claim_activity_at) (Issue
    // #4638, narrowed by #6523) -- neither can be self-refreshed by a comment
    // the way updated_at can: claim_labeled_at only moves on a re-claim, and
    // the activity signal only counts comments carrying THIS claim's
    // claim-activity marker (a stranger's comment moves neither). Only fall
    // back to updated_at when BOTH are unavailable (timeline fetch
    // failure/partial response, or no comment evidence at all), matching the
    // pre-#4618 fail-open posture.
    let anchor = match (pr.claim_labeled_at, pr.most_recent_claim_activity_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => pr.updated_at,
    };
    match anchor {
        Some(freshness_at) => {
            let age_minutes = (now - freshness_at).num_seconds() as f64 / 60.0;
            if age_minutes >= stale_minutes {
                PrReconcileAction::Reclaim(
                    dead_pid_reason.unwrap_or(PrReclaimReason::Aged { age_minutes }),
                )
            } else {
                PrReconcileAction::Keep
            }
        }
        // No age evidence at all: fail safe, never reclaim.
        None => PrReconcileAction::Keep,
    }
}

/// Plan PR-side reconciliation decisions for every `pr` in `prs`, given an
/// already-loaded `journal`. Performs no I/O of its own — `run_registry_pid_for`
/// is the caller's injected checkpoint→run-registry lookup, called only for a
/// PR whose branch joins to an issue number with no journal entry, mirroring
/// [`plan`]'s own priority order.
#[must_use]
pub fn plan_pr(
    repo: &str,
    prs: &[ClaimedPr],
    journal: &SweepJournal,
    run_registry_pid_for: &dyn Fn(u32) -> Option<u32>,
    is_alive: &dyn Fn(u32) -> bool,
    stale_minutes: f64,
    now: DateTime<Utc>,
) -> Vec<(u32, PrReconcileAction)> {
    prs.iter()
        .map(|pr| {
            let issue_number = pr
                .head_ref_name
                .as_deref()
                .and_then(parse_issue_from_branch);
            let (entry, run_registry_pid) = match issue_number {
                Some(n) => {
                    let entry = sweep_journal::find(journal, repo, n);
                    let run_registry_pid = if entry.is_none() {
                        run_registry_pid_for(n)
                    } else {
                        None
                    };
                    (entry, run_registry_pid)
                }
                // No join key at all: no journal entry, no run-registry
                // lookup -- decide_pr falls straight through to the age rule.
                None => (None, None),
            };
            (pr.number, decide_pr(pr, entry, run_registry_pid, is_alive, stale_minutes, now))
        })
        .collect()
}

// ============================================================================
// PR-side verdict labels: loom:pr / loom:changes-requested (Issue #5686)
// ============================================================================

/// Env kill switch for the stale-verdict backstop. Like
/// [`RECONCILE_ENABLED_ENV`] this is a kill switch, not a feature gate: the
/// pass is corrective and inert on any PR whose verdict carries no marker, so
/// it defaults to ON. `0`/`false`/`no`/`off` disables it; anything else
/// (including unset) leaves it enabled. The master [`RECONCILE_ENABLED_ENV`]
/// switch still gates the whole reconciliation pass above this one.
pub const VERDICT_STALENESS_ENABLED_ENV: &str = "LOOM_VERDICT_STALENESS_RECONCILE";

/// Env kill switch for the unmarked-verdict anchoring remediation (Issue
/// #6319), nested inside [`VERDICT_STALENESS_ENABLED_ENV`]. Also a kill
/// switch rather than a feature gate — anchoring writes no labels and fires
/// only on a verdict the pass has *positively confirmed* carries no marker —
/// so it defaults to ON. `0`/`false`/`no`/`off` disables it, leaving the
/// pre-#6319 behavior (count and log the unanchored verdict, remediate
/// nothing).
pub const VERDICT_ANCHOR_ENABLED_ENV: &str = "LOOM_VERDICT_ANCHOR";

/// The marker Judge stamps into every verdict comment
/// (`defaults/.claude/commands/loom/judge.md` -> "Verdict SHA Marker"),
/// recording WHICH TREE the verdict describes:
///
/// ```text
/// <!-- loom:verdict-sha sha=<head-sha> verdict=approved|changes-requested -->
/// ```
///
/// Distinct from [`CLAIM_ACTIVITY_MARKER_PREFIX`] / [`STANDDOWN_MARKER_PREFIX`]
/// (claim freshness) and from judge.md's `<!-- loom:fallback-evaluated
/// sha=... -->` (fallback-queue dedup) — four markers, three questions.
pub const VERDICT_MARKER_PREFIX: &str = "<!-- loom:verdict-sha sha=";

/// Is the stale-verdict backstop enabled? See [`VERDICT_STALENESS_ENABLED_ENV`].
#[must_use]
pub fn verdict_staleness_enabled() -> bool {
    match std::env::var(VERDICT_STALENESS_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Is the unmarked-verdict anchoring remediation enabled? See
/// [`VERDICT_ANCHOR_ENABLED_ENV`].
#[must_use]
pub fn verdict_anchoring_enabled() -> bool {
    match std::env::var(VERDICT_ANCHOR_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Which terminal review verdict a [`VerdictPr`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictKind {
    /// `loom:pr` — Judge approved. The dangerous direction when stale: a
    /// surviving approval lets Champion auto-merge a tree nobody reviewed.
    Approved,
    /// `loom:changes-requested` — Judge rejected. When stale it stalls the PR:
    /// the label says a verdict was already rendered, so no Judge reclaims it.
    ChangesRequested,
}

impl VerdictKind {
    /// The forge label name for this verdict.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "loom:pr",
            Self::ChangesRequested => "loom:changes-requested",
        }
    }

    /// The `verdict=` token this verdict is recorded under in the marker.
    #[must_use]
    pub fn marker_token(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequested => "changes-requested",
        }
    }
}

/// Labels that park a PR out of automated flow — an operator hold, or
/// Champion's capped-PR recovery parking. A stale verdict on such a PR is
/// still stale, but clearing it would silently un-park the PR, so this pass
/// leaves it alone (mirrors `verdict-staleness-guard.sh`'s `--clear`
/// suppression and doctor.md's Priority-2 `PARK_LABELS` exclusion).
pub const VERDICT_HOLD_LABELS: [&str; 3] = ["loom:blocked", "loom:operator", "loom:operator-only"];

/// An open PR carrying a terminal verdict label, trimmed to the fields the
/// staleness decision needs.
#[derive(Debug, Clone, PartialEq)]
pub struct VerdictPr {
    pub number: u32,
    /// Which verdict label this PR carries.
    pub kind: VerdictKind,
    /// The PR's current `headRefOid`. `None` when the listing did not
    /// resolve one — [`decide_verdict`] then fails safe to `Keep`.
    pub head_sha: Option<String>,
    /// The SHA recorded by the newest [`VERDICT_MARKER_PREFIX`] marker whose
    /// `verdict=` token matches [`Self::kind`]. `None` for a verdict written
    /// before the marker convention shipped (or by a host still running the
    /// older prompt) — [`decide_verdict`] fails safe to `Keep` there too.
    pub marker_sha: Option<String>,
    /// Was the PR's comment listing actually read? Distinguishes the two
    /// causes of `marker_sha: None` that [`decide_verdict`] deliberately
    /// treats identically (both fail safe to `Keep`) but which
    /// [`decide_anchor`] must NOT (Issue #6319):
    ///
    /// - `true` — the comments were fetched and carry no marker for this
    ///   verdict kind. Positive evidence the verdict is unanchored, so it is
    ///   both countable and safe to remediate.
    /// - `false` — the fetch failed, or was skipped entirely (a held PR).
    ///   Absence of evidence, not evidence of absence: anchoring here would
    ///   post a duplicate marker comment on every tick for the length of an
    ///   API outage, so the anchoring pass declines.
    pub marker_scan_ok: bool,
    /// Does the PR carry any of [`VERDICT_HOLD_LABELS`]?
    pub on_hold: bool,
}

/// Why [`decide_verdict`] left a verdict in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictKeepReason {
    /// The recorded SHA matches the current head — the verdict still
    /// describes the tree in front of it.
    Fresh,
    /// No marker for this verdict kind. Fail safe: never force-clear a
    /// verdict on missing evidence (this is every pre-#5686 verdict).
    Unverifiable,
    /// The PR's head SHA could not be resolved. Fail safe.
    NoHeadSha,
    /// Stale, but the PR is on an explicit hold ([`VERDICT_HOLD_LABELS`]) —
    /// clearing it would un-park a PR a human deliberately held.
    Held,
}

/// The staleness decision for one verdict-labelled PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictAction {
    /// Leave the verdict label alone.
    Keep(VerdictKeepReason),
    /// The verdict describes a tree that is gone: clear the label and return
    /// the PR to `loom:review-requested`.
    Invalidate {
        marker_sha: String,
        head_sha: String,
    },
}

/// Why [`decide_anchor`] declined to anchor an unmarked verdict (Issue #6319).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorSkipReason {
    /// The verdict already records a SHA — [`decide_verdict`] owns it.
    AlreadyAnchored,
    /// The PR is parked on an explicit hold ([`VERDICT_HOLD_LABELS`]). Its
    /// comments are never fetched (so its marker state is unknown), and a PR
    /// a human deliberately took out of automated flow should not collect
    /// automated comments either.
    Held,
    /// The comment listing could not be read, so "no marker" is unproven.
    /// Anchoring on a failed fetch would spam one comment per tick.
    MarkerScanFailed,
    /// No resolvable head SHA — there is nothing to anchor the verdict to.
    NoHeadSha,
}

/// What to do about a verdict [`decide_verdict`] found
/// [`VerdictKeepReason::Unverifiable`] (Issue #6319).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorAction {
    /// Record `head_sha` as the tree this verdict describes, by posting a
    /// marker comment. **Writes no labels**: the verdict is neither granted,
    /// revoked, nor re-queued — it merely becomes invalidatable from here on.
    Anchor { head_sha: String },
    /// Leave it alone.
    Skip(AnchorSkipReason),
}

/// Pure decision function for remediating an unanchored verdict — the
/// companion to [`decide_verdict`], deliberately kept **separate** rather
/// than folded into it (Issue #6319).
///
/// Keeping the two apart is load-bearing, not stylistic. [`decide_verdict`]
/// answers #5686's question — "does this verdict still describe the tree in
/// front of it?" — and its answers must not shift: a verdict that already
/// carries a marker behaves byte-for-byte as it did before this function
/// existed. This one answers a different question — "can this verdict ever be
/// checked at all?" — and only ever runs after [`decide_verdict`] has already
/// failed safe to `Keep(Unverifiable)`.
///
/// Anchoring strictly reduces exposure and can never widen it. It does not
/// grant approval (Champion merges on the *label*, which is already there and
/// is left untouched); it only bounds how long a subsequent force-push can go
/// undetected, from "forever" to "until the next tick". What it cannot do is
/// recover a head move that happened *before* the anchor — a verdict anchored
/// late is anchored to a tree that may never have been reviewed, which is why
/// this is a backstop for judge.md's marker rather than a replacement.
#[must_use]
pub fn decide_anchor(pr: &VerdictPr) -> AnchorAction {
    if pr.marker_sha.as_deref().is_some_and(|s| !s.is_empty()) {
        return AnchorAction::Skip(AnchorSkipReason::AlreadyAnchored);
    }
    if pr.on_hold {
        return AnchorAction::Skip(AnchorSkipReason::Held);
    }
    if !pr.marker_scan_ok {
        return AnchorAction::Skip(AnchorSkipReason::MarkerScanFailed);
    }
    match pr.head_sha.as_deref() {
        Some(head_sha) if !head_sha.is_empty() => AnchorAction::Anchor {
            head_sha: head_sha.to_string(),
        },
        _ => AnchorAction::Skip(AnchorSkipReason::NoHeadSha),
    }
}

/// One verdict-reconciliation pass's counters (Issue #6319). Before this,
/// [`forge::reconcile_pr_verdicts`] returned only `(checked, invalidated)`,
/// so the `Unverifiable` outcome — the one that silently reinstates the
/// pre-#5686 hazard — had no counter anywhere in the daemon and was
/// invisible to an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerdictReconcileStats {
    /// Verdict-labelled PRs inspected.
    pub checked: usize,
    /// Stale verdicts cleared and re-queued as `loom:review-requested`.
    pub invalidated: usize,
    /// Verdicts positively confirmed to carry no marker. Counts only PRs
    /// whose comments were actually read ([`VerdictPr::marker_scan_ok`]), so
    /// an API failure or a held PR never inflates it.
    pub unverifiable: usize,
    /// Of those, how many this pass anchored to the current head.
    pub anchored: usize,
}

impl VerdictReconcileStats {
    /// Unanchored verdicts this pass could not remediate — the residual
    /// exposure an operator needs to see.
    #[must_use]
    pub fn residual_unverifiable(&self) -> usize {
        self.unverifiable.saturating_sub(self.anchored)
    }

    /// Fold another workspace's counters in.
    pub fn merge(&mut self, other: Self) {
        self.checked += other.checked;
        self.invalidated += other.invalidated;
        self.unverifiable += other.unverifiable;
        self.anchored += other.anchored;
    }
}

/// The newest SHA recorded for `kind` across `bodies` (oldest-first, the order
/// the forge's comment listing returns). Returns `None` when no comment
/// carries a marker for that verdict kind.
///
/// **Filtering on `verdict=` is load-bearing, not cosmetic.** A PR rejected at
/// SHA A and later approved at SHA B carries markers for both, and only the
/// one matching the CURRENTLY-HELD label says anything about the current
/// verdict. Taking "the newest marker of any kind" would let a marker written
/// for a superseded verdict vouch for (or wrongly invalidate) the live one.
#[must_use]
pub fn extract_latest_verdict_sha(bodies: &[String], kind: VerdictKind) -> Option<String> {
    let pattern = format!(
        r"<!-- loom:verdict-sha sha=([0-9a-f]{{7,40}}) verdict={} -->",
        regex::escape(kind.marker_token())
    );
    let re = regex::Regex::new(&pattern).ok()?;
    bodies
        .iter()
        .rev()
        .find_map(|body| re.captures_iter(body).last().map(|c| c[1].to_string()))
}

/// Pure decision function for one verdict-labelled PR — no I/O, fully
/// unit-testable, mirroring [`decide`] / [`decide_pr`]'s shape.
///
/// Any head-SHA change invalidates the verdict. There is deliberately no
/// force-push-vs-fast-forward detector: for a statement about a tree, an
/// appended commit is as much "not the tree that was reviewed" as a rebase is,
/// and distinguishing them would not change a single answer (#5686 scopes it
/// out explicitly).
#[must_use]
pub fn decide_verdict(pr: &VerdictPr) -> VerdictAction {
    let Some(head_sha) = pr.head_sha.as_deref() else {
        return VerdictAction::Keep(VerdictKeepReason::NoHeadSha);
    };
    let Some(marker_sha) = pr.marker_sha.as_deref() else {
        return VerdictAction::Keep(VerdictKeepReason::Unverifiable);
    };
    if marker_sha.is_empty() || head_sha.is_empty() {
        return VerdictAction::Keep(VerdictKeepReason::Unverifiable);
    }
    // Compare on the marker's own length so an abbreviated marker SHA still
    // matches the full head SHA it prefixes. Roles always stamp the full
    // `headRefOid`; this only guards a hand-written or truncated marker.
    if head_sha.starts_with(marker_sha) {
        return VerdictAction::Keep(VerdictKeepReason::Fresh);
    }
    if pr.on_hold {
        return VerdictAction::Keep(VerdictKeepReason::Held);
    }
    VerdictAction::Invalidate {
        marker_sha: marker_sha.to_string(),
        head_sha: head_sha.to_string(),
    }
}

// ============================================================================
// Checkpoint -> run-registry join (Issue #4348)
// ============================================================================

/// Best-effort extraction of the `task_id` a `/loom:sweep` checkpoint
/// recorded for `issue`, from `<root>/.loom/sweep-checkpoint/issue-<issue>.json`
/// (schema owned by `defaults/scripts/sweep-checkpoint.sh`; treated as opaque
/// JSON here, mirroring `sweep_registry::read_checkpoint_phase`'s posture
/// toward the same file). `None` on a missing file, unreadable file,
/// malformed JSON, or a missing/non-string `task_id` key.
#[must_use]
fn read_checkpoint_task_id(root: &Path, issue: u32) -> Option<String> {
    let path = root
        .join(".loom")
        .join("sweep-checkpoint")
        .join(format!("issue-{issue}.json"));
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("task_id")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

/// Best-effort extraction of the `phase` a `/loom:sweep` checkpoint recorded
/// for `issue`, from `<root>/.loom/sweep-checkpoint/issue-<issue>.json` (schema
/// owned by `defaults/scripts/sweep-checkpoint.sh`; `phase` is one of
/// `curator-done|builder-done|judge-rejected|judge-done|doctor-done|merge-done`).
/// `None` on a missing/unreadable/malformed file or a missing/non-string
/// `phase` key — the exit-0/no-progress reclaim (Issue #4462) treats any such
/// failure as "cannot confirm no progress" and falls through to the age rule,
/// never a spurious fast reclaim.
#[must_use]
fn read_checkpoint_phase(root: &Path, issue: u32) -> Option<String> {
    let path = root
        .join(".loom")
        .join("sweep-checkpoint")
        .join(format!("issue-{issue}.json"));
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("phase")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
}

/// The only checkpoint phase from which the exit-0/no-progress fast reclaim
/// (Issue #4462) may fire: `curator-done` is the sole pre-Builder phase, so a
/// checkpoint stalled here means the Builder never completed and no PR was
/// produced. Every later phase (`builder-done` and beyond) implies a PR
/// already exists, so a stalled sweep there is the resume/age machinery's
/// concern, not this fast path's.
const NO_PROGRESS_PHASE: &str = "curator-done";

/// Best-effort extraction of the `timestamp` a `/loom:sweep` checkpoint
/// recorded for `issue`, from `<root>/.loom/sweep-checkpoint/issue-<issue>.json`
/// (schema owned by `defaults/scripts/sweep-checkpoint.sh`, an ISO 8601 UTC
/// string). `None` on a missing/unreadable/malformed file, a missing/
/// non-string `timestamp` key, or a value that fails to parse as RFC 3339 —
/// the exit-0/no-progress grace period (Issue #4616) treats any such failure
/// as "cannot confirm the checkpoint's age" and falls through to the
/// (unconditionally slower) age rule, never a spurious fast reclaim.
#[must_use]
fn read_checkpoint_timestamp(root: &Path, issue: u32) -> Option<DateTime<Utc>> {
    let path = root
        .join(".loom")
        .join("sweep-checkpoint")
        .join(format!("issue-{issue}.json"));
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let ts = v.get("timestamp").and_then(serde_json::Value::as_str)?;
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Best-effort extraction of the liveness `pid` an in-session `/loom:sweep`
/// registered for `task_id`, from `<root>/.loom/sweep-run/<task_id>.json`
/// (schema owned by `defaults/scripts/sweep-run-registry.sh new`). `None` on
/// a missing file, unreadable file, malformed JSON, or a missing/non-numeric
/// `pid` key.
#[must_use]
fn read_run_registry_pid(root: &Path, task_id: &str) -> Option<u32> {
    let path = root
        .join(".loom")
        .join("sweep-run")
        .join(format!("{task_id}.json"));
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
}

/// Join a `loom:building` issue's checkpoint `task_id` to the
/// `.loom/sweep-run/<task_id>.json` run registry (Issue #4348) — the evidence
/// source for a manually/externally spawned `/loom:sweep` that never wrote a
/// [`crate::sweep_journal`] entry (only
/// [`crate::sweep_registry::SweepRegistry::dispatch`] does). Returns `None`
/// when either lookup fails for any reason: a missing/malformed checkpoint,
/// an absent `task_id`, or a missing/malformed run-registry entry all
/// degrade identically to "no evidence" — a caller must fall through to
/// [`decide`]'s age-based `NoRecordStale` rule, never treat a failed join as
/// proof of death.
#[must_use]
pub fn resolve_run_registry_pid(root: &Path, issue: u32) -> Option<u32> {
    let task_id = read_checkpoint_task_id(root, issue)?;
    read_run_registry_pid(root, &task_id)
}

// ============================================================================
// Shared entry point (startup + periodic, Issue #4348)
// ============================================================================

/// Run one reconciliation pass across every workspace registered in
/// [`crate::workspace_registry::WorkspaceRegistry`] (an empty registry
/// reduces to just `fallback_root`): flips stale `loom:building` claims back
/// to `loom:issue` ([`forge::reconcile_workspace`]). Shared by the
/// daemon-startup call site (`main.rs`, `is_startup = true`) and
/// [`spawn_periodic_reconciliation_task`]'s interval loop (`is_startup =
/// false`) — otherwise identical behavior at both call sites, gated by the
/// same [`reconciliation_enabled`] kill switch.
///
/// `is_startup` (Issue #6615) is threaded down to [`plan`]/[`decide`]: on the
/// startup pass only, a `loom:building` claim with zero liveness evidence
/// whatsoever (no journal entry, no run-registry pid, no no-progress
/// checkpoint) is reclaimed immediately as
/// [`ReclaimReason::NoRecordAtStartup`] instead of waiting out the
/// [`resolve_stale_hours`] age gate — see that reason's doc comment for why
/// this is safe only immediately after a restart.
pub fn run_reconciliation_pass(fallback_root: &Path, is_startup: bool) {
    if !reconciliation_enabled() {
        log::info!("claim_reconciliation: pass disabled ({RECONCILE_ENABLED_ENV}=0)");
        return;
    }
    // Shared GitHub rate limit exhausted (#4429): every workspace's listing
    // would fail (and a rate-limited pass is indistinguishable from "nothing
    // to reclaim" in the return values), so skip the whole pass — the next
    // interval tick retries after the window resets.
    if crate::rate_limit_breaker::global_is_suppressed() {
        log::info!(
            "claim_reconciliation: pass skipped — shared GitHub API rate limit exhausted (#4429)"
        );
        return;
    }
    let workspace_registry =
        crate::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = workspace_registry.effective_roots(fallback_root);
    let gh_bin = std::path::PathBuf::from("gh");
    let mut total_checked = 0usize;
    let mut total_reclaimed = 0usize;
    let mut total_pr_checked = 0usize;
    let mut total_pr_reclaimed = 0usize;
    let mut verdict_stats = VerdictReconcileStats::default();
    let pass_kind = if is_startup { "startup" } else { "periodic" };
    for root in &roots {
        let (checked, reclaimed) = forge::reconcile_workspace(&gh_bin, root, is_startup);
        total_checked += checked;
        total_reclaimed += reclaimed;
        let (pr_checked, pr_reclaimed) = forge::reconcile_pr_claims(&gh_bin, root);
        total_pr_checked += pr_checked;
        total_pr_reclaimed += pr_reclaimed;
        verdict_stats.merge(forge::reconcile_pr_verdicts(&gh_bin, root));
    }
    if total_reclaimed > 0 {
        log::info!(
            "claim_reconciliation: {pass_kind} pass checked {total_checked} loom:building \
             issue(s) across {} workspace(s), reclaimed {total_reclaimed} stale claim(s) \
             (#4348/#6615)",
            roots.len()
        );
    } else {
        log::debug!(
            "claim_reconciliation: {pass_kind} pass checked {total_checked} loom:building \
             issue(s) across {} workspace(s), nothing to reclaim",
            roots.len()
        );
    }
    if total_pr_reclaimed > 0 {
        log::info!(
            "claim_reconciliation: PR-side pass checked {total_pr_checked} claim(s) \
             (loom:reviewing/loom:treating) across {} workspace(s), reclaimed \
             {total_pr_reclaimed} stale claim(s) (#4367)",
            roots.len()
        );
    } else {
        log::debug!(
            "claim_reconciliation: PR-side pass checked {total_pr_checked} claim(s) \
             (loom:reviewing/loom:treating) across {} workspace(s), nothing to reclaim",
            roots.len()
        );
    }
    let total_verdict_checked = verdict_stats.checked;
    if verdict_stats.invalidated > 0 {
        log::info!(
            "claim_reconciliation: verdict-staleness pass checked {total_verdict_checked} \
             verdict(s) (loom:pr/loom:changes-requested) across {} workspace(s), cleared {} \
             stale verdict(s) (#5686)",
            roots.len(),
            verdict_stats.invalidated
        );
    } else {
        log::debug!(
            "claim_reconciliation: verdict-staleness pass checked {total_verdict_checked} \
             verdict(s) (loom:pr/loom:changes-requested) across {} workspace(s), nothing stale",
            roots.len()
        );
    }
    // #6319: UNVERIFIABLE used to be a silent `Keep`. Surface it — an
    // unanchored verdict is an approval nothing can invalidate, so the
    // residual count is the number of PRs currently carrying the pre-#5686
    // hazard.
    if verdict_stats.unverifiable > 0 {
        log::warn!(
            "claim_reconciliation: verdict-staleness pass found {} verdict(s) with no \
             verdict-sha marker across {} workspace(s) — anchored {} to their current head, {} \
             still UNVERIFIABLE (a verdict nothing can invalidate; see #6319)",
            verdict_stats.unverifiable,
            roots.len(),
            verdict_stats.anchored,
            verdict_stats.residual_unverifiable()
        );
    }
}

/// Spawn the periodic reconciliation loop on the shared daemon runtime
/// (Issue #4348). Every [`resolve_reconcile_interval`] the daemon re-runs
/// [`run_reconciliation_pass`] against `fallback_root` — the SAME logic the
/// daemon already runs once at startup, just repeated for the life of the
/// process, so a `loom:building` claim whose sweep dies mid-run (an external
/// `SIGKILL` of a manually spawned detached sweep, not just a daemon
/// restart) is recovered without an operator intervening.
///
/// The first tick is deliberately skipped: the daemon just ran this exact
/// pass moments ago at startup (`main.rs`), so re-running it instantly would
/// be redundant. Each tick's `gh` calls block, so it runs on a blocking
/// thread (`tokio::task::spawn_blocking`), mirroring
/// [`crate::token_ranking_refresh::spawn_multi_token_ranking_refresh_task`].
pub fn spawn_periodic_reconciliation_task(
    fallback_root: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    // Safehouse-aware cadence (#4431): with live peer-claims carrying the
    // fast in-flight signal (re-advertised each reaper tick), a
    // safehouse-enabled host demotes this pass to a slow healing sweep.
    let interval = resolve_reconcile_interval_for(&fallback_root);
    log::info!(
        "claim_reconciliation: periodic pass enabled (interval={}s, #4348/#4431)",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first fire: the startup pass already ran
        // moments ago in `main.rs`, so re-running instantly is redundant.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let root = fallback_root.clone();
            // `is_startup = false`: this is the periodic tick, not the
            // daemon-startup pass — the #6615 immediate no-evidence fast
            // path must never fire here (see `run_reconciliation_pass`'s doc
            // comment).
            if let Err(e) =
                tokio::task::spawn_blocking(move || run_reconciliation_pass(&root, false)).await
            {
                log::error!(
                    "claim_reconciliation: periodic pass panicked ({e}); continuing to the next tick"
                );
            }
        }
    })
}

/// `gh`/label-flip glue. Not unit-tested directly (mirrors
/// [`crate::work_finder::forge`] / [`crate::epic_supervisor::forge`]) — the
/// decision logic above is the fully-covered surface; this module is a thin,
/// best-effort `Command` wrapper.
pub mod forge {
    use super::{
        apply_live_claim_veto, classify_lease_evidence, decide_anchor, decide_verdict,
        extract_latest_verdict_sha, most_recent_claim_activity_at, plan, plan_pr,
        resolve_lease_ttl_minutes, resolve_no_progress_grace_minutes, resolve_stale_hours,
        verdict_anchoring_enabled, verdict_staleness_enabled, AnchorAction, BuildingIssue,
        ClaimedPr, LeaseEvidence, NoProgressEvidence, PrClaimKind, PrClaimOutcome, PrComment,
        PrReclaimReason, PrReconcileAction, ReclaimReason, ReconcileAction, VerdictAction,
        VerdictKeepReason, VerdictKind, VerdictPr, VerdictReconcileStats, LEASE_MARKER_PREFIX,
        MAX_ISSUES_PER_WORKSPACE, VERDICT_HOLD_LABELS,
    };
    use crate::sweep_journal;
    use anyhow::{anyhow, Context, Result};
    use chrono::{DateTime, Utc};
    use serde::Deserialize;
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn list_building_issues(gh_bin: &Path, root: &Path) -> Result<Vec<BuildingIssue>> {
        // ETag-cached REST listing (#4428): an unchanged claim set costs zero
        // rate limit (304). `LOOM_REPO` precedence is handled inside; the
        // `pull_request` filter keeps the pre-#4428 issue-only semantics.
        let rows = crate::forge_listing::list_issues_cached(
            gh_bin,
            Some(root),
            None,
            "loom:building",
            "open",
        )?;
        Ok(rows
            .into_iter()
            .filter(|r| !r.is_pull_request)
            .map(|r| BuildingIssue {
                number: r.number,
                updated_at: r
                    .updated_at
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc)),
            })
            .collect())
    }

    /// Does an OPEN pull request exist for issue `issue`'s conventional branch
    /// (`feature/issue-<N>`, the name `worktree.sh` establishes)? Returns
    /// `Some(true)` when at least one open PR is found, `Some(false)` when the
    /// query definitively returns none, and `None` when the query could not be
    /// run/parsed. The Issue #4462 exit-0/no-progress reclaim treats only a
    /// definitive `Some(false)` as "no PR"; a `None` (cannot confirm) falls
    /// through to the age rule, never a spurious fast reclaim. This runs ONLY
    /// for a checkpoint already known to be stalled at the pre-Builder
    /// `curator-done` phase (see `no_progress_for` in `reconcile_workspace`),
    /// where by construction the Builder never completed — so a
    /// branch-name match is sufficient; there is no earlier-attempt PR to miss.
    fn first_open_linked_pr(gh_bin: &Path, root: &Path, issue: u32) -> Option<bool> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("list")
            .arg("--state")
            .arg("open")
            .arg("--head")
            .arg(format!("feature/issue-{issue}"))
            .arg("--json")
            .arg("number")
            .arg("--jq")
            .arg(".[].number")
            .current_dir(root)
            .stdin(Stdio::null());
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        let output = cmd.output().ok()?;
        if !output.status.success() {
            return None;
        }
        let has_open = String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|l| l.trim().parse::<u32>().is_ok());
        Some(has_open)
    }

    /// Flip an issue's claim label from `loom:building` back to
    /// `loom:issue`, then confirm the swap actually landed (#6263).
    ///
    /// **Root cause (#6263)**: `gh issue edit --remove-label X --add-label Y`
    /// is NOT atomic. Upstream `gh` (`pkg/cmd/pr/shared/editable_http.go`,
    /// `UpdateIssue`) applies label adds and removes as two *independent*
    /// GraphQL mutations (`addLabelsToLabelable` / `removeLabelsFromLabelable`)
    /// fired concurrently via an `errgroup.Group`, specifically so an
    /// unrelated label edited concurrently by another actor is not clobbered
    /// by a full-list replace — but that same design means our own two
    /// halves (remove `loom:building`, add `loom:issue`) are two separate
    /// server-side operations with no shared transaction. A zero exit proves
    /// both mutations were *accepted*, not that they landed atomically
    /// relative to a third mutation on the same issue (e.g. Curator/Champion
    /// re-adding `loom:issue` mid-flight) — this is the plausible mechanism
    /// behind #6254 carrying both `loom:issue` and `loom:building` for ~37
    /// minutes on 2026-08-15.
    ///
    /// So after the edit reports success, re-fetch the issue's current
    /// labels and verify both halves landed. If not, retry the edit exactly
    /// **once** (bounded, never an unbounded loop — the exact same idempotent
    /// `--remove-label`/`--add-label` pair, a no-op for whichever half
    /// already landed) and re-verify. A persistent mismatch after the retry
    /// is returned as an `Err` with enough detail for an operator to act —
    /// the caller already logs any `Err` at `warn` and does not count it as
    /// reclaimed.
    ///
    /// Fails OPEN on the *verification* fetch itself (a transient `gh issue
    /// view` hiccup): logs a WARN and returns `Ok(())`, treating the earlier
    /// zero-exit edit as authoritative rather than retrying indefinitely or
    /// manufacturing a false failure — consistent with this module's
    /// existing best-effort convention (mirrors `reclaim_pr`'s own
    /// best-effort label re-fetch).
    fn reclaim(gh_bin: &Path, root: &Path, issue: u32) -> Result<()> {
        reclaim_edit(gh_bin, root, issue)?;
        verify_reclaim_labels(gh_bin, root, issue)
    }

    fn reclaim_edit(gh_bin: &Path, root: &Path, issue: u32) -> Result<()> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:building")
            .arg("--add-label")
            .arg("loom:issue");
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh issue edit failed for #{issue} in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Fetch `issue`'s current label names (best-effort). Mirrors
    /// `pr_label_names` below, for the issue side.
    fn issue_label_names(gh_bin: &Path, root: &Path, issue: u32) -> Result<Vec<String>> {
        #[derive(Debug, Deserialize)]
        struct GhLabel {
            name: String,
        }
        #[derive(Debug, Deserialize)]
        struct GhIssueLabels {
            labels: Vec<GhLabel>,
        }
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("view")
            .arg(issue.to_string())
            .arg("--json")
            .arg("labels");
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh issue view {issue} --json labels failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let parsed: GhIssueLabels =
            serde_json::from_slice(&out.stdout).context("parse gh issue view labels JSON")?;
        Ok(parsed.labels.into_iter().map(|l| l.name).collect())
    }

    /// `Ok(true)` = confirmed `loom:building` absent and `loom:issue`
    /// present; `Ok(false)` = confirmed mismatch; `Err` = the label fetch
    /// itself failed (caller fails open on this case, see [`reclaim`]).
    fn reclaim_labels_confirmed(gh_bin: &Path, root: &Path, issue: u32) -> Result<bool> {
        let labels = issue_label_names(gh_bin, root, issue)?;
        let has_building = labels.iter().any(|l| l == "loom:building");
        let has_issue = labels.iter().any(|l| l == "loom:issue");
        Ok(!has_building && has_issue)
    }

    /// Post-mutation verification for [`reclaim`] (#6263) — see its doc
    /// comment for the full rationale. Bounded to exactly one retry.
    fn verify_reclaim_labels(gh_bin: &Path, root: &Path, issue: u32) -> Result<()> {
        match reclaim_labels_confirmed(gh_bin, root, issue) {
            Ok(true) => return Ok(()),
            Ok(false) => {
                log::warn!(
                    "claim_reconciliation: reclaim of #{issue} in {} appears only partially \
                     applied after `gh issue edit` reported success — retrying once (#6263)",
                    root.display(),
                );
            }
            Err(e) => {
                log::warn!(
                    "claim_reconciliation: could not verify reclaim of #{issue} in {} ({e}) — \
                     treating the earlier zero-exit `gh issue edit` as authoritative \
                     (fail-open, #6263)",
                    root.display(),
                );
                return Ok(());
            }
        }

        // Bounded retry: exactly one. Re-issuing --remove-label/--add-label
        // is idempotent for whichever half already landed.
        reclaim_edit(gh_bin, root, issue).with_context(|| {
            format!("retry of partial reclaim for #{issue} in {} failed", root.display())
        })?;

        match reclaim_labels_confirmed(gh_bin, root, issue) {
            Ok(true) => Ok(()),
            Ok(false) => Err(anyhow!(
                "reclaim of #{issue} in {} did not fully land even after one retry — \
                 loom:building/loom:issue may need a manual fix (#6263)",
                root.display(),
            )),
            Err(e) => {
                log::warn!(
                    "claim_reconciliation: could not verify retried reclaim of #{issue} in {} \
                     ({e}) — treating the retried zero-exit `gh issue edit` as authoritative \
                     (fail-open, #6263)",
                    root.display(),
                );
                Ok(())
            }
        }
    }

    /// Reconcile stale `loom:building` claims for one registered workspace
    /// `root`, using the machine-level journal as the liveness source.
    /// Best-effort and bounded: any `gh` failure is logged at `warn` and this
    /// workspace's pass returns `(0, 0)` rather than propagating an error (one
    /// repo's forge hiccup must never block the daemon's startup, nor the
    /// other registered workspaces).
    ///
    /// Returns `(checked, reclaimed)` — the number of `loom:building` issues
    /// inspected and the number actually reclaimed, for the caller's summary
    /// log line.
    ///
    /// `is_startup` (Issue #6615) is `true` only for the daemon-startup call
    /// site, `false` for the periodic one — see [`plan`]'s doc comment for
    /// what it changes.
    ///
    /// **Epic #6165 Phase 4 (#6317):** this reclamation decision no longer
    /// consults the peer-claim/safehouse advertisement channel at all — the
    /// lease-freshness gate below (Issue #6286) is the sole fleet-scoped
    /// authority. An earlier gate (Issue #6157) additionally froze reclaim
    /// while peer coordination looked DEGRADED (sustained advertising with
    /// no receive); that gate has been removed, because a healthy-vs-
    /// degraded *advertisement channel* was never meant to be load-bearing
    /// for correctness in the first place (`peer_claims.rs`'s own "soft
    /// claim, not a mutex" framing, and this epic's own root-cause finding).
    /// The peer-claim channel keeps its #4028 fast-backoff role at dispatch
    /// time ([`crate::sweep_registry::SweepRegistry::dispatch`]'s
    /// `peer_claimed_issues` check) — advisory only, never consulted here.
    pub fn reconcile_workspace(gh_bin: &Path, root: &Path, is_startup: bool) -> (usize, usize) {
        let repo = root.display().to_string();

        let issues = match list_building_issues(gh_bin, root) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("claim_reconciliation: {}: {e}", root.display());
                crate::rate_limit_breaker::global_observe_failure(
                    &e.to_string(),
                    "claim_reconciliation",
                );
                return (0, 0);
            }
        };
        if issues.is_empty() {
            return (0, 0);
        }

        let journal_path = match sweep_journal::default_journal_path() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("claim_reconciliation: cannot resolve journal path: {e}");
                return (0, 0);
            }
        };
        // IMPORTANT (#3975): decide FIRST against the raw, unpruned journal.
        // A prior version pruned dead-PID entries here before calling
        // `plan()`/`decide()` -- which erased the exact evidence the
        // `DeadPid` branch needs to fire its *unconditional*, immediate
        // reclaim. With the entry gone, every claim silently fell through
        // to the `NoRecordStale` branch and its much longer
        // `stale_hours` grace period, so a claim whose sweep had *just*
        // died (recent label, dead PID) was wrongly kept for hours instead
        // of reclaimed right away. Pruning now happens only *after*
        // deciding, via the per-reclaimed-issue `remove_sweep` below --
        // dead entries for issues this pass didn't touch are cleaned up
        // lazily by the next `record_sweep`/`upsert` elsewhere (which
        // still prunes the whole journal), so the file never accumulates
        // an unbounded graveyard.
        let journal = sweep_journal::load(&journal_path);

        let stale_hours = resolve_stale_hours();
        let no_progress_grace_minutes = resolve_no_progress_grace_minutes();
        let now = chrono::Utc::now();
        // Issue #4348: checkpoint->run-registry join, only ever consulted by
        // `plan()` for an issue with no journal entry.
        let run_registry_pid_for = |issue: u32| super::resolve_run_registry_pid(root, issue);
        // Issue #4462 (refined by #4616): exit-0/no-progress evidence,
        // consulted by `plan()` only for an issue with no journal entry AND no
        // run-registry pid (see `plan`'s ordering). A checkpoint stalled at
        // the pre-Builder `curator-done` phase with no open linked PR is a
        // no-progress candidate -- `decide` gates the actual reclaim on the
        // checkpoint's own timestamp clearing `no_progress_grace_minutes`, so
        // a legitimately-just-resumed Builder retry (same evidence shape for
        // the first few minutes) is not misfired on. Ordered cheap-check-first:
        // the phase read is a local file, so the (gh-querying) open-PR check
        // runs only for a genuinely no-progress checkpoint. Fails safe: any
        // inability to confirm -> None -> age gate.
        let no_progress_for = |issue: u32| -> Option<NoProgressEvidence> {
            if super::read_checkpoint_phase(root, issue).as_deref()
                != Some(super::NO_PROGRESS_PHASE)
            {
                return None;
            }
            if first_open_linked_pr(gh_bin, root, issue) != Some(false) {
                return None;
            }
            super::read_checkpoint_timestamp(root, issue).map(|checkpoint_timestamp| {
                NoProgressEvidence {
                    checkpoint_timestamp,
                }
            })
        };
        let decisions = plan(
            &repo,
            &issues,
            &journal,
            &run_registry_pid_for,
            &no_progress_for,
            no_progress_grace_minutes,
            &crate::sweep_registry::is_pid_alive,
            stale_hours,
            now,
            is_startup,
        );
        let checked = decisions.len();

        // Issue #6286 (Epic #6165 Phase 2): resolved once per pass, not per
        // issue — a mid-pass config flip (extremely unlikely inside one
        // bounded `gh`-call pass, but possible) must not apply a different
        // TTL to half the pass than the other half.
        let lease_ttl_minutes = resolve_lease_ttl_minutes();

        let mut reclaimed = 0usize;
        for (issue_number, action) in decisions {
            let ReconcileAction::Reclaim(reason) = action else {
                continue;
            };
            // Issue #6286 (Epic #6165 Phase 2): consult the claim's lease
            // record — the fleet-scoped liveness source Phase 1 (#6179/
            // #6180) writes and renews — before trusting any of the
            // HOST-scoped evidence `plan()` used above (journal, checkpoint→
            // run-registry join, label-age grace period; see this module's
            // top doc comment). A fresh lease is positive, fleet-scoped
            // evidence the claim's holder is alive, so the reclaim is
            // refused regardless of what the (possibly down, or simply
            // unconfigured) peer-claim/safehouse channel reports — that
            // channel is no longer consulted here at all (Epic #6165 Phase
            // 4, #6317): this lease check is the sole fleet-scoped gate.
            //
            // Issue #6320: classify the probe's result (absent / fresh /
            // stale) and carry it into BOTH the refusal and the reclaim log
            // lines, so an operator reading `daemon.log` after an unattended
            // reclaim can tell "the lease expired" from "there was never a
            // lease" — the exact distinction #6320 could only infer from a
            // timeline.
            let lease_evidence = classify_lease_evidence(
                fetch_freshest_lease_updated_at(gh_bin, root, issue_number),
                now,
                lease_ttl_minutes,
            );
            if matches!(lease_evidence, LeaseEvidence::Fresh { .. }) {
                log::warn!(
                    "claim_reconciliation: REFUSING to reclaim #{issue_number} in {} — \
                     {lease_evidence}, within the {lease_ttl_minutes}m TTL — reclaim reason \
                     that would have fired: {reason:?} (#6286)",
                    root.display(),
                );
                continue;
            }
            // #4556 live-claim veto: a dead *recorded* pid is not proof the sweep
            // is gone. Probe for a confirmed-live claim (live lock owner / live
            // machine-level journal record / live `/loom:sweep <N>` process) and
            // leave the label alone if one exists — reverting `loom:building` out
            // from under a working sweep is what re-exposed #4275 to the
            // work-finder and started its seven-dispatch storm. Probed only on a
            // Reclaim decision, so a healthy pass pays nothing.
            //
            // Judge note (#4605): pass this pass's already-resolved
            // `journal_path` rather than `None`. `None` would make the probe's
            // leg 2 re-resolve the process-global default, which silently
            // diverges from — and could disagree with — the very journal
            // `plan()` decided against whenever a workspace overrides the path.
            let live_claim =
                crate::live_claim::probe(root, Some(journal_path.as_path()), issue_number);
            if matches!(apply_live_claim_veto(action, live_claim.as_ref()), ReconcileAction::Keep) {
                log::warn!(
                    "claim_reconciliation: NOT reclaiming #{issue_number} in {} despite \
                     {reason:?} — the issue still has {} (#4556 live-claim veto)",
                    root.display(),
                    live_claim.map_or_else(|| "a live claim".to_string(), |e| e.to_string()),
                );
                continue;
            }
            match reclaim(gh_bin, root, issue_number) {
                Ok(()) => {
                    reclaimed += 1;
                    // #4348 acceptance criterion: WARN-level, with the
                    // issue number, last-known pid, and (when the evidence
                    // was the run-registry join) the run id -- an operator
                    // reading `daemon.log` after an unattended reclaim needs
                    // this without cross-referencing the journal by hand.
                    let last_known_pid = match reason {
                        ReclaimReason::DeadPid { pid } | ReclaimReason::DeadRunRegistry { pid } => {
                            Some(pid)
                        }
                        ReclaimReason::ExitedNoProgress
                        | ReclaimReason::NoRecordStale { .. }
                        | ReclaimReason::NoRecordAtStartup => None,
                    };
                    let run_id = matches!(reason, ReclaimReason::DeadRunRegistry { .. })
                        .then(|| super::read_checkpoint_task_id(root, issue_number))
                        .flatten();
                    // #6320: `{lease_evidence}` distinguishes "the lease
                    // expired" from "no lease was ever published" (e.g. an
                    // in-session `/loom:sweep` on a Loom old enough to
                    // predate `sweep-lease-publish.sh`) — without it, the
                    // two are indistinguishable in the log.
                    log::warn!(
                        "claim_reconciliation: reclaimed loom:building -> loom:issue for #{issue_number} \
                         in {} ({reason:?}, {lease_evidence}, last_known_pid={last_known_pid:?}, run_id={run_id:?})",
                        root.display(),
                    );
                    // Best-effort tidy-up: drop the (now stale-or-absent)
                    // journal entry so the next pass doesn't re-derive it.
                    let _ = sweep_journal::remove_sweep(&repo, issue_number);
                }
                Err(e) => {
                    log::warn!(
                        "claim_reconciliation: failed to reclaim #{issue_number} in {}: {e}",
                        root.display()
                    );
                }
            }
        }

        (checked, reclaimed)
    }

    // ------------------------------------------------------------------
    // PR-side claim labels: loom:reviewing / loom:treating (Issue #4367)
    // ------------------------------------------------------------------

    #[derive(Debug, Deserialize)]
    struct GhClaimedPr {
        number: u32,
        #[serde(rename = "updatedAt", default)]
        updated_at: Option<String>,
        #[serde(rename = "headRefName", default)]
        head_ref_name: Option<String>,
    }

    fn list_prs_with_label(gh_bin: &Path, root: &Path, label: &str) -> Result<Vec<ClaimedPr>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("list")
            .arg("--state")
            .arg("open")
            .arg("--label")
            .arg(label)
            .arg("--limit")
            .arg(MAX_ISSUES_PER_WORKSPACE.to_string())
            .arg("--json")
            .arg("number,updatedAt,headRefName");
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr list --label {label} failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let rows: Vec<GhClaimedPr> =
            serde_json::from_slice(&out.stdout).context("parse gh pr list JSON")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let claim_labeled_at = fetch_claim_labeled_at(gh_bin, root, r.number, label);
                // Issue #4638: only worth fetching when there is a
                // claim_labeled_at to compare against -- with no anchor at
                // all, decide_pr falls back to updated_at, which GitHub
                // already keeps at least as fresh as any comment. (#6523: the
                // claim-activity marker is keyed on this same timestamp, so
                // without it there is nothing to match against either.)
                let most_recent_claim_activity_at = claim_labeled_at.and_then(|since| {
                    fetch_most_recent_claim_activity_at(gh_bin, root, r.number, since)
                });
                ClaimedPr {
                    number: r.number,
                    updated_at: r
                        .updated_at
                        .as_deref()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc)),
                    claim_labeled_at,
                    most_recent_claim_activity_at,
                    head_ref_name: r.head_ref_name,
                }
            })
            .collect())
    }

    /// Best-effort fetch of the most recent `labeled <label>` timeline event
    /// for `pr_number` (Issue #4618) — the freshness signal [`decide_pr`]
    /// prefers over the PR's aggregate `updatedAt`, since a stand-down
    /// comment bumps the latter but never re-applies the label. Mirrors
    /// [`crate::quarantine_reconciliation::forge::fetch_last_blocked_labeled_at`]'s
    /// shape. Returns `None` on any failure/timeout/unparseable-output or
    /// when the label was never applied — callers fall back to `updatedAt`
    /// in that case, the same fail-open posture used elsewhere in this
    /// module.
    fn fetch_claim_labeled_at(
        gh_bin: &Path,
        root: &Path,
        pr_number: u32,
        label: &str,
    ) -> Option<DateTime<Utc>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/timeline"))
            .arg("--paginate")
            .arg("--jq")
            .arg(format!(
                r#"[.[] | select(.event == "labeled" and .label.name == "{label}") | .created_at] | max // empty"#
            ));
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        parse_max_timestamp(&out.stdout)
    }

    /// Parse a `gh api --paginate --jq '... | max // empty'` result into the
    /// maximum RFC-3339 timestamp across every line of output. `--paginate`
    /// re-invokes the `--jq` filter once per page and concatenates the
    /// per-page results rather than applying the filter across the combined
    /// set (Issue #4637) — on a timeline spanning more than one page (>100
    /// events) this yields one `max`-per-page line, not a single overall
    /// max. Each non-empty/non-`null` line (bare or JSON-quoted) is parsed
    /// independently and the maximum across all lines is returned; a
    /// single-line result (the common case) is handled identically to
    /// before. Returns `None` when there is no parseable timestamp on any
    /// line — the same fail-open contract as the pre-#4637 single-line
    /// parse.
    pub(crate) fn parse_max_timestamp(stdout: &[u8]) -> Option<DateTime<Utc>> {
        let raw = String::from_utf8_lossy(stdout);
        raw.lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed == "null" {
                    return None;
                }
                let unquoted = trimmed.trim_matches('"');
                chrono::DateTime::parse_from_rfc3339(unquoted)
                    .ok()
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            })
            .max()
    }

    /// Best-effort fetch of the freshest `updated_at` among `issue_number`'s
    /// lease-record comments (Issue #6179's `LEASE_MARKER_PREFIX` marker) —
    /// the fleet-scoped liveness evidence Epic #6165 Phase 2 (#6286)
    /// consults as the FINAL gate before reclaiming a `loom:building` claim.
    ///
    /// Uses the REST comments endpoint (`.../issues/<N>/comments`), not `gh
    /// issue view --json comments` — the latter exposes `createdAt` but not
    /// `updatedAt` at all, and the renewal loop's idempotent PATCH
    /// (`defaults/docs/lease-renewal.md`) only ever changes a comment's
    /// `updated_at`, never creates a new comment. `--paginate` mirrors
    /// [`fetch_claim_labeled_at`]'s handling of a multi-page result (see
    /// [`parse_max_timestamp`]'s doc comment) — irrelevant in the overwhelming
    /// common case (one lease comment per issue) but correct regardless.
    ///
    /// Returns `None` when there is no lease comment at all (a claim
    /// predating this feature, or a lease write that failed) or the query
    /// itself failed. Per `defaults/docs/lease-record.md`, callers MUST NOT
    /// treat `None` as evidence of anything either way — see
    /// [`lease_is_fresh`]'s doc comment for the corresponding "found but not
    /// fresh" case this leaves to the caller.
    pub(crate) fn fetch_freshest_lease_updated_at(
        gh_bin: &Path,
        root: &Path,
        issue_number: u32,
    ) -> Option<DateTime<Utc>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{issue_number}/comments"))
            .arg("--paginate")
            .arg("--jq")
            .arg(format!(
                r#"[.[] | select(.body | startswith("{LEASE_MARKER_PREFIX}")) | .updated_at] | max // empty"#
            ));
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        parse_max_timestamp(&out.stdout)
    }

    /// One row of `gh pr view --json comments`, trimmed to the fields the
    /// claim-activity scan needs. Both fields are optional so a partial /
    /// unexpected payload degrades to "not claimant activity" rather than
    /// failing the whole fetch.
    #[derive(Debug, Deserialize)]
    struct GhPrComment {
        #[serde(rename = "createdAt", default)]
        created_at: Option<String>,
        #[serde(default)]
        body: Option<String>,
    }

    /// Best-effort fetch of the most recent **claimant activity** comment
    /// posted on `pr_number` after `since` (Issue #4638, narrowed by #6523) —
    /// [`ClaimedPr::most_recent_claim_activity_at`], the evidence
    /// [`decide_pr`]'s age gate uses alongside `claim_labeled_at` to avoid
    /// reclaiming a genuinely live, non-pid-joinable claimant.
    ///
    /// The `gh` call only *narrows* (comments posted after `since`, to bound
    /// the payload); the decision itself is the pure
    /// [`most_recent_claim_activity_at`], so the exact rule that ships is the
    /// one the unit tests exercise. Returns `None` on any
    /// failure/timeout/unparseable-output, or when no comment since `since`
    /// carried this claim's [`super::claim_activity_marker`] — callers then
    /// fall back to `claim_labeled_at`/`updated_at` alone, preserving the
    /// #4618 regression guard (a claim with no claimant heartbeat since must
    /// still age out and be reclaimed).
    fn fetch_most_recent_claim_activity_at(
        gh_bin: &Path,
        root: &Path,
        pr_number: u32,
        since: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        // Render `since` in exactly the shape the forge emits for `createdAt`
        // (`...Z`, second precision) so the jq `>` comparison — which is a raw
        // *string* comparison — orders correctly. `to_rfc3339()` would render
        // the same instant with a `+00:00` offset suffix, which sorts *before*
        // a `Z`-suffixed timestamp of the identical second and so would
        // misclassify a comment posted in the same second as the claim label.
        // (This is also the exact rendering `claim_activity_marker` embeds.)
        let since_iso = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("view")
            .arg(pr_number.to_string())
            .arg("--json")
            .arg("comments")
            .arg("--jq")
            .arg(format!(
                r#"[.comments[] | select(.createdAt > "{since_iso}") | {{createdAt, body}}]"#
            ));
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "null" {
            return None;
        }
        let rows: Vec<GhPrComment> = serde_json::from_str(trimmed).ok()?;
        let comments: Vec<PrComment> = rows
            .into_iter()
            .filter_map(|r| {
                let created_at = chrono::DateTime::parse_from_rfc3339(r.created_at.as_deref()?)
                    .ok()?
                    .with_timezone(&chrono::Utc);
                Some(PrComment {
                    created_at,
                    body: r.body.unwrap_or_default(),
                })
            })
            .collect();
        most_recent_claim_activity_at(&comments, since)
    }

    /// The PR's currently-applied state labels (a best-effort subset of
    /// `--json labels`, used only to decide whether [`reclaim_pr`]'s safety
    /// net needs to fire). A `gh` failure here degrades to an empty list —
    /// see [`reclaim_pr`]'s call site for why that is the safe direction:
    /// it makes the safety net fire (adds `loom:review-requested`) rather
    /// than silently leaving a PR with no state label at all.
    fn pr_label_names(gh_bin: &Path, root: &Path, pr_number: u32) -> Result<Vec<String>> {
        #[derive(Debug, Deserialize)]
        struct GhLabel {
            name: String,
        }
        #[derive(Debug, Deserialize)]
        struct GhPrLabels {
            labels: Vec<GhLabel>,
        }
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("view")
            .arg(pr_number.to_string())
            .arg("--json")
            .arg("labels");
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr view {pr_number} --json labels failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let parsed: GhPrLabels =
            serde_json::from_slice(&out.stdout).context("parse gh pr view labels JSON")?;
        Ok(parsed.labels.into_iter().map(|l| l.name).collect())
    }

    fn add_label(gh_bin: &Path, root: &Path, pr_number: u32, label: &str) -> Result<()> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("edit")
            .arg(pr_number.to_string())
            .arg("--add-label")
            .arg(label);
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr edit --add-label {label} failed for #{pr_number} in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Reclaim one stale PR-side claim: remove `claim_label`, then — as a
    /// safety net — add `loom:review-requested` if the PR is left with none
    /// of the three state labels (`loom:review-requested`,
    /// `loom:changes-requested`, `loom:pr`). The safety-net check is
    /// best-effort: a failure to read the PR's current labels defaults to
    /// treating it as unlabeled (adds `loom:review-requested`) rather than
    /// leaving a PR that might genuinely have no state label undiscoverable.
    fn reclaim_pr(gh_bin: &Path, root: &Path, pr_number: u32, claim_label: &str) -> Result<()> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("edit")
            .arg(pr_number.to_string())
            .arg("--remove-label")
            .arg(claim_label);
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr edit --remove-label {claim_label} failed for #{pr_number} in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        const STATE_LABELS: [&str; 3] =
            ["loom:review-requested", "loom:changes-requested", "loom:pr"];
        let current_labels = pr_label_names(gh_bin, root, pr_number).unwrap_or_default();
        let has_state_label = current_labels
            .iter()
            .any(|l| STATE_LABELS.contains(&l.as_str()));
        if !has_state_label {
            if let Err(e) = add_label(gh_bin, root, pr_number, "loom:review-requested") {
                log::warn!(
                    "claim_reconciliation: reclaimed {claim_label} from PR #{pr_number} in {} \
                     but failed to backfill loom:review-requested: {e}",
                    root.display()
                );
            }
        }
        Ok(())
    }

    /// Reconcile stale `loom:reviewing`/`loom:treating` claims for one
    /// registered workspace `root` (Issue #4367), using the same machine-level
    /// journal + checkpoint→run-registry join as [`reconcile_workspace`]. Best
    /// effort and bounded exactly like the issue-side pass: any `gh` failure
    /// for one claim label is logged at `warn` and that label's sweep
    /// contributes `(0, 0)` rather than propagating an error.
    ///
    /// Returns `(checked, reclaimed)` summed across both claim labels.
    pub fn reconcile_pr_claims(gh_bin: &Path, root: &Path) -> (usize, usize) {
        let (checked, outcomes) = reconcile_pr_claims_report(gh_bin, root, true);
        let reclaimed = outcomes.iter().filter(|o| o.reclaimed).count();
        (checked, reclaimed)
    }

    /// Detailed variant of [`reconcile_pr_claims`] (Issue #6167): runs the
    /// identical detection pass (same [`plan_pr`]/[`super::decide_pr`]
    /// staleness + liveness discipline, same journal/run-registry join
    /// priority) but returns one [`PrClaimOutcome`] per PR-side claim decided
    /// [`PrReconcileAction::Reclaim`] instead of only a summary count —
    /// letting a non-daemon caller (the `recover-orphans` CLI) report what
    /// would be reclaimed without necessarily reclaiming it.
    ///
    /// `recover=false` performs the identical detection pass with **no**
    /// `gh pr edit` calls — every [`PrClaimOutcome::reclaimed`] is `false` —
    /// mirroring the issue-side `recover-orphans` dry-run contract
    /// ([`crate::worktree_ops::orphan_recovery::run_orphan_recovery`]).
    /// `recover=true` reproduces [`reconcile_pr_claims`]'s original
    /// behavior exactly (it is now this function's thin summary wrapper).
    ///
    /// Returns `(checked, outcomes)` — `checked` sums the number of PRs
    /// evaluated across both claim labels; `outcomes` holds only the ones
    /// decided `Reclaim` (a `Keep` decision produces no entry, matching the
    /// issue-side `orphaned`-only reporting convention).
    pub fn reconcile_pr_claims_report(
        gh_bin: &Path,
        root: &Path,
        recover: bool,
    ) -> (usize, Vec<PrClaimOutcome>) {
        let repo = root.display().to_string();

        let journal_path = match sweep_journal::default_journal_path() {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "claim_reconciliation: cannot resolve journal path for PR-side pass: {e}"
                );
                return (0, Vec::new());
            }
        };
        let journal = sweep_journal::load(&journal_path);
        let run_registry_pid_for = |issue: u32| super::resolve_run_registry_pid(root, issue);
        let now = chrono::Utc::now();

        let mut total_checked = 0usize;
        let mut outcomes = Vec::new();

        for kind in [PrClaimKind::Reviewing, PrClaimKind::Treating] {
            let label = kind.label();
            let prs = match list_prs_with_label(gh_bin, root, label) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("claim_reconciliation: {}: {e}", root.display());
                    crate::rate_limit_breaker::global_observe_failure(
                        &e.to_string(),
                        "claim_reconciliation",
                    );
                    continue;
                }
            };
            if prs.is_empty() {
                continue;
            }

            let stale_minutes = kind.stale_minutes();
            let decisions = plan_pr(
                &repo,
                &prs,
                &journal,
                &run_registry_pid_for,
                &crate::sweep_registry::is_pid_alive,
                stale_minutes,
                now,
            );
            total_checked += decisions.len();

            for (pr_number, action) in decisions {
                let PrReconcileAction::Reclaim(reason) = action else {
                    continue;
                };
                let reclaimed = if recover {
                    match reclaim_pr(gh_bin, root, pr_number, label) {
                        Ok(()) => {
                            let last_known_pid = match reason {
                                PrReclaimReason::DeadPid { pid }
                                | PrReclaimReason::DeadRunRegistry { pid } => Some(pid),
                                PrReclaimReason::Aged { .. } => None,
                            };
                            log::warn!(
                                "claim_reconciliation: removed stale {label} from PR #{pr_number} \
                                 in {} ({reason:?}, last_known_pid={last_known_pid:?}) (#4367)",
                                root.display(),
                            );
                            true
                        }
                        Err(e) => {
                            log::warn!(
                                "claim_reconciliation: failed to reclaim {label} from PR \
                                 #{pr_number} in {}: {e}",
                                root.display()
                            );
                            false
                        }
                    }
                } else {
                    false
                };
                outcomes.push(PrClaimOutcome {
                    pr_number,
                    label,
                    reason,
                    reclaimed,
                });
            }
        }

        (total_checked, outcomes)
    }

    // ------------------------------------------------------------------
    // PR-side verdict labels: loom:pr / loom:changes-requested (Issue #5686)
    // ------------------------------------------------------------------

    #[derive(Debug, Deserialize)]
    struct GhVerdictPr {
        number: u32,
        #[serde(rename = "headRefOid", default)]
        head_ref_oid: Option<String>,
        #[serde(default)]
        labels: Vec<GhVerdictLabel>,
    }

    #[derive(Debug, Deserialize)]
    struct GhVerdictLabel {
        name: String,
    }

    #[derive(Debug, Deserialize)]
    struct GhIssueComment {
        #[serde(default)]
        body: Option<String>,
    }

    /// Every comment body on `pr_number`, oldest first. Returns `None` on any
    /// failure — [`decide_verdict`] then sees `marker_sha: None` and fails
    /// safe to `Keep(Unverifiable)`, never a spurious invalidation.
    ///
    /// `--paginate` is REQUIRED: without it only the first page (default
    /// per_page=30, oldest-first) comes back, and the verdict marker is always
    /// among the NEWEST comments — the same pitfall #5455 documented for the
    /// fallback-queue marker scan.
    fn fetch_comment_bodies(gh_bin: &Path, root: &Path, pr_number: u32) -> Option<Vec<String>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("api")
            .arg(format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/comments"))
            .arg("--paginate");
        cmd.current_dir(root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let rows: Vec<GhIssueComment> = serde_json::from_slice(&out.stdout).ok()?;
        Some(rows.into_iter().filter_map(|c| c.body).collect())
    }

    fn list_verdict_prs(gh_bin: &Path, root: &Path, kind: VerdictKind) -> Result<Vec<VerdictPr>> {
        let label = kind.label();
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("list")
            .arg("--state")
            .arg("open")
            .arg("--label")
            .arg(label)
            .arg("--limit")
            .arg(MAX_ISSUES_PER_WORKSPACE.to_string())
            .arg("--json")
            .arg("number,headRefOid,labels");
        cmd.current_dir(root);
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr list --label {label} failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let rows: Vec<GhVerdictPr> =
            serde_json::from_slice(&out.stdout).context("parse gh pr list JSON")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let on_hold = r
                    .labels
                    .iter()
                    .any(|l| VERDICT_HOLD_LABELS.contains(&l.name.as_str()));
                // A held PR is never invalidated, so skip its comment fetch
                // entirely — the decision cannot change and the call costs
                // rate limit for nothing. `marker_scan_ok` records that the
                // skip happened, so the anchoring pass (#6319) cannot mistake
                // "not looked at" for "confirmed unmarked".
                let (marker_sha, marker_scan_ok) = if on_hold {
                    (None, false)
                } else {
                    match fetch_comment_bodies(gh_bin, root, r.number) {
                        Some(bodies) => (extract_latest_verdict_sha(&bodies, kind), true),
                        None => (None, false),
                    }
                };
                VerdictPr {
                    number: r.number,
                    kind,
                    head_sha: r.head_ref_oid,
                    marker_sha,
                    marker_scan_ok,
                    on_hold,
                }
            })
            .collect())
    }

    /// Clear one stale verdict: post the auditable old->new SHA comment, then
    /// swap the verdict label (plus its per-tree companions) for
    /// `loom:review-requested`.
    ///
    /// The comment goes FIRST, deliberately: if the label write then fails,
    /// the PR keeps a verdict that is at least explained, rather than getting
    /// silently re-queued with no record of why. A failed comment aborts
    /// before touching any label, so the transition is never applied without
    /// its audit trail.
    fn invalidate_verdict(
        gh_bin: &Path,
        root: &Path,
        pr: &VerdictPr,
        marker_sha: &str,
        head_sha: &str,
    ) -> Result<()> {
        let label = pr.kind.label();
        let body = format!(
            "<!-- loom:verdict-stale from={marker_sha} to={head_sha} -->\n\
             **Stale review verdict cleared — head SHA moved**\n\n\
             This PR's `{label}` verdict was rendered against `{marker_sha}`, but the current \
             head is `{head_sha}`. A review verdict is a statement about a specific tree, so it \
             does not survive a rebase, a force-push, or new commits.\n\n\
             - Verdict cleared: `{label}` (recorded for `{marker_sha}`)\n\
             - Returned to the review queue: `loom:review-requested` (current head `{head_sha}`)\n\n\
             Judge will re-evaluate the tree that is actually here now. No judgment about the new \
             tree is implied either way — the old verdict simply no longer describes it.\n\n\
             ---\n\
             *Automated by loom-daemon claim reconciliation (#5686)*"
        );

        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("comment")
            .arg(pr.number.to_string())
            .arg("--body")
            .arg(&body);
        cmd.current_dir(root);
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr comment failed for #{} in {}: {}",
                pr.number,
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        // `loom:ci-failure` / `loom:merge-conflict` are findings about the OLD
        // tree too — they ride along with the verdict they were applied
        // beside, so they go with it.
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("edit")
            .arg(pr.number.to_string())
            .arg("--remove-label")
            .arg(label)
            .arg("--remove-label")
            .arg("loom:ci-failure")
            .arg("--remove-label")
            .arg("loom:merge-conflict")
            .arg("--add-label")
            .arg("loom:review-requested");
        cmd.current_dir(root);
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr edit (clear {label}) failed for #{} in {}: {}",
                pr.number,
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Anchor one unmarked verdict to the PR's current head (Issue #6319):
    /// post a comment carrying the `<!-- loom:verdict-sha ... -->` marker
    /// judge.md was supposed to write, so the verdict becomes invalidatable
    /// by the ordinary staleness path from here on.
    ///
    /// **No label is touched.** This is the whole safety argument: the
    /// verdict label was already there and stays exactly as it was, so
    /// anchoring cannot approve, reject, or un-park anything. The only state
    /// it changes is "this verdict can now be checked".
    ///
    /// Idempotent by construction — the marker it posts is precisely what
    /// [`extract_latest_verdict_sha`] scans for, so the next pass reads the
    /// verdict as `Fresh` and never anchors it twice.
    fn anchor_verdict(gh_bin: &Path, root: &Path, pr: &VerdictPr, head_sha: &str) -> Result<()> {
        let label = pr.kind.label();
        let token = pr.kind.marker_token();
        let body = format!(
            "<!-- loom:verdict-sha sha={head_sha} verdict={token} -->\n\
             **Verdict anchored to the current head — no marker had been recorded**\n\n\
             This PR carries `{label}`, but no verdict-SHA marker was ever written for that \
             verdict, so it was **unverifiable**: nothing could tell whether it still described \
             the tree in front of it, and it would have survived a force-push undetected — the \
             exact pre-#5686 hazard.\n\n\
             This comment records the head SHA as of now, `{head_sha}`. It is **not** a review \
             and implies no judgment about this tree: the `{label}` label is unchanged. From \
             here on the verdict is invalidatable — if the head moves off `{head_sha}`, the \
             stale-verdict pass clears `{label}` and returns the PR to `loom:review-requested`.\n\n\
             Anchoring bounds future exposure; it cannot reconstruct which tree was actually \
             reviewed. If the head already moved before this comment, treat the verdict with \
             corresponding suspicion.\n\n\
             ---\n\
             *Automated by loom-daemon claim reconciliation (#6319)*"
        );

        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("comment")
            .arg(pr.number.to_string())
            .arg("--body")
            .arg(&body);
        cmd.current_dir(root);
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr comment (anchor {label}) failed for #{} in {}: {}",
                pr.number,
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Reconcile stale `loom:pr` / `loom:changes-requested` verdicts for one
    /// registered workspace `root` (Issue #5686) — the always-on daemon
    /// backstop behind judge.md's Stale-Verdict Sweep, doctor.md's
    /// Stale-Verdict Check, and champion-pr-merge.md's Verdict-State Janitor
    /// Part 2. Those are the fast paths (they only fire when an agent happens
    /// to look at the PR); this one runs on the periodic tick regardless.
    ///
    /// Inert by construction on any verdict written before the marker
    /// convention shipped: no marker means `Keep(Unverifiable)`, never a
    /// clear. Best effort and bounded exactly like the claim-side passes —
    /// any `gh` failure is logged at `warn` and contributes nothing.
    ///
    /// An unmarked verdict is additionally **counted and anchored** (Issue
    /// #6319) rather than silently kept: see [`decide_anchor`] /
    /// [`anchor_verdict`]. Anchoring writes no labels, so the staleness
    /// behavior of every already-marked verdict is untouched.
    ///
    /// Returns [`VerdictReconcileStats`] summed across both verdict labels.
    pub fn reconcile_pr_verdicts(gh_bin: &Path, root: &Path) -> VerdictReconcileStats {
        let mut stats = VerdictReconcileStats::default();
        if !verdict_staleness_enabled() {
            return stats;
        }
        let anchoring = verdict_anchoring_enabled();

        for kind in [VerdictKind::Approved, VerdictKind::ChangesRequested] {
            let prs = match list_verdict_prs(gh_bin, root, kind) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("claim_reconciliation (verdicts): {}: {e}", root.display());
                    crate::rate_limit_breaker::global_observe_failure(
                        &e.to_string(),
                        "claim_reconciliation",
                    );
                    continue;
                }
            };
            stats.checked += prs.len();

            for pr in prs {
                match decide_verdict(&pr) {
                    VerdictAction::Invalidate {
                        marker_sha,
                        head_sha,
                    } => match invalidate_verdict(gh_bin, root, &pr, &marker_sha, &head_sha) {
                        Ok(()) => {
                            stats.invalidated += 1;
                            log::warn!(
                                "claim_reconciliation: cleared stale {} from PR #{} in {} \
                                 (verdict recorded for {marker_sha}, head is now {head_sha}) — \
                                 re-queued as loom:review-requested (#5686)",
                                pr.kind.label(),
                                pr.number,
                                root.display(),
                            );
                        }
                        Err(e) => {
                            log::warn!(
                                "claim_reconciliation: failed to clear stale {} from PR #{} in \
                                 {}: {e}",
                                pr.kind.label(),
                                pr.number,
                                root.display()
                            );
                        }
                    },
                    VerdictAction::Keep(VerdictKeepReason::Unverifiable) => {
                        // Count only what we POSITIVELY know is unanchored. A
                        // held PR's comments are never fetched and a failed
                        // fetch looks identical to "no marker" — folding
                        // either into the counter would turn an API outage
                        // into a fake integrity alarm.
                        if !pr.marker_scan_ok {
                            continue;
                        }
                        stats.unverifiable += 1;
                        log::warn!(
                            "claim_reconciliation: PR #{} in {} carries {} with NO verdict-sha \
                             marker — the verdict is UNVERIFIABLE and would survive a \
                             force-push undetected (#6319)",
                            pr.number,
                            root.display(),
                            pr.kind.label(),
                        );
                        if !anchoring {
                            continue;
                        }
                        let AnchorAction::Anchor { head_sha } = decide_anchor(&pr) else {
                            continue;
                        };
                        match anchor_verdict(gh_bin, root, &pr, &head_sha) {
                            Ok(()) => {
                                stats.anchored += 1;
                                log::info!(
                                    "claim_reconciliation: anchored PR #{}'s {} verdict to \
                                     {head_sha} in {} — it is now invalidatable by the ordinary \
                                     staleness pass (#6319)",
                                    pr.number,
                                    pr.kind.label(),
                                    root.display(),
                                );
                            }
                            Err(e) => {
                                log::warn!(
                                    "claim_reconciliation: failed to anchor PR #{}'s {} verdict \
                                     in {}: {e} — it stays UNVERIFIABLE until the next tick",
                                    pr.number,
                                    pr.kind.label(),
                                    root.display()
                                );
                            }
                        }
                    }
                    VerdictAction::Keep(_) => {}
                }
            }
        }

        stats
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serial_test::serial;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn issue(number: u32, updated_at: Option<DateTime<Utc>>) -> BuildingIssue {
        BuildingIssue { number, updated_at }
    }

    fn journal_entry(repo: &str, issue: u32, pid: u32) -> JournalEntry {
        JournalEntry {
            repo: repo.to_string(),
            issue,
            pid,
            started_at: Utc::now(),
        }
    }

    #[test]
    fn decide_keeps_when_journal_entry_pid_alive() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action =
            decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| true, 4.0, now, false);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_journal_entry_pid_dead() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action =
            decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| false, 4.0, now, false);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    // --- #4556 live-claim veto ---

    fn live_lock_evidence() -> crate::live_claim::LiveClaimEvidence {
        crate::live_claim::LiveClaimEvidence::ClaimLock {
            pid: 111,
            sweep_id: "sweep-issue-4275-live".to_string(),
        }
    }

    #[test]
    fn live_claim_veto_downgrades_a_dead_pid_reclaim_to_keep() {
        // The confirmed #4275 misfire: `DeadPid { pid: 2781227 }` reverted
        // loom:building -> loom:issue at 03:08:15Z while the sweep was alive.
        let action = ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 2_781_227 });
        assert_eq!(
            apply_live_claim_veto(action, Some(&live_lock_evidence())),
            ReconcileAction::Keep
        );
    }

    #[test]
    fn live_claim_veto_applies_to_every_reclaim_reason() {
        // A live sweep means the claim is legitimate regardless of which rule
        // proposed dropping it.
        for reason in [
            ReclaimReason::DeadPid { pid: 1 },
            ReclaimReason::DeadRunRegistry { pid: 1 },
            ReclaimReason::NoRecordStale { age_hours: 99.0 },
        ] {
            assert_eq!(
                apply_live_claim_veto(
                    ReconcileAction::Reclaim(reason),
                    Some(&live_lock_evidence())
                ),
                ReconcileAction::Keep,
                "{reason:?} must be vetoed by a live claim"
            );
        }
    }

    #[test]
    fn live_claim_veto_is_a_no_op_without_live_evidence() {
        let action = ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 1 });
        assert_eq!(apply_live_claim_veto(action, None), action, "no evidence => unchanged");
        assert_eq!(
            apply_live_claim_veto(ReconcileAction::Keep, Some(&live_lock_evidence())),
            ReconcileAction::Keep,
            "Keep is never escalated"
        );
    }

    #[test]
    fn decide_keeps_when_no_record_and_within_grace() {
        let now = Utc::now();
        let recent = now - Duration::hours(1);
        let action =
            decide(&issue(42, Some(recent)), None, None, None, 10.0, &|_| true, 4.0, now, false);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_no_record_and_past_stale_threshold() {
        let now = Utc::now();
        let old = now - Duration::hours(5);
        let action =
            decide(&issue(42, Some(old)), None, None, None, 10.0, &|_| true, 4.0, now, false);
        match action {
            ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { age_hours }) => {
                assert!(age_hours >= 4.0);
            }
            other => panic!("expected NoRecordStale reclaim, got {other:?}"),
        }
    }

    #[test]
    fn decide_keeps_at_exact_boundary_minus_epsilon() {
        let now = Utc::now();
        // Just under the threshold: still within grace.
        let almost = now - Duration::minutes(239); // 3h59m < 4h
        let action =
            decide(&issue(42, Some(almost)), None, None, None, 10.0, &|_| true, 4.0, now, false);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_keeps_when_no_record_and_no_age_evidence() {
        let now = Utc::now();
        let action = decide(&issue(42, None), None, None, None, 10.0, &|_| true, 4.0, now, false);
        assert_eq!(action, ReconcileAction::Keep, "fail-safe: no evidence => Keep");
    }

    // ------------------------------------------------------------------
    // Startup-only immediate reclaim on total evidence absence (Issue #6615)
    // ------------------------------------------------------------------

    /// AC: on the STARTUP pass (`is_startup = true`), a `loom:building` claim
    /// with no journal entry, no run-registry pid, and no no-progress
    /// checkpoint evidence is reclaimed IMMEDIATELY -- even though the label
    /// is fresh (well within `stale_hours`, which the periodic rule would
    /// still respect) and even with no age evidence at all. This is the exact
    /// shape a crash between `begin_issue_dispatch`'s label flip and
    /// `finish_issue_dispatch`'s journal write leaves behind.
    #[test]
    fn decide_reclaims_immediately_at_startup_with_zero_evidence_and_fresh_label() {
        let now = Utc::now();
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, None, None, None, 10.0, &|_| true, 4.0, now, true);
        assert_eq!(
            action,
            ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup),
            "total absence of evidence must reclaim immediately on the startup pass, not wait \
             out stale_hours"
        );
    }

    #[test]
    fn decide_reclaims_immediately_at_startup_even_with_no_age_evidence() {
        // No `updated_at` at all -- the periodic rule's fail-safe (Keep) must
        // NOT apply here; the startup rule fires ahead of it.
        let now = Utc::now();
        let action = decide(&issue(42, None), None, None, None, 10.0, &|_| true, 4.0, now, true);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup));
    }

    /// Edge case 1 (curator's Test Plan): the exact same zero-evidence shape,
    /// mid-steady-state (`is_startup = false`), must NOT be reclaimed early --
    /// this is what protects a manually-spawned `/loom:sweep` that has not
    /// yet written a journal entry. Byte-for-byte the pre-#6615 behavior.
    #[test]
    fn decide_does_not_reclaim_zero_evidence_mid_steady_state() {
        let now = Utc::now();
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, None, None, None, 10.0, &|_| true, 4.0, now, false);
        assert_eq!(
            action,
            ReconcileAction::Keep,
            "is_startup=false must preserve the existing age-gated behavior"
        );
    }

    /// Edge case 2 (curator's Test Plan): a daemon restart with a genuinely
    /// live child from a prior dispatch (journal entry recording a live pid)
    /// must be KEPT even on the startup pass -- the journal/run-registry
    /// checks are unconditional and run before the `is_startup` fallback is
    /// ever consulted.
    #[test]
    fn decide_keeps_live_journal_pid_across_restart_even_at_startup() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action =
            decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| true, 4.0, now, true);
        assert_eq!(
            action,
            ReconcileAction::Keep,
            "a live journal pid must win regardless of is_startup"
        );
    }

    #[test]
    fn decide_keeps_live_run_registry_pid_across_restart_even_at_startup() {
        let now = Utc::now();
        let action = decide(
            &issue(42, None),
            None,
            Some(222),
            None,
            10.0,
            &|pid| pid == 222,
            4.0,
            now,
            true,
        );
        assert_eq!(
            action,
            ReconcileAction::Keep,
            "a live run-registry pid must win regardless of is_startup"
        );
    }

    /// A DEAD journal pid at startup must still surface as `DeadPid`, not
    /// the new `NoRecordAtStartup` reason -- the more specific evidence takes
    /// priority even when `is_startup` is set.
    #[test]
    fn decide_dead_journal_pid_at_startup_keeps_deadpid_reason() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action =
            decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| false, 4.0, now, true);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    #[test]
    fn plan_reclaims_zero_evidence_issue_immediately_at_startup() {
        let now = Utc::now();
        let journal = SweepJournal::default();
        let issues = vec![issue(1, Some(now - Duration::minutes(1)))];
        let decisions = plan(
            "/repo/a",
            &issues,
            &journal,
            &|_| None,
            &|_| None,
            10.0,
            &|_| true,
            4.0,
            now,
            true,
        );
        assert_eq!(decisions[0], (1, ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup)));
    }

    #[test]
    fn decide_dead_pid_overrides_label_age() {
        // Even a freshly-labeled issue must be reclaimed once its recorded
        // PID is provably dead — the journal is authoritative when present.
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action =
            decide(&fresh_issue, Some(&entry), None, None, 10.0, &|_| false, 4.0, now, false);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    // ------------------------------------------------------------------
    // Run-registry evidence source (Issue #4348)
    // ------------------------------------------------------------------

    #[test]
    fn decide_keeps_when_run_registry_pid_alive_and_no_journal_entry() {
        let now = Utc::now();
        let action = decide(
            &issue(42, None),
            None,
            Some(222),
            None,
            10.0,
            &|pid| pid == 222,
            4.0,
            now,
            false,
        );
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_run_registry_pid_dead_and_no_journal_entry() {
        let now = Utc::now();
        // A fresh label would normally still be within the age-rule grace
        // period, but the run-registry evidence is provable and immediate,
        // no age grace, exactly like the journal's DeadPid branch.
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, None, Some(999), None, 10.0, &|_| false, 4.0, now, false);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadRunRegistry { pid: 999 }));
    }

    #[test]
    fn decide_journal_entry_takes_priority_over_run_registry_pid() {
        // Both evidence sources present: the journal (more authoritative)
        // decides, and the run-registry pid is never even consulted.
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action = decide(
            &issue(42, None),
            Some(&entry),
            Some(999),
            None,
            10.0,
            &|pid| pid == 111,
            4.0,
            now,
            false,
        );
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_falls_back_to_age_rule_when_run_registry_pid_absent() {
        let now = Utc::now();
        let old = now - Duration::hours(5);
        let action =
            decide(&issue(42, Some(old)), None, None, None, 10.0, &|_| true, 4.0, now, false);
        match action {
            ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. }) => {}
            other => panic!("expected NoRecordStale reclaim, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Exit-0/no-progress fast reclaim (Issue #4462)
    // ------------------------------------------------------------------

    #[test]
    fn decide_reclaims_exited_no_progress_ahead_of_age_gate() {
        // No journal entry, no run-registry pid (the in-session sweep's entry
        // was cleaned up at exit), a FRESH label (well within the age grace),
        // but the caller's no-progress evidence is set: a checkpoint stalled at
        // curator-done with no open PR, and its own timestamp is well past the
        // no-progress grace period. This is the #4462 orphan the age gate
        // would otherwise sit on for hours -- reclaim NOW.
        let now = Utc::now();
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let stale_checkpoint = NoProgressEvidence {
            checkpoint_timestamp: now - Duration::minutes(35),
        };
        let action = decide(
            &fresh_issue,
            None,
            None,
            Some(stale_checkpoint),
            10.0,
            &|_| true,
            4.0,
            now,
            false,
        );
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::ExitedNoProgress));
    }

    /// Issue #4616: the exact race a resumed Builder retry produces —
    /// checkpoint at `curator-done`, no PR yet, no journal entry, and the
    /// checkpoint→run-registry join resolves to `None` (the ORIGINAL run's
    /// registry entry was already pruned) — must NOT be reclaimed while the
    /// checkpoint's own timestamp is still within the grace window. This is
    /// indistinguishable, by this evidence alone, from a brand-new resumed
    /// Builder run that has not yet linked back to the checkpoint or opened a
    /// PR.
    #[test]
    fn decide_keeps_exited_no_progress_within_grace_window() {
        let now = Utc::now();
        // The `loom:building` label itself may be old (from the ORIGINAL
        // claim, long before this resumed attempt) -- deliberately outside
        // the age-rule's own grace, so a Keep here can only be explained by
        // the no-progress grace window, not a fall-through to the age rule.
        let old_label_issue = issue(42, Some(now - Duration::hours(5)));
        let fresh_checkpoint = NoProgressEvidence {
            checkpoint_timestamp: now - Duration::minutes(2),
        };
        let action = decide(
            &old_label_issue,
            None,
            None,
            Some(fresh_checkpoint),
            10.0,
            &|_| true,
            4.0,
            now,
            false,
        );
        assert_eq!(
            action,
            ReconcileAction::Keep,
            "a checkpoint within the no-progress grace window must Keep, not Reclaim or fall \
             through to the (stale) age rule"
        );
    }

    #[test]
    fn decide_no_progress_never_overrides_a_live_journal_pid() {
        // A live journal pid is authoritative: even with no-progress evidence
        // past its grace period, the running sweep must be kept. Ordering
        // guarantee -- spurious no-progress evidence can never reclaim a live
        // sweep.
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let stale_checkpoint = NoProgressEvidence {
            checkpoint_timestamp: now - Duration::minutes(35),
        };
        let action = decide(
            &issue(42, None),
            Some(&entry),
            None,
            Some(stale_checkpoint),
            10.0,
            &|_| true,
            4.0,
            now,
            false,
        );
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_no_progress_never_overrides_a_live_run_registry_pid() {
        let now = Utc::now();
        let stale_checkpoint = NoProgressEvidence {
            checkpoint_timestamp: now - Duration::minutes(35),
        };
        let action = decide(
            &issue(42, None),
            None,
            Some(222),
            Some(stale_checkpoint),
            10.0,
            &|pid| pid == 222,
            4.0,
            now,
            false,
        );
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_no_progress_none_still_falls_through_to_age_gate() {
        // no_progress=None must not disturb the existing age-rule behavior:
        // a fresh label with no evidence is still Kept.
        let now = Utc::now();
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, None, None, None, 10.0, &|_| true, 4.0, now, false);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn plan_consults_no_progress_only_when_no_pid_evidence() {
        // #1 has a live journal pid; #2 has a dead run-registry pid; #3 has
        // neither. The no_progress closure records which issues it was asked
        // about and returns stale evidence for all -- it must be consulted
        // for #3 ONLY (mirroring decide()'s priority), and #1/#2 must be
        // decided by their pid evidence, never ExitedNoProgress.
        use std::cell::RefCell;
        let now = Utc::now();
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry("/repo/a", 1, 111)); // alive
        let issues = vec![
            issue(1, None),
            issue(2, Some(now - Duration::minutes(1))),
            issue(3, Some(now - Duration::minutes(1))),
        ];
        let run_registry_pid_for = |n: u32| -> Option<u32> {
            match n {
                2 => Some(999), // dead
                _ => None,
            }
        };
        let asked: RefCell<Vec<u32>> = RefCell::new(Vec::new());
        let no_progress_for = |n: u32| {
            asked.borrow_mut().push(n);
            Some(NoProgressEvidence {
                checkpoint_timestamp: now - Duration::minutes(35),
            })
        };
        let is_alive = |pid: u32| pid == 111;
        let decisions = plan(
            "/repo/a",
            &issues,
            &journal,
            &run_registry_pid_for,
            &no_progress_for,
            10.0,
            &is_alive,
            4.0,
            now,
            false,
        );
        assert_eq!(decisions[0], (1, ReconcileAction::Keep));
        assert_eq!(
            decisions[1],
            (2, ReconcileAction::Reclaim(ReclaimReason::DeadRunRegistry { pid: 999 }))
        );
        assert_eq!(decisions[2], (3, ReconcileAction::Reclaim(ReclaimReason::ExitedNoProgress)));
        assert_eq!(
            *asked.borrow(),
            vec![3],
            "no_progress evidence must be consulted ONLY for the issue with no pid evidence"
        );
    }

    #[test]
    fn plan_maps_each_issue_to_its_own_journal_entry() {
        let now = Utc::now();
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry("/repo/a", 1, 111)); // will be dead
        journal.entries.push(journal_entry("/repo/a", 2, 222)); // will be alive
                                                                // #3 has no journal entry and is stale.
        let issues = vec![
            issue(1, None),
            issue(2, None),
            issue(3, Some(now - Duration::hours(10))),
        ];

        let decisions = plan(
            "/repo/a",
            &issues,
            &journal,
            &|_| None,
            &|_| None,
            10.0,
            &|pid| pid == 222,
            4.0,
            now,
            false,
        );

        assert_eq!(decisions.len(), 3);
        assert_eq!(
            decisions[0],
            (1, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }))
        );
        assert_eq!(decisions[1], (2, ReconcileAction::Keep));
        match decisions[2] {
            (3, ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. })) => {}
            ref other => panic!("expected #3 to be a NoRecordStale reclaim, got {other:?}"),
        }
    }

    #[test]
    fn plan_scopes_journal_lookup_by_repo_string() {
        // Same issue number, different repos — must not cross-contaminate.
        let now = Utc::now();
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry("/repo/other", 42, 111));

        let issues = vec![issue(42, Some(now - Duration::hours(10)))];
        let decisions = plan(
            "/repo/a",
            &issues,
            &journal,
            &|_| None,
            &|_| None,
            10.0,
            &|_| true,
            4.0,
            now,
            false,
        );

        // No entry under "/repo/a" -> falls through to the age check, which
        // is stale here, so it reclaims (not "Keep" from the other repo's
        // live pid).
        match decisions[0] {
            (42, ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. })) => {}
            ref other => panic!("expected repo-scoped NoRecordStale reclaim, got {other:?}"),
        }
    }

    #[test]
    fn plan_consults_run_registry_only_when_journal_entry_absent() {
        let now = Utc::now();
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry("/repo/a", 1, 111)); // journal-backed, alive

        let issues = vec![issue(1, None), issue(2, Some(now - Duration::minutes(1)))];

        // #1 has a journal entry with an alive pid (111) -- the run-registry
        // closure below would say "dead" (999) for issue 1 if it were ever
        // consulted, so a wrong result there proves priority is broken. #2
        // has no journal entry; the run-registry closure resolves a dead pid
        // for it, which must reclaim immediately despite the fresh label.
        let run_registry_pid_for = |issue_num: u32| -> Option<u32> {
            match issue_num {
                1 => Some(999), // must be ignored: journal entry takes priority
                2 => Some(555),
                _ => None,
            }
        };
        let is_alive = |pid: u32| pid == 111; // only the journal's pid is alive

        let decisions = plan(
            "/repo/a",
            &issues,
            &journal,
            &run_registry_pid_for,
            &|_| None,
            10.0,
            &is_alive,
            4.0,
            now,
            false,
        );

        assert_eq!(decisions[0], (1, ReconcileAction::Keep));
        assert_eq!(
            decisions[1],
            (2, ReconcileAction::Reclaim(ReclaimReason::DeadRunRegistry { pid: 555 }))
        );
    }

    #[test]
    #[serial]
    fn reconciliation_enabled_resolves_env_precedence() {
        std::env::remove_var(RECONCILE_ENABLED_ENV);
        assert!(reconciliation_enabled(), "defaults to enabled");

        for off in ["0", "false", "no", "off", "OFF", "False"] {
            std::env::set_var(RECONCILE_ENABLED_ENV, off);
            assert!(!reconciliation_enabled(), "{off} should disable");
        }

        std::env::set_var(RECONCILE_ENABLED_ENV, "1");
        assert!(reconciliation_enabled());

        std::env::remove_var(RECONCILE_ENABLED_ENV);
    }

    #[test]
    #[serial]
    fn resolve_stale_hours_defaults_and_overrides() {
        std::env::remove_var(STALE_HOURS_ENV);
        assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);

        std::env::set_var(STALE_HOURS_ENV, "2.5");
        assert!((resolve_stale_hours() - 2.5).abs() < f64::EPSILON);

        // Non-positive / unparseable falls back to the default.
        std::env::set_var(STALE_HOURS_ENV, "0");
        assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);
        std::env::set_var(STALE_HOURS_ENV, "garbage");
        assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);

        std::env::remove_var(STALE_HOURS_ENV);
    }

    #[test]
    #[serial]
    fn resolve_no_progress_grace_minutes_defaults_and_overrides() {
        std::env::remove_var(NO_PROGRESS_GRACE_MINUTES_ENV);
        assert!(
            (resolve_no_progress_grace_minutes() - DEFAULT_NO_PROGRESS_GRACE_MINUTES).abs()
                < f64::EPSILON
        );

        std::env::set_var(NO_PROGRESS_GRACE_MINUTES_ENV, "3.5");
        assert!((resolve_no_progress_grace_minutes() - 3.5).abs() < f64::EPSILON);

        // Non-positive / unparseable falls back to the default.
        std::env::set_var(NO_PROGRESS_GRACE_MINUTES_ENV, "0");
        assert!(
            (resolve_no_progress_grace_minutes() - DEFAULT_NO_PROGRESS_GRACE_MINUTES).abs()
                < f64::EPSILON
        );
        std::env::set_var(NO_PROGRESS_GRACE_MINUTES_ENV, "garbage");
        assert!(
            (resolve_no_progress_grace_minutes() - DEFAULT_NO_PROGRESS_GRACE_MINUTES).abs()
                < f64::EPSILON
        );

        std::env::remove_var(NO_PROGRESS_GRACE_MINUTES_ENV);
    }

    // ------------------------------------------------------------------
    // Periodic-interval resolution (Issue #4348)
    // ------------------------------------------------------------------

    /// Write a `.loom/config.json` with the given `safehouse` block into a
    /// fresh tempdir root (Issue #4431 interval tests).
    fn root_with_safehouse_config(
        dir: &std::path::Path,
        safehouse_json: &str,
    ) -> std::path::PathBuf {
        let root = dir.join("repo");
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        std::fs::write(
            root.join(".loom").join("config.json"),
            format!(r#"{{"safehouse": {safehouse_json}}}"#),
        )
        .unwrap();
        root
    }

    /// #4431: precedence env > config override > safehouse-mode default >
    /// legacy default, floor always enforced.
    #[test]
    #[serial]
    fn resolve_reconcile_interval_for_is_safehouse_aware() {
        std::env::remove_var(RECONCILE_INTERVAL_ENV);
        // The safehouse env toggle would shadow the config file — clear it so
        // the test exercises the config layer (hosts like loom-worker-1 set
        // it in the daemon unit, but tests must not depend on that).
        std::env::remove_var("LOOM_SAFEHOUSE_ENABLED");
        let dir = tempdir().unwrap();

        // Safehouse enabled → the slow healing cadence.
        let root = root_with_safehouse_config(dir.path(), r#"{"enabled": true}"#);
        assert_eq!(
            resolve_reconcile_interval_for(&root),
            std::time::Duration::from_secs(DEFAULT_SAFEHOUSE_RECONCILE_INTERVAL_SECS)
        );

        // Safehouse disabled (or absent) → byte-for-byte the pre-#4431 default.
        let root = root_with_safehouse_config(dir.path(), r#"{"enabled": false}"#);
        assert_eq!(
            resolve_reconcile_interval_for(&root),
            std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS)
        );
        assert_eq!(
            resolve_reconcile_interval_for(dir.path()),
            std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS),
            "no config file at all must resolve to the legacy default"
        );

        // Per-repo config override wins over the safehouse-mode default…
        let root = root_with_safehouse_config(
            dir.path(),
            r#"{"enabled": true, "claimReconcileIntervalSecs": 900}"#,
        );
        assert_eq!(resolve_reconcile_interval_for(&root), std::time::Duration::from_secs(900));
        // …but a zero/invalid override is ignored, and the floor still holds.
        let root = root_with_safehouse_config(
            dir.path(),
            r#"{"enabled": true, "claimReconcileIntervalSecs": 0}"#,
        );
        assert_eq!(
            resolve_reconcile_interval_for(&root),
            std::time::Duration::from_secs(DEFAULT_SAFEHOUSE_RECONCILE_INTERVAL_SECS)
        );
        let root = root_with_safehouse_config(
            dir.path(),
            r#"{"enabled": true, "claimReconcileIntervalSecs": 5}"#,
        );
        assert_eq!(
            resolve_reconcile_interval_for(&root),
            std::time::Duration::from_secs(MIN_RECONCILE_INTERVAL_SECS)
        );

        // The operator env var beats everything, on any host.
        std::env::set_var(RECONCILE_INTERVAL_ENV, "300");
        let root = root_with_safehouse_config(dir.path(), r#"{"enabled": true}"#);
        assert_eq!(resolve_reconcile_interval_for(&root), std::time::Duration::from_secs(300));
        std::env::remove_var(RECONCILE_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn resolve_reconcile_interval_defaults_and_overrides() {
        std::env::remove_var(RECONCILE_INTERVAL_ENV);
        assert_eq!(
            resolve_reconcile_interval(),
            std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS)
        );

        std::env::set_var(RECONCILE_INTERVAL_ENV, "900");
        assert_eq!(resolve_reconcile_interval(), std::time::Duration::from_secs(900));

        std::env::remove_var(RECONCILE_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn resolve_reconcile_interval_enforces_floor() {
        std::env::set_var(RECONCILE_INTERVAL_ENV, "5");
        assert_eq!(
            resolve_reconcile_interval(),
            std::time::Duration::from_secs(MIN_RECONCILE_INTERVAL_SECS),
            "an interval below the floor must be clamped up, not honored literally"
        );
        std::env::remove_var(RECONCILE_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn resolve_reconcile_interval_ignores_non_positive_or_unparseable() {
        std::env::set_var(RECONCILE_INTERVAL_ENV, "0");
        assert_eq!(
            resolve_reconcile_interval(),
            std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS)
        );
        std::env::set_var(RECONCILE_INTERVAL_ENV, "garbage");
        assert_eq!(
            resolve_reconcile_interval(),
            std::time::Duration::from_secs(DEFAULT_RECONCILE_INTERVAL_SECS)
        );
        std::env::remove_var(RECONCILE_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn run_reconciliation_pass_noops_when_disabled() {
        // Kill-switch interaction: the periodic pass and the startup pass
        // share this exact gate, so disabling it must short-circuit BEFORE
        // any `gh` invocation -- this test has no fake `gh` on `PATH` at all,
        // so a regression that skipped the gate would fail with a spawn
        // error instead of returning quietly.
        std::env::set_var(RECONCILE_ENABLED_ENV, "0");
        let dir = tempdir().unwrap();
        run_reconciliation_pass(dir.path(), true);
        std::env::remove_var(RECONCILE_ENABLED_ENV);
    }

    // ------------------------------------------------------------------
    // Checkpoint -> run-registry join (Issue #4348)
    // ------------------------------------------------------------------

    fn seed_checkpoint_task_id(root: &std::path::Path, issue: u32, task_id: &str) {
        let dir = root.join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("issue-{issue}.json")),
            format!(
                r#"{{"phase":"builder","task_id":"{task_id}","timestamp":"2026-01-01T00:00:00Z","pr_number":null}}"#
            ),
        )
        .unwrap();
    }

    fn seed_run_registry(root: &std::path::Path, task_id: &str, pid: u32) {
        let dir = root.join(".loom").join("sweep-run");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{task_id}.json")),
            format!(r#"{{"run_id":"{task_id}","pid":{pid},"timestamp":"2026-01-01T00:00:00Z"}}"#),
        )
        .unwrap();
    }

    /// Seed a checkpoint recording an arbitrary `phase` (and no run-registry
    /// join by default -- callers add one separately if needed). Used by the
    /// Issue #4462 exit-0/no-progress tests, which need a `curator-done`
    /// checkpoint with NO surviving run-registry entry. The checkpoint
    /// `timestamp` is a fixed date far in the past, well outside any
    /// no-progress grace window, so this helper's checkpoints already read as
    /// "stale enough to reclaim" by construction.
    fn seed_checkpoint_phase(root: &std::path::Path, issue: u32, phase: &str) {
        seed_checkpoint_phase_with_timestamp(root, issue, phase, "2026-01-01T00:00:00Z");
    }

    /// Like [`seed_checkpoint_phase`], but with an explicit `timestamp` —
    /// needed by the Issue #4616 grace-window tests, which must control
    /// whether the checkpoint reads as "just resumed" or "aged past grace".
    fn seed_checkpoint_phase_with_timestamp(
        root: &std::path::Path,
        issue: u32,
        phase: &str,
        timestamp: &str,
    ) {
        let dir = root.join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("issue-{issue}.json")),
            format!(
                r#"{{"phase":"{phase}","task_id":"sweep-{issue}","timestamp":"{timestamp}","pr_number":null}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn resolve_run_registry_pid_returns_none_when_checkpoint_missing() {
        let dir = tempdir().unwrap();
        assert_eq!(resolve_run_registry_pid(dir.path(), 42), None);
    }

    #[test]
    fn resolve_run_registry_pid_returns_none_when_task_id_missing() {
        let dir = tempdir().unwrap();
        let checkpoint_dir = dir.path().join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(checkpoint_dir.join("issue-42.json"), r#"{"phase":"builder"}"#).unwrap();
        assert_eq!(resolve_run_registry_pid(dir.path(), 42), None);
    }

    #[test]
    fn resolve_run_registry_pid_returns_none_when_run_registry_entry_missing() {
        let dir = tempdir().unwrap();
        seed_checkpoint_task_id(dir.path(), 42, "sweep-abc123");
        // No `.loom/sweep-run/sweep-abc123.json` written.
        assert_eq!(resolve_run_registry_pid(dir.path(), 42), None);
    }

    #[test]
    fn resolve_run_registry_pid_joins_checkpoint_and_run_registry() {
        let dir = tempdir().unwrap();
        seed_checkpoint_task_id(dir.path(), 42, "sweep-abc123");
        seed_run_registry(dir.path(), "sweep-abc123", 4242);
        assert_eq!(resolve_run_registry_pid(dir.path(), 42), Some(4242));
    }

    #[test]
    fn resolve_run_registry_pid_returns_none_on_malformed_checkpoint_json() {
        let dir = tempdir().unwrap();
        let checkpoint_dir = dir.path().join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(checkpoint_dir.join("issue-42.json"), "not json at all").unwrap();
        assert_eq!(
            resolve_run_registry_pid(dir.path(), 42),
            None,
            "malformed checkpoint JSON must degrade fail-safe, never panic or fake a pid"
        );
    }

    #[test]
    fn resolve_run_registry_pid_returns_none_on_malformed_run_registry_json() {
        let dir = tempdir().unwrap();
        seed_checkpoint_task_id(dir.path(), 42, "sweep-abc123");
        let run_dir = dir.path().join(".loom").join("sweep-run");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("sweep-abc123.json"), "{ not valid").unwrap();
        assert_eq!(
            resolve_run_registry_pid(dir.path(), 42),
            None,
            "malformed run-registry JSON must degrade fail-safe, never panic or fake a pid"
        );
    }

    /// Regression test for #3975: `reconcile_workspace` used to prune dead
    /// journal entries *before* deciding, which erased the exact evidence the
    /// `DeadPid` branch needs. A claim with a provably-dead recorded PID must
    /// be reclaimed immediately by the daemon's own startup pass, even when
    /// the `loom:building` label is only seconds old (well inside the
    /// `NoRecordStale` grace window) -- two incidents (#6170/#6173 in a
    /// downstream workspace) were exactly this: SIGTERMed sweeps left a dead
    /// PID in the journal, and the pre-decide prune silently downgraded them
    /// to "no record", so they sat un-reclaimed for hours.
    #[test]
    #[serial]
    fn reconcile_workspace_reclaims_dead_pid_entry_even_when_label_is_fresh() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // Seed a dead-PID entry (pid 0 is always dead per `is_pid_alive`).
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry(&repo_str, 99, 0));
        sweep_journal::save(&journal_path, &journal).unwrap();

        // Fake `gh`: the REST listing (#4428) reports one loom:building issue
        // labeled *just now* -- fresh enough that the NoRecordStale
        // (age-based) path would say Keep. Only the DeadPid evidence should
        // trigger a reclaim.
        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 99, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "a dead-PID journal entry must be reclaimed immediately regardless of \
             how fresh the loom:building label is (#3975)"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 99 --remove-label loom:building --add-label loom:issue"),
            "expected reclaim to flip labels for #99; got: {gh_calls:?}"
        );

        // The reclaimed issue's journal entry is cleaned up as part of
        // recovery -- confirms cleanup still happens, just after (not
        // before) the decision that needed the evidence.
        let after = sweep_journal::load(&journal_path);
        assert!(sweep_journal::find(&after, &repo_str, 99).is_none());

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// #6263 regression: a single `gh issue edit --remove-label
    /// loom:building --add-label loom:issue` invocation can exit 0 while
    /// only *partially* applying the swap — root cause: `gh` implements
    /// `--add-label`/`--remove-label` as two independent, concurrently
    /// fired GraphQL mutations (see [`forge::reclaim`]'s doc comment), not
    /// one atomic operation. This is the plausible mechanism behind #6254
    /// carrying both `loom:issue` and `loom:building` simultaneously for
    /// ~37 minutes on 2026-08-15. The fix must detect the partial
    /// application via a post-mutation re-fetch and repair it with exactly
    /// one bounded retry.
    #[test]
    #[serial]
    fn reconcile_workspace_repairs_reclaim_left_partially_applied_by_a_zero_exit_gh() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // Same dead-PID fixture as the #3975 regression test above -- an
        // unconditional, immediate reclaim.
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry(&repo_str, 99, 0));
        sweep_journal::save(&journal_path, &journal).unwrap();

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh_with_partial_reclaim(dir.path(), &gh_log, 99, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "the reclaim must succeed once the post-mutation re-fetch confirms the \
             retried edit actually landed both halves (#6263)"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        let edit_calls = gh_calls
            .lines()
            .filter(|l| {
                l.contains("issue edit 99 --remove-label loom:building --add-label loom:issue")
            })
            .count();
        assert_eq!(
            edit_calls, 2,
            "expected exactly one bounded retry (2 total edit calls) after the first \
             invocation reported success but only partially applied the swap; got: {gh_calls:?}"
        );
        let view_calls = gh_calls
            .lines()
            .filter(|l| l.contains("issue view 99 --json labels"))
            .count();
        assert!(
            view_calls >= 2,
            "expected a post-mutation re-fetch after each edit attempt; got: {gh_calls:?}"
        );

        // The reclaim was confirmed to have fully landed, so the journal
        // entry is cleaned up exactly like a straightforward reclaim.
        let after = sweep_journal::load(&journal_path);
        assert!(sweep_journal::find(&after, &repo_str, 99).is_none());

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// #6263 AC3: if the post-mutation re-fetch keeps showing the label
    /// swap incomplete even after the one bounded retry, the reclaim must
    /// fail (not count toward `reclaimed`) rather than loop indefinitely or
    /// silently accept the wrong final state — the exact number of `gh
    /// issue edit` invocations is asserted to prove the retry is bounded,
    /// not unbounded.
    #[test]
    #[serial]
    fn reconcile_workspace_does_not_loop_forever_when_reclaim_never_repairs() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry(&repo_str, 99, 0));
        sweep_journal::save(&journal_path, &journal).unwrap();

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh_with_persistent_partial_reclaim(dir.path(), &gh_log, 99, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 0,
            "a reclaim that never repairs after the bounded retry must not be counted as \
             reclaimed (#6263)"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        let edit_calls = gh_calls
            .lines()
            .filter(|l| {
                l.contains("issue edit 99 --remove-label loom:building --add-label loom:issue")
            })
            .count();
        assert_eq!(
            edit_calls, 2,
            "the retry must be bounded to exactly one attempt, never an unbounded loop \
             (#6263 AC3); got {edit_calls} edit calls in: {gh_calls:?}"
        );

        // Nothing was confirmed reclaimed, so the journal entry survives
        // untouched -- matches the existing "no cleanup on failure"
        // convention (see the failed-`gh` branch in `reconcile_workspace`).
        let after = sweep_journal::load(&journal_path);
        assert!(sweep_journal::find(&after, &repo_str, 99).is_some());

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// Epic #6165 Phase 4 (#6317) regression: the identical dead-PID-in-
    /// journal fixture the former Issue #6157 "frozen while degraded" test
    /// used to gate on is now reclaimed unconditionally — `reconcile_workspace`
    /// no longer accepts (or consults) any peer-coordination-health evidence
    /// at all. This replaces
    /// `reconcile_workspace_freezes_reclaim_when_peer_coordination_degraded`
    /// / `reconcile_workspace_with_coordination_reclaims_normally_when_not_degraded`
    /// (both removed — their entire premise, an injectable
    /// `coordination_degraded_reason` seam, no longer exists): there is
    /// nothing left to freeze reclaim on peer-channel health, by
    /// construction, so this test simply confirms the ordinary dead-PID
    /// reclaim (mirroring the #3975 fixture) still fires with no such
    /// signal available at all — the peer-claim/safehouse channel plays no
    /// role whatsoever in this decision now, healthy or not.
    #[test]
    #[serial]
    fn reconcile_workspace_reclaims_dead_pid_with_no_peer_coordination_signal_consulted() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // Same dead-PID fixture as #3975's regression test: an
        // unconditional, immediate reclaim, with the peer-coordination
        // global view never registered at all (the default state for every
        // test in this binary — see `peer_claims::GLOBAL_VIEW`'s removal in
        // #6317).
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry(&repo_str, 99, 0));
        sweep_journal::save(&journal_path, &journal).unwrap();

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 99, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "reclaim must fire on dead-PID evidence with no peer-coordination signal of any \
             kind involved in the decision (#6317)"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 99 --remove-label loom:building --add-label loom:issue"),
            "expected reclaim to flip labels for #99; got: {gh_calls:?}"
        );

        let after = sweep_journal::load(&journal_path);
        assert!(sweep_journal::find(&after, &repo_str, 99).is_none());

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    // ------------------------------------------------------------------
    // Integration: run-registry evidence via `forge::reconcile_workspace`
    // (Issue #4348)
    // ------------------------------------------------------------------

    /// Write a fake `gh` script (tests only) that logs every invocation to
    /// `gh_log` and, for the ETag-cached REST listing (`gh api …/issues?…`,
    /// #4428), reports exactly one `loom:building` issue (`issue_number`,
    /// `updated_at`) as an `--include`-style HTTP response with **no ETag**
    /// (so the process-global cache never carries state across tests). Every
    /// other subcommand (e.g. `issue edit`) just logs and exits 0 -- a test
    /// asserts on `gh_log`'s contents to see whether a reclaim was actually
    /// attempted.
    fn write_fake_gh(
        dir: &std::path::Path,
        gh_log: &std::path::Path,
        issue_number: u32,
        updated_at: &str,
    ) -> std::path::PathBuf {
        let fake_gh = dir.join("fake-gh.sh");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{updated_at}"}}]'
  exit 0
fi
if [ "$1" = "pr" ]; then
  # `pr list --head feature/issue-N ...`: no open linked PR by default
  # (the Issue #4462 no-progress path treats empty stdout as "no PR").
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }
        fake_gh
    }

    /// Write a fake `gh` script (tests only, #6263) that reproduces the
    /// non-atomicity of `gh issue edit --remove-label loom:building
    /// --add-label loom:issue`: every `issue edit` invocation exits 0
    /// (matching real `gh`'s exit status once its GraphQL mutations are
    /// accepted), but the *first* subsequent `gh issue view --json labels`
    /// re-fetch reports the swap only partially applied (both
    /// `loom:building` and `loom:issue` present — mirroring the ~37-minute
    /// co-presence observed on #6254). From the second `issue edit`
    /// invocation onward, the labels are reported fully corrected
    /// (`loom:issue` only) — simulating a retry that succeeds.
    fn write_fake_gh_with_partial_reclaim(
        dir: &std::path::Path,
        gh_log: &std::path::Path,
        issue_number: u32,
        updated_at: &str,
    ) -> std::path::PathBuf {
        let fake_gh = dir.join("fake-gh-partial-reclaim.sh");
        let counter = dir.join("edit-count");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{updated_at}"}}]'
  exit 0
fi
if [ "$1" = "pr" ]; then
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "edit" ]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  count=$((count + 1))
  echo "$count" > "{counter}"
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  count=$(cat "{counter}" 2>/dev/null || echo 0)
  if [ "$count" -ge 2 ]; then
    echo '{{"labels":[{{"name":"loom:issue"}}]}}'
  else
    echo '{{"labels":[{{"name":"loom:building"}},{{"name":"loom:issue"}}]}}'
  fi
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
            counter = counter.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }
        fake_gh
    }

    /// Same shape as [`write_fake_gh_with_partial_reclaim`], but the
    /// partial application never repairs — every `gh issue view --json
    /// labels` re-fetch reports both `loom:building` and `loom:issue`
    /// present, regardless of how many `issue edit` retries have run.
    /// Exercises the #6263 AC3 "fail safe, never loop forever" path.
    fn write_fake_gh_with_persistent_partial_reclaim(
        dir: &std::path::Path,
        gh_log: &std::path::Path,
        issue_number: u32,
        updated_at: &str,
    ) -> std::path::PathBuf {
        let fake_gh = dir.join("fake-gh-stuck-reclaim.sh");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{updated_at}"}}]'
  exit 0
fi
if [ "$1" = "pr" ]; then
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "edit" ]; then
  exit 0
fi
if [ "$1" = "issue" ] && [ "$2" = "view" ]; then
  echo '{{"labels":[{{"name":"loom:building"}},{{"name":"loom:issue"}}]}}'
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }
        fake_gh
    }

    // ------------------------------------------------------------------
    // Lease-record freshness (Epic #6165 Phase 2, Issue #6286)
    // ------------------------------------------------------------------

    /// Pure unit coverage for [`lease_is_fresh`]: within the TTL is fresh,
    /// past it is not, and the boundary itself (age == ttl) is NOT fresh
    /// (strict `<`, matching every other age-gate in this module).
    #[test]
    fn lease_is_fresh_within_ttl_stale_past_it_boundary_exclusive() {
        let now = Utc::now();
        assert!(
            lease_is_fresh(now - Duration::minutes(5), now, 15.0),
            "a lease renewed 5 minutes ago is fresh under a 15-minute TTL"
        );
        assert!(
            !lease_is_fresh(now - Duration::minutes(16), now, 15.0),
            "a lease last renewed 16 minutes ago has genuinely expired under a 15-minute TTL"
        );
        assert!(
            !lease_is_fresh(now - Duration::minutes(15), now, 15.0),
            "age exactly equal to the TTL must NOT be treated as fresh (strict <, not <=)"
        );
    }

    /// Issue #6320: a reclaim decision must record WHY it was reclaimable —
    /// specifically whether the lease had expired or was never published at
    /// all. [`classify_lease_evidence`] is the pure classifier that feeds
    /// that log line; absence must classify as `Absent`, never collapse into
    /// "stale".
    #[test]
    fn classify_lease_evidence_separates_absent_from_stale_and_fresh() {
        let now = Utc::now();
        assert_eq!(
            classify_lease_evidence(None, now, 15.0),
            LeaseEvidence::Absent,
            "no lease comment is ABSENT evidence, never 'stale' — per lease-record.md's \
             reader contract, absence is not evidence of abandonment"
        );
        match classify_lease_evidence(Some(now - Duration::minutes(5)), now, 15.0) {
            LeaseEvidence::Fresh { age_minutes } => {
                assert!(
                    (age_minutes - 5.0).abs() < 0.5,
                    "fresh lease reports its own age (got {age_minutes})"
                );
            }
            other => panic!("a 5-minute-old lease under a 15m TTL must be Fresh, got {other:?}"),
        }
        match classify_lease_evidence(Some(now - Duration::minutes(40)), now, 15.0) {
            LeaseEvidence::Stale { age_minutes } => {
                assert!(
                    (age_minutes - 40.0).abs() < 0.5,
                    "stale lease reports its own age (got {age_minutes})"
                );
            }
            other => panic!("a 40-minute-old lease under a 15m TTL must be Stale, got {other:?}"),
        }
    }

    /// The classification only earns its keep if it reaches the operator's
    /// log in a legible, greppable form — the reclaim log line interpolates
    /// `Display`, so assert on that rendering directly (#6320).
    #[test]
    fn lease_evidence_display_is_greppable_and_distinguishes_the_three_cases() {
        let absent = LeaseEvidence::Absent.to_string();
        let fresh = LeaseEvidence::Fresh { age_minutes: 3.25 }.to_string();
        let stale = LeaseEvidence::Stale { age_minutes: 42.0 }.to_string();

        assert!(absent.starts_with("lease_evidence=absent"), "got: {absent}");
        assert!(fresh.starts_with("lease_evidence=fresh"), "got: {fresh}");
        assert!(stale.starts_with("lease_evidence=stale"), "got: {stale}");
        assert!(
            fresh.contains("3.2") || fresh.contains("3.3"),
            "fresh rendering carries the age: {fresh}"
        );
        assert!(stale.contains("42.0"), "stale rendering carries the age: {stale}");
        assert!(
            absent.contains("not evidence of abandonment"),
            "absent rendering states the reader contract so a log reader is not misled: {absent}"
        );
    }

    /// Write a fake `gh` script (tests only) that, in addition to
    /// [`write_fake_gh`]'s ETag-cached REST listing response, answers the
    /// lease-comments fetch (`gh api .../issues/<N>/comments --paginate
    /// --jq ...`, [`forge::fetch_freshest_lease_updated_at`]) with a single
    /// pre-filtered timestamp value — emulating what `gh`'s own `--jq`
    /// filtering would have produced from a real API response containing one
    /// lease-record comment. `lease_updated_at: None` emulates "no lease
    /// comment found at all" (`// empty` in the real jq filter -> empty
    /// stdout).
    fn write_fake_gh_with_lease(
        dir: &std::path::Path,
        gh_log: &std::path::Path,
        issue_number: u32,
        label_updated_at: &str,
        lease_updated_at: Option<&str>,
    ) -> std::path::PathBuf {
        let fake_gh = dir.join("fake-gh-lease.sh");
        let lease_stdout = match lease_updated_at {
            Some(ts) => format!("echo '\"{ts}\"'"),
            None => "true # no lease comment -- empty stdout".to_string(),
        };
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  case "$*" in
    *--include*)
      printf 'HTTP/2.0 200 OK\r\n\r\n'
      echo '[{{"number":{issue_number},"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{label_updated_at}"}}]'
      exit 0
      ;;
    */comments*)
      {lease_stdout}
      exit 0
      ;;
  esac
  exit 0
fi
if [ "$1" = "pr" ]; then
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }
        fake_gh
    }

    /// Issue #6286 acceptance criterion — the core regression test: a live
    /// peer sweep whose lease is being renewed, but whose local liveness
    /// evidence (dead-PID journal entry -- the #3975 fixture that normally
    /// fires an IMMEDIATE, unconditional reclaim) looks dead, with the
    /// peer-claim/safehouse channel simply absent (no `PeerClaimView` ever
    /// registered — the reading a host with no safehouse configured
    /// produces, and per Epic #6165 Phase 4/#6317 now the ONLY reading that
    /// exists, since the peer-claim channel is no longer consulted by this
    /// decision at all). Reclamation must NOT fire while the lease is
    /// fresh, regardless of what the host-scoped evidence says.
    #[test]
    #[serial]
    fn reconcile_workspace_keeps_claim_when_lease_is_fresh_even_with_channel_absent() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // Same dead-PID fixture as the #3975/#6157 regression tests: local
        // evidence alone says "reclaim immediately, no grace period".
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry(&repo_str, 99, 0));
        sweep_journal::save(&journal_path, &journal).unwrap();

        let gh_log = dir.path().join("gh-invocations.log");
        let label_updated_at = Utc::now().to_rfc3339();
        // The lease was renewed 2 minutes ago -- comfortably within the
        // 15-minute default TTL.
        let lease_updated_at = (Utc::now() - Duration::minutes(2)).to_rfc3339();
        let fake_gh = write_fake_gh_with_lease(
            dir.path(),
            &gh_log,
            99,
            &label_updated_at,
            Some(&lease_updated_at),
        );

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1, "the issue is still inspected — only the reclaim ACTION is frozen");
        assert_eq!(
            reclaimed, 0,
            "a fresh lease record must block reclaim even though the dead-PID evidence alone \
             would normally reclaim immediately, and even with no peer-claim/safehouse signal \
             at all (#6286)"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("--add-label loom:issue"),
            "no gh issue edit must be issued while the lease is fresh; got: {gh_calls:?}"
        );
        assert!(
            gh_calls.contains("/comments"),
            "the lease-comments endpoint must actually have been consulted; got: {gh_calls:?}"
        );

        // Nothing was reclaimed, so the journal entry must survive untouched.
        let after = sweep_journal::load(&journal_path);
        assert!(sweep_journal::find(&after, &repo_str, 99).is_some());

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// The fail-safe must not become "never reclaim": a claim whose lease has
    /// genuinely expired (last renewed well past the TTL) must still be
    /// reclaimed once the pre-existing host-scoped evidence says so.
    #[test]
    #[serial]
    fn reconcile_workspace_reclaims_when_lease_has_expired() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry(&repo_str, 99, 0));
        sweep_journal::save(&journal_path, &journal).unwrap();

        let gh_log = dir.path().join("gh-invocations.log");
        let label_updated_at = Utc::now().to_rfc3339();
        // Last renewed 30 minutes ago -- well past the 15-minute default TTL.
        let lease_updated_at = (Utc::now() - Duration::minutes(30)).to_rfc3339();
        let fake_gh = write_fake_gh_with_lease(
            dir.path(),
            &gh_log,
            99,
            &label_updated_at,
            Some(&lease_updated_at),
        );

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "a genuinely expired lease must not block reclamation -- the fail-safe must not \
             become 'never reclaim' (#6286)"
        );

        let after = sweep_journal::load(&journal_path);
        assert!(
            sweep_journal::find(&after, &repo_str, 99).is_none(),
            "a genuine reclaim still cleans up its journal entry"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// A `loom:building` claim with NO lease comment at all (a claim
    /// predating this feature) must reclaim exactly as it did before this
    /// phase existed -- absence of lease evidence is not itself a reason to
    /// refuse.
    #[test]
    #[serial]
    fn reconcile_workspace_reclaims_normally_when_no_lease_comment_exists() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry(&repo_str, 99, 0));
        sweep_journal::save(&journal_path, &journal).unwrap();

        let gh_log = dir.path().join("gh-invocations.log");
        let label_updated_at = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh_with_lease(dir.path(), &gh_log, 99, &label_updated_at, None);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "no lease comment at all must not itself block reclamation (#6286)"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// Issue #3651's fail-safe, re-verified against the lease-only
    /// reclamation path (Epic #6165 Phase 4, #6317): "absent liveness
    /// evidence means every claim is treated as ALIVE, never as orphaned."
    ///
    /// This is the total-absence-of-evidence case, on EVERY axis this
    /// module and Epic #6165 collectively consult: no journal entry, no
    /// run-registry/checkpoint join, no lease comment, AND (implicitly,
    /// since the peer-coordination global view is never registered in this
    /// test binary — see `peer_claims::GLOBAL_VIEW`) no peer-claim
    /// advertisement either. With the `loom:building` label itself freshly
    /// applied (well under [`DEFAULT_STALE_BUILDING_HOURS`]), the claim
    /// must be `Keep`, not reclaimed — a total absence of information is
    /// never, by itself, proof of death; only *aged* absence is (the
    /// `NoRecordStale` branch [`decide`] falls to below, gated on
    /// `stale_hours`, is deliberately NOT exercised by this test's fresh
    /// label).
    #[test]
    #[serial]
    fn reconcile_workspace_keeps_claim_with_zero_evidence_on_every_axis_fresh_label() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        // No journal entry anywhere for this repo -- point the journal seam
        // at an empty file so the daemon's real `~/.loom/sweeps.json` (if
        // any exists on the test host) is never touched, and so there is
        // genuinely zero journal evidence for issue #99.
        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // No checkpoint file at all -- `read_checkpoint_phase` returns
        // `None`, so both the run-registry join and the no-progress
        // evidence short-circuit to `None` without even attempting a `gh`
        // call for either.

        let gh_log = dir.path().join("gh-invocations.log");
        // Freshly applied label -- comfortably under the default 4-hour
        // staleness threshold.
        let label_updated_at = Utc::now().to_rfc3339();
        // No lease comment either (`None`).
        let fake_gh = write_fake_gh_with_lease(dir.path(), &gh_log, 99, &label_updated_at, None);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 0,
            "zero evidence on every axis (journal, run-registry, lease, peer-claim) must fail \
             safe to Keep for a freshly-labeled claim -- absence is never itself proof of \
             death (#3651, re-verified post-#6317)"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("--add-label loom:issue"),
            "no gh issue edit must be issued when every evidence source is absent; got: \
             {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// Fabricated-workspace integration test (Issue #4348 acceptance
    /// criterion): a `loom:building` issue with NO journal entry at all (the
    /// manually/externally spawned sweep's signature -- only
    /// `SweepRegistry::dispatch` writes the journal), but a checkpoint +
    /// run-registry entry recording a now-dead pid, must be reclaimed within
    /// one pass even though the label is fresh (no age-rule grace applies to
    /// this provable-death evidence source, exactly like the journal's
    /// `DeadPid` branch).
    #[test]
    #[serial]
    fn reconcile_workspace_reclaims_via_dead_run_registry_pid_when_no_journal_entry() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        // No journal entry anywhere for this repo -- point the journal seam
        // at an empty file so the daemon's real `~/.loom/sweeps.json` (if
        // any exists on the test host) is never touched.
        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        seed_checkpoint_task_id(&repo_root, 77, "sweep-dead-1");
        seed_run_registry(&repo_root, "sweep-dead-1", 0); // pid 0 is always dead

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 77, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "a dead run-registry pid must be reclaimed even with a fresh label and no \
             journal entry (#4348)"
        );
        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 77 --remove-label loom:building --add-label loom:issue"),
            "expected reclaim to flip labels for #77; got: {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// A `loom:building` issue whose manual sweep is still ALIVE (per the
    /// run-registry join) must be kept -- the periodic/startup pass never
    /// reclaims a live claim, even with no journal entry at all.
    #[test]
    #[serial]
    fn reconcile_workspace_keeps_when_run_registry_pid_alive_and_no_journal_entry() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        seed_checkpoint_task_id(&repo_root, 78, "sweep-alive-1");
        // This test process's own pid is, by definition, alive.
        seed_run_registry(&repo_root, "sweep-alive-1", std::process::id());

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 78, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(reclaimed, 0, "a live run-registry pid must never be reclaimed");

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// A malformed checkpoint must degrade to the existing age rule, never a
    /// spurious reclaim -- with a fresh label, that means `Keep`.
    #[test]
    #[serial]
    fn reconcile_workspace_falls_back_to_age_rule_on_malformed_checkpoint() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        let checkpoint_dir = repo_root.join(".loom").join("sweep-checkpoint");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(checkpoint_dir.join("issue-79.json"), "not json").unwrap();

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 79, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 0,
            "a malformed checkpoint must never be treated as proof of death (fail-safe)"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    // ------------------------------------------------------------------
    // Integration: startup-only immediate reclaim on total evidence absence
    // (Issue #6615)
    // ------------------------------------------------------------------

    /// End-to-end repro of the #6615 gap: a `loom:building` claim with a
    /// FRESH label (well within `stale_hours`), no journal entry, and no
    /// checkpoint at all (so no run-registry join and no no-progress
    /// evidence either) -- exactly what a daemon crash between
    /// `begin_issue_dispatch`'s label flip and `finish_issue_dispatch`'s
    /// journal write leaves behind. `reconcile_workspace(is_startup = true)`
    /// must reclaim it in the very first post-restart pass.
    #[test]
    #[serial]
    fn reconcile_workspace_reclaims_zero_evidence_immediately_when_is_startup_true() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
        // No journal entry, no checkpoint written at all for issue 90.

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339(); // fresh label -- would Keep under the age gate
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 90, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, true);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "zero evidence at all must reclaim immediately on the startup pass, even with a \
             fresh label (#6615)"
        );
        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 90 --remove-label loom:building --add-label loom:issue"),
            "expected reclaim to flip labels for #90; got: {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// The steady-state counterpart (curator's Test Plan edge case 1): the
    /// IDENTICAL zero-evidence, fresh-label shape must NOT be reclaimed when
    /// `is_startup = false` (the periodic pass) -- this is exactly what
    /// protects a manually/externally spawned `/loom:sweep` that has not yet
    /// written a journal entry.
    #[test]
    #[serial]
    fn reconcile_workspace_does_not_reclaim_zero_evidence_when_not_startup() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 91, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 0,
            "the periodic pass must keep protecting a manually-spawned /loom:sweep with no \
             journal entry yet (#6615 must not weaken the existing steady-state age gate)"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    // ------------------------------------------------------------------
    // Integration: exit-0/no-progress fast reclaim (Issue #4462)
    // ------------------------------------------------------------------

    /// The #4462 incident, end to end: an in-session sweep reached
    /// `curator-done`, then died to a transport-failure backoff and exited 0
    /// (its run-registry entry cleaned up at exit — so there is a checkpoint
    /// but NO run-registry join). The label is FRESH (well within the age
    /// grace), and no open PR exists. `reconcile_workspace` must reclaim it
    /// within one pass via the fast no-progress path, not wait out the
    /// (hours-long) age gate.
    #[test]
    #[serial]
    fn reconcile_workspace_reclaims_exited_no_progress_curator_done_no_pr() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // Checkpoint stalled at curator-done, and DELIBERATELY no run-registry
        // entry (the in-session sweep's entry was cleaned up at exit).
        seed_checkpoint_phase(&repo_root, 80, "curator-done");

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339(); // fresh label
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 80, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "a curator-done checkpoint with no run-registry pid and no open PR must be reclaimed \
             fast even with a fresh label (#4462)"
        );
        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 80 --remove-label loom:building --add-label loom:issue"),
            "expected reclaim to flip labels for #80; got: {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// The Issue #4616 regression, end to end: a resumed Builder retry looks
    /// byte-for-byte identical to the #4462 orphan (checkpoint at
    /// `curator-done`, no run-registry join, no open PR) for the first few
    /// minutes after it resumes — the checkpoint's `task_id` is only rewritten
    /// on Builder *completion*, not on resume, so the join legitimately
    /// resolves to nothing. `reconcile_workspace` must NOT reclaim while the
    /// checkpoint's own timestamp is still within the no-progress grace
    /// window, even though the `loom:building` label itself may be old (from
    /// the ORIGINAL, now-superseded claim).
    #[test]
    #[serial]
    fn reconcile_workspace_keeps_exited_no_progress_within_grace_window() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // Checkpoint stalled at curator-done, no run-registry join, but its
        // own timestamp is only 2 minutes old -- well within the default
        // 10-minute grace period (a fresh resume, not a proven orphan).
        let fresh_timestamp = Utc::now() - chrono::Duration::minutes(2);
        seed_checkpoint_phase_with_timestamp(
            &repo_root,
            83,
            "curator-done",
            &fresh_timestamp.to_rfc3339(),
        );

        // The `loom:building` label itself is OLD (the original claim, long
        // before this resumed attempt) -- deliberately outside the age rule's
        // own grace, so a Keep here can only be explained by the no-progress
        // grace window, not a fall-through to a still-fresh label.
        let gh_log = dir.path().join("gh-invocations.log");
        let old_label = (Utc::now() - chrono::Duration::hours(5)).to_rfc3339();
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 83, &old_label);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 0,
            "a curator-done checkpoint whose OWN timestamp is within the no-progress grace \
             window must be kept -- it is indistinguishable from a legitimately-resumed \
             Builder retry (#4616)"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// A `builder-done` checkpoint (past the pre-Builder phase — a PR is
    /// expected to exist) must NOT trip the fast no-progress reclaim; with a
    /// fresh label it falls through to the age gate and is kept.
    #[test]
    #[serial]
    fn reconcile_workspace_no_fast_reclaim_when_checkpoint_past_curator_done() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        seed_checkpoint_phase(&repo_root, 81, "builder-done");

        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339(); // fresh label
        let fake_gh = write_fake_gh(dir.path(), &gh_log, 81, &now);

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 0,
            "only the pre-Builder curator-done phase may fast-reclaim; builder-done and later \
             defer to the resume/age machinery (#4462)"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// A `curator-done` checkpoint but an OPEN linked PR exists — the sweep did
    /// produce something, so the fast no-progress reclaim must NOT fire.
    #[test]
    #[serial]
    fn reconcile_workspace_no_fast_reclaim_when_open_pr_exists() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        seed_checkpoint_phase(&repo_root, 82, "curator-done");

        // Fake gh that reports an OPEN PR for `pr list` (so no_progress=false).
        let gh_log = dir.path().join("gh-invocations.log");
        let now = Utc::now().to_rfc3339();
        let fake_gh = dir.path().join("fake-gh-with-pr.sh");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "api" ]; then
  printf 'HTTP/2.0 200 OK\r\n\r\n'
  echo '[{{"number":82,"state":"open","labels":[{{"name":"loom:building"}}],"updated_at":"{now}"}}]'
  exit 0
fi
if [ "$1" = "pr" ]; then
  echo 4242
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root, false);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 0,
            "an open linked PR means the sweep produced progress -- no fast reclaim (#4462)"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    // ------------------------------------------------------------------
    // PR-side claim labels: decide_pr / plan_pr (Issue #4367)
    // ------------------------------------------------------------------

    fn claimed_pr(
        number: u32,
        updated_at: Option<DateTime<Utc>>,
        head_ref_name: Option<&str>,
    ) -> ClaimedPr {
        // claim_labeled_at/most_recent_claim_activity_at intentionally
        // left unset here so every existing caller of this helper keeps
        // exercising the pre-#4618 updated_at-only fallback path unchanged;
        // the #4618/#4638 regression tests below construct `ClaimedPr`
        // directly to set them.
        ClaimedPr {
            number,
            updated_at,
            claim_labeled_at: None,
            most_recent_claim_activity_at: None,
            head_ref_name: head_ref_name.map(ToString::to_string),
        }
    }

    #[test]
    fn decide_pr_keeps_when_fresh_and_no_join() {
        // fresh-kept: no journal entry, no run-registry pid, and the PR was
        // updated recently -- well within the staleness window.
        let now = Utc::now();
        let fresh = now - Duration::minutes(1);
        let pr = claimed_pr(100, Some(fresh), None);
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        assert_eq!(action, PrReconcileAction::Keep);
    }

    #[test]
    fn decide_pr_reclaims_when_stale_and_no_join() {
        // stale-reclaimed: no journal entry, no run-registry pid, and the PR
        // has aged well past the staleness threshold.
        let now = Utc::now();
        let old = now - Duration::minutes(60);
        let pr = claimed_pr(101, Some(old), None);
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        match action {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
                assert!(age_minutes >= 30.0);
            }
            other => panic!("expected Aged reclaim, got {other:?}"),
        }
    }

    #[test]
    fn decide_pr_keeps_when_journal_pid_alive_even_if_stale() {
        // live-pid-kept: a live joined pid short-circuits to Keep
        // unconditionally, regardless of how stale the label is.
        let now = Utc::now();
        let old = now - Duration::minutes(120);
        let entry = journal_entry("/repo/a", 42, 111);
        let pr = claimed_pr(102, Some(old), Some("feature/issue-42"));
        let action = decide_pr(&pr, Some(&entry), None, &|_| true, 30.0, now);
        assert_eq!(action, PrReconcileAction::Keep);
    }

    #[test]
    fn decide_pr_keeps_when_journal_pid_dead_but_fresh() {
        // dead-pid-but-fresh-kept: the age gate applies unconditionally, even
        // to a dead joined pid -- a fresh label is kept regardless.
        let now = Utc::now();
        let fresh = now - Duration::minutes(1);
        let entry = journal_entry("/repo/a", 42, 111);
        let pr = claimed_pr(103, Some(fresh), Some("feature/issue-42"));
        let action = decide_pr(&pr, Some(&entry), None, &|_| false, 30.0, now);
        assert_eq!(action, PrReconcileAction::Keep);
    }

    #[test]
    fn decide_pr_reclaims_when_journal_pid_dead_and_stale() {
        // A dead joined pid AND an aged label together -- reclaims, carrying
        // the DeadPid reason through (not a generic Aged).
        let now = Utc::now();
        let old = now - Duration::minutes(45);
        let entry = journal_entry("/repo/a", 42, 111);
        let pr = claimed_pr(104, Some(old), Some("feature/issue-42"));
        let action = decide_pr(&pr, Some(&entry), None, &|_| false, 30.0, now);
        assert_eq!(action, PrReconcileAction::Reclaim(PrReclaimReason::DeadPid { pid: 111 }));
    }

    #[test]
    fn decide_pr_keeps_when_no_updated_at() {
        // no-updatedAt-kept: missing/unparseable `updatedAt` fails safe to
        // Keep, even with no join evidence at all.
        let now = Utc::now();
        let pr = claimed_pr(105, None, None);
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        assert_eq!(action, PrReconcileAction::Keep, "fail-safe: no updatedAt => Keep");
    }

    #[test]
    fn decide_pr_falls_through_to_age_rule_on_non_joinable_branch() {
        // non-joinable-branch: a head ref that doesn't match
        // `feature/issue-<N>` has no join key at all -- decide_pr still
        // reaches the age rule and reclaims once stale.
        let now = Utc::now();
        let old = now - Duration::minutes(90);
        let pr = claimed_pr(106, Some(old), Some("some-other-branch-name"));
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        match action {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { .. }) => {}
            other => panic!("expected Aged reclaim, got {other:?}"),
        }
    }

    #[test]
    fn decide_pr_reclaims_when_run_registry_pid_dead_and_stale() {
        let now = Utc::now();
        let old = now - Duration::minutes(90);
        let pr = claimed_pr(107, Some(old), Some("feature/issue-42"));
        let action = decide_pr(&pr, None, Some(999), &|_| false, 30.0, now);
        assert_eq!(
            action,
            PrReconcileAction::Reclaim(PrReclaimReason::DeadRunRegistry { pid: 999 })
        );
    }

    #[test]
    fn decide_pr_journal_entry_takes_priority_over_run_registry_pid() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let pr = claimed_pr(108, None, Some("feature/issue-42"));
        let action = decide_pr(&pr, Some(&entry), Some(999), &|pid| pid == 111, 30.0, now);
        assert_eq!(action, PrReconcileAction::Keep);
    }

    // ------------------------------------------------------------------
    // decide_pr: claim_labeled_at freshness signal (Issue #4618 — PR #4614
    // stand-down-comment livelock regression coverage)
    // ------------------------------------------------------------------

    #[test]
    fn decide_pr_reclaims_via_claim_labeled_at_despite_standdown_inflated_updated_at() {
        // Reproduces the exact PR #4614 shape: the claim label itself was
        // applied 35 minutes ago (well past the 30-minute reviewing
        // threshold) and never re-applied since, but 2+ "standing down, not
        // stomping" comments posted by later Judge passes bumped the PR's
        // aggregate `updatedAt` to a few seconds ago -- each stand-down
        // comment self-refreshing the very signal the pre-#4618 code used to
        // decide freshness. `claim_labeled_at` is immune to that: it only
        // moves when the label is genuinely re-applied, so the reclaim now
        // fires correctly despite the inflated `updated_at`.
        let now = Utc::now();
        let claimed_at = now - Duration::minutes(35);
        let standdown_inflated = now - Duration::seconds(5);
        let pr = ClaimedPr {
            number: 4614,
            updated_at: Some(standdown_inflated),
            claim_labeled_at: Some(claimed_at),
            most_recent_claim_activity_at: None,
            head_ref_name: Some("some-doctor-branch".to_string()),
        };
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        match action {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
                assert!(
                    age_minutes >= 30.0,
                    "expected age derived from claim_labeled_at (~35m), got {age_minutes}"
                );
            }
            other => panic!(
                "expected an Aged reclaim driven by claim_labeled_at, got {other:?} \
                 (stand-down-comment-inflated updated_at must not mask staleness)"
            ),
        }
    }

    #[test]
    fn decide_pr_keeps_when_claim_labeled_at_is_fresh_even_if_updated_at_is_old() {
        // The inverse of the case above, for completeness: a fresh
        // claim_labeled_at (recent reclaim) must read as fresh even when
        // updated_at happens to be stale (e.g. a partial/lagging API field),
        // confirming claim_labeled_at is genuinely primary, not just an
        // additional condition.
        let now = Utc::now();
        let recent_claim = now - Duration::minutes(1);
        let stale_updated_at = now - Duration::minutes(90);
        let pr = ClaimedPr {
            number: 4615,
            updated_at: Some(stale_updated_at),
            claim_labeled_at: Some(recent_claim),
            most_recent_claim_activity_at: None,
            head_ref_name: None,
        };
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        assert_eq!(action, PrReconcileAction::Keep);
    }

    #[test]
    fn decide_pr_falls_back_to_updated_at_when_claim_labeled_at_unresolvable() {
        // When the timeline fetch failed/returned nothing (claim_labeled_at
        // is None), decide_pr must fall back to updated_at exactly like the
        // pre-#4618 behavior -- this is the fail-open case, not a second
        // route to the bug: a caller-side fetch failure should never be
        // amplified into either a spurious reclaim or a permanently-fresh
        // claim.
        let now = Utc::now();
        let old = now - Duration::minutes(60);
        let pr = claimed_pr(4616, Some(old), None);
        assert!(pr.claim_labeled_at.is_none());
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        match action {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { .. }) => {}
            other => panic!("expected fallback-to-updated_at Aged reclaim, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // decide_pr: most_recent_claim_activity_at anchor (Issue #4638 —
    // restoring protection for a genuinely live, non-pid-joinable claimant
    // after #4618 anchored solely on claim_labeled_at)
    // ------------------------------------------------------------------

    #[test]
    fn decide_pr_keeps_when_old_claim_labeled_at_but_recent_genuine_comment() {
        // The exact #4638 shape: claim_labeled_at is 35 minutes old (past the
        // 30-minute threshold) and the PR is not pid-joinable (no journal
        // entry, no run-registry pid), but a claimant heartbeat carrying this
        // claim's activity marker was posted 1 minute ago (#6523) --
        // most_recent_claim_activity_at must refresh the anchor and the claim
        // must be kept, not reclaimed out from under a still-working claimant.
        let now = Utc::now();
        let claimed_at = now - Duration::minutes(35);
        let recent_genuine_comment = now - Duration::minutes(1);
        let pr = ClaimedPr {
            number: 4638,
            updated_at: Some(claimed_at),
            claim_labeled_at: Some(claimed_at),
            most_recent_claim_activity_at: Some(recent_genuine_comment),
            head_ref_name: Some("pr-worktree-review-branch".to_string()),
        };
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        assert_eq!(
            action,
            PrReconcileAction::Keep,
            "a genuine recent comment must refresh the anchor and prevent reclaim"
        );
    }

    #[test]
    fn decide_pr_reclaims_when_old_claim_labeled_at_and_only_standdown_comments_since() {
        // Regression guard for #4618: the caller-side comment fetch excludes
        // marker-tagged stand-down comments, so a claim whose only comments
        // since the claim are stand-down notes surfaces
        // most_recent_claim_activity_at == None here (exactly like no
        // comments at all) -- the anchor must fall back to claim_labeled_at
        // alone and the stale claim must still be reclaimed. This is the
        // #4618 livelock this fix must not reopen.
        let now = Utc::now();
        let claimed_at = now - Duration::minutes(35);
        let pr = ClaimedPr {
            number: 4618,
            updated_at: Some(now - Duration::seconds(5)),
            claim_labeled_at: Some(claimed_at),
            most_recent_claim_activity_at: None,
            head_ref_name: Some("some-doctor-branch".to_string()),
        };
        let action = decide_pr(&pr, None, None, &|_| true, 30.0, now);
        match action {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
                assert!(age_minutes >= 30.0);
            }
            other => panic!(
                "expected Aged reclaim -- marker-only comments must not refresh the anchor, got {other:?}"
            ),
        }
    }

    // ------------------------------------------------------------------
    // Claim-activity marker: only the CLAIMANT's own heartbeat is liveness
    // (Issue #6523 — bringing the daemon side in line with #6514's
    // defaults/scripts/claim-staleness.sh)
    // ------------------------------------------------------------------

    /// The claim timestamp used by the marker fixtures below, rendered exactly
    /// as the forge emits a `labeled` event's `created_at`.
    fn fixture_claimed_at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-19T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn comment(created_at: DateTime<Utc>, body: &str) -> PrComment {
        PrComment {
            created_at,
            body: body.to_string(),
        }
    }

    #[test]
    fn claim_activity_marker_matches_claim_staleness_sh() {
        // The marker MUST be byte-identical to what
        // `defaults/scripts/claim-staleness.sh marker` prints:
        //   ACTIVITY_PREFIX='<!-- loom:claim-activity claim=' + CLAIMED_AT + ' -->'
        // with CLAIMED_AT the timeline `created_at` verbatim (which that
        // script validates as ^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$).
        assert_eq!(CLAIM_ACTIVITY_MARKER_PREFIX, "<!-- loom:claim-activity claim=");
        assert_eq!(
            claim_activity_marker(fixture_claimed_at()),
            "<!-- loom:claim-activity claim=2026-08-19T08:00:00Z -->"
        );
    }

    #[test]
    fn most_recent_claim_activity_at_ignores_an_unrelated_comment() {
        // AC (a) / the #6513 shape reconstructed daemon-side: a routine
        // Builder post-push status note is not claimant liveness. Before
        // #6523 this comment WAS counted (it is not a stand-down note), which
        // is exactly the conflation #6514 removed on the agent side.
        let claimed_at = fixture_claimed_at();
        let comments = vec![
            comment(claimed_at + Duration::minutes(2), "Pushed the fix, CI running."),
            comment(claimed_at + Duration::minutes(9), "Champion: capped-PR notice."),
        ];
        assert_eq!(
            most_recent_claim_activity_at(&comments, claimed_at),
            None,
            "an unrelated comment must not count as claimant activity"
        );
    }

    #[test]
    fn most_recent_claim_activity_at_counts_a_marked_claimant_heartbeat() {
        // AC (b): a comment carrying THIS claim's marker is claimant liveness.
        let claimed_at = fixture_claimed_at();
        let heartbeat_at = claimed_at + Duration::minutes(12);
        let comments = vec![
            comment(claimed_at + Duration::minutes(2), "Pushed the fix, CI running."),
            comment(
                heartbeat_at,
                &format!(
                    "Doctor: still working the failing test.\n{}",
                    claim_activity_marker(claimed_at)
                ),
            ),
        ];
        assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), Some(heartbeat_at));
    }

    #[test]
    fn most_recent_claim_activity_at_takes_the_newest_marked_heartbeat() {
        let claimed_at = fixture_claimed_at();
        let marker = claim_activity_marker(claimed_at);
        let newest = claimed_at + Duration::minutes(20);
        let comments = vec![
            comment(claimed_at + Duration::minutes(5), &marker),
            comment(newest, &marker),
            comment(claimed_at + Duration::minutes(12), &marker),
        ];
        assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), Some(newest));
    }

    #[test]
    fn most_recent_claim_activity_at_ignores_a_marker_for_a_different_claim() {
        // Mirrors claim-staleness.sh: the marker is matched against the
        // claim's OWN labeled-at timestamp, so a heartbeat left behind by an
        // earlier claim generation (before a reclaim + re-claim) cannot keep
        // the new claim alive.
        let claimed_at = fixture_claimed_at();
        let older_claim = claimed_at - Duration::minutes(45);
        let comments = vec![comment(
            claimed_at + Duration::minutes(3),
            &format!("Judge: reviewing.\n{}", claim_activity_marker(older_claim)),
        )];
        assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
    }

    #[test]
    fn most_recent_claim_activity_at_ignores_comments_at_or_before_the_claim() {
        let claimed_at = fixture_claimed_at();
        let marker = claim_activity_marker(claimed_at);
        let comments = vec![
            comment(claimed_at - Duration::minutes(1), &marker),
            comment(claimed_at, &marker),
        ];
        assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
    }

    #[test]
    fn most_recent_claim_activity_at_still_excludes_standdown_comments() {
        // #4618 regression guard, preserved: a stand-down comment is evidence
        // a LATER pass declined to reclaim, never claimant activity — even in
        // the pathological case where its body quotes an activity marker.
        let claimed_at = fixture_claimed_at();
        let comments = vec![comment(
            claimed_at + Duration::minutes(7),
            &format!(
                "Judge pass: standing down, not stomping.\n{}\n{STANDDOWN_MARKER_PREFIX}2026-08-19T08:00:00Z seq=2 -->",
                claim_activity_marker(claimed_at)
            ),
        )];
        assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
    }

    #[test]
    fn decide_pr_reclaims_when_only_unrelated_comments_since_the_claim() {
        // End-to-end AC (a): the #6513 livelock shape, daemon-side. A 35m-old
        // `loom:reviewing` claim on a chatty PR whose only comments since are
        // a Builder status note and a Champion notice. The claim-activity scan
        // yields None, so the anchor stays at claim_labeled_at and the stale
        // claim is reclaimed. Before #6523 that Builder note refreshed the
        // anchor and bought the dead claim another full 30-minute window.
        let now = Utc::now();
        let claimed_at = now - Duration::minutes(35);
        let scanned = most_recent_claim_activity_at(
            &[
                comment(now - Duration::minutes(20), "Pushed the fix, CI running."),
                comment(now - Duration::minutes(2), "Champion: merge-risk hold."),
            ],
            claimed_at,
        );
        assert_eq!(scanned, None, "neither comment is claimant activity");
        let pr = ClaimedPr {
            number: 6523,
            updated_at: Some(now - Duration::minutes(2)),
            claim_labeled_at: Some(claimed_at),
            most_recent_claim_activity_at: scanned,
            head_ref_name: Some("some-judge-branch".to_string()),
        };
        match decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_REVIEWING_MINUTES, now) {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
                assert!(age_minutes >= DEFAULT_STALE_REVIEWING_MINUTES);
            }
            other => panic!(
                "expected an Aged reclaim -- an unrelated comment must not postpone it, got {other:?}"
            ),
        }
    }

    #[test]
    fn decide_pr_keeps_when_a_marked_claimant_heartbeat_is_recent() {
        // End-to-end AC (b): same shape as above, except the claimant itself
        // posted a marked heartbeat 2 minutes ago -- that IS liveness, so the
        // claim is kept.
        let now = Utc::now();
        let claimed_at = now - Duration::minutes(35);
        let heartbeat_at = now - Duration::minutes(2);
        let scanned = most_recent_claim_activity_at(
            &[
                comment(now - Duration::minutes(20), "Pushed the fix, CI running."),
                comment(heartbeat_at, &claim_activity_marker(claimed_at)),
            ],
            claimed_at,
        );
        assert_eq!(scanned, Some(heartbeat_at));
        let pr = ClaimedPr {
            number: 6523,
            updated_at: Some(heartbeat_at),
            claim_labeled_at: Some(claimed_at),
            most_recent_claim_activity_at: scanned,
            head_ref_name: Some("some-judge-branch".to_string()),
        };
        assert_eq!(
            decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_REVIEWING_MINUTES, now),
            PrReconcileAction::Keep
        );
    }

    #[test]
    fn decide_pr_marked_heartbeat_extends_by_exactly_one_window_not_indefinitely() {
        // AC (c): the anchor is a max() of timestamps, not a boolean pin, so a
        // heartbeat buys exactly one more staleness window measured from the
        // HEARTBEAT's own timestamp -- matching claim-staleness.sh's "activity
        // resets the idle clock" rule. Same claim, same single heartbeat,
        // evaluated at two moments either side of that window.
        let claimed_at = Utc::now() - Duration::minutes(180);
        let heartbeat_at = claimed_at + Duration::minutes(5);
        let scanned = most_recent_claim_activity_at(
            &[comment(heartbeat_at, &claim_activity_marker(claimed_at))],
            claimed_at,
        );
        assert_eq!(scanned, Some(heartbeat_at));
        let pr = ClaimedPr {
            number: 6523,
            updated_at: Some(heartbeat_at),
            claim_labeled_at: Some(claimed_at),
            most_recent_claim_activity_at: scanned,
            head_ref_name: None,
        };

        // Just inside the window from the heartbeat: kept.
        let inside = heartbeat_at + Duration::minutes(29);
        assert_eq!(
            decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_REVIEWING_MINUTES, inside),
            PrReconcileAction::Keep,
            "within one threshold window of the heartbeat the claim is still fresh"
        );

        // One minute past it: reclaimed. A single heartbeat cannot pin the
        // claim, however old the claim itself gets.
        let outside = heartbeat_at + Duration::minutes(31);
        match decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_REVIEWING_MINUTES, outside) {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
                assert!(
                    (age_minutes - 31.0).abs() < 0.5,
                    "age must be measured from the heartbeat (~31m), got {age_minutes}"
                );
            }
            other => {
                panic!("expected an Aged reclaim one window past the heartbeat, got {other:?}")
            }
        }

        // Treating's longer floor behaves identically, just later.
        assert_eq!(
            decide_pr(&pr, None, None, &|_| true, DEFAULT_STALE_TREATING_MINUTES, outside),
            PrReconcileAction::Keep,
            "60m treating window is not yet exhausted 31m after the heartbeat"
        );
        match decide_pr(
            &pr,
            None,
            None,
            &|_| true,
            DEFAULT_STALE_TREATING_MINUTES,
            heartbeat_at + Duration::minutes(61),
        ) {
            PrReconcileAction::Reclaim(PrReclaimReason::Aged { .. }) => {}
            other => panic!("expected an Aged reclaim 61m past the heartbeat, got {other:?}"),
        }
    }

    #[test]
    fn decide_pr_age_floor_vetoes_reclaim_regardless_of_comment_activity() {
        // SAFETY (#4790/#4618 double-claim race): #6523 narrows what counts as
        // activity, which can only make a reclaim happen SOONER -- never
        // sooner than the 30m/60m age floor, which stays the veto no
        // comment-activity outcome can bypass. A claim younger than its floor
        // with NO claimant activity at all (the most reclaim-favourable
        // evidence this pass can see) must still be kept -- including when a
        // dead joined pid is also on the table.
        assert!((DEFAULT_STALE_REVIEWING_MINUTES - 30.0).abs() < f64::EPSILON);
        assert!((DEFAULT_STALE_TREATING_MINUTES - 60.0).abs() < f64::EPSILON);
        let now = Utc::now();
        let dead = journal_entry("/repo/a", 6523, 4618);

        for (label, floor) in [
            ("loom:reviewing", DEFAULT_STALE_REVIEWING_MINUTES),
            ("loom:treating", DEFAULT_STALE_TREATING_MINUTES),
        ] {
            // One minute short of the floor.
            let claimed_at = now - Duration::seconds(((floor - 1.0) * 60.0) as i64);
            let pr = ClaimedPr {
                number: 6523,
                updated_at: Some(now - Duration::seconds(5)),
                claim_labeled_at: Some(claimed_at),
                most_recent_claim_activity_at: None,
                head_ref_name: Some("feature/issue-6523".to_string()),
            };
            assert_eq!(
                decide_pr(&pr, None, None, &|_| true, floor, now),
                PrReconcileAction::Keep,
                "{label}: under the age floor, no-activity must still Keep"
            );
            assert_eq!(
                decide_pr(&pr, Some(&dead), None, &|_| false, floor, now),
                PrReconcileAction::Keep,
                "{label}: the age floor vetoes even a dead joined pid"
            );
            assert_eq!(
                decide_pr(&pr, None, Some(4618), &|_| false, floor, now),
                PrReconcileAction::Keep,
                "{label}: the age floor vetoes even a dead run-registry pid"
            );

            // One minute past it: the same evidence now reclaims, confirming
            // the floor -- not the comment scan -- is what moved.
            let aged = ClaimedPr {
                claim_labeled_at: Some(now - Duration::seconds(((floor + 1.0) * 60.0) as i64)),
                ..pr.clone()
            };
            match decide_pr(&aged, None, None, &|_| true, floor, now) {
                PrReconcileAction::Reclaim(PrReclaimReason::Aged { age_minutes }) => {
                    assert!(age_minutes >= floor, "{label}: {age_minutes} < {floor}");
                }
                other => panic!("{label}: expected an Aged reclaim past the floor, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_issue_from_branch_matches_convention() {
        assert_eq!(parse_issue_from_branch("feature/issue-42"), Some(42));
        assert_eq!(parse_issue_from_branch("feature/issue-4367"), Some(4367));
    }

    #[test]
    fn parse_issue_from_branch_rejects_non_matching_shapes() {
        assert_eq!(parse_issue_from_branch("main"), None);
        assert_eq!(parse_issue_from_branch("fix/something"), None);
        assert_eq!(parse_issue_from_branch("feature/issue-"), None);
        assert_eq!(parse_issue_from_branch("feature/issue-abc"), None);
    }

    #[test]
    fn plan_pr_joins_branch_to_journal_entry() {
        let now = Utc::now();
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry("/repo/a", 42, 111)); // will be dead

        let prs = vec![
            claimed_pr(200, Some(now - Duration::minutes(45)), Some("feature/issue-42")),
            claimed_pr(201, Some(now - Duration::minutes(1)), None),
        ];

        let decisions = plan_pr("/repo/a", &prs, &journal, &|_| None, &|_| false, 30.0, now);

        assert_eq!(
            decisions[0],
            (200, PrReconcileAction::Reclaim(PrReclaimReason::DeadPid { pid: 111 }))
        );
        assert_eq!(decisions[1], (201, PrReconcileAction::Keep));
    }

    #[test]
    fn plan_pr_consults_run_registry_only_when_journal_entry_absent() {
        let now = Utc::now();
        let journal = SweepJournal::default();

        let prs = vec![
            claimed_pr(300, Some(now - Duration::minutes(60)), Some("feature/issue-1")),
            claimed_pr(301, Some(now - Duration::minutes(1)), Some("feature/issue-2")),
        ];

        let run_registry_pid_for = |issue_num: u32| -> Option<u32> {
            match issue_num {
                1 => Some(555),
                2 => Some(777),
                _ => None,
            }
        };
        let is_alive = |pid: u32| pid == 777;

        let decisions =
            plan_pr("/repo/a", &prs, &journal, &run_registry_pid_for, &is_alive, 30.0, now);

        assert_eq!(
            decisions[0],
            (300, PrReconcileAction::Reclaim(PrReclaimReason::DeadRunRegistry { pid: 555 }))
        );
        assert_eq!(decisions[1], (301, PrReconcileAction::Keep));
    }

    #[test]
    #[serial]
    fn resolve_stale_reviewing_minutes_defaults_and_overrides() {
        std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
        assert!(
            (resolve_stale_reviewing_minutes() - DEFAULT_STALE_REVIEWING_MINUTES).abs()
                < f64::EPSILON
        );

        std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "15");
        assert!((resolve_stale_reviewing_minutes() - 15.0).abs() < f64::EPSILON);

        std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "0");
        assert!(
            (resolve_stale_reviewing_minutes() - DEFAULT_STALE_REVIEWING_MINUTES).abs()
                < f64::EPSILON
        );
        std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "garbage");
        assert!(
            (resolve_stale_reviewing_minutes() - DEFAULT_STALE_REVIEWING_MINUTES).abs()
                < f64::EPSILON
        );

        std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
    }

    #[test]
    #[serial]
    fn resolve_stale_treating_minutes_defaults_and_overrides() {
        std::env::remove_var(STALE_TREATING_MINUTES_ENV);
        assert!(
            (resolve_stale_treating_minutes() - DEFAULT_STALE_TREATING_MINUTES).abs()
                < f64::EPSILON
        );

        std::env::set_var(STALE_TREATING_MINUTES_ENV, "90");
        assert!((resolve_stale_treating_minutes() - 90.0).abs() < f64::EPSILON);

        std::env::remove_var(STALE_TREATING_MINUTES_ENV);
    }

    // ------------------------------------------------------------------
    // Integration: forge::reconcile_pr_claims (Issue #4367)
    // ------------------------------------------------------------------

    /// Write a fake `gh` script (tests only) that logs every invocation to
    /// `gh_log`, reports exactly one PR carrying the requested claim label
    /// for `pr list`, and reports `extra_labels` (plus nothing else) for
    /// `pr view --json labels` -- letting a test control whether the
    /// safety-net `loom:review-requested` backfill should fire.
    fn write_fake_gh_pr(
        dir: &std::path::Path,
        gh_log: &std::path::Path,
        pr_number: u32,
        updated_at: &str,
        head_ref_name: &str,
        extra_labels: &[&str],
    ) -> std::path::PathBuf {
        let fake_gh = dir.join("fake-gh-pr.sh");
        let labels_json = extra_labels
            .iter()
            .map(|l| format!(r#"{{"name":"{l}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":{pr_number},"updatedAt":"{updated_at}","headRefName":"{head_ref_name}"}}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"labels":[{labels_json}]}}'
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }
        fake_gh
    }

    #[test]
    #[serial]
    fn reconcile_pr_claims_reclaims_stale_reviewing_and_backfills_state_label() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
        std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "30");
        std::env::set_var(STALE_TREATING_MINUTES_ENV, "60");

        // No journal entry, no checkpoint -- non-joinable branch, well past
        // the 30-minute threshold, and no state label at all.
        let gh_log = dir.path().join("gh-invocations.log");
        let old = (Utc::now() - Duration::minutes(90)).to_rfc3339();
        let fake_gh = write_fake_gh_pr(dir.path(), &gh_log, 500, &old, "some-random-branch", &[]);

        let (checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

        // Only `loom:reviewing` is queried with results here (the fake `gh`
        // returns the same single PR for every `pr list` call, so both the
        // reviewing and treating passes see it) -- assert on the label-flip
        // evidence instead of the exact checked count to avoid overfitting
        // to that fixture quirk.
        assert!(checked >= 1);
        assert!(reclaimed >= 1);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("pr edit 500 --remove-label loom:reviewing"),
            "expected loom:reviewing to be removed from #500; got: {gh_calls:?}"
        );
        assert!(
            gh_calls.contains("pr edit 500 --add-label loom:review-requested"),
            "expected the safety net to add loom:review-requested to #500; got: {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
        std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
        std::env::remove_var(STALE_TREATING_MINUTES_ENV);
    }

    #[test]
    #[serial]
    fn reconcile_pr_claims_keeps_fresh_pr() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);
        std::env::set_var(STALE_REVIEWING_MINUTES_ENV, "30");
        std::env::set_var(STALE_TREATING_MINUTES_ENV, "60");

        let gh_log = dir.path().join("gh-invocations.log");
        let fresh = Utc::now().to_rfc3339();
        let fake_gh = write_fake_gh_pr(
            dir.path(),
            &gh_log,
            501,
            &fresh,
            "some-random-branch",
            &["loom:review-requested"],
        );

        let (_checked, reclaimed) = forge::reconcile_pr_claims(&fake_gh, &repo_root);

        assert_eq!(reclaimed, 0, "a fresh PR-side claim must never be reclaimed");

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("--remove-label loom:reviewing")
                && !gh_calls.contains("--remove-label loom:treating"),
            "no claim label should have been removed; got: {gh_calls:?}"
        );

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
        std::env::remove_var(STALE_REVIEWING_MINUTES_ENV);
        std::env::remove_var(STALE_TREATING_MINUTES_ENV);
    }

    // Issue #4637: `gh api --paginate --jq` re-invokes the `--jq` filter once
    // per page and concatenates the per-page results, so a `max // empty`
    // filter against a timeline spanning more than one page (>100 events)
    // yields one line per page rather than a single overall max.
    // `parse_max_timestamp` must resolve the true max across every line.

    #[test]
    fn parse_max_timestamp_single_line_bare() {
        let stdout = b"2026-01-01T00:00:00Z\n";
        let parsed = forge::parse_max_timestamp(stdout).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }

    #[test]
    fn parse_max_timestamp_multi_page_picks_max_out_of_order() {
        // Three pages' worth of per-page `max` lines, deliberately not in
        // chronological order, mirroring what `--paginate` concatenation
        // actually produces.
        let stdout = b"2026-01-01T00:00:00Z\n2026-03-15T12:30:00Z\n2026-02-01T00:00:00Z\n";
        let parsed = forge::parse_max_timestamp(stdout).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-03-15T12:30:00+00:00");
    }

    #[test]
    fn parse_max_timestamp_multi_page_skips_empty_and_null_lines() {
        // A page with no matching event emits an empty line (the `// empty`
        // fallback) or a literal `null`; both must be ignored, not treated
        // as "no timestamp anywhere".
        let stdout = b"\n2026-05-05T05:05:05Z\nnull\n";
        let parsed = forge::parse_max_timestamp(stdout).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-05-05T05:05:05+00:00");
    }

    #[test]
    fn parse_max_timestamp_returns_none_for_empty_output() {
        assert!(forge::parse_max_timestamp(b"").is_none());
        assert!(forge::parse_max_timestamp(b"\n\n").is_none());
        assert!(forge::parse_max_timestamp(b"null\n").is_none());
        assert!(forge::parse_max_timestamp(b"null\nnull\n").is_none());
    }

    #[test]
    fn parse_max_timestamp_returns_none_for_garbage() {
        assert!(forge::parse_max_timestamp(b"not-a-timestamp\n").is_none());
        assert!(forge::parse_max_timestamp(b"not-a-timestamp\nalso-not-one\n").is_none());
    }

    #[test]
    fn parse_max_timestamp_handles_json_quoted_lines() {
        let stdout = b"\"2026-01-01T00:00:00Z\"\n\"2026-06-06T06:06:06Z\"\n";
        let parsed = forge::parse_max_timestamp(stdout).unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-06-06T06:06:06+00:00");
    }

    // --- #5686 stale verdicts (loom:pr / loom:changes-requested) ---

    const SHA_A: &str = "1111111111111111111111111111111111111111";
    const SHA_B: &str = "2222222222222222222222222222222222222222";

    fn marker(sha: &str, token: &str) -> String {
        format!("Reviewed.\n\n<!-- loom:verdict-sha sha={sha} verdict={token} -->")
    }

    fn verdict_pr(kind: VerdictKind, head: Option<&str>, marker_sha: Option<&str>) -> VerdictPr {
        VerdictPr {
            number: 192,
            kind,
            head_sha: head.map(str::to_string),
            marker_sha: marker_sha.map(str::to_string),
            // The ordinary case: the PR's comments WERE read, so a `None`
            // marker means "confirmed unmarked" rather than "not looked at".
            marker_scan_ok: true,
            on_hold: false,
        }
    }

    #[test]
    fn verdict_kind_label_and_marker_token_match_the_prompt_convention() {
        assert_eq!(VerdictKind::Approved.label(), "loom:pr");
        assert_eq!(VerdictKind::Approved.marker_token(), "approved");
        assert_eq!(VerdictKind::ChangesRequested.label(), "loom:changes-requested");
        assert_eq!(VerdictKind::ChangesRequested.marker_token(), "changes-requested");
    }

    #[test]
    fn extract_verdict_sha_takes_the_newest_marker_of_the_matching_kind() {
        let bodies = vec![
            marker(SHA_A, "changes-requested"),
            "Doctor pushed a fix.".to_string(),
            marker(SHA_B, "changes-requested"),
        ];
        assert_eq!(
            extract_latest_verdict_sha(&bodies, VerdictKind::ChangesRequested),
            Some(SHA_B.to_string())
        );
    }

    #[test]
    fn extract_verdict_sha_filters_on_verdict_kind() {
        // A PR rejected at SHA_A and later approved at SHA_B carries markers
        // for BOTH. Only the one matching the currently-held label says
        // anything about the current verdict -- taking "newest marker of any
        // kind" would let the approval marker vouch for the rejection.
        let bodies = vec![
            marker(SHA_A, "changes-requested"),
            marker(SHA_B, "approved"),
        ];
        assert_eq!(
            extract_latest_verdict_sha(&bodies, VerdictKind::Approved),
            Some(SHA_B.to_string())
        );
        assert_eq!(
            extract_latest_verdict_sha(&bodies, VerdictKind::ChangesRequested),
            Some(SHA_A.to_string())
        );
    }

    #[test]
    fn extract_verdict_sha_is_none_without_a_marker_of_that_kind() {
        let bodies = vec!["LGTM, approving.".to_string(), marker(SHA_A, "approved")];
        assert_eq!(extract_latest_verdict_sha(&bodies, VerdictKind::ChangesRequested), None);
        assert_eq!(extract_latest_verdict_sha(&[], VerdictKind::Approved), None);
    }

    #[test]
    fn extract_verdict_sha_ignores_a_malformed_marker() {
        // Non-hex / too-short SHAs and a missing verdict= token must not
        // produce a bogus marker_sha that could invalidate a live verdict.
        let bodies = vec![
            "<!-- loom:verdict-sha sha=zzzz verdict=approved -->".to_string(),
            "<!-- loom:verdict-sha sha=1111 verdict=approved -->".to_string(),
            format!("<!-- loom:verdict-sha sha={SHA_A} -->"),
        ];
        assert_eq!(extract_latest_verdict_sha(&bodies, VerdictKind::Approved), None);
    }

    #[test]
    fn decide_verdict_keeps_when_the_marker_matches_the_current_head() {
        assert_eq!(
            decide_verdict(&verdict_pr(VerdictKind::ChangesRequested, Some(SHA_A), Some(SHA_A))),
            VerdictAction::Keep(VerdictKeepReason::Fresh)
        );
    }

    #[test]
    fn decide_verdict_invalidates_a_rejection_after_a_force_push() {
        // The rjwalters/repo#192 incident: verdict rendered at SHA_A, branch
        // rebased+force-pushed to SHA_B, label never moved.
        assert_eq!(
            decide_verdict(&verdict_pr(VerdictKind::ChangesRequested, Some(SHA_B), Some(SHA_A))),
            VerdictAction::Invalidate {
                marker_sha: SHA_A.to_string(),
                head_sha: SHA_B.to_string(),
            }
        );
    }

    #[test]
    fn decide_verdict_invalidates_a_stale_approval_the_dangerous_direction() {
        assert_eq!(
            decide_verdict(&verdict_pr(VerdictKind::Approved, Some(SHA_B), Some(SHA_A))),
            VerdictAction::Invalidate {
                marker_sha: SHA_A.to_string(),
                head_sha: SHA_B.to_string(),
            }
        );
    }

    #[test]
    fn decide_verdict_fails_safe_without_a_marker() {
        // Every verdict written before #5686 shipped is in this state --
        // clearing them all on rollout is exactly what must NOT happen.
        assert_eq!(
            decide_verdict(&verdict_pr(VerdictKind::Approved, Some(SHA_B), None)),
            VerdictAction::Keep(VerdictKeepReason::Unverifiable)
        );
    }

    #[test]
    fn decide_verdict_fails_safe_without_a_head_sha() {
        assert_eq!(
            decide_verdict(&verdict_pr(VerdictKind::Approved, None, Some(SHA_A))),
            VerdictAction::Keep(VerdictKeepReason::NoHeadSha)
        );
    }

    #[test]
    fn decide_verdict_respects_an_explicit_hold() {
        // Stale, but clearing it would un-park a PR an operator (or
        // Champion's capped-PR recovery pass) deliberately held.
        let mut pr = verdict_pr(VerdictKind::ChangesRequested, Some(SHA_B), Some(SHA_A));
        pr.on_hold = true;
        assert_eq!(decide_verdict(&pr), VerdictAction::Keep(VerdictKeepReason::Held));
    }

    #[test]
    fn decide_verdict_accepts_an_abbreviated_marker_sha_that_prefixes_the_head() {
        let pr = verdict_pr(VerdictKind::Approved, Some(SHA_A), Some(&SHA_A[..8]));
        assert_eq!(decide_verdict(&pr), VerdictAction::Keep(VerdictKeepReason::Fresh));
    }

    #[test]
    fn verdict_hold_labels_cover_every_parking_label() {
        assert!(VERDICT_HOLD_LABELS.contains(&"loom:blocked"));
        assert!(VERDICT_HOLD_LABELS.contains(&"loom:operator"));
        assert!(VERDICT_HOLD_LABELS.contains(&"loom:operator-only"));
    }

    // --- #6319 anchoring an unmarked verdict --------------------------------
    //
    // The gap this closes: the verdict-sha marker exists only because
    // judge.md asks the model to append it, and in production it is dropped
    // roughly one verdict in four. Every dropped marker silently reinstates
    // the pre-#5686 hazard, and until now that state had no counter, no log
    // line, and no remediation anywhere in the daemon.

    #[test]
    fn decide_anchor_stamps_the_current_head_for_a_confirmed_unmarked_verdict() {
        // The observed production case: an approving verdict with no marker.
        assert_eq!(
            decide_anchor(&verdict_pr(VerdictKind::Approved, Some(SHA_B), None)),
            AnchorAction::Anchor {
                head_sha: SHA_B.to_string()
            }
        );
        assert_eq!(
            decide_anchor(&verdict_pr(VerdictKind::ChangesRequested, Some(SHA_A), None)),
            AnchorAction::Anchor {
                head_sha: SHA_A.to_string()
            }
        );
    }

    #[test]
    fn decide_anchor_treats_an_empty_marker_as_unmarked() {
        // decide_verdict folds `Some("")` into Unverifiable; the anchoring
        // pass must agree, or an empty marker would be permanently stuck.
        assert_eq!(
            decide_anchor(&verdict_pr(VerdictKind::Approved, Some(SHA_B), Some(""))),
            AnchorAction::Anchor {
                head_sha: SHA_B.to_string()
            }
        );
    }

    #[test]
    fn decide_anchor_never_touches_a_verdict_that_already_carries_a_marker() {
        // The AC that matters most: an already-marked verdict must behave
        // byte-for-byte as it did before #6319, fresh or stale.
        assert_eq!(
            decide_anchor(&verdict_pr(VerdictKind::Approved, Some(SHA_A), Some(SHA_A))),
            AnchorAction::Skip(AnchorSkipReason::AlreadyAnchored)
        );
        assert_eq!(
            decide_anchor(&verdict_pr(VerdictKind::Approved, Some(SHA_B), Some(SHA_A))),
            AnchorAction::Skip(AnchorSkipReason::AlreadyAnchored)
        );
    }

    #[test]
    fn decide_anchor_skips_a_held_pr() {
        // A held PR's comments are never fetched (list_verdict_prs skips the
        // call), so its marker state is unknown -- and a PR a human parked
        // should not collect automated comments either.
        let mut pr = verdict_pr(VerdictKind::Approved, Some(SHA_B), None);
        pr.on_hold = true;
        pr.marker_scan_ok = false;
        assert_eq!(decide_anchor(&pr), AnchorAction::Skip(AnchorSkipReason::Held));
    }

    #[test]
    fn decide_anchor_skips_when_the_comment_scan_failed() {
        // A failed comment fetch is indistinguishable from "no marker".
        // Anchoring on it would post one duplicate marker comment per tick
        // for the whole duration of a GitHub API outage.
        let mut pr = verdict_pr(VerdictKind::Approved, Some(SHA_B), None);
        pr.marker_scan_ok = false;
        assert_eq!(decide_anchor(&pr), AnchorAction::Skip(AnchorSkipReason::MarkerScanFailed));
    }

    #[test]
    fn decide_anchor_skips_without_a_resolvable_head_sha() {
        assert_eq!(
            decide_anchor(&verdict_pr(VerdictKind::Approved, None, None)),
            AnchorAction::Skip(AnchorSkipReason::NoHeadSha)
        );
        assert_eq!(
            decide_anchor(&verdict_pr(VerdictKind::Approved, Some(""), None)),
            AnchorAction::Skip(AnchorSkipReason::NoHeadSha)
        );
    }

    #[test]
    fn decide_anchor_only_ever_fires_where_decide_verdict_said_unverifiable() {
        // Structural invariant: anchoring is a remediation for exactly one
        // decide_verdict outcome. If it ever fired on a Fresh/Invalidate/Held
        // PR it would be writing a marker over a live verdict decision.
        for pr in [
            verdict_pr(VerdictKind::Approved, Some(SHA_A), Some(SHA_A)), // Fresh
            verdict_pr(VerdictKind::Approved, Some(SHA_B), Some(SHA_A)), // Invalidate
            verdict_pr(VerdictKind::Approved, None, Some(SHA_A)),        // NoHeadSha
        ] {
            assert_ne!(decide_verdict(&pr), VerdictAction::Keep(VerdictKeepReason::Unverifiable));
            assert!(matches!(decide_anchor(&pr), AnchorAction::Skip(_)));
        }
    }

    #[test]
    fn verdict_reconcile_stats_report_the_residual_unanchored_exposure() {
        let mut stats = VerdictReconcileStats {
            checked: 4,
            invalidated: 1,
            unverifiable: 3,
            anchored: 2,
        };
        assert_eq!(stats.residual_unverifiable(), 1);
        stats.merge(VerdictReconcileStats {
            checked: 2,
            invalidated: 0,
            unverifiable: 1,
            anchored: 0,
        });
        assert_eq!(stats.checked, 6);
        assert_eq!(stats.invalidated, 1);
        assert_eq!(stats.unverifiable, 4);
        assert_eq!(stats.anchored, 2);
        assert_eq!(stats.residual_unverifiable(), 2);
        // Never underflows if a future caller anchors without counting.
        let odd = VerdictReconcileStats {
            unverifiable: 0,
            anchored: 1,
            ..VerdictReconcileStats::default()
        };
        assert_eq!(odd.residual_unverifiable(), 0);
    }

    #[test]
    #[serial]
    fn verdict_anchoring_is_enabled_by_default_and_killable_by_env() {
        let prev = std::env::var(VERDICT_ANCHOR_ENABLED_ENV).ok();
        std::env::remove_var(VERDICT_ANCHOR_ENABLED_ENV);
        assert!(verdict_anchoring_enabled(), "must default to ON");
        for off in ["0", "false", "no", "off", "OFF"] {
            std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, off);
            assert!(!verdict_anchoring_enabled(), "{off} must disable anchoring");
        }
        std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, "1");
        assert!(verdict_anchoring_enabled());
        match prev {
            Some(v) => std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, v),
            None => std::env::remove_var(VERDICT_ANCHOR_ENABLED_ENV),
        }
    }

    #[test]
    #[serial]
    fn verdict_staleness_is_enabled_by_default_and_killable_by_env() {
        let prev = std::env::var(VERDICT_STALENESS_ENABLED_ENV).ok();
        std::env::remove_var(VERDICT_STALENESS_ENABLED_ENV);
        assert!(verdict_staleness_enabled(), "must default to ON");
        for off in ["0", "false", "no", "off", "OFF"] {
            std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, off);
            assert!(!verdict_staleness_enabled(), "{off} must disable the pass");
        }
        std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, "1");
        assert!(verdict_staleness_enabled());
        match prev {
            Some(v) => std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, v),
            None => std::env::remove_var(VERDICT_STALENESS_ENABLED_ENV),
        }
    }
}
