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
//! A step whose claim was not released (another claimant holds the item, a
//! merge step with no claim label) has no noted read; the tick reads the
//! item itself. The queue is per workspace root and
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

fn queue() -> &'static Mutex<Queue> {
    static QUEUE: OnceLock<Mutex<Queue>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// At most this many ended steps wait per root; older ones are dropped.
const MAX_QUEUED: usize = 256;

pub(crate) fn push(root: &Path, step: EndedStep) {
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
