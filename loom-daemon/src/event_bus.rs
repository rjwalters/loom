//! In-memory pub/sub event bus for the loom-daemon
//! (Issue #3453, Phase B of epic #3449).
//!
//! # Overview
//!
//! This module provides a lightweight in-process pub/sub bus that
//! coordinates sweep-lifecycle events between the daemon, the reaper task,
//! and external subscribers (the MCP layer, monitoring tools, etc.).
//!
//! The bus is **NOT** NATS/ZeroMQ. It is a thin wrapper around
//! [`tokio::sync::broadcast`] with:
//!
//! - A bounded channel (default capacity 1,024 — see [`DEFAULT_CAPACITY`]).
//! - Prefix-match topic routing (`sweep.issue.*` matches
//!   `sweep.issue.123.phase`).
//! - Pass-through overflow semantics — slow subscribers receive a
//!   [`Event::TopicLag`] sentinel and continue from the current channel
//!   position. Matches tokio's `Receiver::Lagged` overflow behaviour.
//!
//! # Topic taxonomy (frozen for v0.10.0)
//!
//! | Topic | Publisher | Payload |
//! |-------|-----------|---------|
//! | `sweep.issue.{N}.phase`   | Sweep child via `PublishEvent` | `{phase, pr_number?}` |
//! | `sweep.issue.{N}.blocker` | Sweep child | `{reason, label_added}` |
//! | `sweep.issue.{N}.exited`  | Daemon reaper | `{exit_code, duration_sec, death_class?}` |
//! | `sweep.issue.{N}.crashed` | Daemon reaper | `{checkpoint_phase, death_class?}` |
//! | `sweep.global.dispatch`   | Daemon | `{sweep_id, kind, runtime?, runtime_source?}` |
//! | `sweep.global.runtime_rejected` | Daemon | `{kind, role, runtime, runtime_source, unmet_capabilities, reason}` |
//! | `sweep.global.completed`  | Daemon | `{sweep_id, outcome}` |
//! | `epic.issue.{N}.decompose` | Epic supervisor | `{epic, action, state}` |
//! | `epic.issue.{N}.expand`    | Epic supervisor | `{epic, action, state}` |
//! | `epic.issue.{N}.join`      | Epic supervisor | `{epic, action, state}` |
//! | `epic.issue.{N}.close`     | Epic supervisor | `{epic, action, state}` |
//! | `daemon.capacity.advisory` | Work finder | `{pressured, queued, healthy_accounts, exhausted_accounts, total_accounts, estimated_drain_minutes?, message}` |
//! | `daemon.drain.started`   | Drain supervisor | `{in_flight, timeout_secs, force_after_timeout, deadline}` |
//! | `daemon.drain.completed` | Drain supervisor | `{in_flight}` (always 0) |
//! | `daemon.drain.aborted`   | Daemon (IPC) | `{was_draining}` |
//! | `daemon.drain.timeout`   | Drain supervisor | `{in_flight, forced, cancelled?}` |
//! | `daemon.dispatch.headroom_advisory` | Daemon (IPC, `dispatch_sweep`) | `{repo_root, low_headroom, occupancy, dynamic_cap, disk_headroom, ram_headroom, token_axis_limit, message}` |
//! | `daemon.preflight.advisory` | Daemon reaper (`SweepRegistry`) | `{workspace_root, consecutive_deaths, marker, message}` |
//!
//! New topics require a follow-up issue — the taxonomy is intentionally
//! pinned. The four `epic.issue.{N}.*` topics were authorized by **#3873**
//! (epic #3842 Phase 4) for the epic supervisor's action classes
//! (decompose / expand / join / close). The `daemon.capacity.advisory` topic
//! was authorized by **#3902** (epic #3809) for the work finder's
//! token-capacity backpressure advisory (fired on pressure state change). The
//! four `daemon.drain.*` topics were authorized by **#4090** for the scheduled
//! drain-and-restart primitive (started / completed / aborted / timeout). The
//! `daemon.dispatch.headroom_advisory` topic was authorized by **#4234**
//! (decomposed from #4231) for the `dispatch_sweep` IPC handler's
//! advisory-first headroom consult — fired on a state change into/out of
//! "occupancy at or over the dynamic concurrency cap," mirroring
//! `daemon.capacity.advisory`'s dedup discipline but scoped to a single
//! dispatch request rather than the work finder's periodic tick. The
//! `daemon.preflight.advisory` topic was authorized by **#4386** for the
//! reaper's workspace-level claude-wrapper pre-flight-death tripwire — fired
//! on a state change into/out of "N consecutive dispatches, across different
//! issues, died at the wrapper's MCP-init pre-flight check before ever
//! exec'ing the CLI," the same dedup discipline as the two advisories above.
//! `sweep.global.runtime_rejected` topic was authorized by **#4494** (epic
//! #4489 Phase 5) for fail-closed runtime-capability admission: `.dispatch`
//! only ever describes *admitted* work, so refused work — a dispatch the
//! daemon declined before any claim/label/account/log/spawn side effect —
//! needed its own structured, secret-free representation rather than being
//! invisible on the bus. It is emitted once per refused dispatch attempt (not
//! on a state-change/dedup schedule) because each attempt is itself the event.
//! Per-death classification (`death_class`) rides the existing frozen
//! `sweep.issue.{N}.exited` / `.crashed` topics as an additive payload field
//! rather than a new topic (#4386). The bus accepts arbitrary topic
//! strings (`publish` does not reject unknown topics) so the publisher side
//! stays open for future extension, but the documented taxonomy is the
//! contract subscribers should rely on.
//!
//! # Durability (Issue #4644)
//!
//! This bus is **in-memory only** — a bounded [`tokio::sync::broadcast`]
//! channel. A published event with no attached subscriber (or one that
//! disconnects/lags past the buffer) is gone forever; nothing here persists
//! across a daemon restart. For `sweep.issue.{N}.exited` / `.crashed`
//! specifically, [`crate::sweep_outcomes`] closes that gap with a durable,
//! append-only on-disk journal (`<workspace>/.loom/logs/sweep-outcomes.jsonl`)
//! written at the same emit sites, additively — this bus's behavior and topic
//! contract are unchanged. Consult that module when a terminal outcome needs
//! to be queryable after the sweep registry's ~1h in-memory GC window, or
//! after a daemon restart.

