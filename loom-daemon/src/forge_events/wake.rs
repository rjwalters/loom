//! Early-tick wake seam for the `forge.event` prompt (ADR-0021, Epic #8764
//! Phase 2, issue #8766).
//!
//! # What this is
//!
//! Phase 1 ([`super`]) polls the operator's per-host feed and publishes one
//! `forge.event` bus prompt per verified, non-empty page. **Nothing consumed
//! it.** This module holds every consumer that does: each turns a qualifying
//! prompt into an **early tick of a loop that already exists**, and nothing
//! else.
//!
//! The seam is deliberately shaped so a consumer cannot become a decision
//! path even by accident. [`EarlyTicker`] is a drop-in replacement for the
//! `tokio::time::Interval` a daemon loop already owns — same
//! `set_missed_tick_behavior` / `tick()` surface — so the *only* thing a wake
//! can do is make the loop's own next iteration happen sooner. The loop body
//! is untouched: it re-lists, re-memoes and re-decides through exactly the
//! forge reads its timer would have driven anyway.
//!
//! # Invariants (ADR-0014, restated because they bound this file)
//!
//! 1. **A wake is a prompt, not a truth.** The payload selects *which loop to
//!    tick now* using routing hints only — `source`, `count`, and the page's
//!    event-type names. No field of the payload is compared against forge
//!    state, memoed, or carried into the tick: [`EarlyTicker::tick`] returns
//!    `()`, so there is no channel by which event content could reach a
//!    decision.
//! 2. **Per-loop opt-out, default off.** Each consumer sits behind its own
//!    `forgeEvents.events.*` flag (precedence **env > config > default**,
//!    default `false`). Off ⇒ [`EarlyTicker`] holds no subscription at all:
//!    no bridge task, no bus receiver, and `tick()` is a bare
//!    `Interval::tick()`. A fleet can run the Phase 1 feed with byte-identical
//!    dispatch behaviour.
//! 3. **Degradation is the existing cadence.** Feed off, `host_mismatch`,
//!    `backoff`, or consumer disabled ⇒ zero publications or zero
//!    subscriptions, and the loop ticks exactly as it does today. The failure
//!    mode is latency-only by construction, not by care.
//! 4. **Burst coalescing.** The wake is a [`tokio::sync::Notify`] permit,
//!    which saturates at one: a 500-event burst that arrives while the loop is
//!    busy leaves **one** pending permit, not 500. A minimum-spacing floor
//!    ([`DEFAULT_MIN_SPACING_SECS`]) then bounds how soon that permit may be
//!    spent after the previous tick of *either* kind, so the early-tick path
//!    can never raise a loop's forge-request rate above
//!    `interval / min_spacing` times its configured cadence — at the defaults,
//!    2x, and never a new endpoint or a new rate-limit envelope.
//!
//! # Consumers
//!
//! Phase 2 names three, and all three ship here as [`Consumer`] descriptors —
//! a name, its `forgeEvents.events.*` flag, and the page event-type names it
//! acts on:
//!
//! | [`Consumer`] | Loop it ticks early | Why that loop |
//! |---|---|---|
//! | [`WORK_FINDER`] | [`crate::work_finder`]'s dispatch tick | It is the loop that lists claimable issues, re-derives the ready queue, and dispatches its head. |
//! | [`QUEUE_HEAD`] | [`crate::claim_reconciliation`]'s periodic pass | A `loom:building` claim whose sweep is gone is what *holds* the queue head; this pass is the loop that releases it. |
//! | [`IN_FLIGHT_PR`] | [`crate::watch_registry`]'s watch monitor | It is the loop that polls a watched issue/PR for terminal state (merged / closed) and reports it. |
//!
//! Issue #8766 named a "dispatch queue head re-check" and an "in-flight PR
//! merge-status re-check" as if each were its own module. Neither exists under
//! those names, which the issue anticipated ("locate the actual existing
//! code paths rather than assume the names above"). The two loops above are
//! those code paths: the *ready queue* is re-derived inside each work-finder
//! tick rather than by a separate loop, so the thing that actually gates the
//! head moving is claim reconciliation; and the daemon's only standing
//! "is this PR done yet" poller is the watch monitor.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::event_bus::EventBus;
use crate::types::Event;

