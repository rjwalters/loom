//! Per-PR review and CI signals for [`super::fleet_state::fleet_state`]
//! (#10197), from the `review` / `check_run` rows of the raw event cache.
//!
//! Pure and fed only rows that survived `fleet_state`'s `< t` line, in
//! canonical order, so it inherits the leak-freedom and determinism of the
//! replay. Settle markers (no `label`) say only that a PR's listing was read.
//!
//! - **CI** follows [`super::friction::ci_status`], the reader behind the
//!   logged `pr_ci_status`: per check name, the latest run started before `t`
//!   (GitHub's `filter=latest`); `failing` if any of those had completed
//!   before `t` with a failing conclusion
//!   ([`super::friction::FAILING`]), else `pending` if any had not completed
//!   by `t`, else `passing`. With no run started before `t` the answer is
//!   unknown (`None`), never `none`: the cache holds only the last head's
//!   runs, so "no runs yet" cannot be told from "an earlier head's runs".
//! - **Review** is the state of the latest `approved` / `changes_requested`
//!   review submitted before `t`. Known leak, documented in eta.md: the
//!   listing serves a review's *current* state, so an approval dismissed after
//!   `t` reads as `dismissed` (ignored here) from its submission on — it can
//!   only under-report an approval.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::fleet_events::{EventKind, RawEvent};
use super::fleet_events_reviews::{split_check_label, STARTED};

#[derive(Default)]
struct PrRows {
    /// Per check name: the latest start, `(started_at, run id)`.
    latest_start: BTreeMap<String, (DateTime<Utc>, u64)>,
    /// Per run id: its conclusion, once completed.
    conclusion: BTreeMap<u64, String>,
    /// The latest decisive review state.
    review: Option<&'static str>,
}

/// What the review and check-run rows before `t` say, per PR.
#[derive(Default)]
pub struct PrSignals {
    reviews_read: bool,
    checks_read: bool,
    prs: BTreeMap<u32, PrRows>,
}

impl PrSignals {
    /// Take one row (call in canonical order). Other kinds are ignored.
    pub fn observe(&mut self, e: &RawEvent) {
        match e.kind {
            EventKind::Review => self.reviews_read = true,
            EventKind::CheckRun => self.checks_read = true,
            _ => return,
        }
        let Some(label) = e.label.as_deref() else {
            return;
        };
        let pr = self.prs.entry(e.item).or_default();
        if e.kind == EventKind::Review {
            match label {
                "approved" => pr.review = Some("approved"),
                "changes_requested" => pr.review = Some("changes_requested"),
                _ => {}
            }
            return;
        }
        let Some((state, name)) = split_check_label(label) else {
            return;
        };
        if state == STARTED {
            let start = (e.event_time, e.seq);
            let latest = pr.latest_start.entry(name.to_string()).or_insert(start);
            if start > *latest {
                *latest = start;
            }
        } else {
            pr.conclusion.insert(e.seq, state.to_string());
        }
    }

    /// Whether any review row (a settle marker included) precedes `t`.
    #[must_use]
    pub fn reviews_read(&self) -> bool {
        self.reviews_read
    }

    /// Whether any check-run row (a settle marker included) precedes `t`.
    #[must_use]
    pub fn checks_read(&self) -> bool {
        self.checks_read
    }

    /// PR `pr`'s CI at `t`: `passing`, `failing` or `pending`; `None` when no
    /// run of it had started.
    #[must_use]
    pub fn ci(&self, pr: u32) -> Option<&'static str> {
        let rows = self.prs.get(&pr)?;
        if rows.latest_start.is_empty() {
            return None;
        }
        let ends: Vec<Option<&String>> = rows
            .latest_start
            .values()
            .map(|(_, run)| rows.conclusion.get(run))
            .collect();
        if ends
            .iter()
            .flatten()
            .any(|c| super::friction::FAILING.contains(&c.as_str()))
        {
            Some("failing")
        } else if ends.iter().any(Option::is_none) {
            Some("pending")
        } else {
            Some("passing")
        }
    }

    /// PR `pr`'s latest decisive review state at `t`.
    #[must_use]
    pub fn review(&self, pr: u32) -> Option<&'static str> {
        self.prs.get(&pr)?.review
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::eta::fleet_events::{ItemKind, SOURCE_FORGE};

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    fn row(kind: EventKind, label: &str, secs: i64, seq: u64) -> RawEvent {
        RawEvent::new(
            "o/r",
            7,
            ItemKind::Pr,
            kind,
            Some(label.to_string()),
            t(secs),
            SOURCE_FORGE,
            seq,
            t(0),
        )
    }

    fn signals(rows: &[RawEvent]) -> PrSignals {
        let mut s = PrSignals::default();
        for r in rows {
            s.observe(r);
        }
        s
    }

    #[test]
    fn the_latest_run_per_name_decides_like_the_live_reader() {
        use EventKind::CheckRun;
        let failed_then_rerun = [
            row(CheckRun, "started:test", 10, 1),
            row(CheckRun, "failure:test", 20, 1),
            row(CheckRun, "started:test", 30, 2),
            row(CheckRun, "started:lint", 30, 3),
            row(CheckRun, "success:lint", 40, 3),
        ];
        assert_eq!(signals(&failed_then_rerun[..2]).ci(7), Some("failing"));
        // The rerun supersedes the failure and is still running.
        assert_eq!(signals(&failed_then_rerun).ci(7), Some("pending"));
        let mut done = failed_then_rerun.to_vec();
        done.push(row(CheckRun, "success:test", 50, 2));
        assert_eq!(signals(&done).ci(7), Some("passing"));
        // Any failing latest run fails the head, whatever else runs.
        let mut cancelled = failed_then_rerun.to_vec();
        cancelled.push(row(CheckRun, "cancelled:test", 50, 2));
        assert_eq!(signals(&cancelled).ci(7), Some("failing"));
        // A neutral or skipped conclusion is not a failure.
        assert_eq!(
            signals(&[
                row(CheckRun, "started:x", 1, 9),
                row(CheckRun, "skipped:x", 2, 9)
            ])
            .ci(7),
            Some("passing")
        );
    }

    #[test]
    fn no_started_run_is_unknown_and_markers_only_mark_reading() {
        let mut marker = row(EventKind::CheckRun, "x", 5, 0);
        marker.label = None;
        let s = signals(&[marker]);
        assert!(s.checks_read());
        assert!(!s.reviews_read());
        assert_eq!(s.ci(7), None);
    }

    #[test]
    fn the_latest_decisive_review_wins_and_comments_do_not_count() {
        use EventKind::Review;
        let rows = [
            row(Review, "changes_requested", 10, 1),
            row(Review, "approved", 20, 2),
            row(Review, "commented", 30, 3),
            row(Review, "dismissed", 40, 4),
        ];
        assert_eq!(signals(&rows[..1]).review(7), Some("changes_requested"));
        assert_eq!(signals(&rows).review(7), Some("approved"));
        assert_eq!(signals(&rows[2..]).review(7), None);
    }
}