use crate::types::Event;

use std::collections::HashSet;
use tokio::sync::broadcast;

// ============================================================================
// Constants
// ============================================================================

/// Default broadcast channel capacity. Matches the curator's frozen
/// architectural default (`1024`).
pub const DEFAULT_CAPACITY: usize = 1024;

// ============================================================================
// Event bus
// ============================================================================

/// In-memory pub/sub event bus.
///
/// Cloning the bus is **not** the way to add subscribers — call
/// [`EventBus::subscribe`] instead. Wrap the bus in an `Arc` if you need
/// to share it across tasks (the daemon's main wiring does so).
#[derive(Debug)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    capacity: usize,
}

impl EventBus {
    /// Construct a new bus with the default capacity (1024).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Construct a new bus with an explicit channel capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx, capacity }
    }

    /// Channel capacity as configured at construction.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of currently active subscribers (excludes the internal
    /// sender). Exposed for tests/metrics.
    #[must_use]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Publish an event to the bus.
    ///
    /// Returns `Ok(receiver_count)` on success or `Err(0)` when no
    /// receivers are active (matches the underlying `broadcast::Sender::send`
    /// convention but using `usize` directly so callers don't need to
    /// import tokio types).
    pub fn publish(&self, event: Event) -> Result<usize, PublishError> {
        match self.tx.send(event) {
            Ok(n) => Ok(n),
            Err(_) => Err(PublishError::NoSubscribers),
        }
    }

    /// Convenience helper: build a `Generic` event and publish it.
    pub fn publish_generic(
        &self,
        topic: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<usize, PublishError> {
        self.publish(Event::Generic {
            topic: topic.into(),
            payload,
        })
    }

    /// Subscribe to a set of topic prefixes.
    ///
    /// The returned [`Subscription`] wraps a `tokio::sync::broadcast::Receiver`
    /// with a topic-filter applied at receive time. Pass an empty slice
    /// to receive **all** events on the bus (useful for debugging and
    /// the operator-facing `tail_event_bus` MCP tool slated for Phase C).
    ///
    /// Topic matching is **prefix match**, not glob:
    ///
    /// - `sweep.issue.123` matches `sweep.issue.123.phase`
    /// - `sweep.issue` matches `sweep.issue.123.phase`, `sweep.issue.456.exited`, etc.
    /// - `sweep` matches every event whose topic starts with `sweep`
    /// - Empty string matches everything (treated identically to an empty filter set)
    #[must_use]
    pub fn subscribe<I, S>(&self, topics: I) -> Subscription
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let topics: HashSet<String> = topics.into_iter().map(Into::into).collect();
        let receive_all = topics.is_empty() || topics.contains("");
        Subscription {
            rx: self.tx.subscribe(),
            topics,
            receive_all,
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Publish errors
// ============================================================================

/// Error type returned by [`EventBus::publish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishError {
    /// No active subscribers — the event was dropped. This is *not* an
    /// error condition for fire-and-forget callers; the daemon-side
    /// publish sites log at `debug` level and continue.
    NoSubscribers,
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSubscribers => write!(f, "no active subscribers"),
        }
    }
}

