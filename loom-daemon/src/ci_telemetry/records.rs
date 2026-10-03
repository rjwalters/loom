//! GitHub Actions REST shapes → telemetry envelopes.
//!
//! Each completed **job** becomes one unit of three envelopes (`ci.job`, its
//! `ci.duration`, its `loom.ci.job` span) plus, since #9089, one extra
//! span-only envelope per executed **step** (`loom.ci.step`, a child of that
//! job's span); each completed **run** becomes one unit of three (`ci.run`,
//! its `ci.duration`, its `loom.ci.run` span). Trace and
//! span ids are **derived** from the GitHub identities (`repo`, `run_id`,
//! `job_id`, step number), never random, so a replayed or second-host
//! emission of the same run is byte-identical in identity and a backend can
//! deduplicate on it.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Deserialize;

use crate::telemetry::ci::{duration_ms, CiDurationMetric};
use crate::telemetry::trace::{
    SpanId, SpanRecord, SpanStatus, TraceAttributes, TraceContext, TraceId,
};
use crate::telemetry::{
    trace::SpanName, CiDurationRecord, CiJobRecord, CiRunRecord, RepoVisibility, TelemetryEnvelope,
    TelemetryRecord,
};

/// One `GET /orgs/{org}/repos` row, reduced to what the poller uses.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RepoJson {
    pub name: String,
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub archived: bool,
}

impl RepoJson {
    #[must_use]
    pub fn visibility(&self) -> RepoVisibility {
        if self.private {
            RepoVisibility::Private
        } else {
            RepoVisibility::Public
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActorJson {
    pub login: String,
}

fn first_attempt() -> u32 {
    1
}

/// One entry of a run's `pull_requests` array — the direct PR-number source
/// for a `pull_request`-triggered run, without needing a second API call.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestRefJson {
    pub number: u64,
}

/// A run row's `head_commit` — the commit the run executed against. Only the
/// message is read: its subject is the poller-visible signal that a run was
/// triggered by the #8508 re-date remedy (#9337).
#[derive(Debug, Clone, Deserialize)]
pub struct HeadCommitJson {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub tree_id: Option<String>,
    #[serde(default)]
    pub message: String,
}

/// Why a workflow run attempt happened — `loom.ci.trigger_reason` on the
/// `loom.ci.run` span and the `ci.run` record (#9337).
///
/// A pure function of ONE `/actions/runs` row ([`RunJson::trigger_reason`]),
/// so a replayed or second-host emission always attributes identically. No
/// cross-run comparison (e.g. "tree-identical to the previous head"): that
/// would need state across polls and hosts and break replay determinism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason {
    /// Fresh code on the head: a push / PR / merge-queue event whose head
    /// commit is not the re-date remedy's. Known under-count of the other
    /// values: a `gh pr update-branch` / `git merge main` head or a
    /// hand-pushed `--allow-empty` commit is a freshness bump in spirit but
    /// lands here, because a merge commit can also be real conflict work.
    NewCommit,
    /// The head commit is the #8508 re-date remedy's tree-identical no-op
    /// commit, pushed because `main` moved under a green PR (#8248 guard).
    StaleMainBump,
    /// `run_attempt > 1`: the same run re-run in place on the same head
    /// (`gh run rerun`, "Re-run failed jobs", …). The cause of the re-run is
    /// not knowable from the row; the name is kept from #9337.
    FlakyRetry,
    /// Anything else: `workflow_dispatch`, `schedule`, `issue_comment`, …, or
    /// a row without a head commit message to rule the re-date remedy out.
    Unknown,
}

impl TriggerReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerReason::NewCommit => "new_commit",
            TriggerReason::StaleMainBump => "stale_main_bump",
            TriggerReason::FlakyRetry => "flaky_retry",
            TriggerReason::Unknown => "unknown",
        }
    }
}

/// One `GET /repos/{o}/{r}/actions/runs` row.
#[derive(Debug, Clone, Deserialize)]
pub struct RunJson {
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default = "first_attempt")]
    pub run_attempt: u32,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub run_started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub triggering_actor: Option<ActorJson>,
    #[serde(default)]
    pub actor: Option<ActorJson>,
    /// The PR(s) GitHub associates with this run — populated for
    /// `pull_request`-triggered runs, empty otherwise (Issue #9007).
    #[serde(default)]
    pub pull_requests: Vec<PullRequestRefJson>,
    /// The commit the run executed against (#9337); absent or `null` in
    /// older recordings — attribution then falls to `unknown`.
    #[serde(default)]
    pub head_commit: Option<HeadCommitJson>,
}

