//! Per-suite spans from a shard's uploaded timings artifact (Issue #9089).
//!
//! # Why an artifact at all
//!
//! A `loom.ci.job` span says a `Shell Test Suites (hermetic, 1/2)` leg took
//! 113s; its `loom.ci.step` children say 110s of that was the one `run:` step.
//! Neither can say **which of the ~118 suites in that leg** spent the time, so
//! rebalancing the `LOOM_CI_SHARD` split stays guesswork. That data exists
//! only inside the runner, and the only surface a completed job can hand it to
//! the poller on is an artifact: the job log is explicitly excluded (see
//! `ci-observability.md` §"Why there is no `step` attribute" — an attribute
//! derived from log text rides straight past the gateway's body scrub), and
//! `GITHUB_STEP_SUMMARY` is not readable through the API.
//!
//! # The contract, both halves in one place
//!
//! `run-ci-suites.sh` writes [`SCHEMA`]-tagged JSON and `ci.yml` uploads it as
//! an artifact whose name starts with [`ARTIFACT_NAME_PREFIX`]. The poller
//! reads **only** those artifacts, and only for a run that already has a
//! `shell-suite-shard` job — so a repo that does not shard shell suites pays
//! nothing, not even the artifacts listing.
//!
//! # Every value here is untrusted input
//!
//! A `pull_request` from a fork runs the **fork's** `run-ci-suites.sh`, so
//! suite names, outcomes and windows are attacker-controlled text, not repo
//! state. Hence: a byte cap before parsing ([`MAX_ARTIFACT_BYTES`]), a span
//! cap per job ([`MAX_SUITE_SPANS_PER_JOB`]), control-stripped and truncated
//! names (`bounded_attributes` DROPS a value with a control character or over
//! 256 chars, which would silently lose the one attribute naming the suite), a
//! closed outcome vocabulary, and windows clamped into the job's own span. A
//! record that fails any of it is skipped and counted — never repaired into
//! something plausible.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::records::{ShardInfo, ShardKind};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceContext};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

/// The schema tag `run-ci-suites.sh` writes and the poller requires. A record
/// with any other value is skipped, so a future incompatible revision can be
/// rolled out on the producer side without a daemon reading it as v1.
pub const SCHEMA: &str = "loom.ci.suite-timings/1";

/// Artifacts whose name starts with this are the only ones the poller
/// downloads. `ci.yml` uploads `ci-suite-timings-<shard>`.
pub const ARTIFACT_NAME_PREFIX: &str = "ci-suite-timings";

/// Upper bound on the artifact text parsed for one shard. A 235-suite record
/// is ~30 KB; this is two orders of magnitude of headroom and still bounds
/// what a hostile fork can make the poller parse.
pub const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;

/// Upper bound on suite spans emitted for one job. `ci-wired.txt` lists 235
/// suites across 2 legs today; this bounds one leg even if the split changes.
pub const MAX_SUITE_SPANS_PER_JOB: usize = 512;

/// Upper bound on timings artifacts downloaded for one run — one per shard,
/// with headroom. Bounds a cycle's wall time when a run (or a hostile fork)
/// uploads many artifacts matching the prefix.
pub const MAX_ARTIFACTS_PER_RUN: usize = 8;

/// Longest suite name carried on a span attribute; see the module note.
const MAX_SUITE_NAME_CHARS: usize = 200;

