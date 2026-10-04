//! The pulls-listing page parser for the raw fleet event cache (#10197).
//!
//! `GET repos/{owner}/{repo}/pulls?state=all&sort=created&direction=desc`
//! lists every PR newest-created first, 100 per call, with the PR's body and
//! its `created_at` / `closed_at` / `merged_at`. It fills two gaps the
//! issue-events listing ([`super::fleet_events_forge`]) leaves:
//!
//! - **Closing references.** Each PR yields one [`EventKind::ClosingRef`] row
//!   per issue its body closes (GitHub's closing keywords, read with
//!   [`crate::merge_pr::refs::closing_refs`] — the same quota-free parser
//!   `merge-pr` falls back to), or one target-less row when it closes none.
//!   [`super::fleet_state`] needs them for `pr_open_skip_lockout`: the work
//!   finder's open-PR guard refuses a ready issue while a PR that closes it is
//!   open.
//! - **Open / merge / close times** of PRs older than the issue-events
//!   listing's depth window, so an old PR's closure is known even when its
//!   `closed` event has aged out of that listing.
//!
//! # Knowable-at of a closing reference
//!
//! A `closing_ref` row is stamped at the PR's `created_at`: Loom's builder
//! writes `Closes #N` into the body at `gh pr create`. A body edited later to
//! add or drop a reference is read as if it had always said so — the one place
//! this cache stamps a fact earlier than the forge recorded it. The listing
//! carries no body history, so nothing finer is available from this endpoint.
//!
//! Only same-repo `#N` references count (`owner/repo#N` and URL forms are not
//! read), and the base branch is not checked.
//!
//! Every row carries `seq = 0` except the `closing_ref` rows, which carry the
//! PR's REST id: those are what a refresh recognises as already cached, and
//! only this listing produces them.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::fleet_events::{EventKind, ItemKind, RawEvent, SOURCE_FORGE};

#[derive(Deserialize)]
struct RestPull {
    id: u64,
    number: u32,
    created_at: DateTime<Utc>,
    #[serde(default)]
    closed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    merged_at: Option<DateTime<Utc>>,
    #[serde(default)]
    body: Option<String>,
}

/// Issue numbers `body` closes, ascending, deduped, `u32`-sized only.
#[must_use]
pub fn closed_issues(body: &str) -> Vec<u32> {
    crate::merge_pr::refs::closing_refs(body)
        .into_iter()
        .filter_map(|n| u32::try_from(n).ok())
        .collect()
}

/// Parse one page of the pulls listing into raw rows read at `fetched_at`.
/// Returns the rows and how many PRs the page held.
///
/// # Errors
///
/// The body is not a JSON array of pull requests.
pub fn parse_pulls(
    repo: &str,
    body: &str,
    fetched_at: DateTime<Utc>,
) -> anyhow::Result<(Vec<RawEvent>, usize)> {
    let pulls: Vec<RestPull> = serde_json::from_str(body)?;
    let mut events = Vec::new();
    for pr in &pulls {
        let row = |kind: EventKind, at: DateTime<Utc>, seq: u64| {
            RawEvent::new(
                repo,
                pr.number,
                ItemKind::Pr,
                kind,
                None,
                at,
                SOURCE_FORGE,
                seq,
                fetched_at,
            )
        };
        events.push(row(EventKind::Opened, pr.created_at, 0));
        let targets = closed_issues(pr.body.as_deref().unwrap_or(""));
        if targets.is_empty() {
            events.push(row(EventKind::ClosingRef, pr.created_at, pr.id));
        }
        for target in targets {
            events.push(row(EventKind::ClosingRef, pr.created_at, pr.id).with_target(Some(target)));
        }
        if let Some(at) = pr.merged_at {
            events.push(row(EventKind::Merged, at, 0));
        }
        if let Some(at) = pr.closed_at {
            events.push(row(EventKind::Closed, at, 0));
        }
    }
    Ok((events, pulls.len()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BODY: &str = r#"[
      {"id": 9002, "number": 12, "created_at": "2026-10-04T09:00:00Z",
       "closed_at": null, "merged_at": null,
       "body": "Closes #5\n\nAlso fixes #7 and is part of #8."},
      {"id": 9001, "number": 11, "created_at": "2026-10-03T09:00:00Z",
       "closed_at": "2026-10-03T12:00:00Z", "merged_at": "2026-10-03T12:00:00Z",
       "body": "Part of #5"},
      {"id": 9000, "number": 10, "created_at": "2026-10-02T09:00:00Z",
       "closed_at": "2026-10-02T10:00:00Z", "merged_at": null, "body": null}
    ]"#;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-04T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn rows(item: u32, events: &[RawEvent]) -> Vec<(EventKind, Option<u32>, u64)> {
        events
            .iter()
            .filter(|e| e.item == item)
            .map(|e| (e.kind, e.target, e.seq))
            .collect()
    }

    #[test]
    fn closing_keywords_count_and_part_of_does_not() {
        assert_eq!(closed_issues("Closes #5\nfixes #7, part of #8"), vec![5, 7]);
        assert!(closed_issues("Part of #10197").is_empty());
        assert!(closed_issues("Discloses #3").is_empty());
    }

    #[test]
    fn each_pr_yields_open_refs_and_end_rows() {
        let (events, n) = parse_pulls("o/r", BODY, now()).unwrap();
        assert_eq!(n, 3);
        assert!(events
            .iter()
            .all(|e| e.item_kind == ItemKind::Pr && e.fetched_at == now()));
        assert_eq!(
            rows(12, &events),
            vec![
                (EventKind::Opened, None, 0),
                (EventKind::ClosingRef, Some(5), 9002),
                (EventKind::ClosingRef, Some(7), 9002),
            ]
        );
        assert_eq!(
            rows(11, &events),
            vec![
                (EventKind::Opened, None, 0),
                (EventKind::ClosingRef, None, 9001),
                (EventKind::Merged, None, 0),
                (EventKind::Closed, None, 0),
            ]
        );
        assert_eq!(
            rows(10, &events),
            vec![
                (EventKind::Opened, None, 0),
                (EventKind::ClosingRef, None, 9000),
                (EventKind::Closed, None, 0),
            ]
        );
        // Stamped at the PR's creation, never at fetch time.
        let r = events.iter().find(|e| e.target == Some(5)).unwrap();
        assert_eq!(r.event_time.to_rfc3339(), "2026-10-04T09:00:00+00:00");
    }

    #[test]
    fn the_synthesised_open_row_matches_the_issue_events_one() {
        // Same content from either listing → same id → stored once.
        let (events, _) = parse_pulls("o/r", BODY, now()).unwrap();
        let from_pulls = events
            .iter()
            .find(|e| e.item == 12 && e.kind == EventKind::Opened)
            .unwrap();
        let from_issue_events = RawEvent::new(
            "o/r",
            12,
            ItemKind::Pr,
            EventKind::Opened,
            None,
            from_pulls.event_time,
            SOURCE_FORGE,
            0,
            now() + chrono::Duration::hours(1),
        );
        assert_eq!(from_pulls.id, from_issue_events.id);
    }

    #[test]
    fn distinct_targets_get_distinct_ids() {
        let (events, _) = parse_pulls("o/r", BODY, now()).unwrap();
        let ids: std::collections::HashSet<&str> = events
            .iter()
            .filter(|e| e.item == 12 && e.kind == EventKind::ClosingRef)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids.len(), 2);
    }
}
