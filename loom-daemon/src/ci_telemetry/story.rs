//! Stitch CI runs into their issue's D32 story trace (Issue #9088, phase 2 of
//! #9037; harness-ops `docs/story-trace.md`).
//!
//! A completed run that resolves to **exactly one** issue N of its own repo
//! gets an *additional* `loom.ci.run` span in N's story trace
//! (`story_context(repo_id, N)`), parented to the story root span, and each of
//! its jobs an additional `loom.ci.job` span under that. The run's own
//! per-run trace ([`super::records::run_context`]) is emitted exactly as
//! before; the two copies link to each other both ways, so existing CI saved
//! views keep working and a story reader can jump to the per-run trace.
//!
//! # Which issue a run belongs to
//!
//! The candidates are the union of
//!
//! 1. every closing reference of every PR in the run's `pull_requests[]`
//!    (GraphQL `closingIssuesReferences` — the forge's own closes-graph, not
//!    body parsing), and
//! 2. `head_branch == feature/issue-N` (the fallback that covers `push` runs,
//!    which GitHub reports with no PR).
//!
//! Exactly one same-repo issue stitches. Zero candidates, several, a closing
//! reference into another repository, a truncated reference list, a PR whose
//! references could not be read, or a repo whose GitHub `repo_id` cannot be
//! resolved ([`crate::telemetry::repo_identity`]) all mean **not stitched** —
//! counted in the cycle summary and logged, never guessed. There is no
//! name-derived fallback: a run in a repo without a `repo_id` would otherwise
//! mint a story trace the storyline reconciler can never join.
//!
//! # Idempotence
//!
//! Every stitched id is derived: the story trace and parent from D32, the run
//! span from `(repo_id, run_id, attempt)` and the job span from
//! `(repo_id, job_id)`. The stitched spans travel inside the same ledger units
//! as the per-run records, so they inherit the poller's exactly-once commit,
//! and a re-derivation (a re-poll, a replay, another host) yields the same ids.
//!
//! # API budget
//!
//! Closing references cost one GraphQL request per repo per cycle at most,
//! batched over every not-yet-emitted run's PRs (aliased `pullRequest` fields,
//! [`GRAPHQL_BATCH`] per request), and cached per PR for [`CLOSING_REFS_TTL`].
//! A repo with no candidate run makes no request at all, and neither does a
//! repo whose `repo_id` is unresolvable. A GraphQL rate limit is not fed to
//! the org-wide REST backoff (a separate budget); it only suppresses further
//! lookups for the rest of the cycle, leaving those runs unstitched.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::api::{ApiError, GithubApi};
use super::records::{JobJson, RunJson};
use crate::telemetry::repo_identity::{self, RepoIdentity};
use crate::telemetry::trace::{
    story_context, SpanId, SpanLink, SpanName, TraceContext, STORY_KEY_VERSION,
};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// Resolves `owner/repo` to its GitHub identity (a seam for tests).
pub type RepoIdentityFn = fn(&str) -> Option<RepoIdentity>;

/// The production resolver: the #9068 process-wide `repo_id` cache.
#[must_use]
pub fn resolve_repo_identity(owner_repo: &str) -> Option<RepoIdentity> {
    repo_identity::resolve(owner_repo)
}

/// PRs per GraphQL request (aliased fields; far below GitHub's node limits).
pub const GRAPHQL_BATCH: usize = 50;
/// Closing references read per PR. More than one same-repo issue is already
/// ambiguous, so a small page suffices; `totalCount` catches the rest.
const CLOSING_REFS_PAGE: usize = 10;
/// How long a PR's closing references are reused across cycles.
pub const CLOSING_REFS_TTL: Duration = Duration::from_secs(600);
const CACHE_CAP: usize = 4096;

/// One PR's closing references as the forge reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosingRefs {
    /// `(repository databaseId, issue number)` per reference returned.
    pub refs: Vec<(u64, u32)>,
    /// `totalCount` — larger than `refs.len()` when the list was truncated.
    pub total: usize,
}

/// A resolved story membership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryRef {
    pub repo_id: u64,
    /// `Owner/Name#N` — the `loom.story` attribute, from the resolved name.
    pub story: String,
    pub issue: u32,
    /// The run's PR, when GitHub associated exactly one with it.
    pub pr_number: Option<u64>,
    /// `story_context(repo_id, issue)`: the story trace and its root span.
    pub root: TraceContext,
}

