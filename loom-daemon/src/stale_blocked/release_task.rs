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
//! "Runs here" means the daemon actually started the loop. Both loops are
//! admitted at the **fallback/home** workspace (`daemon_service`), and the
//! role runner then also checks each root — so a root's own
//! `roleRunner.enabled` counts only when the home workspace started the loop
//! ([`Loops`], decided by `daemon_service` and passed in).
//!
//! Every root, every tick, records exactly one
//! [`Outcome`](super::release_outcome::Outcome), so silence is diagnosable
//! from `host.health` without host logs.

use std::collections::HashSet;
use std::path::PathBuf;
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

/// Which daemon-level loops `daemon_service` actually started, decided once at
/// startup and handed to [`spawn_task`] (not re-read from config, so the task
/// can never claim a loop runs that was not spawned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loops {
    pub work_finder: bool,
    pub role_runner: bool,
}

impl From<(bool, bool)> for Loops {
    /// `(work_finder, role_runner)`, in that order.
    fn from((work_finder, role_runner): (bool, bool)) -> Self {
        Self {
            work_finder,
            role_runner,
        }
    }
}

/// Whether this host takes part in a root's automation: the work finder was
/// started here, or the role-runner loops were started here **and** the root
/// itself has the role runner enabled (the per-root check the loops apply).
#[must_use]
pub fn served(loops: Loops, root_role_runner_enabled: bool) -> bool {
    loops.work_finder || (loops.role_runner && root_role_runner_enabled)
}

/// One visit of every registered root (blocking: the pass shells out to `gh`).
pub fn tick_once(loops: Loops, roots: &[PathBuf]) {
    let gh_bin = PathBuf::from(crate::gh_invocation::gh_bin());
    let mode = release_gh::mode();
    for root in roots {
        let rate_limited = crate::rate_limit_breaker::global_is_suppressed();
        let root_rr =
            crate::role_runner::resolve_enabled(&crate::role_runner::read_role_runner_config(root));
        match pre_gate(mode, served(loops, root_rr), rate_limited) {
            Some(outcome) => record(root, outcome, None),
            None => {
                let _ = release_gh::maybe_run(&gh_bin, root);
            }
        }
    }
}

/// Spawn the task. Held for the process lifetime by the caller. `loops` is
/// what the daemon started; `fallback_root` only resolves the registry.
pub fn spawn_task(fallback_root: PathBuf, loops: Loops) -> tokio::task::JoinHandle<()> {
    log::info!(
        "stale_blocked_release: task started (visits every registered workspace each {}s; \
         work finder {}, role runner {}; per-workspace cadence, shard and write-scope gates \
         apply, #10763)",
        TICK.as_secs(),
        if loops.work_finder { "on" } else { "off" },
        if loops.role_runner { "on" } else { "off" },
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
            if let Err(e) = tokio::task::spawn_blocking(move || tick_once(loops, &roots)).await {
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

    const HOME_OFF: Loops = Loops {
        work_finder: false,
        role_runner: false,
    };
    const HOME_RR: Loops = Loops {
        work_finder: false,
        role_runner: true,
    };
    const HOME_WF: Loops = Loops {
        work_finder: true,
        role_runner: false,
    };

    #[test]
    fn home_off_repo_on_is_not_served() {
        assert!(!served(HOME_OFF, true));
    }

    #[test]
    fn home_on_repo_on_is_served() {
        assert!(served(HOME_RR, true));
    }

    #[test]
    fn home_on_repo_off_without_work_finder_is_not_served() {
        assert!(!served(HOME_RR, false));
    }

    #[test]
    fn work_finder_only_is_served() {
        assert!(served(HOME_WF, false));
        assert!(served(HOME_WF, true));
    }

    #[test]
    fn loops_from_tuple_is_work_finder_then_role_runner() {
        assert_eq!(Loops::from((true, false)), HOME_WF);
        assert_eq!(Loops::from((false, true)), HOME_RR);
    }
}