use super::{BUS_TOPIC, PAYLOAD_SOURCE};

#[cfg(test)]
mod tests;

// ============================================================================
// Env overrides + defaults
// ============================================================================

/// `forgeEvents.events.workFinderTick` env override.
pub const WORK_FINDER_TICK_ENV: &str = "LOOM_FORGE_EVENTS_WORK_FINDER_TICK";

/// `forgeEvents.events.queueHeadWake` env override.
pub const QUEUE_HEAD_WAKE_ENV: &str = "LOOM_FORGE_EVENTS_QUEUE_HEAD_WAKE";

/// `forgeEvents.events.inFlightPrWatch` env override.
pub const IN_FLIGHT_PR_WATCH_ENV: &str = "LOOM_FORGE_EVENTS_IN_FLIGHT_PR_WATCH";

/// `forgeEvents.events.minSpacingSecs` env override.
pub const MIN_SPACING_SECS_ENV: &str = "LOOM_FORGE_EVENTS_WAKE_MIN_SPACING_SECS";

/// Minimum wall-clock distance between a loop's previous tick (of **either**
/// kind) and an early tick.
///
/// This is the rate bound, not a debounce nicety. Without it a chatty feed
/// could drive a loop at the feed's own 10-second poll cadence instead of the
/// loop's 60-second one — a 6x increase in that loop's forge-request volume,
/// which is exactly the "new rate-limit envelope" ADR-0021 forbids. At 30s
/// against the work finder's 60s default, an early tick can at most double a
/// loop's tick rate, and only while events are genuinely arriving.
pub const DEFAULT_MIN_SPACING_SECS: u64 = 30;

/// The event-type names that make a page *claimable-shaped* for the work
/// finder: an issue opened/labeled/closed, or a comment on one.
///
/// These are GitHub webhook event names as the operator's Worker forwards
/// them, matched against the page payload's `types` array. A page carrying
/// only (say) `check_run` tells the work finder nothing it acts on, so it is
/// not a prompt for this consumer — which is the whole reason the payload
/// carries type names at all.
pub const WORK_FINDER_EVENT_TYPES: &[&str] = &["issues", "issue_comment"];

/// The event-type names that may have *released the head* of the dispatch
/// queue: a comment on a claimed issue (an abandon note, a lease that stopped
/// being renewed) or a pull request reaching a terminal state.
///
/// Claim reconciliation is the loop that decides whether a `loom:building`
/// claim is still backed by a live sweep. Both shapes above are the forge-side
/// residue of a sweep ending — the moment it becomes worth asking that
/// question again sooner than the pass's own multi-minute cadence.
pub const QUEUE_HEAD_EVENT_TYPES: &[&str] = &["issue_comment", "pull_request"];

/// The event-type names that may have moved a watched PR toward terminal
/// state: the PR itself changing (merged / closed / reopened) or its checks
/// completing.
///
/// `check_run` / `check_suite` are included because a watch on a PR is usually
/// really a watch on "did this land", and checks completing is the last thing
/// that happens before an auto-merge does.
pub const IN_FLIGHT_PR_EVENT_TYPES: &[&str] = &["pull_request", "check_run", "check_suite"];

// ============================================================================
// Config
// ============================================================================

