//! The daemon task that drives the `loom:blocked` release pass (#10763).
//!
//! # Why its own task
//!
//! The pass (#10556) used to ride the work finder's per-root listing. Its
//! ownership gate, though, is the **role-runner** shard
//! ([`crate::role_shard::decide`]), and a fleet does not run both loops on the
//! same hosts: the work finder is opt-in per host, and role rotation is
//! commonly confined to a subset of hosts. A workspace whose shard owner did
//! not run the work finder was released by nobody — every work-finder host
//! skipped it as not owned, and the owner never reached the hook. Nothing
//! logged it.
//!
//! This task runs on every daemon, independent of either loop, and serves a
//! workspace when **either** loop runs for it here ([`served`]): the shard
//! owner is a role-runner host by construction, and a work-finder host keeps
//! the pass it had. A host running neither stays out, as before.
//!
//! Every root, every tick, records exactly one
//! [`Outcome`](super::release_outcome::Outcome), so silence is diagnosable
//! from `host.health` without host logs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::release_gh::{self, Mode};
use super::release_outcome::{record, Outcome};
use crate::workspace_registry::{filter_missing_roots, WorkspaceRegistry};

/// How often the task visits every root. The pass's own cadence
/// (`LOOM_RELEASE_STALE_BLOCKED_INTERVAL_SECS`, default 300) still decides
/// when a root is due; this only bounds how late a due root is noticed.
pub const TICK: Duration = Duration::from_secs(60);

/// The gates the task itself answers, in order: `Some(outcome)` when one
/// refuses, before any cadence slot is spent or forge call made.
#[must_use]
pub fn pre_gate(mode: Mode, served: bool, rate_limited: bool) -> Option<Outcome> {
    if mode == Mode::Off {
        Some(Outcome::SkippedOff)
    } else if !served {
        Some(Outcome::NotServed)
    } else if rate_limited {
        Some(Outcome::RateLimited)
    } else {
        None
    }
}

/// Whether this host takes part in `root`'s automation: the work finder runs
/// here (a daemon-level switch, resolved from `fallback_root` exactly as
/// `daemon_service` does) or the role runner is enabled for `root`.
#[must_use]
pub fn served(fallback_root: &Path, root: &Path) -> bool {
    let wf = crate::work_finder::read_work_finder_config(fallback_root);
    crate::work_finder::resolve_enabled(&wf)
        || crate::role_runner::resolve_enabled(&crate::role_runner::read_role_runner_config(root))
}

/// One visit of every registered root (blocking: the pass shells out to `gh`).
pub fn tick_once(fallback_root: &Path, roots: &[PathBuf]) {
    let gh_bin = PathBuf::from(crate::gh_invocation::gh_bin());
    let mode = release_gh::mode();
    for root in roots {
        let rate_limited = crate::rate_limit_breaker::global_is_suppressed();
        match pre_gate(mode, served(fallback_root, root), rate_limited) {
            Some(outcome) => record(root, outcome, None),
            None => {
                let _ = release_gh::maybe_run(&gh_bin, root);
            }
        }
    }
}

/// Spawn the task. Held for the process lifetime by the caller.
pub fn spawn_task(fallback_root: PathBuf) -> tokio::task::JoinHandle<()> {
    log::info!(
        "stale_blocked_release: task started (visits every registered workspace each {}s; \
         per-workspace cadence, shard and write-scope gates apply, #10763)",
        TICK.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // no pass at boot
        let mut missing_warned: HashSet<PathBuf> = HashSet::new();
        loop {
            ticker.tick().await;
            let roots = WorkspaceRegistry::load_default()
                .unwrap_or_default()
                .effective_roots(&fallback_root);
            let roots = filter_missing_roots(roots, &mut missing_warned);
            let fallback = fallback_root.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || tick_once(&fallback, &roots)).await
            {
                log::warn!("stale_blocked_release: tick panicked ({e}); next tick retries");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_gate_order() {
        assert_eq!(pre_gate(Mode::Off, false, true), Some(Outcome::SkippedOff));
        assert_eq!(pre_gate(Mode::On, false, true), Some(Outcome::NotServed));
        assert_eq!(pre_gate(Mode::DryRun, true, true), Some(Outcome::RateLimited));
        assert_eq!(pre_gate(Mode::On, true, false), None);
        assert_eq!(pre_gate(Mode::DryRun, true, false), None);
    }
}
