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
//!   the PR's `updatedAt` is also stale (per-label threshold,
//!   [`resolve_stale_reviewing_minutes`] / [`resolve_stale_treating_minutes`]).
//!   A **live** joined pid still short-circuits to `Keep` unconditionally,
//!   exactly like the issue side — that evidence is trustworthy either way.
//!   A missing/unparseable `updatedAt` fails safe to `Keep`, same rationale
//!   as the issue side's total-absence-of-evidence case.
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

/// Resolve the periodic reconciliation interval from
/// [`RECONCILE_INTERVAL_ENV`], falling back to
/// [`DEFAULT_RECONCILE_INTERVAL_SECS`] for an absent, unparseable, or
/// non-positive value, and always clamped up to
/// [`MIN_RECONCILE_INTERVAL_SECS`] regardless of source.
#[must_use]
pub fn resolve_reconcile_interval() -> Duration {
    let secs = std::env::var(RECONCILE_INTERVAL_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_RECONCILE_INTERVAL_SECS);
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
#[must_use]
pub fn decide(
    issue: &BuildingIssue,
    journal_entry: Option<&JournalEntry>,
    run_registry_pid: Option<u32>,
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
#[must_use]
pub fn plan(
    repo: &str,
    issues: &[BuildingIssue],
    journal: &SweepJournal,
    run_registry_pid_for: &dyn Fn(u32) -> Option<u32>,
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
            (issue.number, decide(issue, entry, run_registry_pid, is_alive, stale_hours, now))
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
    /// Parsed `updatedAt` timestamp, when available.
    pub updated_at: Option<DateTime<Utc>>,
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
    match pr.updated_at {
        Some(updated) => {
            let age_minutes = (now - updated).num_seconds() as f64 / 60.0;
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
    let interval = resolve_reconcile_interval();
    log::info!(
        "claim_reconciliation: periodic pass enabled (interval={}s, #4348)",
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
        plan, plan_pr, resolve_stale_hours, BuildingIssue, ClaimedPr, PrClaimKind, PrReclaimReason,
        PrReconcileAction, ReclaimReason, ReconcileAction, MAX_ISSUES_PER_WORKSPACE,
    };
    use crate::sweep_journal;
    use anyhow::{anyhow, Context, Result};
    use serde::Deserialize;
    use std::path::Path;
    use std::process::{Command, Stdio};

    #[derive(Debug, Deserialize)]
    struct GhBuildingIssue {
        number: u32,
        #[serde(rename = "updatedAt", default)]
        updated_at: Option<String>,
    }

    fn list_building_issues(gh_bin: &Path, root: &Path) -> Result<Vec<BuildingIssue>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("list")
            .arg("--label")
            .arg("loom:building")
            .arg("--state")
            .arg("open")
            .arg("--limit")
            .arg(MAX_ISSUES_PER_WORKSPACE.to_string())
            .arg("--json")
            .arg("number,updatedAt");
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
                "gh issue list --label loom:building failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let rows: Vec<GhBuildingIssue> =
            serde_json::from_slice(&out.stdout).context("parse gh issue list JSON")?;
        Ok(rows
            .into_iter()
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
        let now = chrono::Utc::now();
        // Issue #4348: checkpoint->run-registry join, only ever consulted by
        // `plan()` for an issue with no journal entry.
        let run_registry_pid_for = |issue: u32| super::resolve_run_registry_pid(root, issue);
        let decisions = plan(
            &repo,
            &issues,
            &journal,
            &run_registry_pid_for,
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
                    let last_known_pid = match reason {
                        ReclaimReason::DeadPid { pid } | ReclaimReason::DeadRunRegistry { pid } => {
                            Some(pid)
                        }
                        ReclaimReason::NoRecordStale { .. } => None,
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
            .map(|r| ClaimedPr {
                number: r.number,
                updated_at: r
                    .updated_at
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc)),
                head_ref_name: r.head_ref_name,
            })
            .collect())
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
        let action = decide(&issue(42, None), Some(&entry), None, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_journal_entry_pid_dead() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action = decide(&issue(42, None), Some(&entry), None, &|_| false, 4.0, now);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    #[test]
    fn decide_keeps_when_no_record_and_within_grace() {
        let now = Utc::now();
        let recent = now - Duration::hours(1);
        let action = decide(&issue(42, Some(recent)), None, None, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_no_record_and_past_stale_threshold() {
        let now = Utc::now();
        let old = now - Duration::hours(5);
        let action = decide(&issue(42, Some(old)), None, None, &|_| true, 4.0, now);
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
        let action = decide(&issue(42, Some(almost)), None, None, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_keeps_when_no_record_and_no_age_evidence() {
        let now = Utc::now();
        let action = decide(&issue(42, None), None, None, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep, "fail-safe: no evidence => Keep");
    }

    #[test]
    fn decide_dead_pid_overrides_label_age() {
        // Even a freshly-labeled issue must be reclaimed once its recorded
        // PID is provably dead — the journal is authoritative when present.
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, Some(&entry), None, &|_| false, 4.0, now);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    // ------------------------------------------------------------------
    // Run-registry evidence source (Issue #4348)
    // ------------------------------------------------------------------

    #[test]
    fn decide_keeps_when_run_registry_pid_alive_and_no_journal_entry() {
        let now = Utc::now();
        let action = decide(&issue(42, None), None, Some(222), &|pid| pid == 222, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_run_registry_pid_dead_and_no_journal_entry() {
        let now = Utc::now();
        // A fresh label would normally still be within the age-rule grace
        // period, but the run-registry evidence is provable and immediate,
        // no age grace, exactly like the journal's DeadPid branch.
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, None, Some(999), &|_| false, 4.0, now);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadRunRegistry { pid: 999 }));
    }

    #[test]
    fn decide_journal_entry_takes_priority_over_run_registry_pid() {
        // Both evidence sources present: the journal (more authoritative)
        // decides, and the run-registry pid is never even consulted.
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action = decide(&issue(42, None), Some(&entry), Some(999), &|pid| pid == 111, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_falls_back_to_age_rule_when_run_registry_pid_absent() {
        let now = Utc::now();
        let old = now - Duration::hours(5);
        let action = decide(&issue(42, Some(old)), None, None, &|_| true, 4.0, now);
        match action {
            ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. }) => {}
            other => panic!("expected NoRecordStale reclaim, got {other:?}"),
        }
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

        let decisions = plan("/repo/a", &issues, &journal, &|_| None, &|pid| pid == 222, 4.0, now);

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
        let decisions = plan("/repo/a", &issues, &journal, &|_| None, &|_| true, 4.0, now);

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

        let decisions =
            plan("/repo/a", &issues, &journal, &run_registry_pid_for, &is_alive, 4.0, now);

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

    // ------------------------------------------------------------------
    // Periodic-interval resolution (Issue #4348)
    // ------------------------------------------------------------------

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

        // Fake `gh`: `issue list` reports one loom:building issue labeled
        // *just now* -- fresh enough that the NoRecordStale (age-based) path
        // would say Keep. Only the DeadPid evidence should trigger a reclaim.
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        let now = Utc::now().to_rfc3339();
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "list" ]; then
  echo '[{{"number":99,"updatedAt":"{now}"}}]'
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
            now = now,
        );
        std::fs::write(&fake_gh, &script).unwrap();
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }

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
    /// `gh_log` and, for `issue list`, reports exactly one `loom:building`
    /// issue (`issue_number`, `updated_at`). Every other subcommand (e.g.
    /// `issue edit`) just logs and exits 0 -- a test asserts on `gh_log`'s
    /// contents to see whether a reclaim was actually attempted.
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
if [ "$1" = "issue" ] && [ "$2" = "list" ]; then
  echo '[{{"number":{issue_number},"updatedAt":"{updated_at}"}}]'
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
    // PR-side claim labels: decide_pr / plan_pr (Issue #4367)
    // ------------------------------------------------------------------

    fn claimed_pr(
        number: u32,
        updated_at: Option<DateTime<Utc>>,
        head_ref_name: Option<&str>,
    ) -> ClaimedPr {
        ClaimedPr {
            number,
            updated_at,
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
