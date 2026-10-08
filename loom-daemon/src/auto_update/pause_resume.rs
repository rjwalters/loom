//! Pause-and-roll, the resuming side: H5 Verifying (issue #10832; design
//! `docs/design/daemon-roll-pause-resume.md` §7 "H5 Verifying", §8, §9).
//!
//! [`super::pause_roll`] (H4) stops every agent at a safe point, records them
//! in the pause manifest and exits. This module is what the next process does
//! with that manifest. Without it the manifest is only an audit trail: restart
//! recovery treats each paused agent as a crash and starts it over.
//!
//! # H5, in order
//!
//! 1. **Load** the manifest ([`pause_manifest::load`], typed). `Missing` is the
//!    ordinary start. `Corrupt` and `UnknownVersion` are logged once, reported
//!    (`daemon.roll.manifest.unreadable`) and left to plain restart recovery,
//!    which H4 set up for exactly that case. `Stale` (older than the lease TTL)
//!    is processed, but every `resume` item is requeued (`manifest-stale`)
//!    instead of resumed. A manifest still `pausing` is an H4 that died: an
//!    item with no safe-point record is requeued (`pause-budget-missed`).
//! 2. **Hold dispatch.** The work finder, role runner and epic supervisor stay
//!    paused until step 7. An operator stop or a fleet `paused` state already
//!    in force is respected: H5 waits for it to lift, and requeues everything
//!    once the manifest goes stale rather than resume work on a stopped host.
//! 3. **Verify the teardown.** Nothing from a paused agent's tree may be alive
//!    when it is resumed; whatever is found is reaped and recorded
//!    (`roll.item.residue_reaped`).
//! 4. **Refresh the leases** of everything that will be resumed, first, so the
//!    probation below does not eat into the lease TTL and no other host can
//!    take a claim while this one is still verifying itself.
//! 5. **Health probation.** IPC answering and the heartbeat fresh, sustained
//!    for `verifyProbationSecs`. Nothing is relaunched before it passes. A
//!    failure restarts the window; it is retried until the manifest goes
//!    stale, at which point every item is requeued. (Rollback and quarantine
//!    of an unhealthy binary are #9735's; this only refuses to resume on one.)
//! 6. **Resume** (`phase = resuming`): each `resume` item, in manifest order,
//!    after the §4 cross-checks, from its saved session in the same workspace
//!    with its claim kept. Then **requeue** what cannot be resumed, and finish
//!    the requeue forge writes H4 left `planned`. Bounded by
//!    `resumeBudgetSecs`; an item not reached by then is `resume-timeout`.
//! 7. **Finish**: `phase = resumed` (`abandoned` for a stale manifest), archive
//!    the manifest as `roll-pause-manifest.<id>.done.json` (newest 10 kept),
//!    lift the recovery suppression ([`crate::roll_pause::suppress`]) and the
//!    dispatch hold. The host is back at H0.
//!
//! # What ends an item
//!
//! | Manifest item | H5 does |
//! |---|---|
//! | `resume`, `paused` | resume it; on any refusal requeue with that reason |
//! | `requeue`, `planned` | the forge writes H4 could not finish, then release |
//! | `requeue`, `requeued` | release its lock and journal entry (H4 never does) |
//! | `exited` / `completed` | its work ended by itself: release, no forge write |
//! | PR-set sweep | never resumed: release its per-PR locks |
//! | `resumed` (a crash during H5) | nothing: it is already running |
//!
//! "Release" removes the item's claim lock and journal entry and keeps its
//! checkpoint, so a requeued sweep's next dispatch skips the phases it had
//! finished. An item whose requeue forge write fails is handed to ordinary
//! restart recovery instead (`SweepRegistry::reconstruct_issues`): no item
//! leaves the manifest without a terminal status, and no claim is stranded.
//!
//! # A crash during H5
//!
//! The manifest is rewritten after every item. A process that dies in step 6
//! leaves `phase = resuming`; the next start does not relaunch an item marked
//! `resumed`, and one whose relaunch had started but was not yet marked is
//! found by its lock (a live process resuming that item) and marked instead.
//!
//! # Rollback
//!
//! A binary at or after #10715 that reads a manifest written for another
//! version resumes it all the same and records `resumed_on = rollback`. When
//! the running version is *below* the roll's target, the roll did not take,
//! and the target is recorded so the old binary does not pause the host for
//! it again at once ([`attempt`]). A binary older than #10715 never reads the
//! manifest; H4 left every lock, journal entry and checkpoint in place for
//! its ordinary recovery (design §8).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::pause_manifest::{
    self, Disposition, ItemKind, ItemStatus, LoadOutcome, ManifestEvent, ManifestItem,
    PauseManifest, Phase, Runtime, SafePointRecord,
};
use super::pause_roll::{PauseRollTuning, REASON_BUDGET_MISSED};
use crate::roll_pause::resume::{resume_prompt, valid_session_id, ResumePromptInput};
use crate::sweep_registry::resume_handle::RollResumeLaunch;
use crate::sweep_registry::roll_requeue::RollRequeueNotice;
use crate::sweep_registry::roll_resume::RollResumeRefusal;

pub mod attempt;
pub(crate) mod host;
mod startup;

pub use startup::{arm_at_startup, status, Startup};

use host::{Launched, Liveness, ResumeHost};