impl RunJson {
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.status.as_deref() == Some("completed")
    }

    #[must_use]
    pub fn workflow(&self) -> String {
        self.name.clone().unwrap_or_else(|| "unnamed".to_string())
    }

    /// The join-key PR/issue number for this run, when derivable (Issue
    /// #9007). Two independent sources, tried in order:
    ///
    /// 1. `pull_requests[].number` — the true PR number, present only when
    ///    GitHub associates a PR with the run (typically `event ==
    ///    "pull_request"`).
    /// 2. [`crate::claim_reconciliation::parse_issue_from_branch`] on
    ///    `head_branch` — a `feature/issue-N` branch's **issue** number, used
    ///    as a fallback for `push`-triggered runs where no PR association is
    ///    reported. This is a different number space than (1) (issue vs. PR),
    ///    but both serve the same purpose here: joining this CI run back to
    ///    the sweep that produced it, following the sweep side's own
    ///    `loom.pr_number` convention.
    ///
    /// `None` when neither source resolves — never `0`.
    #[must_use]
    pub fn pr_number(&self) -> Option<u32> {
        if let Some(pr) = self.pull_requests.first() {
            return u32::try_from(pr.number).ok();
        }
        self.head_branch
            .as_deref()
            .and_then(crate::claim_reconciliation::parse_issue_from_branch)
    }

    /// Why this run attempt happened (#9337). Evaluated in order:
    ///
    /// 1. `run_attempt > 1` → [`TriggerReason::FlakyRetry`] (a re-run of a
    ///    re-date head is still a re-run: the in-place re-run is the more
    ///    recent trigger);
    /// 2. no head commit message → [`TriggerReason::Unknown`] (the re-date
    ///    remedy cannot be ruled out);
    /// 3. the message's first line is the #8508 re-date subject
    ///    ([`crate::merge_pr::redate::is_redate_commit_subject`]) →
    ///    [`TriggerReason::StaleMainBump`];
    /// 4. `event ∈ {push, pull_request, pull_request_target, merge_group}` →
    ///    [`TriggerReason::NewCommit`];
    /// 5. otherwise [`TriggerReason::Unknown`].
    #[must_use]
    pub fn trigger_reason(&self) -> TriggerReason {
        if self.run_attempt > 1 {
            return TriggerReason::FlakyRetry;
        }
        let Some(subject) = self
            .head_commit
            .as_ref()
            .and_then(|c| c.message.lines().next())
            .filter(|s| !s.is_empty())
        else {
            return TriggerReason::Unknown;
        };
        if crate::merge_pr::redate::is_redate_commit_subject(subject) {
            return TriggerReason::StaleMainBump;
        }
        match self.event.as_str() {
            "push" | "pull_request" | "pull_request_target" | "merge_group" => {
                TriggerReason::NewCommit
            }
            _ => TriggerReason::Unknown,
        }
    }

    /// Milliseconds the run queued before starting: `run_started_at −
    /// created_at`, floored at zero (#9007 follow-up). `None` when GitHub
    /// reported no start.
    #[must_use]
    pub fn queued_ms(&self) -> Option<i64> {
        self.run_started_at
            .map(|started| duration_ms(self.created_at, started))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunsPage {
    #[serde(default)]
    pub workflow_runs: Vec<RunJson>,
}

/// One `GET /repos/{o}/{r}/actions/runs/{id}/jobs` row.
#[derive(Debug, Clone, Deserialize)]
pub struct JobJson {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    /// When GitHub created the job (queued it for a runner). Absent on a
    /// recording made before #9089 — a missing value never reads as a zero
    /// queue, it makes [`Self::queued_ms`] `None`.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "first_attempt")]
    pub run_attempt: u32,
    /// The job's steps, in the order GitHub reports them (#9089). Present on
    /// every `/actions/runs/{id}/jobs` row the poller already fetches, so
    /// step spans cost no extra API call. Empty on a recording made before
    /// #9089, and on a job that failed before any step ran.
    #[serde(default)]
    pub steps: Vec<StepJson>,
}

impl JobJson {
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }

    /// Milliseconds this job sat queued for a runner: `started_at −
    /// created_at`, floored at zero (#9089 — the per-job analogue of
    /// [`RunJson::queued_ms`], #9007). `None` when GitHub reported no
    /// `created_at` (a pre-#9089 recording) or no `started_at`.
    #[must_use]
    pub fn queued_ms(&self) -> Option<i64> {
        match (self.created_at, self.started_at) {
            (Some(created), Some(started)) => Some(duration_ms(created, started)),
            _ => None,
        }
    }

    /// Milliseconds this job spent blocked on its `needs:` predecessors before
    /// GitHub created it at all: `created_at − baseline`, floored at zero
    /// (#9089, issue problem 5). See [`JobCreationBaseline`] for what the
    /// baseline is and why. `None` when either end is unknown — a missing
    /// value never reads as "waited on nothing".
    #[must_use]
    pub fn dependency_wait_ms(&self, baseline: JobCreationBaseline) -> Option<i64> {
        match (baseline.0, self.created_at) {
            (Some(first), Some(created)) => Some(duration_ms(first, created)),
            _ => None,
        }
    }
}

