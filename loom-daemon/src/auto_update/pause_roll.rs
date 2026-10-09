//! Pause-and-roll, the old-binary side: H3 Staged → H4 Pausing → exit (issue
//! #10831; design `docs/design/daemon-roll-pause-resume.md` §7, §9, §11 PR 2).
//!
//! # What changed
//!
//! Before #10831 every automatic roll waited for in-flight work to reach zero
//! (#6007's re-arm and abandon budget, #8998's stall suppression), so a busy
//! host never rolled. Now **every** roll trigger
//! (`floor`, `repo_ahead`, `config_restart`, `autoupdate`) takes one path:
//! [`start_pause_roll`]. It pauses every daemon-dispatched agent at a safe
//! point, records them in the pause manifest, and exits for a supervised
//! relaunch onto the staged binary. No code path waits for in-flight == 0 for
//! a roll. The new binary's side (H5, resume) is #10832.
//!
//! # H4, in order (design §7)
//!
//! 1. Close dispatch (no sweep and no role run starts from here on), then
//!    wait for the dispatches that are mid-spawn to be recorded
//!    (`sweep_registry::roll_gate`), bounded by the settle window.
//! 2. Snapshot every in-flight agent (sweeps across all managed roots, plus
//!    role runs) and classify each with [`super::pause_classify`]: younger than
//!    `minResumableAgeSecs` is `requeue` (`young-agent-reset`), no session id
//!    is `requeue` (`session-not-resumable`), anything else is `resume`.
//! 3. **Write the manifest (`phase = pausing`) before any agent is signalled.**
//!    A failed write aborts the pause: nothing was signalled, dispatch resumes,
//!    and the failure is alerted (H7 `pause-failed`).
//! 4. Tear down the `requeue` items at once.
//! 5. Raise the pause request for the `resume` items and poll for the
//!    safe-point record **that answers this request**; stop each tree once it
//!    appears. A record left by an earlier pause is never trusted.
//! 6. Refresh each paused item's lease once, so the next start has a full TTL.
//! 7. At the budget deadline, tear down whatever is still `stopping` and
//!    requeue it (`pause-budget-missed`).
//!
//! Every teardown in steps 4, 5 and 7 runs on a pool ([`stops`], #11051):
//! H4 hands a tree over and goes on polling, so the stops run in parallel and
//! the budget-missed items are all stopped at the deadline, not one after
//! another. The stop phase ends at the budget plus a fixed margin
//! ([`PauseRollTuning::stop_bound`]) whatever the teardowns do.
//! 8. Do the requeue forge writes. A write that does not finish in the forge
//!    window stays `planned` in the manifest for the next start.
//! 9. Rewrite the manifest with `phase = paused`. **Never** remove a paused
//!    item's lock, journal entry or checkpoint: they are the fallback for a
//!    binary that cannot read the manifest (design §8).
//! 10. Exit through the drain supervisor's exit path.
//!
//! # The budget and the runtimes
//!
//! `pauseBudgetSecs` (default 120 s) covers steps 4-7. Three findings from
//! #10830's live test shape how it is spent:
//!
//! - **The park window must stay under each runtime's hook timeout.** The hook
//!   parks a call for at most [`crate::roll_pause::MAX_PARK_SECS`]; this side
//!   polls every [`PauseRollTuning::poll`] and stops a tree as soon as its
//!   safe-point record exists, so a parked call is normally stopped within a
//!   second or two, long before the hook would deny it. An agent that ends its
//!   turn after a deny has still stopped resumably: its record exists, so it
//!   is `paused`, not `exited`.
//! - **Codex records nothing for a parked call**, and its safe point relies on
//!   **serial tool calls** (its managed hook is pre-tool-use only, so there is
//!   no in-flight ledger: the first parked call is taken as the safe point).
//!   The safe-point record's `parked_tool`/`parked_summary` is therefore the
//!   only record of the call a resumed Codex session must re-run, and it is
//!   copied into the manifest.
//! - **An agent whose runtime never reaches the hook misses the budget.** A
//!   session container on an image older than the `roll-pause` hook, a call a
//!   guard hook denied that left a fresh ledger entry, or simply a tool call
//!   longer than the budget: none writes a safe-point record, and each falls
//!   to kill-and-requeue at step 7 (`pause-budget-missed`). The roll never
//!   hangs on such an agent.
//!
//! # Operator interplay
//!
//! See `ipc/drain_pause.rs`. The pause checks who owns the drain at every step
//! boundary and commits (under the drain lock) before it stops any tree.
//!
//! # H4 always ends
//!
//! Every external command H4 runs is bounded (`teardown::run_bounded`, the
//! forge timeouts), and the run as a whole has a deadline
//! ([`PauseRollTuning::h4_deadline`]) its supervisor enforces
//! ([`supervise::expire`]). A pause that passes it before the commit is ended
//! and dispatch resumes; one that passes it after the commit is finished by
//! force ([`ledger::RunLedger::force_finish`]) and the daemon restarts.
//! Dispatch is never left paused with an abort refused.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::pause_classify::{self, ClassifyInput};
use super::pause_manifest::{
    self, Disposition, ItemStatus, ManifestEvent, ManifestItem, PauseManifest, Phase, Roll,
    SafePointRecord, TargetSource, WrittenBy,
};
use crate::ipc::{DrainState, PauseOwnership};
use crate::roll_pause;
use crate::sweep_registry::roll_requeue::RollRequeueNotice;

