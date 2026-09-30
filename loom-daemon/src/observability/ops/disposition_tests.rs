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
        story_points: None,
        halt_cause: None,
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
        story_points: None,
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

    // HardExclusion's rule string ("external") IS in its own closed
    // vocabulary now (#9672): the span names which rule declined the issue.
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
    assert_eq!(rows[0].park_label.as_deref(), Some("external"));

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

// --------------------------------------------------------------------------- halt_cause (#9673)

fn repos_with_repo() -> HashMap<String, QueueRepoRef> {
    let mut repos = HashMap::new();
    repos.insert("/repo".to_string(), repo_ref("acme/widgets"));
    repos
}

#[test]
fn halt_cause_is_present_only_for_a_closed_vocabulary_detail() {
    let repos = repos_with_repo();

    // A real #9017 halt-cause token survives, verbatim.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/repo",
            1,
            1,
            Qd::WorkspaceHalted,
            Some("main_red"),
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].halt_cause.as_deref(), Some("main_red"));

    // A cause-less legacy row (a pre-#9017 caller) exports nothing.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row("/repo", 2, 1, Qd::WorkspaceHalted, None)],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].halt_cause, None);

    // A detail token outside the closed vocabulary is never exported.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/repo",
            3,
            1,
            Qd::WorkspaceHalted,
            Some("sorta_halted"),
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].halt_cause, None);

    // A disposition that never carries a halt cause ignores detail text
    // entirely, however cause-shaped it looks.
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/repo",
            4,
            1,
            Qd::DeferredCapacity,
            Some("main_red"),
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].halt_cause, None);
}

#[test]
fn every_halt_cause_token_round_trips_through_the_wire_vocabulary() {
    // The exporter re-emits `HaltCause::from_wire(..)?.as_str()`, so each
    // closed-vocabulary token must survive the round trip unchanged.
    for cause in crate::work_finder::halt_cause::HaltCause::ALL {
        assert_eq!(HaltCause::from_wire(cause.as_str()), Some(cause));
    }
}

#[test]
fn workspace_halted_emission_carries_the_halt_cause_attribute() {
    let emission = Emission {
        disposition: Qd::WorkspaceHalted,
        park_label: None,
        halt_cause: Some("token_pool".to_string()),
        ..changed_emission()
    };
    let span = build_span(&emission, Utc::now(), None, &HashMap::new());
    assert_eq!(span.attributes["loom.queue.disposition"], "workspace_halted");
    assert_eq!(span.attributes["loom.queue.state"], "blocked");
    assert_eq!(span.attributes["loom.queue.halt_cause"], "token_pool");
    // The key must survive the export-time attribute allowlist
    // (`OPS_SPAN_ATTRIBUTE_KEYS` is re-applied at export), or the attribute
    // would silently never reach SigNoz.
    let bounded = span.clone().bounded();
    assert_eq!(bounded.attributes["loom.queue.halt_cause"], "token_pool");
    assert!(span.validate().is_ok());

    // No halt cause, no attribute — a cause-less legacy row stays
    // indistinguishable from a pre-#9673 span rather than exporting an empty
    // value.
    let emission = Emission {
        disposition: Qd::WorkspaceHalted,
        park_label: None,
        halt_cause: None,
        ..changed_emission()
    };
    let span = build_span(&emission, Utc::now(), None, &HashMap::new());
    assert!(!span.attributes.contains_key("loom.queue.halt_cause"));
}

// ------------------------------------------------------------------ hard_exclusion label (#9672)

#[test]
fn hard_exclusion_rows_export_exactly_the_closed_exclusion_labels() {
    let mut repos = HashMap::new();
    repos.insert("/repo".to_string(), repo_ref("acme/widgets"));

    // Every HARD_EXCLUSION_LABELS entry exports verbatim, so a span can say
    // WHICH rule declined the issue.
    for label in crate::hard_exclusion::HARD_EXCLUSION_LABELS {
        let summary = WorkFinderTickSummary {
            queue: vec![summary_row("/repo", 1, 1, Qd::HardExclusion, Some(label))],
            ..Default::default()
        };
        let (rows, _) = build_rows(&summary, &repos);
        assert_eq!(rows[0].park_label.as_deref(), Some(*label));
    }

    // A detail string outside every closed vocabulary still never exports —
    // including one on a HardExclusion row (forward compatibility for rules
    // not yet in HARD_EXCLUSION_LABELS).
    let summary = WorkFinderTickSummary {
        queue: vec![summary_row(
            "/repo",
            2,
            1,
            Qd::HardExclusion,
            Some("team:never-build"),
        )],
        ..Default::default()
    };
    let (rows, _) = build_rows(&summary, &repos);
    assert_eq!(rows[0].park_label, None);
}

#[test]
fn hard_exclusion_emission_carries_the_label_attribute_through_the_export_allowlist() {
    let emission = Emission {
        disposition: Qd::HardExclusion,
        park_label: Some("external".to_string()),
        halt_cause: None,
        ..changed_emission()
    };
    let span = build_span(&emission, Utc::now(), None, &HashMap::new());
    assert_eq!(span.attributes["loom.queue.disposition"], "hard_exclusion");
    assert_eq!(span.attributes["loom.queue.park_label"], "external");
    // `loom.queue.park_label` is already in both the span allowlist and the
    // collector gateway's keep_keys, so reusing it for #9672 adds no new
    // allowlist surface — assert the export-time bound anyway.
    let bounded = span.clone().bounded();
    assert_eq!(bounded.attributes["loom.queue.park_label"], "external");
    assert!(span.validate().is_ok());
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
        halt_cause: None,
    }
}

