//! Pure builder and tracker tests for `dispatch_eligibility` (Issue #12790).
//! `record`'s impure wrapper is exercised only for its early return (no OTLP
//! sink is registered in this test binary), exactly like `ops::disposition`.

use chrono::{Duration, TimeZone, Utc};

use super::*;
use crate::priority_pick::Level;
use crate::telemetry::queue_repo::QueueRepoRef;
use crate::types::{QueueDisposition as Qd, ReadyQueueRow, RowPlan, WorkFinderTickSummary};

fn at(secs: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
}

fn qrow(repo: &str, issue: u32, rank: usize, disposition: Qd) -> ReadyQueueRow {
    ReadyQueueRow {
        rank,
        repo: repo.to_string(),
        issue,
        workspace_weight: 100,
        urgent: false,
        level: Level::Default,
        main_red_fix: false,
        created_at: None,
        story_points: None,
        disposition,
        detail: None,
        state: disposition.state().to_string(),
        reason: disposition.reason().to_string(),
        plan: RowPlan::default(),
    }
}

fn summary(rows: Vec<ReadyQueueRow>, max_concurrent: usize) -> WorkFinderTickSummary {
    WorkFinderTickSummary {
        at: at(0),
        max_concurrent,
        queue: rows,
        ..WorkFinderTickSummary::default()
    }
}

/// The identity resolver: every root `owner/repo` resolves to itself, public.
fn resolve_all(root: &str) -> Option<QueueRepoRef> {
    Some(QueueRepoRef {
        repo: root.to_string(),
        visibility: RepoVisibility::Public,
    })
}

// ------------------------------------------------------------------- build_rows

#[test]
fn running_rows_are_not_emitted_but_count_toward_work_ahead() {
    let s = summary(
        vec![
            qrow("o/r", 1, 1, Qd::InFlight),         // running: occupies pool
            qrow("o/r", 2, 2, Qd::DeferredCapacity), // eligible, waiting
            qrow("o/r", 3, 3, Qd::OpenPr),           // blocked
        ],
        2,
    );
    let rows = build_rows(&s, resolve_all);
    // The InFlight row gets no fact of its own.
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.issue != 1));

    let ready = rows.iter().find(|r| r.issue == 2).unwrap();
    assert_eq!(ready.eligibility, Eligibility::Eligible);
    assert_eq!(ready.reason, "deferred_capacity");
    // One eligible competitor ahead: the running row #1 (it occupies a slot).
    assert_eq!(ready.eligible_work_ahead, 1);
    assert_eq!(ready.eligible_work_ahead_repo, 1);

    let blocked = rows.iter().find(|r| r.issue == 3).unwrap();
    assert_eq!(blocked.eligibility, Eligibility::Blocked);
    assert_eq!(blocked.reason, "open_pr");
    // Two eligible ahead (#1 running, #2 ready); the blocked row is excluded.
    assert_eq!(blocked.eligible_work_ahead, 2);
}

#[test]
fn work_ahead_excludes_blocked_items_and_splits_by_repo() {
    let s = summary(
        vec![
            qrow("o/a", 1, 1, Qd::DeferredCapacity), // eligible, repo a
            qrow("o/b", 2, 2, Qd::Parked),           // blocked, repo b (not counted)
            qrow("o/a", 3, 3, Qd::DeferredBuildBackoff), // eligible, repo a
            qrow("o/a", 4, 4, Qd::DeferredCapacity), // the subject
        ],
        2,
    );
    let rows = build_rows(&s, resolve_all);
    let subject = rows.iter().find(|r| r.issue == 4).unwrap();
    // Host pool: ranks 1 and 3 are eligible and ahead; rank 2 is blocked.
    assert_eq!(subject.eligible_work_ahead, 2);
    // Repo pool (o/a): same two (rank 2 is a different repo anyway).
    assert_eq!(subject.eligible_work_ahead_repo, 2);
}

#[test]
fn unresolved_rows_are_dropped_and_level_rides_along() {
    let mut row = qrow("o/r", 1, 1, Qd::DeferredCapacity);
    row.level = Level::VeryImportant;
    let s = summary(vec![row, qrow("o/unresolved", 2, 2, Qd::DeferredCapacity)], 2);
    // Only o/r resolves.
    let rows = build_rows(&s, |root| (root == "o/r").then(|| resolve_all(root)).flatten());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].level, "very-important");
    assert_eq!(rows[0].max_concurrent, 2);
}

#[test]
fn more_than_256_waiting_rows_are_all_emitted() {
    let queue: Vec<ReadyQueueRow> = (0..300)
        .map(|i| qrow("o/r", i + 1, i as usize + 1, Qd::DeferredCapacity))
        .collect();
    let rows = build_rows(&summary(queue, 2), resolve_all);
    assert_eq!(rows.len(), 300);
    let issues: std::collections::HashSet<u32> = rows.iter().map(|r| r.issue).collect();
    assert_eq!(issues.len(), 300);
}

// --------------------------------------------------------- EligibilityTracker::diff

