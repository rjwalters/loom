//! Journal-reading helpers shared by every test module here: read the
//! envelopes one cycle wrote, assert identity uniqueness, count them by kind,
//! and the span counts a clean cycle over the recorded fixture produces.
//!
//! A sibling file rather than more of [`super`]: these are the assertions every
//! module in this directory reaches for, so they belong somewhere a reader can
//! open on their own — and the span-count constants are the one place a new
//! span family (#9089 steps and suites, #9456 tests) has to be accounted for,
//! which is easier to see here than buried in the parent's harness.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::{envelope_identity, journal_path, Journal, TelemetryEnvelope, TelemetryRecord};

pub(crate) fn journal(root: &Path) -> Vec<TelemetryEnvelope> {
    Journal::reader(journal_path(root)).read_all().unwrap()
}

/// Every CI envelope identity appears exactly once in the journal.
pub(crate) fn assert_no_duplicates(root: &Path) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for env in journal(root) {
        *counts.entry(envelope_identity(&env).unwrap()).or_default() += 1;
    }
    let dupes: Vec<_> = counts.iter().filter(|(_, n)| **n > 1).collect();
    assert!(dupes.is_empty(), "double-emitted: {dupes:?}");
}

/// Spans one clean cycle over the fixture emits: 6 run spans + 24 job spans
/// + [`FIXTURE_STEP_SPANS`] + [`FIXTURE_SUITE_SPANS`] (#9089)
/// + [`FIXTURE_TEST_SPANS`] (#9456).
pub(crate) const FIXTURE_SPANS: usize =
    6 + 24 + FIXTURE_STEP_SPANS + FIXTURE_SUITE_SPANS + FIXTURE_TEST_SPANS;

/// `loom.ci.step` spans the fixture's two `steps[]`-bearing jobs produce
/// (#9089): four from job 10012 and one from job 10034 — whose second step
/// has neither timestamp, so it is deliberately NOT a span.
pub(crate) const FIXTURE_STEP_SPANS: usize = 5;

/// `loom.ci.suite` spans the fixture's one suite-timings artifact produces
/// (#9089), all under job 10012 (`Shell Test Suites (hermetic, 2/2)`): its
/// record holds six entries, of which the skipped one (no window) and the
/// repeat of an earlier suite name emit nothing.
pub(crate) const FIXTURE_SUITE_SPANS: usize = 4;

/// `loom.ci.test` spans the fixture's one JUnit artifact produces (#9456), all
/// under job 10014 (`Rust Unit Tests (2/3)`): its document holds nine
/// `<testcase>` elements, of which the sub-floor one, the `<skipped/>` one,
/// the one with no `timestamp` and the repeated `(classname, name)` emit
/// nothing.
pub(crate) const FIXTURE_TEST_SPANS: usize = 5;

pub(crate) fn kind_counts(root: &Path) -> (usize, usize, usize, usize) {
    let (mut runs, mut jobs, mut durations, mut spans) = (0, 0, 0, 0);
    for env in journal(root) {
        match env.record {
            TelemetryRecord::CiRun(_) => runs += 1,
            TelemetryRecord::CiJob(_) => jobs += 1,
            TelemetryRecord::CiDuration(_) => durations += 1,
            TelemetryRecord::Span(_) => spans += 1,
            TelemetryRecord::CiJobLog(_) => {}
            other => panic!("unexpected record in CI journal: {other:?}"),
        }
    }
    (runs, jobs, durations, spans)
}

/// Every `ci.job.log` chunk in the journal, grouped by `job_id` and ordered
/// by `chunk_index` — the reconstruction a SigNoz query performs (AC1).
pub(crate) fn reconstruct_logs(
    root: &Path,
) -> BTreeMap<u64, Vec<crate::telemetry::CiJobLogRecord>> {
    let mut by_job: BTreeMap<u64, Vec<crate::telemetry::CiJobLogRecord>> = BTreeMap::new();
    for env in journal(root) {
        if let TelemetryRecord::CiJobLog(record) = env.record {
            by_job.entry(record.job_id).or_default().push(record);
        }
    }
    for chunks in by_job.values_mut() {
        chunks.sort_by_key(|chunk| chunk.chunk_index);
    }
    by_job
}
