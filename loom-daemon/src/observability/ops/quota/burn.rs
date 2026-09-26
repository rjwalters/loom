//! The shared incremental burn seam (Issues #8857, #8930).
//!
//! Every subscription store is read the same way: a [`BurnSource`] decodes
//! only what was written since its previous poll into timestamped
//! [`BurnEvent`]s, and the [`BurnLedger`] assigns each event to the window its
//! timestamp falls in. Windows end [`MESSAGE_SETTLE_LAG_SECS`] before the
//! sample and abut, so an event is counted exactly once:
//!
//! - an event at or before the window end is counted in this window;
//! - a later one (inside the settle lag) is held for the next window;
//! - one that arrives late (written after its window closed) is counted in
//!   the current window rather than lost, unless it is older than
//!   [`LATE_GRACE_SECS`]: that is history (a resumed session copying old
//!   records, or a file first seen after a restart), and it is never counted.
//!
//! The first sample only anchors the window: sources read their stores to the
//! end, emitting nothing earlier than the anchor, so history is never replayed
//! as a burst.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::telemetry::ops::{MetricName, MetricPoint};

/// How far behind now the burn window ends, so a message's streamed chunks
/// are on disk before it is counted. (OpenCode steps are counted by row
/// identity, so a late completion commit is caught regardless — #8966.)
pub const MESSAGE_SETTLE_LAG_SECS: i64 = 60;

/// How late an event may be read and still be counted, in the current window.
/// Anything older is history. Also bounds which files are tracked.
pub const LATE_GRACE_SECS: i64 = 3600;

/// Token and request totals for one model over one window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelBurn {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub requests: i64,
}

impl ModelBurn {
    /// Whether nothing was spent (an errored request, a duplicate event).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_write == 0
            && self.requests == 0
    }

    fn add(&mut self, other: &ModelBurn) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.requests = self.requests.saturating_add(other.requests);
    }
}

/// Usage a store recorded at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurnEvent {
    /// The `provider` label (`claude`, `codex`, `zai`, `kimi`, …).
    pub provider: String,
    pub model: String,
    pub at: DateTime<Utc>,
    pub usage: ModelBurn,
}

/// Which decoded events a poll may emit: those at or after `not_before`.
/// Older ones still advance the source's state (message ids, counters).
#[derive(Debug, Clone, Copy)]
pub struct Emit {
    pub not_before: DateTime<Utc>,
    pub now: DateTime<Utc>,
}

impl Emit {
    #[must_use]
    pub fn accepts(self, at: DateTime<Utc>) -> bool {
        at >= self.not_before
    }

    /// Files not written since this are not tracked. One settle lag earlier
    /// than the next poll's `not_before` (the previous window end minus the
    /// grace), so every record of a dropped file is history if the file is
    /// written again and read from the start.
    #[must_use]
    pub fn active_since(self) -> DateTime<Utc> {
        self.now - Duration::seconds(LATE_GRACE_SECS + MESSAGE_SETTLE_LAG_SECS)
    }
}

/// One store read incrementally. A poll reads what was written since the
/// previous poll and pushes the non-empty events `emit` accepts.
pub trait BurnSource: Send {
    fn poll(&mut self, emit: Emit, out: &mut Vec<BurnEvent>);
}

/// Events read but not yet counted, because they fall after the window end.
#[derive(Debug, Default)]
pub struct BurnLedger {
    pending: Vec<BurnEvent>,
}

/// Per-`(provider, model)` totals for one window.
pub type WindowBurn = BTreeMap<(String, String), ModelBurn>;

impl BurnLedger {
    pub fn push(&mut self, events: impl IntoIterator<Item = BurnEvent>) {
        self.pending.extend(events);
    }

    /// Totals of the pending events at or before `end`. Later events stay
    /// pending, unless they are more than [`LATE_GRACE_SECS`] ahead (a clock
    /// that ran ahead must not hold usage back forever).
    pub fn drain_through(&mut self, end: DateTime<Utc>) -> WindowBurn {
        let horizon = end + Duration::seconds(LATE_GRACE_SECS);
        let mut totals = WindowBurn::new();
        self.pending.retain(|event| {
            if event.at > end && event.at <= horizon {
                return true;
            }
            totals
                .entry((event.provider.clone(), event.model.clone()))
                .or_default()
                .add(&event.usage);
            false
        });
        totals
    }
}

/// Delta-counter points for per-provider/model burn. Zero counters are not
/// emitted.
#[must_use]
pub fn burn_points(burn: &WindowBurn) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    for ((provider, model), totals) in burn {
        for (name, value) in [
            (MetricName::LlmTokensInput, totals.input),
            (MetricName::LlmTokensOutput, totals.output),
            (MetricName::LlmTokensCacheRead, totals.cache_read),
            (MetricName::LlmTokensCacheWrite, totals.cache_write),
            (MetricName::LlmRequests, totals.requests),
        ] {
            if value > 0 {
                points.push(
                    MetricPoint::int(name, value)
                        .label("provider", provider.as_str())
                        .label("model", model.as_str()),
                );
            }
        }
    }
    points
}

/// Every store's cursor plus the window, across samples.
pub struct Burn {
    sources: Vec<Box<dyn BurnSource>>,
    ledger: BurnLedger,
    /// End of the last burn window; `None` until the first sample anchors it.
    until: Option<DateTime<Utc>>,
}

/// One closed window: `(start, end]` and what was spent in it.
pub type BurnWindow = (DateTime<Utc>, DateTime<Utc>, WindowBurn);

impl Burn {
    #[must_use]
    pub fn new(sources: Vec<Box<dyn BurnSource>>) -> Self {
        Self {
            sources,
            ledger: BurnLedger::default(),
            until: None,
        }
    }

    /// This host's stores: Claude, Codex, OpenCode and Kimi.
    #[must_use]
    pub fn host() -> Self {
        Self::new(vec![
            Box::new(super::claude::ClaudeSource::default()),
            Box::new(super::codex::CodexSource::default()),
            Box::new(super::opencode::OpencodeSource::default()),
            Box::new(super::kimi::KimiSource::default()),
        ])
    }

    fn poll(&mut self, not_before: DateTime<Utc>, now: DateTime<Utc>) {
        let emit = Emit { not_before, now };
        let mut events = Vec::new();
        for source in &mut self.sources {
            source.poll(emit, &mut events);
        }
        self.ledger
            .push(events.into_iter().filter(|e| !e.usage.is_empty()));
    }

    /// Advance the window to `now - lag`: poll every store and return the
    /// closed window, or `None` on the anchoring first sample (or when the
    /// clock went backwards, which polls nothing).
    pub fn sample(&mut self, now: DateTime<Utc>) -> Option<BurnWindow> {
        let end = now - Duration::seconds(MESSAGE_SETTLE_LAG_SECS);
        let Some(start) = self.until else {
            self.poll(end, now);
            self.until = Some(end);
            return None;
        };
        if end <= start {
            return None;
        }
        self.poll(start - Duration::seconds(LATE_GRACE_SECS), now);
        self.until = Some(end);
        Some((start, end, self.ledger.drain_through(end)))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "burn_tests.rs"]
mod tests;