/// Requeue reason: the manifest is older than its `max_age_secs`.
pub const REASON_STALE: &str = "manifest-stale";
/// Requeue reason: the saved session cannot be found where the runtime looks.
pub const REASON_STORE: &str = "session-store-unavailable";
/// Requeue reason: the session was already roll-resumed [`MAX_ROLL_RESUMES`] times.
pub const REASON_EXHAUSTED: &str = "resume-attempts-exhausted";
/// Requeue reason: the session container refused the resumed exec.
pub const REASON_SESSION_DOWN: &str = "session-down";
/// Requeue reason: the item was not reached before `resumeBudgetSecs` ran out.
pub const REASON_TIMEOUT: &str = "resume-timeout";
/// Requeue reason: no resumable session was recorded.
pub const REASON_NOT_RESUMABLE: &str = super::pause_classify::REASON_NOT_RESUMABLE;
/// Requeue reason: a PR-set sweep, which this path never resumes.
pub const REASON_PR_SET: &str = "guard-refused:pr-set-sweep";

/// Roll resumes one session may have (design §7).
pub const MAX_ROLL_RESUMES: u32 = 3;
/// Finished manifests kept for audit.
pub const ARCHIVE_KEEP: usize = 10;

/// The timing H5 runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeTuning {
    /// How long health must hold before anything is relaunched.
    pub verify_probation: Duration,
    /// How long step 6 may take.
    pub resume_budget: Duration,
    /// How often health is sampled during probation.
    pub probe_interval: Duration,
    /// How long a relaunched process must stay up to count as resumed.
    pub start_confirm: Duration,
    /// Poll interval while waiting (for a hold to lift, a launch to settle).
    pub poll: Duration,
    /// The window the forge writes of one step share.
    pub forge_window: Duration,
}

impl ResumeTuning {
    /// The H5 timing for a host whose pause budgets are `tuning`.
    #[must_use]
    pub fn from_pause(tuning: &PauseRollTuning) -> Self {
        Self {
            verify_probation: tuning.verify_probation,
            resume_budget: tuning.resume_budget,
            probe_interval: Duration::from_secs(5),
            start_confirm: Duration::from_secs(10),
            poll: Duration::from_millis(500),
            forge_window: Duration::from_secs(45),
        }
    }
}

/// Everything one H5 run needs to know.
#[derive(Debug, Clone)]
pub struct ResumePlan {
    /// Where the live manifest is.
    pub manifest_path: PathBuf,
    /// The version this process is running.
    pub running_version: String,
    pub tuning: ResumeTuning,
}

/// The pause/resume state `status --json` reports (`drain.resume`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseResumeStatus {
    /// The manifest being (or last) processed by this process.
    #[serde(default)]
    pub manifest_id: Option<String>,
    /// The manifest's phase: `pausing`, `paused`, `resuming`, `resumed`,
    /// `abandoned`.
    #[serde(default)]
    pub phase: Option<String>,
    /// How the manifest loaded: `loaded`, `stale`, `corrupt`,
    /// `unknown-version`.
    #[serde(default)]
    pub load: String,
    /// The H5 step in progress: `hold`, `residue`, `lease`, `probation`,
    /// `resume`, `requeue`, `done`, `interrupted`, `unreadable`.
    #[serde(default)]
    pub step: String,
    /// `true` while dispatch is held for this manifest.
    #[serde(default)]
    pub holding: bool,
    #[serde(default)]
    pub from_version: Option<String>,
    #[serde(default)]
    pub to_version: Option<String>,
    #[serde(default)]
    pub running_version: String,
    /// `target` when this process runs the version the roll was going to (or
    /// the roll had none), `rollback` otherwise.
    #[serde(default)]
    pub resumed_on: Option<String>,
    /// Items in the manifest.
    #[serde(default)]
    pub items: u32,
    /// Agents resumed from their saved session.
    #[serde(default)]
    pub resumed: u32,
    /// Agents whose work had ended by itself during the pause.
    #[serde(default)]
    pub completed: u32,
    /// Requeued agents, by reason (H4's and H5's together).
    #[serde(default)]
    pub requeued_by_reason: BTreeMap<String, u32>,
    /// Items handed to ordinary restart recovery (a requeue forge write that
    /// failed).
    #[serde(default)]
    pub recovered: u32,
    /// Items with processes still alive at H5, which were reaped.
    #[serde(default)]
    pub residue_reaped: u32,
    /// Health samples that failed during probation.
    #[serde(default)]
    pub health_failures: u32,
    /// `verifyProbationSecs` in force.
    #[serde(default)]
    pub verify_probation_secs: u64,
    /// `resumeBudgetSecs` in force.
    #[serde(default)]
    pub resume_budget_secs: u64,
    /// The manifest's `max_age_secs` (the lease TTL at pause time).
    #[serde(default)]
    pub max_age_secs: u64,
    /// Observed seconds until probation passed.
    #[serde(default)]
    pub probation_secs: Option<u64>,
    /// Observed seconds of step 6.
    #[serde(default)]
    pub resume_secs: Option<u64>,
    /// Observed seconds from the start of the pause (H4) to the end of H5:
    /// the window the lease TTL bounds.
    #[serde(default)]
    pub pause_to_resume_secs: Option<u64>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// A short note on the last transition.
    #[serde(default)]
    pub note: Option<String>,
}

