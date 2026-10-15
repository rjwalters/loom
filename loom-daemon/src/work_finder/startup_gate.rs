//! The work finder's bounded wait for the startup passes (Issue #12222).
//!
//! The work finder used to block its first tick until every startup pass had
//! finished (#6615/#7974). The startup claim-reconciliation pass makes
//! several forge calls per `loom:building` claim, serially over every root,
//! and under step dispatch every in-flight issue keeps `loom:building` while
//! its PR is open: hundreds of claims per host, 20–40+ minutes, on every host
//! on every roll. The whole fleet dispatched nothing for that long.
//!
//! [`wait`] now waits for the passes in two steps:
//!
//! 1. **Uncapped**, for [`StartupPasses::LocalDone`]: the local passes (the
//!    trace-join close, which closes every open entry and so must run before
//!    this process starts a session; profile provisioning). Seconds.
//! 2. **Capped**, for [`StartupPasses::Done`]: at most [`wait_cap`]
//!    ([`WAIT_ENV`], then [`WAIT_KEY`], then [`DEFAULT_WAIT_SECS`]). Past the
//!    cap the work finder ticks anyway and the pass carries on in the
//!    background.
//!
//! Ticking during the claim pass is safe because what the gate protected is
//! now held by the pass itself. The one startup-only decision,
//! `ReclaimReason::NoRecordAtStartup` (reclaim a zero-evidence claim at
//! once), would race a claim this process just made; it now applies only to
//! issues last updated before the pass began
//! (`claim_reconciliation::startup_cutoff`). Every other reclaim reason is the
//! periodic pass's, which always runs alongside the work finder, and the
//! lease, live-claim and closed-issue checks each reclaim still makes are
//! unchanged. The step path also verifies each item's lease before it spawns.
//!
//! Telemetry: the first completed tick publishes [`FIRST_TICK_TOPIC`] (a
//! `daemon.event` record in SigNoz) with the seconds since the startup passes
//! began and whether the cap fired.

use std::path::Path;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::daemon_startup_reconciliation::StartupPasses;

/// Env override for the cap, in seconds.
pub const WAIT_ENV: &str = "LOOM_STARTUP_RECONCILE_WAIT_SECS";
/// Dotted config key for the cap, in seconds (any config tier).
pub const WAIT_KEY: &str = "autonomous.startupReconcileWaitSecs";
/// Default cap: long enough for a quiet host's pass, short enough that a
/// roll costs the fleet a minute and a half of dispatch, not half an hour.
pub const DEFAULT_WAIT_SECS: u64 = 90;
/// The `daemon.event` topic of the first completed tick.
pub const FIRST_TICK_TOPIC: &str = "daemon.work_finder.first_tick";

/// The cap: env > config > [`DEFAULT_WAIT_SECS`]. An unparsable value at a
/// tier falls through to the next one.
#[must_use]
pub fn wait_cap(root: &Path) -> Duration {
    let env = std::env::var(WAIT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok());
    let secs = env.unwrap_or_else(|| {
        let effective = crate::config_resolver::resolve_effective_config(root);
        crate::config_resolver::get_path(&effective, WAIT_KEY)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_WAIT_SECS)
    });
    Duration::from_secs(secs)
}

/// How the wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every startup pass had finished.
    Ready,
    /// The cap expired with the forge-bound passes still running.
    CapExpired,
}

impl Outcome {
    /// Whether the cap fired.
    #[must_use]
    pub fn cap_fired(self) -> bool {
        self == Self::CapExpired
    }
}

/// Wait for the local passes (uncapped), then for the rest at most `cap`.
/// A closed channel (the passes' task is gone) counts as finished, as the
/// old `wait_for` did.
pub async fn wait(rx: &mut watch::Receiver<StartupPasses>, cap: Duration) -> Outcome {
    let _ = rx.wait_for(|p| *p >= StartupPasses::LocalDone).await;
    if tokio::time::timeout(cap, rx.wait_for(|p| *p == StartupPasses::Done))
        .await
        .is_ok()
    {
        return Outcome::Ready;
    }
    log::info!(
        "work_finder: startup reconciliation still running after {}s; ticking anyway — the \
         pass continues in the background and reclaims only claims older than this daemon \
         ({WAIT_ENV} / {WAIT_KEY}, #12222)",
        cap.as_secs()
    );
    Outcome::CapExpired
}

