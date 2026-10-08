//! The starred-issue liveness contract and loom-ui star intents (#9244 slice
//! C, #9301).
//!
//! Slice A made a starred (`loom:operator-priority`) issue sort first; slice B
//! made every role take starred work first. Ordering alone does not land an
//! issue: on 2026-09-28 none of the eight priority issues was moving, and
//! none of them was waiting in a queue. They were parked on things no agent
//! can resolve (a forge merge refusal, an operator decision, an exhausted
//! token pool) and nobody told a human. This module closes that gap:
//!
//! 1. **Landing state** ([`landing`]). For every starred issue the daemon
//!    computes one [`LandingStage`] with a next actor and time in stage, and
//!    publishes it on `DaemonStatusReport` (so `loom-daemon status`, `queue`
//!    and the `operator_attention` health section show it) and on
//!    `queue.snapshot` (so loom-ui does).
//! 2. **Immediate escalation** ([`escalate`]). A stage no agent can leave is
//!    `needs-operator` with one concrete [`OperatorAsk`] on the first pass
//!    that sees it (`pools-exhausted` after a grace window,
//!    `poolsExhaustedGraceMinutes`, default 10, so a peer can claim first). The ask is posted once as a comment on the issue, carrying
//!    a `<!-- loom:operator-priority-escalation key=… -->` marker. The marker
//!    is the dedupe: across ticks (an in-memory ledger short-circuits the
//!    check) and across hosts (every host reads the issue's comments for the
//!    marker before posting). Only markers from trusted authors count
//!    ([`trust`]): an insider, the fleet App, or this daemon.
//! 3. **Watchdog** ([`progress`]). An agent-owned stage with no forward
//!    progress for `noProgressMinutes` (default 30) escalates too, keyed by
//!    the stalled fingerprint so the same stall is reported once. Progress
//!    and the key use forge-visible facts only (labels, the PR, comments and
//!    lease renewals), so every host names a stall alike.
//! 4. **Blocker inheritance** ([`inherit`]). The issue blocking a starred
//!    issue (named by `loom:blocked`, the open incident tied to a merge
//!    refusal, or the repo's red-main fix) inherits the star's queue position
//!    in the work finder and its escalation, and loses both once it stops
//!    blocking, or once its repo has been unreadable for
//!    [`task::MAX_FAILED_PASSES`] passes. A cross-repo blocker is an operator
//!    ask: stars do not cross repos. A stale block (no open blocker named in
//!    the body or comments) is resolved by the pass itself ([`stale`]): an
//!    all-closed block is removed, an unnamed one is handed to Curator, and
//!    only a block Curator could not name reaches the operator (#10151). With
//!    `propagate` on (the default), a starred issue's children by every link
//!    [`edges`] resolves inherit the same way (#10012), transitively to
//!    [`collect::MAX_INHERIT_DEPTH`], and the pass writes the inherited star
//!    as the label, taking it back once the root loses its star
//!    ([`materialize`]).
//! 5. **Priority levels** ([`levels`], #10307). Every open issue that
//!    blocks a level >= 2 issue (`loom:operator-high-priority`), directly or
//!    transitively and across managed repos, carries the level's derived
//!    label (`loom:high-priority-inherited`) with a body provenance marker, and
//!    loses it once no source reaches it. Over-cap levels and blockers that
//!    need the operator lead the digest.
//! 6. **loom-ui star intents** ([`intents`]). The `/ingest` ack may carry
//!    `operator_priority_intents`; the exporter queues them and this module
//!    validates and applies them idempotently, with one audit comment whose
//!    `requested_at` becomes the authoritative starred-at.
//!
//! # Channels
//!
//! The ask reaches the operator through the forge comment (a GitHub
//! notification, deduped by marker), the `operator_attention` health section,
//! `loom-daemon status` / `queue`, and loom-ui via `queue.snapshot`. Safehouse
//! (and through it the team Matrix room) is **deferred**: the Safehouse sink
//! only narrates the frozen event-bus taxonomy, and a second ad hoc socket
//! client for one message kind is a new integration, not a reuse. The
//! follow-up is recorded on the PR.
//!
//! # One lister per fleet for the idle probe (W12 part 2)
//!
//! For a repo with no open starred issue the pass only lists the operator
//! labels and stops. With `fleet.captainGauges.starFacts` the fleet captain
//! makes those listings for the fleet, and this host skips its evaluator for
//! a repo the captain freshly reports as star-free ([`captain`]). A repo with
//! a star is still evaluated here in full, by every host that manages it.
//!
//! # Where it runs
//!
//! One background thread ([`task`]) while the work finder is enabled, every
//! `intervalSecs` (default 120). All forge reads are ETag-cached listings
//! except the per-issue comment reads, which happen only for a new escalation
//! key or an approved PR whose `updated_at` moved.

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::types::StarLivenessReport;
pub use crate::types::{AskKind, LandingStage, OperatorAsk, StarLandingRow};