/// How an H5 run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H5Outcome {
    /// No manifest: an ordinary start.
    NoManifest,
    /// The manifest could not be used (corrupt, or a newer schema). Every item
    /// is left to plain restart recovery.
    Unreadable(PauseResumeStatus),
    /// Every item is resumed, requeued or released, and the manifest archived.
    Finished(PauseResumeStatus),
    /// A drain replaced the dispatch hold before H5 finished. The manifest is
    /// left `resuming` (or untouched) for the next start.
    Interrupted(PauseResumeStatus),
}

/// What H5 does with one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// Already running (a crash during H5).
    Done,
    /// Relaunch the saved session.
    Resume,
    /// Release the claim and record why; with the forge writes or without.
    Requeue { forge: bool },
    /// Its work ended by itself during the pause: release, write nothing.
    Complete,
    /// Hand it to ordinary restart recovery.
    Recover,
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn secs_since(t: Instant) -> u64 {
    t.elapsed().as_secs()
}

/// `target` or `rollback` (design §8).
fn resumed_on(running: &str, to_version: &str) -> &'static str {
    let bare = |v: &str| v.trim().trim_start_matches('v').to_string();
    if to_version.trim().is_empty() || to_version == "unknown" || bare(running) == bare(to_version)
    {
        "target"
    } else {
        "rollback"
    }
}

/// Decide one item's fate from its manifest record. `stale` is the manifest's
/// load outcome; `safe_point` is the record found on disk for an item H4 never
/// got to mark `paused`.
fn plan_item(item: &mut ManifestItem, stale: bool, safe_point: Option<SafePointRecord>) -> Fate {
    let (disposition, reason) = item.effective_disposition();
    match &item.status {
        ItemStatus::Resumed => return Fate::Done,
        ItemStatus::Requeued => return Fate::Requeue { forge: false },
        // The agent ended by itself while the pause was running. Whatever it
        // was doing, it finished doing it: this is completed work, not a
        // crash to resume from a checkpoint.
        ItemStatus::Exited | ItemStatus::Completed => return Fate::Complete,
        ItemStatus::Failed => return Fate::Recover,
        ItemStatus::Planned
        | ItemStatus::Stopping
        | ItemStatus::Paused
        | ItemStatus::Unknown(_) => {}
    }
    let requeue = |item: &mut ManifestItem, reason: &str| {
        item.disposition = Disposition::Requeue;
        item.reason = Some(reason.to_string());
        Fate::Requeue { forge: true }
    };
    if disposition != Disposition::Resume {
        let reason = reason.unwrap_or_else(|| "unknown".to_string());
        return requeue(item, &reason);
    }
    if item.kind == ItemKind::Sweep && item.issue.is_none() {
        return requeue(item, REASON_PR_SET);
    }
    if stale {
        return requeue(item, REASON_STALE);
    }
    if item.status != ItemStatus::Paused {
        // H4 died before it marked this item. It is resumable only if its
        // agent had already parked.
        match safe_point {
            Some(record) => {
                item.safe_point = Some(record);
                item.status = ItemStatus::Paused;
            }
            None => return requeue(item, REASON_BUDGET_MISSED),
        }
    }
    let resumable = item.resume_handle.as_ref().is_some_and(|h| {
        matches!(h.runtime, Runtime::Claude | Runtime::Codex)
            && h.session_id.as_deref().is_some_and(valid_session_id)
    });
    if !resumable {
        return requeue(item, REASON_NOT_RESUMABLE);
    }
    if item
        .resume_handle
        .as_ref()
        .is_some_and(|h| h.resume_count >= MAX_ROLL_RESUMES)
    {
        return requeue(item, REASON_EXHAUSTED);
    }
    Fate::Resume
}

/// The launch that resumes `item`: its saved session, the resume prompt, and
/// the lineage the new run records. `None` when the item has no session.
#[must_use]
pub(crate) fn launch_of(item: &ManifestItem, manifest: &PauseManifest) -> Option<RollResumeLaunch> {
    let handle = item.resume_handle.as_ref()?;
    let session_id = handle.session_id.clone()?;
    let prompt = resume_prompt(&ResumePromptInput {
        from_version: manifest.roll.from_version.clone(),
        to_version: Some(manifest.roll.to_version.clone()).filter(|v| v != "unknown"),
        parked_tool: item.safe_point.as_ref().and_then(|s| s.parked_tool.clone()),
        parked_summary: item
            .safe_point
            .as_ref()
            .and_then(|s| s.parked_summary.clone()),
    });
    Some(RollResumeLaunch {
        runtime: handle.runtime.as_str().to_string(),
        session_id,
        prompt,
        resume_of: item.id.clone(),
        resume_count: handle.resume_count.saturating_add(1),
        agent_started_at: item.agent_started_at.map(|t| t.to_rfc3339()),
        session_store: handle.session_store.clone(),
        account: handle.account.clone(),
        container: handle
            .container
            .as_ref()
            .and_then(|c| c.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        sandbox: handle.sandbox.clone(),
        lease_sweep_id: handle.lease_sweep_id.clone(),
    })
}

/// One item while H5 works on it.
struct Work {
    fate: Fate,
    launched: Option<Launched>,
    reported: bool,
}

/// The H5 run's mutable state.
struct Run<'a> {
    host: Arc<dyn ResumeHost>,
    plan: &'a ResumePlan,
    manifest: PauseManifest,
    work: Vec<Work>,
    status: PauseResumeStatus,
    stale: bool,
}

