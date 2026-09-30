//! Pure-tracker and row/span-building tests for `ops::disposition` (Issue
//! #9222). `record`'s own impure wrapper is exercised only for its early
//! return (no ops sink is ever registered in this test binary), exactly like
//! `ops::dwell`'s `record_tick_without_sink_is_a_noop`.

use std::collections::HashMap;

use chrono::{Duration, TimeZone, Utc};

use super::*;
use crate::telemetry::queue_snapshot::QueueRepoRef;
use crate::telemetry::RepoVisibility;
use crate::types::{QueueDisposition as Qd, ReadyQueueRow, WorkFinderTickSummary};

fn at(secs: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
}

fn row(slug: &str, issue: u32, rank: usize, disposition: Qd) -> DispositionRow {
    DispositionRow {
        slug: slug.to_string(),
        visibility: RepoVisibility::Public,
        issue,
        rank,
        disposition,
        park_label: None,
        pr_number: None,
        candidate_rank: u32::try_from(rank).unwrap_or(u32::MAX),
        total_candidates: 1,
        priority_score: None,
    }
}

// ------------------------------------------------------------- DispositionTracker::diff

#[test]
fn first_sight_emits_changed_with_no_previous_disposition() {
    let mut tracker = DispositionTracker::default();
    let rows = [row("acme/widgets", 1, 1, Qd::DeferredSaturation)];
    let emissions = tracker.diff(&rows, &[], at(0), Duration::seconds(600));
    assert_eq!(emissions.len(), 1);
    assert_eq!(emissions[0].transition, Transition::Changed);
    assert_eq!(emissions[0].previous_disposition, None);
    assert_eq!(emissions[0].disposition, Qd::DeferredSaturation);
}

#[test]
fn same_disposition_within_the_refresh_window_emits_nothing() {
    let mut tracker = DispositionTracker::default();
    let rows = [row("acme/widgets", 1, 1, Qd::DeferredSaturation)];
    assert_eq!(
        tracker
            .diff(&rows, &[], at(0), Duration::seconds(600))
            .len(),
        1
    );
    let emissions = tracker.diff(&rows, &[], at(60), Duration::seconds(600));
    assert!(emissions.is_empty(), "{emissions:?}");
}

#[test]
fn same_disposition_after_the_window_emits_refresh() {
    let mut tracker = DispositionTracker::default();
    let rows = [row("acme/widgets", 1, 1, Qd::DeferredSaturation)];
    assert_eq!(
        tracker
            .diff(&rows, &[], at(0), Duration::seconds(600))
            .len(),
        1
    );
    let emissions = tracker.diff(&rows, &[], at(600), Duration::seconds(600));
    assert_eq!(emissions.len(), 1);
    assert_eq!(emissions[0].transition, Transition::Refresh);
    assert_eq!(emissions[0].previous_disposition, None);
}

#[test]
fn a_change_emits_changed_with_previous_disposition() {
    let mut tracker = DispositionTracker::default();
    let first = [row("acme/widgets", 1, 1, Qd::DeferredSaturation)];
    tracker.diff(&first, &[], at(0), Duration::seconds(600));
    let second = [row("acme/widgets", 1, 1, Qd::Dispatched)];
    let emissions = tracker.diff(&second, &[], at(1), Duration::seconds(600));
    assert_eq!(emissions.len(), 1);
    assert_eq!(emissions[0].transition, Transition::Changed);
    assert_eq!(emissions[0].previous_disposition, Some(Qd::DeferredSaturation));
    assert_eq!(emissions[0].disposition, Qd::Dispatched);
}

#[test]
fn a_key_missing_from_a_successfully_listed_repo_emits_left_queue_once() {
    let mut tracker = DispositionTracker::default();
    let first = [row("acme/widgets", 1, 1, Qd::DeferredSaturation)];
    tracker.diff(&first, &[], at(0), Duration::seconds(600));
    // Repo still listed successfully (not in `failed_slugs`), but issue 1 no
    // longer appears in its rows.
    let emissions = tracker.diff(&[], &[], at(1), Duration::seconds(600));
    assert_eq!(emissions.len(), 1);
    assert_eq!(emissions[0].transition, Transition::LeftQueue);
    assert_eq!(emissions[0].issue, 1);
    assert_eq!(
        emissions[0].disposition,
        Qd::DeferredSaturation,
        "carries the last-known disposition"
    );
    assert_eq!(emissions[0].rank, None);
    // Emitted once: a further empty sample has nothing left to say.
    let again = tracker.diff(&[], &[], at(2), Duration::seconds(600));
    assert!(again.is_empty());
}