pub mod captain;
pub mod collect;
pub mod edges;
pub mod escalate;
pub mod forge;
pub mod inherit;
pub mod inherited_star;
pub mod intents;
pub mod landing;
pub mod levels;
pub mod materialize;
pub mod parent_link;
pub mod progress;
pub mod propagation_rules;
pub mod queue;
pub mod refusal;
pub mod render;
pub mod stale;
pub mod task;
pub mod trust;

#[cfg(test)]
pub(crate) mod tests;

/// Env override for [`Settings::no_progress`] (minutes).
pub const NO_PROGRESS_MINUTES_ENV: &str = "LOOM_OPERATOR_PRIORITY_NO_PROGRESS_MINUTES";
/// Env override for [`Settings::escalate`] (`0`/`false` disables forge writes).
pub const ESCALATE_ENV: &str = "LOOM_OPERATOR_PRIORITY_ESCALATE";
/// Env override for [`Settings::interval`] (seconds).
pub const INTERVAL_SECS_ENV: &str = "LOOM_OPERATOR_PRIORITY_INTERVAL_SECS";
/// Env override for [`Settings::pools_grace`] (minutes; `0` asks at once).
pub const POOLS_GRACE_MINUTES_ENV: &str = "LOOM_OPERATOR_PRIORITY_POOLS_GRACE_MINUTES";
/// Env override for [`Settings::propagate`] (`0`/`false` turns it off).
pub const PROPAGATE_ENV: &str = "LOOM_OPERATOR_PRIORITY_PROPAGATE";
/// Env override for [`Settings::materialize_labels`] (#10012 §2–§3).
pub const MATERIALIZE_LABELS_ENV: &str = "LOOM_OPERATOR_PRIORITY_MATERIALIZE_LABELS";

/// Default watchdog window.
pub const DEFAULT_NO_PROGRESS_MINUTES: u64 = 30;
/// Default pass interval.
pub const DEFAULT_INTERVAL_SECS: u64 = 120;
/// Default `pools-exhausted` grace window.
pub const DEFAULT_POOLS_GRACE_MINUTES: u64 = 10;

/// Resolved `autonomous.operatorPriority` settings (**env > config >
/// default** for each knob).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// `noProgressMinutes`: the watchdog window.
    pub no_progress: Duration,
    /// `escalate`: whether escalations and intents are written to the forge.
    /// Off, the pass still computes and publishes every landing state.
    pub escalate: bool,
    /// `intervalSecs`: how often the pass runs.
    pub interval: Duration,
    /// `poolsExhaustedGraceMinutes`: how long an issue waits on this host's
    /// exhausted pool before the `pools-exhausted` ask, so a peer host with
    /// capacity can claim it first.
    pub pools_grace: Duration,
    /// `propagate`: whether a star reaches a starred issue's children through
    /// every parent/child link [`edges`] resolves (park records, task lists,
    /// dependency phrases), not only through the liveness blockers
    /// (`loom:blocked` blockers, refusal incident, red-main fix), which
    /// always inherit (#10012).
    pub propagate: bool,
    /// `materializeLabels`: whether the pass writes the inherited star as the
    /// `loom:operator-priority` label (and its PR's), and takes it back once
    /// the root loses its star (#10012 §2–§3, [`materialize`]). **Off by
    /// default**: an opt-in until the fleet has run it, since a revert leaves
    /// the labels it added. Needs `propagate` and `escalate` too; with it off
    /// the inherited star stays an in-memory ordering, as before.
    pub materialize_labels: bool,
    /// `levelCaps`: the most open issues fleet-wide (as this host sees it)
    /// that may carry each level's operator label (#10307), by level. Over
    /// the cap is reported in the digest, never refused.
    pub level_caps: LevelCaps,
}

/// Most levels [`LevelCaps`] holds a cap for.
pub const MAX_CAPPED_LEVEL: usize = 8;

/// Per-level caps (`autonomous.operatorPriority.levelCaps`, an object keyed
/// by level: `{"2": 5}`), defaulting to the level table's `default_cap`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelCaps(pub [Option<usize>; MAX_CAPPED_LEVEL + 1]);

