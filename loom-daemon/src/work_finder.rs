//! Autonomous work-finder loop — forge-polling dispatch of `loom:issue` items
//! (Phase A of epic #3809).
//!
//! The daemon-native work finder is the **core missing brain**: the component
//! that turns a human-approved `loom:issue` into a dispatched build without an
//! operator. Before this loop the Rust `loom-daemon` had no forge poller — its
//! only sweep entry point was the explicit `DispatchSweep` IPC request. The
//! deleted v0.10.0 shepherd brain did this; this module restores it on the
//! daemon runtime.
//!
//! # Shape (mirrors [`crate::epic_supervisor`])
//!
//! Per tick, the finder:
//!
//! 1. Queries the forge for ready work — `gh issue list --label loom:issue
//!    --state open --json number,labels` via [`GhWorkSource`], the direct
//!    analogue of [`crate::epic_supervisor::forge::GhEpicSource`].
//! 2. Filters out issues that are **already in flight** (present in the
//!    [`SweepRegistry`](crate::sweep_registry::SweepRegistry) as a `Running` /
//!    `Pending` sweep) or that defensively carry any [`SKIP_LABELS`] entry
//!    (`loom:building` / `loom:blocked` / `loom:operator-only`).
//! 3. For each remaining issue, dispatches through the existing
//!    [`SweepRegistry::dispatch`](crate::sweep_registry::SweepRegistry::dispatch)
//!    path — up to a **fixed** max-concurrency cap. `dispatch()` already flips
//!    `loom:issue → loom:building`, acquires the per-issue `mkdir`-atomic claim
//!    lock, and spawns the rotated-token child.
//!
//! # Idempotency & fail-safe
//!
//! The finder never reimplements the claim/label/dedup machinery — it reuses
//! the three layers `dispatch()` already provides:
//!
//! - **Idempotency key** — each dispatch uses `workfinder-<issue>` so a running
//!   sweep with the same key short-circuits to a no-op (`was_new = false`).
//! - **Claim lock** — `dispatch()` acquires `.loom/locks/issue-<N>` atomically;
//!   a collision (e.g. a concurrent epic-supervisor sweep for the same child)
//!   fails loudly and is logged, never double-dispatched.
//! - **Registry dedup** — the authoritative "already in-flight" check is the
//!   registry itself: an issue with a live `Running`/`Pending` entry is skipped
//!   even if the forge still shows `loom:issue` (label-flip lag).
//!
//! A forge-query error aborts *that* tick only; the caller logs it and the next
//! tick proceeds normally. A single dispatch error is logged and counted, never
//! fatal — one wedged issue must not starve the rest, and nothing propagates a
//! panic out of the detached loop task.
//!
//! # Why a plain `tokio::spawn` (not a dedicated OS thread)
//!
//! Unlike [`crate::epic_supervisor`], whose concrete dispatcher is
//! spawn-and-wait (`Command::status()` blocks for the whole Architect/Champion
//! process lifetime, holding the #3707 mutex), every call into
//! [`SweepRegistry::dispatch`](crate::sweep_registry::SweepRegistry::dispatch)
//! returns quickly: it spawns the child via `Command::spawn` and returns the
//! handle immediately for the reaper to reap later. The finder holds no mutex
//! across a long call, so a plain `tokio::spawn` interval task on the shared
//! daemon runtime is sufficient and correct — matching the reaper task
//! ([`crate::sweep_registry::spawn_reaper_task`]) rather than the epic
//! supervisor's OS-thread machinery.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;

// ============================================================================
// Constants
// ============================================================================

/// Environment variable enabling the work-finder loop.
///
/// The finder is **opt-in** — unset or a false-y value keeps it OFF, so the
/// daemon's behavior is byte-for-byte unchanged when the variable is absent —
/// because the loop autonomously dispatches build sweeps (spawning
/// rotated-token children and flipping `loom:issue → loom:building`). Set to
/// `1` / `true` / `yes` / `on` (case-insensitive) to enable.
pub const WORK_FINDER_ENABLE_ENV: &str = "LOOM_WORK_FINDER";