/// One `GET /repos/{o}/{r}/actions/runs/{id}/artifacts` row, reduced to what
/// the poller uses.
#[derive(Debug, Clone, Deserialize)]
pub struct ArtifactJson {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub expired: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtifactsPage {
    #[serde(default)]
    pub artifacts: Vec<ArtifactJson>,
}

/// `GET` path for a run's artifacts listing.
#[must_use]
pub fn artifacts_path(repo: &str, run_id: u64) -> String {
    format!("repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100")
}

/// Whether this artifact is a suite-timings record worth downloading. An
/// **expired** artifact is not: GitHub answers 410 for it, which would be
/// recorded as a failure every cycle for nothing.
#[must_use]
pub fn is_timings_artifact(artifact: &ArtifactJson) -> bool {
    artifact.name.starts_with(ARTIFACT_NAME_PREFIX) && !artifact.expired
}

/// One suite entry as written by `run-ci-suites.sh`.
#[derive(Debug, Clone, Deserialize)]
pub struct SuiteEntryJson {
    #[serde(default)]
    pub suite: String,
    #[serde(default)]
    pub outcome: String,
    /// Epoch seconds. `0` for a suite that never ran (skipped by the
    /// live-daemon guard, missing from disk, or whose result file was lost) —
    /// which is why the pair, not the duration, is the wire format: a suite
    /// that did not run must not be indistinguishable from one that ran in
    /// under a second.
    #[serde(default)]
    pub started_at_epoch: i64,
    #[serde(default)]
    pub ended_at_epoch: i64,
    #[serde(default)]
    pub retried: bool,
}

/// One shard's timings record.
#[derive(Debug, Clone, Deserialize)]
pub struct SuiteTimingsJson {
    #[serde(default)]
    pub schema: String,
    /// `GITHUB_RUN_ID`, or `"local"` outside Actions. Cross-checked against
    /// the run being polled: an artifact uploaded by a *different* run (a
    /// workflow that copies one forward) must not be attributed to this one.
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub run_attempt: String,
    /// `k/N` — this leg's `LOOM_CI_SHARD`. Empty on an unsharded (local) run,
    /// which is never paired with a job.
    #[serde(default)]
    pub shard: String,
    #[serde(default)]
    pub suites: Vec<SuiteEntryJson>,
}

/// Why a timings record was not turned into spans. Every variant is a named
/// reason the cycle logs — a record silently dropped would be worse than no
/// record at all, because the absence of suite spans would look like "this leg
/// ran no suites".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// The file did not parse as JSON.
    Unparseable(String),
    /// `schema` was absent or not [`SCHEMA`].
    UnknownSchema(String),
    /// `run_id` named a different run than the one being polled.
    ForeignRun { recorded: String, polled: u64 },
    /// `shard` was empty or not `k/N`.
    NoShard(String),
    /// No `shell-suite-shard` job of this run has that `(k/N)`, or several do.
    /// Never guessed — the same rule story stitching follows for an ambiguous
    /// issue candidate.
    NoUniqueJob { shard: String, matches: usize },
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::Unparseable(detail) => write!(f, "unparseable timings record: {detail}"),
            RejectReason::UnknownSchema(schema) => {
                write!(f, "unrecognised schema {schema:?} (expected {SCHEMA:?})")
            }
            RejectReason::ForeignRun { recorded, polled } => write!(
                f,
                "record names run {recorded:?} but was attached to run {polled}"
            ),
            RejectReason::NoShard(shard) => {
                write!(f, "record carries no usable shard identity ({shard:?})")
            }
            RejectReason::NoUniqueJob { shard, matches } => write!(
                f,
                "shard {shard:?} matches {matches} shell-suite-shard job(s) of this run, not exactly 1"
            ),
        }
    }
}

/// A parsed, run-validated record plus the `(k, N)` it belongs to.
#[derive(Debug, Clone)]
pub struct SuiteTimings {
    pub shard_index: u32,
    pub shard_total: u32,
    pub suites: Vec<SuiteEntryJson>,
}

/// Parse and validate one artifact's text against the run it was attached to.
pub fn parse(text: &str, polled_run_id: u64) -> Result<SuiteTimings, RejectReason> {
    let record: SuiteTimingsJson =
        serde_json::from_str(text).map_err(|e| RejectReason::Unparseable(e.to_string()))?;
    if record.schema != SCHEMA {
        return Err(RejectReason::UnknownSchema(record.schema));
    }
    // A record from outside Actions (`run_id: "local"`) is not this run's, and
    // neither is one naming another run id. Only an exact match passes.
    if record.run_id != polled_run_id.to_string() {
        return Err(RejectReason::ForeignRun {
            recorded: record.run_id,
            polled: polled_run_id,
        });
    }
    let Some((index, total)) = parse_shard_pair(&record.shard) else {
        return Err(RejectReason::NoShard(record.shard));
    };
    Ok(SuiteTimings {
        shard_index: index,
        shard_total: total,
        suites: record.suites,
    })
}