#[test]
fn a_key_in_a_repo_whose_listing_failed_emits_nothing_and_keeps_its_state() {
    let mut tracker = DispositionTracker::default();
    let first = [row("acme/widgets", 1, 1, Qd::DeferredSaturation)];
    tracker.diff(&first, &[], at(0), Duration::seconds(600));
    let failed = vec!["acme/widgets".to_string()];
    let emissions = tracker.diff(&[], &failed, at(1), Duration::seconds(600));
    assert!(emissions.is_empty(), "{emissions:?}");
    // State survived: a later successful listing with the SAME disposition
    // inside the refresh window still emits nothing.
    let still_same = tracker.diff(&first, &[], at(2), Duration::seconds(600));
    assert!(still_same.is_empty());
}

// ------------------------------------------------------------------------ build_rows

fn summary_row(
    repo: &str,
    issue: u32,
    rank: usize,
    disposition: Qd,
    detail: Option<&str>,
) -> ReadyQueueRow {
    ReadyQueueRow {
        rank,
        repo: repo.to_string(),
        issue,
        workspace_priority: 100,
        urgent: false,
        operator_priority: false,
        operator_priority_at: None,
        main_red_fix: false,
        created_at: None,
        tier: None,
        disposition,
        detail: detail.map(str::to_string),
        state: disposition.state().to_string(),
        reason: disposition.reason().to_string(),
        plan: Default::default(),
    }
}

fn repo_ref(slug: &str) -> QueueRepoRef {
    QueueRepoRef {
        repo: slug.to_string(),
        visibility: RepoVisibility::Public,
    }
}

#[test]
fn an_unresolved_root_is_dropped_and_counted() {
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/unresolved/root",
            1,
            1,
            Qd::DeferredCapacity,
            None,
        )],
        ..Default::default()
    };
    let (rows, dropped) = build_rows(&summary, &HashMap::new());
    assert!(rows.is_empty());
    assert_eq!(dropped.unresolved, 1);
    assert_eq!(dropped.truncated, 0);
}

#[test]
fn more_than_256_rows_are_truncated_and_counted() {
    let mut repos = HashMap::new();
    repos.insert("/repo".to_string(), repo_ref("acme/widgets"));
    let queue: Vec<ReadyQueueRow> = (0..300)
        .map(|i| summary_row("/repo", i, i as usize + 1, Qd::DeferredCapacity, None))
        .collect();
    let summary = WorkFinderTickSummary {
        queue,
        ..Default::default()
    };
    let (rows, dropped) = build_rows(&summary, &repos);
    assert_eq!(rows.len(), MAX_ROWS);
    assert_eq!(dropped.truncated, 300 - MAX_ROWS);
    assert_eq!(dropped.unresolved, 0);
}

#[test]
fn loom_repo_is_the_resolved_slug_never_the_local_path() {
    let mut repos = HashMap::new();
    repos.insert("/Users/joseph/dev/loom".to_string(), repo_ref("rjwalters/loom"));
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/Users/joseph/dev/loom",
            42,
            1,
            Qd::DeferredCapacity,
            None,
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].slug, "rjwalters/loom");
    assert!(!rows[0].slug.starts_with('/'));
}