/// The instant a run attempt's **first** job was created — the zero point
/// every job's [`JobJson::dependency_wait_ms`] is measured from (#9089).
///
/// # Scoped to ONE attempt, never to the listing
///
/// The poller lists jobs with `filter=all`, which is GitHub's "include jobs
/// from old executions of this run" mode — so on a re-run that one response
/// carries **every** attempt's jobs. A baseline taken as the `min` over the
/// whole listing would charge attempt 2 with the entire inter-attempt gap as
/// though it were a `needs:` wait. Measured against this repo's real runs:
/// `36456576713` reports attempt 1's earliest `created_at` at
/// `2026-09-28T17:13:13Z` and attempt 2's at `17:52:23Z` (39m10s apart), and
/// `36435114396` reports `14:19:32Z` vs `16:43:44Z` (2h24m apart). Every
/// attempt-2 job would then report that gap instead of ~0, and section 15's
/// "`p90_dep_s` above `p90_queue_s` means gated, not capacity-starved" alert
/// would fire on every re-run.
///
/// So construct one only through [`JobCreationBaseline::of_attempt`], or —
/// when handling a whole listing — through [`JobCreationBaselines`], whose
/// [`JobCreationBaselines::for_job`] measures each job against **its own**
/// attempt (`job.run_attempt`, the same per-job attempt `UnitKey::job` keys
/// on — not the run row's).
///
/// # Why the first job, not the run row
///
/// GitHub creates a `needs:`-gated job only once its predecessors finish, so a
/// job's own `created_at` already encodes how long its dependency closure took:
/// on a `main` CI run measured 2026-09-29, every ungated job reported
/// `created_at` `01:34:16` while every job with `needs: build-daemon` reported
/// `01:35:15` — one second after `Build loom-daemon` completed. The gap between
/// those two instants **is** the dependency wait, and it needs no second API
/// call and no knowledge of the workflow's `needs:` graph.
///
/// The baseline is taken from the jobs listing rather than from the run row's
/// `run_started_at` deliberately: the run row's queue semantics are a different
/// measurement (`ci.run`'s own `queued_ms`, #9007, is time before *any* job
/// existed), and mixing the two would double-count a run's queue wait into
/// every one of its jobs. Reading the baseline out of the same listing the
/// waits come from keeps the quantity internally consistent — the earliest job
/// of a run always measures exactly `0`, by construction.
///
/// # What it is not
///
/// It does not name *which* dependency a job waited on, and it does not
/// separate a multi-level `needs:` chain into its links — it is the whole
/// closure's elapsed time. For an ungated job it is GitHub's own job-creation
/// lag (sub-second in the run above), not a dependency; read a value of a
/// second or two as noise, not as a serialized edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobCreationBaseline(Option<DateTime<Utc>>);

impl JobCreationBaseline {
    /// The earliest `created_at` reported across the jobs of **one** run
    /// attempt, ignoring every other attempt in the same `filter=all` listing
    /// (see the type docs for what mixing them costs). `None` when GitHub
    /// reported none for any job of that attempt (a pre-#9089 recording),
    /// which makes every one of its jobs' dependency waits `None` too.
    #[must_use]
    pub fn of_attempt(jobs: &[JobJson], attempt: u32) -> Self {
        Self(
            jobs.iter()
                .filter(|job| job.run_attempt == attempt)
                .filter_map(|job| job.created_at)
                .min(),
        )
    }

    /// The baseline instant, if one was derivable.
    #[must_use]
    pub fn instant(self) -> Option<DateTime<Utc>> {
        self.0
    }
}

