//! Measured cost of Loom's own lifecycle instrumentation on a representative run.
//!
//! The acceptance question this answers is narrow and checkable: when Loom
//! traces a sweep, how much wall time and how many bytes does the
//! instrumentation itself add, and do the emitted attributes/events stay
//! inside their declared caps?
//!
//! Everything here drives the **real** instrumentation path — the same
//! [`super::lifecycle`] entry points a dispatched sweep uses, the same
//! [`crate::telemetry::trace::journal::Journal`] fsync per boundary, the same
//! [`super::lifecycle::backfill`] drain onto a [`super::queue::DurableQueue`],
//! and the same [`crate::telemetry::trace::SpanRecord::bounded`] emission
//! policy. It is deliberately **not** the synthetic
//! [`crate::telemetry::fixture`] generator, which hand-builds span records and
//! therefore could not measure instrumentation cost at all.
//!
//! # What is measured, and what is not
//!
//! Measured: the added wall time of the instrumented lifecycle over the same
//! call sequence with tracing disabled, the journal bytes persisted, the
//! post-bounding record bytes handed to the exporter, and the realised
//! attribute/event/link bounds.
//!
//! Not measured, and never reported as if it were: network export latency to
//! a real backend, backend ingestion/indexing cost, and any provider-side
//! cost. A run with no model work also has no model cost to attribute; this
//! harness never calls a provider.
//!
//! # Reference duration
//!
//! An absolute "added 30ms" number means nothing without the run it is added
//! to. The reference denominator is read from this host's **own** observed
//! sweep outcomes ([`crate::sweep_outcomes::read_all`]) rather than invented,
//! and the report names the sample size and percentile it used. When no
//! observed history exists the fraction is reported as absent — never as
//! zero, and never as a guess.

use crate::telemetry::trace::{journal::Journal, store::TraceStore, SpanName, SpanStatus};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

/// One phase of the representative lifecycle, in the shape the span model
/// declares: `loom.phase` -> `loom.role_attempt` -> preflight/run -> tools.
#[derive(Debug, Clone, Copy)]
pub struct PhaseShape {
    pub role: &'static str,
    pub result: &'static str,
    pub status: SpanStatus,
}

/// The repair waterfall the issue names as the hard case: a Builder success, a
/// Judge rejection, a Doctor recovery, a second Judge acceptance, then merge.
/// Deliberately the *longest* ordinary lifecycle, so the reported overhead is
/// an upper bound among normal outcomes rather than a best case.
pub const REPAIR_CYCLE: &[PhaseShape] = &[
    PhaseShape {
        role: "builder",
        result: "success",
        status: SpanStatus::Ok,
    },
    PhaseShape {
        role: "judge",
        result: "rejected",
        status: SpanStatus::Error,
    },
    PhaseShape {
        role: "doctor",
        result: "success",
        status: SpanStatus::Ok,
    },
    PhaseShape {
        role: "judge",
        result: "success",
        status: SpanStatus::Ok,
    },
    PhaseShape {
        role: "merge",
        result: "success",
        status: SpanStatus::Ok,
    },
];

/// Shape of the run being measured. `tools_per_attempt` is a **parameter**,
/// not a measured fleet average: Loom only owns a `loom.tool` span at its own
/// native-tool bridge boundary, so the true per-attempt count depends on the
/// runtime in use and is not observable from the outcome journal.
#[derive(Debug, Clone)]
pub struct RunShape {
    pub phases: &'static [PhaseShape],
    pub tools_per_attempt: usize,
}

impl Default for RunShape {
    fn default() -> Self {
        Self {
            phases: REPAIR_CYCLE,
            tools_per_attempt: 4,
        }
    }
}

impl RunShape {
    /// Spans one instrumented repetition is expected to persist: the sweep
    /// root, plus phase/attempt/preflight/run/tools for every phase.
    #[must_use]
    pub fn expected_spans(&self) -> usize {
        1 + self.phases.len() * (4 + self.tools_per_attempt)
    }
}