/// Issue #9669: every row carries its 1-indexed queue position (the plan's
/// pass-2 position when annotated, else the comparator rank — a blocked row's
/// would-be position), the tick's ready-queue depth as the denominator, and
/// the comparator keys as the `priority_score` JSON object.
#[test]
fn build_rows_extracts_queue_position_metadata() {
    use crate::types::PlanKey;

    let key = |name: &str, value: serde_json::Value| PlanKey {
        name: name.to_string(),
        value,
    };
    let mut repos = HashMap::new();
    repos.insert("/repo".to_string(), repo_ref("acme/widgets"));
    let mut planned = summary_row("/repo", 7, 3, Qd::DeferredCapacity, None);
    planned.plan.position = Some(2);
    planned.plan.keys = vec![
        key("operator_priority", serde_json::Value::Bool(false)),
        key("main_red_fix", serde_json::Value::Bool(false)),
        key("workspace_priority", serde_json::json!(100)),
    ];
    // Issue 9 is blocked (no plan position): its rank is the fallback.
    let mut blocked = summary_row("/repo", 9, 1, Qd::Parked, Some("loom:blocked"));
    blocked.plan.keys = planned.plan.keys.clone();
    let summary = WorkFinderTickSummary {
        queue: vec![planned, blocked],
        ..Default::default()
    };
    let (rows, dropped) = build_rows(&summary, &repos);
    assert!(dropped.unresolved == 0 && dropped.truncated == 0);
    assert_eq!(rows.len(), 2);
    // Every row sees the same denominator: the tick's ready-queue depth.
    assert_eq!(rows.iter().map(|r| r.total_candidates).collect::<Vec<_>>(), [2, 2]);
    // The planned row's candidate_rank is its pass-2 position, not its rank.
    assert_eq!(rows[0].candidate_rank, 2);
    // The blocked row has no plan position: the comparator rank stands in.
    assert_eq!(rows[1].candidate_rank, 1);
    // priority_score is the comparator keys, compact JSON in key order.
    let expected = r#"{"operator_priority":false,"main_red_fix":false,"workspace_priority":100}"#;
    assert_eq!(rows[0].priority_score.as_deref(), Some(expected));
    assert_eq!(rows[1].priority_score.as_deref(), Some(expected));
}

/// Issue #9669: a row without a plan annotation (no keys) carries no
/// `priority_score` — the attribute is omitted, never fabricated.
#[test]
fn a_row_without_plan_keys_has_no_priority_score() {
    let mut repos = HashMap::new();
    repos.insert("/repo".to_string(), repo_ref("acme/widgets"));
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row("/repo", 7, 1, Qd::DeferredCapacity, None)],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].priority_score, None);
}

// ------------------------------------------------------------------ park_label / pr_number

#[test]
fn park_label_is_present_only_for_a_label_in_the_closed_vocabulary() {
    // A real park label (in PARK_LABELS) survives.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row("/repo", 1, 1, Qd::Parked, Some("loom:blocked"))],
        ..Default::default()
    };
    let mut repos = HashMap::new();
    repos.insert("/repo".to_string(), repo_ref("acme/widgets"));
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].park_label.as_deref(), Some("loom:blocked"));

    // A repo-configured extra skip label is NOT in PARK_LABELS ∪ SKIP_LABELS:
    // dropped even though the disposition is Parked.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/repo",
            2,
            1,
            Qd::Parked,
            Some("team:custom-hold"),
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].park_label, None);

    // HardExclusion's rule string ("external") is not in the closed
    // vocabulary either.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/repo",
            3,
            1,
            Qd::HardExclusion,
            Some("external"),
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].park_label, None);

    // A disposition that never carries a park label ignores detail text
    // entirely, however label-shaped it looks.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/repo",
            4,
            1,
            Qd::DeferredCapacity,
            Some("loom:blocked"),
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].park_label, None);
}

#[test]
fn pr_number_is_present_only_for_open_pr_rows() {
    let mut repos = HashMap::new();
    repos.insert("/repo".to_string(), repo_ref("acme/widgets"));
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row("/repo", 1, 1, Qd::OpenPr, Some("open PR #456"))],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].pr_number, Some(456));

    // Free-form dispatch-error text is never exported, even as a number.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/repo",
            2,
            1,
            Qd::DispatchError,
            Some("spawn failed: #500"),
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].pr_number, None);
}

// --------------------------------------------------------------------------- build_span

fn changed_emission() -> Emission {
    Emission {
        slug: "acme/widgets".to_string(),
        visibility: RepoVisibility::Public,
        issue: 98,
        rank: Some(3),
        disposition: Qd::DeferredSaturation,
        previous_disposition: Some(Qd::DeferredCapacity),
        transition: Transition::Changed,
        park_label: Some("loom:blocked".to_string()),
        pr_number: None,
        candidate_rank: Some(2),
        total_candidates: Some(7),
        priority_score: Some(
            r#"{"operator_priority":false,"main_red_fix":false,"workspace_priority":100}"#
                .to_string(),
        ),
    }
}