/// Environment variable overriding the work-finder tick interval (seconds).
pub const WORK_FINDER_INTERVAL_ENV: &str = "LOOM_WORK_FINDER_INTERVAL_SECS";

/// Default work-finder tick interval. Much tighter than the epic supervisor's
/// 300s default — the `loom:issue` backlog should drain promptly — while still
/// keeping forge query volume low.
pub const DEFAULT_WORK_FINDER_INTERVAL_SECS: u64 = 60;

/// Environment variable overriding the fixed max-concurrency cap.
pub const WORK_FINDER_MAX_CONCURRENT_ENV: &str = "LOOM_WORK_FINDER_MAX_CONCURRENT";

/// Default fixed max-concurrency cap (MVP). Phase B replaces this with dynamic
/// scaling; for now the finder never lets the count of live sweeps it started
/// exceed this.
pub const DEFAULT_WORK_FINDER_MAX_CONCURRENT: usize = 3;

/// Labels that disqualify an issue from dispatch even if it still appears in
/// the `loom:issue`-filtered listing.
///
/// A `loom:issue` row should never itself carry these (they are mutually
/// exclusive states in the `.github/labels.yml` state machine), but `gh`'s
/// label cache can be briefly stale, so the finder checks defensively.
pub const SKIP_LABELS: &[&str] = &["loom:building", "loom:blocked", "loom:operator-only"];

// ============================================================================
// Fetched work facts
// ============================================================================

/// One ready-work candidate fetched from the forge: its issue number and the
/// labels it currently carries (for defensive [`SKIP_LABELS`] filtering).
///
/// Keeping this a plain data struct (no forge I/O) makes [`tick`] a pure
/// function of already-fetched data, mirroring the [`crate::epic_supervisor`]
/// design. A [`WorkSource`] materializes these from the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    /// The issue number.
    pub number: u32,
    /// The labels currently on the issue.
    pub labels: Vec<String>,
}

impl WorkItem {
    /// Convenience constructor.
    #[must_use]
    pub fn new(number: u32, labels: Vec<String>) -> Self {
        Self { number, labels }
    }

    /// True when the issue carries any [`SKIP_LABELS`] entry.
    #[must_use]
    pub fn is_skipped(&self) -> bool {
        self.labels
            .iter()
            .any(|l| SKIP_LABELS.contains(&l.as_str()))
    }
}

// ============================================================================
// Source + dispatcher traits
// ============================================================================

/// Fetches the ready-to-build `loom:issue` items the finder iterates each tick.
///
/// Abstracting the forge read behind a trait keeps [`tick`] testable with a
/// fake source and lets the concrete `gh` query evolve independently — exactly
/// as [`crate::epic_supervisor::EpicSource`] does.
pub trait WorkSource {
    /// Return one [`WorkItem`] per open `loom:issue`.
    ///
    /// # Errors
    ///
    /// Returns an error when the forge query fails. The caller logs it and
    /// retries on the next tick — the error is never fatal.
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>>;
}

/// Performs the actual sweep dispatches the finder schedules and reports which
/// issues are already in flight.
///
/// The finder owns *when* and *whether* (scheduling + the concurrency cap); the
/// dispatcher owns *how* (the registry `dispatch()` call and the in-flight
/// query). Splitting it out keeps [`tick`] unit-testable without a real
/// registry or `gh` credentials.
pub trait WorkDispatcher {
    /// The set of issue numbers that currently have a live (`Running` /
    /// `Pending`) sweep — the authoritative "already in-flight" view.
    fn in_flight(&self) -> HashSet<u32>;

    /// Dispatch a build sweep for `issue`. Returns `true` when a **new** sweep
    /// was started, `false` when the dispatch was an idempotency no-op (a sweep
    /// with the same key was already running).
    ///
    /// # Errors
    ///
    /// Returns an error when the dispatch fails (e.g. a claim-lock collision).
    /// The caller logs and counts it; it is never fatal.
    fn dispatch(&mut self, issue: u32) -> Result<bool>;
}

// ============================================================================
// Tick
// ============================================================================