#[test]
fn every_attribute_survives_the_export_time_allowlist() {
    let span = build_span(&changed_emission(), Utc::now(), None, &HashMap::new());
    assert_eq!(span.clone().bounded().attributes, span.attributes);
    assert_eq!(span.name, SpanName::DispatchDisposition);
    assert_eq!(span.attributes["loom.repo"], "acme/widgets");
    assert_eq!(span.attributes["loom.repo.visibility"], "public");
    assert_eq!(span.attributes["loom.issue"], "98");
    assert_eq!(span.attributes["loom.queue.disposition"], "deferred_saturation");
    assert_eq!(span.attributes["loom.queue.state"], "ready");
    assert_eq!(span.attributes["loom.queue.rank"], "3");
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
        ..changed_emission()
    };
    let span = build_span(&emission, Utc::now(), None, &HashMap::new());
    assert!(!span.attributes.contains_key("loom.queue.rank"));
    assert!(!span
        .attributes
        .contains_key("loom.queue.previous_disposition"));
    assert_eq!(span.attributes["loom.queue.transition"], "left_queue");
}

#[test]
fn a_span_is_parented_to_the_tick_when_given_a_context_and_a_root_otherwise() {
    let root_span = build_span(&changed_emission(), Utc::now(), None, &HashMap::new());
    assert_eq!(root_span.parent_span_id, None);

    let parent = crate::telemetry::trace::TraceContext::derived("dispatch.tick", &["k"]);
    let child_span = build_span(&changed_emission(), Utc::now(), Some(&parent), &HashMap::new());
    assert_eq!(child_span.parent_span_id, Some(parent.span_id.clone()));
    assert_eq!(child_span.context.trace_id, parent.trace_id);
    assert_ne!(child_span.context.span_id, parent.span_id);
}

// --------------------------------------------------------------------------- lockout stamping (Issue #9674)

use super::super::lockout::{FrozenBacklog, RepoLockout};

fn locked_map(
    slug: &str,
    backlog: FrozenBacklog,
    duration: Option<i64>,
) -> HashMap<String, RepoLockout> {
    HashMap::from([(
        slug.to_string(),
        RepoLockout {
            backlog,
            duration_secs: duration,
        },
    )])
}

/// An `open_pr` span in a locked repo carries the repo's whole lockout
/// weight: candidate count, summed story points, and the observation-floor
/// clock — and everything survives the export-time allowlist, so the facts
/// actually reach SigNoz.
#[test]
fn an_open_pr_span_in_a_locked_repo_carries_the_lockout_weight() {
    let emission = Emission {
        disposition: Qd::OpenPr,
        park_label: None,
        pr_number: Some(456),
        ..changed_emission()
    };
    let lockouts = locked_map(
        "acme/widgets",
        FrozenBacklog {
            candidates: 51,
            points: 97,
        },
        Some(3_600),
    );
    let span = build_span(&emission, Utc::now(), None, &lockouts);
    assert_eq!(span.attributes["loom.queue.disposition"], "open_pr");
    assert_eq!(span.attributes["lockout.frozen_candidates_count"], "51");
    assert_eq!(span.attributes["lockout.frozen_points_sum"], "97");
    assert_eq!(span.attributes["lockout.duration_seconds"], "3600");
    assert_eq!(span.clone().bounded().attributes, span.attributes);
}

/// The count/sum attributes are the lock itself — stamped even while the
/// clock has no reading yet — while a repo under no lock stays silent.
#[test]
fn lockout_attributes_are_absent_without_a_lock_or_outside_open_pr_rows() {
    // OpenPr row, but this sample's rows say the repo is not locked.
    let emission = Emission {
        disposition: Qd::OpenPr,
        pr_number: Some(456),
        park_label: None,
        ..changed_emission()
    };
    let span = build_span(&emission, Utc::now(), None, &HashMap::new());
    assert!(!span
        .attributes
        .contains_key("lockout.frozen_candidates_count"));
    assert!(!span.attributes.contains_key("lockout.frozen_points_sum"));
    assert!(!span.attributes.contains_key("lockout.duration_seconds"));

    // Locked repo, but the row is a different disposition: the repo's other
    // rows do not double-report the lock.
    let lockouts = locked_map(
        "acme/widgets",
        FrozenBacklog {
            candidates: 2,
            points: 5,
        },
        Some(30),
    );
    let span = build_span(&changed_emission(), Utc::now(), None, &lockouts);
    assert!(!span
        .attributes
        .contains_key("lockout.frozen_candidates_count"));

    // A zero-duration clock (first observation) still stamps `0`, not absence.
    let emission = Emission {
        disposition: Qd::OpenPr,
        pr_number: Some(1),
        park_label: None,
        ..changed_emission()
    };
    let lockouts = locked_map(
        "acme/widgets",
        FrozenBacklog {
            candidates: 1,
            points: 0,
        },
        Some(0),
    );
    let span = build_span(&emission, Utc::now(), None, &lockouts);
    assert_eq!(span.attributes["lockout.duration_seconds"], "0");
}

/// A `left_queue` span never reports lock state: the row has already left the
/// queue, so the repo's current lock is not this row's story.
#[test]
fn a_left_queue_span_never_carries_lockout_attributes() {
    let emission = Emission {
        transition: Transition::LeftQueue,
        rank: None,
        previous_disposition: None,
        park_label: None,
        pr_number: None,
        disposition: Qd::OpenPr,
        ..changed_emission()
    };
    let lockouts = locked_map(
        "acme/widgets",
        FrozenBacklog {
            candidates: 1,
            points: 3,
        },
        Some(60),
    );
    let span = build_span(&emission, Utc::now(), None, &lockouts);
    assert_eq!(span.attributes["loom.queue.transition"], "left_queue");
    assert!(!span
        .attributes
        .contains_key("lockout.frozen_candidates_count"));
    assert!(!span.attributes.contains_key("lockout.duration_seconds"));
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