pub(crate) mod host;
pub(crate) mod ledger;
mod refusal;
mod stops;
mod supervise;
pub(crate) mod teardown;

use host::{Candidate, PauseHost};
use ledger::RunLedger;
pub use supervise::start_pause_roll;

/// Requeue reason: the agent had not reached a safe point when the pause
/// budget ran out.
pub const REASON_BUDGET_MISSED: &str = "pause-budget-missed";

/// `autonomous.autoUpdate.pauseRoll.pauseBudgetSecs` default.
pub const DEFAULT_PAUSE_BUDGET_SECS: u64 = 120;
/// `autonomous.autoUpdate.pauseRoll.verifyProbationSecs` default (H5).
pub const DEFAULT_VERIFY_PROBATION_SECS: u64 = 90;
/// `autonomous.autoUpdate.pauseRoll.resumeBudgetSecs` default (H5).
pub const DEFAULT_RESUME_BUDGET_SECS: u64 = 120;
/// Env overrides (env > config > default, like every `autoUpdate` knob).
pub const PAUSE_BUDGET_ENV: &str = "LOOM_AUTO_UPDATE_PAUSE_ROLL_PAUSE_BUDGET_SECS";
pub const VERIFY_PROBATION_ENV: &str = "LOOM_AUTO_UPDATE_PAUSE_ROLL_VERIFY_PROBATION_SECS";
pub const RESUME_BUDGET_ENV: &str = "LOOM_AUTO_UPDATE_PAUSE_ROLL_RESUME_BUDGET_SECS";

/// The least time the forge writes of steps 6 and 8 get, whatever is left of
/// the budget (design §7 "Timeout").
const FORGE_FLOOR: Duration = Duration::from_secs(30);
/// The longest step 1 waits for mid-spawn dispatches to be recorded. A
/// dispatch is mid-spawn for at most `TOKEN_NAME_CAPTURE_TIMEOUT` (5 s).
const PENDING_SETTLE_MAX: Duration = Duration::from_secs(30);
/// What H4 may take beyond its settle window, pause budget and forge floor
/// before its supervisor ends it: the stop margin and scheduling slack.
const H4_DEADLINE_MARGIN: Duration = Duration::from_secs(90);
/// How many trees H4 stops at once (#11051).
pub const DEFAULT_STOP_CONCURRENCY: usize = 8;
/// How long past the pause budget H4 waits for the teardowns still running
/// before it kills their process groups (#11051). A teardown normally takes a
/// few seconds: the scope's 5 s grace, then the tree kill's 3 s grace.
pub const DEFAULT_STOP_MARGIN: Duration = Duration::from_secs(15);

/// What a roll is rolling to, and who asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollTarget {
    /// What triggered the roll.
    pub source: TargetSource,
    /// The version being rolled to, when known (a source rebuild has none).
    pub to_version: Option<String>,
    /// The published artifact checksum, when known.
    pub to_artifact_sha256: Option<String>,
    /// The artifact identity a later tick compares a fresh release against to
    /// supersede this roll (#8514). `None` for a roll with no artifact.
    pub label: Option<String>,
}

impl RollTarget {
    /// A source-rebuild roll: the checkout is ahead of the running binary.
    #[must_use]
    pub fn repo_ahead() -> Self {
        Self {
            source: TargetSource::RepoAhead,
            to_version: None,
            to_artifact_sha256: None,
            label: None,
        }
    }
}

/// The resolved `autonomous.autoUpdate.pauseRoll.*` knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseRollTuning {
    /// H4's safe-point budget.
    pub pause_budget: Duration,
    /// H5's health probation (recorded for #10832; validated here).
    pub verify_probation: Duration,
    /// H5's resume budget (recorded for #10832; validated here).
    pub resume_budget: Duration,
    /// The 5-minute rule (`0` disables it).
    pub min_resumable_age_secs: u64,
    /// The lease TTL the budgets are validated against, and the manifest's
    /// `max_age_secs`.
    pub lease_ttl: Duration,
    /// How often H4 polls for safe-point records.
    pub poll: Duration,
    /// The longest step 1 waits for mid-spawn dispatches.
    pub pending_settle: Duration,
    /// The least time the forge writes get.
    pub forge_floor: Duration,
    /// H4's allowance beyond settle + budget + forge floor ([`Self::h4_deadline`]).
    pub deadline_margin: Duration,
    /// How many trees H4 stops at once.
    pub stop_concurrency: usize,
    /// How long past the budget the stop phase may run ([`Self::stop_bound`]).
    pub stop_margin: Duration,
}

impl PauseRollTuning {
    /// The shipped defaults against `lease_ttl`.
    #[must_use]
    pub fn defaults(lease_ttl: Duration) -> Self {
        Self {
            pause_budget: Duration::from_secs(DEFAULT_PAUSE_BUDGET_SECS),
            verify_probation: Duration::from_secs(DEFAULT_VERIFY_PROBATION_SECS),
            resume_budget: Duration::from_secs(DEFAULT_RESUME_BUDGET_SECS),
            min_resumable_age_secs: pause_classify::DEFAULT_MIN_RESUMABLE_AGE_SECS,
            lease_ttl,
            poll: Duration::from_secs(1),
            pending_settle: PENDING_SETTLE_MAX,
            forge_floor: FORGE_FLOOR,
            deadline_margin: H4_DEADLINE_MARGIN,
            stop_concurrency: DEFAULT_STOP_CONCURRENCY,
            stop_margin: DEFAULT_STOP_MARGIN,
        }
    }

