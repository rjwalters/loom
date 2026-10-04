//! Per-PR review and CI signals for [`super::fleet_state::fleet_state`]
//! (#10197), from the `review` / `check_run` / `head_commit` rows of the raw
//! event cache.
//!
//! Pure and fed only rows that survived `fleet_state`'s `< t` line, in
//! canonical order, so it inherits the leak-freedom and determinism of the
//! replay. Settle markers (no `label`) say only that a PR's listing was read.
//!
//! - **CI** is scoped to the PR's **head at `t`**: the commit of the latest
//!   `head_commit` row before `t`. Every check-run row names the commit its
//!   run ran on ([`super::fleet_events::RawEvent::commit`]); runs of any other
//!   commit are ignored, so an earlier head's verdict can never leak into a
//!   later head's (the log is append-only and keeps every head's runs). The
//!   answer is unknown (`None`), never guessed, when the cache cannot
//!   establish the head: no `head_commit` row before `t`, or a run before `t`
//!   on a commit no `head_commit` row before `t` names — a push newer than
//!   the recorded head (the head row is stamped at the PR's `updated_at`,
//!   which can trail the push, so runs may start first). A check-run row
//!   without a commit (written before the field existed) is ignored.
//! - Over the head's runs it follows [`super::friction::ci_status`], the
//!   reader behind the logged `pr_ci_status`: per check name, the latest run
//!   started before `t` (GitHub's `filter=latest`); `failing` if any of those
//!   had completed before `t` with a failing conclusion
//!   ([`super::friction::FAILING`]), else `pending` if any had not completed
//!   by `t`, else `passing`. With no run of the head started before `t` the
//!   answer is unknown (`None`), never `none` or the previous head's verdict.
//! - **Review** is the state of the latest `approved` / `changes_requested`
//!   review submitted before `t`. Known leak, documented in eta.md: the
//!   listing serves a review's *current* state, so an approval dismissed after
//!   `t` reads as `dismissed` (ignored here) from its submission on — it can
//!   only under-report an approval.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use super::fleet_events::{EventKind, RawEvent};
use super::fleet_events_reviews::{split_check_label, STARTED};

