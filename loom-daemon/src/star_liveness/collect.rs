//! Gather one repo's facts and classify every starred issue and every
//! blocker that inherits a star. Generic over [`StarForge`], so the replay
//! fixture drives exactly this code.
//!
//! # Reads per pass (only for a repo with at least one starred issue)
//!
//! - the ETag-cached listing of each level's operator label
//!   (`loom:operator-priority`, `loom:operator-high-priority`, #10307; issues
//!   **and** PRs: Builder copies the star onto its PR, slice B);
//! - the ETag-cached listings for [`PR_LABELS`], to find each starred
//!   issue's open PR by its closing / `Part of` reference;
//! - the comments of a starred issue's **approved** PR, only when that PR's
//!   `updated_at` moved since the last read (merge-refusal detection);
//! - for a detected refusal, the incident lookup: single-issue reads of the
//!   issues the refusal comment names, and, when none is an open issue and
//!   the refusal quotes one of the specific forge phrases, one issue search
//!   for that phrase (repeated each pass only while no open incident is
//!   known; hits must be trusted-authored and quote it word-bounded);
//! - one single-issue read per same-repo blocker of a starred issue (the
//!   blocker's state, and its labels when it inherits a star);
//! - the comments of a `loom:blocked` starred issue whose body names no open
//!   blocker (#10151): blockers named there, and the stale-block markers;
//! - the inheritance walk: one single-issue read per same-repo child not
//!   already read this pass (closed children and PRs included), plus the
//!   reads of each evaluated child's own same-repo blockers. Together these
//!   are at most [`MAX_WALK_READS_PER_PASS`] per repo per pass, counted as
//!   forge reads (cache misses), not as open children found.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;

use super::edges::{self, Edge, EdgeSource, Node, Root};
use super::forge::StarForge;
use super::inherited_star::RootState;
use super::landing::{
    classify, BlockerRef, Capacity, ItemFacts, Landing, MergeRefusal, PrFacts, StarFacts,
    BLOCKED_LABEL,
};
use super::materialize::{self, Classified, OwnerCache, Plan};
use super::progress::{fingerprint, short_hash};
use super::refusal::{self, Detected};
use super::stale::{self, CommentFacts};
use crate::forge_listing::RestIssue;
use crate::types::{AskKind, CapView, QueueDisposition, ReadyQueueRow};
use crate::work_finder::{WorkItem, OPERATOR_PRIORITY_LABEL};

/// PR labels listed to find a starred issue's open PR.
pub const PR_LABELS: &[&str] = &[
    "loom:review-requested",
    "loom:pr",
    "loom:changes-requested",
    "loom:operator",
    "loom:operator-decision",
    "loom:operator-only",
    "loom:merge-conflict",
    "loom:ci-failure",
];

/// How deep inheritance follows a chain of blockers.
pub const MAX_INHERIT_DEPTH: usize = 3;

/// Most single-issue forge reads (cache misses) the inheritance walk makes
/// per repo per pass: every child it reads, open or closed, issue or PR,
/// plus every blocker read made to evaluate an open child. A child whose
/// read, or whose blockers' reads, would exceed it waits for a later pass.
/// The starred issues' own blocker reads are outside the walk and not
/// counted here.
pub const MAX_WALK_READS_PER_PASS: usize = 50;

/// Passes a child found closed (or a PR) is skipped without a read before
/// the walk looks at it again, in case it was reopened.
const SETTLED_TTL_PASSES: u64 = 20;

/// Cached refusal detections, keyed by (repo, PR) and valid while the PR's
/// `updated_at` is unchanged, plus the incident a signature search found
/// (re-checked open every pass).
///
/// Also the inheritance walk's memory across passes, so a capped walk makes
/// progress instead of restarting the same traversal: children it found
/// closed (or PRs), which cost no read for [`SETTLED_TTL_PASSES`] passes,
/// and children the cap deferred, which are resumed first next pass, from
/// the path that reached them, whatever their depth or root.
#[derive(Debug, Default)]
pub struct RefusalCache {
    entries: HashMap<(String, u32), (Option<String>, Option<Detected>)>,
    incidents: HashMap<(String, u32), u32>,
    /// Walk passes started (any repo); the clock for `settled`.
    walk_pass: u64,
    /// Children that inherit nothing, with the pass that found them so.
    settled: HashMap<(String, u32), u64>,
    /// Children the read cap deferred, not yet evaluated, each with the edges
    /// from its starred root down to it.
    deferred: BTreeMap<(String, u32), Vec<Edge>>,
    /// Who owns each starred issue's star (#10012 §3).
    owners: OwnerCache,
}