/// The `.loom/config.json` `forgeEvents.events` block — one opt-in flag per
/// Phase 2 consumer, plus the shared spacing floor.
///
/// Deliberately a separate struct from [`super::ForgeEventsConfig`]: Phase 1's
/// block answers "should this host poll a feed", this one answers "may a
/// verified page make a loop tick early". A fleet that wants the feed's
/// journal and status without any dispatch-path effect sets the first and
/// leaves this one absent, which is also the default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsumerConfig {
    /// `forgeEvents.events.workFinderTick`.
    pub work_finder_tick: Option<bool>,
    /// `forgeEvents.events.queueHeadWake`.
    pub queue_head_wake: Option<bool>,
    /// `forgeEvents.events.inFlightPrWatch`.
    pub in_flight_pr_watch: Option<bool>,
    /// `forgeEvents.events.minSpacingSecs` (a zero/invalid value is dropped to
    /// `None` — a zero floor would remove the rate bound entirely).
    pub min_spacing_secs: Option<u64>,
}

/// One Phase 2 consumer: which loop it ticks, which flag arms it, and which
/// page event-type names it acts on.
///
/// A descriptor rather than three copies of the same constructor, so "add a
/// consumer" is a `const` and a call site — and so the default-off, spacing
/// floor and coalescing guarantees are implemented **once** and cannot drift
/// between consumers.
#[derive(Clone, Copy)]
pub struct Consumer {
    /// Human-readable name, used only in log lines.
    pub name: &'static str,
    /// The `forgeEvents.events.*` key that arms this consumer.
    pub config_key: &'static str,
    /// The environment variable that overrides that key.
    pub env: &'static str,
    /// Page event-type names this consumer acts on.
    pub types: &'static [&'static str],
    /// Project this consumer's flag out of a parsed [`ConsumerConfig`].
    flag: fn(&ConsumerConfig) -> Option<bool>,
}

impl std::fmt::Debug for Consumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Consumer")
            .field("name", &self.name)
            .field("config_key", &self.config_key)
            .field("types", &self.types)
            .finish_non_exhaustive()
    }
}

/// Consumer (a): early tick of the work-finder dispatch loop.
pub const WORK_FINDER: Consumer = Consumer {
    name: "work-finder tick",
    config_key: "workFinderTick",
    env: WORK_FINDER_TICK_ENV,
    types: WORK_FINDER_EVENT_TYPES,
    flag: |c| c.work_finder_tick,
};

/// Consumer (b): early tick of the periodic claim-reconciliation pass — the
/// loop that releases a dead `loom:building` claim blocking the queue head.
pub const QUEUE_HEAD: Consumer = Consumer {
    name: "queue-head wake",
    config_key: "queueHeadWake",
    env: QUEUE_HEAD_WAKE_ENV,
    types: QUEUE_HEAD_EVENT_TYPES,
    flag: |c| c.queue_head_wake,
};

/// Consumer (c): early tick of the durable watch monitor — the loop that polls
/// a watched in-flight issue/PR for terminal state.
pub const IN_FLIGHT_PR: Consumer = Consumer {
    name: "in-flight PR watch",
    config_key: "inFlightPrWatch",
    env: IN_FLIGHT_PR_WATCH_ENV,
    types: IN_FLIGHT_PR_EVENT_TYPES,
    flag: |c| c.in_flight_pr_watch,
};

/// Every shipped consumer, in Phase 2's own (a)/(b)/(c) order.
pub const ALL_CONSUMERS: &[Consumer] = &[WORK_FINDER, QUEUE_HEAD, IN_FLIGHT_PR];