#[test]
fn every_attribute_survives_the_export_time_allowlist() {
    let span = build_span(&changed_emission(), Utc::now(), None);
    assert_eq!(span.clone().bounded().attributes, span.attributes);
    assert_eq!(span.name, SpanName::DispatchDisposition);
    assert_eq!(span.attributes["loom.repo"], "acme/widgets");
    assert_eq!(span.attributes["loom.repo.visibility"], "public");
    assert_eq!(span.attributes["loom.issue"], "98");
    assert_eq!(span.attributes["loom.queue.disposition"], "deferred_saturation");
    assert_eq!(span.attributes["loom.queue.state"], "ready");
    assert_eq!(span.attributes["loom.queue.rank"], "3");
    // Queue-position metadata (Issue #9669).
    assert_eq!(span.attributes["loom.queue.candidate_rank"], "2");
    assert_eq!(span.attributes["loom.queue.total_candidates"], "7");
    assert_eq!(
        span.attributes["loom.queue.priority_score"],
        r#"{"operator_priority":false,"main_red_fix":false,"workspace_priority":100}"#
    );
    assert_eq!(span.attributes["loom.queue.transition"], "changed");
    assert_eq!(span.attributes["loom.queue.previous_disposition"], "deferred_capacity");
    assert_eq!(span.attributes["loom.queue.park_label"], "loom:blocked");
    assert!(span.validate().is_ok());
}

#[test]
fn left_queue_emission_omits_rank_and_previous_disposition() {
    let emission = Emission {
        transition: Transition::LeftQueue,
        rank: None,
        previous_disposition: None,
        park_label: None,
        pr_number: None,
        candidate_rank: None,
        total_candidates: None,
        priority_score: None,
        ..changed_emission()
    };
    let span = build_span(&emission, Utc::now(), None);
    assert!(!span.attributes.contains_key("loom.queue.rank"));
    // Issue #9669: a row that left the queue is no longer ranked, so none of
    // the queue-position metadata is exported either.
    assert!(!span.attributes.contains_key("loom.queue.candidate_rank"));
    assert!(!span.attributes.contains_key("loom.queue.total_candidates"));
    assert!(!span.attributes.contains_key("loom.queue.priority_score"));
    assert!(!span
        .attributes
        .contains_key("loom.queue.previous_disposition"));
    assert_eq!(span.attributes["loom.queue.transition"], "left_queue");
}

#[test]
fn a_span_is_parented_to_the_tick_when_given_a_context_and_a_root_otherwise() {
    let root_span = build_span(&changed_emission(), Utc::now(), None);
    assert_eq!(root_span.parent_span_id, None);

    let parent = crate::telemetry::trace::TraceContext::derived("dispatch.tick", &["k"]);
    let child_span = build_span(&changed_emission(), Utc::now(), Some(&parent));
    assert_eq!(child_span.parent_span_id, Some(parent.span_id.clone()));
    assert_eq!(child_span.context.trace_id, parent.trace_id);
    assert_ne!(child_span.context.span_id, parent.span_id);
}

// --------------------------------------------------------------------------- record()

#[tokio::test]
async fn record_without_sink_is_a_noop() {
    // No ops sink is registered in this test binary: nothing panics, nothing
    // emits — mirrors `ops::dwell::record_tick_without_sink_is_a_noop`.
    let mut slug_cache = HashMap::new();
    record(&mut slug_cache).await;
}

#[test]
fn dropped_points_emit_one_delta_counter_per_nonzero_reason() {
    assert!(dropped_points(DroppedCounts::default()).is_empty());
    let points = dropped_points(DroppedCounts {
        unresolved: 2,
        truncated: 0,
    });
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].name, MetricName::QueueDispositionRowsDropped);
    assert_eq!(points[0].labels["reason"], "unresolved");
}

#[test]
fn refresh_secs_falls_back_to_the_default_on_bad_input() {
    assert_eq!(refresh_secs(None), DEFAULT_REFRESH_SECS);
    assert_eq!(refresh_secs(Some("garbage")), DEFAULT_REFRESH_SECS);
    assert_eq!(refresh_secs(Some("0")), DEFAULT_REFRESH_SECS);
    assert_eq!(refresh_secs(Some("120")), 120);
}