    /// How long step 1 waits for mid-spawn dispatches.
    #[must_use]
    pub fn settle_window(&self) -> Duration {
        self.pending_settle.min(self.pause_budget / 2)
    }

    /// The longest H4 may run before its supervisor ends it: the settle
    /// window, the pause budget, the forge floor and the margin. With the
    /// defaults that is 30 + 120 + 30 + 90 = 270 s. H5 then has its probation
    /// and resume budget (90 + 120 s), so the whole roll stays at 480 s, under
    /// the 600 s the budget validation allows against a 15-minute lease.
    #[must_use]
    pub fn h4_deadline(&self) -> Duration {
        self.settle_window() + self.pause_budget + self.forge_floor + self.deadline_margin
    }

    /// Reject a budget combination that could outlive the lease (design §7
    /// "Bound on the window"): H4 refreshes each paused item's lease once, and
    /// H5 must finish resuming before that lease ages out, so the three
    /// budgets together must stay under two thirds of the lease TTL.
    ///
    /// # Errors
    /// With the operator-facing reason when a budget is zero or the sum
    /// reaches the limit.
    pub fn validate(&self) -> Result<(), String> {
        let (p, v, r) = (
            self.pause_budget.as_secs(),
            self.verify_probation.as_secs(),
            self.resume_budget.as_secs(),
        );
        if p == 0 || v == 0 || r == 0 {
            return Err(format!(
                "autonomous.autoUpdate.pauseRoll: every budget must be positive (pauseBudgetSecs \
                 {p}, verifyProbationSecs {v}, resumeBudgetSecs {r})"
            ));
        }
        let sum = p.saturating_add(v).saturating_add(r);
        let ttl = self.lease_ttl.as_secs();
        // sum >= 2/3 * ttl, in integers.
        if sum.saturating_mul(3) >= ttl.saturating_mul(2) {
            return Err(format!(
                "autonomous.autoUpdate.pauseRoll: pauseBudgetSecs {p} + verifyProbationSecs {v} + \
                 resumeBudgetSecs {r} = {sum}s reaches two thirds of the {ttl}s lease TTL ({}s); a \
                 paused agent's lease could age out before it is resumed",
                ttl.saturating_mul(2) / 3
            ));
        }
        Ok(())
    }

    /// Resolve the knobs for `repo_root` (env > config > default) and validate
    /// them. An invalid combination is **rejected**: the defaults are used
    /// instead and the reason is returned for the caller to alert on.
    #[must_use]
    pub fn resolve(repo_root: &Path) -> (Self, Option<String>) {
        let ttl_minutes = crate::claim_reconciliation::resolve_lease_ttl_minutes();
        // Whole seconds; the TTL is minutes, so nothing is lost.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lease_ttl = Duration::from_secs((ttl_minutes * 60.0).max(0.0) as u64);
        let effective = crate::config_resolver::resolve_effective_config(repo_root);
        let knob = |env: &str, key: &str, default: u64| {
            std::env::var(env)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .or_else(|| {
                    crate::config_resolver::get_path(
                        &effective,
                        &format!("autonomous.autoUpdate.pauseRoll.{key}"),
                    )
                    .and_then(serde_json::Value::as_u64)
                })
                .unwrap_or(default)
        };
        let defaults = Self::defaults(lease_ttl);
        let resolved = Self {
            pause_budget: Duration::from_secs(knob(
                PAUSE_BUDGET_ENV,
                "pauseBudgetSecs",
                DEFAULT_PAUSE_BUDGET_SECS,
            )),
            verify_probation: Duration::from_secs(knob(
                VERIFY_PROBATION_ENV,
                "verifyProbationSecs",
                DEFAULT_VERIFY_PROBATION_SECS,
            )),
            resume_budget: Duration::from_secs(knob(
                RESUME_BUDGET_ENV,
                "resumeBudgetSecs",
                DEFAULT_RESUME_BUDGET_SECS,
            )),
            min_resumable_age_secs: pause_classify::resolve_min_resumable_age_secs(repo_root),
            ..defaults
        };
        match resolved.validate() {
            Ok(()) => (resolved, None),
            Err(why) => (
                Self {
                    min_resumable_age_secs: resolved.min_resumable_age_secs,
                    ..defaults
                },
                Some(format!("{why}. REJECTED: the default budgets are used instead.")),
            ),
        }
    }
}

/// Everything one H4 run needs to know.
#[derive(Debug, Clone)]
pub struct PausePlan {
    pub target: RollTarget,
    pub tuning: PauseRollTuning,
    /// Where the manifest is written.
    pub manifest_path: PathBuf,
    /// The running version.
    pub from_version: String,
    /// The drain generation this pause owns.
    pub generation: u64,
    /// H3 entry.
    pub staged_at: DateTime<Utc>,
    /// The detected supervisor, for `written_by`.
    pub supervisor: Option<String>,
}