/// Read the `forgeEvents.events` block from `root`'s resolved config.
///
/// Soft-fails to [`ConsumerConfig::default`] (every flag `None` ⇒ off) on a
/// missing file, malformed JSON, or an absent `forgeEvents` / `events` block
/// — the same contract as [`super::read_config`].
#[must_use]
pub fn read_config(root: &Path) -> ConsumerConfig {
    let config = crate::config_resolver::resolve_effective_config(root);
    let Some(block) = crate::config_resolver::get_path(&config, "forgeEvents.events") else {
        return ConsumerConfig::default();
    };
    ConsumerConfig {
        work_finder_tick: block
            .get(WORK_FINDER.config_key)
            .and_then(serde_json::Value::as_bool),
        queue_head_wake: block
            .get(QUEUE_HEAD.config_key)
            .and_then(serde_json::Value::as_bool),
        in_flight_pr_watch: block
            .get(IN_FLIGHT_PR.config_key)
            .and_then(serde_json::Value::as_bool),
        min_spacing_secs: block
            .get("minSpacingSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0),
    }
}

/// **env > config > default** (`false`) for one consumer.
///
/// The default is `false` for *every* consumer and there is no way to express
/// "on unless told otherwise" here — the ADR-0021 per-loop opt-out is a
/// property of this one function, not of three independent call sites.
#[must_use]
pub fn resolve_enabled(consumer: &Consumer, config: &ConsumerConfig) -> bool {
    super::env_bool(consumer.env)
        .or((consumer.flag)(config))
        .unwrap_or(false)
}

/// **env > config > default** ([`DEFAULT_MIN_SPACING_SECS`]).
#[must_use]
pub fn resolve_min_spacing(config: &ConsumerConfig) -> Duration {
    let secs = std::env::var(MIN_SPACING_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .or(config.min_spacing_secs)
        .unwrap_or(DEFAULT_MIN_SPACING_SECS);
    Duration::from_secs(secs)
}

// ============================================================================
// Qualification — routing hints only
// ============================================================================

/// Does this `forge.event` payload prompt a consumer that cares about
/// `wanted` event types?
///
/// Three checks, all on routing hints: the payload is from the feed consumer
/// (`source`), the page was non-empty (`count`), and at least one of the
/// page's event-type names is one this consumer acts on. Nothing here reads
/// forge state, because the payload deliberately carries none (#8765): no
/// repo, no issue or PR number, no labels, no actor.
#[must_use]
pub fn payload_qualifies(payload: &serde_json::Value, wanted: &[&str]) -> bool {
    if payload.get("source").and_then(serde_json::Value::as_str) != Some(PAYLOAD_SOURCE) {
        return false;
    }
    if payload
        .get("count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        == 0
    {
        return false;
    }
    payload
        .get("types")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|types| {
            types
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|name| wanted.contains(&name))
        })
}

/// [`payload_qualifies`], lifted to a bus event.
///
/// Anything that is not a [`BUS_TOPIC`] `Generic` is not a prompt — including
/// the bus's own [`Event::TopicLag`] sentinel. A lag means prompts were
/// dropped, and the honest answer to a dropped prompt is the polling floor
/// (invariant 3), not a speculative wake on an unknown page.
#[must_use]
pub fn event_qualifies(event: &Event, wanted: &[&str]) -> bool {
    match event {
        Event::Generic { topic, payload } if topic == BUS_TOPIC => {
            payload_qualifies(payload, wanted)
        }
        _ => false,
    }
}

// ============================================================================
// Counters
// ============================================================================

/// What the wake seam did, for tests and for a future status surface.
///
/// Counters only — nothing here is read back into a decision.
#[derive(Debug, Default)]
pub struct WakeCounters {
    /// Qualifying `forge.event` prompts observed by the bridge.
    pub prompts: AtomicU64,
    /// Ticks actually taken early because of a prompt.
    pub early_ticks: AtomicU64,
    /// Prompts dropped by the [`DEFAULT_MIN_SPACING_SECS`] floor. A healthy
    /// number under a burst: it is coalescing working, not a fault.
    pub throttled: AtomicU64,
}

impl WakeCounters {
    /// Qualifying prompts observed.
    #[must_use]
    pub fn prompts(&self) -> u64 {
        self.prompts.load(Ordering::Relaxed)
    }

    /// Early ticks taken.
    #[must_use]
    pub fn early_ticks(&self) -> u64 {
        self.early_ticks.load(Ordering::Relaxed)
    }

    /// Prompts suppressed by the spacing floor.
    #[must_use]
    pub fn throttled(&self) -> u64 {
        self.throttled.load(Ordering::Relaxed)
    }
}

/// Subscribe to [`BUS_TOPIC`] and convert each qualifying prompt into a single
/// [`Notify`] permit.
///
/// The permit is where burst coalescing lives: `notify_one` saturates at one
/// stored permit, so however many pages land while the loop is mid-tick, the
/// loop owes itself at most one extra iteration.
///
/// The task ends when the bus closes (every daemon-held handle lives for the
/// process, so in practice it ends when [`EarlyTicker`] aborts it on drop).
pub fn spawn_bridge(
    bus: &EventBus,
    wanted: &'static [&'static str],
    notify: Arc<Notify>,
    counters: Arc<WakeCounters>,
) -> tokio::task::JoinHandle<()> {
    let mut subscription = bus.subscribe([BUS_TOPIC]);
    tokio::spawn(async move {
        while let Ok(event) = subscription.recv().await {
            if event_qualifies(&event, wanted) {
                counters.prompts.fetch_add(1, Ordering::Relaxed);
                notify.notify_one();
            }
        }
    })
}