/// Everything about the repo that is not a forge read.
pub struct RepoContext<'a> {
    /// Forge `owner/repo`.
    pub slug: &'a str,
    /// This host's id.
    pub host: &'a str,
    /// The last work-finder tick's rows for this repo.
    pub tick_rows: &'a [ReadyQueueRow],
    /// The last tick's rows for every repo on this host, in dispatch order:
    /// a starred issue's queue position is host-wide, like the cap (#10214).
    /// Empty falls back to [`Self::tick_rows`].
    pub host_queue: &'a [ReadyQueueRow],
    /// The last tick's cap terms, when recorded (#10214).
    pub cap: Option<CapView>,
    /// This host's pool exhaustion, when any (its description).
    pub pool: Option<String>,
    /// Whether a workspace on this host manages a forge slug (for a
    /// cross-repo blocker).
    pub managed: &'a dyn Fn(&str) -> bool,
    /// A starred-at recorded from a loom-ui intent on this host.
    pub recorded_starred_at: &'a dyn Fn(u32) -> Option<String>,
}

/// One classified row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluated {
    pub facts: StarFacts,
    pub landing: Landing,
    pub starred_at: Option<String>,
    pub inherited_from: Option<u32>,
    pub fingerprint: String,
    /// The issue as a work item (for the inheritance registry).
    pub item: WorkItem,
}

fn item_facts(r: &RestIssue) -> ItemFacts {
    ItemFacts {
        number: r.number,
        labels: r.labels.clone(),
        body: r.body.clone(),
        created_at: r.created_at.clone(),
        updated_at: r.updated_at.clone(),
        open: r.state.eq_ignore_ascii_case("open"),
    }
}

fn work_item(r: &RestIssue) -> WorkItem {
    WorkItem::with_created_at(r.number, r.labels.clone(), r.created_at.clone())
        .with_body(r.body.clone())
        .with_updated_at(r.updated_at.clone())
}

/// Issues each PR declares it closes or contributes to.
fn linked_issues(pr: &RestIssue) -> Vec<u32> {
    let body = pr.body.as_deref().unwrap_or_default();
    let mut refs: BTreeSet<u64> = crate::merge_pr::refs::closing_refs(body)
        .into_iter()
        .collect();
    refs.extend(crate::merge_pr::refs::partial_increment_refs(body));
    refs.into_iter()
        .filter_map(|n| u32::try_from(n).ok())
        .collect()
}

/// What [`Evaluator::visit`] found at a child.
enum Visit {
    /// Closed, a PR, or recently found so: it inherits nothing.
    Settled,
    /// The read cap did not leave room for it.
    Deferred,
    Open(Box<Evaluated>),
}

/// A node the walk will expand: its number, what its landing says inherits,
/// and the edges from its starred root down to it (its depth is their count).
type Pending = (u32, Vec<u32>, Vec<Edge>);

/// The per-pass evaluator for one repo.
pub struct Evaluator<'a> {
    pub forge: &'a mut dyn StarForge,
    pub ctx: RepoContext<'a>,
    pub refusals: &'a mut RefusalCache,
    issues: HashMap<u32, Option<RestIssue>>,
    prs_by_issue: BTreeMap<u32, RestIssue>,
    propagate: bool,
    /// Single-issue forge reads (cache misses) made so far.
    reads: usize,
    /// While set, no read past this count reaches the forge (the walk).
    read_cap: Option<usize>,
    /// The star writes this pass's walk calls for (#10012 §2–§3), made by
    /// the caller ([`materialize::apply`]). Empty with `propagate` off.
    pub plan: Plan,
}

impl<'a> Evaluator<'a> {
    #[must_use]
    pub fn new(
        forge: &'a mut dyn StarForge,
        ctx: RepoContext<'a>,
        refusals: &'a mut RefusalCache,
    ) -> Self {
        Self {
            forge,
            ctx,
            refusals,
            issues: HashMap::new(),
            prs_by_issue: BTreeMap::new(),
            propagate: true,
            reads: 0,
            read_cap: None,
            plan: Plan::default(),
        }
    }

    /// Whether a star also reaches the children [`edges::child_edges`]
    /// resolves (`autonomous.operatorPriority.propagate`), beyond the
    /// liveness blockers. On by default.
    #[must_use]
    pub fn with_propagate(mut self, propagate: bool) -> Self {
        self.propagate = propagate;
        self
    }