/// Per-tick outcome counts, for observability and test assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Ready `loom:issue` rows returned by the source this tick.
    pub seen: usize,
    /// Issues for which a **new** sweep was dispatched this tick.
    pub dispatched: usize,
    /// Issues skipped because they carried a [`SKIP_LABELS`] entry.
    pub skipped_labeled: usize,
    /// Issues skipped because a live sweep already exists for them (registry
    /// in-flight set, or an idempotency no-op from `dispatch()`).
    pub skipped_in_flight: usize,
    /// Issues deferred to a future tick because the concurrency cap was reached.
    pub deferred_capacity: usize,
    /// Dispatch attempts that returned an error (logged, non-fatal).
    pub errors: usize,
}

/// Run one work-finder tick: fetch ready issues, filter, and dispatch up to the
/// fixed concurrency cap.
///
/// The count of live sweeps at tick start (`dispatcher.in_flight().len()`) is
/// treated as the current occupancy; the finder dispatches only while
/// `occupancy < max_concurrent`, incrementing occupancy per new dispatch so a
/// single tick never overshoots the cap.
///
/// # Errors
///
/// Propagates a source (`list_ready_issues`) error so the caller can log it and
/// retry next tick. Individual dispatch errors are logged and counted in
/// [`TickReport::errors`] rather than aborting the tick.
pub fn tick(
    source: &mut impl WorkSource,
    dispatcher: &mut impl WorkDispatcher,
    max_concurrent: usize,
) -> Result<TickReport> {
    let ready = source.list_ready_issues()?;
    let mut report = TickReport {
        seen: ready.len(),
        ..TickReport::default()
    };

    let in_flight = dispatcher.in_flight();
    let mut occupancy = in_flight.len();

    for item in ready {
        // 1. Defensive skip-label filter (stale forge cache).
        if item.is_skipped() {
            report.skipped_labeled += 1;
            continue;
        }
        // 2. Authoritative in-flight dedup against the registry.
        if in_flight.contains(&item.number) {
            report.skipped_in_flight += 1;
            continue;
        }
        // 3. Fixed concurrency cap — defer the rest to a future tick.
        if occupancy >= max_concurrent {
            report.deferred_capacity += 1;
            continue;
        }
        // 4. Dispatch. The registry's idempotency key + claim lock make a
        //    double-dispatch of an already-running issue a no-op / loud error.
        match dispatcher.dispatch(item.number) {
            Ok(true) => {
                report.dispatched += 1;
                occupancy += 1;
            }
            Ok(false) => {
                // Idempotency no-op: a sweep with the same key was already
                // running (label-flip lag). Count as in-flight, not a new
                // dispatch, and do not consume a capacity slot.
                report.skipped_in_flight += 1;
            }
            Err(e) => {
                report.errors += 1;
                log::warn!("work_finder: dispatch for issue #{} failed: {e}", item.number);
            }
        }
    }

    Ok(report)
}

// ============================================================================
// Env-var configuration helpers
// ============================================================================