/// Every attempt's [`JobCreationBaseline`] for one `filter=all` jobs listing,
/// computed in a single pass (#9089).
///
/// This is what a caller holding a whole listing wants: the listing mixes
/// attempts, so each job must be measured against its own attempt's first job,
/// and doing that per job would rescan the listing once per row. Built once
/// before the per-job pass, it keeps the "computed once per listing" property
/// while making the attempt scoping impossible to forget — a job whose attempt
/// is somehow absent gets [`JobCreationBaseline::default()`] ("not measured"),
/// never another attempt's instant.
#[derive(Debug, Clone, Default)]
pub struct JobCreationBaselines(std::collections::HashMap<u32, JobCreationBaseline>);

impl JobCreationBaselines {
    #[must_use]
    pub fn of_listing(jobs: &[JobJson]) -> Self {
        let mut by_attempt: std::collections::HashMap<u32, JobCreationBaseline> =
            std::collections::HashMap::new();
        for job in jobs {
            let Some(created) = job.created_at else {
                // Still register the attempt: an attempt whose every job lacks
                // a `created_at` must stay "not measured", not fall through to
                // another attempt's baseline.
                by_attempt.entry(job.run_attempt).or_default();
                continue;
            };
            let slot = by_attempt.entry(job.run_attempt).or_default();
            slot.0 = Some(slot.0.map_or(created, |earliest| earliest.min(created)));
        }
        Self(by_attempt)
    }

    /// The baseline for this job's **own** attempt.
    #[must_use]
    pub fn for_job(&self, job: &JobJson) -> JobCreationBaseline {
        self.0.get(&job.run_attempt).copied().unwrap_or_default()
    }
}

/// One entry of a job row's `steps[]` array (#9089).
///
/// GitHub reports `number` as the step's 1-based position within the job, and
/// `started_at` / `completed_at` only once the step has actually run — a step
/// the job never reached carries neither, and produces no span.
#[derive(Debug, Clone, Deserialize)]
pub struct StepJson {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub number: u32,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

impl StepJson {
    /// The step's `[start, end]` window, or `None` when GitHub reported no
    /// start or no completion — a step that never ran is not a zero-length
    /// span at the job's start, it is absent. `end` is floored at `start`, so
    /// reported clock skew never yields a span that ends before it begins
    /// (`SpanRecord::validate` rejects those outright).
    #[must_use]
    pub fn window(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match (self.started_at, self.completed_at) {
            (Some(started), Some(completed)) => Some((started, completed.max(started))),
            _ => None,
        }
    }
}

/// Upper bound on step spans emitted for one job (#9089). GitHub's own limit
/// on `steps:` per job is well under this, so it binds only on a pathological
/// or hostile row; it exists so one job can never expand into unbounded span
/// volume.
pub const MAX_STEP_SPANS_PER_JOB: usize = 64;

/// Longest step name carried on a span attribute. `bounded_attributes` DROPS
/// a value longer than 256 chars outright, so a long step name would silently
/// lose `loom.ci.step` — truncating here keeps the attribute present and
/// visibly elided instead.
const MAX_STEP_NAME_CHARS: usize = 200;

/// A step name reduced to what a span attribute may carry: control characters
/// (which `bounded_attributes` rejects the whole value for) collapsed to
/// spaces, then truncated on a character boundary with an ellipsis.
#[must_use]
fn step_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= MAX_STEP_NAME_CHARS {
        return cleaned;
    }
    let mut out: String = cleaned.chars().take(MAX_STEP_NAME_CHARS).collect();
    out.push('…');
    out
}

/// Which family a matrix leg's shard attributes describe (#9089).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShardKind {
    /// A `cargo nextest run --partition count:k/N` leg (`Rust Unit Tests`,
    /// `Rust OTLP Feature Tests`).
    NextestPartition,
    /// A `run-ci-suites.sh` / `LOOM_CI_SHARD` round-robin leg (`Shell Test
    /// Suites`).
    ShellSuiteShard,
    /// Not a sharded matrix leg.
    #[default]
    None,
}

impl ShardKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ShardKind::NextestPartition => "nextest-partition",
            ShardKind::ShellSuiteShard => "shell-suite-shard",
            ShardKind::None => "none",
        }
    }
}

/// A job's shard identity, parsed from its display name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShardInfo {
    pub kind: ShardKind,
    pub index: Option<u32>,
    pub total: Option<u32>,
}