impl Run<'_> {
    fn event(&mut self, item: Option<&str>, event: &str, detail: Option<String>) {
        self.manifest.events.push(ManifestEvent {
            at: rfc3339(Utc::now()),
            by_version: Some(self.plan.running_version.clone()),
            item: item.map(str::to_string),
            event: event.to_string(),
            detail,
        });
    }

    fn save(&self) {
        if let Err(e) = pause_manifest::save(&self.plan.manifest_path, &self.manifest) {
            log::error!(
                "pause_resume: could not rewrite the pause manifest {} ({e}); continuing",
                self.plan.manifest_path.display()
            );
        }
    }

    fn step(&mut self, step: &str) {
        self.status.step = step.to_string();
        self.status.phase = Some(self.manifest.phase.as_str().to_string());
        self.status.holding = self.host.dispatch_held();
        self.host.publish(&self.status);
    }

    fn is_stale_now(&self) -> bool {
        let max_age = i64::try_from(self.manifest.roll.max_age_secs).unwrap_or(i64::MAX);
        self.manifest.age_secs(Utc::now()) > max_age
    }

    /// The manifest went stale while H5 was waiting: nothing is resumed.
    fn go_stale(&mut self) {
        if self.stale {
            return;
        }
        self.stale = true;
        self.status.load = "stale".to_string();
        self.event(None, "went_stale", Some("while waiting to resume".to_string()));
        for idx in 0..self.work.len() {
            if self.work[idx].fate == Fate::Resume {
                self.requeue_as(idx, REASON_STALE, None);
            }
        }
    }

    /// Turn `idx` into a requeue with `reason`.
    fn requeue_as(&mut self, idx: usize, reason: &str, detail: Option<String>) {
        let item = &mut self.manifest.items[idx];
        item.disposition = Disposition::Requeue;
        item.reason = Some(reason.to_string());
        // `issue-closed` is never requeued on the forge, only recorded (#9463).
        let forge = reason != crate::sweep_registry::roll_resume::REASON_ISSUE_CLOSED;
        self.work[idx].fate = Fate::Requeue { forge };
        let id = item.id.clone();
        self.event(
            Some(&id),
            "resume_refused",
            Some(detail.map_or_else(|| reason.to_string(), |d| format!("{reason}: {d}"))),
        );
    }

    /// Emit `daemon.roll.item` for `idx` once.
    fn report(&mut self, idx: usize, forge: &str, new_item: Option<&str>) {
        if self.work[idx].reported {
            return;
        }
        self.work[idx].reported = true;
        let item = &self.manifest.items[idx];
        let now = Utc::now();
        let payload = serde_json::json!({
            "stage": "resume",
            "manifest_id": self.manifest.manifest_id,
            "item_id": item.id,
            "new_item_id": new_item,
            "kind": item.kind.as_str(),
            "runtime": item.resume_handle.as_ref().map(|h| h.runtime.as_str().to_string()),
            "disposition": item.disposition.as_str(),
            "status": item.status.as_str(),
            "reason": item.reason,
            "agent_age_secs": item
                .agent_started_at
                .and_then(|t| u64::try_from((now - t).num_seconds()).ok()),
            "issue": item.issue,
            "role": item.role,
            "from_version": self.manifest.roll.from_version,
            "to_version": self.manifest.roll.to_version,
            "running_version": self.plan.running_version,
            "resumed_on": self.status.resumed_on,
            "verify_probation_secs": self.plan.tuning.verify_probation.as_secs(),
            "resume_budget_secs": self.plan.tuning.resume_budget.as_secs(),
            "forge": forge,
        });
        self.host.emit("daemon.roll.item", payload);
    }

    fn notice(&self, idx: usize) -> RollRequeueNotice {
        let item = &self.manifest.items[idx];
        RollRequeueNotice {
            from_version: self
                .manifest
                .roll
                .from_version
                .clone()
                .unwrap_or_else(|| self.manifest.written_by.version.clone()),
            to_version: Some(self.manifest.roll.to_version.clone()).filter(|v| v != "unknown"),
            phase: item.checkpoint_phase.clone(),
            agent_age_secs: item
                .agent_started_at
                .and_then(|t| u64::try_from((Utc::now() - t).num_seconds()).ok()),
            reason: item.reason.clone().unwrap_or_default(),
            worktree: item.worktree.as_ref().map(|w| w.path.clone()),
            worktree_dirty: item.worktree.as_ref().and_then(|w| w.dirty),
            manifest_id: self.manifest.manifest_id.clone(),
        }
    }

    fn count_requeue(&mut self, idx: usize) {
        let reason = self.manifest.items[idx]
            .reason
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        *self.status.requeued_by_reason.entry(reason).or_insert(0) += 1;
    }

    /// Mark `idx` resumed as `launched`.
    fn mark_resumed(&mut self, idx: usize, new_item: &str, how: &str) {
        let item = &mut self.manifest.items[idx];
        item.status = ItemStatus::Resumed;
        item.reason = None;
        if let Some(handle) = item.resume_handle.as_mut() {
            handle.resume_count = handle.resume_count.saturating_add(1);
        }
        let id = item.id.clone();
        self.work[idx].fate = Fate::Done;
        self.status.resumed += 1;
        self.event(Some(&id), "resumed", Some(format!("{how} as {new_item}")));
        self.report(idx, "none", Some(new_item));
        self.host
            .settle(&self.manifest.manifest_id, &self.manifest.items[idx], new_item);
        self.save();
    }

    fn indices(&self, wanted: impl Fn(Fate) -> bool) -> Vec<usize> {
        (0..self.work.len())
            .filter(|i| wanted(self.work[*i].fate))
            .collect()
    }
}

