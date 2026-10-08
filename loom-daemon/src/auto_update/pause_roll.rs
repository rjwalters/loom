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
//! 1. Wait for `Pending` dispatches to settle.
//! 2. Snapshot every in-flight agent (sweeps across all managed roots, plus
//!    role runs) and classify each with [`super::pause_classify`]: younger than
//!    `minResumableAgeSecs` is `requeue` (`young-agent-reset`), no session id
//!    is `requeue` (`session-not-resumable`), anything else is `resume`.
//! 3. **Write the manifest (`phase = pausing`) before any agent is signalled.**
//!    A failed write aborts the pause: nothing was signalled, dispatch resumes,
//!    and the failure is alerted (H7 `pause-failed`).
//! 4. Tear down the `requeue` items at once.
//! 5. Raise the pause request for the `resume` items and poll for safe-point
//!    records; stop each tree once its record appears.
//! 6. Refresh each paused item's lease once, so the next start has a full TTL.
//! 7. At the budget deadline, tear down whatever is still `stopping` and
//!    requeue it (`pause-budget-missed`).
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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::pause_classify::{self, ClassifyInput};
use super::pause_manifest::{
    self, Disposition, ItemStatus, LoadOutcome, ManifestEvent, ManifestItem, PauseManifest, Phase,
    Roll, SafePointRecord, TargetSource, WrittenBy,
};
use crate::event_bus::EventBus;
use crate::ipc::{DrainBegin, DrainState, PauseOwnership, PauseRollStatus};
use crate::roll_pause;
use crate::sweep_registry::roll_requeue::RollRequeueNotice;
use crate::workspace_pool::WorkspacePool;

pub(crate) mod host;
pub(crate) mod teardown;