/// Parse a job's shard identity from its display name (#9089). `ci.yml`'s two
/// sharded job families already print `(index/total)` in their `name:` —
/// `Rust Unit Tests (1/3)`, `Rust OTLP Feature Tests (2/3)`, `Shell Test
/// Suites (hermetic, 1/2)` — so a trailing `(…k/N)` group is a strong,
/// no-extra-API-call signal: the jobs listing the poller already fetches
/// carries it. The `Shell Test Suites` prefix distinguishes the round-robin
/// shell-shard family (`LOOM_CI_SHARD`) from the nextest-partition family
/// (`cargo nextest run --partition`); a name with no matching group is
/// unsharded ([`ShardKind::None`], `index`/`total` both `None`).
#[must_use]
pub fn parse_shard(job_name: &str) -> ShardInfo {
    static SHARD_RE: OnceLock<Regex> = OnceLock::new();
    let re = SHARD_RE.get_or_init(|| {
        Regex::new(r"\((?:[^()]*,\s*)?(\d+)/(\d+)\)\s*$").expect("static shard-suffix pattern")
    });
    let Some(caps) = re.captures(job_name) else {
        return ShardInfo::default();
    };
    let index = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
    let total = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
    let kind = if job_name.starts_with("Shell Test Suites") {
        ShardKind::ShellSuiteShard
    } else {
        ShardKind::NextestPartition
    };
    ShardInfo { kind, index, total }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobsPage {
    #[serde(default)]
    pub jobs: Vec<JobJson>,
}

/// A run attempt's trace context: trace id and root span id both derived
/// from `(repo, run_id, attempt)` — one trace per run attempt. Always
/// sampled.
#[must_use]
pub fn run_context(repo: &str, run_id: u64, attempt: u32) -> TraceContext {
    let run = run_id.to_string();
    let attempt = attempt.to_string();
    TraceContext {
        trace_id: TraceId::derived(&["loom.ci.trace", repo, &run, &attempt]),
        span_id: SpanId::derived(&["loom.ci.run", repo, &run, &attempt]),
        flags: 1,
    }
}

/// A job span's context inside its run attempt's trace.
#[must_use]
pub fn job_context(repo: &str, run_id: u64, attempt: u32, job_id: u64) -> TraceContext {
    let run = run_context(repo, run_id, attempt);
    TraceContext {
        span_id: SpanId::derived(&["loom.ci.job", repo, &job_id.to_string()]),
        ..run
    }
}

/// A step span's context inside its run attempt's trace (#9089). Derived from
/// `(repo, job_id, step number)` — the same determinism rule the run and job
/// contexts follow, so a replayed or second-host emission of the same step is
/// byte-identical in identity. The step *number*, not its name, is the
/// identity: renaming a step in `ci.yml` does not fork a step's span id, and
/// two steps sharing a name (common — `run: make` twice) stay distinct.
#[must_use]
pub fn step_context(
    repo: &str,
    run_id: u64,
    attempt: u32,
    job_id: u64,
    number: u32,
) -> TraceContext {
    let run = run_context(repo, run_id, attempt);
    TraceContext {
        span_id: SpanId::derived(&[
            "loom.ci.step",
            repo,
            &job_id.to_string(),
            &number.to_string(),
        ]),
        ..run
    }
}

/// A suite span's context inside its run attempt's trace (#9089). Derived from
/// `(repo, job_id, sanitized suite name)`, following the same determinism rule
/// as the run, job and step contexts: a replayed or second-host emission of
/// the same suite is byte-identical in identity, which is what lets the
/// journal deduplicate on `span|<span_id>`.
///
/// The **name** is the identity here, unlike a step's number, because a
/// suite's position within a shard is not stable: `ci-wired.txt` order and the
/// `LOOM_CI_SHARD` round robin both shift every entry when one suite is added.
/// An ordinal-derived id would fork on every manifest edit and make "this
/// suite's duration over the last week" unanswerable — the exact question the
/// spans exist for. The name is sanitized *before* derivation so the id
/// matches the attribute that is actually emitted.
#[must_use]
pub fn suite_context(
    repo: &str,
    run_id: u64,
    attempt: u32,
    job_id: u64,
    suite: &str,
) -> TraceContext {
    let run = run_context(repo, run_id, attempt);
    TraceContext {
        span_id: SpanId::derived(&["loom.ci.suite", repo, &job_id.to_string(), suite]),
        ..run
    }
}

/// A test span's context inside its run attempt's trace (#9456). Derived from
/// `(repo, job_id, sanitized binary id, sanitized test name)`, following the
/// same determinism rule as the run, job, step and suite contexts: a replayed
/// or second-host emission of the same test is byte-identical in identity,
/// which is what lets the journal deduplicate on `span|<span_id>`.
///
/// The **stable name** is the identity here, as for a suite and unlike a
/// step's number, and for a sharper version of the same reason:
/// `--partition count:k/N` is a hash over the test list, so adding ONE test
/// reshuffles which leg runs many of the others. An ordinal-derived id would
/// fork on every suite edit and make "this test's duration over the last week"
/// unanswerable — the exact question the spans exist for. `classname` (the
/// nextest binary id) is part of the key because a test path is only unique
/// within its binary: `tests::smoke` exists in several. (`classname` is
/// `loom-daemon` for the lib tests and `loom-daemon::<target>` for an
/// integration binary.)
///
/// Both halves are sanitized *before* derivation so the id matches the
/// attributes that are actually emitted.
#[must_use]
pub fn test_context(
    repo: &str,
    run_id: u64,
    attempt: u32,
    job_id: u64,
    binary: &str,
    test: &str,
) -> TraceContext {
    let run = run_context(repo, run_id, attempt);
    TraceContext {
        span_id: SpanId::derived(&["loom.ci.test", repo, &job_id.to_string(), binary, test]),
        ..run
    }
}

fn span_status(conclusion: Option<&str>) -> SpanStatus {
    match conclusion {
        Some("success") => SpanStatus::Ok,
        Some("failure" | "timed_out" | "startup_failure") => SpanStatus::Error,
        _ => SpanStatus::Unset,
    }
}

/// Span attributes, stamped with the recording daemon's provenance. The
/// derived IDs stay host-independent; only these attributes name the recorder.
pub(super) fn span_attributes(pairs: Vec<(&str, Option<String>)>) -> TraceAttributes {
    let mut attributes = pairs
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k.to_string(), v)))
        .collect();
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    attributes
}

