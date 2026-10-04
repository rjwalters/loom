//! Page parsers for the two per-PR listings of the raw fleet event cache
//! (#10197): a PR's formal reviews and its head commit's check runs.
//!
//! - `GET repos/{o}/{r}/pulls/{n}/reviews` → one [`EventKind::Review`] row per
//!   submitted review: `label` = the state, lowercased (`approved`,
//!   `changes_requested`, `commented`, `dismissed`), `event_time` =
//!   `submitted_at`, `seq` = the review id. A pending review (no
//!   `submitted_at`) is not a forge fact yet and is skipped.
//! - `GET repos/{o}/{r}/commits/{sha}/check-runs` → up to two
//!   [`EventKind::CheckRun`] rows per run, both with `seq` = the run id:
//!   `started:<name>` at `started_at`, and, once the run has completed,
//!   `<conclusion>:<name>` at `completed_at`. A queued run (no `started_at`)
//!   yields nothing. GitHub's default `filter=latest` lists only the latest
//!   run of each name, which is also how [`super::fleet_state_prs`] reads them.
//!
//! The item of every row is the PR the listing was read for — the fan-out
//! driver ([`super::fleet_events_fanout`]) passes it; neither payload names it
//! reliably (a check run's `pull_requests` is empty for a fork).
//!
//! `fetched_at` is stored apart as always; the ids are content-derived, so a
//! re-read (a resumed run, an open PR's refresh) adds only what is new — a run
//! that has since completed adds its completion row.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::fleet_events::{EventKind, ItemKind, RawEvent, SOURCE_FORGE};

/// `label` prefix of a check run's start row.
pub const STARTED: &str = "started";

#[derive(Deserialize)]
struct RestReview {
    id: u64,
    state: String,
    #[serde(default)]
    submitted_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct RestCheckRuns {
    check_runs: Vec<RestCheckRun>,
}

#[derive(Deserialize)]
struct RestCheckRun {
    id: u64,
    name: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    completed_at: Option<DateTime<Utc>>,
}

fn row(
    repo: &str,
    pr: u32,
    kind: EventKind,
    label: String,
    at: DateTime<Utc>,
    seq: u64,
    fetched_at: DateTime<Utc>,
) -> RawEvent {
    RawEvent::new(repo, pr, ItemKind::Pr, kind, Some(label), at, SOURCE_FORGE, seq, fetched_at)
}

/// Parse one page of PR `pr`'s reviews. Returns the rows and how many reviews
/// the page held.
///
/// # Errors
///
/// The body is not a JSON array of reviews.
pub fn parse_reviews(
    repo: &str,
    pr: u32,
    body: &str,
    fetched_at: DateTime<Utc>,
) -> anyhow::Result<(Vec<RawEvent>, usize)> {
    let reviews: Vec<RestReview> = serde_json::from_str(body)?;
    let events = reviews
        .iter()
        .filter_map(|r| {
            let at = r.submitted_at?;
            let state = r.state.to_ascii_lowercase();
            (state != "pending")
                .then(|| row(repo, pr, EventKind::Review, state, at, r.id, fetched_at))
        })
        .collect();
    Ok((events, reviews.len()))
}

/// Parse one page of the check runs of PR `pr`'s head commit. Returns the
/// rows and how many runs the page held.
///
/// # Errors
///
/// The body is not a check-runs object.
pub fn parse_check_runs(
    repo: &str,
    pr: u32,
    body: &str,
    fetched_at: DateTime<Utc>,
) -> anyhow::Result<(Vec<RawEvent>, usize)> {
    let page: RestCheckRuns = serde_json::from_str(body)?;
    let mut events = Vec::new();
    for run in &page.check_runs {
        let Some(started) = run.started_at else {
            continue;
        };
        let start = format!("{STARTED}:{}", run.name);
        events.push(row(repo, pr, EventKind::CheckRun, start, started, run.id, fetched_at));
        if run.status.as_deref() == Some("completed") {
            if let (Some(conclusion), Some(done)) = (&run.conclusion, run.completed_at) {
                let label = format!("{conclusion}:{}", run.name);
                events.push(row(repo, pr, EventKind::CheckRun, label, done, run.id, fetched_at));
            }
        }
    }
    Ok((events, page.check_runs.len()))
}

/// `(state, check name)` of a `check_run` row's label. The state never holds
/// a `:`; a name may.
#[must_use]
pub fn split_check_label(label: &str) -> Option<(&str, &str)> {
    label.split_once(':')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-04T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    const REVIEWS: &str = r#"[
      {"id": 501, "user": {"login": "a"}, "state": "COMMENTED",
       "submitted_at": "2026-10-03T10:00:00Z", "commit_id": "x"},
      {"id": 502, "user": {"login": "b"}, "state": "APPROVED",
       "submitted_at": "2026-10-03T11:00:00Z", "commit_id": "x"},
      {"id": 503, "user": {"login": "c"}, "state": "PENDING", "commit_id": "x"}
    ]"#;

