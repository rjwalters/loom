//! Work-finder tick telemetry (Issue #8860): one `loom.dispatch.tick` span per
//! tick, plus the tick's candidate outcomes as `loom.dispatch.decisions` delta
//! counters labelled with an explicit `reason`, and (Issue #8907) one
//! `loom.dispatch.admission` child span per `dispatch()` attempt.
//!
//! Everything here is derived from the [`TickReport`] the tick already
//! produces for `loom-daemon health` (#4761), so the exported reasons cannot
//! disagree with the per-tick summary log line. The mapping is pure;
//! [`record_tick`] is the only side effect, and it returns before building
//! anything when no ops sink is registered.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::queue_snapshot::QueueRepoRef;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use crate::work_finder::{Admission, TickReport};

/// Every `reason` label value, paired with its [`TickReport`] counter. Zero
/// counters are not emitted. `error` counts failed dispatches and failed
/// ready-issue listings.
///
/// The typed-refusal counters (#8907) are subsets of `skipped_backoff`
/// (`lease_order_lost`) and `errors` (`token_selection_failed`,
/// `claim_collision`, `claim_lock_held`), so their parents are reported net of
/// them: every candidate lands in exactly one reason.
#[must_use]
pub fn decision_counts(report: &TickReport) -> [(&'static str, usize); 26] {
    let classified_errors =
        report.refused_token_selection + report.refused_claim_collision + report.refused_claim_lock;
    [
        ("dispatched", report.dispatched),
        ("labeled", report.skipped_labeled),
        ("in_flight", report.skipped_in_flight),
        ("quarantined", report.skipped_quarantined),
        ("workspace_commands_missing", report.skipped_workspace_commands_missing),
        ("pr_open", report.skipped_pr_open),
        ("peer_claim", report.skipped_peer_claim),
        (
            "backoff",
            report
                .skipped_backoff
                .saturating_sub(report.refused_lease_order),
        ),
        ("pr_open_backoff", report.skipped_pr_open_backoff),
        ("noop_cooldown", report.skipped_noop_cooldown),
        ("declined", report.skipped_declined),
        ("prless_retry", report.skipped_prless_retry),
        ("recheck_interval", report.skipped_recheck_interval),
        ("host_constraint", report.skipped_host_constraint),
        ("host_class", report.skipped_host_class),
        ("capacity", report.deferred_capacity),
        ("ramp_cap", report.deferred_ramp_cap),
        ("saturation", report.deferred_saturation),
        ("build_backoff", report.deferred_build_backoff),
        ("out_of_slice", report.deferred_out_of_slice),
        ("repo_cap", report.deferred_repo_cap),
        ("error", report.errors.saturating_sub(classified_errors)),
        // Typed dispatch refusals (Issue #8907) — one contiguous block.
        ("lease_order_lost", report.refused_lease_order),
        ("token_selection_failed", report.refused_token_selection),
        ("claim_collision", report.refused_claim_collision),
        ("claim_lock_held", report.refused_claim_lock),
    ]
}

/// The tick's single outcome, first match wins:
///
/// | value | when |
/// |---|---|
/// | `dispatched` | at least one sweep started |
/// | `halted_main_red` | a main-health gate held a workspace |
/// | `saturation_held` | the saturation admission brake was engaged |
/// | `build_backoff_held` | the build back-off (#9410) was engaged |
/// | `error` | a dispatch or listing failed and nothing started |
/// | `no_eligible_work` | no ready candidates at all |
/// | `capacity_full` | candidates deferred by the concurrency or ramp cap |
/// | `all_skipped` | every candidate was skipped for a per-issue reason |
#[must_use]
pub fn tick_result(report: &TickReport) -> &'static str {
    if report.dispatched > 0 {
        "dispatched"
    } else if report.halted {
        "halted_main_red"
    } else if report.saturation_held {
        "saturation_held"
    } else if report.build_backoff_held {
        "build_backoff_held"
    } else if report.errors > 0 {
        "error"
    } else if report.seen == 0 {
        "no_eligible_work"
    } else if report.deferred_capacity + report.deferred_ramp_cap > 0 {
        "capacity_full"
    } else {
        "all_skipped"
    }
}

fn int(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// The tick's metric points: non-zero decision counters plus the candidate
/// and cap gauges.
#[must_use]
pub fn tick_points(report: &TickReport, max_concurrent: usize) -> Vec<MetricPoint> {
    let mut points: Vec<MetricPoint> = decision_counts(report)
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(reason, count)| {
            MetricPoint::int(MetricName::DispatchDecisions, int(count)).label("reason", reason)
        })
        .collect();
    points.push(MetricPoint::int(MetricName::DispatchCandidates, int(report.seen)));
    points.push(MetricPoint::int(MetricName::DispatchMaxConcurrent, int(max_concurrent)));
    points
}

