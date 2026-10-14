//! The work finder's landed pre-filter (Issue #11853).
//!
//! # The problem
//!
//! An issue whose closing PR merged can stay open: the PR merged into a
//! branch other than the default, or someone reopened the issue. Nothing
//! stopped it being dispatched again. The open-PR guard and its pre-filter
//! only see *open* PRs, and the #11429 journal check only refuses on the host
//! that landed the work, for 30 minutes. One issue drew 54 dispatch attempts
//! in a day.
//!
//! # What this does
//!
//! At the dispatch point, right after the open-PR pre-filter
//! ([`super::open_pr_prefilter`]), the first candidate of a workspace to
//! arrive there asks once for every candidate of that workspace without an
//! open linked PR: has a PR that **closes** it merged while it stays open?
//! A candidate that has:
//!
//! - is resolved `AlreadyLanded` (detail `merged closing PR #P`) with no
//!   `dispatch()` attempt;
//! - gets a fleet-wide landed cooldown of [`LANDED_COOLDOWN`], broadcast like
//!   the #4485 dispatch backoff, so no other host dispatches it either; and
//! - is routed to Curator keep-or-close instead of a Builder: `loom:issue` is
//!   swapped for `loom:triage`, with one comment naming the merged PR and
//!   carrying [`route_marker`]. Curator's queue is `loom:triage`; its
//!   admission step (`curator.md`, "Admission: keep or close") closes the
//!   issue if the PR finished it, or keeps it.
//!
//! # What "landed" means
//!
//! A PR in the issue's GitHub closes-graph (`closedByPullRequestsReferences`,
//! closed PRs included: a closing keyword such as `Closes #N`, or a
//! Development-sidebar link) is `MERGED`, and merged after the issue was
//! last reopened. A `Part of #N` / `Refs #N` PR is not in the closes-graph,
//! so its merge never counts: that work is unfinished, and the issue keeps
//! its stage (the no-op cooldown handles a dispatch that finds nothing to
//! do). A merge older than the issue's last reopen is a person asking for
//! more work, and does not count either.
//!
//! # At most once
//!
//! Before routing, the issue's comments are read for [`route_marker`] naming
//! the same PR. If it is there, Curator already saw this landing and put the
//! issue back in the build queue: the verdict is *kept*, remembered for the
//! life of the process, and the issue is dispatched as normal. The label
//! swap also takes the issue out of every host's candidate listing. A
//! `loom:frozen`, park-labelled or claimed issue is never relabelled; it
//! still gets the cooldown.
//!
//! # Cost
//!
//! One aliased GraphQL query (≈1 point) per workspace per tick, and only on
//! a tick with a candidate whose verdict is unknown or older than
//! [`LANDED_RECHECK`]. A verdict is dropped as soon as the candidate shows an
//! open PR, so the tick its PR merges, the issue is asked about again. The
//! comment read and label writes happen only for a landed issue. Nothing is
//! asked while the rate-limit breaker is open.
//!
//! # Failure
//!
//! Fail open. An unreadable answer, an issue alias the forge did not answer,
//! or a truncated closes-graph is no verdict: the candidate goes to
//! `dispatch()` as before. Only a verified merged closing PR removes it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::claim_reconciliation::gh_call;
use crate::comment_trust::{records, TrustPolicy};
use crate::sweep_registry::{SweepRegistry, LANDED_COOLDOWN};

/// The conditional-read caller / breaker name of this pre-filter.
const CALLER: &str = "work_finder.landed_prefilter";

/// How long a "not landed" verdict is trusted before it is asked again.
pub const LANDED_RECHECK: Duration = Duration::from_secs(30 * 60);

/// Most issues asked about in one query (one query per workspace per tick).
pub const LANDED_BATCH: usize = 100;

/// `first:` on each closes-graph: a truncated graph is no verdict.
const CLOSING_FIRST: usize = 20;

/// Deadline of the closes-graph query.
const GRAPHQL_TIMEOUT: Duration = Duration::from_secs(60);

/// The hidden marker the routing comment carries, naming the merged PR.
#[must_use]
pub fn route_marker(pr: u32) -> String {
    format!("<!-- loom:landed-route pr={pr} -->")
}