// ============================================================================
// The seam itself
// ============================================================================

/// A `tokio::time::Interval` that may also be ticked early by a `forge.event`
/// prompt.
///
/// Constructed disarmed ([`EarlyTicker::plain`]) or armed
/// ([`EarlyTicker::for_work_finder`]). Disarmed, it is the interval it wraps
/// and nothing else — this is the shape that makes the default-off guarantee
/// structural rather than conditional.
#[derive(Debug)]
pub struct EarlyTicker {
    ticker: tokio::time::Interval,
    /// `None` ⇒ disarmed: no subscription, no bridge, no wake path.
    wake: Option<Arc<Notify>>,
    min_spacing: Duration,
    /// When the previous tick of *either* kind returned. An early tick is
    /// refused within [`Self::min_spacing`] of it.
    ///
    /// A [`tokio::time::Instant`], not a `std::time::Instant`, so the floor
    /// lives on the same clock as the interval it bounds — which is also what
    /// lets a paused-time test advance past it deterministically instead of
    /// sleeping.
    last_tick: Option<Instant>,
    counters: Arc<WakeCounters>,
    bridge: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for EarlyTicker {
    fn drop(&mut self) {
        if let Some(bridge) = self.bridge.take() {
            bridge.abort();
        }
    }
}

impl EarlyTicker {
    /// A disarmed ticker: `tokio::time::interval(interval)` with no wake path.
    #[must_use]
    pub fn plain(interval: Duration) -> Self {
        EarlyTicker {
            ticker: tokio::time::interval(interval),
            wake: None,
            min_spacing: Duration::from_secs(DEFAULT_MIN_SPACING_SECS),
            last_tick: None,
            counters: Arc::new(WakeCounters::default()),
            bridge: None,
        }
    }

    /// A loop's ticker, armed only when `consumer`'s own
    /// `forgeEvents.events.*` flag resolves true for `root`.
    ///
    /// When the flag is off this is [`EarlyTicker::plain`] — it does not
    /// subscribe to the bus, does not spawn a task, and leaves the loop's
    /// observable behaviour byte-identical to a pre-Phase-2 daemon. Every
    /// consumer goes through this one function, so that guarantee is
    /// structural rather than repeated three times and hopefully consistent.
    #[must_use]
    pub fn for_consumer(
        consumer: &'static Consumer,
        interval: Duration,
        bus: &EventBus,
        root: &Path,
    ) -> Self {
        let config = read_config(root);
        if !resolve_enabled(consumer, &config) {
            log::debug!(
                "forge_events::wake: {} early tick disabled (set \
                 forgeEvents.events.{}=true to arm it)",
                consumer.name,
                consumer.config_key
            );
            return Self::plain(interval);
        }
        let min_spacing = resolve_min_spacing(&config);
        log::info!(
            "forge_events::wake: {} early tick armed (types={:?}, min_spacing={}s, cadence={}s)",
            consumer.name,
            consumer.types,
            min_spacing.as_secs(),
            interval.as_secs()
        );
        Self::armed(interval, bus, consumer.types, min_spacing)
    }