fn envelope(
    host_id: &str,
    record: TelemetryRecord,
    ctx: Option<TraceContext>,
) -> TelemetryEnvelope {
    let mut env = TelemetryEnvelope::new(host_id, record);
    env.trace_context = ctx;
    env
}

/// The envelopes of one completed run's run-level unit: `ci.run`,
/// `ci.duration` (run), and the `loom.ci.run` root span.
#[must_use]
pub fn run_envelopes(repo: &RepoJson, run: &RunJson, host_id: &str) -> Vec<TelemetryEnvelope> {
    let started_at = run.run_started_at.unwrap_or(run.created_at);
    let completed_at = run.updated_at.max(started_at);
    let duration = duration_ms(started_at, completed_at);
    let workflow = run.workflow();
    let ctx = run_context(&repo.full_name, run.id, run.run_attempt);
    let trigger_reason = run.trigger_reason().as_str();
    let record = CiRunRecord {
        repo: repo.full_name.clone(),
        visibility: repo.visibility(),
        run_id: run.id,
        run_attempt: run.run_attempt,
        workflow: workflow.clone(),
        git_ref: run.head_branch.clone(),
        head_sha: run.head_sha.clone(),
        event: run.event.clone(),
        status: run.status.clone().unwrap_or_default(),
        conclusion: run.conclusion.clone(),
        triggered_by: run
            .triggering_actor
            .as_ref()
            .or(run.actor.as_ref())
            .map(|a| a.login.clone()),
        started_at,
        completed_at,
        duration_ms: duration,
        queued_ms: run.queued_ms(),
        trigger_reason: Some(trigger_reason.to_string()),
    };
    let duration_record = CiDurationRecord {
        metric: CiDurationMetric::Run,
        repo: repo.full_name.clone(),
        visibility: repo.visibility(),
        run_id: run.id,
        run_attempt: run.run_attempt,
        job_id: None,
        workflow: workflow.clone(),
        job: None,
        runner: None,
        conclusion: run.conclusion.clone(),
        started_at,
        completed_at,
        duration_ms: duration,
    };
    let span = SpanRecord {
        context: ctx.clone(),
        parent_span_id: None,
        name: SpanName::CiRun,
        started_at,
        ended_at: completed_at,
        status: span_status(run.conclusion.as_deref()),
        attributes: span_attributes(vec![
            ("loom.repo", Some(repo.full_name.clone())),
            ("loom.repo.visibility", Some(visibility_str(repo.visibility()).to_string())),
            ("loom.ci.run_id", Some(run.id.to_string())),
            ("loom.ci.workflow", Some(workflow)),
            ("loom.ci.event", Some(run.event.clone())),
            ("loom.ci.conclusion", run.conclusion.clone()),
            ("loom.ci.head_sha", Some(run.head_sha.clone())),
            ("loom.ci.ref", run.head_branch.clone()),
            ("loom.pr_number", run.pr_number().map(|n| n.to_string())),
            ("loom.ci.queued_ms", run.queued_ms().map(|ms| ms.to_string())),
            ("loom.ci.run_attempt", Some(run.run_attempt.to_string())),
            ("loom.ci.trigger_reason", Some(trigger_reason.to_string())),
        ]),
        events: Vec::new(),
        links: Vec::new(),
    };
    vec![
        envelope(host_id, TelemetryRecord::CiRun(record), Some(ctx.clone())),
        envelope(host_id, TelemetryRecord::CiDuration(duration_record), None),
        envelope(host_id, TelemetryRecord::Span(span), Some(ctx)),
    ]
}