/// The routing comment for issue work landed by `pr`.
#[must_use]
pub fn route_comment(pr: u32) -> String {
    format!(
        "PR #{pr}, which closes this issue, has merged, but the issue is still open. It goes \
         back to Curator (`loom:triage`) instead of to a Builder: close it if #{pr} finished \
         the work, or curate it again if work remains. This is done once per merged PR \
         (#11853).\n\n{}",
        route_marker(pr)
    )
}

// ---------------------------------------------------------------------------
// The closes-graph: query and verdict
// ---------------------------------------------------------------------------

/// One aliased query: each issue's state, its last reopen, and its
/// closes-graph with closed (and so merged) PRs included.
#[must_use]
pub fn landed_query(owner: &str, name: &str, issues: &[u32]) -> String {
    let fields: String = issues
        .iter()
        .map(|n| {
            format!(
                " i{n}: issue(number: {n}) {{ state timelineItems(itemTypes: [REOPENED_EVENT], \
                 last: 1) {{ nodes {{ ... on ReopenedEvent {{ createdAt }} }} }} \
                 closedByPullRequestsReferences(first: {CLOSING_FIRST}, includeClosedPrs: true) \
                 {{ totalCount nodes {{ number state mergedAt }} }} }}"
            )
        })
        .collect();
    format!("query {{ repository(owner: \"{owner}\", name: \"{name}\") {{{fields} }} }}")
}