    /// The children of `parent`: the issues its landing says inherit
    /// (blockers, refusal incident, red-main fix), plus, with `propagate`,
    /// every child its own text links ([`edges::child_edges`]).
    fn children(&self, parent: u32, landing_inherits: &[u32]) -> Vec<Edge> {
        let mut out: Vec<Edge> = landing_inherits
            .iter()
            .map(|&child| Edge {
                parent,
                child,
                source: EdgeSource::LandingBlocker,
            })
            .collect();
        if self.propagate {
            if let Some(Some(issue)) = self.issues.get(&parent) {
                let node = Node {
                    number: issue.number,
                    title: issue.title.as_deref().unwrap_or_default(),
                    body: issue.body.as_deref().unwrap_or_default(),
                    labels: &issue.labels,
                    is_pull_request: issue.is_pull_request,
                };
                out.extend(edges::child_edges(self.ctx.slug, &node));
            }
        }
        out
    }

    fn tick_row(&self, n: u32) -> Option<&ReadyQueueRow> {
        self.ctx.tick_rows.iter().find(|r| r.issue == n)
    }

    fn issue(&mut self, n: u32) -> Option<RestIssue> {
        if let Some(cached) = self.issues.get(&n) {
            return cached.clone();
        }
        if self.read_cap.is_some_and(|cap| self.reads >= cap) {
            log::debug!(
                "star_liveness: {} reached its read cap this pass; #{n} not read",
                self.ctx.slug
            );
            return None;
        }
        self.reads += 1;
        let read = self.forge.issue(n).unwrap_or_else(|e| {
            log::debug!("star_liveness: reading {}#{n} failed: {e}", self.ctx.slug);
            None
        });
        self.issues.insert(n, read.clone());
        read
    }

    fn open_issue(&mut self, n: u32) -> bool {
        self.issue(n)
            .is_some_and(|i| !i.is_pull_request && i.state.eq_ignore_ascii_case("open"))
    }

    fn detected(&mut self, pr: &RestIssue, issue: u32) -> Option<Detected> {
        let key = (self.ctx.slug.to_string(), pr.number);
        if let Some((at, verdict)) = self.refusals.entries.get(&key) {
            if *at == pr.updated_at {
                return verdict.clone();
            }
        }
        match self.forge.comments(pr.number) {
            Ok(comments) => {
                let me = self.forge.self_login();
                let believed = super::trust::only_trusted(&comments, me.as_deref());
                let verdict = refusal::detect(&believed, pr.number, issue);
                self.refusals
                    .entries
                    .insert(key, (pr.updated_at.clone(), verdict.clone()));
                verdict
            }
            Err(e) => {
                log::debug!(
                    "star_liveness: reading comments of {}#{} failed: {e}",
                    self.ctx.slug,
                    pr.number
                );
                None
            }
        }
    }

    /// The open incident issue tied to `d` (see [`refusal`]): one the
    /// refusal comment names, else the newest trusted-authored one quoting
    /// its specific forge phrase. A generic refusal never searches.
    fn incident(&mut self, d: &Detected, pr: u32, issue: u32) -> Option<u32> {
        if let Some(n) = d.named.clone().into_iter().find(|n| self.open_issue(*n)) {
            return Some(n);
        }
        let key = (self.ctx.slug.to_string(), pr);
        if let Some(n) = self.refusals.incidents.get(&key).copied() {
            if self.open_issue(n) {
                return Some(n);
            }
            self.refusals.incidents.remove(&key);
        }
        let phrase = d.signature()?;
        let found = match self.forge.search_open_issues(phrase) {
            Ok(rows) => rows,
            Err(e) => {
                log::debug!("star_liveness: incident search in {} failed: {e}", self.ctx.slug);
                return None;
            }
        };
        let me = self.forge.self_login();
        let hit = found
            .into_iter()
            .filter(|h| {
                super::trust::trusted_author(
                    h.issue.author.as_deref(),
                    h.author_association.as_deref(),
                    me.as_deref(),
                )
            })
            .map(|h| h.issue)
            .filter(|i| i.number != pr && i.number != issue && !i.is_pull_request)
            .filter(|i| i.state.eq_ignore_ascii_case("open"))
            .filter(|i| {
                let title = i.title.as_deref().unwrap_or_default();
                let body = i.body.as_deref().unwrap_or_default();
                refusal::quotes_phrase(&format!("{title}\n{body}"), phrase)
            })
            .max_by_key(|i| (i.created_at.clone(), i.number))?;
        let n = hit.number;
        self.issues.insert(n, Some(hit));
        self.refusals.incidents.insert(key, n);
        Some(n)
    }

    fn refusal_for(&mut self, pr: &RestIssue, issue: u32) -> Option<MergeRefusal> {
        if !pr.labels.iter().any(|l| l == "loom:pr") {
            return None;
        }
        let d = self.detected(pr, issue)?;
        let incident = self.incident(&d, pr.number, issue);
        Some(MergeRefusal {
            reason: d.class.reason,
            raw: d.raw,
            incident,
        })
    }