pub(super) fn visibility_str(visibility: RepoVisibility) -> &'static str {
    match visibility {
        RepoVisibility::Public => "public",
        RepoVisibility::Private => "private",
    }
}

/// The envelopes of one completed job's unit: `ci.job`, `ci.duration`
/// (job), and the `loom.ci.job` span parented to the run span.
///
/// `baseline` is **this job's own attempt's** [`JobCreationBaseline`], from
/// which its `dependency_wait_ms` is measured; pass
/// `JobCreationBaselines::of_listing(&jobs).for_job(job)` when handling a whole
/// `filter=all` listing (which mixes attempts), or
/// `JobCreationBaseline::default()` when only one job is in hand and the
/// dependency wait is deliberately not being measured.
#[must_use]
pub fn job_envelopes(
    repo: &RepoJson,
    run: &RunJson,
    job: &JobJson,
    baseline: JobCreationBaseline,
    host_id: &str,
) -> Vec<TelemetryEnvelope> {
    let started_at = job.started_at.unwrap_or(run.created_at);
    let completed_at = job.completed_at.unwrap_or(started_at).max(started_at);
    let duration = duration_ms(started_at, completed_at);
    let workflow = run.workflow();
    let runner = job.labels.first().cloned();
    let ctx = job_context(&repo.full_name, run.id, job.run_attempt, job.id);
    let run_span = run_context(&repo.full_name, run.id, job.run_attempt).span_id;
    let shard = parse_shard(&job.name);
    let dependency_wait_ms = job.dependency_wait_ms(baseline);
    let record = CiJobRecord {
        repo: repo.full_name.clone(),
        visibility: repo.visibility(),
        run_id: run.id,
        job_id: job.id,
        workflow: workflow.clone(),
        job: job.name.clone(),
        runner: runner.clone(),
        attempts: job.run_attempt,
        status: job.status.clone(),
        conclusion: job.conclusion.clone(),
        timed_out: job.conclusion.as_deref() == Some("timed_out"),
        started_at,
        completed_at,
        duration_ms: duration,
        queued_ms: job.queued_ms(),
        dependency_wait_ms,
        shard_index: shard.index,
        shard_total: shard.total,
        shard_kind: shard.kind.as_str().to_string(),
    };
    let duration_record = CiDurationRecord {
        metric: CiDurationMetric::Job,
        repo: repo.full_name.clone(),
        visibility: repo.visibility(),
        run_id: run.id,
        run_attempt: job.run_attempt,
        job_id: Some(job.id),
        workflow: workflow.clone(),
        job: Some(job.name.clone()),
        runner: runner.clone(),
        conclusion: job.conclusion.clone(),
        started_at,
        completed_at,
        duration_ms: duration,
    };
    let span = SpanRecord {
        context: ctx.clone(),
        parent_span_id: Some(run_span),
        name: SpanName::CiJob,
        started_at,
        ended_at: completed_at,
        status: span_status(job.conclusion.as_deref()),
        attributes: span_attributes(vec![
            ("loom.repo", Some(repo.full_name.clone())),
            ("loom.repo.visibility", Some(visibility_str(repo.visibility()).to_string())),
            ("loom.ci.run_id", Some(run.id.to_string())),
            ("loom.ci.job_id", Some(job.id.to_string())),
            ("loom.ci.workflow", Some(workflow)),
            ("loom.ci.job", Some(job.name.clone())),
            ("loom.ci.runner", runner),
            ("loom.ci.attempts", Some(job.run_attempt.to_string())),
            ("loom.ci.conclusion", job.conclusion.clone()),
            ("loom.ci.head_sha", Some(run.head_sha.clone())),
            ("loom.ci.ref", run.head_branch.clone()),
            ("loom.pr_number", run.pr_number().map(|n| n.to_string())),
            ("loom.ci.queued_ms", job.queued_ms().map(|ms| ms.to_string())),
            ("loom.ci.dependency_wait_ms", dependency_wait_ms.map(|ms| ms.to_string())),
            ("loom.ci.shard.index", shard.index.map(|i| i.to_string())),
            ("loom.ci.shard.total", shard.total.map(|t| t.to_string())),
            ("loom.ci.shard.kind", Some(shard.kind.as_str().to_string())),
        ]),
        events: Vec::new(),
        links: Vec::new(),
    };
    let mut envelopes = vec![
        envelope(host_id, TelemetryRecord::CiJob(record), Some(ctx.clone())),
        envelope(host_id, TelemetryRecord::CiDuration(duration_record), None),
        envelope(host_id, TelemetryRecord::Span(span), Some(ctx.clone())),
    ];
    envelopes.extend(step_envelopes(repo, run, job, &shard, &ctx, host_id));
    envelopes
}