/// Realised emission bounds over every span the measured run actually
/// produced, after [`crate::telemetry::trace::SpanRecord::bounded`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Bounds {
    pub max_attribute_value_bytes: usize,
    pub max_attributes_per_span: usize,
    pub max_events_per_span: usize,
    pub max_links_per_span: usize,
    /// Attribute keys observed on emitted spans. Empty of anything outside the
    /// allowlist is the property that matters; the list is reported so a
    /// reviewer can check it rather than trust a boolean.
    pub attribute_keys: Vec<String>,
}

/// Bytes, not just time: an exporter that is fast but writes megabytes is not
/// cheap. Both numbers are for one repetition of the representative run.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ByteCost {
    /// Journal bytes persisted on disk before the durable drain.
    pub journal_bytes: u64,
    /// Serialized bytes of the bounded span records handed to the exporter.
    /// OTLP adds a fixed per-batch resource/scope envelope that is not counted
    /// here, because it does not scale with the number of spans.
    pub bounded_record_bytes: u64,
    pub bytes_per_span: u64,
}

/// Observed sweep durations this host actually recorded, used as the
/// denominator. `source` names where it came from so a reader never has to
/// guess whether a number was measured or supplied.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Reference {
    pub source: &'static str,
    pub sample_size: usize,
    pub p50_seconds: i64,
    pub p90_seconds: i64,
}

// No `Eq`: `overhead_fraction_of_p50` is a derived ratio, and a float has no
// total equality. `PartialEq` is all a report comparison needs or should claim.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    /// Always true: no provider, forge, or network call occurs in this harness.
    pub offline: bool,
    pub repetitions: usize,
    pub phases: usize,
    pub tools_per_attempt: usize,
    pub spans_per_run: usize,
    /// Median wall time of one fully instrumented representative run.
    pub instrumented_median_ns: u128,
    /// Median wall time of the identical call sequence with tracing disabled.
    pub baseline_median_ns: u128,
    /// Instrumentation cost for one representative run.
    pub added_median_ns: u128,
    pub added_min_ns: u128,
    pub added_max_ns: u128,
    pub added_ns_per_span: u128,
    pub bytes: ByteCost,
    pub bounds: Bounds,
    /// Absent when this host has recorded no sweep outcomes; an unknown
    /// denominator is reported as unknown, never as zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<Reference>,
    /// Added wall time as a fraction of the p50 observed sweep — the
    /// conservative reading, since p50 is the smallest plausible denominator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overhead_fraction_of_p50: Option<f64>,
    /// Named explicitly so no reader mistakes this for end-to-end export cost.
    pub excludes: Vec<&'static str>,
}

/// Percentiles of this host's own recorded sweep durations.
///
/// Nearest-rank selection over the sorted sample, never interpolation, so every
/// reported percentile is a duration this host actually recorded. Over ten
/// records p90 is therefore the ninth value, not the maximum — the maximum is
/// p100, and reporting it as p90 would flatter `overhead_fraction_of_p50`'s
/// sibling reading by inflating the denominator.
///
/// Returns `None` for an absent or empty journal rather than substituting a
/// default: the point of the denominator is that it was observed.
#[must_use]
pub fn observed_reference(workspace: &Path) -> Option<Reference> {
    let path = crate::sweep_outcomes::default_outcomes_path(workspace);
    let mut durations: Vec<i64> = crate::sweep_outcomes::read_all(&path)
        .into_iter()
        .map(|record| record.duration_sec)
        .filter(|seconds| *seconds > 0)
        .collect();
    if durations.is_empty() {
        return None;
    }
    durations.sort_unstable();
    let at = |fraction: f64| {
        let index = ((durations.len() as f64 - 1.0) * fraction).round() as usize;
        durations[index.min(durations.len() - 1)]
    };
    Some(Reference {
        source: "observed_sweep_outcomes",
        sample_size: durations.len(),
        p50_seconds: at(0.5),
        p90_seconds: at(0.9),
    })
}

struct Sample {
    elapsed_ns: u128,
    spans: Vec<crate::telemetry::trace::SpanRecord>,
    journal_bytes: u64,
    bounded_record_bytes: u64,
    bounds: Bounds,
}