    /// The dependency refs a `loom:blocked` issue's body names.
    fn dependency_refs(&self, issue: &RestIssue) -> Vec<String> {
        if !issue.labels.iter().any(|l| l == BLOCKED_LABEL) {
            return Vec::new();
        }
        let body = issue.body.as_deref().unwrap_or_default();
        crate::dep_classify::refs::parse_named_blocker_refs(body, self.ctx.slug)
    }

    /// `r`'s number when it names an issue in this repo.
    fn same_repo_number(&self, r: &str) -> Option<u32> {
        let prefix = format!("{}#", self.ctx.slug.to_ascii_lowercase());
        r.to_ascii_lowercase()
            .strip_prefix(&prefix)
            .and_then(|n| n.parse::<u32>().ok())
    }

    /// Forge reads [`Self::blocked_facts`] would make for `issue` from its
    /// body (its same-repo blockers not read yet this pass).
    fn pending_blocker_reads(&self, issue: &RestIssue) -> usize {
        self.dependency_refs(issue)
            .iter()
            .filter_map(|r| self.same_repo_number(r))
            .filter(|n| *n != issue.number && !self.issues.contains_key(n))
            .collect::<BTreeSet<u32>>()
            .len()
    }

    /// One named ref (`owner/repo#N`) as a [`BlockerRef`], its state read
    /// when it is in this repo.
    fn blocker_ref(&mut self, r: String, issue: u32) -> BlockerRef {
        match self.same_repo_number(&r) {
            Some(n) if n != issue => {
                let open = self.issue(n).map(|b| b.state.eq_ignore_ascii_case("open"));
                BlockerRef {
                    display: format!("#{n}"),
                    number: Some(n),
                    open,
                    cross_repo_managed: None,
                }
            }
            Some(_) => BlockerRef {
                display: r,
                number: None,
                open: Some(false),
                cross_repo_managed: None,
            },
            None => {
                let slug = r.split('#').next().unwrap_or_default().to_string();
                BlockerRef {
                    display: r,
                    number: None,
                    open: None,
                    cross_repo_managed: Some((self.ctx.managed)(&slug)),
                }
            }
        }
    }

    /// The blockers a `loom:blocked` issue names, and what its comments say
    /// about the block (#10151). The body is read first; only when it names
    /// no open blocker are the issue's trusted comments read, both for
    /// blockers named there and for the pass's own stale-block markers.
    fn blocked_facts(&mut self, issue: &RestIssue) -> (Vec<BlockerRef>, Option<CommentFacts>) {
        if !issue.labels.iter().any(|l| l == BLOCKED_LABEL) {
            return (Vec::new(), Some(CommentFacts::default()));
        }
        let mut refs: BTreeSet<String> = self.dependency_refs(issue).into_iter().collect();
        let mut blockers: Vec<BlockerRef> = refs
            .clone()
            .into_iter()
            .map(|r| self.blocker_ref(r, issue.number))
            .collect();
        if blockers.iter().any(|b| b.open != Some(false)) {
            return (blockers, Some(CommentFacts::default()));
        }
        // The comments read counts against the walk's read cap like any other
        // single-issue read; past the cap it is not made, which reads as
        // "comments unread" (no write this pass).
        if self.read_cap.is_some_and(|cap| self.reads >= cap) {
            log::debug!(
                "star_liveness: {} reached its read cap this pass; comments of #{} not read",
                self.ctx.slug,
                issue.number
            );
            return (blockers, None);
        }
        self.reads += 1;
        let facts = match self.forge.comments(issue.number) {
            Ok(comments) => {
                let me = self.forge.self_login();
                stale::comment_facts(&comments, me.as_deref())
            }
            Err(e) => {
                log::debug!(
                    "star_liveness: reading comments of {}#{} failed: {e}",
                    self.ctx.slug,
                    issue.number
                );
                return (blockers, None);
            }
        };
        for text in &facts.bodies {
            for r in crate::dep_classify::refs::parse_named_blocker_refs(text, self.ctx.slug) {
                if refs.insert(r.clone()) {
                    let b = self.blocker_ref(r, issue.number);
                    blockers.push(b);
                }
            }
        }
        (blockers, Some(facts))
    }

    fn capacity(&self, n: u32) -> Capacity {
        if let Some(detail) = &self.ctx.pool {
            return Capacity::PoolExhausted {
                detail: detail.clone(),
            };
        }
        self.tick_row(n)
            .and_then(|row| {
                super::queue::wait(row, self.ctx.host_queue, self.ctx.tick_rows, self.ctx.cap)
            })
            .map_or(Capacity::Available, Capacity::Deferred)
    }

