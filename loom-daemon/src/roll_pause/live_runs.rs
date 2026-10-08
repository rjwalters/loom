//! Live role runs, for the pause-and-roll H4 snapshot (issue #10831; design
//! `docs/design/daemon-roll-pause-resume.md` §4 "Role run").
//!
//! Role runs (Champion, Curator, Judge, …) have no registry entry and no claim
//! lock (`ipc.rs`'s `restart_scheduled_message` says as much), so before
//! #10831 nothing could enumerate them across a restart. The role runner now
//! registers each run here for as long as its child runs: its item id (the
//! `LOOM_DAEMON_ITEM_ID` the pause hook keys on), pid and process group, start
//! time, timeout and resume handle. The H4 pause reads [`snapshot`] next to the
//! sweep registry so a role run is paused, reset or requeued exactly like a
//! sweep (operator decision Q7).
//!
//! Registration is an RAII [`LiveRunGuard`]: every return path of the role
//! launcher drops it, so a finished run never lingers here.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

/// One live role run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRun {
    /// The synthetic item id (`role-<role>-<time>-<rand>`).
    pub item_id: String,
    /// The role (`champion`, `judge`, …).
    pub role: String,
    /// The workspace root the run belongs to.
    pub root: PathBuf,
    /// The role child's pid. It leads its own process group.
    pub pid: u32,
    /// When the run's session first started.
    pub started_at: DateTime<Utc>,
    /// The runtime (`claude` / `codex`).
    pub runtime: String,
    /// The session id known at launch: the pinned Claude session id, or the
    /// saved session of a run a roll resumed (#10832, either runtime).
    pub claude_session_id: Option<String>,
    /// The systemd scope unit the spawn script was asked to use, when one was
    /// named (Linux). Not proof the scope exists.
    pub scope_unit: Option<String>,
    /// The run's pause state dir root (`<root>/.loom/state/roll-pause`).
    pub pause_root: PathBuf,
    /// The model the run was launched with, when known.
    pub model: Option<String>,
    /// The run's timeout and when it started (monotonic), so H4 can record the
    /// time it has left (`timeout_remaining_secs`).
    pub timeout: Duration,
    pub started_mono: Instant,
    /// Set when this run resumes a session a roll paused (#10832): the saved
    /// session and its lineage, so a second roll records them again.
    pub(crate) resume: Option<crate::sweep_registry::resume_handle::RollResumeLaunch>,
}

impl LiveRun {
    /// Seconds of the run's timeout still left at `now`.
    #[must_use]
    pub fn timeout_remaining_secs(&self, now: Instant) -> u64 {
        self.timeout
            .saturating_sub(now.saturating_duration_since(self.started_mono))
            .as_secs()
    }
}

fn registry() -> &'static Mutex<BTreeMap<String, LiveRun>> {
    static RUNS: OnceLock<Mutex<BTreeMap<String, LiveRun>>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Removes its run from the registry when dropped.
#[derive(Debug)]
pub struct LiveRunGuard {
    item_id: String,
}

impl Drop for LiveRunGuard {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.item_id);
    }
}

/// Register a live role run until the returned guard is dropped.
#[must_use]
pub fn register(run: LiveRun) -> LiveRunGuard {
    let item_id = run.item_id.clone();
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(item_id.clone(), run);
    LiveRunGuard { item_id }
}

/// Every live role run, ordered by item id.
#[must_use]
pub fn snapshot() -> Vec<LiveRun> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str) -> LiveRun {
        LiveRun {
            item_id: id.to_string(),
            role: "judge".to_string(),
            root: PathBuf::from("/r"),
            pid: 4242,
            started_at: Utc::now(),
            runtime: "claude".to_string(),
            claude_session_id: Some("s".to_string()),
            scope_unit: None,
            pause_root: PathBuf::from("/r/.loom/state/roll-pause"),
            model: None,
            timeout: Duration::from_secs(600),
            started_mono: Instant::now(),
            resume: None,
        }
    }

    #[test]
    fn a_run_is_listed_until_its_guard_drops() {
        let id = "role-judge-live-runs-test";
        let guard = register(run(id));
        assert!(snapshot().iter().any(|r| r.item_id == id));
        drop(guard);
        assert!(!snapshot().iter().any(|r| r.item_id == id));
    }

    #[test]
    fn the_remaining_timeout_never_underflows() {
        let mut r = run("x");
        r.timeout = Duration::from_secs(5);
        r.started_mono = Instant::now() - Duration::from_secs(60);
        assert_eq!(r.timeout_remaining_secs(Instant::now()), 0);
    }
}
