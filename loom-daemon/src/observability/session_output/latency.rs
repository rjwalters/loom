//! Producer-lag measurement for `session.output` (#9764).
//!
//! # What is being measured, and what is not
//!
//! Source-event-to-queryable latency has two halves, and this module owns
//! exactly one of them:
//!
//! | half | measured as | owned by |
//! |---|---|---|
//! | producer lag | `observed_at - source_at` | **here** |
//! | export + gateway + backend | `backend_ingest - observed_at` | the consumer's query |
//!
//! The split is deliberate and is why the OTLP mapping refuses to collapse the
//! two timestamps (`observability/otlp/mapping/session_output.rs`): a consumer
//! that has both can attribute a latency breach to the producer or to the
//! pipeline, and can tell a quiet run from a stalled export. Collapsing them
//! would leave only a single number that cannot distinguish either case. The
//! consumer-side query for the second half is in
//! `defaults/docs/session-output.md`.
//!
//! # Why a sample can be refused
//!
//! `observed_at - source_at` is a *latency* only when the producer was already
//! watching at `source_at`. A transcript that existed before this producer
//! attached is replayed from its retained tail
//! ([`super::claude::ATTACH_TAIL_EVENTS`]), and those events' timestamps can be
//! arbitrarily old — a daemon restart mid-session routinely yields source times
//! hours in the past. Subtracting them produces the transcript's **age**, not
//! the pipeline's lag, and a single such sample would dominate p95 and report a
//! stall that never occurred.
//!
//! [`LagWindow::observe`] therefore admits a sample only when `source_at` is at
//! or after the moment the producer began watching the run, and counts every
//! refusal in [`LagStats::historical_excluded`] so the exclusion is visible on
//! the wire instead of looking like missing data.

use chrono::{DateTime, Utc};

use crate::telemetry::kinds::session_output::LagStats;

/// Fresh samples retained for the percentile computation. A sliding window
/// rather than a lifetime accumulator: a live dashboard wants the lag *now*,
/// and an hour-old sample from a since-resolved stall should not keep a healthy
/// run's p95 red. Bounded, so a long run's memory does not grow.
pub const WINDOW: usize = 256;

/// A bounded sliding window of fresh producer-lag samples for one run.
#[derive(Debug, Default)]
pub struct LagWindow {
    /// Lag samples in arrival order, newest last, at most [`WINDOW`].
    samples: std::collections::VecDeque<i64>,
    /// Source events refused for predating the watch start. Cumulative for the
    /// life of the run — never decremented by a window eviction, so a consumer
    /// cannot see it fall back to zero and conclude the exclusions stopped.
    historical_excluded: u64,
}

impl LagWindow {
    /// Offer one source event's timing to the window.
    ///
    /// `watch_since` is when this producer began watching the run. Returns
    /// `true` when the sample was admitted as a latency measurement, `false`
    /// when it was refused as historical — the caller needs no branch on it,
    /// but the tests assert on it directly.
    pub fn observe(
        &mut self,
        source_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
        watch_since: DateTime<Utc>,
    ) -> bool {
        if source_at < watch_since {
            self.historical_excluded = self.historical_excluded.saturating_add(1);
            return false;
        }
        // Floored at 0 for the same reason `producer_lag_ms` floors: a source
        // clock ahead of ours is skew, not negative latency.
        let lag = (observed_at - source_at).num_milliseconds().max(0);
        if self.samples.len() >= WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(lag);
        true
    }

    /// The window's distribution, or `None` when no fresh sample has been
    /// observed yet.
    ///
    /// `None` is distinct from "p95 = 0": a run whose every event was
    /// historical has measured nothing, and reporting zero would assert a
    /// latency the producer never observed. `historical_excluded` still travels
    /// on a later status record once any fresh sample exists.
    #[must_use]
    pub fn snapshot(&self) -> Option<LagStats> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<i64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        Some(LagStats {
            samples: sorted.len() as u64,
            p50_ms: percentile(&sorted, 50),
            p95_ms: percentile(&sorted, 95),
            // `last` over a sorted non-empty vec; the window is the max's scope.
            max_ms: sorted.last().copied().unwrap_or(0),
            historical_excluded: self.historical_excluded,
        })
    }

    /// Fresh samples currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether no fresh sample has been retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Source events refused for predating the watch start.
    #[must_use]
    pub fn historical_excluded(&self) -> u64 {
        self.historical_excluded
    }
}

