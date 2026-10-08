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
//!
//! # The launch gate (Judge finding 3 on #10974)
//!
//! A role tick checks the drain flag when it starts, then prepares for a
//! while before it spawns. A roll requested in between would snapshot before
//! the run is registered and miss it. So a launch takes a [`LaunchPermit`]
//! immediately before its spawn and registers through it: the permit holds
//! the gate's lock across "spawn, then register", and is refused once H4 has
//! closed the gate for the run's root ([`close_launches`], which takes the
//! same lock). When `close_launches` returns, no launch for those roots is in
//! progress and every run that did start is in [`snapshot`]. Like the sweep
//! gate, a root is closed *by* a pause run and reopens when no run holds it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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
    /// The pinned Claude session id, when the runtime is Claude.
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

/// Closed roots, each with the pause runs holding it closed.
type ClosedRoots = BTreeMap<PathBuf, BTreeSet<String>>;

/// The key that closes the gate for **every** root. The daemon's pause closes
/// this, not a list of roots, so a role launch is refused however its
/// workspace path happens to be spelled.
#[must_use]
pub fn all_roots() -> PathBuf {
    PathBuf::new()
}

fn closed_roots() -> &'static Mutex<ClosedRoots> {
    static CLOSED: OnceLock<Mutex<ClosedRoots>> = OnceLock::new();
    CLOSED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Permission to spawn one role run. Holds the launch gate's lock: take it
/// immediately before the spawn and end it with [`LaunchPermit::register`]
/// (or drop it when the spawn failed).
#[derive(Debug)]
pub struct LaunchPermit {
    _gate: std::sync::MutexGuard<'static, ClosedRoots>,
}

impl LaunchPermit {
    /// Register the spawned run, then release the gate.
    #[must_use]
    pub fn register(self, run: LiveRun) -> LiveRunGuard {
        register(run)
    }
}

/// A permit to launch a role run in `root`, or `None` while a roll's pause
/// has closed the gate for it.
#[must_use]
pub fn begin_launch(root: &Path) -> Option<LaunchPermit> {
    let closed = closed_roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let refused = closed.contains_key(root) || closed.contains_key(&all_roots());
    (!refused).then(|| LaunchPermit { _gate: closed })
}

/// Close the launch gate for `roots` on behalf of pause run `owner`, or take
/// `owner`'s hold off them. Waits for any launch that is between its spawn
/// and its registration.
pub fn close_launches(roots: &[PathBuf], owner: &str, closed: bool) {
    let mut map = closed_roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for root in roots {
        if closed {
            map.entry(root.clone())
                .or_default()
                .insert(owner.to_string());
        } else if let Some(owners) = map.get_mut(root) {
            owners.remove(owner);
            if owners.is_empty() {
                map.remove(root);
            }
        }
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

    /// #10974: a closed gate refuses a launch for its roots only, and a
    /// launch that got its permit is registered before the gate can close.
    #[test]
    fn a_closed_gate_refuses_launches_for_its_roots_until_reopened() {
        let root = PathBuf::from("/r/launch-gate-test");
        let other = PathBuf::from("/r/launch-gate-test-other");
        let id = "role-judge-launch-gate-test";
        let permit = begin_launch(&root).expect("the gate starts open");
        let (tx, rx) = std::sync::mpsc::channel();
        let closer = {
            let root = root.clone();
            std::thread::spawn(move || {
                close_launches(std::slice::from_ref(&root), "rp-old", true);
                // What H4 reads next: the run that held the permit is listed.
                let _ = tx.send(snapshot().iter().any(|r| r.item_id == id));
            })
        };
        // The close waits for the launch in progress.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        let guard = permit.register(LiveRun {
            root: root.clone(),
            ..run(id)
        });
        assert!(rx.recv_timeout(Duration::from_secs(20)).unwrap(), "registered before the close");
        closer.join().unwrap();

        assert!(begin_launch(&root).is_none(), "closed for the paused root");
        assert!(begin_launch(&other).is_some(), "another root is untouched");
        // A replacement run closes it too; the old run's late stand-down
        // does not reopen it.
        close_launches(std::slice::from_ref(&root), "rp-new", true);
        close_launches(std::slice::from_ref(&root), "rp-old", false);
        assert!(begin_launch(&root).is_none(), "still closed by the replacement");
        close_launches(std::slice::from_ref(&root), "rp-new", false);
        assert!(begin_launch(&root).is_some(), "reopened once no run holds it");
        drop(guard);
    }

    #[test]
    fn the_remaining_timeout_never_underflows() {
        let mut r = run("x");
        r.timeout = Duration::from_secs(5);
        r.started_mono = Instant::now() - Duration::from_secs(60);
        assert_eq!(r.timeout_remaining_secs(Instant::now()), 0);
    }
}