/// Whether the work-finder loop is enabled, per [`WORK_FINDER_ENABLE_ENV`].
///
/// Off by default (opt-in) — parsing mirrors
/// [`crate::epic_supervisor::supervisor_enabled`].
#[must_use]
pub fn enabled() -> bool {
    std::env::var(WORK_FINDER_ENABLE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Resolve the tick interval from [`WORK_FINDER_INTERVAL_ENV`], falling back to
/// [`DEFAULT_WORK_FINDER_INTERVAL_SECS`]. A zero or unparseable value falls back
/// to the default (a zero-interval busy loop is never useful).
#[must_use]
pub fn resolve_interval() -> Duration {
    std::env::var(WORK_FINDER_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map_or_else(|| Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS), Duration::from_secs)
}

/// Resolve the fixed max-concurrency cap from
/// [`WORK_FINDER_MAX_CONCURRENT_ENV`], falling back to
/// [`DEFAULT_WORK_FINDER_MAX_CONCURRENT`]. A zero or unparseable value falls
/// back to the default (a zero cap would dispatch nothing, defeating the loop).
#[must_use]
pub fn resolve_max_concurrent() -> usize {
    std::env::var(WORK_FINDER_MAX_CONCURRENT_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_WORK_FINDER_MAX_CONCURRENT)
}

// ============================================================================
// Runtime wiring — the loop runs on the shared daemon runtime
// ============================================================================

/// Spawn the work-finder loop on the shared daemon runtime and return its task
/// handle so the daemon can keep it alive for the process lifetime.
///
/// Every `interval`, the task runs one [`tick`]. Unlike the epic supervisor,
/// no dedicated OS thread is needed: [`SweepRegistry::dispatch`] returns
/// promptly (fire-and-forget child spawn), so the finder never parks a runtime
/// worker in a minutes-long blocking call — the same footing as the reaper
/// task ([`crate::sweep_registry::spawn_reaper_task`]).
pub fn spawn_work_finder_task<S, D>(
    mut source: S,
    mut dispatcher: D,
    interval: Duration,
    max_concurrent: usize,
) -> tokio::task::JoinHandle<()>
where
    S: WorkSource + Send + 'static,
    D: WorkDispatcher + Send + 'static,
{
    log::info!(
        "work_finder: starting loop (interval={}s, max_concurrent={max_concurrent})",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so we don't churn at boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match tick(&mut source, &mut dispatcher, max_concurrent) {
                Ok(report) => {
                    if report.dispatched > 0 || report.errors > 0 {
                        log::info!(
                            "work_finder: tick — {} seen, {} dispatched, {} labeled-skip, \
                             {} in-flight-skip, {} deferred, {} error(s)",
                            report.seen,
                            report.dispatched,
                            report.skipped_labeled,
                            report.skipped_in_flight,
                            report.deferred_capacity,
                            report.errors
                        );
                    }
                }
                Err(e) => {
                    log::warn!("work_finder: tick failed to list ready issues: {e}");
                }
            }
        }
    })
}

// ============================================================================
// Concrete runtime adapters (forge-backed source + registry dispatcher)
// ============================================================================

/// Concrete [`WorkSource`] / [`WorkDispatcher`] implementations that wire the
/// finder to the live forge (`gh`) and the daemon's [`SweepRegistry`].
///
/// The pure [`tick`] logic above is exercised in tests via mocks; these
/// adapters are the runtime glue and shell out to `gh` / spawn children, so
/// they are not unit-tested directly (mirroring
/// [`crate::epic_supervisor::forge`]).
pub mod forge {
    use super::{WorkDispatcher, WorkItem, WorkSource};
    use crate::sweep_registry::SweepRegistry;
    use crate::types::{SweepKind, SweepState};
    use anyhow::{anyhow, Context, Result};
    use serde::Deserialize;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    /// Minimal `gh issue list --json number,labels` row.
    #[derive(Debug, Deserialize)]
    struct GhIssue {
        number: u32,
        #[serde(default)]
        labels: Vec<GhLabel>,
    }

    #[derive(Debug, Deserialize)]
    struct GhLabel {
        name: String,
    }

    /// A forge-backed [`WorkSource`] that lists open `loom:issue` items via
    /// `gh`. Mirrors [`crate::epic_supervisor::forge::GhEpicSource`].
    pub struct GhWorkSource {
        gh_bin: PathBuf,
        repo: Option<String>,
    }

    impl GhWorkSource {
        /// Construct a source using `gh` from `PATH`, honoring `LOOM_REPO` for
        /// the `--repo` flag when set.
        #[must_use]
        pub fn new() -> Self {
            Self {
                gh_bin: PathBuf::from("gh"),
                repo: std::env::var("LOOM_REPO").ok(),
            }
        }

        /// Override the `gh` binary path (for tests / non-standard installs).
        #[must_use]
        pub fn with_gh_bin(mut self, bin: PathBuf) -> Self {
            self.gh_bin = bin;
            self
        }
    }