/// How an H4 run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H4Outcome {
    /// Every agent is stopped or requeued and the manifest is written. The
    /// caller exits: for a relaunch, or (`then_exit`) to stay down.
    Paused {
        manifest_id: String,
        then_exit: bool,
    },
    /// The pause lost the drain before it stopped anything and undid itself
    /// (pause requests withdrawn, manifest deleted). `promoted` ⇒ the drain is
    /// now an operator drain the caller must supervise.
    StoodDown { promoted: bool },
    /// The manifest could not be written; nothing was signalled and dispatch
    /// resumed (H7 `pause-failed`).
    Failed(String),
}

/// One item while H4 works on it.
struct Work {
    cand: Candidate,
    item: ManifestItem,
    /// When its pause request was raised.
    requested: Option<Instant>,
    /// Whether its `daemon.roll.item` event has been emitted.
    reported: bool,
    teardown_ms: Option<u64>,
    safe_point_wait_ms: Option<u64>,
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn new_manifest_id(now: DateTime<Utc>) -> String {
    format!(
        "rp-{}-{}",
        now.format("%Y%m%dT%H%M%SZ"),
        &uuid::Uuid::new_v4().simple().to_string()[..4]
    )
}

/// Build one manifest item from a candidate and its classification.
fn manifest_item(c: &Candidate, now: DateTime<Utc>, min_age: u64) -> ManifestItem {
    let handle = c.resume_handle.as_ref();
    // An agent that cannot be asked to pause (no pause dir: dispatched before
    // #10830) has no session this roll can stop at a safe point.
    let session_id = c
        .pause_dir
        .as_ref()
        .and_then(|_| handle.and_then(|h| h.session_id.clone()));
    let verdict = pause_classify::classify(
        &ClassifyInput {
            kind: c.kind.clone(),
            agent_age_secs: c.agent_age_secs(now),
            runtime: handle.map(|h| h.runtime.clone()),
            session_id,
        },
        min_age,
    );
    ManifestItem {
        id: c.id.clone(),
        kind: c.kind.clone(),
        repo: host::repo_string(&c.repo),
        disposition: verdict.disposition,
        status: ItemStatus::Planned,
        reason: verdict.reason,
        issue: c.issue,
        pr: None,
        pid: c.pid,
        // The observed process start when the snapshot has one (it is the
        // identity the teardown checks), else the registry's record.
        pid_started_at: c.proc_started_at.or(c.run_started_at).map(rfc3339),
        pgid: c.pgid,
        scope_unit: c.scope_unit.clone(),
        agent_started_at: c.agent_started_at,
        run_started_at: c.run_started_at,
        resume_handle: c.resume_handle.clone(),
        safe_point: None,
        checkpoint_phase: c.checkpoint_phase.clone(),
        worktree: c.worktree.clone(),
        // A sweep's claim is its issue's `loom:building`; a role run's is
        // whatever its claim breadcrumb recorded (#10832), if anything.
        claim: c.issue.map_or_else(
            || c.role_claim().map(|claim| claim.manifest_value()),
            |_| Some(serde_json::json!({ "label": "loom:building", "on": "issue" })),
        ),
        lease_comment_id: None,
        lease_refreshed_at: None,
        log_path: c.log_path.clone(),
        overflow: false,
        role: c.role.clone(),
        timeout_remaining_secs: c.timeout_remaining_secs,
        holds_issue_creation_mutex: c.holds_issue_creation_mutex,
        stopped_at: None,
    }
}

/// The H4 run's mutable state.
struct Run<'a> {
    drain: &'a DrainState,
    host: Arc<dyn PauseHost>,
    plan: &'a PausePlan,
    manifest: PauseManifest,
    work: Vec<Work>,
    /// What this run has created, shared with its supervisor.
    ledger: &'a RunLedger,
}

