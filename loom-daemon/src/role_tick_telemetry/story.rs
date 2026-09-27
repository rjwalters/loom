//! Role-runner ticks join the D32 story trace of every issue/PR they acted on
//! (Issue #9168, part of #9037).
//!
//! A tick is spawned without a target, so its own `loom.role_attempt` root is
//! an independent trace. After the tick ends, the targets its transcript
//! wrote to ([`super::targets`], collected in the same pass that tallies
//! `actions`) each get one **additional** `loom.role_attempt` span in that
//! target's story trace:
//!
//! - **Which story.** The forge classifies each number with one batched
//!   GraphQL `issueOrPullRequest` query ([`GRAPHQL_BATCH`] per request,
//!   answers cached for [`CLOSING_REFS_TTL`]). An issue is its own story. A
//!   PR goes through the CI stitcher's exact rule
//!   ([`crate::ci_telemetry::story::decide_candidates`]): its closing
//!   references plus its `feature/issue-N` head branch must name exactly one
//!   issue of this repository. Zero or several candidates, an unreadable
//!   answer, a foreign repository, or an unresolvable `repo_id` → no span.
//! - **Identity.** Parent: the story root (`story_context(repo_id, N)`).
//!   Span id: `story_root.derived_child(["loom.role_tick", <tick execution
//!   id>, "pr:<M>" | "issue:<N>"])` — every input is on the span (trace and
//!   parent ids, `loom.sweep_id`, `loom.pr_number` / `loom.issue`), so a
//!   re-emit yields the same id.
//! - **Content.** `loom.role`, `loom.issue`, `loom.pr_number` (PR targets),
//!   `loom.story`, `loom.story.key_version`, `loom.repo`, `loom.result`,
//!   `loom.runtime`/`loom.model` when known, `loom.sweep_id` (the tick's
//!   execution id) and `loom.timing_source=tick`: start/end are the whole
//!   tick's, not the per-target action's. A span link points at the tick's
//!   own root.
//! - **Delivery.** Appended as completed spans to the tick's own trace
//!   journal, which the daemon's backfill drains into the durable queue like
//!   every lifecycle span. A span id already in the journal is not appended
//!   twice.
//!
//! Everything here is best-effort and runs after the tick's child exited:
//! a failure logs and drops the story spans, never the tick.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::targets::{own_repo_numbers, Target};
use crate::ci_telemetry::api::{ApiError, ApiResponse, GithubApi};
use crate::ci_telemetry::story::{
    decide_candidates, owner_name, parse_refs_field, ClosingRefs, Stitch, StoryRef,
    CLOSING_REFS_PAGE, CLOSING_REFS_TTL, GRAPHQL_BATCH,
};
use crate::observability::lifecycle::RoleTrace;
use crate::telemetry::repo_identity::RepoIdentity;
use crate::telemetry::trace::{
    SpanLink, SpanName, SpanRecord, SpanStatus, TraceAttributes, STORY_KEY_VERSION,
};

/// Every attribute key a story span can carry, besides provenance. Each must
/// survive the daemon's span allowlist and the gateway's span `keep_keys`
/// (contract-tested).
pub const STORY_SPAN_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.role",
    "loom.issue",
    "loom.pr_number",
    "loom.story",
    "loom.story.key_version",
    "loom.repo",
    "loom.result",
    "loom.runtime",
    "loom.model",
    "loom.sweep_id",
    "loom.timing_source",
];

/// The bound on one tick's forge lookup; past it the story spans are dropped.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
const CACHE_CAP: usize = 4096;

/// What the forge says a number is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Issue,
    PullRequest {
        refs: ClosingRefs,
        head_branch: Option<String>,
    },
}

/// The tick-level facts every story span of one tick shares.
#[derive(Debug, Clone)]
pub struct TickFacts {
    pub trace: RoleTrace,
    pub role: String,
    pub ended_at: DateTime<Utc>,
    /// The tick's `RoleTickResult`, serialized (`success`, `failure`, …).
    pub result: String,
    pub runtime: Option<String>,
    pub model: Option<String>,
}

/// Join `facts`' tick to the stories of `targets`. Blocking and best-effort:
/// one local `git`, the cached `repo_id`, and at most one bounded GraphQL
/// request per [`GRAPHQL_BATCH`] uncached numbers.
pub fn emit(root: &Path, facts: &TickFacts, targets: &BTreeSet<Target>) {
    if targets.is_empty() || !crate::observability::tracing::enabled(root) {
        return;
    }
    let Some(slug) = crate::release_resolve::host::repo_slug(root) else {
        return;
    };
    let (numbers, foreign) = own_repo_numbers(targets, &slug);
    if foreign > 0 {
        log::debug!(
            "role_tick_telemetry: {} tick refused {foreign} cross-repo target(s) (#9168)",
            facts.role
        );
    }
    if numbers.is_empty() {
        return;
    }
    let Some(identity) = crate::telemetry::repo_identity::resolve(&slug) else {
        return;
    };
    let resolved = resolve(&BoundedGh, &identity, &numbers);
    let spans = plan(facts, &identity, &slug, &numbers, &resolved);
    if let Err(error) = journal(root, &facts.trace.execution, spans) {
        log::warn!("role_tick_telemetry: story spans not journalled (#9168): {error}");
    }
}