use host::{Candidate, PauseHost};

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
/// The longest step 1 waits for `Pending` dispatches to settle.
const PENDING_SETTLE_MAX: Duration = Duration::from_secs(30);

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
    /// The longest step 1 waits for `Pending` dispatches.
    pub pending_settle: Duration,
    /// The least time the forge writes get.
    pub forge_floor: Duration,
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
        }
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
        pid_started_at: c.run_started_at.map(rfc3339),
        pgid: c.pgid,
        scope_unit: c.scope_unit.clone(),
        agent_started_at: c.agent_started_at,
        run_started_at: c.run_started_at,
        resume_handle: c.resume_handle.clone(),
        safe_point: None,
        checkpoint_phase: c.checkpoint_phase.clone(),
        worktree: c.worktree.clone(),
        claim: c
            .issue
            .map(|_| serde_json::json!({ "label": "loom:building", "on": "issue" })),
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
    /// Whether the manifest file exists on disk.
    written: bool,
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
        let result = pause_manifest::save(&self.plan.manifest_path, &self.manifest);
        if result.is_ok() {
            self.written = true;
        }
        result
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
    /// hand the sweeps back to the reaper, delete the manifest.
    fn stand_down(&mut self, promoted: bool) -> H4Outcome {
        for w in &self.work {
            if let Some(dir) = &w.cand.pause_dir {
                let _ = roll_pause::withdraw(dir);
            }
            self.host.hold(&w.cand, false);
        }
        if self.written {
            let _ = std::fs::remove_file(&self.plan.manifest_path);
        }
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

    /// Stop `idx`'s tree. `false` when the pause no longer owns the drain (so
    /// nothing was stopped).
    fn stop(&mut self, idx: usize) -> bool {
        if !self.drain.pause_commit_stop(self.gen()) {
            return false;
        }
        let report = self.host.teardown(&self.work[idx].cand);
        let w = &mut self.work[idx];
        w.teardown_ms = Some(report.elapsed_ms);
        w.item.stopped_at = Some(Utc::now());
        let id = w.item.id.clone();
        if !report.survivors.is_empty() {
            log::error!(
                "pause_roll: {id}: {} process(es) survived the teardown: {:?}",
                report.survivors.len(),
                report.survivors
            );
        }
        let detail = format!(
            "{} pid(s){}{}",
            report.pids.len(),
            if report.scope_stopped {
                ", scope stopped"
            } else {
                ""
            },
            report
                .scope_note
                .as_deref()
                .map_or_else(String::new, |n| format!(", {n}; used the process tree"))
        );
        self.event(Some(&id), "stopped", Some(detail));
        true
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

/// Run H4 (design §7). Blocking; call it off the async runtime.
///
/// `drain` must already hold a [`crate::ipc::DrainOrigin::PauseRoll`] drain of
/// generation `plan.generation` ([`DrainState::begin_pause_roll`]).
#[allow(clippy::too_many_lines)]
pub(crate) fn run_h4(drain: &DrainState, host: Arc<dyn PauseHost>, plan: &PausePlan) -> H4Outcome {
    let tuning = plan.tuning;
    let started = Instant::now();
    let pause_started_at = Utc::now();
    let manifest_id = new_manifest_id(pause_started_at);
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
                pause_budget_secs: Some(tuning.pause_budget.as_secs()),
                min_resumable_age_secs: Some(tuning.min_resumable_age_secs),
                max_age_secs: tuning.lease_ttl.as_secs(),
            },
            items: Vec::new(),
            events: Vec::new(),
        },
        work: Vec::new(),
        written: false,
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

    // ---- Step 1: let Pending dispatches settle --------------------------------
    enter!(1);
    let settle_until = started + tuning.pending_settle.min(tuning.pause_budget / 2);
    while run.host.pending_dispatches() > 0 && Instant::now() < settle_until {
        std::thread::sleep(tuning.poll);
        enter!(1);
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
    for w in &run.work {
        run.host.hold(&w.cand, true);
    }

    // ---- Step 4: tear down the requeue items at once --------------------------
    enter!(4);
    let stop_started = Instant::now();
    let deadline = stop_started + tuning.pause_budget;
    let mut stopped_any = false;
    for idx in 0..run.work.len() {
        if run.work[idx].item.disposition != Disposition::Requeue {
            continue;
        }
        if !run.stop(idx) {
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
        let mut progressed = false;
        for idx in pending {
            let dir = run.work[idx].cand.pause_dir.clone();
            let safe_point = dir.as_deref().and_then(roll_pause::read_safe_point);
            if let Some(sp) = safe_point {
                if !run.stop(idx) {
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
                run.report(idx, "none");
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
                    let _ = roll_pause::withdraw(dir);
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
    drain.pause_enter_step(plan.generation, 7);
    for idx in stopping(&run) {
        if !run.stop(idx) {
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
    let stop_secs = stop_started.elapsed().as_secs();
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
        // that reuses this item dir. The safe-point record stays for H5.
        if let Some(dir) = &w.cand.pause_dir {
            let _ = roll_pause::withdraw(dir);
        }
    }
    run.manifest.phase = Phase::Paused;
    run.manifest.roll.pause_completed_at = Some(Utc::now());
    run.event(None, "pause_completed", None);
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
            "total_secs": started.elapsed().as_secs(),
        }),
    );
    H4Outcome::Paused {
        manifest_id,
        then_exit,
    }
}

/// H3 Staged → H4: start a pause roll to `target` (design §7 "H3 Staged").
///
/// Returns `true` when a restart is now coming: this call started the pause,
/// or a drain was already in progress (an operator drain wins; the staged
/// binary is picked up when that drain's restart fires, and a then-exit
/// teardown stops the daemon instead, which is logged).
///
/// Returns `false`, having paused nothing, when the roll cannot proceed. There
/// is **no drain fallback**: a roll that cannot pause never waits for the
/// in-flight count to reach zero. It is alerted (H7) and retried on a later
/// tick.
///
/// Must be called inside a tokio runtime context: it spawns the supervisor.
pub fn start_pause_roll(
    drain: &Arc<DrainState>,
    workspace_pool: &Arc<WorkspacePool>,
    fallback_root: &Path,
    event_bus: &Arc<EventBus>,
    target: &RollTarget,
) -> bool {
    let staged_at = Utc::now();
    let refuse = |state: &str, why: String| {
        log::error!("pause_roll: roll NOT started (H7 {state}): {why}");
        let _ = event_bus.publish_generic(
            "daemon.roll.refused",
            serde_json::json!({
                "state": state,
                "reason": why,
                "target_source": target.source.as_str(),
                "to_version": target.to_version,
            }),
        );
        false
    };
    let Some(supervisor) = crate::ipc::detect_supervisor() else {
        return refuse(
            "unsupervised",
            "no supervisor detected (LOOM_DAEMON_SUPERVISOR unset), so nothing would relaunch the \
             daemon after the pause. Dispatch was NOT paused. Restart manually to run the staged \
             binary."
                .to_string(),
        );
    };
    let Some(manifest_path) = pause_manifest::manifest_path() else {
        return refuse(
            "pause-failed",
            "no state directory resolves for the pause manifest (no home directory and \
             LOOM_AUTO_UPDATE_STATE_DIR unset); work is never stopped without being recorded"
                .to_string(),
        );
    };
    // A manifest from an earlier pause that is still within its lease window
    // belongs to a resume that has not finished (or, before #10832, to
    // restart recovery that is still running). Do not pause again on top of it.
    match pause_manifest::load(&manifest_path, staged_at) {
        LoadOutcome::Loaded(m) if !matches!(m.phase, Phase::Resumed | Phase::Abandoned) => {
            return refuse(
                "resume-pending",
                format!(
                    "pause manifest {} ({}, phase {}) from the previous roll is still live; the \
                     next roll waits until it is resumed or older than its {}s max age",
                    m.manifest_id,
                    manifest_path.display(),
                    m.phase.as_str(),
                    m.roll.max_age_secs
                ),
            );
        }
        _ => {}
    }
    let (tuning, rejected) = PauseRollTuning::resolve(fallback_root);
    if let Some(why) = rejected {
        log::error!("pause_roll: {why}");
        let _ = event_bus
            .publish_generic("daemon.roll.config_rejected", serde_json::json!({ "reason": why }));
    }
    let progress = PauseRollStatus {
        budget_secs: tuning.pause_budget.as_secs(),
        min_resumable_age_secs: tuning.min_resumable_age_secs,
        target_source: Some(target.source.as_str().to_string()),
        to_version: target.to_version.clone(),
        ..PauseRollStatus::default()
    };
    match drain.begin_pause_roll(tuning.pause_budget, progress) {
        DrainBegin::Started { generation, .. } => {
            drain.set_roll_target(target.label.clone());
            let plan = PausePlan {
                target: target.clone(),
                tuning,
                manifest_path,
                from_version: env!("CARGO_PKG_VERSION").to_string(),
                generation,
                staged_at,
                supervisor: Some(supervisor),
            };
            let _ = event_bus.publish_generic(
                "daemon.roll.pause_started",
                serde_json::json!({
                    "target_source": target.source.as_str(),
                    "to_version": target.to_version,
                    "from_version": plan.from_version,
                    "pause_budget_secs": tuning.pause_budget.as_secs(),
                    "verify_probation_secs": tuning.verify_probation.as_secs(),
                    "resume_budget_secs": tuning.resume_budget.as_secs(),
                    "min_resumable_age_secs": tuning.min_resumable_age_secs,
                }),
            );
            log::warn!(
                "pause_roll: H4 pausing for a {} roll to {} (budget {}s): every in-flight agent is \
                 stopped at a safe point or requeued, then the daemon restarts",
                target.source.as_str(),
                target.to_version.as_deref().unwrap_or("the rebuilt binary"),
                tuning.pause_budget.as_secs()
            );
            let host: Arc<dyn PauseHost> = Arc::new(host::DaemonPauseHost::new(
                workspace_pool.clone(),
                fallback_root.to_path_buf(),
                event_bus.clone(),
            ));
            tokio::spawn(supervise(
                drain.clone(),
                workspace_pool.clone(),
                fallback_root.to_path_buf(),
                event_bus.clone(),
                host,
                plan,
            ));
            true
        }
        DrainBegin::AlreadyDraining {
            active_then_exit, ..
        } => {
            if active_then_exit {
                log::warn!(
                    "pause_roll: an operator then-exit (teardown) drain is in progress and wins: \
                     the daemon will STOP when drained and will NOT relaunch into the staged \
                     binary. Start it again to pick up the update."
                );
            } else {
                log::warn!(
                    "pause_roll: a drain is already in progress; it wins, and its restart picks \
                     up the staged binary. No pause was started."
                );
            }
            true
        }
    }
}

/// The pause supervisor: run H4 off the runtime, then act on how it ended.
async fn supervise(
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    host: Arc<dyn PauseHost>,
    plan: PausePlan,
) {
    let generation = plan.generation;
    let h4_drain = drain.clone();
    let joined = tokio::task::spawn_blocking(move || run_h4(&h4_drain, host, &plan)).await;
    let outcome = match joined {
        Ok(outcome) => outcome,
        Err(e) => {
            log::error!("pause_roll: the H4 task failed ({e})");
            let committed = drain.snapshot().pause.is_some_and(|p| p.stopped);
            if !committed {
                if drain.pause_ownership(generation) == PauseOwnership::Ours {
                    drain.resolve_timeout(format!(
                        "PAUSE FAILED (H7 pause-failed): the pause task failed ({e}) before any \
                         agent was stopped; dispatch resumed"
                    ));
                }
                return;
            }
            // Agents were already stopped: the restart is the only way they
            // are picked up again (the manifest on disk says `pausing`, which
            // the next start can finish).
            H4Outcome::Paused {
                manifest_id: "unknown".to_string(),
                then_exit: drain.then_exit(),
            }
        }
    };
    match outcome {
        H4Outcome::Paused {
            manifest_id,
            then_exit,
        } => {
            // #8652: close the paused interval BEFORE exiting — the exit is
            // what would otherwise lose it.
            drain.close_paused_interval(Utc::now());
            if then_exit {
                log::warn!(
                    "pause_roll: pause complete (manifest {manifest_id}, phase paused); an \
                     operator then-exit won, so exiting {} and staying down. The next start \
                     resumes or requeues the recorded agents.",
                    crate::ipc::drain_exit_code(true)
                );
                crate::observability::shutdown::exit(crate::ipc::drain_exit_code(true)).await;
            }
            let sup = crate::restart_verify::detect_and_spawn_verifier(std::process::id());
            log::warn!(
                "pause_roll: pause complete (manifest {manifest_id}, phase paused); exiting {} \
                 for a {sup}-supervised relaunch onto the staged binary",
                crate::ipc::drain_exit_code(false)
            );
            crate::observability::shutdown::exit(crate::ipc::drain_exit_code(false)).await;
        }
        H4Outcome::StoodDown { promoted: true } => {
            // Rule 1 of the operator interplay: the drain is now an operator
            // drain with no supervisor of its own. Supervise it.
            crate::ipc::drain_supervisor::run_drain_supervisor(
                drain,
                workspace_pool,
                fallback_root,
                event_bus,
                generation,
                crate::ipc::DRAIN_POLL_INTERVAL,
            )
            .await;
        }
        H4Outcome::StoodDown { promoted: false } | H4Outcome::Failed(_) => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "pause_roll/tests.rs"]
mod tests;