impl Run<'_> {
    fn gen(&self) -> u64 {
        self.plan.generation
    }

    fn event(&mut self, item: Option<&str>, event: &str, detail: Option<String>) {
        self.manifest.events.push(ManifestEvent {
            at: rfc3339(Utc::now()),
            by_version: Some(self.plan.from_version.clone()),
            item: item.map(str::to_string),
            event: event.to_string(),
            detail,
        });
    }

    /// Copy the working items into the manifest and write it. After step 3 a
    /// failed write is logged and the pause carries on: the last manifest on
    /// disk still names every item, and a `phase = pausing` manifest is one
    /// the next start can finish.
    fn save(&mut self) -> std::io::Result<()> {
        self.manifest.items = self.work.iter().map(|w| w.item.clone()).collect();
        self.ledger.save(&self.manifest)
    }

    fn save_best_effort(&mut self) {
        if let Err(e) = self.save() {
            log::error!(
                "pause_roll: could not rewrite the pause manifest {} ({e}); continuing, the last \
                 written manifest still records every agent",
                self.plan.manifest_path.display()
            );
        }
    }

    /// Emit `daemon.roll.item` for `idx` once, and count it in status.
    fn report(&mut self, idx: usize, forge: &str) {
        let now = Utc::now();
        let w = &mut self.work[idx];
        if w.reported {
            return;
        }
        w.reported = true;
        let tuning = &self.plan.tuning;
        let payload = serde_json::json!({
            "manifest_id": self.manifest.manifest_id,
            "item_id": w.item.id,
            "kind": w.item.kind.as_str(),
            "runtime": w.item.resume_handle.as_ref().map(|h| h.runtime.as_str().to_string()),
            "disposition": w.item.disposition.as_str(),
            "status": w.item.status.as_str(),
            "reason": w.item.reason,
            "agent_age_secs": w.cand.agent_age_secs(now),
            "issue": w.item.issue,
            "role": w.item.role,
            "from_version": self.plan.from_version,
            "to_version": self.plan.target.to_version,
            "target_source": self.plan.target.source.as_str(),
            "pause_budget_secs": tuning.pause_budget.as_secs(),
            "verify_probation_secs": tuning.verify_probation.as_secs(),
            "resume_budget_secs": tuning.resume_budget.as_secs(),
            "min_resumable_age_secs": tuning.min_resumable_age_secs,
            "safe_point_wait_ms": w.safe_point_wait_ms,
            "teardown_ms": w.teardown_ms,
            "forge": forge,
        });
        let status = w.item.status.clone();
        let requeue = w.item.disposition == Disposition::Requeue;
        let reason = w.item.reason.clone();
        self.host.emit("daemon.roll.item", payload);
        self.drain.pause_update(self.gen(), |p| {
            if requeue {
                *p.requeued_by_reason
                    .entry(reason.unwrap_or_else(|| "unknown".to_string()))
                    .or_insert(0) += 1;
                if forge == "deferred" {
                    p.deferred_forge_writes += 1;
                }
            } else if status == ItemStatus::Paused {
                p.paused += 1;
            } else {
                p.exited += 1;
            }
        });
    }

    /// Undo a pause that has stopped nothing: withdraw every pause request,
    /// hand the sweeps back to the reaper, delete the manifest. Only this
    /// run's: a superseded run that gets here late leaves its replacement's
    /// manifest, requests and holds alone ([`RunLedger::undo`]).
    fn stand_down(&mut self, promoted: bool) -> H4Outcome {
        self.ledger.undo(self.host.as_ref());
        log::warn!(
            "pause_roll: standing down before any agent was stopped ({}): pause requests \
             withdrawn, manifest deleted, nothing stopped",
            if promoted {
                "an operator drain took over"
            } else {
                "the roll was aborted or superseded"
            }
        );
        self.host.emit(
            "daemon.roll.stood_down",
            serde_json::json!({
                "manifest_id": self.manifest.manifest_id,
                "promoted": promoted,
                "items": self.work.len(),
            }),
        );
        H4Outcome::StoodDown { promoted }
    }

    fn notice(&self, idx: usize) -> RollRequeueNotice {
        let w = &self.work[idx];
        RollRequeueNotice {
            from_version: self.plan.from_version.clone(),
            to_version: self.plan.target.to_version.clone(),
            phase: w.item.checkpoint_phase.clone(),
            agent_age_secs: w.cand.agent_age_secs(Utc::now()),
            reason: w.item.reason.clone().unwrap_or_default(),
            worktree: w.item.worktree.as_ref().map(|wt| wt.path.clone()),
            worktree_dirty: w.item.worktree.as_ref().and_then(|wt| wt.dirty),
            manifest_id: self.manifest.manifest_id.clone(),
        }
    }
}

/// What one forge worker did.
enum ForgeDone {
    Lease(usize, Result<(), String>),
    Requeue(usize, Result<(), String>),
}

/// Reopens dispatch for a run that ends without restarting the daemon (it
/// stood down, failed, or panicked). A run that ends `Paused` keeps it closed:
/// the process is about to exit.
struct DispatchClosed {
    host: Arc<dyn PauseHost>,
    run: String,
    keep: bool,
}

impl Drop for DispatchClosed {
    fn drop(&mut self) {
        if !self.keep {
            self.host.close_dispatch(false, &self.run);
        }
    }
}

/// [`run_h4_with`] with a ledger of its own, for callers with no supervisor.
#[cfg(test)]
pub(crate) fn run_h4(drain: &DrainState, host: Arc<dyn PauseHost>, plan: &PausePlan) -> H4Outcome {
    let ledger = RunLedger::new(new_manifest_id(Utc::now()), plan.manifest_path.clone());
    run_h4_with(drain, host, plan, &ledger)
}