/// The `loom.ci.step` spans of one job, each a child of that job's span
/// (#9089).
///
/// Built from the `steps[]` array of the jobs listing the poller already
/// fetched, so no extra API call: a job span alone cannot say whether a Rust
/// leg's ~250s went to compiling or to running tests, and these can. A step
/// GitHub reported no start or no completion for produces no span
/// ([`StepJson::window`]); the rest are emitted in order, capped at
/// [`MAX_STEP_SPANS_PER_JOB`].
///
/// Each span repeats its job's identity (`loom.ci.job`, `loom.ci.job_id`) and
/// shard attributes so "which step of which leg is slow" is one group-by, not
/// a trace join. Step spans are deliberately **span-only** — there is no
/// `ci.step` log record and no duration metric — because the metric-label
/// allowlist admits no step dimension and a per-step histogram would multiply
/// the 30-day metric series count by the step count of every job.
#[must_use]
fn step_envelopes(
    repo: &RepoJson,
    run: &RunJson,
    job: &JobJson,
    shard: &ShardInfo,
    job_ctx: &TraceContext,
    host_id: &str,
) -> Vec<TelemetryEnvelope> {
    job.steps
        .iter()
        .filter_map(|step| Some((step, step.window()?)))
        .take(MAX_STEP_SPANS_PER_JOB)
        .map(|(step, (started_at, ended_at))| {
            let ctx = step_context(&repo.full_name, run.id, job.run_attempt, job.id, step.number);
            let span = SpanRecord {
                context: ctx.clone(),
                parent_span_id: Some(job_ctx.span_id.clone()),
                name: SpanName::CiStep,
                started_at,
                ended_at,
                status: span_status(step.conclusion.as_deref()),
                attributes: span_attributes(vec![
                    ("loom.repo", Some(repo.full_name.clone())),
                    ("loom.repo.visibility", Some(visibility_str(repo.visibility()).to_string())),
                    ("loom.ci.run_id", Some(run.id.to_string())),
                    ("loom.ci.job_id", Some(job.id.to_string())),
                    ("loom.ci.workflow", Some(run.workflow())),
                    ("loom.ci.job", Some(job.name.clone())),
                    ("loom.ci.step", Some(step_name(&step.name))),
                    ("loom.ci.step_number", Some(step.number.to_string())),
                    ("loom.ci.conclusion", step.conclusion.clone()),
                    ("loom.ci.shard.index", shard.index.map(|i| i.to_string())),
                    ("loom.ci.shard.total", shard.total.map(|t| t.to_string())),
                    ("loom.ci.shard.kind", Some(shard.kind.as_str().to_string())),
                ]),
                events: Vec::new(),
                links: Vec::new(),
            };
            envelope(host_id, TelemetryRecord::Span(span), Some(ctx))
        })
        .collect()
}

/// The journal-level identity of one CI envelope — what "already emitted"
/// means when a committed-but-unconfirmed unit is replayed. `None` for any
/// non-CI envelope.
#[must_use]
pub fn envelope_identity(env: &TelemetryEnvelope) -> Option<String> {
    match &env.record {
        TelemetryRecord::CiRun(r) => {
            Some(format!("ci.run|{}|{}|{}", r.repo, r.run_id, r.run_attempt))
        }
        TelemetryRecord::CiJob(r) => Some(format!("ci.job|{}|{}", r.repo, r.job_id)),
        TelemetryRecord::CiJobLog(r) => {
            Some(format!("ci.job.log|{}|{}|{}", r.repo, r.job_id, r.chunk_index))
        }
        TelemetryRecord::CiDuration(r) => Some(format!(
            "ci.duration|{}|{}|{}|{}",
            r.repo,
            r.run_id,
            r.run_attempt,
            r.job_id
                .map_or_else(|| "run".to_string(), |id| id.to_string())
        )),
        TelemetryRecord::Span(s) => Some(format!("span|{}", s.context.span_id.as_str())),
        _ => None,
    }
}