    fn red_main_fix(&self) -> Option<u32> {
        self.ctx
            .tick_rows
            .iter()
            .filter(|r| r.main_red_fix)
            .map(|r| r.issue)
            .min()
    }

    /// The state of `root`, the root an inherited star names: starred when it
    /// is in this pass's starred listing or, closed, still carries an
    /// operator label (labels are never cleaned on close, AC 5).
    fn root_state(&mut self, root: u32, listed: &BTreeSet<u32>) -> RootState {
        if listed.contains(&root) {
            return RootState::Starred;
        }
        match self.issue(root) {
            Some(i)
                if crate::operator_levels::own_level_in(
                    crate::operator_levels::table(),
                    &i.labels,
                ) >= 1 =>
            {
                RootState::Starred
            }
            Some(_) => RootState::Unstarred,
            None => RootState::Unknown,
        }
    }

    /// Whether `issue` names, in its own text, a parent that is still
    /// starred (or unreadable): a child-side link the walk does not follow,
    /// checked before an orphaned star is removed.
    fn has_starred_parent(&mut self, issue: &RestIssue, listed: &BTreeSet<u32>) -> bool {
        let node = Node {
            number: issue.number,
            title: issue.title.as_deref().unwrap_or_default(),
            body: issue.body.as_deref().unwrap_or_default(),
            labels: &issue.labels,
            is_pull_request: issue.is_pull_request,
        };
        let parents: Vec<u32> = edges::parent_edges(self.ctx.slug, &node)
            .into_iter()
            .map(|e| e.parent)
            .collect();
        parents
            .into_iter()
            .any(|p| self.root_state(p, listed) != RootState::Unstarred)
    }

    /// Split the starred listing by owner (#10012 §3). With `propagate` off
    /// every starred issue is a root, as before.
    fn classify(&mut self, issues: &[RestIssue]) -> Classified {
        let listed: BTreeSet<u32> = issues.iter().map(|i| i.number).collect();
        if !self.propagate {
            return Classified {
                roots: listed,
                ..Classified::default()
            };
        }
        let known = materialize::owners(
            self.forge,
            &mut self.refusals.owners,
            self.ctx.slug,
            issues,
            materialize::MAX_OWNER_READS_PER_PASS,
        );
        materialize::classify(&known, |root| self.root_state(root, &listed))
    }

    /// One step of the walk onto `child`: skip it when it is known to
    /// inherit nothing, defer it when the cap is reached, else evaluate it.
    fn visit(&mut self, child: u32, cap: usize, now: u64) -> Visit {
        let key = (self.ctx.slug.to_string(), child);
        if self.refusals.settled.contains_key(&key) {
            return Visit::Settled;
        }
        let Some(issue) = self.issue(child) else {
            return Visit::Deferred;
        };
        if issue.is_pull_request || !issue.state.eq_ignore_ascii_case("open") {
            self.refusals.deferred.remove(&key);
            self.refusals.settled.insert(key, now);
            return Visit::Settled;
        }
        if self.reads + self.pending_blocker_reads(&issue) > cap {
            log::debug!(
                "star_liveness: {} reached {MAX_WALK_READS_PER_PASS} walk reads \
                 this pass; #{child} waits for a later one",
                self.ctx.slug
            );
            return Visit::Deferred;
        }
        self.refusals.deferred.remove(&key);
        Visit::Open(Box::new(self.evaluate_one(&issue, None, None)))
    }