    impl Default for GhWorkSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl WorkSource for GhWorkSource {
        fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
            let mut cmd = Command::new(&self.gh_bin);
            cmd.arg("issue")
                .arg("list")
                .arg("--label")
                .arg("loom:issue")
                .arg("--state")
                .arg("open")
                .arg("--limit")
                .arg("200")
                .arg("--json")
                .arg("number,labels");
            if let Some(ref repo) = self.repo {
                cmd.arg("--repo").arg(repo);
            }
            cmd.stderr(Stdio::piped());
            let out = cmd
                .output()
                .with_context(|| format!("failed to invoke {}", self.gh_bin.display()))?;
            if !out.status.success() {
                return Err(anyhow!(
                    "gh issue list --label loom:issue failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let rows: Vec<GhIssue> =
                serde_json::from_slice(&out.stdout).context("parse gh issue list JSON")?;
            Ok(rows
                .into_iter()
                .map(|r| WorkItem::new(r.number, r.labels.into_iter().map(|l| l.name).collect()))
                .collect())
        }
    }

    /// A concrete [`WorkDispatcher`] backed by the daemon [`SweepRegistry`].
    ///
    /// `dispatch()` calls the registry's own `dispatch()` — reusing its
    /// idempotency key, `mkdir`-atomic claim lock, and `loom:issue →
    /// loom:building` label flip — so the finder never reimplements the race
    /// guard. `in_flight()` reads the registry's `Running` / `Pending` entries.
    pub struct RegistryDispatcher {
        registry: Arc<Mutex<SweepRegistry>>,
    }

    impl RegistryDispatcher {
        /// Construct a dispatcher over the shared registry.
        #[must_use]
        pub fn new(registry: Arc<Mutex<SweepRegistry>>) -> Self {
            Self { registry }
        }
    }

    impl WorkDispatcher for RegistryDispatcher {
        fn in_flight(&self) -> HashSet<u32> {
            let reg = match self.registry.lock() {
                Ok(r) => r,
                Err(poisoned) => {
                    log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                    return HashSet::new();
                }
            };
            let mut set = HashSet::new();
            for state in [SweepState::Running, SweepState::Pending] {
                for info in reg.list(Some(&state)) {
                    if let SweepKind::Issue(n) = info.kind {
                        set.insert(n);
                    }
                }
            }
            set
        }

        fn dispatch(&mut self, issue: u32) -> Result<bool> {
            let mut reg = self
                .registry
                .lock()
                .map_err(|e| anyhow!("sweep registry mutex poisoned: {e}"))?;
            // Idempotency key + the registry's claim lock make a re-dispatch of
            // an already-running issue a no-op (`was_new = false`) or a loud
            // lock-collision error.
            let key = format!("workfinder-{issue}");
            let outcome = reg.dispatch(&SweepKind::Issue(issue), Some(key), None, None, None)?;
            Ok(outcome.was_new)
        }
    }
}

// Re-export the concrete adapters at the module root for ergonomic wiring.
pub use forge::{GhWorkSource, RegistryDispatcher};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ===================================================================
    // Mock source + dispatcher
    // ===================================================================

    /// A fake [`WorkSource`] returning a scripted sequence of results, one per
    /// `tick`. Each entry is either an `Ok(items)` or a forge `Err`.
    struct FakeSource {
        results: std::collections::VecDeque<Result<Vec<WorkItem>>>,
    }

    impl FakeSource {
        fn once(items: Vec<WorkItem>) -> Self {
            let mut results = std::collections::VecDeque::new();
            results.push_back(Ok(items));
            Self { results }
        }
    }

    impl WorkSource for FakeSource {
        fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
            self.results.pop_front().unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    /// A recording [`WorkDispatcher`] with a configurable in-flight set.
    #[derive(Default)]
    struct RecordingDispatcher {
        dispatched: Vec<u32>,
        in_flight: HashSet<u32>,
        /// Issue numbers whose dispatch should report an idempotency no-op.
        noop_issues: HashSet<u32>,
        /// Issue numbers whose dispatch should error.
        fail_issues: HashSet<u32>,
    }