    /// [`Self::for_consumer`] for [`WORK_FINDER`].
    #[must_use]
    pub fn for_work_finder(interval: Duration, bus: &EventBus, root: &Path) -> Self {
        Self::for_consumer(&WORK_FINDER, interval, bus, root)
    }

    /// [`Self::for_consumer`] for [`QUEUE_HEAD`].
    #[must_use]
    pub fn for_queue_head(interval: Duration, bus: &EventBus, root: &Path) -> Self {
        Self::for_consumer(&QUEUE_HEAD, interval, bus, root)
    }

    /// [`Self::for_consumer`] for [`IN_FLIGHT_PR`].
    #[must_use]
    pub fn for_in_flight_pr(interval: Duration, bus: &EventBus, root: &Path) -> Self {
        Self::for_consumer(&IN_FLIGHT_PR, interval, bus, root)
    }

    /// An armed ticker over an explicit event-type set and spacing floor.
    ///
    /// The shared constructor behind [`Self::for_work_finder`], and the seam
    /// tests use to arm a ticker without a config file.
    #[must_use]
    pub fn armed(
        interval: Duration,
        bus: &EventBus,
        wanted: &'static [&'static str],
        min_spacing: Duration,
    ) -> Self {
        let notify = Arc::new(Notify::new());
        let counters = Arc::new(WakeCounters::default());
        let bridge = spawn_bridge(bus, wanted, notify.clone(), counters.clone());
        EarlyTicker {
            ticker: tokio::time::interval(interval),
            wake: Some(notify),
            min_spacing,
            last_tick: None,
            counters,
            bridge: Some(bridge),
        }
    }

    /// Delegates to [`tokio::time::Interval::set_missed_tick_behavior`], so a
    /// loop that already sets it keeps doing so unchanged.
    pub fn set_missed_tick_behavior(&mut self, behavior: tokio::time::MissedTickBehavior) {
        self.ticker.set_missed_tick_behavior(behavior);
    }

    /// `true` when a wake path exists (the consumer flag resolved on).
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.wake.is_some()
    }

    /// The seam's counters.
    #[must_use]
    pub fn counters(&self) -> &Arc<WakeCounters> {
        &self.counters
    }

    /// Wait for the next tick — the configured cadence, or a qualifying
    /// `forge.event` prompt, whichever comes first.
    ///
    /// Disarmed, this *is* `Interval::tick().await`.
    ///
    /// Armed, the select is `biased` toward the cadence branch so a ready
    /// timer always wins a tie: the feed can only ever add ticks, never
    /// displace or delay the ones the loop already owed itself. A prompt that
    /// arrives within [`Self::min_spacing`] of the previous tick is counted
    /// and dropped, and the wait continues — deliberately dropped rather than
    /// deferred, because a prompt is "check now", and a check that already
    /// happened moments ago has answered it.
    pub async fn tick(&mut self) {
        let Some(wake) = self.wake.clone() else {
            self.ticker.tick().await;
            return;
        };
        loop {
            tokio::select! {
                biased;
                _ = self.ticker.tick() => {
                    self.last_tick = Some(Instant::now());
                    return;
                }
                () = wake.notified() => {
                    let now = Instant::now();
                    if self
                        .last_tick
                        .is_some_and(|last| now.duration_since(last) < self.min_spacing)
                    {
                        self.counters.throttled.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    self.last_tick = Some(now);
                    self.counters.early_ticks.fetch_add(1, Ordering::Relaxed);
                    // Re-anchor the cadence to this tick, so an early tick
                    // replaces the pending timer tick rather than being
                    // followed immediately by it.
                    self.ticker.reset();
                    return;
                }
            }
        }
    }
}