impl std::error::Error for PublishError {}

// ============================================================================
// Subscription
// ============================================================================

/// A topic-filtered subscription on the event bus.
///
/// Wraps a `tokio::sync::broadcast::Receiver` and filters events by
/// topic prefix at receive time. The subscription's overflow behaviour
/// matches the underlying broadcast channel: when the receiver lags
/// behind the publisher beyond the channel's capacity, [`recv`] returns
/// a synthetic [`Event::TopicLag`] event and resumes from the current
/// channel head (the operator-visible signal that some events were
/// missed, matching tokio's `Lagged` semantics).
///
/// [`recv`]: Subscription::recv
#[derive(Debug)]
pub struct Subscription {
    rx: broadcast::Receiver<Event>,
    topics: HashSet<String>,
    /// When true, the filter is a no-op and every event is delivered.
    receive_all: bool,
}

impl Subscription {
    /// Receive the next event matching this subscription's topics.
    ///
    /// Returns:
    ///
    /// - `Ok(event)` — a normal event or a synthetic [`Event::TopicLag`].
    /// - `Err(RecvError::Closed)` — the bus has been dropped.
    ///
    /// Internally this loops past non-matching events; only events whose
    /// topic prefix-matches one of this subscription's filters (or all
    /// events, when the filter set is empty) are returned to the caller.
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        loop {
            match self.rx.recv().await {
                Ok(event) => {
                    if self.matches(&event) {
                        return Ok(event);
                    }
                    // Otherwise loop and consume the next event.
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(RecvError::Closed);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Slow-subscriber overflow. Emit a topic_lag sentinel
                    // so the subscriber gets a clear signal that some
                    // events were dropped — pass-through, don't silently
                    // skip. Matches tokio's broadcast Lagged semantics.
                    return Ok(Event::TopicLag { skipped });
                }
            }
        }
    }

    /// Try to receive a single event without blocking. Returns
    /// `Err(RecvError::Empty)` when no events are available. Useful for
    /// tests and polling-style consumers.
    pub fn try_recv(&mut self) -> Result<Event, RecvError> {
        loop {
            match self.rx.try_recv() {
                Ok(event) => {
                    if self.matches(&event) {
                        return Ok(event);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => return Err(RecvError::Empty),
                Err(broadcast::error::TryRecvError::Closed) => return Err(RecvError::Closed),
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    return Ok(Event::TopicLag { skipped });
                }
            }
        }
    }

    /// Returns the current topic filter set (read-only). Empty means
    /// "receive all events".
    #[must_use]
    pub fn topics(&self) -> &HashSet<String> {
        &self.topics
    }

    /// Topic-prefix match against the subscription's filter set.
    ///
    /// Returns `true` when the event's topic starts with at least one of
    /// the configured topic-prefix strings. The empty prefix and empty
    /// filter set both match every topic.
    fn matches(&self, event: &Event) -> bool {
        if self.receive_all {
            return true;
        }
        let topic = event.topic();
        self.topics
            .iter()
            .any(|prefix| topic_matches(&topic, prefix))
    }
}