#[derive(Default)]
struct PrRows {
    /// Per commit, per check name: the latest start, `(started_at, run id)`.
    latest_start: BTreeMap<String, BTreeMap<String, (DateTime<Utc>, u64)>>,
    /// Every commit a `head_commit` row before `t` named.
    heads: BTreeSet<String>,
    /// The commit of the latest `head_commit` row before `t`.
    head: Option<String>,
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
    /// Take one row (call in canonical order). Kinds other than review, check
    /// run and head commit are ignored.
    pub fn observe(&mut self, e: &RawEvent) {
        match e.kind {
            EventKind::Review => self.reviews_read = true,
            EventKind::CheckRun => self.checks_read = true,
            EventKind::HeadCommit => {
                if let Some(sha) = &e.label {
                    let pr = self.prs.entry(e.item).or_default();
                    pr.heads.insert(sha.clone());
                    pr.head = Some(sha.clone());
                }
                return;
            }
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
        let (Some((state, name)), Some(commit)) = (split_check_label(label), &e.commit) else {
            return;
        };
        if state == STARTED {
            let start = (e.event_time, e.seq);
            let latest = pr
                .latest_start
                .entry(commit.clone())
                .or_default()
                .entry(name.to_string())
                .or_insert(start);
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

    /// PR `pr`'s CI at `t`, for its head at `t`: `passing`, `failing` or
    /// `pending`; `None` when the head is not established or no run of it had
    /// started.
    #[must_use]
    pub fn ci(&self, pr: u32) -> Option<&'static str> {
        let rows = self.prs.get(&pr)?;
        let head = rows.head.as_ref()?;
        // A run on a commit no head row names yet: a newer push, head unknown.
        if rows.latest_start.keys().any(|c| !rows.heads.contains(c)) {
            return None;
        }
        let runs = rows.latest_start.get(head)?;
        let ends: Vec<Option<&String>> = runs
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
        let commit = (kind == EventKind::CheckRun).then(|| "h1".to_string());
        on(kind, label, secs, seq, commit)
    }

    fn on(kind: EventKind, label: &str, secs: i64, seq: u64, commit: Option<String>) -> RawEvent {
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
        .with_commit(commit)
    }

    /// A check-run row of commit `sha`.
    fn run(sha: &str, label: &str, secs: i64, seq: u64) -> RawEvent {
        on(EventKind::CheckRun, label, secs, seq, Some(sha.to_string()))
    }

    fn head(sha: &str, secs: i64) -> RawEvent {
        on(EventKind::HeadCommit, sha, secs, 0, None)
    }

    /// The rows, behind a head row for `h1` at second 0 (all fixtures run on
    /// `h1` unless they say otherwise).
    fn signals(rows: &[RawEvent]) -> PrSignals {
        let mut s = PrSignals::default();
        s.observe(&head("h1", 0));
        for r in rows {
            s.observe(r);
        }
        s
    }

    /// Exactly `rows`, canonically ordered, as `fleet_state` feeds them.
    fn replay(rows: &[RawEvent], before: i64) -> PrSignals {
        let mut rows: Vec<&RawEvent> = rows.iter().filter(|r| r.event_time < t(before)).collect();
        rows.sort_by(|a, b| a.canonical_key().cmp(&b.canonical_key()));
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
        assert_eq!(s.ci(7), None, "a head with no run is unknown");
        assert!(!s.reviews_read());
    }

    #[test]
    fn without_a_head_row_ci_is_unknown_and_commitless_rows_are_ignored() {
        let mut s = PrSignals::default();
        s.observe(&run("h1", "started:test", 1, 1));
        assert_eq!(s.ci(7), None, "no head row: the head is not established");
        // A row written before `commit` existed cannot be attributed.
        let mut s = signals(&[]);
        s.observe(&on(EventKind::CheckRun, "started:test", 1, 1, None));
        assert_eq!(s.ci(7), None);
    }

    #[test]
    fn a_head_change_before_any_run_of_the_new_head_is_unknown() {
        let rows = [
            head("a", 0),
            run("a", "started:test", 10, 1),
            run("a", "failure:test", 20, 1),
            head("b", 30),
            run("b", "started:test", 50, 2),
        ];
        assert_eq!(replay(&rows, 30).ci(7), Some("failing"));
        // Head B is recorded, none of its runs has started: not A's failure.
        assert_eq!(replay(&rows, 40).ci(7), None);
        assert_eq!(replay(&rows, 60).ci(7), Some("pending"));
        // A previously passing head does not stay passing either.
        let passing = [
            head("a", 0),
            run("a", "started:test", 10, 1),
            run("a", "success:test", 20, 1),
            head("b", 30),
        ];
        assert_eq!(replay(&passing, 25).ci(7), Some("passing"));
        assert_eq!(replay(&passing, 40).ci(7), None);
    }

    #[test]
    fn a_new_head_that_drops_a_failing_check_does_not_inherit_the_failure() {
        let rows = [
            head("a", 0),
            run("a", "started:test", 10, 1),
            run("a", "failure:test", 20, 1),
            head("b", 30),
            run("b", "started:lint", 40, 2),
            run("b", "success:lint", 50, 2),
        ];
        assert_eq!(replay(&rows, 25).ci(7), Some("failing"));
        assert_eq!(replay(&rows, 45).ci(7), Some("pending"));
        assert_eq!(replay(&rows, 1000).ci(7), Some("passing"));
    }

    #[test]
    fn runs_of_a_commit_that_is_not_the_head_are_ignored() {
        // A run of a newer push whose head row is not before `t` yet (the head
        // row is stamped at `updated_at`, which can trail the push): unknown,
        // never the recorded head's verdict.
        let rows = [
            head("a", 0),
            run("a", "started:test", 10, 1),
            run("a", "success:test", 20, 1),
            run("b", "started:test", 30, 2),
            head("b", 60),
        ];
        assert_eq!(replay(&rows, 25).ci(7), Some("passing"));
        assert_eq!(replay(&rows, 40).ci(7), None);
        assert_eq!(replay(&rows, 70).ci(7), Some("pending"));
        // An older head's runs are ignored once a newer head is recorded.
        let older = [
            head("a", 0),
            head("b", 5),
            run("b", "started:test", 10, 2),
            run("b", "success:test", 20, 2),
            run("a", "started:test", 12, 1),
            run("a", "failure:test", 15, 1),
        ];
        assert_eq!(replay(&older, 30).ci(7), Some("passing"));
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
