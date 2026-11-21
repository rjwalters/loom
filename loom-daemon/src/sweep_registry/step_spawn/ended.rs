//! Steps that ended and still await the outcome ledger (#11909).
//!
//! A step's end ([`SweepRegistry::end_step`]) runs under the registry lock,
//! often inside the reaper. It queues the step here; the work finder's step
//! tick drains the queue with the lock released, classifies each step and
//! records it (`step_dispatch::step_end`).
//!
//! **The post-state is the restore's read.** A dead step's claim release
//! (`restore_to_ready/step_restore.rs`) reads the item once, then writes the
//! restore. The ledger's post-state must be computed from that read, before
//! the write: a read after it would always see the claim gone. So the release
//! notes its post-state here ([`note_post`]), keyed by the step's sweep id,
//! and the tick takes it ([`take_post`]). The tick runs after both, so the
//! order of the end and the release does not matter (the reaper releases
//! then ends; startup adoption ends then releases).
//! The release notes its read before it decides anything: for the role
//! steps (`restore_role_step`) `note_step_post` precedes the release plan,
//! so the post-state is noted even when the plan then leaves the claim
//! untouched (its `Nothing` plan fires when the claim label is already
//! gone — the step released it, or another role advanced the item — or
//! when the item is frozen); the Builder's `restore_builder_step` notes
//! its read unconditionally too. The steps with no noted read are three:
//! the `skip_label_flip` early return, a `ChampionMerge` step (no claim
//! label), and a step whose item is held elsewhere — the reaper and startup
//! adoption check `step_claim_held_elsewhere` before `restore_step_claim`,
//! and a roll requeue makes its own ownership check instead
//! (`roll_claim_still_ours` for an issue role, `roll_pr_step_claim_still_ours`
//! for a PR role, which does call `step_claim_held_elsewhere`); all three
//! skip the release entirely, so no read and no note happen; the tick reads
//! the item itself for those.
//! The queue is per workspace root and
//! in-memory: a step that ended just before a restart is not recorded, which
//! costs at most one cooldown the ledger would have armed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::step_role::StepRole;

/// One ended step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedStep {
    pub sweep_id: String,
    /// The number the step ran on (the PR for a PR step).
    pub item: u32,
    pub role: StepRole,
    pub log_path: PathBuf,
    pub ran_for: Duration,
    /// Killed at its role's timeout.
    pub timed_out: bool,
    /// The exit-time provider feedback classified its failure `Exhausted`
    /// (#13966): the launch died on an empty api-key seat, so the ledger
    /// records a refusal (`provider_exhausted`), never a run or a cooldown.
    pub provider_exhausted: bool,
    /// The launcher's own typed preflight-rejection reason for THIS attempt
    /// (#14634): a `preflight_<stage>` token from the worker launcher's
    /// per-spawn socket (or the coarse `preflight_rejected`), accepted only
    /// when the worker exited 78 and wrote no outcome line. The step-log
    /// marker is diagnostic, never the classifier input. The report is
    /// spent only where the ledger would otherwise have recorded
    /// `unclassified`; ownership release, the `failed` class and the
    /// fast-fail ladder are unchanged.
    pub launch_rejection: Option<&'static str>,
    /// The step's own Codex terminal record said the runtime's sandbox
    /// refused every tool call (`SANDBOX_UNAVAILABLE`, #14455): nothing ran,
    /// so the ledger records `failed` / `sandbox_unavailable` and charges the
    /// item nothing.
    pub sandbox_unavailable: bool,
}

type Queue = HashMap<PathBuf, Vec<EndedStep>>;
type Posts = HashMap<(PathBuf, String), Option<crate::step_dispatch::outcome_ledger::PostState>>;

fn posts() -> &'static Mutex<Posts> {
    static POSTS: OnceLock<Mutex<Posts>> = OnceLock::new();
    POSTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Note the post-state a step's claim release read (`None`: the read
/// failed), before its restore write.
pub(crate) fn note_post(
    root: &Path,
    sweep_id: &str,
    post: Option<crate::step_dispatch::outcome_ledger::PostState>,
) {
    let mut p = posts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if p.len() >= 4 * MAX_QUEUED {
        p.clear();
    }
    p.insert((root.to_path_buf(), sweep_id.to_string()), post);
}

/// The post-state noted for `sweep_id`: `Some(read)` when its release took
/// one (`read` is `None` for a failed read), `None` when none was noted.
#[must_use]
pub fn take_post(
    root: &Path,
    sweep_id: &str,
) -> Option<Option<crate::step_dispatch::outcome_ledger::PostState>> {
    posts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(root.to_path_buf(), sweep_id.to_string()))
}

/// As [`take_post`], but leaves the noted post-state for the tick (#14234):
/// the step's span close reads it before the classifier runs, and
/// [`take_post`] must still hand it to `step_end::settle` afterwards.
#[must_use]
pub(crate) fn peek_post(
    root: &Path,
    sweep_id: &str,
) -> Option<Option<crate::step_dispatch::outcome_ledger::PostState>> {
    posts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(root.to_path_buf(), sweep_id.to_string()))
        .copied()
}

fn queue() -> &'static Mutex<Queue> {
    static QUEUE: OnceLock<Mutex<Queue>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// At most this many ended steps wait per root; older ones are dropped.
const MAX_QUEUED: usize = 256;

pub fn push(root: &Path, step: EndedStep) {
    let mut q = queue()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let list = q.entry(root.to_path_buf()).or_default();
    list.push(step);
    if list.len() > MAX_QUEUED {
        list.remove(0);
    }
}

/// Every step of `root` that ended since the last call.
#[must_use]
pub fn take(root: &Path) -> Vec<EndedStep> {
    queue()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(root)
        .unwrap_or_default()
}