/// Errors returned by [`Subscription::recv`] and [`Subscription::try_recv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The bus has been dropped — no further events will arrive.
    Closed,
    /// `try_recv` only: no event is currently buffered.
    Empty,
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "event bus closed"),
            Self::Empty => write!(f, "no events available"),
        }
    }
}

impl std::error::Error for RecvError {}

// ============================================================================
// Topic-prefix matching helper
// ============================================================================

/// Prefix-match `topic` against `prefix`.
///
/// Returns true when:
///
/// - `prefix` is empty (matches everything), OR
/// - `topic == prefix` (exact match), OR
/// - `topic.starts_with(format!("{prefix}."))` — segment-aligned prefix.
///
/// The segment-alignment rule prevents `sweep.iss` from matching
/// `sweep.issue.123` (which would be a glob-style match but not the
/// intended dot-segmented prefix). Operators specifying prefixes like
/// `sweep.issue` get all `sweep.issue.*` events without accidentally
/// including a hypothetical `sweep.issuetype.foo` event.
#[must_use]
pub fn topic_matches(topic: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if topic == prefix {
        return true;
    }
    // Segment-aligned: topic must continue with a '.' after the prefix.
    if let Some(rest) = topic.strip_prefix(prefix) {
        return rest.starts_with('.');
    }
    false
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::{Event, SweepKind};
    use serde_json::json;

    fn phase_event(issue: u32, phase: &str) -> Event {
        Event::SweepPhase {
            issue,
            phase: phase.to_string(),
            pr_number: None,
            repo: None,
        }
    }

    // ------------------------------------------------------------------
    // Topic-prefix matching
    // ------------------------------------------------------------------

    #[test]
    fn topic_matches_empty_prefix_matches_all() {
        assert!(topic_matches("anything.at.all", ""));
        assert!(topic_matches("", ""));
    }

    #[test]
    fn topic_matches_exact_match() {
        assert!(topic_matches("sweep.issue.123.phase", "sweep.issue.123.phase"));
    }

    #[test]
    fn topic_matches_segment_prefix() {
        // sweep.issue prefix should match sweep.issue.123.phase
        assert!(topic_matches("sweep.issue.123.phase", "sweep.issue"));
        // sweep prefix should match anything under sweep.*
        assert!(topic_matches("sweep.global.dispatch", "sweep"));
        assert!(topic_matches("sweep.issue.42.exited", "sweep"));
    }

    #[test]
    fn topic_matches_rejects_non_segment_prefix() {
        // 'sweep.iss' should NOT match 'sweep.issue.123' — the prefix is
        // not segment-aligned. This guards against accidental matches on
        // half-word prefixes.
        assert!(!topic_matches("sweep.issue.123", "sweep.iss"));
        // A prefix that is a different sibling segment doesn't match.
        assert!(!topic_matches("sweep.global.dispatch", "sweep.issue"));
    }

    #[test]
    fn topic_matches_rejects_unrelated() {
        assert!(!topic_matches("sweep.issue.123.phase", "other.topic"));
    }

    // ------------------------------------------------------------------
    // EventBus construction
    // ------------------------------------------------------------------

    #[test]
    fn bus_default_capacity_is_1024() {
        let bus = EventBus::new();
        assert_eq!(bus.capacity(), DEFAULT_CAPACITY);
        assert_eq!(bus.capacity(), 1024);
    }

    #[test]
    fn bus_custom_capacity() {
        let bus = EventBus::with_capacity(64);
        assert_eq!(bus.capacity(), 64);
    }

    #[test]
    fn bus_receiver_count_tracks_subscriptions() {
        let bus = EventBus::new();
        assert_eq!(bus.receiver_count(), 0);
        let _sub_a = bus.subscribe::<[&str; 0], &str>([]);
        assert_eq!(bus.receiver_count(), 1);
        let _sub_b = bus.subscribe(["sweep.issue"]);
        assert_eq!(bus.receiver_count(), 2);
        drop(_sub_a);
        assert_eq!(bus.receiver_count(), 1);
    }

    // ------------------------------------------------------------------
    // Publish/subscribe routing
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn publish_routes_to_matching_subscriber() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe(["sweep.issue.123"]);
        let _ignored = bus.publish(phase_event(123, "builder")).unwrap();

        let event = sub.recv().await.unwrap();
        match event {
            Event::SweepPhase {
                issue,
                phase,
                pr_number: _,
                repo: _,
            } => {
                assert_eq!(issue, 123);
                assert_eq!(phase, "builder");
            }
            other => panic!("expected SweepPhase, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_filters_non_matching_topics() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe(["sweep.issue.999"]);

        // Two events: 123 should be filtered out, 999 should arrive.
        let _ = bus.publish(phase_event(123, "builder"));
        let _ = bus.publish(phase_event(999, "judge"));

        let event = sub.recv().await.unwrap();
        match event {
            Event::SweepPhase { issue, .. } => assert_eq!(issue, 999),
            other => panic!("expected SweepPhase for 999, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_topic_set_receives_all() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let _ = bus.publish(phase_event(1, "builder"));
        let _ = bus.publish_generic(
            "sweep.global.dispatch",
            json!({"sweep_id": "abc", "kind": "Issue(7)"}),
        );

        let first = sub.recv().await.unwrap();
        let second = sub.recv().await.unwrap();
        match (first, second) {
            (Event::SweepPhase { .. }, Event::Generic { .. })
            | (Event::Generic { .. }, Event::SweepPhase { .. }) => {}
            other => panic!("unexpected event pair: {other:?}"),
        }
    }

    #[tokio::test]
    async fn segment_prefix_matches_all_under_namespace() {
        let bus = EventBus::new();
        // Subscribe to all sweep.issue.* events
        let mut sub = bus.subscribe(["sweep.issue"]);

        let _ = bus.publish(phase_event(42, "builder"));
        let _ = bus.publish(Event::SweepExited {
            issue: 42,
            exit_code: Some(0),
            duration_sec: 12,
            no_progress: false,
            death_class: None,
            repo: None,
        });
        let _ = bus.publish_generic("sweep.global.dispatch", json!({"sweep_id": "x"}));

        // Should receive both sweep.issue events but NOT the global one.
        let e1 = sub.recv().await.unwrap();
        assert!(e1.topic().starts_with("sweep.issue."));
        let e2 = sub.recv().await.unwrap();
        assert!(e2.topic().starts_with("sweep.issue."));

        // try_recv must now return Empty (the global event was filtered).
        let empty = sub.try_recv();
        assert!(matches!(empty, Err(RecvError::Empty)));
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_returns_error() {
        let bus = EventBus::new();
        let result = bus.publish(phase_event(1, "builder"));
        assert!(matches!(result, Err(PublishError::NoSubscribers)));
    }

    // ------------------------------------------------------------------
    // Slow-subscriber overflow -> topic_lag
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn slow_subscriber_receives_topic_lag_on_overflow() {
        // Tiny bus (capacity 4) so we can overflow it cheaply.
        let bus = EventBus::with_capacity(4);
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        // Publish far more events than the channel can hold without
        // letting `sub` consume — this guarantees a Lagged condition.
        for i in 0..20 {
            let _ =
                bus.publish_generic(format!("sweep.issue.{i}.phase"), json!({"phase": "builder"}));
        }

        // The first recv() should surface a TopicLag sentinel because
        // the receiver is so far behind the publisher.
        let event = sub.recv().await.unwrap();
        match event {
            Event::TopicLag { skipped } => {
                assert!(skipped > 0, "topic_lag should report skipped > 0");
            }
            other => panic!("expected TopicLag, got: {other:?}"),
        }

        // After the lag signal, the subscription should resume at the
        // current channel head; the next recv() returns a normal event
        // (one of the post-overflow events still in the buffer).
        let next = sub.recv().await.unwrap();
        match next {
            Event::Generic { topic, .. } => {
                assert!(topic.starts_with("sweep.issue."));
            }
            other => panic!("expected Generic post-lag event, got: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Topic-router HashMap behavior (multiple subscribers, different filters)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn multiple_subscribers_with_distinct_filters() {
        let bus = EventBus::new();
        // Subscriber A watches only one issue.
        let mut sub_a = bus.subscribe(["sweep.issue.111"]);
        // Subscriber B watches global events.
        let mut sub_b = bus.subscribe(["sweep.global"]);
        // Subscriber C watches everything sweep.*.
        let mut sub_c = bus.subscribe(["sweep"]);

        let _ = bus.publish(phase_event(111, "builder"));
        let _ = bus.publish_generic("sweep.global.dispatch", json!({"sweep_id": "abc"}));
        let _ = bus.publish(phase_event(222, "judge"));

        // A: only issue 111
        let a1 = sub_a.recv().await.unwrap();
        match a1 {
            Event::SweepPhase { issue, .. } => assert_eq!(issue, 111),
            other => panic!("sub_a expected SweepPhase(111), got: {other:?}"),
        }
        assert!(matches!(sub_a.try_recv(), Err(RecvError::Empty)));

        // B: only the global dispatch
        let b1 = sub_b.recv().await.unwrap();
        match b1 {
            Event::Generic { topic, .. } => assert_eq!(topic, "sweep.global.dispatch"),
            other => panic!("sub_b expected Generic sweep.global.dispatch, got: {other:?}"),
        }
        assert!(matches!(sub_b.try_recv(), Err(RecvError::Empty)));

        // C: all three events
        for _ in 0..3 {
            let _ = sub_c.recv().await.unwrap();
        }
        assert!(matches!(sub_c.try_recv(), Err(RecvError::Empty)));
    }

    // ------------------------------------------------------------------
    // Event helper coverage
    // ------------------------------------------------------------------

    #[test]
    fn sweep_kind_helper_produces_expected_topic_for_dispatch() {
        // The Event::SweepGlobalDispatch helper must produce
        // `sweep.global.dispatch` regardless of SweepKind contents.
        let ev = Event::SweepGlobalDispatch {
            sweep_id: "sweep-issue-42-1".to_string(),
            kind: SweepKind::Issue(42),
            runtime: None,
            runtime_source: None,
            repo: None,
        };
        assert_eq!(ev.topic(), "sweep.global.dispatch");
    }

    /// Issue #4494: refused (capability-rejected) work has its own topic, and
    /// the payload must serialize as a structured, secret-free rejection
    /// reason that a prefix subscriber on `sweep.global.*` receives.
    #[tokio::test]
    async fn runtime_rejected_topic_serializes_structured_and_secret_free() {
        let ev = Event::SweepGlobalRuntimeRejected {
            kind: SweepKind::Issue(4494),
            role: "sweep-lifecycle".to_string(),
            runtime: "codex".to_string(),
            runtime_source: crate::types::RuntimeSource::DefaultConfig,
            unmet_capabilities: vec!["worktreeIsolation".to_string()],
            reason: "unmet capabilities: worktreeIsolation".to_string(),
            repo: None,
        };
        assert_eq!(ev.topic(), "sweep.global.runtime_rejected");

        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"role\":\"sweep-lifecycle\""), "{json}");
        assert!(json.contains("\"runtime_source\":\"default-config\""), "{json}");
        assert!(json.contains("\"unmet_capabilities\":[\"worktreeIsolation\"]"), "{json}");
        // Secret-free: no account/token/credential material rides this event.
        for forbidden in ["token", "oauth", "credential", "api_key"] {
            assert!(!json.to_ascii_lowercase().contains(forbidden), "{json}");
        }
        // Round-trips (subscribers deserialize the same shape).
        let decoded: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.topic(), "sweep.global.runtime_rejected");

        // A `sweep.global.*` prefix subscriber receives it alongside dispatch.
        let bus = EventBus::with_capacity(8);
        let mut sub = bus.subscribe(["sweep.global"]);
        bus.publish(ev).unwrap();
        let received = sub.recv().await.unwrap();
        assert_eq!(received.topic(), "sweep.global.runtime_rejected");
    }
}
