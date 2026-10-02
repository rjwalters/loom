//! Spans built from a run's uploaded artifacts (#9089 suites, #9456 per-test).
//!
//! Split out of [`super::poll`] rather than living in it: the two artifact
//! families share one artifacts listing, one staging/download/read path and one
//! degrade-and-count failure policy, and that machinery is a self-contained
//! stage of a cycle — `poll` calls it once, in [`super::poll::record_run`], and
//! otherwise does not care how a `loom.ci.suite` or `loom.ci.test` span is
//! produced.
//!
//! The division of labour across the three modules:
//!
//! | Module | Owns |
//! |---|---|
//! | [`super::suites`] | the suite-timings wire format, its validation and its `loom.ci.suite` spans |
//! | [`super::nextest`] | the JUnit parser, the tail selection and its `loom.ci.test` spans |
//! | this module | the forge side both share: the cost gate, the listing, the per-run artifact caps, the download, and the per-family accounting |

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::api::{ApiError, GithubApi};
use super::ledger::{Ledger, UnitKey};
use super::nextest;
use super::poll::{is_credential_rejection, paginate, CycleContext, CycleReport};
use super::records::{parse_shard, JobJson, RepoJson, RunJson, ShardInfo, ShardKind};
use super::state_dir;
use super::suites::{self, ArtifactsPage};
use crate::telemetry::TelemetryEnvelope;

/// Where one artifact is unpacked. Under the poller's own state dir (not
/// `/tmp`) so a host with a full or noexec `/tmp` fails the same way as the
/// ledger would, and so the path is removed by the same cleanup that removes
/// the state dir.
fn artifact_dir(root: &Path, run_id: u64, artifact_id: u64) -> PathBuf {
    state_dir(root)
        .join("artifacts")
        .join(format!("{run_id}-{artifact_id}"))
}

/// Which uploaded-artifact family one artifact belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtifactKind {
    /// `ci-suite-timings-<shard>` → `loom.ci.suite` spans (#9089).
    SuiteTimings,
    /// `ci-test-timings-<family>-<k>-<N>` → `loom.ci.test` spans (#9456).
    TestTimings,
}

impl ArtifactKind {
    fn label(self) -> &'static str {
        match self {
            ArtifactKind::SuiteTimings => "suite-timings",
            ArtifactKind::TestTimings => "test-timings",
        }
    }

    fn max_bytes(self) -> u64 {
        match self {
            ArtifactKind::SuiteTimings => suites::MAX_ARTIFACT_BYTES as u64,
            ArtifactKind::TestTimings => nextest::MAX_JUNIT_BYTES,
        }
    }

    /// Which shard family of job this artifact's spans hang off, and therefore
    /// which jobs have to be unemitted for the artifacts listing to be worth
    /// one request.
    fn shard_kind(self) -> ShardKind {
        match self {
            ArtifactKind::SuiteTimings => ShardKind::ShellSuiteShard,
            ArtifactKind::TestTimings => ShardKind::NextestPartition,
        }
    }

    fn max_artifacts_per_run(self) -> usize {
        match self {
            ArtifactKind::SuiteTimings => suites::MAX_ARTIFACTS_PER_RUN,
            ArtifactKind::TestTimings => nextest::MAX_ARTIFACTS_PER_RUN,
        }
    }
}