fn ts(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// The verdict for one issue's alias: `Some(Some(pr))` landed by `pr` (the
/// latest qualifying merge), `Some(None)` verified not landed, `None` no
/// verdict (alias missing, malformed, or a truncated graph with no merge in
/// it).
#[must_use]
pub fn alias_verdict(issue: &Value) -> Option<Option<u32>> {
    let state = issue.get("state")?.as_str()?;
    if !state.eq_ignore_ascii_case("open") {
        // A closed issue is the 2.5 guard's to refuse, not this pre-filter's.
        return Some(None);
    }
    let reopened = issue["timelineItems"]["nodes"]
        .as_array()
        .and_then(|n| n.iter().filter_map(|e| ts(&e["createdAt"])).max());
    let graph = issue.get("closedByPullRequestsReferences")?;
    let nodes = graph.get("nodes")?.as_array()?;
    let landed = nodes
        .iter()
        .filter(|pr| pr["state"].as_str() == Some("MERGED"))
        .filter_map(|pr| {
            let number = u32::try_from(pr["number"].as_u64()?).ok()?;
            let merged = ts(&pr["mergedAt"]);
            // A merge older than the last reopen is not this issue's landing.
            let after_reopen = match (reopened, merged) {
                (Some(r), Some(m)) => m > r,
                (Some(_), None) => false,
                (None, _) => true,
            };
            after_reopen.then_some((merged, number))
        })
        .max()
        .map(|(_, number)| number);
    if landed.is_some() {
        return Some(landed);
    }
    let total = graph.get("totalCount")?.as_u64()?;
    (usize::try_from(total).ok() == Some(nodes.len())).then_some(None)
}

/// Parse a [`landed_query`] answer: the verdict of each issue that has one.
/// `None` when the answer carries no `data.repository` at all.
#[must_use]
pub fn parse_landed(body: &str, issues: &[u32]) -> Option<HashMap<u32, Option<u32>>> {
    let json: Value = serde_json::from_str(body.trim()).ok()?;
    let repository = json.get("data")?.get("repository")?;
    if !repository.is_object() {
        return None;
    }
    Some(
        issues
            .iter()
            .filter_map(|n| Some((*n, alias_verdict(repository.get(format!("i{n}"))?)?)))
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Routing to Curator
// ---------------------------------------------------------------------------

/// The forge calls of this pre-filter, injected so it is testable offline.
pub trait LandedForge {
    /// [`parse_landed`] over the closes-graph of `issues`, or `None` when
    /// the query could not be made or answered.
    fn closing_verdicts(&mut self, issues: &[u32]) -> Option<HashMap<u32, Option<u32>>>;

    /// Issue `issue`'s labels now, `None` when it is no longer an open issue.
    ///
    /// # Errors
    /// The read failed: nothing is relabelled on a guess.
    fn labels(&mut self, issue: u32) -> Result<Option<Vec<String>>>;

    /// Whether a comment on `issue` already carries [`route_marker`]`(pr)`.
    ///
    /// # Errors
    /// The read failed: nothing is relabelled on a guess.
    fn route_marked(&mut self, issue: u32, pr: u32) -> Result<bool>;

    /// Add `label` to `issue`.
    fn add_label(&mut self, issue: u32, label: &str) -> bool;

    /// Remove `label` from `issue`; already absent is success.
    fn remove_label(&mut self, issue: u32, label: &str) -> bool;

    /// Post `body` as a comment on `issue`.
    fn comment(&mut self, issue: u32, body: &str) -> bool;
}

/// What [`route_to_curator`] did with a landed issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `loom:issue` swapped for `loom:triage`, comment posted.
    Routed,
    /// Curator already saw this landing and kept the issue: not landed.
    Kept,
    /// Left as it is (a veto label, no `loom:issue`, a failed read or write);
    /// the cooldown still applies.
    Unrouted(&'static str),
}

const ISSUE_LABEL: &str = "loom:issue";
const TRIAGE_LABEL: &str = crate::intake_reconcile::TRIAGE_LABEL;

/// Route issue `issue`, landed by `pr`, to Curator keep-or-close.
pub fn route_to_curator<F: LandedForge + ?Sized>(forge: &mut F, issue: u32, pr: u32) -> Route {
    match forge.route_marked(issue, pr) {
        Ok(true) => return Route::Kept,
        Ok(false) => {}
        Err(_) => return Route::Unrouted("comment read failed"),
    }
    let labels = match forge.labels(issue) {
        Ok(Some(labels)) => labels,
        Ok(None) => return Route::Unrouted("not an open issue"),
        Err(_) => return Route::Unrouted("label read failed"),
    };
    if !labels.iter().any(|l| l == ISSUE_LABEL) {
        return Route::Unrouted("no loom:issue label");
    }
    if labels.iter().any(|l| l == super::BUILDING_LABEL)
        || super::restore_veto(&labels, &[]).is_some()
    {
        return Route::Unrouted("frozen, parked or claimed");
    }
    // Labels first, so a failure never leaves a comment with no routing.
    if !forge.add_label(issue, TRIAGE_LABEL) {
        return Route::Unrouted("adding loom:triage failed");
    }
    if !forge.remove_label(issue, ISSUE_LABEL) {
        return Route::Unrouted("removing loom:issue failed");
    }
    if !forge.comment(issue, &route_comment(pr)) {
        log::warn!("work_finder: routed issue #{issue} to Curator, but its comment failed");
    }
    Route::Routed
}

// ---------------------------------------------------------------------------
// The per-workspace verdict cache and the detection step
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    NotLanded,
    Landed(u32),
    /// Curator kept the issue after this landing: never asked again.
    Kept(u32),
}

/// One workspace's verdicts, with when each was read.
#[derive(Debug, Default)]
pub struct LandedCache {
    verdicts: HashMap<u32, (Instant, Verdict)>,
}

impl LandedCache {
    /// The candidates to ask about at `now`: no verdict, or one older than
    /// [`LANDED_RECHECK`], at most [`LANDED_BATCH`]. Forgets the verdict of
    /// every issue in `open_pr`, so the tick its PR merges asks again.
    fn due(&mut self, candidates: &[u32], open_pr: &[u32], now: Instant) -> Vec<u32> {
        for issue in open_pr {
            if !matches!(self.verdicts.get(issue), Some((_, Verdict::Kept(_)))) {
                self.verdicts.remove(issue);
            }
        }
        candidates
            .iter()
            .copied()
            .filter(|n| match self.verdicts.get(n) {
                None => true,
                Some((_, Verdict::Kept(_))) => false,
                Some((at, _)) => now.saturating_duration_since(*at) >= LANDED_RECHECK,
            })
            .take(LANDED_BATCH)
            .collect()
    }
}

/// Every candidate in `candidates` verified landed, mapped to its merged
/// closing PR, routing each newly landed one to Curator. `open_pr` are the
/// workspace's candidates with an open linked PR this tick.
pub fn detect<F: LandedForge + ?Sized>(
    forge: &mut F,
    cache: &mut LandedCache,
    candidates: &[u32],
    open_pr: &[u32],
    now: Instant,
) -> HashMap<u32, u32> {
    let ask = cache.due(candidates, open_pr, now);
    let mut fresh: Vec<(u32, u32)> = Vec::new();
    if !ask.is_empty() {
        for (issue, verdict) in forge.closing_verdicts(&ask).unwrap_or_default() {
            let verdict = match verdict {
                None => Verdict::NotLanded,
                Some(pr) => {
                    fresh.push((issue, pr));
                    Verdict::Landed(pr)
                }
            };
            cache.verdicts.insert(issue, (now, verdict));
        }
    }
    fresh.sort_unstable();
    for (issue, pr) in fresh {
        let route = route_to_curator(forge, issue, pr);
        log::info!(
            "work_finder: issue #{issue} is still open but its closing PR #{pr} has merged; \
             not dispatching it ({route:?}, #11853)"
        );
        if route == Route::Kept {
            cache.verdicts.insert(issue, (now, Verdict::Kept(pr)));
        }
    }
    candidates
        .iter()
        .filter_map(|n| match cache.verdicts.get(n) {
            Some((_, Verdict::Landed(pr))) => Some((*n, *pr)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Production glue
// ---------------------------------------------------------------------------

fn caches() -> &'static Mutex<HashMap<PathBuf, LandedCache>> {
    static CACHES: OnceLock<Mutex<HashMap<PathBuf, LandedCache>>> = OnceLock::new();
    CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// [`super::WorkDispatcher::landed_closing_prs`] for the production
/// dispatcher: [`detect`] against the live forge, then the landed cooldown
/// armed for each landed candidate. Empty whenever nothing can be verified.
/// The registry lock is never held across a forge call.
#[must_use]
pub fn landed_in(
    registry: &Arc<Mutex<SweepRegistry>>,
    candidates: &[u32],
    open_pr: &[u32],
) -> HashMap<u32, u32> {
    if candidates.is_empty() {
        return HashMap::new();
    }
    let target = registry
        .lock()
        .ok()
        .and_then(|reg| reg.open_pr_listing_target());
    let Some((gh_bin, root, slug)) = target else {
        return HashMap::new();
    };
    let mut forge = GhLandedForge {
        gh_bin,
        root: root.clone(),
        slug,
    };
    let landed = {
        let mut all = caches()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache = all.entry(root).or_default();
        detect(&mut forge, cache, candidates, open_pr, Instant::now())
    };
    if !landed.is_empty() {
        if let Ok(mut reg) = registry.lock() {
            for issue in landed.keys() {
                reg.arm_landed_cooldown(*issue, LANDED_COOLDOWN);
            }
        }
    }
    landed
}

/// The live forge: `gh` against `slug` from the workspace `root`.
struct GhLandedForge {
    gh_bin: PathBuf,
    root: PathBuf,
    slug: String,
}

impl GhLandedForge {
    fn issue_path(&self, issue: u32) -> String {
        format!("repos/{}/issues/{issue}", self.slug)
    }

    fn trusted_route_marked(&self, issue: u32, pr: u32) -> Result<bool> {
        // The marker decides whether Curator already saw this landing, so only
        // a trusted author's comment may carry it (#9548): read the whole
        // listing, not a `--jq` projection, and filter it through the policy.
        let url = format!("{}/comments?per_page=100", self.issue_path(issue));
        let read = gh_call::read("landed.route_marker", &self.gh_bin, &self.root);
        let out = self.run(read.args(["api", "--paginate", url.as_str()]))?;
        let trusted = TrustPolicy::for_root(&self.root)
            .trusted_listing(out.as_bytes())
            .ok_or_else(|| anyhow!("the comment listing did not parse"))?;
        let marker = route_marker(pr);
        Ok(records::bodies(&trusted)
            .iter()
            .any(|body| body.contains(&marker)))
    }

    fn run(&self, inv: crate::gh_invocation::GhInvocation) -> Result<String> {
        let out = gh_call::output(inv)?;
        if !out.status.success() {
            return Err(anyhow!("gh failed: {}", gh_call::stderr(&out)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// `owner` and `name` of `slug`, each safe inside a GraphQL string literal.
fn owner_name(slug: &str) -> Option<(&str, &str)> {
    let safe = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    slug.split_once('/').filter(|(o, n)| safe(o) && safe(n))
}

impl LandedForge for GhLandedForge {
    fn closing_verdicts(&mut self, issues: &[u32]) -> Option<HashMap<u32, Option<u32>>> {
        use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
        if crate::rate_limit_breaker::global_skip_pass(CALLER) {
            return None;
        }
        let (owner, name) = owner_name(&self.slug)?;
        let field = format!("query={}", landed_query(owner, name, issues));
        let inv = GhInvocation::new(
            Operation::new("work_finder.landed_closing_refs"),
            AccessIntent::Read,
            GhTarget::None,
            GRAPHQL_TIMEOUT,
        )
        .forge_op(crate::forge_call_stats::ops::ISSUE_CLOSED_BY_PULL_REQUESTS)
        .identity_scope(None, Some(&self.slug))
        .program(&self.gh_bin)
        .current_dir(&self.root)
        .args(["api", "graphql", "-f", field.as_str()]);
        // `gh` exits non-zero when any alias errored but still prints the
        // partial `data`: parse first, judge per alias.
        let out = gh_call::output(inv).ok()?;
        let parsed = parse_landed(&String::from_utf8_lossy(&out.stdout), issues);
        if parsed.is_none() {
            let stderr = gh_call::stderr(&out);
            crate::rate_limit_breaker::global_observe_failure(&stderr, CALLER);
            log::debug!("work_finder: landed closes-graph query unanswered ({stderr}); no verdict");
        }
        parsed
    }

    fn labels(&mut self, issue: u32) -> Result<Option<Vec<String>>> {
        let path = self.issue_path(issue);
        let body = self.run(
            gh_call::read("landed.issue_labels", &self.gh_bin, &self.root)
                .args(["api", path.as_str()]),
        )?;
        let v: Value = serde_json::from_str(body.trim()).context("parse the issue")?;
        let is_pr = v.get("pull_request").is_some_and(|p| !p.is_null());
        if is_pr || v["state"].as_str() != Some("open") {
            return Ok(None);
        }
        let labels = v["labels"]
            .as_array()
            .ok_or_else(|| anyhow!("the issue carries no labels array"))?;
        Ok(Some(
            labels
                .iter()
                .filter_map(|l| l["name"].as_str().map(str::to_owned))
                .collect(),
        ))
    }

    fn route_marked(&mut self, issue: u32, pr: u32) -> Result<bool> {
        self.trusted_route_marked(issue, pr)
    }

    fn add_label(&mut self, issue: u32, label: &str) -> bool {
        let path = format!("{}/labels", self.issue_path(issue));
        let field = format!("labels[]={label}");
        let w = gh_call::write("landed.add_triage", &self.gh_bin, &self.root);
        self.run(w.args(["api", "-X", "POST", path.as_str(), "-f", field.as_str()]))
            .is_ok()
    }

    fn remove_label(&mut self, issue: u32, label: &str) -> bool {
        let path = format!("{}/labels/{}", self.issue_path(issue), label.replace(':', "%3A"));
        let w = gh_call::write("landed.remove_issue", &self.gh_bin, &self.root);
        match self.run(w.args(["api", "-X", "DELETE", path.as_str()])) {
            Ok(_) => true,
            Err(e) => e.to_string().contains("404"),
        }
    }

    fn comment(&mut self, issue: u32, body: &str) -> bool {
        let path = format!("{}/comments", self.issue_path(issue));
        let field = format!("body={body}");
        let w = gh_call::write("landed.comment", &self.gh_bin, &self.root);
        self.run(w.args(["api", "-X", "POST", path.as_str(), "-f", field.as_str()]))
            .is_ok()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "landed_prefilter_tests.rs"]
mod tests;