/// The `p`th percentile of an **already-sorted**, non-empty slice, by nearest
/// rank.
///
/// Nearest-rank rather than interpolating: a lag percentile is reported in
/// whole milliseconds against a budget stated in seconds, so interpolation
/// would add precision the measurement does not have, and nearest-rank always
/// returns a value some event actually experienced.
fn percentile(sorted: &[i64], p: usize) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    // ceil(p/100 * n) - 1, clamped — so p95 of 20 samples is the 19th.
    let rank = (p * sorted.len()).div_ceil(100);
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(second: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000 + second, 0).unwrap()
    }

    fn ms(base: i64, millis: u32) -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000 + base, millis * 1_000_000)
            .unwrap()
    }

    #[test]
    fn an_empty_window_measured_nothing_and_says_so() {
        let window = LagWindow::default();
        assert!(window.is_empty());
        assert_eq!(window.snapshot(), None, "zero would assert an unmeasured latency");
    }

    #[test]
    fn a_fresh_sample_is_the_source_to_read_delta() {
        let mut window = LagWindow::default();
        assert!(window.observe(ms(10, 0), ms(10, 250), at(0)));
        let stats = window.snapshot().unwrap();
        assert_eq!(stats.samples, 1);
        assert_eq!(stats.p50_ms, 250);
        assert_eq!(stats.p95_ms, 250);
        assert_eq!(stats.max_ms, 250);
        assert_eq!(stats.historical_excluded, 0);
    }

    #[test]
    fn a_historical_event_is_excluded_rather_than_reported_as_a_stall() {
        let mut window = LagWindow::default();
        // A transcript line written an hour before the producer attached: the
        // delta is the file's age, not the pipeline's lag.
        assert!(!window.observe(at(-3_600), at(0), at(0)));
        assert_eq!(window.snapshot(), None, "an age must not become a latency");
        assert_eq!(window.historical_excluded(), 1);

        // One genuinely fresh sample, and the window reports only that — the
        // 3600 s outlier is nowhere in the distribution.
        assert!(window.observe(ms(1, 0), ms(1, 100), at(0)));
        let stats = window.snapshot().unwrap();
        assert_eq!(stats.samples, 1);
        assert_eq!(stats.max_ms, 100);
        assert!(stats.p95_ms < 1_000, "{stats:?} was polluted by the backlog");
        assert_eq!(stats.historical_excluded, 1, "the exclusion stays visible");
    }

    #[test]
    fn an_event_exactly_at_the_watch_start_is_fresh() {
        // The boundary is inclusive: an event written in the same instant the
        // producer began watching was not missed, and excluding it would drop
        // the first sample of every run.
        let mut window = LagWindow::default();
        assert!(window.observe(at(0), ms(0, 5), at(0)));
        assert_eq!(window.snapshot().unwrap().samples, 1);
    }

    #[test]
    fn clock_skew_floors_at_zero_instead_of_reporting_negative_latency() {
        let mut window = LagWindow::default();
        assert!(window.observe(at(10), at(5), at(0)));
        assert_eq!(window.snapshot().unwrap().max_ms, 0);
    }

    #[test]
    fn percentiles_are_nearest_rank_over_a_known_distribution() {
        let mut window = LagWindow::default();
        // 1..=100 ms, so the expected ranks are exact and easy to read.
        for i in 1..=100 {
            assert!(window.observe(at(0), ms(0, i), at(0)));
        }
        let stats = window.snapshot().unwrap();
        assert_eq!(stats.samples, 100);
        assert_eq!(stats.p50_ms, 50);
        assert_eq!(stats.p95_ms, 95);
        assert_eq!(stats.max_ms, 100);
    }

    #[test]
    fn the_window_slides_and_stays_bounded() {
        let mut window = LagWindow::default();
        for _ in 0..(WINDOW * 3) {
            window.observe(ms(0, 0), ms(0, 7), at(0));
        }
        assert_eq!(window.len(), WINDOW, "the window must not grow without bound");
        assert_eq!(window.snapshot().unwrap().samples, WINDOW as u64);
    }

    #[test]
    fn a_resolved_stall_ages_out_of_the_window() {
        let mut window = LagWindow::default();
        // One very slow pass...
        window.observe(at(0), at(9), at(0));
        assert_eq!(window.snapshot().unwrap().max_ms, 9_000);
        // ...then a full window of healthy ones. The stall must not keep p95
        // red forever, which is the whole reason this is a sliding window.
        for _ in 0..WINDOW {
            window.observe(ms(0, 0), ms(0, 20), at(0));
        }
        let stats = window.snapshot().unwrap();
        assert_eq!(stats.max_ms, 20, "{stats:?} still carries the evicted stall");
    }

    #[test]
    fn exclusions_survive_window_eviction() {
        let mut window = LagWindow::default();
        window.observe(at(-99), at(0), at(0));
        for _ in 0..(WINDOW * 2) {
            window.observe(ms(0, 0), ms(0, 1), at(0));
        }
        // The sample ring rolled over many times; the exclusion count is
        // cumulative and must not have been reset with it.
        assert_eq!(window.snapshot().unwrap().historical_excluded, 1);
    }

    #[test]
    fn a_single_sample_is_its_own_every_percentile() {
        let mut window = LagWindow::default();
        window.observe(ms(0, 0), ms(0, 42), at(0));
        let stats = window.snapshot().unwrap();
        assert_eq!((stats.p50_ms, stats.p95_ms, stats.max_ms), (42, 42, 42));
    }

    #[test]
    fn percentile_of_an_empty_slice_is_zero_rather_than_a_panic() {
        // `snapshot` never calls it empty, but an index panic in a telemetry
        // path would take down a daemon over a dashboard nicety.
        assert_eq!(percentile(&[], 95), 0);
    }
}