    fn evaluate_one(
        &mut self,
        issue: &RestIssue,
        inherited_from: Option<u32>,
        starred_at: Option<String>,
    ) -> Evaluated {
        let n = issue.number;
        let pr = self.prs_by_issue.get(&n).cloned();
        let pr_facts = pr.as_ref().map(|p| PrFacts {
            item: item_facts(p),
            refusal: None,
        });
        let pr_facts = match (pr_facts, pr.as_ref()) {
            (Some(mut facts), Some(p)) => {
                facts.refusal = self.refusal_for(p, n);
                Some(facts)
            }
            _ => None,
        };
        let row = self.tick_row(n);
        let disposition = row.map(|r| r.disposition);
        let row_starred_at = row.and_then(|r| r.operator_priority_at.clone());
        let (blockers, comment_facts) = self.blocked_facts(issue);
        let comments_unread = comment_facts.is_none();
        let comment_facts = comment_facts.unwrap_or_default();
        let facts = StarFacts {
            repo: self.ctx.slug.to_string(),
            managed: true,
            issue: item_facts(issue),
            pr: pr_facts,
            blockers,
            curator_handoff: comment_facts.handoff,
            unblocked_before: comment_facts.unblocked,
            comments_unread,
            red_main_fix: self.red_main_fix(),
            live_sweep: matches!(
                disposition,
                Some(QueueDisposition::InFlight | QueueDisposition::Dispatched)
            ),
            peer_claimed: disposition == Some(QueueDisposition::PeerClaim),
            capacity: self.capacity(n),
            host: self.ctx.host.to_string(),
        };
        let mut landing = classify(&facts);
        let starred_at = starred_at
            .or(row_starred_at)
            .or_else(|| (self.ctx.recorded_starred_at)(n));
        let fp = fingerprint(
            &issue.labels,
            pr.as_ref()
                .map(|p| (p.number, p.labels.as_slice(), p.updated_at.as_deref())),
        );
        // One key per issue state, not per host or per exhaustion episode:
        // every host and every re-exhaustion of an issue that has not moved
        // share it, so the ask is posted once until the issue progresses.
        if let Some(ask) = landing
            .ask
            .as_mut()
            .filter(|a| a.kind == AskKind::PoolsExhausted)
        {
            ask.key = format!("{}:{}", AskKind::PoolsExhausted.as_str(), short_hash(&fp));
        }
        Evaluated {
            facts,
            landing,
            starred_at,
            inherited_from,
            fingerprint: fp,
            item: work_item(issue),
        }
    }

