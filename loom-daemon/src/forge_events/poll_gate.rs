//! Event-gated per-workspace polling (ADR-0021 amendment, step 2 — issue
//! #9255).
//!
//! # What this is
//!
//! While the feed status is `healthy`, a workspace whose repo has had **no
//! feed event** since its last successful poll skips its discovery poll. It is
//! re-polled when an event names its repo, or when its **hard maximum
//! staleness** expires (default [`MAX_STALENESS_MULTIPLIER`] x the loop's base
//! cadence, never more than [`MAX_STALENESS_CEILING_SECS`]). Any non-`healthy`
//! status restores the base cadence on the very next read, with no grace
//! period.
//!
//! Opt-in: `forgeEvents.pollGating` (env [`POLL_GATING_ENV`]), default OFF.
//! Off, [`gated_list`] is a bare call of the caller's closure: no lock, no
//! repo resolution, no allocation, so a default host is byte-identical.
//!
//! # Invariants (ADR-0014 / ADR-0021 amendment)
//!
//! 1. **An event is a prompt, not a truth.** [`PollGate::note_event`] records
//!    only *that* a repo changed (an invalidation key), never what changed.
//!    Nothing in an event is carried into a decision.
//! 2. **Polling is the correctness floor.** Skipping is bounded by the hard
//!    cap; a silently lossy feed costs at most one cap interval of latency.
//!    The cost is measured, not assumed: a hard-cap re-poll that found a change
//!    the feed never reported is counted as a **lossy** re-poll and shown on
//!    `loom-daemon status`.
//! 3. **Reads that gate a decision are never held.** Only
//!    [`ReadKind::Discovery`] can be skipped; [`ReadKind::Claim`],
//!    [`ReadKind::LabelTransition`] and [`ReadKind::Merge`] always poll
//!    ([`PollGate::decide`]). The only call site, `GhWorkSource`, lists
//!    *candidates*; the claim itself (`dispatch`) re-reads the forge.
//! 4. **Unattributable events invalidate everyone.** An event with no usable
//!    `repo`, or a page the feed clamped, bumps every workspace: over-reporting
//!    costs calls, under-reporting costs latency, and only the first is safe
//!    to do on a guess.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::types::{ForgeEventsState, PollGatingStatus, PollGatingWorkspace};

use super::keys::repo_is_path_safe;

/// `forgeEvents.pollGating` env override.
pub const POLL_GATING_ENV: &str = "LOOM_FORGE_EVENTS_POLL_GATING";

/// Default hard cap, as a multiple of the loop's base cadence.
pub const MAX_STALENESS_MULTIPLIER: u64 = 10;

/// Absolute ceiling on the hard cap (ADR-0021 amendment: "never more than 15
/// minutes").
pub const MAX_STALENESS_CEILING_SECS: u64 = 15 * 60;

/// **env > config > default** (`false`).
#[must_use]
pub fn resolve_poll_gating(config: &super::ForgeEventsConfig) -> bool {
    std::env::var(POLL_GATING_ENV)
        .ok()
        .map(|value| {
            matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
        })
        .or(config.poll_gating)
        .unwrap_or(false)
}

/// The hard maximum staleness for a loop of `base` cadence: `10 x base`,
/// clamped to [`MAX_STALENESS_CEILING_SECS`] but never below `base` itself
/// (a loop already slower than the ceiling is simply never stretched).
#[must_use]
pub fn hard_cap(base: Duration) -> Duration {
    let stretched = base.saturating_mul(u32::try_from(MAX_STALENESS_MULTIPLIER).unwrap_or(10));
    stretched
        .min(Duration::from_secs(MAX_STALENESS_CEILING_SECS))
        .max(base)
}

/// What a read is *for*. Only discovery may be skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadKind {
    /// "Is there anything for me to do?" — a candidate listing.
    Discovery,
    /// A read that gates a claim.
    Claim,
    /// A read that gates a label transition.
    LabelTransition,
    /// A read that gates a merge.
    Merge,
}

/// Why a poll happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollReason {
    /// Gating is off, or the workspace has no resolvable repo.
    Ungated,
    /// Feed is not `healthy`: base cadence, no grace.
    FeedNotHealthy,
    /// The read gates a decision; never held.
    DecisionRead,
    /// No successful poll yet.
    FirstPoll,
    /// A feed event named this repo (or was unattributable).
    Event,
    /// The hard maximum staleness expired.
    HardCap,
}

/// The gate's verdict for one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Poll(PollReason),
    Skip,
}

#[derive(Debug)]
struct Ws<T> {
    repo: String,
    last_poll: Option<Instant>,
    /// Event clock value observed when the last poll *started*.
    seen_clock: u64,
    fingerprint: Option<u64>,
    held: Option<T>,
    gated: bool,
}

/// Pure gating state, generic over the held discovery result so the logic is
/// testable without the work-finder types.
#[derive(Debug)]
pub struct PollGate<T> {
    enabled: bool,
    base_cadence: Duration,
    clock: u64,
    repo_last: HashMap<String, u64>,
    unscoped_last: u64,
    workspaces: BTreeMap<String, Ws<T>>,
    skipped: u64,
    event_repolls: u64,
    hard_cap_repolls: u64,
    lossy_repolls: u64,
}