/// The tick's span: its own sampled root trace covering `started_at..ended_at`,
/// its ID derived from the tick's start instant.
#[must_use]
pub fn tick_span(
    report: &TickReport,
    max_concurrent: usize,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) -> SpanRecord {
    let result = tick_result(report);
    let attributes: TraceAttributes = [
        ("loom.dispatch.result", result.to_string()),
        ("loom.dispatch.seen", report.seen.to_string()),
        ("loom.dispatch.dispatched", report.dispatched.to_string()),
        ("loom.dispatch.errors", report.errors.to_string()),
        ("loom.dispatch.max_concurrent", max_concurrent.to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    let mut attributes = attributes;
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context: TraceContext::derived(
            SpanName::DispatchTick.as_str(),
            &[&crate::telemetry::trace::instant(started_at)],
        ),
        parent_span_id: None,
        name: SpanName::DispatchTick,
        started_at,
        ended_at: ended_at.max(started_at),
        status: if result == "error" {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        },
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// One `loom.dispatch.admission` child span per `dispatch()` attempt the
/// tick made (Issue #8907), parented to `tick` (the tick's own span) and
/// clamped inside it. Attributes: `loom.issue`, `loom.dispatch.admission_result`
/// and `loom.dispatch.reason` (the `loom.dispatch.decisions` reason), plus
/// `loom.repo`/`loom.repo.visibility` (Issue #9222) when `repo_refs` has an
/// entry for the admission's `workspace_idx` — omitted, never a local path,
/// when it does not.
///
/// A `pr_open` admission (the #4123 guard's skip) in a repo `lockouts` says
/// is locked also carries that repo's lockout attributes (Issue #9674):
/// `lockout.frozen_candidates_count`, `lockout.frozen_points_sum` and
/// `lockout.duration_seconds` — the same facts the
/// `loom.dispatch.disposition` span for the same refusal carries, on the
/// tick-time span an operator reaches first.
#[must_use]
pub fn admission_spans(
    report: &TickReport,
    tick: &SpanRecord,
    repo_refs: &HashMap<usize, QueueRepoRef>,
    lockouts: &HashMap<String, crate::observability::ops::lockout::RepoLockout>,
) -> Vec<SpanRecord> {
    report
        .admissions
        .iter()
        .map(|admission| {
            let started_at = admission.started_at.clamp(tick.started_at, tick.ended_at);
            let mut attributes: TraceAttributes = [
                ("loom.issue", admission.issue.to_string()),
                ("loom.dispatch.admission_result", admission.result.to_string()),
                ("loom.dispatch.reason", admission.reason.to_string()),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
            if let Some(repo_ref) = repo_refs.get(&admission.workspace_idx) {
                attributes.insert("loom.repo".to_string(), repo_ref.repo.clone());
                attributes.insert(
                    "loom.repo.visibility".to_string(),
                    visibility_str(repo_ref.visibility).to_string(),
                );
                // Issue #9674: stamp the repo's lockout weight on the very
                // admission spans the open-PR guard produced.
                if admission.reason == "pr_open" {
                    if let Some(lockout) = lockouts.get(&repo_ref.repo) {
                        attributes.insert(
                            "lockout.frozen_candidates_count".to_string(),
                            lockout.backlog.candidates.to_string(),
                        );
                        attributes.insert(
                            "lockout.frozen_points_sum".to_string(),
                            i64::try_from(lockout.backlog.points)
                                .unwrap_or(i64::MAX)
                                .to_string(),
                        );
                        if let Some(secs) = lockout.duration_secs {
                            attributes
                                .insert("lockout.duration_seconds".to_string(), secs.to_string());
                        }
                    }
                }
            }
            crate::telemetry::trace::provenance::stamp(&mut attributes);
            SpanRecord {
                context: tick.context.derived_child(&[
                    SpanName::DispatchAdmission.as_str(),
                    &admission.issue.to_string(),
                    &crate::telemetry::trace::instant(started_at),
                ]),
                parent_span_id: Some(tick.context.span_id.clone()),
                name: SpanName::DispatchAdmission,
                started_at,
                ended_at: admission.ended_at.clamp(started_at, tick.ended_at),
                status: if admission.result == "error" {
                    SpanStatus::Error
                } else {
                    SpanStatus::Ok
                },
                attributes,
                events: Vec::new(),
                links: Vec::new(),
            }
        })
        .collect()
}

/// UCUM-free wire text for a repo visibility tag (mirrors the private
/// `visibility_str` helpers in `otlp::mapping` and `ci_telemetry::records` —
/// each ops-adjacent module keeps its own copy rather than sharing one across
/// unrelated call graphs).
fn visibility_str(visibility: crate::telemetry::RepoVisibility) -> &'static str {
    match visibility {
        crate::telemetry::RepoVisibility::Public => "public",
        crate::telemetry::RepoVisibility::Private => "private",
    }
}

/// The workspace roots referenced by `admissions` and by `report`'s ready-
/// queue rows, resolved through the synchronous
/// [`super::super::repo_ref::cached_repo_ref`] cache the collector populates
/// on its own cadence (Issue #9222). Never shells out itself — a root not yet
/// resolved this process is simply absent from the map, which
/// [`admission_spans`] reads as "omit `loom.repo`". The queue rows are in the
/// resolution set (Issue #9674) so a repo whose candidates were all refused
/// pre-dispatch still resolves, keeping the lockout aggregation slug-keyed.
fn resolved_repo_refs(
    admissions: &[Admission],
    queue: &[crate::work_finder::ready_queue::TickQueueRow],
    roots: &[PathBuf],
) -> HashMap<usize, QueueRepoRef> {
    let mut resolved = HashMap::new();
    let idxs = admissions
        .iter()
        .map(|a| a.workspace_idx)
        .chain(queue.iter().map(|r| r.key.workspace_idx));
    for idx in idxs {
        if resolved.contains_key(&idx) {
            continue;
        }
        let Some(root) = roots.get(idx) else {
            continue;
        };
        if let Some(repo_ref) = super::super::repo_ref::cached_repo_ref(&root.display().to_string())
        {
            resolved.insert(idx, repo_ref);
        }
    }
    resolved
}

/// A tick's completion instant paired with its trace context.
type TickContext = (DateTime<Utc>, TraceContext);

/// The most recently recorded tick's completion instant and trace context, so
/// `ops::disposition` (Issue #9222) can parent its spans to the same tick when
/// its summary's `at` matches, and emit roots otherwise (a restart, or a
/// sample that lands between two ticks). Read via [`last_tick_context`],
/// written only by [`record_tick`].
static LAST_TICK: OnceLock<Mutex<Option<TickContext>>> = OnceLock::new();

fn last_tick_slot() -> &'static Mutex<Option<TickContext>> {
    LAST_TICK.get_or_init(|| Mutex::new(None))
}

/// The `(completed_at, trace_context)` of the most recent tick this process
/// exported, or `None` before the first one.
#[must_use]
pub fn last_tick_context() -> Option<TickContext> {
    last_tick_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Export one completed tick: its span, one admission span per dispatch
/// attempt, and its metric points. Returns immediately when no ops sink is
/// registered (observability off, or no OTLP exporter). `completed_at` is the
/// same instant the work-finder's `publish_tick` stamps on the tick's
/// [`crate::types::WorkFinderTickSummary::at`], so `ops::disposition` can tell
/// whether a later sample's summary is this exact tick (see
/// [`last_tick_context`]). `roots` names each admission's workspace (Issue
/// #9222); empty for the single-workspace loop, exactly like every other
/// `roots` parameter in this package.
pub fn record_tick(
    report: &TickReport,
    max_concurrent: usize,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    roots: &[PathBuf],
) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    // Issue #9674: aggregate this tick's pr-open-skip rows per repo, advance
    // the global lockout clock at the tick boundary (insert-if-absent; the
    // disposition sampler is the clearing authority), and stamp the repo's
    // weight on the guard's own `pr_open` admission spans below. A workspace
    // whose listing failed this tick is absent from `report.queue` without
    // that being evidence of a cleared lock — mirror the disposition sampler's
    // failed-repo discipline (unresolvable roots are dropped, the same
    // accepted edge case its own `failed_slugs` documents).
    let repo_refs = resolved_repo_refs(&report.admissions, &report.queue, roots);
    let failed: Vec<String> = report
        .listing_failed
        .iter()
        .filter_map(|idx| repo_refs.get(idx).map(|r| r.repo.clone()))
        .collect();
    let lockouts = crate::observability::ops::lockout::sample(
        report.queue.iter().filter_map(|row| {
            let slug = repo_refs.get(&row.key.workspace_idx)?.repo.as_str();
            let disposition = row.disposition?;
            Some((slug, disposition, row.story_points))
        }),
        &failed,
        completed_at,
    );
    let tick = tick_span(report, max_concurrent, started_at, completed_at);
    for span in admission_spans(report, &tick, &repo_refs, &lockouts) {
        sink.emit_span(span);
    }
    *last_tick_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((completed_at, tick.context.clone()));
    sink.emit_span(tick);
    sink.emit_metrics_since(tick_points(report, max_concurrent), Some(started_at));
}