/// Execute one representative run against `root`, returning what it cost.
///
/// The same function serves both arms: with tracing disabled every
/// [`super::lifecycle`] entry point short-circuits, so the baseline measures
/// exactly the call sequence minus the instrumentation, not a different
/// program.
fn run_once(root: &Path, shape: &RunShape, execution: &str) -> Result<Sample> {
    let began = std::time::Instant::now();
    let sweep = super::lifecycle::begin(
        root,
        execution,
        SpanName::Sweep,
        super::lifecycle::attributes(&[
            ("loom.sweep_id", execution),
            ("loom.issue", "8525"),
            ("loom.repo", "rjwalters/loom"),
            ("loom.timing_source", "owned_boundary"),
        ]),
    );
    for phase_shape in shape.phases {
        let metadata = super::lifecycle::attributes(&[
            ("loom.role", phase_shape.role),
            ("loom.phase", phase_shape.role),
            ("loom.issue", "8525"),
            ("loom.pr_number", "8579"),
            ("loom.timing_source", "owned_boundary"),
        ]);
        let phase = sweep
            .as_ref()
            .and_then(|s| s.child(SpanName::Phase, metadata.clone()));
        let attempt = phase
            .as_ref()
            .and_then(|p| p.child(SpanName::RoleAttempt, metadata.clone()));
        if let Some(preflight) = attempt
            .as_ref()
            .and_then(|a| a.child(SpanName::RuntimePreflight, metadata.clone()))
        {
            preflight.finish("accepted", SpanStatus::Ok);
        }
        let run = attempt
            .as_ref()
            .and_then(|a| a.child(SpanName::RuntimeRun, metadata.clone()));
        for _ in 0..shape.tools_per_attempt {
            if let Some(tool) = run.as_ref().and_then(|r| {
                let mut tool_metadata = metadata.clone();
                tool_metadata.insert("loom.tool.name".into(), "read".into());
                r.child(SpanName::Tool, tool_metadata)
            }) {
                tool.finish("success", SpanStatus::Ok);
            }
        }
        if let Some(run) = run {
            run.finish(phase_shape.result, phase_shape.status);
        }
        if let Some(attempt) = attempt {
            attempt.finish(phase_shape.result, phase_shape.status);
        }
        if let Some(phase) = phase {
            phase.finish(phase_shape.result, phase_shape.status);
        }
    }
    // Measured before the drain retires the journal.
    let journal_path = Journal::for_context(&TraceStore::new(root).path(root, execution))
        .path()
        .to_path_buf();
    let journal_bytes = std::fs::metadata(&journal_path)
        .map(|m| m.len())
        .unwrap_or(0);
    super::lifecycle::finish_execution(root, execution, "success", Default::default());
    let queue = super::queue::DurableQueue::open(root.join("overhead-queue.jsonl"), 100_000);
    // `Journal::drain` caps a single call at 512 journal entries (~256 spans,
    // two entries per span). A run whose shape exceeds that cap needs more
    // than one pass to reach EOF; loop to completion so the measured window
    // covers the run's full export regardless of span count, not just its
    // first ~256 spans (#8642).
    while super::lifecycle::backfill(root, &queue) > 0 {}
    let elapsed_ns = began.elapsed().as_nanos();

    let spans: Vec<_> = queue
        .peek_batch(100_000)
        .into_iter()
        .filter_map(|envelope| match envelope.record {
            crate::telemetry::TelemetryRecord::Span(span) => Some(span.bounded()),
            _ => None,
        })
        .collect();
    let bounded_record_bytes = u64::try_from(
        serde_json::to_string(&spans)
            .context("serializing bounded span records")?
            .len(),
    )
    .unwrap_or(u64::MAX);
    let mut attribute_keys: Vec<String> = spans
        .iter()
        .flat_map(|span| span.attributes.keys().cloned())
        .collect();
    attribute_keys.sort_unstable();
    attribute_keys.dedup();
    let bounds = Bounds {
        max_attribute_value_bytes: spans
            .iter()
            .flat_map(|span| span.attributes.values().map(String::len))
            .max()
            .unwrap_or(0),
        max_attributes_per_span: spans.iter().map(|s| s.attributes.len()).max().unwrap_or(0),
        max_events_per_span: spans.iter().map(|s| s.events.len()).max().unwrap_or(0),
        max_links_per_span: spans.iter().map(|s| s.links.len()).max().unwrap_or(0),
        attribute_keys,
    };
    // The queue file is per-repetition scratch; leaving it would make a later
    // repetition's byte accounting include an earlier one's records.
    let _ = std::fs::remove_file(root.join("overhead-queue.jsonl"));
    Ok(Sample {
        elapsed_ns,
        spans,
        journal_bytes,
        bounded_record_bytes,
        bounds,
    })
}