/// `"k/N"` with `1 <= k <= N`, both non-zero. Anything else is no shard.
fn parse_shard_pair(shard: &str) -> Option<(u32, u32)> {
    let (k, n) = shard.split_once('/')?;
    let k: u32 = k.trim().parse().ok()?;
    let n: u32 = n.trim().parse().ok()?;
    (k >= 1 && n >= 1 && k <= n).then_some((k, n))
}

/// Which job of this run a record belongs to: the unique `shell-suite-shard`
/// leg with the same `(index, total)`. `jobs` is `(job_id, ShardInfo)` for
/// every job of the run.
pub fn match_job(timings: &SuiteTimings, jobs: &[(u64, ShardInfo)]) -> Result<u64, RejectReason> {
    let matches: Vec<u64> = jobs
        .iter()
        .filter(|(_, shard)| {
            shard.kind == ShardKind::ShellSuiteShard
                && shard.index == Some(timings.shard_index)
                && shard.total == Some(timings.shard_total)
        })
        .map(|(job_id, _)| *job_id)
        .collect();
    match matches.as_slice() {
        [only] => Ok(*only),
        other => Err(RejectReason::NoUniqueJob {
            shard: format!("{}/{}", timings.shard_index, timings.shard_total),
            matches: other.len(),
        }),
    }
}

/// A suite name reduced to what a span attribute may carry — the same
/// treatment `records::step_name` applies, for the same reason.
#[must_use]
fn suite_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= MAX_SUITE_NAME_CHARS {
        return cleaned;
    }
    let mut out: String = cleaned.chars().take(MAX_SUITE_NAME_CHARS).collect();
    out.push('…');
    out
}

/// The outcome vocabulary a span may carry. Anything else becomes `unknown`
/// rather than being forwarded verbatim — the attribute is grouped on, so an
/// open vocabulary from a fork's script is unbounded cardinality.
#[must_use]
fn suite_outcome(raw: &str) -> &'static str {
    match raw {
        "pass" => "pass",
        "fail" => "fail",
        "skip" => "skip",
        "missing" => "missing",
        "no-result" => "no-result",
        _ => "unknown",
    }
}

fn outcome_status(outcome: &str) -> SpanStatus {
    match outcome {
        "pass" => SpanStatus::Ok,
        "fail" | "missing" | "no-result" => SpanStatus::Error,
        _ => SpanStatus::Unset,
    }
}

/// One suite's window, clamped inside `[job_started, job_ended]`.
///
/// `None` when the entry carries no window at all (a suite that never ran:
/// both epochs `0`) — absent, never a zero-length span at the job's start,
/// which would read as "ran instantly". The clamp bounds the damage a wrong
/// runner clock (or a forged record) can do to the trace: a child span outside
/// its parent's window is rendered as a detached bar, and the job's own
/// GitHub-reported window is the authority here, not the runner's `date +%s`.
#[must_use]
pub fn window(
    entry: &SuiteEntryJson,
    job_started: DateTime<Utc>,
    job_ended: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    if entry.started_at_epoch <= 0 || entry.ended_at_epoch <= 0 {
        return None;
    }
    let start = DateTime::<Utc>::from_timestamp(entry.started_at_epoch, 0)?;
    let end = DateTime::<Utc>::from_timestamp(entry.ended_at_epoch, 0)?;
    let start = start.clamp(job_started, job_ended);
    let end = end.clamp(start, job_ended);
    Some((start, end))
}

/// Everything a suite span needs about its job, resolved once by the caller.
#[derive(Debug, Clone)]
pub struct SuiteSpanTarget<'a> {
    pub repo: &'a str,
    pub visibility: RepoVisibility,
    pub run_id: u64,
    pub attempt: u32,
    pub job_id: u64,
    pub job: &'a str,
    pub workflow: &'a str,
    pub shard: ShardInfo,
    pub job_context: TraceContext,
    pub job_started: DateTime<Utc>,
    pub job_ended: DateTime<Utc>,
}