/// Run H4 (design §7). Blocking; call it off the async runtime.
///
/// `drain` must already hold a [`crate::ipc::DrainOrigin::PauseRoll`] drain of
/// generation `plan.generation` ([`DrainState::begin_pause_roll`]). `ledger`
/// names the run (its manifest id) and records what it creates, so the
/// supervisor can undo or finish a run that cannot do so itself.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_h4_with(
    drain: &DrainState,
    host: Arc<dyn PauseHost>,
    plan: &PausePlan,
    ledger: &RunLedger,
) -> H4Outcome {
    let tuning = plan.tuning;
    let started = Instant::now();
    let pause_started_at = Utc::now();
    let manifest_id = ledger.id().to_string();
    let mut gate = DispatchClosed {
        host: Arc::clone(&host),
        run: manifest_id.clone(),
        keep: false,
    };
    let mut run = Run {
        drain,
        host,
        plan,
        manifest: PauseManifest {
            schema_version: pause_manifest::SCHEMA_VERSION,
            manifest_id: manifest_id.clone(),
            phase: Phase::Pausing,
            written_by: WrittenBy {
                version: plan.from_version.clone(),
                artifact_sha256: None,
                pid: Some(std::process::id()),
                host: std::env::var("LOOM_HOST_ID")
                    .ok()
                    .or_else(|| std::env::var("HOSTNAME").ok()),
                supervisor: plan.supervisor.clone(),
            },
            roll: Roll {
                from_version: Some(plan.from_version.clone()),
                to_version: plan
                    .target
                    .to_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                to_artifact_sha256: plan.target.to_artifact_sha256.clone(),
                target_source: Some(plan.target.source.clone()),
                staged_at: Some(plan.staged_at),
                pause_started_at,
                pause_completed_at: None,
                pause_duration_ms: None,
                pause_budget_secs: Some(tuning.pause_budget.as_secs()),
                min_resumable_age_secs: Some(tuning.min_resumable_age_secs),
                max_age_secs: tuning.lease_ttl.as_secs(),
            },
            items: Vec::new(),
            events: Vec::new(),
        },
        work: Vec::new(),
        ledger,
    };
    // Every step boundary is an ownership check. Until something is stopped,
    // losing the drain means standing down.
    macro_rules! enter {
        ($step:expr) => {
            match drain.pause_enter_step(plan.generation, $step) {
                PauseOwnership::Ours => {}
                PauseOwnership::Promoted => return run.stand_down(true),
                PauseOwnership::Gone => return run.stand_down(false),
            }
        };
    }

    // ---- Step 1: close dispatch, then let mid-spawn dispatches be recorded ----
    // From here nothing new starts. A dispatch whose child is already spawned
    // is not in the registry yet; with the gate closed that count only falls.
    enter!(1);
    run.host.close_dispatch(true, &manifest_id);
    let settle_until = started + tuning.settle_window();
    let mut unsettled = run.host.pending_dispatches();
    while unsettled > 0 && Instant::now() < settle_until {
        std::thread::sleep(tuning.poll);
        enter!(1);
        unsettled = run.host.pending_dispatches();
    }
    if unsettled > 0 {
        // A dispatch is mid-spawn for seconds at most, so this is a stuck
        // spawn. Its agent is not in the snapshot; say so where the next
        // start will read it.
        log::error!(
            "pause_roll: {unsettled} dispatch(es) still mid-spawn after the {}s settle window;              their agents are NOT in this pause's manifest and are left to restart recovery",
            tuning.settle_window().as_secs()
        );
        run.event(None, "unsettled_dispatches", Some(unsettled.to_string()));
    }
    let settle_secs = started.elapsed().as_secs();

    // ---- Step 2: snapshot and classify ---------------------------------------
    enter!(2);
    let now = Utc::now();
    run.work = run
        .host
        .snapshot()
        .into_iter()
        .map(|cand| Work {
            item: manifest_item(&cand, now, tuning.min_resumable_age_secs),
            cand,
            requested: None,
            reported: false,
            teardown_ms: None,
            safe_point_wait_ms: None,
        })
        .collect();
    let cands: Vec<Candidate> = run.work.iter().map(|w| w.cand.clone()).collect();
    ledger.set_candidates(&cands);
    let items = u32::try_from(run.work.len()).unwrap_or(u32::MAX);
    drain.pause_update(plan.generation, |p| {
        p.items = items;
        p.manifest_id = Some(manifest_id.clone());
        p.settle_secs = Some(settle_secs);
    });

    // ---- Step 3: the manifest, before anything is signalled -------------------
    enter!(3);
    run.event(None, "pause_started", Some(format!("{items} item(s)")));
    if let Err(e) = run.save() {
        let note =
            format!(
            "PAUSE FAILED (H7 pause-failed): the pause manifest {} could not be written ({e}), so \
             the roll to {} was aborted before any agent was signalled. Dispatch resumed; this \
             host stays on {} until the next roll attempt. Fix the state directory (disk full? \
             permissions?).",
            plan.manifest_path.display(),
            plan.target.to_version.as_deref().unwrap_or("the rebuilt binary"),
            plan.from_version
        );
        log::error!("pause_roll: {note}");
        run.host.emit(
            "daemon.roll.pause_failed",
            serde_json::json!({ "manifest_id": manifest_id, "error": e.to_string(), "items": items }),
        );
        return match drain.pause_ownership(plan.generation) {
            PauseOwnership::Ours => {
                drain.resolve_timeout(note.clone());
                H4Outcome::Failed(note)
            }
            PauseOwnership::Promoted => H4Outcome::StoodDown { promoted: true },
            PauseOwnership::Gone => H4Outcome::StoodDown { promoted: false },
        };
    }
    // The manifest records every agent: take them over from the reaper, so a
    // tree this pause stops keeps its lock, journal entry and checkpoint.
    {
        let _artefacts = ledger::artefacts();
        for w in &run.work {
            run.host.hold(&w.cand, true, &manifest_id);
        }
    }

    // ---- Step 4: tear down the requeue items at once --------------------------
    enter!(4);
    let stop_started = Instant::now();
    let deadline = stop_started + tuning.pause_budget;
    let stop_bound = stop_started + tuning.stop_bound();
    let mut pool = stops::StopPool::new(Arc::clone(&run.host), tuning.stop_concurrency);
    let mut stopped_any = false;
    for idx in 0..run.work.len() {
        if run.work[idx].item.disposition != Disposition::Requeue {
            continue;
        }
        if !run.start_stop(&mut pool, idx) {
            if stopped_any {
                break; // unreachable: a committed pause cannot lose the drain
            }
            let promoted = drain.pause_ownership(plan.generation) == PauseOwnership::Promoted;
            return run.stand_down(promoted);
        }
        stopped_any = true;
    }
    if stopped_any {
        run.save_best_effort();
    }

    // ---- Step 5: pause requests, then safe points -----------------------------
    if stopped_any {
        drain.pause_enter_step(plan.generation, 5);
    } else {
        enter!(5);
    }
    let request = roll_pause::PauseRequest {
        requested_at: rfc3339(Utc::now()),
        from_version: Some(plan.from_version.clone()),
        to_version: plan.target.to_version.clone(),
        manifest_id: Some(manifest_id.clone()),
    };
    // Under the artefacts lock: a superseded run standing down late checks
    // "is this request mine?" before it deletes, and must not interleave.
    let artefacts = ledger::artefacts();
    for idx in 0..run.work.len() {
        let w = &mut run.work[idx];
        if w.item.disposition != Disposition::Resume {
            continue;
        }
        // `resume` implies a pause dir (see `manifest_item`).
        let raised = w
            .cand
            .pause_dir
            .as_ref()
            .is_some_and(|dir| roll_pause::request_pause(dir, &request).is_ok());
        w.item.status = ItemStatus::Stopping;
        w.requested = Some(Instant::now());
        if !raised {
            // The request could not be written, so the agent will never park.
            // It is torn down at the deadline like any other miss.
            log::warn!("pause_roll: {}: could not raise the pause request", w.item.id);
        }
    }
    drop(artefacts);
    run.save_best_effort();

    let stopping = |run: &Run<'_>| {
        (0..run.work.len())
            .filter(|i| run.work[*i].item.status == ItemStatus::Stopping)
            .collect::<Vec<_>>()
    };
    loop {
        let pending = stopping(&run);
        if pending.is_empty() {
            break;
        }
        if !stopped_any {
            match drain.pause_ownership(plan.generation) {
                PauseOwnership::Ours => {}
                PauseOwnership::Promoted => return run.stand_down(true),
                PauseOwnership::Gone => return run.stand_down(false),
            }
        }
        let mut progressed = run.collect_stops(&mut pool);
        for idx in pending {
            let dir = run.work[idx].cand.pause_dir.clone();
            // Only a record that answers THIS request: one left by an earlier
            // pause describes a call that has since been released.
            let safe_point = dir
                .as_deref()
                .and_then(|d| roll_pause::read_safe_point_for(d, &request));
            if let Some(sp) = safe_point {
                if !run.start_stop(&mut pool, idx) {
                    let promoted =
                        drain.pause_ownership(plan.generation) == PauseOwnership::Promoted;
                    return run.stand_down(promoted);
                }
                stopped_any = true;
                progressed = true;
                let w = &mut run.work[idx];
                w.safe_point_wait_ms = w
                    .requested
                    .map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
                w.item.safe_point = Some(SafePointRecord {
                    reached_at: sp.reached_at.clone(),
                    parked_tool: Some(sp.parked_tool.clone()),
                    parked_summary: Some(sp.parked_summary.clone()),
                });
                w.item.status = ItemStatus::Paused;
                // A live-captured session id (Codex) the dispatch-time handle
                // did not have yet.
                if let (Some(h), Some(sid)) = (w.item.resume_handle.as_mut(), sp.session_id) {
                    if h.session_id.is_none() {
                        h.session_id = Some(sid);
                    }
                }
                let id = w.item.id.clone();
                run.event(Some(&id), "safe_point", Some(format!("parked {}", sp.parked_tool)));
                // Its `daemon.roll.item` goes out when its teardown finishes.
            } else if !run.host.is_alive(&run.work[idx].cand) {
                // It ended by itself before a safe point: not paused. The
                // existing crash path decides between resume and requeue at
                // the next start, exactly as it would without a roll (§4).
                progressed = true;
                let w = &mut run.work[idx];
                w.item.status = ItemStatus::Exited;
                w.item.reason = Some("exited-before-safe-point".to_string());
                w.item.stopped_at = Some(Utc::now());
                if let Some(dir) = &w.cand.pause_dir {
                    roll_pause::withdraw_for(dir, &manifest_id);
                }
                let id = w.item.id.clone();
                run.event(Some(&id), "exited", None);
                run.report(idx, "none");
            }
        }
        if progressed {
            run.save_best_effort();
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(tuning.poll);
    }

    // ---- Step 7: the budget deadline -----------------------------------------
    // Every item still `stopping` is handed to the pool now, together, and the
    // stop phase then waits for the teardowns until the stop bound.
    drain.pause_enter_step(plan.generation, 7);
    for idx in stopping(&run) {
        if !run.start_stop(&mut pool, idx) {
            // Only reachable with nothing stopped yet.
            let promoted = drain.pause_ownership(plan.generation) == PauseOwnership::Promoted;
            return run.stand_down(promoted);
        }
        let w = &mut run.work[idx];
        w.item.disposition = Disposition::Requeue;
        w.item.status = ItemStatus::Planned;
        w.item.reason = Some(REASON_BUDGET_MISSED.to_string());
        let id = w.item.id.clone();
        run.event(Some(&id), "budget_missed", None);
    }
    run.finish_stops(&mut pool, stop_bound);
    drop(pool);
    let stop_elapsed = stop_started.elapsed();
    let stop_secs = stop_elapsed.as_secs();
    let stop_ms = u64::try_from(stop_elapsed.as_millis()).unwrap_or(u64::MAX);
    run.manifest.roll.pause_duration_ms = Some(stop_ms);
    log::warn!(
        "pause_roll: every agent is stopped: the pause took {stop_ms}ms (budget {}s, bound {}s, \
         {} item(s))",
        tuning.pause_budget.as_secs(),
        tuning.stop_bound().as_secs(),
        run.work.len()
    );
    drain.pause_update(plan.generation, |p| p.stop_secs = Some(stop_secs));
    run.save_best_effort();

    // ---- Steps 6 and 8: lease refresh and requeue forge writes ----------------
    // They get what is left of the budget, and never less than the floor. A
    // slow forge must not block the roll: each write runs on its own thread,
    // and one that has not answered when the window closes is left `planned`.
    drain.pause_enter_step(plan.generation, 6);
    let window = deadline
        .saturating_duration_since(Instant::now())
        .max(tuning.forge_floor);
    let forge_deadline = Instant::now() + window;
    let call_timeout = crate::sweep_registry::reaper::reap_gh_timeout().min(window);
    let (tx, rx) = std::sync::mpsc::channel::<ForgeDone>();
    let mut outstanding = 0usize;
    for idx in 0..run.work.len() {
        let (status, disposition) = {
            let w = &run.work[idx];
            (w.item.status.clone(), w.item.disposition.clone())
        };
        let host = Arc::clone(&run.host);
        let cand = run.work[idx].cand.clone();
        let tx = tx.clone();
        if status == ItemStatus::Paused {
            outstanding += 1;
            std::thread::spawn(move || {
                let _ = tx.send(ForgeDone::Lease(idx, host.refresh_lease(&cand, call_timeout)));
            });
        } else if disposition == Disposition::Requeue {
            outstanding += 1;
            let notice = run.notice(idx);
            std::thread::spawn(move || {
                let _ = tx.send(ForgeDone::Requeue(idx, host.requeue(&cand, &notice)));
            });
        }
    }
    drop(tx);
    drain.pause_enter_step(plan.generation, 8);
    while outstanding > 0 {
        let left = forge_deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(ForgeDone::Lease(idx, result)) => {
                outstanding -= 1;
                let id = run.work[idx].item.id.clone();
                match result {
                    Ok(()) => run.work[idx].item.lease_refreshed_at = Some(Utc::now()),
                    Err(e) => {
                        log::warn!("pause_roll: {id}: lease refresh failed: {e}");
                        run.event(Some(&id), "lease_refresh_failed", Some(e));
                    }
                }
            }
            Ok(ForgeDone::Requeue(idx, result)) => {
                outstanding -= 1;
                let id = run.work[idx].item.id.clone();
                match result {
                    Ok(()) => {
                        run.work[idx].item.status = ItemStatus::Requeued;
                        run.event(Some(&id), "requeued", run.work[idx].item.reason.clone());
                        run.report(idx, "done");
                    }
                    Err(e) => {
                        log::warn!(
                            "pause_roll: {id}: requeue forge write failed ({e}); left planned for \
                             the next start"
                        );
                        run.event(Some(&id), "requeue_deferred", Some(e));
                        run.report(idx, "deferred");
                    }
                }
            }
            // The window closed (or every worker is gone): stop waiting.
            Err(_) => break,
        }
    }
    for idx in 0..run.work.len() {
        let w = &run.work[idx];
        if w.item.disposition == Disposition::Requeue && !w.reported {
            let id = w.item.id.clone();
            log::warn!(
                "pause_roll: {id}: requeue forge write did not finish in the {}s forge window; \
                 left planned for the next start",
                window.as_secs()
            );
            run.event(Some(&id), "requeue_deferred", Some("forge window closed".to_string()));
            run.report(idx, "deferred");
        }
    }

    // ---- Step 9: phase = paused; locks, journal and checkpoints untouched -----
    drain.pause_enter_step(plan.generation, 9);
    for w in &run.work {
        // The trees are gone; a leftover request would park the next session
        // that reuses this item dir. The safe point is in the manifest; the
        // record on disk answers only this request and is never read for
        // another.
        if let Some(dir) = &w.cand.pause_dir {
            roll_pause::withdraw_for(dir, &manifest_id);
        }
    }
    run.manifest.phase = Phase::Paused;
    run.manifest.roll.pause_completed_at = Some(Utc::now());
    run.event(None, "pause_completed", Some(format!("stop phase {stop_ms}ms")));
    run.save_best_effort();

    // ---- Step 10: hand back to the caller, which exits ------------------------
    drain.pause_enter_step(plan.generation, 10);
    let then_exit = drain.then_exit();
    run.host.emit(
        "daemon.roll.paused",
        serde_json::json!({
            "manifest_id": manifest_id,
            "items": items,
            "then_exit": then_exit,
            "from_version": plan.from_version,
            "to_version": plan.target.to_version,
            "target_source": plan.target.source.as_str(),
            "pause_budget_secs": tuning.pause_budget.as_secs(),
            "settle_secs": settle_secs,
            "stop_secs": stop_secs,
            "stop_ms": stop_ms,
            "total_secs": started.elapsed().as_secs(),
        }),
    );
    // The daemon exits from here: dispatch stays closed.
    gate.keep = true;
    H4Outcome::Paused {
        manifest_id,
        then_exit,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "pause_roll/tests.rs"]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "pause_roll/hardening_tests.rs"]
mod hardening_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "pause_roll/handoff_tests.rs"]
mod handoff_tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "pause_roll/stops_tests.rs"]
mod stops_tests;