impl Default for LevelCaps {
    fn default() -> Self {
        let mut caps = [None; MAX_CAPPED_LEVEL + 1];
        for row in crate::operator_levels::table() {
            if let Some(slot) = caps.get_mut(usize::from(row.level)) {
                *slot = row.default_cap;
            }
        }
        Self(caps)
    }
}

impl LevelCaps {
    /// The cap for `level`, if any.
    #[must_use]
    pub fn cap(&self, level: u8) -> Option<usize> {
        self.0.get(usize::from(level)).copied().flatten()
    }

    fn from_block(v: Option<&serde_json::Value>) -> Self {
        let mut caps = Self::default();
        if let Some(obj) = v.and_then(serde_json::Value::as_object) {
            for (k, v) in obj {
                let (Ok(level), Some(n)) = (k.trim().parse::<usize>(), v.as_u64()) else {
                    continue;
                };
                if let Some(slot) = caps.0.get_mut(level) {
                    *slot = usize::try_from(n).ok().filter(|n| *n > 0);
                }
            }
        }
        caps
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            no_progress: Duration::from_secs(DEFAULT_NO_PROGRESS_MINUTES * 60),
            escalate: true,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            pools_grace: Duration::from_secs(DEFAULT_POOLS_GRACE_MINUTES * 60),
            propagate: true,
            materialize_labels: false,
            level_caps: LevelCaps::default(),
        }
    }
}

fn positive_u64(v: Option<&serde_json::Value>) -> Option<u64> {
    v.and_then(serde_json::Value::as_u64).filter(|n| *n > 0)
}

fn env_u64(name: &str) -> Option<u64> {
    env_u64_or_zero(name).filter(|n| *n > 0)
}

fn env_u64_or_zero(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn env_bool(name: &str) -> Option<bool> {
    let v = std::env::var(name).ok()?;
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Settings {
    /// Resolve from an `autonomous.operatorPriority` JSON block (or `None`)
    /// plus the environment.
    #[must_use]
    pub fn from_block(block: Option<&serde_json::Value>) -> Self {
        let d = Self::default();
        let cfg = |k: &str| block.and_then(|b| b.get(k));
        let minutes = env_u64(NO_PROGRESS_MINUTES_ENV)
            .or_else(|| positive_u64(cfg("noProgressMinutes")))
            .unwrap_or(DEFAULT_NO_PROGRESS_MINUTES);
        let interval = env_u64(INTERVAL_SECS_ENV)
            .or_else(|| positive_u64(cfg("intervalSecs")))
            .unwrap_or(DEFAULT_INTERVAL_SECS);
        let grace = env_u64_or_zero(POOLS_GRACE_MINUTES_ENV)
            .or_else(|| cfg("poolsExhaustedGraceMinutes").and_then(serde_json::Value::as_u64))
            .unwrap_or(DEFAULT_POOLS_GRACE_MINUTES);
        let escalate = env_bool(ESCALATE_ENV)
            .or_else(|| cfg("escalate").and_then(serde_json::Value::as_bool))
            .unwrap_or(d.escalate);
        let propagate = env_bool(PROPAGATE_ENV)
            .or_else(|| cfg("propagate").and_then(serde_json::Value::as_bool))
            .unwrap_or(d.propagate);
        let materialize_labels = env_bool(MATERIALIZE_LABELS_ENV)
            .or_else(|| cfg("materializeLabels").and_then(serde_json::Value::as_bool))
            .unwrap_or(d.materialize_labels);
        Self {
            no_progress: Duration::from_secs(minutes.saturating_mul(60)),
            escalate,
            interval: Duration::from_secs(interval),
            pools_grace: Duration::from_secs(grace.saturating_mul(60)),
            propagate,
            materialize_labels,
            level_caps: LevelCaps::from_block(cfg("levelCaps")),
        }
    }

    /// Resolve for the workspace at `root` (`.loom/config.json` through the
    /// config resolver). A missing or malformed block is all defaults.
    #[must_use]
    pub fn resolve(root: &Path) -> Self {
        let effective = crate::config_resolver::resolve_effective_config(root);
        let block = crate::config_resolver::get_path(&effective, "autonomous")
            .and_then(|a| a.get("operatorPriority"))
            .cloned();
        Self::from_block(block.as_ref())
    }
}

fn last_slot() -> &'static Mutex<Option<StarLivenessReport>> {
    static SLOT: OnceLock<Mutex<Option<StarLivenessReport>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Publish the latest pass for `status` and `queue.snapshot`.
pub fn publish_report(report: StarLivenessReport) {
    *last_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
}

/// The latest pass, or `None` before the first one (or with the work finder
/// off).
#[must_use]
pub fn last_report() -> Option<StarLivenessReport> {
    last_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
