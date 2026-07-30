//! Reconciliation of stale `loom:building` claims across every managed
//! workspace (Issue #3953, acceptance criterion 3/4; promoted from a
//! startup-only pass to a periodic one by Issue #4348).
//!
//! The persisted [`crate::sweep_journal`] gives a fresh daemon (post-restart,
//! post-rate-limit-kill, post-upgrade) an authoritative liveness source that
//! survives the in-memory [`crate::sweep_registry::SweepRegistry`] being wiped
//! clean. This module is the consumer that turns that evidence into action:
//! for every registered workspace, list the open `loom:building` issues and
//! decide, per issue, whether the claim is still backed by a live sweep.
//!
//! ## Decision rule
//!
//! - A journal entry recording a **live** PID ⇒ [`ReconcileAction::Keep`] —
//!   the claim is genuinely in-flight.
//! - A journal entry recording a **dead** PID ⇒
//!   [`ReconcileAction::Reclaim`]`(`[`ReclaimReason::DeadPid`]`)` — the sweep
//!   behind this claim is provably gone.
//! - **No** journal entry (a manually/externally spawned `/loom:sweep` never
//!   writes one — only [`crate::sweep_registry::SweepRegistry::dispatch`]
//!   does), but the checkpoint→run-registry join described below resolves a
//!   pid ⇒ same live/dead split, reclaiming with
//!   [`ReclaimReason::DeadRunRegistry`] on a dead pid (Issue #4348 — this is
//!   the evidence source that recovers a detached sweep killed by an
//!   external `SIGKILL` while the daemon itself stays up).
//! - **No** evidence at all ⇒ reclaim only once the label has been stale
//!   longer than [`resolve_stale_hours`] (`updated_at` age), otherwise `Keep`
//!   ([`ReclaimReason::NoRecordStale`]). This mirrors the Python tool's
//!   label-age grace period philosophy (#3651): absence of evidence is not,
//!   by itself, proof of orphanhood — only *aged* absence is.
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
//!
//! Reclaiming removes only the stale claim label (the state label restores
//! discoverability by itself); as a safety net, if the PR is then left
//! carrying none of `loom:review-requested`/`loom:changes-requested`/`loom:pr`,
//! [`forge::reclaim_pr`] adds `loom:review-requested` so a fresh Judge pass
//! picks it back up. This pass runs from the same [`run_reconciliation_pass`]
//! entry point as the issue-side sweep, under the same
//! [`reconciliation_enabled`] kill switch — no separate wiring needed.
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
/// **Age-gate freshness signal (Issue #4618)**: the age gate below prefers
/// `pr.claim_labeled_at` (the claim label's own most recent `labeled`
/// timeline event) over `pr.updated_at` (the PR's aggregate "last modified"
/// timestamp). Before this fix, `updated_at` was the sole signal, and GitHub
/// bumps it on ANY comment — including a Judge/Doctor "standing down, not
/// stomping" comment posted by a later pass declining to reclaim. That made
/// the check perpetually self-refreshing: each stand-down comment satisfied
/// the very freshness test the next pass ran, so a claim could survive past
/// the staleness window once and then never be reclaimed again (PR #4614).
/// `claim_labeled_at` cannot be bumped that way — it only changes when the
/// label is genuinely re-applied — so it is used whenever resolvable, with
/// `updated_at` kept only as the fail-open fallback for when the timeline
/// fetch itself failed or returned nothing (same fail-safe posture as the
/// pre-#4618 `None` branch below).
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
    // Prefer claim_labeled_at (Issue #4618) -- it cannot be self-refreshed by
    // a stand-down comment the way updated_at can. Only fall back to
    // updated_at when claim_labeled_at is unavailable (timeline fetch
    // failure/partial response), matching the pre-#4618 fail-open posture.
    match pr.claim_labeled_at.or(pr.updated_at) {
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
/// daemon-startup call site (`main.rs`) and
/// [`spawn_periodic_reconciliation_task`]'s interval loop — identical
/// behavior at both call sites, gated by the same [`reconciliation_enabled`]
/// kill switch.
pub fn run_reconciliation_pass(fallback_root: &Path) {
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
    for root in &roots {
        let (checked, reclaimed) = forge::reconcile_workspace(&gh_bin, root);
        total_checked += checked;
        total_reclaimed += reclaimed;
        let (pr_checked, pr_reclaimed) = forge::reconcile_pr_claims(&gh_bin, root);
        total_pr_checked += pr_checked;
        total_pr_reclaimed += pr_reclaimed;
    }
    if total_reclaimed > 0 {
        log::info!(
            "claim_reconciliation: pass checked {total_checked} loom:building issue(s) across \
             {} workspace(s), reclaimed {total_reclaimed} stale claim(s) (#4348)",
            roots.len()
        );
    } else {
        log::debug!(
            "claim_reconciliation: pass checked {total_checked} loom:building issue(s) across \
             {} workspace(s), nothing to reclaim",
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
            if let Err(e) =
                tokio::task::spawn_blocking(move || run_reconciliation_pass(&root)).await
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
        plan, plan_pr, resolve_no_progress_grace_minutes, resolve_stale_hours, BuildingIssue,
        ClaimedPr, NoProgressEvidence, PrClaimKind, PrReclaimReason, PrReconcileAction,
        ReclaimReason, ReconcileAction, MAX_ISSUES_PER_WORKSPACE,
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
        let output = cmd.output().ok()?;
        if !output.status.success() {
            return None;
        }
        let has_open = String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|l| l.trim().parse::<u32>().is_ok());
        Some(has_open)
    }

    fn reclaim(gh_bin: &Path, root: &Path, issue: u32) -> Result<()> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:building")
            .arg("--add-label")
            .arg("loom:issue");
        cmd.current_dir(root);
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
    pub fn reconcile_workspace(gh_bin: &Path, root: &Path) -> (usize, usize) {
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
        );
        let checked = decisions.len();

        let mut reclaimed = 0usize;
        for (issue_number, action) in decisions {
            let ReconcileAction::Reclaim(reason) = action else {
                continue;
            };
            match reclaim(gh_bin, root, issue_number) {
                Ok(()) => {
                    reclaimed += 1;
                    // #4348 acceptance criterion: WARN-level, with the
                    // issue number, last-known pid, and (when the evidence
                    // was the run-registry join) the run id -- an operator
                    // reading `daemon.log` after an unattended reclaim needs
                    // this without cross-referencing the journal by hand.
                    let last_known_pid =
                        match reason {
                            ReclaimReason::DeadPid { pid }
                            | ReclaimReason::DeadRunRegistry { pid } => Some(pid),
                            ReclaimReason::ExitedNoProgress
                            | ReclaimReason::NoRecordStale { .. } => None,
                        };
                    let run_id = matches!(reason, ReclaimReason::DeadRunRegistry { .. })
                        .then(|| super::read_checkpoint_task_id(root, issue_number))
                        .flatten();
                    log::warn!(
                        "claim_reconciliation: reclaimed loom:building -> loom:issue for #{issue_number} \
                         in {} ({reason:?}, last_known_pid={last_known_pid:?}, run_id={run_id:?})",
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
                ClaimedPr {
                    number: r.number,
                    updated_at: r
                        .updated_at
                        .as_deref()
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc)),
                    claim_labeled_at,
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
        let unquoted = trimmed.trim_matches('"');
        chrono::DateTime::parse_from_rfc3339(unquoted)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
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
        let repo = root.display().to_string();

        let journal_path = match sweep_journal::default_journal_path() {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "claim_reconciliation: cannot resolve journal path for PR-side pass: {e}"
                );
                return (0, 0);
            }
        };
        let journal = sweep_journal::load(&journal_path);
        let run_registry_pid_for = |issue: u32| super::resolve_run_registry_pid(root, issue);
        let now = chrono::Utc::now();

        let mut total_checked = 0usize;
        let mut total_reclaimed = 0usize;

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
                match reclaim_pr(gh_bin, root, pr_number, label) {
                    Ok(()) => {
                        total_reclaimed += 1;
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
                    }
                    Err(e) => {
                        log::warn!(
                            "claim_reconciliation: failed to reclaim {label} from PR #{pr_number} \
                             in {}: {e}",
                            root.display()
                        );
                    }
                }
            }
        }

        (total_checked, total_reclaimed)
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
        let action = decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_journal_entry_pid_dead() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action = decide(&issue(42, None), Some(&entry), None, None, 10.0, &|_| false, 4.0, now);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    #[test]
    fn decide_keeps_when_no_record_and_within_grace() {
        let now = Utc::now();
        let recent = now - Duration::hours(1);
        let action = decide(&issue(42, Some(recent)), None, None, None, 10.0, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_no_record_and_past_stale_threshold() {
        let now = Utc::now();
        let old = now - Duration::hours(5);
        let action = decide(&issue(42, Some(old)), None, None, None, 10.0, &|_| true, 4.0, now);
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
        let action = decide(&issue(42, Some(almost)), None, None, None, 10.0, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_keeps_when_no_record_and_no_age_evidence() {
        let now = Utc::now();
        let action = decide(&issue(42, None), None, None, None, 10.0, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep, "fail-safe: no evidence => Keep");
    }

    #[test]
    fn decide_dead_pid_overrides_label_age() {
        // Even a freshly-labeled issue must be reclaimed once its recorded
        // PID is provably dead — the journal is authoritative when present.
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, Some(&entry), None, None, 10.0, &|_| false, 4.0, now);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    // ------------------------------------------------------------------
    // Run-registry evidence source (Issue #4348)
    // ------------------------------------------------------------------

    #[test]
    fn decide_keeps_when_run_registry_pid_alive_and_no_journal_entry() {
        let now = Utc::now();
        let action =
            decide(&issue(42, None), None, Some(222), None, 10.0, &|pid| pid == 222, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_run_registry_pid_dead_and_no_journal_entry() {
        let now = Utc::now();
        // A fresh label would normally still be within the age-rule grace
        // period, but the run-registry evidence is provable and immediate,
        // no age grace, exactly like the journal's DeadPid branch.
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, None, Some(999), None, 10.0, &|_| false, 4.0, now);
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
        );
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_falls_back_to_age_rule_when_run_registry_pid_absent() {
        let now = Utc::now();
        let old = now - Duration::hours(5);
        let action = decide(&issue(42, Some(old)), None, None, None, 10.0, &|_| true, 4.0, now);
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
        let action =
            decide(&fresh_issue, None, None, Some(stale_checkpoint), 10.0, &|_| true, 4.0, now);
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
        let action =
            decide(&old_label_issue, None, None, Some(fresh_checkpoint), 10.0, &|_| true, 4.0, now);
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
        );
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_no_progress_none_still_falls_through_to_age_gate() {
        // no_progress=None must not disturb the existing age-rule behavior:
        // a fresh label with no evidence is still Kept.
        let now = Utc::now();
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, None, None, None, 10.0, &|_| true, 4.0, now);
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
        let decisions =
            plan("/repo/a", &issues, &journal, &|_| None, &|_| None, 10.0, &|_| true, 4.0, now);

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
        run_reconciliation_pass(dir.path());
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

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

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

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

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

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

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

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 0,
            "a malformed checkpoint must never be treated as proof of death (fail-safe)"
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

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

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

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

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

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

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

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

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
        // claim_labeled_at intentionally left unset here so every existing
        // caller of this helper keeps exercising the pre-#4618
        // updated_at-only fallback path unchanged; the #4618 regression test
        // below constructs `ClaimedPr` directly to set it.
        ClaimedPr {
            number,
            updated_at,
            claim_labeled_at: None,
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
}
