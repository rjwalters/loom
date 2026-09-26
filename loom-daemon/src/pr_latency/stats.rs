//! The summary applied to a set of segment samples (Issue #8923).
//!
//! Small on purpose. The one design rule it exists to enforce: **an empty
//! sample set has no percentiles**, so `None` reaches the renderer and prints
//! as `—`. Returning `0` for "nothing was measured" is the failure mode the
//! cycle-time question set calls out as "absent vs. zero", and it is worse here
//! than there, because a `0` in a latency table reads as *instant*.

use serde::Serialize;

/// Nearest-rank percentile/max summary of a set of durations, in seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Distribution {
    /// How many PRs contributed a sample. **Read this first**: every other
    /// field is meaningless at `n = 0` and absent rather than zero.
    pub n: usize,
    pub p50_secs: Option<i64>,
    pub p90_secs: Option<i64>,
    pub max_secs: Option<i64>,
    /// Summed seconds — the segment's total contribution to the queue, which is
    /// what a "where does the time go" reading needs; a long tail of one and a
    /// broad median are different problems with the same p50.
    pub total_secs: Option<i64>,
}

impl Distribution {
    /// Summarize `samples`. Order is irrelevant; the input is sorted here.
    ///
    /// Percentiles use **nearest-rank on the sorted samples** (index
    /// `ceil(p·n) − 1`), not interpolation: every value reported is a real
    /// observed PR, so a number in the table can always be traced back to one.
    pub fn of(mut samples: Vec<i64>) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_unstable();
        let n = samples.len();
        Self {
            n,
            p50_secs: Some(Self::nearest_rank(&samples, 50)),
            p90_secs: Some(Self::nearest_rank(&samples, 90)),
            max_secs: samples.last().copied(),
            total_secs: Some(samples.iter().sum()),
        }
    }

    /// `sorted` must be non-empty and ascending.
    fn nearest_rank(sorted: &[i64], pct: usize) -> i64 {
        let n = sorted.len();
        // ceil(pct * n / 100), clamped into 1..=n, then to a 0-based index.
        let rank = (pct * n).div_ceil(100).clamp(1, n);
        sorted[rank - 1]
    }

    /// True when nothing was measured — the caller should print an absence, not
    /// a value.
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
}

/// Seconds as a compact hours string (`"68.5h"`), or `"—"` when absent.
///
/// Hours because every figure in this problem space is hours: the queues this
/// measures run from minutes to a week, and `246600s` is unreadable at a glance.
pub fn hours(secs: Option<i64>) -> String {
    match secs {
        Some(s) => format!("{:.1}h", s as f64 / 3600.0),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_percentiles_not_zero_ones() {
        let d = Distribution::of(Vec::new());
        assert_eq!(d.n, 0);
        assert!(d.is_empty());
        assert_eq!(d.p50_secs, None);
        assert_eq!(d.p90_secs, None);
        assert_eq!(d.max_secs, None);
        assert_eq!(d.total_secs, None);
        assert_eq!(hours(d.p50_secs), "—");
    }

    #[test]
    fn nearest_rank_reports_observed_values_only() {
        let d = Distribution::of(vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
        assert_eq!(d.n, 10);
        // ceil(0.5*10)=5 => index 4 => 50. Never 55 (no interpolation).
        assert_eq!(d.p50_secs, Some(50));
        // ceil(0.9*10)=9 => index 8 => 90.
        assert_eq!(d.p90_secs, Some(90));
        assert_eq!(d.max_secs, Some(100));
        assert_eq!(d.total_secs, Some(550));
    }

    #[test]
    fn single_sample_is_its_own_every_statistic() {
        let d = Distribution::of(vec![7200]);
        assert_eq!((d.p50_secs, d.p90_secs, d.max_secs), (Some(7200), Some(7200), Some(7200)));
        assert_eq!(hours(d.p50_secs), "2.0h");
    }

    #[test]
    fn a_measured_zero_is_still_a_value() {
        // A PR approved in the same event batch that requested review has a
        // genuine 0-second segment. That must not read as "unmeasured".
        let d = Distribution::of(vec![0, 0, 3600]);
        assert_eq!(d.n, 3);
        assert_eq!(d.p50_secs, Some(0));
        assert_eq!(hours(d.p50_secs), "0.0h");
        assert!(!d.is_empty());
    }
}
