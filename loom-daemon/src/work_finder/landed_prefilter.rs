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
//! Development-sidebar link) is `MERGED`, merged after the issue was last
//! reopened, and belongs to the issue's own repository. A `Part of #N` /
//! `Refs #N` PR is not in the closes-graph, so its merge never counts: that
//! work is unfinished, and the issue keeps its stage (the no-op cooldown
//! handles a dispatch that finds nothing to do). A merge older than the
//! issue's last reopen is a person asking for more work, and does not count
//! either. Nor does a PR merged in another repository whose body says
//! `Closes owner/repo#N` (#11899): it is in the closes-graph, but anyone can
//! write that line in a repo of their own.
//!
//! # At most once
//!
//! Before routing, the issue's comments are read for [`route_marker`] naming
//! the same PR. If it is there, Curator already saw this landing and put the
//! issue back in the build queue: the verdict is *kept*, and the issue is
//! dispatched as normal. The label swap also takes the issue out of every
//! host's candidate listing. A `loom:frozen`, park-labelled or claimed issue
//! is never relabelled; it still gets the cooldown.
//!
//! The host also remembers which landing it handed to Curator for each issue
//! (routed, or found kept), so an answer naming the same PR again is *kept*
//! without a comment read. That record is what stops a route whose comment
//! write failed from routing again every cooldown (#11901). A kept verdict is
//! re-asked like any other, so a second closing PR that merges while the
//! issue stays open is a new landing and is routed in its turn (#11901).
//!
//! # Lifting the cooldown
//!
//! A landed candidate under this host's landed cooldown that is listed again
//! (Curator put it back, or someone relabelled it) is asked about at once
//! when this host routed it, or after [`LANDED_RECHECK`] otherwise. If it was
//! reopened after the merge, or the answer is *kept*, the cooldown is lifted
//! here and on every peer and the issue dispatches this tick, rather than
//! after six hours ([`lift_in`], #11901).
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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::claim_reconciliation::gh_call;
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
/// closes-graph with closed (and so merged) PRs included, each PR naming its
/// repository.
#[must_use]
pub fn landed_query(owner: &str, name: &str, issues: &[u32]) -> String {
    let fields: String = issues
        .iter()
        .map(|n| {
            format!(
                " i{n}: issue(number: {n}) {{ state timelineItems(itemTypes: [REOPENED_EVENT], \
                 last: 1) {{ nodes {{ ... on ReopenedEvent {{ createdAt }} }} }} \
                 closedByPullRequestsReferences(first: {CLOSING_FIRST}, includeClosedPrs: true) \
                 {{ totalCount nodes {{ number state mergedAt repository {{ nameWithOwner }} }} }} }}"
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

/// The verdict for one issue's alias in repository `repo` (`owner/name`):
/// `Some(Some(pr))` landed by `pr` (the latest qualifying merge), `Some(None)`
/// verified not landed, `None` no verdict (alias missing, malformed, or a
/// truncated graph with no merge in it). Only a PR of `repo` itself
/// qualifies (#11899): a cross-repo `Closes owner/repo#N` is anyone's to
/// write, and a PR naming no repository is not trusted to be this one.
#[must_use]
pub fn alias_verdict(issue: &Value, repo: &str) -> Option<Option<u32>> {
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
        .filter(|pr| {
            pr["repository"]["nameWithOwner"]
                .as_str()
                .is_some_and(|r| r.eq_ignore_ascii_case(repo))
        })
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

/// Parse a [`landed_query`] answer for repository `repo` (`owner/name`): the
/// verdict of each issue that has one. `None` when the answer carries no
/// `data.repository` at all.
#[must_use]
pub fn parse_landed(body: &str, issues: &[u32], repo: &str) -> Option<HashMap<u32, Option<u32>>> {
    let json: Value = serde_json::from_str(body.trim()).ok()?;
    let repository = json.get("data")?.get("repository")?;
    if !repository.is_object() {
        return None;
    }
    Some(
        issues
            .iter()
            .filter_map(|n| Some((*n, alias_verdict(repository.get(format!("i{n}"))?, repo)?)))
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
    /// The labels were swapped but the comment failed, so no marker is on
    /// the forge: only this host's own record of the route stops it being
    /// routed again (#11901).
    RoutedUnmarked,
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
        log::warn!(
            "work_finder: routed issue #{issue} to Curator, but its comment failed; this host \
             records the route itself (#11901)"
        );
        return Route::RoutedUnmarked;
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
    /// Curator already saw this landing and kept the issue: dispatchable.
    /// Re-asked like any verdict, so a later landing is seen (#11901).
    Kept(u32),
}

/// One workspace's verdicts, with when each was read.
#[derive(Debug, Default)]
pub struct LandedCache {
    verdicts: HashMap<u32, (Instant, Verdict)>,
    /// The landing this host handed to Curator for each issue: routed here,
    /// or found kept. A later answer naming the same PR is kept without a
    /// comment read, which also covers a route whose comment failed (#11901).
    handed: HashMap<u32, u32>,
    /// Issues this host routed whose return to the queue has not been asked
    /// about yet: [`lift`] asks at once, not after [`LANDED_RECHECK`].
    routed: HashSet<u32>,
}

impl LandedCache {
    fn stale(&self, issue: u32, now: Instant) -> bool {
        self.verdicts
            .get(&issue)
            .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= LANDED_RECHECK)
    }

    /// The candidates to ask about at `now`: no verdict, or one older than
    /// [`LANDED_RECHECK`], at most [`LANDED_BATCH`]. Forgets the verdict of
    /// every issue in `open_pr`, so the tick its PR merges asks again.
    fn due(&mut self, candidates: &[u32], open_pr: &[u32], now: Instant) -> Vec<u32> {
        for issue in open_pr {
            self.verdicts.remove(issue);
        }
        candidates
            .iter()
            .copied()
            .filter(|n| self.stale(*n, now))
            .take(LANDED_BATCH)
            .collect()
    }

    fn landed(&self, issue: u32) -> Option<u32> {
        match self.verdicts.get(&issue) {
            Some((_, Verdict::Landed(pr))) => Some(*pr),
            _ => None,
        }
    }
}

/// Ask about `ask` and record each answer, routing every new landing to
/// Curator. Returns the issues the forge answered for.
fn settle<F: LandedForge + ?Sized>(
    forge: &mut F,
    cache: &mut LandedCache,
    ask: &[u32],
    now: Instant,
) -> Vec<u32> {
    if ask.is_empty() {
        return Vec::new();
    }
    let mut answered = Vec::new();
    let mut fresh: Vec<(u32, u32)> = Vec::new();
    for (issue, verdict) in forge.closing_verdicts(ask).unwrap_or_default() {
        answered.push(issue);
        let verdict = match verdict {
            None => Verdict::NotLanded,
            Some(pr) if cache.handed.get(&issue) == Some(&pr) => Verdict::Kept(pr),
            Some(pr) => {
                fresh.push((issue, pr));
                Verdict::Landed(pr)
            }
        };
        cache.verdicts.insert(issue, (now, verdict));
    }
    fresh.sort_unstable();
    for (issue, pr) in fresh {
        let route = route_to_curator(forge, issue, pr);
        log::info!(
            "work_finder: issue #{issue} is still open but its closing PR #{pr} has merged; \
             not dispatching it ({route:?}, #11853)"
        );
        match route {
            Route::Kept => {
                cache.handed.insert(issue, pr);
                cache.verdicts.insert(issue, (now, Verdict::Kept(pr)));
            }
            Route::Routed | Route::RoutedUnmarked => {
                cache.handed.insert(issue, pr);
                cache.routed.insert(issue);
            }
            Route::Unrouted(_) => {}
        }
    }
    answered.sort_unstable();
    answered
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
    settle(forge, cache, &ask, now);
    candidates
        .iter()
        .filter_map(|n| cache.landed(*n).map(|pr| (*n, pr)))
        .collect()
}

/// Of `held`, listed candidates under this host's landed cooldown, the ones
/// whose cooldown has nothing left to protect (#11901): reopened after the
/// merge, or kept by Curator. An issue this host routed is asked about the
/// first time it is listed again; any other once its verdict is
/// [`LANDED_RECHECK`] old. A new landing is routed like any other.
pub fn lift<F: LandedForge + ?Sized>(
    forge: &mut F,
    cache: &mut LandedCache,
    held: &[u32],
    now: Instant,
) -> Vec<u32> {
    let ask: Vec<u32> = held
        .iter()
        .copied()
        .filter(|n| cache.routed.contains(n) || cache.stale(*n, now))
        .take(LANDED_BATCH)
        .collect();
    let answered = settle(forge, cache, &ask, now);
    for n in &answered {
        cache.routed.remove(n);
    }
    answered
        .into_iter()
        .filter(|n| {
            matches!(cache.verdicts.get(n), Some((_, Verdict::NotLanded | Verdict::Kept(_))))
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

/// [`super::WorkDispatcher::lift_landed_cooldowns`] for the production
/// dispatcher (#11901): [`lift`] over the `held` candidates under this host's
/// own landed cooldown, then each lifted one's cooldown cleared here and on
/// every peer. Empty whenever nothing can be verified. The registry lock is
/// never held across a forge call.
#[must_use]
pub fn lift_in(registry: &Arc<Mutex<SweepRegistry>>, held: &[u32]) -> HashSet<u32> {
    let (mine, target) = match registry.lock() {
        Ok(reg) => {
            let landed = reg.landed_cooldown_issues();
            let mine: Vec<u32> = held
                .iter()
                .copied()
                .filter(|n| landed.contains(n))
                .collect();
            (mine, reg.open_pr_listing_target())
        }
        Err(_) => return HashSet::new(),
    };
    let Some((gh_bin, root, slug)) = target.filter(|_| !mine.is_empty()) else {
        return HashSet::new();
    };
    let mut forge = GhLandedForge {
        gh_bin,
        root: root.clone(),
        slug,
    };
    let lifted = {
        let mut all = caches()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache = all.entry(root).or_default();
        lift(&mut forge, cache, &mine, Instant::now())
    };
    if !lifted.is_empty() {
        if let Ok(mut reg) = registry.lock() {
            for issue in &lifted {
                reg.clear_landed_cooldown(*issue);
            }
        }
    }
    lifted.into_iter().collect()
}

/// The live forge: `gh` against `slug` from the workspace `root`.
struct GhLandedForge {
    gh_bin: PathBuf,
    root: PathBuf,
    slug: String,
}

impl GhLandedForge {
    /// The #9548 write gate: every label and comment write of this
    /// pre-filter targets `slug`, vetted from `root` (cached by write_scope).
    fn may_write(&self) -> bool {
        crate::write_scope::gate_repo_with(
            &self.root,
            &self.slug,
            &self.gh_bin,
            "landed routing to Curator (#11853)",
        )
    }

    fn issue_path(&self, issue: u32) -> String {
        format!("repos/{}/issues/{issue}", self.slug)
    }

    fn run(&self, inv: crate::gh_invocation::GhInvocation) -> Result<String> {
        let out = gh_call::output(inv)?;
        if !out.status.success() {
            return Err(anyhow!("gh failed: {}", gh_call::stderr(&out)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Whether a TRUSTED comment on `issue` carries [`route_marker`]`(pr)`. Only
/// a trusted comment counts (#9548): an outsider's forged marker must not
/// suppress routing.
fn read_route_marked(forge: &GhLandedForge, issue: u32, pr: u32) -> Result<bool> {
    let url = format!("{}/comments?per_page=100", forge.issue_path(issue));
    let read = gh_call::read("landed.route_marker", &forge.gh_bin, &forge.root);
    let listing = forge.run(read.args(["api", "--paginate", url.as_str()]))?;
    let bodies = crate::comment_trust::TrustPolicy::for_root(&forge.root)
        .trusted_bodies(listing.as_bytes())
        .ok_or_else(|| anyhow!("unparseable comment listing for issue #{issue}"))?;
    let marker = route_marker(pr);
    Ok(bodies.iter().any(|b| b.contains(&marker)))
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
        let parsed = parse_landed(&String::from_utf8_lossy(&out.stdout), issues, &self.slug);
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
        read_route_marked(self, issue, pr)
    }

    fn add_label(&mut self, issue: u32, label: &str) -> bool {
        if !self.may_write() {
            return false;
        }
        let path = format!("{}/labels", self.issue_path(issue));
        let field = format!("labels[]={label}");
        let w = gh_call::write("landed.add_triage", &self.gh_bin, &self.root);
        self.run(w.args(["api", "-X", "POST", path.as_str(), "-f", field.as_str()]))
            .is_ok()
    }

    fn remove_label(&mut self, issue: u32, label: &str) -> bool {
        if !self.may_write() {
            return false;
        }
        let path = format!("{}/labels/{}", self.issue_path(issue), label.replace(':', "%3A"));
        let w = gh_call::write("landed.remove_issue", &self.gh_bin, &self.root);
        match self.run(w.args(["api", "-X", "DELETE", path.as_str()])) {
            Ok(_) => true,
            Err(e) => e.to_string().contains("404"),
        }
    }

    fn comment(&mut self, issue: u32, body: &str) -> bool {
        if !self.may_write() {
            return false;
        }
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