/// The `loom.ci.suite` (#9089) and `loom.ci.test` (#9456) spans for this run's
/// sharded legs, keyed by job id. One artifacts listing serves both families —
/// splitting them would double the request for every run that has both.
///
/// **Cost gate.** Returns immediately — zero requests — unless this run has at
/// least one *not-yet-emitted* `shell-suite-shard` or `nextest-partition` job.
/// A repo that shards neither therefore pays nothing for either feature, not
/// even the artifacts listing, and a re-listed run whose jobs are all already
/// seen does not re-download anything. The gate is per family: a run with only
/// nextest legs never downloads a suite-timings artifact, and vice versa.
///
/// **Failure policy.** This runs BEFORE the run's units are committed, so the
/// spans ride in the job units themselves and stay exactly-once with
/// everything else — no second key space and no separate retry pass. The
/// tradeoff is deliberate and bounded in the other direction: a failure here
/// degrades to *no suite/test spans for this run*, counted in
/// `suite_artifact_failures` / `test_artifact_failures` and logged, and is
/// never retried. Holding a run's `ci.run`/`ci.job` records hostage to a side
/// artifact would be the worse failure — the records are the primary signal,
/// these spans are a refinement of one job in it.
///
/// Only a rate limit or a rejected credential escapes as `Err`, because those
/// are properties of the host and abort the whole cycle wherever they happen.
pub(super) fn for_run(
    ctx: &CycleContext<'_>,
    api: &dyn GithubApi,
    ledger: &Ledger,
    repo: &RepoJson,
    run: &RunJson,
    jobs: &[JobJson],
    report: &mut CycleReport,
) -> Result<BTreeMap<u64, Vec<TelemetryEnvelope>>, ApiError> {
    let shards: Vec<(u64, ShardInfo)> = jobs
        .iter()
        .map(|job| (job.id, parse_shard(&job.name)))
        .collect();
    let has_unemitted_leg = |kind: ShardKind| {
        jobs.iter().zip(&shards).any(|(job, (_, shard))| {
            shard.kind == kind
                && !ledger.is_seen(&UnitKey::job(&repo.full_name, run.id, job.id, job.run_attempt))
        })
    };
    let wanted: Vec<ArtifactKind> = [ArtifactKind::SuiteTimings, ArtifactKind::TestTimings]
        .into_iter()
        .filter(|kind| has_unemitted_leg(kind.shard_kind()))
        .collect();
    if wanted.is_empty() {
        return Ok(BTreeMap::new());
    }

    let artifacts = paginate(
        api,
        suites::artifacts_path(&repo.full_name, run.id),
        &mut report.summary.requests,
        |body| serde_json::from_str::<ArtifactsPage>(body).map(|p| p.artifacts),
    )?;
    let mut by_job: BTreeMap<u64, Vec<TelemetryEnvelope>> = BTreeMap::new();
    let mut downloaded_of: BTreeMap<&'static str, usize> = BTreeMap::new();
    for artifact in &artifacts {
        let Some(kind) = wanted.iter().copied().find(|kind| match kind {
            ArtifactKind::SuiteTimings => suites::is_timings_artifact(artifact),
            ArtifactKind::TestTimings => nextest::is_test_artifact(artifact),
        }) else {
            continue;
        };
        let seen = downloaded_of.entry(kind.label()).or_default();
        if *seen >= kind.max_artifacts_per_run() {
            continue;
        }
        *seen += 1;
        let built = one_artifact_spans(
            ctx,
            api,
            repo,
            run,
            jobs,
            &shards,
            artifact,
            kind,
            &mut report.summary.requests,
        )?;
        match built {
            Some((job_id, envelopes)) => {
                match kind {
                    ArtifactKind::SuiteTimings => {
                        report.summary.suite_records_read += 1;
                        report.summary.suite_spans_emitted += envelopes.len();
                    }
                    ArtifactKind::TestTimings => {
                        report.summary.test_records_read += 1;
                        report.summary.test_spans_emitted += envelopes.len();
                    }
                }
                by_job.entry(job_id).or_default().extend(envelopes);
            }
            None => match kind {
                ArtifactKind::SuiteTimings => report.summary.suite_artifact_failures += 1,
                ArtifactKind::TestTimings => report.summary.test_artifact_failures += 1,
            },
        }
    }
    let _ = std::fs::remove_dir(state_dir(ctx.root).join("artifacts"));
    Ok(by_job)
}

/// Stage, download, read and convert ONE artifact. `Ok(None)` is the
/// degrade-and-count outcome (a failed download, an unreadable or rejected
/// record); only a rate limit or a rejected credential escapes as `Err`.
#[allow(clippy::too_many_arguments)]
fn one_artifact_spans(
    ctx: &CycleContext<'_>,
    api: &dyn GithubApi,
    repo: &RepoJson,
    run: &RunJson,
    jobs: &[JobJson],
    shards: &[(u64, ShardInfo)],
    artifact: &suites::ArtifactJson,
    kind: ArtifactKind,
    requests: &mut usize,
) -> Result<Option<(u64, Vec<TelemetryEnvelope>)>, ApiError> {
    let dest = artifact_dir(ctx.root, run.id, artifact.id);
    let _ = std::fs::remove_dir_all(&dest);
    if let Err(error) = std::fs::create_dir_all(&dest) {
        log::warn!(
            "ci_telemetry: could not stage {} artifact {} of {} run {}: {error}",
            kind.label(),
            artifact.name,
            repo.full_name,
            run.id
        );
        return Ok(None);
    }
    *requests += 1;
    let downloaded = api.download_artifact(&repo.full_name, run.id, &artifact.name, &dest);
    let text = match downloaded {
        Ok(()) => suites::read_artifact_text_capped(&dest, kind.max_bytes(), kind.label()),
        Err(error @ ApiError::RateLimited { .. }) => {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(error);
        }
        Err(error) if is_credential_rejection(&error) => {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(error);
        }
        Err(error) => {
            log::warn!(
                "ci_telemetry: {} artifact {} of {} run {} could not be downloaded: {error}",
                kind.label(),
                artifact.name,
                repo.full_name,
                run.id
            );
            None
        }
    };
    let _ = std::fs::remove_dir_all(&dest);
    let Some(text) = text else {
        return Ok(None);
    };
    let built = match kind {
        ArtifactKind::SuiteTimings => {
            suite_envelopes_from_text(repo, run, jobs, shards, &text, &ctx.host_id)
                .map_err(|reason| reason.to_string())
        }
        ArtifactKind::TestTimings => {
            test_envelopes_from_text(repo, run, jobs, shards, &artifact.name, &text, &ctx.host_id)
                .map_err(|reason| reason.to_string())
        }
    };
    match built {
        Ok(pair) => Ok(Some(pair)),
        Err(reason) => {
            log::info!(
                "ci_telemetry: {} artifact {} of {} run {} produced no spans: {reason}",
                kind.label(),
                artifact.name,
                repo.full_name,
                run.id
            );
            Ok(None)
        }
    }
}