/// Reports the first completed tick once (#12222).
#[derive(Debug)]
pub struct FirstTick {
    since: Instant,
    outcome: Option<Outcome>,
}

impl FirstTick {
    /// `since` is when the startup passes began (about daemon start).
    #[must_use]
    pub fn new(since: Instant, outcome: Outcome) -> Self {
        Self {
            since,
            outcome: Some(outcome),
        }
    }

    /// Call at the end of every completed tick; logs and publishes only the
    /// first time.
    pub fn completed(&mut self, bus: &crate::event_bus::EventBus) {
        let Some(outcome) = self.outcome.take() else {
            return;
        };
        let secs = self.since.elapsed().as_secs_f64();
        log::info!(
            "work_finder: first tick completed {secs:.1}s after daemon start (startup \
             reconciliation cap fired: {}) (#12222)",
            outcome.cap_fired()
        );
        let _ = bus.publish_generic(
            FIRST_TICK_TOPIC,
            serde_json::json!({
                "secs_since_start": (secs * 10.0).round() / 10.0,
                "startup_reconcile_cap_fired": outcome.cap_fired(),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reconciliation pass that never signals ready: the wait ends at the
    /// cap, not later.
    #[tokio::test(start_paused = true)]
    async fn a_pass_that_never_finishes_releases_the_tick_at_the_cap() {
        let (tx, mut rx) = watch::channel(StartupPasses::LocalDone);
        let start = tokio::time::Instant::now();
        let outcome = wait(&mut rx, Duration::from_secs(90)).await;
        assert_eq!(outcome, Outcome::CapExpired);
        assert_eq!(start.elapsed(), Duration::from_secs(90));
        drop(tx);
    }

    /// A fast pass: the wait ends as soon as it is ready.
    #[tokio::test(start_paused = true)]
    async fn a_fast_pass_releases_the_tick_when_ready() {
        let (tx, mut rx) = watch::channel(StartupPasses::Running);
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            tx.send_replace(StartupPasses::LocalDone);
            tokio::time::sleep(Duration::from_secs(3)).await;
            tx.send_replace(StartupPasses::Done);
            tx
        });
        let start = tokio::time::Instant::now();
        let outcome = wait(&mut rx, Duration::from_secs(90)).await;
        assert_eq!(outcome, Outcome::Ready);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        drop(sender.await.unwrap());
    }

    /// The local passes are never capped: a tick waits for them however long.
    #[tokio::test(start_paused = true)]
    async fn the_local_passes_are_waited_for_uncapped() {
        let (tx, mut rx) = watch::channel(StartupPasses::Running);
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(300)).await;
            tx.send_replace(StartupPasses::LocalDone);
            tx
        });
        let start = tokio::time::Instant::now();
        let outcome = wait(&mut rx, Duration::from_secs(90)).await;
        assert_eq!(outcome, Outcome::CapExpired);
        assert_eq!(start.elapsed(), Duration::from_secs(390));
        drop(sender.await.unwrap());
    }

    /// Already done: no delay at all.
    #[tokio::test(start_paused = true)]
    async fn an_already_finished_pass_does_not_delay() {
        let (_tx, mut rx) = watch::channel(StartupPasses::Done);
        let start = tokio::time::Instant::now();
        assert_eq!(wait(&mut rx, Duration::from_secs(90)).await, Outcome::Ready);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// A dropped sender (the passes' task is gone) never wedges the tick.
    #[tokio::test(start_paused = true)]
    async fn a_closed_channel_counts_as_finished() {
        let (tx, mut rx) = watch::channel(StartupPasses::Running);
        drop(tx);
        assert_eq!(wait(&mut rx, Duration::from_secs(90)).await, Outcome::Ready);
    }

    #[test]
    #[serial_test::serial]
    fn the_cap_reads_env_then_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(WAIT_ENV, "17");
        assert_eq!(wait_cap(tmp.path()), Duration::from_secs(17));
        std::env::set_var(WAIT_ENV, "junk");
        assert_eq!(wait_cap(tmp.path()), Duration::from_secs(DEFAULT_WAIT_SECS));
        std::env::remove_var(WAIT_ENV);
        assert_eq!(wait_cap(tmp.path()), Duration::from_secs(DEFAULT_WAIT_SECS));
    }
}