/// The story spans for `numbers`, given what the forge said about each
/// (`resolved`; a missing entry is unreadable). Pure.
#[must_use]
pub fn plan(
    facts: &TickFacts,
    identity: &RepoIdentity,
    slug: &str,
    numbers: &[u32],
    resolved: &HashMap<u32, Resolved>,
) -> Vec<SpanRecord> {
    numbers
        .iter()
        .filter_map(|number| match story_of(identity, *number, resolved.get(number)) {
            Stitch::Stitched(story) => Some(story_span(facts, slug, &story)),
            other => {
                log::debug!(
                    "role_tick_telemetry: {} tick target #{number} joins no story: {other:?}",
                    facts.role
                );
                None
            }
        })
        .collect()
}

/// Which story `number` belongs to — the CI stitcher's rule, see the module
/// docs.
#[must_use]
pub fn story_of(identity: &RepoIdentity, number: u32, resolved: Option<&Resolved>) -> Stitch {
    match resolved {
        None => Stitch::Unresolved(format!("#{number} could not be classified")),
        Some(Resolved::Issue) => decide_candidates(identity, &[], &HashMap::new(), Some(number)),
        Some(Resolved::PullRequest { refs, head_branch }) => {
            let pr = u64::from(number);
            let closing = HashMap::from([(pr, Some(refs.clone()))]);
            let branch = head_branch
                .as_deref()
                .and_then(crate::claim_reconciliation::parse_issue_from_branch);
            decide_candidates(identity, &[pr], &closing, branch)
        }
    }
}

/// The span-id key naming the target within its story.
fn target_key(story: &StoryRef) -> String {
    match story.pr_number {
        Some(pr) => format!("pr:{pr}"),
        None => format!("issue:{}", story.issue),
    }
}