/// Parse one JUnit artifact and build its leg's `loom.ci.test` spans (#9456).
/// Split out for the same reason as [`suite_envelopes_from_text`]: the whole
/// name → pair → parse → select → emit path is testable without a download.
///
/// The artifact **name** is the pairing key here, not anything inside the
/// file: JUnit XML carries no run id, no shard and no job name. There is
/// therefore no `ForeignRun` cross-check as there is for a suite-timings
/// record — the run attribution is GitHub's own per-run artifacts listing.
fn test_envelopes_from_text(
    repo: &RepoJson,
    run: &RunJson,
    jobs: &[JobJson],
    shards: &[(u64, ShardInfo)],
    artifact_name: &str,
    text: &str,
    host_id: &str,
) -> Result<(u64, Vec<TelemetryEnvelope>), nextest::RejectReason> {
    let identity = nextest::parse_artifact_name(artifact_name)
        .ok_or_else(|| nextest::RejectReason::NoArtifactIdentity(artifact_name.to_string()))?;
    let named: Vec<(u64, &str, ShardInfo)> = jobs
        .iter()
        .zip(shards)
        .map(|(job, (_, shard))| (job.id, job.name.as_str(), *shard))
        .collect();
    let job_id = nextest::match_job(&identity, &named)?;
    let cases = nextest::parse(text)?;
    let job = jobs
        .iter()
        .find(|job| job.id == job_id)
        .expect("match_job only returns a job id taken from this run's own jobs");
    // The same window `job_envelopes` gives the job span, so a test span is
    // always inside its parent even when the runner's clock disagrees.
    let job_started = job.started_at.unwrap_or(run.created_at);
    let job_ended = job.completed_at.unwrap_or(job_started).max(job_started);
    let workflow = run.workflow();
    let target = nextest::TestSpanTarget {
        repo: &repo.full_name,
        visibility: repo.visibility(),
        run_id: run.id,
        attempt: job.run_attempt,
        job_id: job.id,
        job: &job.name,
        workflow: &workflow,
        shard: parse_shard(&job.name),
        job_context: super::records::job_context(&repo.full_name, run.id, job.run_attempt, job.id),
        job_started,
        job_ended,
    };
    Ok((job_id, nextest::test_envelopes(&target, &cases, host_id)))
}

/// Parse one artifact's text and build its job's suite spans. Split out so the
/// whole parse → validate → match → emit path is testable without a download.
fn suite_envelopes_from_text(
    repo: &RepoJson,
    run: &RunJson,
    jobs: &[JobJson],
    shards: &[(u64, ShardInfo)],
    text: &str,
    host_id: &str,
) -> Result<(u64, Vec<TelemetryEnvelope>), suites::RejectReason> {
    let timings = suites::parse(text, run.id)?;
    let job_id = suites::match_job(&timings, shards)?;
    let job = jobs
        .iter()
        .find(|job| job.id == job_id)
        .expect("match_job only returns a job id taken from this run's own jobs");
    let shard = parse_shard(&job.name);
    // The same window `job_envelopes` gives the job span, so a suite span is
    // always inside its parent even when the runner's clock disagrees.
    let job_started = job.started_at.unwrap_or(run.created_at);
    let job_ended = job.completed_at.unwrap_or(job_started).max(job_started);
    let workflow = run.workflow();
    let target = suites::SuiteSpanTarget {
        repo: &repo.full_name,
        visibility: repo.visibility(),
        run_id: run.id,
        attempt: job.run_attempt,
        job_id: job.id,
        job: &job.name,
        workflow: &workflow,
        shard,
        job_context: super::records::job_context(&repo.full_name, run.id, job.run_attempt, job.id),
        job_started,
        job_ended,
    };
    Ok((job_id, suites::suite_envelopes(&target, &timings, host_id)))
}
