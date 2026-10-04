//! Gather one repo's facts and classify every starred issue and every
//! blocker that inherits a star. Generic over [`StarForge`], so the replay
//! fixture drives exactly this code.
//!
//! # Reads per pass (only for a repo with at least one starred issue)
//!
//! - the ETag-cached `loom:operator-priority` listing (issues **and** PRs:
//!   Builder copies the star onto its PR, slice B);
//! - the ETag-cached listings for [`PR_LABELS`], to find each starred
//!   issue's open PR by its closing / `Part of` reference;
//! - the comments of a starred issue's **approved** PR, only when that PR's
//!   `updated_at` moved since the last read (merge-refusal detection);
//! - for a detected refusal, the incident lookup: single-issue reads of the
//!   issues the refusal comment names, and, when none is an open issue and
//!   the refusal quotes one of the specific forge phrases, one issue search
//!   for that phrase (repeated each pass only while no open incident is
//!   known; hits must be trusted-authored and quote it word-bounded);
//! - one single-issue read per same-repo blocker (the blocker's state, and
//!   its labels when it inherits a star).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;

use super::forge::StarForge;
use super::landing::{
    classify, BlockerRef, Capacity, ItemFacts, Landing, MergeRefusal, PrFacts, StarFacts,
    BLOCKED_LABEL,
};
use super::progress::{fingerprint, short_hash};
use super::refusal::{self, Detected};
use crate::forge_listing::RestIssue;
use crate::types::{AskKind, QueueDisposition, ReadyQueueRow};
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

/// Cached refusal detections, keyed by (repo, PR) and valid while the PR's
/// `updated_at` is unchanged, plus the incident a signature search found
/// (re-checked open every pass).
#[derive(Debug, Default)]
pub struct RefusalCache {
    entries: HashMap<(String, u32), (Option<String>, Option<Detected>)>,
    incidents: HashMap<(String, u32), u32>,
}

/// Everything about the repo that is not a forge read.
pub struct RepoContext<'a> {
    /// Forge `owner/repo`.
    pub slug: &'a str,
    /// This host's id.
    pub host: &'a str,
    /// The last work-finder tick's rows for this repo.
    pub tick_rows: &'a [ReadyQueueRow],
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

/// The per-pass evaluator for one repo.
pub struct Evaluator<'a> {
    pub forge: &'a mut dyn StarForge,
    pub ctx: RepoContext<'a>,
    pub refusals: &'a mut RefusalCache,
    issues: HashMap<u32, Option<RestIssue>>,
    prs_by_issue: BTreeMap<u32, RestIssue>,
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
        }
    }

    fn tick_row(&self, n: u32) -> Option<&ReadyQueueRow> {
        self.ctx.tick_rows.iter().find(|r| r.issue == n)
    }

    fn issue(&mut self, n: u32) -> Option<RestIssue> {
        if let Some(cached) = self.issues.get(&n) {
            return cached.clone();
        }
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

    fn blockers(&mut self, issue: &RestIssue) -> Vec<BlockerRef> {
        if !issue.labels.iter().any(|l| l == BLOCKED_LABEL) {
            return Vec::new();
        }
        let body = issue.body.as_deref().unwrap_or_default();
        let prefix = format!("{}#", self.ctx.slug.to_ascii_lowercase());
        crate::dep_classify::refs::parse_named_blocker_refs(body, self.ctx.slug)
            .into_iter()
            .map(|r| {
                let same = r
                    .to_ascii_lowercase()
                    .strip_prefix(&prefix)
                    .and_then(|n| n.parse::<u32>().ok());
                match same {
                    Some(n) if n != issue.number => {
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
            })
            .collect()
    }

    fn capacity(&self, n: u32) -> Capacity {
        if let Some(detail) = &self.ctx.pool {
            return Capacity::PoolExhausted {
                detail: detail.clone(),
            };
        }
        match self.tick_row(n).map(|r| r.disposition) {
            Some(
                d @ (QueueDisposition::DeferredCapacity
                | QueueDisposition::DeferredRampCap
                | QueueDisposition::DeferredSaturation
                | QueueDisposition::DeferredBuildBackoff
                | QueueDisposition::DeferredOutOfSlice
                | QueueDisposition::DeferredRepoCap
                | QueueDisposition::HostConstraint
                | QueueDisposition::HostClassRefused),
            ) => Capacity::Deferred {
                reason: d.reason().to_string(),
            },
            _ => Capacity::Available,
        }
    }

    fn red_main_fix(&self) -> Option<u32> {
        self.ctx
            .tick_rows
            .iter()
            .filter(|r| r.main_red_fix)
            .map(|r| r.issue)
            .min()
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
        let facts = StarFacts {
            repo: self.ctx.slug.to_string(),
            managed: true,
            issue: item_facts(issue),
            pr: pr_facts,
            blockers: self.blockers(issue),
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
        let starred = self.forge.list_open(OPERATOR_PRIORITY_LABEL)?;
        let issues: Vec<RestIssue> = starred
            .iter()
            .filter(|r| !r.is_pull_request)
            .cloned()
            .collect();
        if issues.is_empty() {
            return Ok(Vec::new());
        }
        let mut prs: BTreeMap<u32, RestIssue> = starred
            .into_iter()
            .filter(|r| r.is_pull_request)
            .map(|r| (r.number, r))
            .collect();
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

        let mut out: Vec<Evaluated> = issues
            .iter()
            .map(|i| self.evaluate_one(i, None, None))
            .collect();
        let mut seen: BTreeSet<u32> = issues.iter().map(|i| i.number).collect();
        let mut frontier: Vec<(u32, u32, Option<String>)> = out
            .iter()
            .filter_map(|e| {
                e.landing
                    .inherits
                    .map(|b| (b, e.facts.issue.number, e.starred_at.clone()))
            })
            .collect();
        for _ in 0..MAX_INHERIT_DEPTH {
            let mut next = Vec::new();
            for (blocker, from, at) in frontier {
                if !seen.insert(blocker) {
                    continue;
                }
                let Some(issue) = self.issue(blocker) else {
                    continue;
                };
                if issue.is_pull_request || !issue.state.eq_ignore_ascii_case("open") {
                    continue;
                }
                let e = self.evaluate_one(&issue, Some(from), at);
                if let Some(b) = e.landing.inherits {
                    next.push((b, blocker, e.starred_at.clone()));
                }
                out.push(e);
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
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