/// Run `job` for each of `indices` on its own thread and collect what answers
/// within `window`. A slow forge must not hold H5: what has not answered is
/// reported as `Err`.
fn fan_out<T: Send + 'static>(
    indices: &[usize],
    window: Duration,
    job: impl Fn(usize) -> Box<dyn FnOnce() -> T + Send>,
) -> BTreeMap<usize, Option<T>> {
    let (tx, rx) = std::sync::mpsc::channel::<(usize, T)>();
    for idx in indices {
        let (tx, idx, run) = (tx.clone(), *idx, job(*idx));
        std::thread::spawn(move || {
            let _ = tx.send((idx, run()));
        });
    }
    drop(tx);
    let deadline = Instant::now() + window;
    let mut out: BTreeMap<usize, Option<T>> = indices.iter().map(|i| (*i, None)).collect();
    let mut left = indices.len();
    while left > 0 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((idx, value)) => {
                out.insert(idx, Some(value));
                left -= 1;
            }
            Err(_) => break,
        }
    }
    out
}

/// Archive a finished manifest beside the live path and keep the newest
/// [`ARCHIVE_KEEP`]. Returns the archive path.
fn archive(manifest_path: &Path, manifest_id: &str) -> Option<PathBuf> {
    let dir = manifest_path.parent()?;
    let safe: String = manifest_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let done = dir.join(format!("roll-pause-manifest.{safe}.done.json"));
    if let Err(e) = std::fs::rename(manifest_path, &done) {
        log::error!(
            "pause_resume: could not archive {} as {} ({e}); removing it so the next roll is not \
             blocked",
            manifest_path.display(),
            done.display()
        );
        let _ = std::fs::remove_file(manifest_path);
        return None;
    }
    let mut archives: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("roll-pause-manifest.") && name.ends_with(".done.json")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    archives.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, old) in archives.into_iter().skip(ARCHIVE_KEEP) {
        let _ = std::fs::remove_file(old);
    }
    Some(done)
}

