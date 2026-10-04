//! The pulls-listing page parser for the raw fleet event cache (#10197).
//!
//! `GET repos/{owner}/{repo}/pulls?state=all&sort=created&direction=desc`
//! lists every PR newest-created first, 100 per call, with the PR's body and
//! its `created_at` / `closed_at` / `merged_at`. It fills two gaps the
//! issue-events listing ([`super::fleet_events_forge`]) leaves:
//!
//! - **Linkage references.** Each PR yields one [`EventKind::ClosingRef`] row
//!   per issue its body links, or one target-less row when it links none.
//!   Links are read with [`crate::worktree_ops::gh::linkage_refs`] — the very
//!   phrase set the work finder's open-PR guard (#4123 / #8940) uses: GitHub's
//!   closing keywords *and* the partial-increment phrases `Part of #N` /
//!   `Contributes to #N`, tolerant of a colon or markdown before `#N`
//!   (`Closes: #N`, `**Part of:** #N`). The row's `label` records the family
//!   (`"closes"` / `"part_of"`) so a later consumer can tell them apart without
//!   a refetch. [`super::fleet_state`] counts both, as the guard does, for
//!   `pr_open_skip_lockout`.
//! - **Open / merge / close times** of PRs older than the issue-events
//!   listing's depth window, so an old PR's closure is known even when its
//!   `closed` event has aged out of that listing.
//!
//! - **Head commits.** One [`EventKind::HeadCommit`] row per PR (`label` =
//!   `head.sha`), stamped at the PR's `updated_at` — a push bumps
//!   `updated_at`, so the head was that commit by then. It is the work list of
//!   the check-run fetcher ([`super::fleet_events_fanout`]); a re-read after
//!   the PR changed adds a newer row, never rewrites the old one.
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
//! read), and the base branch is not checked. The real guard also discards a
//! PR whose author is not trusted (author/association filter); this listing
//! keeps every PR — a known approximation that can only over-report a lockout.
//!
//! Every row carries `seq = 0` except the `closing_ref` rows, which carry the
//! PR's REST id: those are what a refresh recognises as already cached, and
//! only this listing produces them.
//!
//! # Merged / closed rows are stored twice, by design
//!
//! The `merged` / `closed` rows synthesised here carry `seq = 0`; the
//! issue-events listing records the same events with the forge event id as
//! `seq`. The ids therefore differ and a closed PR has both rows on disk. The
//! replay is unaffected (closing an already-closed item is a no-op). Do not
//! "fix" this by dropping one source: the pulls rows are what keep an old PR's
//! closure known after its issue event has aged out of that listing.
//!
//! # What a refresh sees
//!
//! A refresh reads only the head pages (newest-created first), where new PRs
//! appear. A closure of an older PR reaches the cache through the issue-events
//! refresh; a body edit on an older PR is not seen at all.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::fleet_events::{EventKind, ItemKind, RawEvent, SOURCE_FORGE};
use crate::worktree_ops::gh::LinkageKind;

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
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    head: Option<RestHead>,
}

#[derive(Deserialize)]
struct RestHead {
    sha: String,
}