    impl WorkDispatcher for RecordingDispatcher {
        fn in_flight(&self) -> HashSet<u32> {
            self.in_flight.clone()
        }
        fn dispatch(&mut self, issue: u32) -> Result<bool> {
            if self.fail_issues.contains(&issue) {
                anyhow::bail!("forced dispatch failure for #{issue}");
            }
            self.dispatched.push(issue);
            Ok(!self.noop_issues.contains(&issue))
        }
    }

    fn issue(n: u32) -> WorkItem {
        WorkItem::new(n, vec!["loom:issue".to_string()])
    }

    // ===================================================================
    // tick — dispatch scheduling
    // ===================================================================

    #[test]
    fn test_tick_dispatches_up_to_cap() {
        // N=5 ready issues, cap K=2 → exactly 2 dispatched this tick, 3 deferred.
        let mut source = FakeSource::once((1..=5).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 2).unwrap();

        assert_eq!(report.seen, 5);
        assert_eq!(report.dispatched, 2);
        assert_eq!(report.deferred_capacity, 3);
        assert_eq!(report.errors, 0);
        assert_eq!(disp.dispatched, vec![1, 2]);
    }

    #[test]
    fn test_tick_all_dispatched_when_under_cap() {
        let mut source = FakeSource::once((1..=3).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 10).unwrap();

        assert_eq!(report.dispatched, 3);
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(disp.dispatched, vec![1, 2, 3]);
    }

    #[test]
    fn test_tick_existing_occupancy_counts_against_cap() {
        // 2 already in flight, cap 3 ⇒ only 1 slot free even though 4 ready.
        let mut source = FakeSource::once(vec![issue(10), issue(11), issue(12), issue(13)]);
        let mut disp = RecordingDispatcher {
            in_flight: HashSet::from([100, 101]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 3).unwrap();

        assert_eq!(report.dispatched, 1);
        assert_eq!(report.deferred_capacity, 3);
        assert_eq!(disp.dispatched, vec![10]);
    }

    #[test]
    fn test_tick_skips_issue_already_in_registry() {
        // #7 is already in flight in the registry even though the source still
        // reports it as loom:issue (label-flip lag) — it must be skipped.
        let mut source = FakeSource::once(vec![issue(7), issue(8)]);
        let mut disp = RecordingDispatcher {
            in_flight: HashSet::from([7]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10).unwrap();

        assert_eq!(report.skipped_in_flight, 1);
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![8]);
    }

    #[test]
    fn test_tick_skips_skip_labeled_issues() {
        // Each SKIP_LABELS entry disqualifies a row even in the loom:issue list.
        let mut source = FakeSource::once(vec![
            WorkItem::new(1, vec!["loom:issue".into(), "loom:building".into()]),
            WorkItem::new(2, vec!["loom:issue".into(), "loom:blocked".into()]),
            WorkItem::new(3, vec!["loom:issue".into(), "loom:operator-only".into()]),
            issue(4),
        ]);
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 10).unwrap();

        assert_eq!(report.skipped_labeled, 3);
        assert_eq!(report.dispatched, 1);
        assert_eq!(disp.dispatched, vec![4]);
    }

    #[test]
    fn test_tick_idempotency_noop_not_counted_as_dispatch() {
        // dispatch() returns Ok(false) (a sweep with the same key was already
        // running) — it must not count as a new dispatch nor consume a slot.
        let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
        let mut disp = RecordingDispatcher {
            noop_issues: HashSet::from([1]),
            ..Default::default()
        };
        // Cap of 2: #1 is a no-op (frees its slot), so #2 AND #3 still dispatch.
        let report = tick(&mut source, &mut disp, 2).unwrap();

        assert_eq!(report.dispatched, 2, "only #2 and #3 are new dispatches");
        assert_eq!(report.skipped_in_flight, 1, "#1 was an idempotency no-op");
        assert_eq!(report.deferred_capacity, 0);
        assert_eq!(disp.dispatched, vec![1, 2, 3]);
    }