    /// Evaluate the repo: every open starred issue, then the blockers that
    /// inherit a star, breadth-first up to [`MAX_INHERIT_DEPTH`].
    ///
    /// # Errors
    /// The starred listing itself failed (nothing can be said about the repo).
    pub fn run(&mut self) -> Result<Vec<Evaluated>> {
        // Every operator level's label (#10307): level >= 2 counts as
        // starred, and a level-2 issue need not also carry the star. The
        // star's own listing failing still fails the repo, as before.
        let mut by_number: BTreeMap<u32, RestIssue> = BTreeMap::new();
        // A failed level listing may hide a root: nothing is unstarred then.
        let mut listings_complete = true;
        for label in crate::operator_levels::operator_labels(crate::operator_levels::table()) {
            let rows = match self.forge.list_open(label) {
                Ok(rows) => rows,
                Err(e) if label == OPERATOR_PRIORITY_LABEL => return Err(e),
                Err(e) => {
                    log::debug!("star_liveness: listing {label} in {} failed: {e}", self.ctx.slug);
                    listings_complete = false;
                    Vec::new()
                }
            };
            for r in rows {
                by_number.entry(r.number).or_insert(r);
            }
        }
        let starred: Vec<RestIssue> = by_number.into_values().collect();
        let issues: Vec<RestIssue> = starred
            .iter()
            .filter(|r| !r.is_pull_request)
            .cloned()
            .collect();
        // Starred open PRs: never roots, but the pass stars the PR of a
        // starred issue and takes back an inherited PR star (#10591).
        let starred_prs: Vec<RestIssue> = starred
            .iter()
            .filter(|r| r.is_pull_request)
            .cloned()
            .collect();
        if issues.is_empty() && !(self.propagate && !starred_prs.is_empty()) {
            return Ok(Vec::new());
        }
        let mut prs: BTreeMap<u32, RestIssue> =
            starred_prs.iter().map(|r| (r.number, r.clone())).collect();
        for label in PR_LABELS {
            match self.forge.list_open(label) {
                Ok(rows) => prs.extend(
                    rows.into_iter()
                        .filter(|r| r.is_pull_request)
                        .map(|r| (r.number, r)),
                ),
                Err(e) => {
                    log::debug!("star_liveness: listing {label} in {} failed: {e}", self.ctx.slug)
                }
            }
        }
        // Newest PR wins when several link one issue.
        for pr in prs.values() {
            for issue in linked_issues(pr) {
                self.prs_by_issue.insert(issue, pr.clone());
            }
        }
        for i in &issues {
            self.issues.insert(i.number, Some(i.clone()));
        }
        // An inherited star is walked as a child of its root, not as a root
        // of its own (#10012 §3).
        let owned: Vec<RestIssue> = issues.iter().chain(&starred_prs).cloned().collect();
        let classified = self.classify(&owned);

        let mut out: Vec<Evaluated> = issues
            .iter()
            .filter(|i| classified.roots.contains(&i.number))
            .map(|i| self.evaluate_one(i, None, None))
            .collect();
        let roots: Vec<Root> = out
            .iter()
            .map(|e| Root {
                number: e.facts.issue.number,
                starred_at: e.starred_at.clone(),
            })
            .collect();

        // Explore breadth-first from every root at once, so each issue is
        // expanded at its shallowest depth; then [`edges::descendants`]
        // decides, per child, which starred ancestor it inherits from
        // (earliest starred-at). Every open child inherits, not only the
        // first one named (#10012).
        let mut seen: BTreeSet<u32> = classified.roots.clone();
        let mut found: Vec<Evaluated> = Vec::new();
        let mut graph: Vec<Edge> = Vec::new();
        // Nodes to expand, by depth: a deferred child resumed below joins its
        // own depth's list, so it is not starved by shallower siblings.
        let mut levels: Vec<Vec<Pending>> = vec![Vec::new(); MAX_INHERIT_DEPTH + 1];
        levels[0] = out
            .iter()
            .map(|e| (e.facts.issue.number, e.landing.inherits.clone(), Vec::new()))
            .collect();
        // Count the walk's forge reads, not the open children it finds: a
        // closed child or a PR costs a read too (#10073 review).
        let walk_start = self.reads;
        let cap = walk_start + MAX_WALK_READS_PER_PASS;
        self.read_cap = Some(cap);
        let slug = self.ctx.slug.to_string();
        self.refusals.walk_pass += 1;
        let now = self.refusals.walk_pass;
        self.refusals
            .settled
            .retain(|_, at| now.saturating_sub(*at) < SETTLED_TTL_PASSES);

        // Resume what the cap deferred last pass before anything else, down
        // its recorded path (an ancestor not evaluated yet is evaluated
        // first), so a deep child is not starved by a full shallow frontier
        // or by an earlier root. A path the forge text no longer supports is
        // dropped.
        let mut inherits_of: HashMap<u32, Vec<u32>> = levels[0]
            .iter()
            .map(|(n, inh, _)| (*n, inh.clone()))
            .collect();
        let resumable: Vec<(u32, Vec<Edge>)> = self
            .refusals
            .deferred
            .iter()
            .filter(|((s, _), _)| *s == slug)
            .map(|((_, child), path)| (*child, path.clone()))
            .collect();
        for (target, path) in resumable {
            let key = (slug.clone(), target);
            for (i, recorded) in path.iter().enumerate() {
                let still_linked = inherits_of.get(&recorded.parent).is_some_and(|inh| {
                    self.children(recorded.parent, inh)
                        .iter()
                        .any(|e| e.child == recorded.child)
                });
                if !still_linked {
                    self.refusals.deferred.remove(&key);
                    break;
                }
                let child = recorded.child;
                if seen.contains(&child) {
                    continue;
                }
                match self.visit(child, cap, now) {
                    Visit::Settled => {
                        self.refusals.deferred.remove(&key);
                        break;
                    }
                    Visit::Deferred => break,
                    Visit::Open(e) => {
                        seen.insert(child);
                        inherits_of.insert(child, e.landing.inherits.clone());
                        levels[i + 1].push((
                            child,
                            e.landing.inherits.clone(),
                            path[..=i].to_vec(),
                        ));
                        found.push(*e);
                    }
                }
            }
        }

        // Explore breadth-first from every root at once, so each issue is
        // expanded at its shallowest depth; then [`edges::descendants`]
        // decides, per child, which starred ancestor it inherits from
        // (earliest starred-at). Every open child inherits, not only the
        // first one named (#10012).
        for depth in 0..MAX_INHERIT_DEPTH {
            let frontier = std::mem::take(&mut levels[depth]);
            for (parent, inherits, path) in frontier {
                for edge in self.children(parent, &inherits) {
                    let child = edge.child;
                    graph.push(edge);
                    if !seen.insert(child) {
                        continue;
                    }
                    let mut child_path = path.clone();
                    child_path.push(edge);
                    match self.visit(child, cap, now) {
                        Visit::Settled => {}
                        Visit::Deferred => {
                            self.refusals
                                .deferred
                                .insert((slug.clone(), child), child_path);
                        }
                        Visit::Open(e) => {
                            levels[depth + 1].push((child, e.landing.inherits.clone(), child_path));
                            found.push(*e);
                        }
                    }
                }
            }
        }
        self.read_cap = None;
        let walk_complete =
            listings_complete && !self.refusals.deferred.keys().any(|(s, _)| *s == slug);
        let open: BTreeSet<u32> = found.iter().map(|e| e.facts.issue.number).collect();
        graph.retain(|e| open.contains(&e.child));
        let inherited = edges::descendants(&roots, &graph, MAX_INHERIT_DEPTH);
        // Only a parent/child link the issue text records materializes as
        // the label (#10012 §1). A landing-only edge (a blocker named in a
        // comment, a merge refusal's incident, the red-main fix) is a
        // transient liveness fact: it orders the work in memory but never
        // writes a lasting star.
        let structural: Vec<Edge> = graph
            .iter()
            .filter(|e| e.source != EdgeSource::LandingBlocker)
            .copied()
            .collect();
        let materialized = edges::descendants(&roots, &structural, MAX_INHERIT_DEPTH);
        let mut unreached: BTreeMap<u32, Evaluated> = BTreeMap::new();
        // (issue, root, starred_at) for each starred issue the pass reaches
        // by a recorded link: its open PR inherits the star too.
        let mut pr_targets: Vec<(u32, u32, Option<String>)> = Vec::new();
        for r in &roots {
            let carries = issues.iter().any(|i| {
                i.number == r.number && i.labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL)
            });
            if carries {
                pr_targets.push((r.number, r.number, r.starred_at.clone()));
            }
        }
        for mut e in found {
            let n = e.facts.issue.number;
            let Some(inh) = inherited.get(&n) else {
                unreached.insert(n, e);
                continue;
            };
            if let Some(m) = materialized.get(&n) {
                pr_targets.push((n, m.root, m.starred_at.clone()));
            }
            if let Some(m) = materialized.get(&n).filter(|_| {
                self.propagate
                    && !e
                        .facts
                        .issue
                        .labels
                        .iter()
                        .any(|l| l == OPERATOR_PRIORITY_LABEL)
            }) {
                self.plan.adds.push(materialize::Add {
                    child: n,
                    root: m.root,
                    starred_at: m.starred_at.clone(),
                });
            }
            e.inherited_from = Some(inh.via);
            e.starred_at = inh.starred_at.clone().or(e.starred_at);
            super::landing::withhold_inherited_handoff(&e.facts, &mut e.landing, inh.via, inh.root);
            out.push(e);
        }
        if self.propagate {
            let mut planned: BTreeSet<u32> = BTreeSet::new();
            for (n, root, starred_at) in pr_targets {
                let Some(pr) = self.prs_by_issue.get(&n) else {
                    continue;
                };
                if pr.labels.iter().any(|l| l == OPERATOR_PRIORITY_LABEL)
                    || !planned.insert(pr.number)
                {
                    continue;
                }
                self.plan.adds.push(materialize::Add {
                    child: pr.number,
                    root,
                    starred_at,
                });
            }
        }
        self.plan.adds.sort_by_key(|a| a.child);
        // An inherited star nothing reaches: removed when its root lost its
        // star and the walk saw everything, else kept as a starred row.
        let listed: BTreeSet<u32> = issues.iter().map(|i| i.number).collect();
        for i in &issues {
            let n = i.number;
            if classified.roots.contains(&n) || inherited.contains_key(&n) {
                continue;
            }
            if let Some(&root) = classified.orphaned.get(&n) {
                if walk_complete && !self.has_starred_parent(i, &listed) {
                    self.plan
                        .removes
                        .push(materialize::Remove { child: n, root });
                    continue;
                }
            }
            let e = match unreached.remove(&n) {
                Some(e) => e,
                None => self.evaluate_one(i, None, None),
            };
            out.push(e);
        }
        // An inherited PR star whose root lost its star, whose linked issue
        // is neither starred nor inherited (an issue this pass is taking the
        // star off counts as neither), after a complete walk. A star the
        // operator owns (or whose owner is unread) is never touched.
        if walk_complete {
            let removed: BTreeSet<u32> = self.plan.removes.iter().map(|r| r.child).collect();
            for pr in &starred_prs {
                let Some(&root) = classified.orphaned.get(&pr.number) else {
                    continue;
                };
                let alive = linked_issues(pr).into_iter().any(|n| {
                    inherited.contains_key(&n) || (listed.contains(&n) && !removed.contains(&n))
                });
                if !alive {
                    self.plan.removes.push(materialize::Remove {
                        child: pr.number,
                        root,
                    });
                }
            }
        }
        Ok(out)
    }
}

/// A row for a star this host cannot act on because no workspace here
/// manages the repo (a loom-ui intent). Pure.
#[must_use]
pub fn unmanaged(slug: &str, number: u32, starred_at: Option<String>) -> Evaluated {
    let facts = StarFacts {
        repo: slug.to_string(),
        managed: false,
        issue: ItemFacts {
            number,
            open: true,
            ..ItemFacts::default()
        },
        ..StarFacts::default()
    };
    let landing = classify(&facts);
    Evaluated {
        facts,
        landing,
        starred_at,
        inherited_from: None,
        fingerprint: String::new(),
        item: WorkItem::new(number, Vec::new()),
    }
}