/// Run H5 (design §7). Blocking; call it off the async runtime.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_h5(host: Arc<dyn ResumeHost>, plan: &ResumePlan) -> H5Outcome {
    let started = Instant::now();
    let tuning = plan.tuning;
    let mut status = PauseResumeStatus {
        running_version: plan.running_version.clone(),
        verify_probation_secs: tuning.verify_probation.as_secs(),
        resume_budget_secs: tuning.resume_budget.as_secs(),
        started_at: Some(Utc::now()),
        ..PauseResumeStatus::default()
    };

    // ---- Step 1: load ---------------------------------------------------------
    let (manifest, stale) = match pause_manifest::load(&plan.manifest_path, Utc::now()) {
        LoadOutcome::Missing => return H5Outcome::NoManifest,
        LoadOutcome::Loaded(m) => (m, false),
        LoadOutcome::Stale(m) => (m, true),
        unreadable => {
            let (load, why) = match unreadable {
                LoadOutcome::UnknownVersion(v) => (
                    "unknown-version",
                    format!(
                        "schema_version {v} is newer than this binary knows ({})",
                        pause_manifest::SCHEMA_VERSION
                    ),
                ),
                LoadOutcome::Corrupt(why) => ("corrupt", why),
                _ => ("corrupt", "unreadable".to_string()),
            };
            log::error!(
                "pause_resume: the pause manifest {} cannot be used ({load}: {why}). Nothing is \
                 resumed from it; every agent it recorded is left to ordinary restart recovery, \
                 which releases or recovers each claim (design §8).",
                plan.manifest_path.display()
            );
            host.emit(
                "daemon.roll.manifest.unreadable",
                serde_json::json!({
                    "path": plan.manifest_path.display().to_string(),
                    "outcome": load,
                    "detail": why,
                    "running_version": plan.running_version,
                }),
            );
            status.load = load.to_string();
            status.step = "unreadable".to_string();
            status.note = Some(why);
            status.finished_at = Some(Utc::now());
            host.publish(&status);
            return H5Outcome::Unreadable(status);
        }
    };
    status.manifest_id = Some(manifest.manifest_id.clone());
    status.load = if stale { "stale" } else { "loaded" }.to_string();
    status.from_version.clone_from(&manifest.roll.from_version);
    status.to_version = Some(manifest.roll.to_version.clone());
    status.max_age_secs = manifest.roll.max_age_secs;
    status.items = u32::try_from(manifest.items.len()).unwrap_or(u32::MAX);
    let on = resumed_on(&plan.running_version, &manifest.roll.to_version);
    status.resumed_on = Some(on.to_string());

    let mut run = Run {
        host,
        plan,
        manifest,
        work: Vec::new(),
        status,
        stale,
    };
    let manifest_id = run.manifest.manifest_id.clone();

    // A manifest that is already terminal was finished by a process that died
    // before it could archive it.
    if matches!(run.manifest.phase, Phase::Resumed | Phase::Abandoned) {
        archive(&plan.manifest_path, &manifest_id);
        run.host
            .finish(&manifest_id, "the pause manifest was already finished");
        run.status.finished_at = Some(Utc::now());
        run.step("done");
        return H5Outcome::Finished(run.status);
    }

    let interrupted_h4 = run.manifest.phase == Phase::Pausing;
    let resuming = run.manifest.phase == Phase::Resuming;
    run.event(
        None,
        "resume_started",
        Some(format!(
            "phase {}, {} item(s), running {}{}",
            run.manifest.phase.as_str(),
            run.manifest.items.len(),
            plan.running_version,
            if stale { ", STALE" } else { "" }
        )),
    );
    if on == "rollback" {
        run.event(
            None,
            "resumed_on",
            Some(format!(
                "rollback (running {}, the roll was to {})",
                plan.running_version, run.manifest.roll.to_version
            )),
        );
        // The roll did not take: do not pause the host for this target again
        // at once (see `attempt`).
        if attempt::below(&plan.running_version, &run.manifest.roll.to_version) {
            if let Some(dir) = plan.manifest_path.parent() {
                match attempt::record_failure(
                    dir,
                    &run.manifest.roll.to_version,
                    run.manifest.roll.to_artifact_sha256.as_deref(),
                    &plan.running_version,
                    Utc::now(),
                ) {
                    Ok(failed) => log::warn!(
                        "pause_resume: the roll to {} did not take (this process is {}); no roll \
                         to it starts before {} (attempt {})",
                        failed.version,
                        plan.running_version,
                        failed.not_before.to_rfc3339(),
                        failed.attempts
                    ),
                    Err(e) => log::warn!("pause_resume: could not record the failed roll: {e}"),
                }
            }
        }
    }

    for idx in 0..run.manifest.items.len() {
        let on_disk = (run.manifest.items[idx].status != ItemStatus::Paused)
            .then(|| run.host.safe_point(&run.manifest.items[idx]))
            .flatten();
        let fate = plan_item(&mut run.manifest.items[idx], stale, on_disk);
        run.work.push(Work {
            fate,
            launched: None,
            reported: false,
        });
    }
    if interrupted_h4 {
        run.event(None, "pause_finished", Some("H4 did not complete; finished at H5".to_string()));
    }
    // A crash during an earlier H5: an item whose relaunch had started is
    // already running. Mark it; never launch it twice.
    if resuming {
        for idx in run.indices(|f| f == Fate::Resume) {
            if let Some(new_id) = run.host.already_resumed(&run.manifest.items[idx]) {
                run.mark_resumed(idx, &new_id, "found already running");
            }
        }
    }
    run.save();

    // ---- Step 2: hold dispatch ------------------------------------------------
    run.step("hold");
    while !run.host.hold_dispatch() {
        // Another hold is in force (an operator stop, a fleet pause, a drain):
        // dispatch is paused either way. Do not resume work on a stopped host;
        // once the manifest goes stale its claims are given back instead.
        if run.is_stale_now() {
            run.go_stale();
            break;
        }
        std::thread::sleep(tuning.poll);
    }
    let holding = run.host.dispatch_held();

    // ---- Step 3: verify the teardown ------------------------------------------
    run.step("residue");
    for idx in run.indices(|f| !matches!(f, Fate::Done)) {
        // An item H4 already requeued gave its claim back: by now someone
        // else may be working in its worktree, so only its own scope counts.
        let scope_only = run.work[idx].fate == (Fate::Requeue { forge: false });
        let report = run.host.reap_residue(&run.manifest.items[idx], scope_only);
        if report.pids.is_empty() && !report.scope_stopped {
            continue;
        }
        run.status.residue_reaped += 1;
        let id = run.manifest.items[idx].id.clone();
        let detail = format!(
            "{} pid(s){}{}",
            report.pids.len(),
            if report.scope_stopped {
                ", scope stopped"
            } else {
                ""
            },
            if report.survivors.is_empty() {
                String::new()
            } else {
                format!(", {} survived", report.survivors.len())
            }
        );
        log::warn!("pause_resume: {id}: reaped residue of the paused agent: {detail}");
        run.event(Some(&id), "residue_reaped", Some(detail));
        run.host.emit(
            "daemon.roll.item.residue_reaped",
            serde_json::json!({
                "manifest_id": manifest_id,
                "item_id": id,
                "pids": report.pids,
                "survivors": report.survivors,
                "scope_stopped": report.scope_stopped,
            }),
        );
    }

    // ---- Step 4: refresh the leases, first -------------------------------------
    run.step("lease");
    let to_refresh = run.indices(|f| f == Fate::Resume);
    if !to_refresh.is_empty() {
        let per_call = tuning.forge_window;
        let refreshed = fan_out(&to_refresh, tuning.forge_window, |idx| {
            let (host, item) = (Arc::clone(&run.host), run.manifest.items[idx].clone());
            Box::new(move || host.refresh_lease(&item, per_call))
        });
        for (idx, result) in refreshed {
            let id = run.manifest.items[idx].id.clone();
            match result {
                Some(Ok(())) => run.manifest.items[idx].lease_refreshed_at = Some(Utc::now()),
                Some(Err(e)) => {
                    log::warn!("pause_resume: {id}: lease refresh failed: {e}");
                    run.event(Some(&id), "lease_refresh_failed", Some(e));
                }
                None => run.event(
                    Some(&id),
                    "lease_refresh_failed",
                    Some("no answer in the forge window".to_string()),
                ),
            }
        }
        run.save();
    }

    // ---- Step 5: health probation ---------------------------------------------
    run.step("probation");
    let mut window_started: Option<Instant> = None;
    let mut waiting_logged = false;
    let mut interrupted = false;
    while holding && !run.stale {
        if !run.host.dispatch_held() {
            interrupted = true;
            break;
        }
        if run.is_stale_now() {
            run.go_stale();
            break;
        }
        match run.host.health_sample() {
            Ok(()) => {
                let since = *window_started.get_or_insert_with(Instant::now);
                if since.elapsed() >= tuning.verify_probation {
                    break;
                }
            }
            Err(why) => {
                // Not sustained: the window starts over. Before the first
                // good sample this is the process still coming up.
                if window_started.is_none() && !waiting_logged {
                    waiting_logged = true;
                    log::info!("pause_resume: waiting for the first healthy sample ({why})");
                }
                if window_started.take().is_some() {
                    run.status.health_failures += 1;
                    log::error!(
                        "pause_resume: health probation failed ({why}); nothing is resumed on an \
                         unhealthy binary. Retrying until the manifest is {}s old.",
                        run.manifest.roll.max_age_secs
                    );
                    run.event(None, "health_failed", Some(why.clone()));
                    run.host.emit(
                        "daemon.roll.health_failed",
                        serde_json::json!({
                            "manifest_id": manifest_id,
                            "reason": why,
                            "running_version": plan.running_version,
                            "to_version": run.manifest.roll.to_version,
                            "failures": run.status.health_failures,
                        }),
                    );
                    run.save();
                    run.step("probation");
                }
            }
        }
        std::thread::sleep(tuning.probe_interval);
    }
    run.status.probation_secs = Some(secs_since(started));
    if interrupted {
        return interrupt(run, "a drain replaced the dispatch hold during health probation");
    }

    // ---- Step 6: resume, then requeue -----------------------------------------
    run.manifest.phase = Phase::Resuming;
    run.event(None, "probation_passed", None);
    run.save();
    run.step("resume");
    let resume_started = Instant::now();
    let deadline = resume_started + tuning.resume_budget;
    for idx in run.indices(|f| f == Fate::Resume) {
        if !run.host.dispatch_held() {
            return interrupt(run, "a drain replaced the dispatch hold during the resume");
        }
        if Instant::now() >= deadline {
            run.requeue_as(idx, REASON_TIMEOUT, None);
            continue;
        }
        let item = run.manifest.items[idx].clone();
        let Some(launch) = launch_of(&item, &run.manifest) else {
            run.requeue_as(idx, REASON_NOT_RESUMABLE, None);
            continue;
        };
        let left = deadline.saturating_duration_since(Instant::now());
        let attempt = run
            .host
            .check(&item, &launch)
            .and_then(|()| run.host.launch(&item, &launch, left));
        match attempt {
            Ok(launched) => {
                run.event(
                    Some(&item.id),
                    "relaunched",
                    Some(format!("as {} (pid {:?})", launched.item_id, launched.pid)),
                );
                run.work[idx].launched = Some(launched);
            }
            Err(RollResumeRefusal { reason, detail }) => {
                log::warn!("pause_resume: {}: not resumed ({reason}: {detail})", item.id);
                run.requeue_as(idx, &reason, Some(detail));
            }
        }
        run.save();
    }
    // A relaunched process counts as resumed once it has stayed up for the
    // start-confirm window (or ended cleanly). One that dies inside it did not
    // start its session.
    loop {
        let pending: Vec<usize> = (0..run.work.len())
            .filter(|i| run.work[*i].fate == Fate::Resume && run.work[*i].launched.is_some())
            .collect();
        if pending.is_empty() {
            break;
        }
        let out_of_time = Instant::now() >= deadline;
        for idx in pending {
            let Some(launched) = run.work[idx].launched.clone() else {
                continue;
            };
            let item = run.manifest.items[idx].clone();
            match run.host.liveness(&item, &launched) {
                Liveness::Finished => run.mark_resumed(idx, &launched.item_id, "resumed and ended"),
                Liveness::Running
                    if out_of_time || launched.at.elapsed() >= tuning.start_confirm =>
                {
                    run.mark_resumed(idx, &launched.item_id, "resumed");
                }
                Liveness::Running => {}
                Liveness::Died { reason, detail } => {
                    log::warn!("pause_resume: {}: the resume did not start ({detail})", item.id);
                    run.host.abandon(&item, &launched);
                    run.work[idx].launched = None;
                    run.requeue_as(idx, &reason, Some(detail));
                    run.save();
                }
            }
        }
        std::thread::sleep(tuning.poll.min(Duration::from_millis(200)));
    }
    run.status.resume_secs = Some(secs_since(resume_started));

    run.step("requeue");
    let with_forge = run.indices(|f| f == Fate::Requeue { forge: true });
    let done = fan_out(&with_forge, tuning.forge_window.saturating_mul(2), |idx| {
        let (host, item, notice) =
            (Arc::clone(&run.host), run.manifest.items[idx].clone(), run.notice(idx));
        Box::new(move || host.requeue(&item, &notice, true))
    });
    for (idx, result) in done {
        let id = run.manifest.items[idx].id.clone();
        match result {
            Some(Ok(())) => {
                run.manifest.items[idx].status = ItemStatus::Requeued;
                run.count_requeue(idx);
                run.event(Some(&id), "requeued", run.manifest.items[idx].reason.clone());
                run.report(idx, "done", None);
            }
            failed => {
                // The claim is not stranded: ordinary recovery takes it.
                let why = match failed {
                    Some(Err(e)) => e,
                    _ => "no answer in the forge window".to_string(),
                };
                log::warn!(
                    "pause_resume: {id}: requeue forge write failed ({why}); handing the item to \
                     restart recovery"
                );
                run.manifest.items[idx].status = ItemStatus::Failed;
                run.work[idx].fate = Fate::Recover;
                run.event(Some(&id), "requeue_failed", Some(why));
            }
        }
    }
    for idx in run.indices(|f| f == Fate::Requeue { forge: false }) {
        let item = run.manifest.items[idx].clone();
        let notice = run.notice(idx);
        if let Err(e) = run.host.requeue(&item, &notice, false) {
            log::warn!("pause_resume: {}: release failed: {e}", item.id);
        }
        run.manifest.items[idx].status = ItemStatus::Requeued;
        run.count_requeue(idx);
        run.report(idx, "none", None);
    }
    for idx in run.indices(|f| f == Fate::Complete) {
        let item = run.manifest.items[idx].clone();
        let notice = run.notice(idx);
        if let Err(e) = run.host.requeue(&item, &notice, false) {
            log::warn!("pause_resume: {}: release failed: {e}", item.id);
        }
        run.manifest.items[idx].status = ItemStatus::Completed;
        run.status.completed += 1;
        run.event(Some(&item.id), "completed", Some("its work ended during the pause".to_string()));
        run.report(idx, "none", None);
    }
    for idx in run.indices(|f| f == Fate::Recover) {
        let item = run.manifest.items[idx].clone();
        run.host.recover(&manifest_id, &item);
        run.status.recovered += 1;
        run.event(Some(&item.id), "recovered", Some("left to restart recovery".to_string()));
        run.report(idx, "deferred", None);
    }

    // ---- Step 7: finish ---------------------------------------------------------
    run.manifest.phase = if run.stale {
        Phase::Abandoned
    } else {
        Phase::Resumed
    };
    let pause_to_resume = u64::try_from(run.manifest.age_secs(Utc::now())).ok();
    run.status.pause_to_resume_secs = pause_to_resume;
    run.event(None, "resume_completed", None);
    run.save();
    archive(&plan.manifest_path, &manifest_id);
    // The roll took: forget any failed attempt at it.
    if on == "target" {
        if let Some(dir) = plan.manifest_path.parent() {
            attempt::clear(dir);
        }
    }
    let note = format!(
        "pause-and-roll resume finished (manifest {manifest_id}, {}): {} resumed, {} requeued, {} \
         completed, {} left to restart recovery; dispatch resumed",
        run.manifest.phase.as_str(),
        run.status.resumed,
        run.status.requeued_by_reason.values().sum::<u32>(),
        run.status.completed,
        run.status.recovered
    );
    log::warn!("pause_resume: {note}");
    run.host.finish(&manifest_id, &note);
    run.host.emit(
        "daemon.roll.paused.resumed",
        serde_json::json!({
            "manifest_id": manifest_id,
            "phase": run.manifest.phase.as_str(),
            "from_version": run.manifest.roll.from_version,
            "to_version": run.manifest.roll.to_version,
            "running_version": plan.running_version,
            "resumed_on": on,
            "items": run.status.items,
            "resumed": run.status.resumed,
            "completed": run.status.completed,
            "requeued_by_reason": run.status.requeued_by_reason,
            "recovered": run.status.recovered,
            "residue_reaped": run.status.residue_reaped,
            "health_failures": run.status.health_failures,
            "verify_probation_secs": tuning.verify_probation.as_secs(),
            "resume_budget_secs": tuning.resume_budget.as_secs(),
            "probation_secs": run.status.probation_secs,
            "resume_secs": run.status.resume_secs,
            "pause_to_resume_secs": pause_to_resume,
            "max_age_secs": run.manifest.roll.max_age_secs,
        }),
    );
    run.status.note = Some(note);
    run.status.finished_at = Some(Utc::now());
    run.step("done");
    H5Outcome::Finished(run.status)
}

/// H5 lost the dispatch hold to a real drain: stop, and leave the manifest
/// (and the recovery suppression) for the next start to finish.
fn interrupt(mut run: Run<'_>, why: &str) -> H5Outcome {
    log::warn!(
        "pause_resume: {why}; stopping. The pause manifest {} stays in place and the next start \
         finishes it.",
        run.manifest.manifest_id
    );
    run.event(None, "resume_interrupted", Some(why.to_string()));
    run.save();
    run.status.note = Some(why.to_string());
    run.step("interrupted");
    H5Outcome::Interrupted(run.status)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "pause_resume/tests.rs"]
mod tests;