    #[test]
    fn test_tick_dispatch_error_is_non_fatal() {
        // #2 errors; the tick still dispatches #1 and #3 and reports 1 error.
        let mut source = FakeSource::once(vec![issue(1), issue(2), issue(3)]);
        let mut disp = RecordingDispatcher {
            fail_issues: HashSet::from([2]),
            ..Default::default()
        };
        let report = tick(&mut source, &mut disp, 10).unwrap();

        assert_eq!(report.dispatched, 2);
        assert_eq!(report.errors, 1);
        assert_eq!(disp.dispatched, vec![1, 3]);
    }

    #[test]
    fn test_tick_source_error_propagates_then_next_tick_succeeds() {
        // First tick's source errors; tick() returns Err (the loop logs it,
        // non-fatal). The second tick succeeds and dispatches normally.
        let mut results = std::collections::VecDeque::new();
        results.push_back(Err(anyhow::anyhow!("gh unavailable")));
        results.push_back(Ok(vec![issue(1), issue(2)]));
        let mut source = FakeSource { results };
        let mut disp = RecordingDispatcher::default();

        let first = tick(&mut source, &mut disp, 10);
        assert!(first.is_err(), "source error propagates out of the tick");
        assert_eq!(disp.dispatched.len(), 0, "no dispatch on the erroring tick");

        let second = tick(&mut source, &mut disp, 10).unwrap();
        assert_eq!(second.dispatched, 2, "the next tick proceeds normally");
        assert_eq!(disp.dispatched, vec![1, 2]);
    }

    #[test]
    fn test_tick_empty_ready_is_noop() {
        let mut source = FakeSource::once(vec![]);
        let mut disp = RecordingDispatcher::default();
        let report = tick(&mut source, &mut disp, 10).unwrap();
        assert_eq!(report, TickReport::default());
        assert!(disp.dispatched.is_empty());
    }

    // ===================================================================
    // WorkItem
    // ===================================================================

    #[test]
    fn test_work_item_is_skipped() {
        assert!(!issue(1).is_skipped());
        assert!(WorkItem::new(1, vec!["loom:building".into()]).is_skipped());
        assert!(WorkItem::new(1, vec!["loom:blocked".into()]).is_skipped());
        assert!(WorkItem::new(1, vec!["loom:operator-only".into()]).is_skipped());
        assert!(!WorkItem::new(1, vec!["loom:curated".into()]).is_skipped());
    }

    // ===================================================================
    // Env-var configuration
    // ===================================================================

    #[test]
    #[serial]
    fn test_enabled_off_by_default() {
        std::env::remove_var(WORK_FINDER_ENABLE_ENV);
        assert!(!enabled(), "unset ⇒ disabled (zero behavior change)");
    }

    #[test]
    #[serial]
    fn test_enabled_truthy_values() {
        for v in ["1", "true", "yes", "on", "TRUE", "On", " Yes "] {
            std::env::set_var(WORK_FINDER_ENABLE_ENV, v);
            assert!(enabled(), "{v:?} should enable");
        }
        std::env::remove_var(WORK_FINDER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_enabled_falsy_values() {
        for v in ["0", "false", "no", "off", "", "maybe"] {
            std::env::set_var(WORK_FINDER_ENABLE_ENV, v);
            assert!(!enabled(), "{v:?} should not enable");
        }
        std::env::remove_var(WORK_FINDER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_default_and_override() {
        std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));

        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "120");
        assert_eq!(resolve_interval(), Duration::from_secs(120));

        // Zero and unparseable fall back to the default.
        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "0");
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));
        std::env::set_var(WORK_FINDER_INTERVAL_ENV, "garbage");
        assert_eq!(resolve_interval(), Duration::from_secs(DEFAULT_WORK_FINDER_INTERVAL_SECS));
        std::env::remove_var(WORK_FINDER_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_max_concurrent_default_and_override() {
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
        assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);

        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "7");
        assert_eq!(resolve_max_concurrent(), 7);

        // Zero and unparseable fall back to the default.
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "0");
        assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
        std::env::set_var(WORK_FINDER_MAX_CONCURRENT_ENV, "nope");
        assert_eq!(resolve_max_concurrent(), DEFAULT_WORK_FINDER_MAX_CONCURRENT);
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
    }
}