fn elig_row(slug: &str, issue: u32, eligibility: Eligibility, reason: &str) -> EligRow {
    EligRow {
        slug: slug.to_string(),
        visibility: RepoVisibility::Public,
        issue,
        rank: 1,
        eligibility,
        reason: reason.to_string(),
        state: if eligibility == Eligibility::Blocked {
            "blocked"
        } else {
            "ready"
        }
        .to_string(),
        eligible_work_ahead: 0,
        eligible_work_ahead_repo: 0,
        level: "default".to_string(),
        max_concurrent: 2,
    }
}

#[test]
fn first_sight_emits_changed_and_anchors_block_start_to_the_tick() {
    let mut tracker = EligibilityTracker::default();
    let rows = [elig_row(
        "o/r",
        1,
        Eligibility::Eligible,
        "deferred_capacity",
    )];
    let emissions = tracker.diff(&rows, &[], at(5), at(9), Duration::seconds(600));
    assert_eq!(emissions.len(), 1);
    assert_eq!(emissions[0].transition, EligibilityTransition::Changed);
    assert_eq!(emissions[0].block_start, at(5), "block_start anchors to tick_at, not now");
}

#[test]
fn same_class_keeps_block_start_and_refreshes_only_after_the_window() {
    let mut tracker = EligibilityTracker::default();
    let rows = [elig_row(
        "o/r",
        1,
        Eligibility::Eligible,
        "deferred_capacity",
    )];
    assert_eq!(
        tracker
            .diff(&rows, &[], at(0), at(0), Duration::seconds(600))
            .len(),
        1
    );
    // Within the window: nothing.
    assert!(tracker
        .diff(&rows, &[], at(60), at(60), Duration::seconds(600))
        .is_empty());
    // After the window: a refresh that keeps the original block_start (at(0)).
    let later = tracker.diff(&rows, &[], at(700), at(700), Duration::seconds(600));
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].transition, EligibilityTransition::Refresh);
    assert_eq!(later[0].block_start, at(0));
}

#[test]
fn a_class_flip_re_anchors_block_start() {
    let mut tracker = EligibilityTracker::default();
    let eligible = [elig_row(
        "o/r",
        1,
        Eligibility::Eligible,
        "deferred_capacity",
    )];
    let blocked = [elig_row("o/r", 1, Eligibility::Blocked, "open_pr")];
    assert_eq!(
        tracker
            .diff(&eligible, &[], at(0), at(0), Duration::seconds(600))
            .len(),
        1
    );
    let flip = tracker.diff(&blocked, &[], at(30), at(30), Duration::seconds(600));
    assert_eq!(flip.len(), 1);
    assert_eq!(flip[0].transition, EligibilityTransition::Changed);
    assert_eq!(flip[0].block_start, at(30), "the block began at the flip tick");
}

#[test]
fn a_disappeared_item_is_forgotten_unless_its_repo_listing_was_not_whole() {
    let mut tracker = EligibilityTracker::default();
    let rows = [elig_row("o/r", 1, Eligibility::Blocked, "open_pr")];
    assert_eq!(
        tracker
            .diff(&rows, &[], at(0), at(0), Duration::seconds(600))
            .len(),
        1
    );

    // Gone with a whole listing → forgotten; a reappearance is first sight.
    assert!(tracker
        .diff(&[], &[], at(60), at(60), Duration::seconds(600))
        .is_empty());
    let reappear = tracker.diff(&rows, &[], at(120), at(120), Duration::seconds(600));
    assert_eq!(reappear[0].transition, EligibilityTransition::Changed);

    // Gone while that repo's listing was not whole → kept, no re-anchor.
    assert!(tracker
        .diff(&[], &["o/r".to_string()], at(180), at(180), Duration::seconds(600))
        .is_empty());
    let still_known = tracker.diff(&rows, &[], at(240), at(240), Duration::seconds(600));
    assert!(still_known.is_empty(), "class unchanged, within window: {still_known:?}");
}

#[test]
fn build_record_maps_every_field() {
    let emission = Emission {
        row: EligRow {
            eligible_work_ahead: 4,
            eligible_work_ahead_repo: 1,
            level: "very-important".to_string(),
            ..elig_row("o/r", 7, Eligibility::Eligible, "deferred_build_backoff")
        },
        transition: EligibilityTransition::Changed,
        block_start: at(5),
    };
    let record = build_record(&emission, "host-a", at(9), at(5));
    assert_eq!(record.repo, "o/r");
    assert_eq!(record.resource_pool, "o/r");
    assert_eq!(record.issue, 7);
    assert_eq!(record.host, "host-a");
    assert_eq!(record.eligibility, Eligibility::Eligible);
    assert_eq!(record.reason, "deferred_build_backoff");
    assert_eq!(record.block_start, at(5));
    assert_eq!(record.sampled_at, at(9));
    assert_eq!(record.tick_at, at(5));
    assert_eq!(record.eligible_work_ahead, 4);
    assert_eq!(record.level, "very-important");
}

#[test]
fn record_without_a_sink_is_a_noop() {
    // No sink registered in this binary: the wrapper returns before touching
    // the tracker or the (absent) work-finder summary.
    record();
}