impl<T: Clone> PollGate<T> {
    #[must_use]
    pub fn new(enabled: bool, base_cadence: Duration) -> Self {
        Self {
            enabled,
            base_cadence,
            clock: 0,
            repo_last: HashMap::new(),
            unscoped_last: 0,
            workspaces: BTreeMap::new(),
            skipped: 0,
            event_repolls: 0,
            hard_cap_repolls: 0,
            lossy_repolls: 0,
        }
    }

    pub fn configure(&mut self, enabled: bool, base_cadence: Duration) {
        self.enabled = enabled;
        self.base_cadence = base_cadence;
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Record a feed event as an invalidation key. `repo` is `None` (or not
    /// path-safe) for an event that cannot be attributed: it invalidates every
    /// workspace.
    pub fn note_event(&mut self, repo: Option<&str>) {
        if !self.enabled {
            return;
        }
        self.clock += 1;
        match repo.map(str::trim).filter(|r| repo_is_path_safe(r)) {
            Some(repo) => {
                self.repo_last.insert(repo.to_ascii_lowercase(), self.clock);
            }
            None => self.unscoped_last = self.clock,
        }
    }

    /// Record a clamped page: events were skipped, so nothing is attributable.
    pub fn note_unattributable(&mut self) {
        self.note_event(None);
    }

    fn event_since(&self, repo: &str, seen: u64) -> bool {
        self.unscoped_last > seen
            || self
                .repo_last
                .get(&repo.to_ascii_lowercase())
                .is_some_and(|at| *at > seen)
    }

    /// Decide whether the read for `key` (a stable workspace identity) of
    /// `repo` may be skipped. `healthy` is the live feed status.
    pub fn decide(
        &mut self,
        key: &str,
        repo: Option<&str>,
        kind: ReadKind,
        healthy: bool,
        now: Instant,
    ) -> Decision {
        if !self.enabled {
            return Decision::Poll(PollReason::Ungated);
        }
        if kind != ReadKind::Discovery {
            return Decision::Poll(PollReason::DecisionRead);
        }
        let Some(repo) = repo else {
            return Decision::Poll(PollReason::Ungated);
        };
        if !healthy {
            if let Some(ws) = self.workspaces.get_mut(key) {
                ws.gated = false;
            }
            return Decision::Poll(PollReason::FeedNotHealthy);
        }
        let cap = hard_cap(self.base_cadence);
        let verdict = match self.workspaces.get(key) {
            None => Decision::Poll(PollReason::FirstPoll),
            Some(ws) if ws.held.is_none() => Decision::Poll(PollReason::FirstPoll),
            Some(ws) if self.event_since(repo, ws.seen_clock) => Decision::Poll(PollReason::Event),
            Some(ws) => match ws.last_poll {
                Some(at) if now.saturating_duration_since(at) < cap => Decision::Skip,
                _ => Decision::Poll(PollReason::HardCap),
            },
        };
        if let Some(ws) = self.workspaces.get_mut(key) {
            ws.gated = verdict == Decision::Skip;
        }
        if verdict == Decision::Skip {
            self.skipped += 1;
        }
        verdict
    }

    /// The event clock to pass back to [`Self::record_poll`]; captured *before*
    /// the poll so an event landing mid-poll forces a re-poll next time.
    #[must_use]
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// The held discovery result, for a [`Decision::Skip`].
    #[must_use]
    pub fn held(&self, key: &str) -> Option<T> {
        self.workspaces.get(key).and_then(|ws| ws.held.clone())
    }

    /// Record a successful poll. `fingerprint` summarizes the result so a
    /// hard-cap re-poll that changed it (with no event) counts as lossy.
    pub fn record_poll(
        &mut self,
        key: &str,
        repo: &str,
        reason: PollReason,
        clock_at_decide: u64,
        fingerprint: u64,
        result: T,
        now: Instant,
    ) {
        if !self.enabled {
            return;
        }
        match reason {
            PollReason::Event => self.event_repolls += 1,
            PollReason::HardCap => {
                self.hard_cap_repolls += 1;
                let changed = self
                    .workspaces
                    .get(key)
                    .and_then(|ws| ws.fingerprint)
                    .is_some_and(|prev| prev != fingerprint);
                if changed {
                    self.lossy_repolls += 1;
                }
            }
            _ => {}
        }
        self.workspaces.insert(
            key.to_string(),
            Ws {
                repo: repo.to_string(),
                last_poll: Some(now),
                seen_clock: clock_at_decide,
                fingerprint: Some(fingerprint),
                held: Some(result),
                gated: false,
            },
        );
    }

    #[must_use]
    pub fn snapshot(&self, healthy: bool, now: Instant) -> PollGatingStatus {
        PollGatingStatus {
            enabled: self.enabled,
            gating_active: self.enabled && healthy,
            hard_cap_secs: hard_cap(self.base_cadence).as_secs(),
            polls_skipped: self.skipped,
            event_repolls: self.event_repolls,
            hard_cap_repolls: self.hard_cap_repolls,
            lossy_repolls: self.lossy_repolls,
            workspaces: self
                .workspaces
                .iter()
                .map(|(key, ws)| PollGatingWorkspace {
                    workspace: key.clone(),
                    repo: ws.repo.clone(),
                    gated: ws.gated && healthy,
                    last_poll_age_secs: ws
                        .last_poll
                        .map(|at| now.saturating_duration_since(at).as_secs()),
                })
                .collect(),
        }
    }
}

// ============================================================================
// Process-global gate (the work finder rebuilds its sources every tick)
// ============================================================================

type WorkItems = Vec<crate::work_finder::WorkItem>;

fn global() -> &'static Mutex<PollGate<WorkItems>> {
    static GATE: OnceLock<Mutex<PollGate<WorkItems>>> = OnceLock::new();
    GATE.get_or_init(|| {
        Mutex::new(PollGate::new(
            false,
            Duration::from_secs(crate::work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS),
        ))
    })
}

fn lock() -> std::sync::MutexGuard<'static, PollGate<WorkItems>> {
    global()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Arm (or disarm) the process gate. Called once at daemon start.
pub fn configure(enabled: bool, base_cadence: Duration) {
    lock().configure(enabled, base_cadence);
}

/// Arm the gate from the workspace's `forgeEvents.pollGating` (env > config >
/// off), with the work finder's cadence as the hard cap's base (#9255).
pub fn configure_for(workspace: &std::path::Path, base_cadence: Duration) {
    configure(resolve_poll_gating(&super::read_config(workspace)), base_cadence);
}

/// Feed ingestion hook: called by the feed consumer for every verified page,
/// before the status flips to `healthy`. No-op when gating is off.
pub fn ingest_page(events: &[serde_json::Value], clamped: bool) {
    let mut gate = lock();
    if !gate.enabled() {
        return;
    }
    for event in events {
        let repo = event.get("repo").and_then(serde_json::Value::as_str);
        gate.note_event(repo);
    }
    if clamped {
        gate.note_unattributable();
    }
}

/// A stable summary of a candidate listing: number, labels and `updated_at`.
fn fingerprint(items: &[crate::work_finder::WorkItem]) -> u64 {
    let mut rows: Vec<(u32, Vec<&str>, Option<&str>)> = items
        .iter()
        .map(|item| {
            let mut labels: Vec<&str> = item.labels.iter().map(String::as_str).collect();
            labels.sort_unstable();
            (item.number, labels, item.updated_at.as_deref())
        })
        .collect();
    rows.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rows.hash(&mut hasher);
    hasher.finish()
}

/// `owner/repo` for a workspace root from its git remote (no forge call).
fn repo_for_root(root: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?;
    crate::forge_cmd::parse_nwo_from_remote_url(url.trim())
}

/// Run `list` (one workspace's discovery listing) under the gate.
///
/// Gate off (the default), feed not `healthy`, or repo unresolvable: `list`
/// runs, exactly as before. Otherwise a workspace with no event since its last
/// poll and inside its hard cap returns its held listing without a forge call.
///
/// # Errors
///
/// Propagates `list`'s error; a failed poll records nothing, so the next read
/// polls again.
pub fn gated_list<E>(
    root: Option<&std::path::Path>,
    repo_override: Option<&str>,
    list: impl FnOnce() -> Result<WorkItems, E>,
) -> Result<WorkItems, E> {
    if !lock().enabled() {
        return list();
    }
    let healthy = super::global_state() == ForgeEventsState::Healthy;
    let key = root.map_or_else(|| "<cwd>".to_string(), |r| r.display().to_string());
    let known_repo = lock().workspaces.get(&key).map(|ws| ws.repo.clone());
    let repo = repo_override
        .map(str::to_string)
        .or(known_repo)
        .or_else(|| root.and_then(repo_for_root));
    let (decision, clock) = {
        let mut gate = lock();
        let d = gate.decide(&key, repo.as_deref(), ReadKind::Discovery, healthy, Instant::now());
        (d, gate.clock())
    };
    match decision {
        Decision::Skip => {
            if let Some(held) = lock().held(&key) {
                return Ok(held);
            }
            list()
        }
        Decision::Poll(reason) => {
            let items = list()?;
            if let Some(repo) = repo {
                lock().record_poll(
                    &key,
                    &repo,
                    reason,
                    clock,
                    fingerprint(&items),
                    items.clone(),
                    Instant::now(),
                );
            }
            Ok(items)
        }
    }
}

/// The status block for `loom-daemon status`; `None` while gating is off.
#[must_use]
pub fn snapshot() -> Option<PollGatingStatus> {
    let gate = lock();
    if !gate.enabled() {
        return None;
    }
    let healthy = super::global_state() == ForgeEventsState::Healthy;
    Some(gate.snapshot(healthy, Instant::now()))
}

#[cfg(test)]
#[path = "poll_gate/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "poll_gate/wiring_tests.rs"]
mod wiring_tests;
