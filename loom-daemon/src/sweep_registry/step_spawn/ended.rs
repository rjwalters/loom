//! Steps that ended and still await the outcome ledger (#11909).
//!
//! A step's end ([`SweepRegistry::end_step`]) runs under the registry lock,
//! often inside the reaper, so it cannot read the forge post-state the
//! ledger needs (`outcome_ledger::classify`). It queues the step here
//! instead; the work finder's step tick drains the queue with the lock
//! released, reads each item's post-state, classifies and records it
//! (`step_dispatch::step_end`). The queue is per workspace root and
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