    const CHECKS: &str = r#"{"total_count": 3, "check_runs": [
      {"id": 71, "name": "test: unit", "status": "completed", "conclusion": "failure",
       "started_at": "2026-10-03T10:00:00Z", "completed_at": "2026-10-03T10:05:00Z"},
      {"id": 72, "name": "lint", "status": "in_progress", "conclusion": null,
       "started_at": "2026-10-03T10:00:30Z", "completed_at": null},
      {"id": 73, "name": "deploy", "status": "queued", "conclusion": null,
       "started_at": null, "completed_at": null}
    ]}"#;

    #[test]
    fn submitted_reviews_become_rows_and_pending_ones_do_not() {
        let (events, n) = parse_reviews("o/r", 12, REVIEWS, now()).unwrap();
        assert_eq!(n, 3);
        let rows: Vec<(u32, &str, u64, String)> = events
            .iter()
            .map(|e| (e.item, e.label.as_deref().unwrap(), e.seq, e.event_time.to_rfc3339()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (12, "commented", 501, "2026-10-03T10:00:00+00:00".to_string()),
                (12, "approved", 502, "2026-10-03T11:00:00+00:00".to_string()),
            ]
        );
        assert!(events.iter().all(|e| e.kind == EventKind::Review
            && e.item_kind == ItemKind::Pr
            && e.fetched_at == now()));
    }

    #[test]
    fn check_runs_yield_a_start_row_and_a_completion_row() {
        let (events, n) = parse_check_runs("o/r", 12, CHECKS, now()).unwrap();
        assert_eq!(n, 3);
        let rows: Vec<(&str, u64, String)> = events
            .iter()
            .map(|e| (e.label.as_deref().unwrap(), e.seq, e.event_time.to_rfc3339()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("started:test: unit", 71, "2026-10-03T10:00:00+00:00".to_string()),
                ("failure:test: unit", 71, "2026-10-03T10:05:00+00:00".to_string()),
                ("started:lint", 72, "2026-10-03T10:00:30+00:00".to_string()),
            ]
        );
        // The two rows of one run share `seq` but not an id.
        assert_ne!(events[0].id, events[1].id);
        assert_eq!(split_check_label("failure:test: unit"), Some(("failure", "test: unit")));
    }

    #[test]
    fn a_rerun_after_completion_adds_only_the_completion_row() {
        let (before, _) = parse_check_runs("o/r", 12, CHECKS, now()).unwrap();
        let later = CHECKS.replace(
            r#""status": "in_progress", "conclusion": null,
       "started_at": "2026-10-03T10:00:30Z", "completed_at": null"#,
            r#""status": "completed", "conclusion": "success",
       "started_at": "2026-10-03T10:00:30Z", "completed_at": "2026-10-03T10:09:00Z""#,
        );
        let (after, _) =
            parse_check_runs("o/r", 12, &later, now() + chrono::Duration::hours(1)).unwrap();
        let known: std::collections::HashSet<&str> = before.iter().map(|e| e.id.as_str()).collect();
        let fresh: Vec<&str> = after
            .iter()
            .filter(|e| !known.contains(e.id.as_str()))
            .map(|e| e.label.as_deref().unwrap())
            .collect();
        assert_eq!(fresh, vec!["success:lint"]);
    }

    #[test]
    fn malformed_bodies_are_errors() {
        assert!(parse_reviews("o/r", 1, "{}", now()).is_err());
        assert!(parse_check_runs("o/r", 1, "[]", now()).is_err());
    }
}