/// The `loom.ci.suite` spans of one shard, each a child of that shard's job
/// span.
///
/// Span-only, for the same reasons step spans are (`ci-observability.md`): the
/// metric label allowlist admits no suite dimension, and a per-suite histogram
/// would multiply the 30-day series count by every leg's suite count.
///
/// A suite name is emitted **at most once** per job: a duplicated entry would
/// derive the same span id twice, and the backend would deduplicate one of the
/// two away silently. Dropping the repeat here makes the count deterministic
/// instead.
#[must_use]
pub fn suite_envelopes(
    target: &SuiteSpanTarget<'_>,
    timings: &SuiteTimings,
    host_id: &str,
) -> Vec<TelemetryEnvelope> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut envelopes = Vec::new();
    for entry in &timings.suites {
        if envelopes.len() >= MAX_SUITE_SPANS_PER_JOB {
            break;
        }
        let name = suite_name(&entry.suite);
        if name.is_empty() || !seen.insert(name.clone()) {
            continue;
        }
        let Some((started_at, ended_at)) = window(entry, target.job_started, target.job_ended)
        else {
            continue;
        };
        let outcome = suite_outcome(&entry.outcome);
        let ctx = super::records::suite_context(
            target.repo,
            target.run_id,
            target.attempt,
            target.job_id,
            &name,
        );
        let span = SpanRecord {
            context: ctx.clone(),
            parent_span_id: Some(target.job_context.span_id.clone()),
            name: SpanName::CiSuite,
            started_at,
            ended_at,
            status: outcome_status(outcome),
            attributes: super::records::span_attributes(vec![
                ("loom.repo", Some(target.repo.to_string())),
                (
                    "loom.repo.visibility",
                    Some(super::records::visibility_str(target.visibility).to_string()),
                ),
                ("loom.ci.run_id", Some(target.run_id.to_string())),
                ("loom.ci.job_id", Some(target.job_id.to_string())),
                ("loom.ci.workflow", Some(target.workflow.to_string())),
                ("loom.ci.job", Some(target.job.to_string())),
                ("loom.ci.suite", Some(name)),
                ("loom.ci.suite.outcome", Some(outcome.to_string())),
                ("loom.ci.suite.retried", Some(entry.retried.to_string())),
                ("loom.ci.shard.index", target.shard.index.map(|i| i.to_string())),
                ("loom.ci.shard.total", target.shard.total.map(|t| t.to_string())),
                ("loom.ci.shard.kind", Some(target.shard.kind.as_str().to_string())),
            ]),
            events: Vec::new(),
            links: Vec::new(),
        };
        let mut envelope = TelemetryEnvelope::new(host_id, TelemetryRecord::Span(span));
        envelope.trace_context = Some(ctx);
        envelopes.push(envelope);
    }
    envelopes
}

/// The first file under `dir` (recursively, depth-first in sorted order) whose
/// contents are valid UTF-8 and at most [`MAX_ARTIFACT_BYTES`] long.
///
/// `gh run download --name X --dir D` extracts X's files directly into `D`,
/// but a `gh` that creates `D/X/` instead is not worth a version probe, so the
/// walk is recursive. A file over the cap is skipped rather than truncated:
/// half a JSON document does not parse anyway, and truncating would turn a
/// deliberate flood into a confusing parse error instead of a named skip.
pub fn read_artifact_text(dir: &Path) -> Option<String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = read_artifact_text(&path) {
                return Some(found);
            }
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > MAX_ARTIFACT_BYTES as u64 {
            log::warn!(
                "ci_telemetry: suite-timings artifact file {} is {} byte(s), over the {MAX_ARTIFACT_BYTES}-byte cap — skipped",
                path.display(),
                metadata.len()
            );
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some(text);
        }
    }
    None
}