/// Issues `body` links (ascending, one per issue) and the phrase family that
/// linked each — the open-PR guard's own rule.
#[must_use]
pub fn linked_issues(body: &str) -> Vec<(u32, LinkageKind)> {
    crate::worktree_ops::gh::linkage_refs(body)
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
        let row = |kind: EventKind, label: Option<&str>, at: DateTime<Utc>, seq: u64| {
            RawEvent::new(
                repo,
                pr.number,
                ItemKind::Pr,
                kind,
                label.map(str::to_string),
                at,
                SOURCE_FORGE,
                seq,
                fetched_at,
            )
        };
        events.push(row(EventKind::Opened, None, pr.created_at, 0));
        let targets = linked_issues(pr.body.as_deref().unwrap_or(""));
        if targets.is_empty() {
            // Unlabelled and untargeted: the same id the pre-#10197-fix
            // formula gave it.
            events.push(row(EventKind::ClosingRef, None, pr.created_at, pr.id));
        }
        for (target, kind) in targets {
            events.push(
                row(EventKind::ClosingRef, Some(kind.as_str()), pr.created_at, pr.id)
                    .with_target(Some(target)),
            );
        }
        if let (Some(head), Some(at)) = (&pr.head, pr.updated_at) {
            events.push(row(EventKind::HeadCommit, Some(&head.sha), at, 0));
        }
        if let Some(at) = pr.merged_at {
            events.push(row(EventKind::Merged, None, at, 0));
        }
        if let Some(at) = pr.closed_at {
            events.push(row(EventKind::Closed, None, at, 0));
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
       "updated_at": "2026-10-03T12:00:05Z", "head": {"sha": "abc123", "ref": "f"},
       "body": "Part of #5"},
      {"id": 9000, "number": 10, "created_at": "2026-10-02T09:00:00Z",
       "closed_at": "2026-10-02T10:00:00Z", "merged_at": null, "body": null}
    ]"#;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-04T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    type Row<'a> = (EventKind, Option<&'a str>, Option<u32>, u64);

    fn rows(item: u32, events: &[RawEvent]) -> Vec<Row<'_>> {
        events
            .iter()
            .filter(|e| e.item == item)
            .map(|e| (e.kind, e.label.as_deref(), e.target, e.seq))
            .collect()
    }

    #[test]
    fn the_guard_phrase_set_counts_including_part_of() {
        use LinkageKind::{Closes, PartOf};
        assert_eq!(
            linked_issues("Closes #5\nfixes #7, part of #8"),
            vec![(5, Closes), (7, Closes), (8, PartOf)]
        );
        assert_eq!(linked_issues("Part of #10197"), vec![(10197, PartOf)]);
        assert_eq!(linked_issues("Contributes to #9"), vec![(9, PartOf)]);
        // Colon / markdown between the phrase and the number (#4508 tolerance).
        assert_eq!(linked_issues("Closes: #42"), vec![(42, Closes)]);
        assert_eq!(linked_issues("**Part of:** #10197"), vec![(10197, PartOf)]);
        // Near-misses and bare mentions are not links.
        assert!(linked_issues("Discloses #3").is_empty());
        assert!(linked_issues("See #3").is_empty());
        // Both families on one issue: the closing keyword wins.
        assert_eq!(linked_issues("Part of #5. Closes #5"), vec![(5, Closes)]);
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
                (EventKind::Opened, None, None, 0),
                (EventKind::ClosingRef, Some("closes"), Some(5), 9002),
                (EventKind::ClosingRef, Some("closes"), Some(7), 9002),
                (EventKind::ClosingRef, Some("part_of"), Some(8), 9002),
            ]
        );
        assert_eq!(
            rows(11, &events),
            vec![
                (EventKind::Opened, None, None, 0),
                (EventKind::ClosingRef, Some("part_of"), Some(5), 9001),
                (EventKind::HeadCommit, Some("abc123"), None, 0),
                (EventKind::Merged, None, None, 0),
                (EventKind::Closed, None, None, 0),
            ]
        );
        assert_eq!(
            rows(10, &events),
            vec![
                (EventKind::Opened, None, None, 0),
                (EventKind::ClosingRef, None, None, 9000),
                (EventKind::Closed, None, None, 0),
            ]
        );
        // Stamped at the PR's creation, never at fetch time.
        let r = events
            .iter()
            .find(|e| e.item == 12 && e.target == Some(5))
            .unwrap();
        assert_eq!(r.event_time.to_rfc3339(), "2026-10-04T09:00:00+00:00");
        // The head commit is stamped at `updated_at`, not at creation.
        let head = events
            .iter()
            .find(|e| e.kind == EventKind::HeadCommit)
            .unwrap();
        assert_eq!(head.event_time.to_rfc3339(), "2026-10-03T12:00:05+00:00");
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
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn a_target_less_row_keeps_the_unlabelled_id() {
        // PR 10 links nothing: its row is unlabelled and untargeted, so its id
        // is the one the formula gave before `label`/`target` were used here.
        let (events, _) = parse_pulls("o/r", BODY, now()).unwrap();
        let row = events
            .iter()
            .find(|e| e.item == 10 && e.kind == EventKind::ClosingRef)
            .unwrap();
        let bare = RawEvent::new(
            "o/r",
            10,
            ItemKind::Pr,
            EventKind::ClosingRef,
            None,
            row.event_time,
            SOURCE_FORGE,
            9000,
            now(),
        );
        assert_eq!(row.id, bare.id);
    }
}