/// One tick's span in `story`.
#[must_use]
pub fn story_span(facts: &TickFacts, slug: &str, story: &StoryRef) -> SpanRecord {
    let context =
        story
            .root
            .derived_child(&["loom.role_tick", &facts.trace.execution, &target_key(story)]);
    let mut attributes: TraceAttributes = [
        ("loom.role", facts.role.clone()),
        ("loom.issue", story.issue.to_string()),
        ("loom.story", story.story.clone()),
        ("loom.story.key_version", STORY_KEY_VERSION.to_string()),
        ("loom.repo", slug.to_ascii_lowercase()),
        ("loom.result", facts.result.clone()),
        ("loom.sweep_id", facts.trace.execution.clone()),
        ("loom.timing_source", "tick".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let optional = [
        ("loom.pr_number", story.pr_number.map(|pr| pr.to_string())),
        ("loom.runtime", facts.runtime.clone()),
        ("loom.model", facts.model.clone()),
    ];
    for (key, value) in optional {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            attributes.insert(key.to_string(), value);
        }
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    let started_at = facts.trace.started_at;
    SpanRecord {
        context,
        parent_span_id: Some(story.root.span_id.clone()),
        name: SpanName::RoleAttempt,
        started_at,
        ended_at: facts.ended_at.max(started_at),
        status: if facts.result == "success" {
            SpanStatus::Ok
        } else {
            SpanStatus::Error
        },
        attributes,
        events: Vec::new(),
        links: vec![SpanLink {
            context: facts.trace.context.clone(),
        }],
    }
    .bounded()
}

/// Append `spans` to `execution`'s trace journal, skipping any span id the
/// journal already holds. Returns how many were appended.
pub fn journal(root: &Path, execution: &str, spans: Vec<SpanRecord>) -> anyhow::Result<usize> {
    use crate::telemetry::trace::{journal::Journal, store::TraceStore};
    if spans.is_empty() {
        return Ok(0);
    }
    let store = TraceStore::new(root);
    let journal = Journal::for_context(&store.path(root, execution));
    // A journal the daemon already retired is simply recreated: its story
    // spans are parented outside it, so it drains and retires again.
    let existing: BTreeSet<String> = if journal.path().exists() {
        journal
            .completed()?
            .into_iter()
            .map(|span| span.context.span_id.as_str().to_owned())
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut appended = 0;
    for span in spans {
        if existing.contains(span.context.span_id.as_str()) {
            continue;
        }
        journal.append_completed(span)?;
        appended += 1;
    }
    Ok(appended)
}

// ---------------------------------------------------------------------------
// Forge classification: one aliased `issueOrPullRequest` query per batch.
// ---------------------------------------------------------------------------

type CacheKey = (String, u32);

fn cache() -> &'static Mutex<HashMap<CacheKey, (Resolved, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, (Resolved, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Classify `numbers` of `identity`'s repository: cached answers first, the
/// rest in [`GRAPHQL_BATCH`]-sized requests. An unreadable number is absent
/// from the result (only successes are cached); the first failure stops
/// further requests for this call.
pub fn resolve(
    api: &dyn GithubApi,
    identity: &RepoIdentity,
    numbers: &[u32],
) -> HashMap<u32, Resolved> {
    let repo_key = identity.full_name.to_ascii_lowercase();
    let mut out = HashMap::new();
    let mut missing = Vec::new();
    {
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for number in numbers {
            match guard.get(&(repo_key.clone(), *number)) {
                Some((resolved, at)) if at.elapsed() < CLOSING_REFS_TTL => {
                    out.insert(*number, resolved.clone());
                }
                _ => missing.push(*number),
            }
        }
    }
    let Some((owner, name)) = owner_name(&identity.full_name) else {
        return out;
    };
    for batch in missing.chunks(GRAPHQL_BATCH) {
        let fetched = match api.graphql(&classify_query(owner, name, batch)) {
            Ok(response) if response.body.contains("\"RATE_LIMITED\"") => {
                log::warn!("role_tick_telemetry: GraphQL rate-limited; tick joins no story");
                break;
            }
            Ok(response) => parse_classified(&response.body, batch),
            Err(error) => {
                log::warn!(
                    "role_tick_telemetry: could not classify {} targets {batch:?}: {error}",
                    identity.full_name
                );
                break;
            }
        };
        let mut guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() > CACHE_CAP {
            guard.retain(|_, (_, at)| at.elapsed() < CLOSING_REFS_TTL);
        }
        for (number, resolved) in fetched {
            guard.insert((repo_key.clone(), number), (resolved.clone(), Instant::now()));
            out.insert(number, resolved);
        }
    }
    out
}

/// One aliased query (`t<N>: issueOrPullRequest(number: N)`). `owner` and
/// `name` are validated by [`owner_name`].
#[must_use]
pub fn classify_query(owner: &str, name: &str, numbers: &[u32]) -> String {
    let fields: String = numbers
        .iter()
        .map(|n| {
            format!(
                " t{n}: issueOrPullRequest(number: {n}) {{ __typename ... on PullRequest {{ \
                 headRefName closingIssuesReferences(first: {CLOSING_REFS_PAGE}) {{ totalCount \
                 nodes {{ number repository {{ databaseId }} }} }} }} }}"
            )
        })
        .collect();
    format!("query {{ repository(owner: \"{owner}\", name: \"{name}\") {{{fields} }} }}")
}

/// Parse a [`classify_query`] answer. A number is present only when its
/// field parsed completely; `null` (no such number) leaves it out.
#[must_use]
pub fn parse_classified(body: &str, numbers: &[u32]) -> HashMap<u32, Resolved> {
    let mut out = HashMap::new();
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return out;
    };
    let repository = &json["data"]["repository"];
    for number in numbers {
        let node = &repository[format!("t{number}")];
        let resolved = match node["__typename"].as_str() {
            Some("Issue") => Resolved::Issue,
            Some("PullRequest") => {
                let Some(refs) = parse_refs_field(&node["closingIssuesReferences"]) else {
                    continue;
                };
                Resolved::PullRequest {
                    refs,
                    head_branch: node["headRefName"].as_str().map(str::to_owned),
                }
            }
            _ => continue,
        };
        out.insert(*number, resolved);
    }
    out
}

/// `gh api graphql` with a wall-clock bound: a hung `gh` must not hold the
/// role's run guard. On timeout the worker thread is abandoned (it exits
/// when `gh` does) and the lookup reads as a transport failure.
struct BoundedGh;

impl GithubApi for BoundedGh {
    fn get(&self, path: &str, _etag: Option<&str>) -> Result<ApiResponse, ApiError> {
        Err(ApiError::Transport(format!("role-tick story join never GETs {path}")))
    }

    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError> {
        self.get(path, None)
    }

    fn graphql(&self, query: &str) -> Result<ApiResponse, ApiError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let query = query.to_owned();
        std::thread::spawn(move || {
            let _ = tx.send(crate::ci_telemetry::api::GhCliApi::from_env().graphql(&query));
        });
        rx.recv_timeout(RESOLVE_TIMEOUT).unwrap_or_else(|_| {
            Err(ApiError::Transport(format!(
                "gh api graphql exceeded {}s",
                RESOLVE_TIMEOUT.as_secs()
            )))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