/// What stitching decided for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stitch {
    Stitched(StoryRef),
    /// No PR closing reference and no `feature/issue-N` branch.
    NoCandidate,
    /// More than one candidate; the (same-repo) issue numbers seen.
    Ambiguous(Vec<u32>),
    /// A candidate may exist but cannot be established; the reason.
    Unresolved(String),
}

/// A stitched run span's id: derived from `(repo_id, run_id, attempt)`.
#[must_use]
pub fn story_run_span_id(repo_id: u64, run_id: u64, attempt: u32) -> SpanId {
    SpanId::derived(&[
        "loom.ci.story.run",
        &repo_id.to_string(),
        &run_id.to_string(),
        &attempt.to_string(),
    ])
}

/// A stitched job span's id: derived from `(repo_id, job_id)` (a job id is
/// already unique per attempt, as in [`job_context`]).
#[must_use]
pub fn story_job_span_id(repo_id: u64, job_id: u64) -> SpanId {
    SpanId::derived(&[
        "loom.ci.story.job",
        &repo_id.to_string(),
        &job_id.to_string(),
    ])
}

fn distinct_prs(run: &RunJson) -> Vec<u64> {
    run.pull_requests
        .iter()
        .map(|pr| pr.number)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Whether a run could be stitched at all — i.e. it is worth resolving the
/// repo identity and PR references for.
#[must_use]
pub fn has_candidate_source(run: &RunJson) -> bool {
    !run.pull_requests.is_empty()
        || run
            .head_branch
            .as_deref()
            .and_then(crate::claim_reconciliation::parse_issue_from_branch)
            .is_some()
}

/// The pure decision for one run. `closing` maps each PR number to its
/// closing references, `None` when they could not be read.
#[must_use]
pub fn decide(
    run: &RunJson,
    identity: Option<&RepoIdentity>,
    closing: &HashMap<u64, Option<ClosingRefs>>,
) -> Stitch {
    if !has_candidate_source(run) {
        return Stitch::NoCandidate;
    }
    let Some(identity) = identity else {
        return Stitch::Unresolved("repo_id unresolvable (no name-derived fallback)".into());
    };
    let prs = distinct_prs(run);
    let mut issues = BTreeSet::new();
    let mut foreign = 0_usize;
    for pr in &prs {
        let Some(Some(refs)) = closing.get(pr) else {
            return Stitch::Unresolved(format!("closing references of PR #{pr} unreadable"));
        };
        if refs.total > refs.refs.len() {
            // Truncated: at least CLOSING_REFS_PAGE + 1 references, ambiguous
            // whatever the unseen ones are.
            foreign += refs.total - refs.refs.len();
        }
        for (repo, number) in &refs.refs {
            if *repo == identity.id {
                issues.insert(*number);
            } else {
                foreign += 1;
            }
        }
    }
    if let Some(n) = run
        .head_branch
        .as_deref()
        .and_then(crate::claim_reconciliation::parse_issue_from_branch)
    {
        issues.insert(n);
    }
    match (issues.len(), foreign) {
        (0, 0) => Stitch::NoCandidate,
        (1, 0) => {
            let issue = issues.first().copied().unwrap_or_default();
            match story_context(identity.id, issue) {
                Ok(root) => Stitch::Stitched(StoryRef {
                    repo_id: identity.id,
                    story: format!("{}#{issue}", identity.full_name),
                    issue,
                    pr_number: (prs.len() == 1).then(|| prs[0]),
                    root,
                }),
                Err(error) => Stitch::Unresolved(format!("story id refused: {error}")),
            }
        }
        // Every reference points elsewhere (or was truncated away): there is
        // no candidate in *this* repository to be ambiguous between.
        (0, _) => Stitch::Unresolved(format!(
            "no candidate issue in this repository ({foreign} closing reference(s) elsewhere or \
             unseen)"
        )),
        _ => Stitch::Ambiguous(issues.into_iter().collect()),
    }
}

/// Per-repo stitching state for one cycle: the resolved identity and every
/// candidate PR's closing references, fetched once up front.
pub struct RepoStories {
    identity: Option<RepoIdentity>,
    closing: HashMap<u64, Option<ClosingRefs>>,
}

impl RepoStories {
    /// Resolve what `runs` (the repo's not-yet-emitted runs) need: nothing
    /// when none has a candidate source, else the repo identity and — only if
    /// that resolves — the closing references of every PR they name.
    /// `graphql_ok` is the cycle-wide GraphQL gate (cleared on a rate limit).
    pub fn prepare(
        resolve: RepoIdentityFn,
        api: &dyn GithubApi,
        full_name: &str,
        runs: &[&RunJson],
        requests: &mut usize,
        graphql_ok: &mut bool,
    ) -> Self {
        let mut stories = RepoStories {
            identity: None,
            closing: HashMap::new(),
        };
        if !runs.iter().any(|run| has_candidate_source(run)) {
            return stories;
        }
        stories.identity = resolve(full_name);
        if stories.identity.is_none() {
            return stories;
        }
        let prs: BTreeSet<u64> = runs.iter().flat_map(|run| distinct_prs(run)).collect();
        stories.closing = closing_refs(
            api,
            full_name,
            &prs.into_iter().collect::<Vec<_>>(),
            requests,
            graphql_ok,
        );
        stories
    }

    #[must_use]
    pub fn decide(&self, run: &RunJson) -> Stitch {
        decide(run, self.identity.as_ref(), &self.closing)
    }
}

type CacheKey = (String, u64);

fn cache() -> &'static Mutex<HashMap<CacheKey, (ClosingRefs, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, (ClosingRefs, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Closing references for `prs` of `full_name`: cached entries first, the
/// rest in [`GRAPHQL_BATCH`]-sized GraphQL requests. A PR whose references
/// could not be read maps to `None` (only successes are cached).
pub fn closing_refs(
    api: &dyn GithubApi,
    full_name: &str,
    prs: &[u64],
    requests: &mut usize,
    graphql_ok: &mut bool,
) -> HashMap<u64, Option<ClosingRefs>> {
    let repo_key = full_name.to_ascii_lowercase();
    let mut out = HashMap::new();
    let mut missing = Vec::new();
    {
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for pr in prs {
            match guard.get(&(repo_key.clone(), *pr)) {
                Some((refs, at)) if at.elapsed() < CLOSING_REFS_TTL => {
                    out.insert(*pr, Some(refs.clone()));
                }
                _ => missing.push(*pr),
            }
        }
    }
    let Some((owner, name)) = full_name.split_once('/').filter(|(o, n)| {
        [*o, *n].iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    }) else {
        out.extend(missing.into_iter().map(|pr| (pr, None)));
        return out;
    };
    for batch in missing.chunks(GRAPHQL_BATCH) {
        let fetched = if *graphql_ok {
            *requests += 1;
            match api.graphql(&closing_refs_query(owner, name, batch)) {
                // GraphQL reports its own rate limit as a 200 with an
                // `errors[].type == "RATE_LIMITED"` body.
                Ok(response) if response.body.contains("\"RATE_LIMITED\"") => {
                    log::warn!(
                        "ci_telemetry: GraphQL rate-limited reading PR closing references; \
                         no more story lookups this cycle"
                    );
                    *graphql_ok = false;
                    HashMap::new()
                }
                Ok(response) => parse_closing_refs(&response.body, batch),
                Err(ApiError::RateLimited { detail, .. }) => {
                    log::warn!(
                        "ci_telemetry: GraphQL rate-limited reading PR closing references; \
                         no more story lookups this cycle ({detail})"
                    );
                    *graphql_ok = false;
                    HashMap::new()
                }
                Err(error) => {
                    log::warn!(
                        "ci_telemetry: could not read closing references of {full_name} PRs \
                         {batch:?}: {error}"
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };
        let mut guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() > CACHE_CAP {
            guard.retain(|_, (_, at)| at.elapsed() < CLOSING_REFS_TTL);
        }
        for pr in batch {
            let refs = fetched.get(pr).cloned();
            if let Some(refs) = &refs {
                guard.insert((repo_key.clone(), *pr), (refs.clone(), Instant::now()));
            }
            out.insert(*pr, refs);
        }
    }
    out
}

/// One aliased query over `prs` (`p<N>: pullRequest(number: N)`). `owner`
/// and `name` are validated to `[A-Za-z0-9._-]` by the caller.
#[must_use]
pub fn closing_refs_query(owner: &str, name: &str, prs: &[u64]) -> String {
    let fields: String = prs
        .iter()
        .map(|pr| {
            format!(
                " p{pr}: pullRequest(number: {pr}) {{ closingIssuesReferences(first: \
                 {CLOSING_REFS_PAGE}) {{ totalCount nodes {{ number repository {{ databaseId }} }} }} }}"
            )
        })
        .collect();
    format!("query {{ repository(owner: \"{owner}\", name: \"{name}\") {{{fields} }} }}")
}

/// Parse a [`closing_refs_query`] answer. A PR is present in the result only
/// when its field parsed completely; a `null` PR or a malformed node leaves
/// it out (unreadable), never an empty reference list.
#[must_use]
pub fn parse_closing_refs(body: &str, prs: &[u64]) -> HashMap<u64, ClosingRefs> {
    let mut out = HashMap::new();
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return out;
    };
    let repository = &json["data"]["repository"];
    for pr in prs {
        let refs = &repository[format!("p{pr}")]["closingIssuesReferences"];
        let (Some(total), Some(nodes)) = (refs["totalCount"].as_u64(), refs["nodes"].as_array())
        else {
            continue;
        };
        let parsed: Option<Vec<(u64, u32)>> = nodes
            .iter()
            .map(|node| {
                Some((
                    node["repository"]["databaseId"].as_u64()?,
                    u32::try_from(node["number"].as_u64()?).ok()?,
                ))
            })
            .collect();
        if let (Some(refs), Ok(total)) = (parsed, usize::try_from(total)) {
            out.insert(*pr, ClosingRefs { refs, total });
        }
    }
    out
}

fn story_attributes(span: &mut crate::telemetry::trace::SpanRecord, story: &StoryRef) {
    let attributes = &mut span.attributes;
    attributes.insert("loom.story".into(), story.story.clone());
    attributes.insert("loom.story.key_version".into(), STORY_KEY_VERSION.into());
    attributes.insert("loom.issue".into(), story.issue.to_string());
    // The per-run span's `loom.pr_number` may be the branch's *issue* number
    // (#9007's fallback); in the story only a real PR number is carried.
    attributes.remove("loom.pr_number");
    if let Some(pr) = story.pr_number {
        attributes.insert("loom.pr_number".into(), pr.to_string());
    }
}

/// Append the stitched copy of the span named `name` in `envelopes`, with
/// `context`/`parent`, and link the two both ways. The per-run copy keeps its
/// ids; only a link is added to it.
fn stitch(
    envelopes: &mut Vec<TelemetryEnvelope>,
    name: SpanName,
    context: TraceContext,
    parent: SpanId,
    story: &StoryRef,
) {
    let Some(original) = envelopes.iter_mut().find_map(|env| match &mut env.record {
        TelemetryRecord::Span(span) if span.name == name => Some(span),
        _ => None,
    }) else {
        return;
    };
    original.links.push(SpanLink {
        context: context.clone(),
    });
    let mut copy = original.clone();
    copy.links = vec![SpanLink {
        context: original.context.clone(),
    }];
    copy.context = context.clone();
    copy.parent_span_id = Some(parent);
    story_attributes(&mut copy, story);
    let host_id = envelopes[0].host_id.clone();
    let mut env = TelemetryEnvelope::new(&host_id, TelemetryRecord::Span(copy));
    env.trace_context = Some(context);
    envelopes.push(env);
}

/// The stitched run span's context in the story trace.
#[must_use]
pub fn story_run_context(story: &StoryRef, run_id: u64, attempt: u32) -> TraceContext {
    TraceContext {
        trace_id: story.root.trace_id.clone(),
        span_id: story_run_span_id(story.repo_id, run_id, attempt),
        flags: 1,
    }
}

/// Add the stitched `loom.ci.run` span to a run unit's envelopes.
pub fn stitch_run(envelopes: &mut Vec<TelemetryEnvelope>, story: &StoryRef, run: &RunJson) {
    let context = story_run_context(story, run.id, run.run_attempt);
    stitch(envelopes, SpanName::CiRun, context, story.root.span_id.clone(), story);
}

/// Add the stitched `loom.ci.job` span to a job unit's envelopes, parented
/// to the stitched run span of the job's attempt.
pub fn stitch_job(
    envelopes: &mut Vec<TelemetryEnvelope>,
    story: &StoryRef,
    run: &RunJson,
    job: &JobJson,
) {
    let context = TraceContext {
        trace_id: story.root.trace_id.clone(),
        span_id: story_job_span_id(story.repo_id, job.id),
        flags: 1,
    };
    let parent = story_run_span_id(story.repo_id, run.id, job.run_attempt);
    stitch(envelopes, SpanName::CiJob, context, parent, story);
}