fn median(mut values: Vec<u128>) -> u128 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0)
}

/// Measure instrumentation overhead across `repetitions` representative runs.
///
/// `instrumented_root` must be a workspace whose resolved observability config
/// enables OTLP tracing; `baseline_root` must be one where it does not. Both
/// are asserted before any timing is taken — an ambient
/// `LOOM_OBSERVABILITY_*` override that silently equalised the two arms would
/// otherwise produce a confidently wrong number.
///
/// # Errors
///
/// Fails when either arm is not in its required tracing state, when the
/// instrumented arm does not persist the spans its shape declares, or when a
/// representative run cannot be executed.
pub fn measure(
    instrumented_root: &Path,
    baseline_root: &Path,
    reference_workspace: &Path,
    shape: &RunShape,
    repetitions: usize,
) -> Result<Report> {
    anyhow::ensure!(repetitions > 0, "repetitions must be at least 1");
    anyhow::ensure!(
        super::tracing::enabled(instrumented_root),
        "instrumented arm is not tracing: this binary needs the otlp feature and an \
         observability config resolving to exporter=otlp with a valid endpoint"
    );
    anyhow::ensure!(
        !super::tracing::enabled(baseline_root),
        "baseline arm is tracing: an ambient LOOM_OBSERVABILITY_* override is \
         equalising both arms, which would understate overhead"
    );
    let mut instrumented = Vec::with_capacity(repetitions);
    let mut baseline = Vec::with_capacity(repetitions);
    let mut last: Option<Sample> = None;
    for repetition in 0..repetitions {
        let execution = format!("overhead-{repetition}");
        let sample = run_once(instrumented_root, shape, &execution)?;
        anyhow::ensure!(
            sample.spans.len() == shape.expected_spans(),
            "instrumented run persisted {} spans, expected {}",
            sample.spans.len(),
            shape.expected_spans()
        );
        instrumented.push(sample.elapsed_ns);
        last = Some(sample);
        baseline.push(run_once(baseline_root, shape, &execution)?.elapsed_ns);
    }
    let sample = last.context("no repetition was executed")?;
    let instrumented_median_ns = median(instrumented.clone());
    let baseline_median_ns = median(baseline.clone());
    let added: Vec<u128> = instrumented
        .iter()
        .zip(&baseline)
        .map(|(run, base)| run.saturating_sub(*base))
        .collect();
    let added_median_ns = median(added.clone());
    let spans_per_run = shape.expected_spans();
    let reference = observed_reference(reference_workspace);
    let overhead_fraction_of_p50 = reference.as_ref().and_then(|reference| {
        (reference.p50_seconds > 0)
            .then(|| added_median_ns as f64 / (reference.p50_seconds as f64 * 1_000_000_000.0))
    });
    Ok(Report {
        offline: true,
        repetitions,
        phases: shape.phases.len(),
        tools_per_attempt: shape.tools_per_attempt,
        spans_per_run,
        instrumented_median_ns,
        baseline_median_ns,
        added_median_ns,
        added_min_ns: added.iter().copied().min().unwrap_or(0),
        added_max_ns: added.iter().copied().max().unwrap_or(0),
        added_ns_per_span: added_median_ns / spans_per_run.max(1) as u128,
        bytes: ByteCost {
            journal_bytes: sample.journal_bytes,
            bounded_record_bytes: sample.bounded_record_bytes,
            bytes_per_span: sample.bounded_record_bytes / spans_per_run.max(1) as u64,
        },
        bounds: sample.bounds,
        reference,
        overhead_fraction_of_p50,
        excludes: vec![
            "network_export_latency",
            "backend_ingestion_and_indexing",
            "provider_side_cost",
        ],
    })
}

// Tracing is inert without the `otlp` feature (`tracing::enabled` returns
// false), so these exercise the instrumented arm only where it can exist.
#[cfg(all(test, feature = "otlp"))]
mod tests;
