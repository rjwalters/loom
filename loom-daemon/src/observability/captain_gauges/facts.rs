//! The captain's forge facts (W12 part 2): the two per-host forge reads that
//! say the same thing on every host, read once and published in the heartbeat
//! ([`super::store`]).
//!
//! | job | the captain reads | a dispatcher stops reading |
//! |---|---|---|
//! | `star-facts` | each operator label's open listing, per repo (caller `star_liveness`) | the same listings, for a repo reported to have no open starred issue |
//! | `queue-blocked` | the open `loom:blocked` listing, per repo (caller `queue_blocked`) | the same listing, for every covered repo |
//!
//! The reads are recorded under the callers they replace, so
//! `loom-daemon forge calls --by caller` shows the same reads on one host
//! instead of on every host.
//!
//! # Why only the "no star here" fact moves
//!
//! The liveness pass is not a fleet fact. Its landing rows read this host's
//! work-finder tick, cap and token-pool holds; its blocker inheritance is a
//! dispatch input, published in-process for this host's work finder; its
//! escalations are forge writes deduplicated by a marker every evaluating
//! host reads. None of that may depend on another host. What every host
//! agrees on is the precondition: a repo with no open starred issue makes the
//! pass produce nothing at all (`collect::Evaluator::run` returns before any
//! other read). That is the one thing a dispatcher takes from the captain,
//! and only as permission to skip a pass whose output is empty.
//!
//! # Coverage is all-or-nothing per repo
//!
//! A repo is covered only when every listing the job needs succeeded this
//! pass. A failed listing (the reader was rate-limited or withdrawn, the
//! forge was down) leaves the repo out, so each dispatcher reads it itself.
//! The failure is logged by the listing helper and reported nowhere else: it
//! never reaches the host-wide rate-limit breaker.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::store::{BlockedFact, JobFacts};
use super::{Config, QUEUE_BLOCKED_JOB, STAR_FACTS_JOB};
use crate::forge_listing::RestIssue;
use crate::observability::queue_blocked::{self, BLOCKED_LABEL};
use crate::workspace_pool::WorkspacePool;

/// The caller the captain's operator-label listings are recorded under: the
/// one whose reads they replace.
pub const STAR_CALLER: &str = "star_liveness";
/// The caller the captain's `loom:blocked` listings are recorded under.
pub const BLOCKED_CALLER: &str = "queue_blocked";

/// Open starred **issues** across one repo's operator-label listings (a
/// starred PR is Builder's copy of the star, and the liveness pass does not
/// evaluate it). Pure.
#[must_use]
pub fn star_count(listings: &[Vec<RestIssue>]) -> u32 {
    let numbers: BTreeSet<u32> = listings
        .iter()
        .flatten()
        .filter(|item| !item.is_pull_request && item.state.eq_ignore_ascii_case("open"))
        .map(|item| item.number)
        .collect();
    u32::try_from(numbers.len()).unwrap_or(u32::MAX)
}

/// The repos `job` (a fresh `star-facts`) reports as having no open starred
/// issue, with its `as_of`. Empty when the captain did not list every label
/// in `labels`: a level this host knows and the captain does not could hide a
/// star, so nothing is believed. Pure.
#[must_use]
pub fn star_free(job: &JobFacts, labels: &[&str]) -> HashMap<String, DateTime<Utc>> {
    if labels.is_empty() || !labels.iter().all(|label| job.labels.contains(*label)) {
        return HashMap::new();
    }
    job.repos
        .iter()
        .filter(|repo| job.counts.get(*repo).copied().unwrap_or(0) == 0)
        .map(|repo| (repo.clone(), job.as_of))
        .collect()
}

/// Whether a label name may be published: Loom's own vocabulary and the tier
/// labels, which is everything a snapshot row is derived from.
fn publishable(label: &str) -> bool {
    label.starts_with("loom:") || label.starts_with("tier:")
}

/// One repo's `loom:blocked` listing reduced to what its snapshot rows need.
/// The same items [`queue_blocked::blocked_rows`] keeps. Pure.
#[must_use]
pub fn blocked_facts(listing: &[RestIssue]) -> Vec<BlockedFact> {
    listing
        .iter()
        .filter(|item| queue_blocked::is_blocked_row(item))
        .map(|item| BlockedFact {
            number: item.number,
            created_at: item.created_at.clone(),
            labels: item
                .labels
                .iter()
                .filter(|label| publishable(label))
                .cloned()
                .collect(),
        })
        .collect()
}

/// Published facts back as listing items, so a dispatcher derives its rows
/// with the very function it uses on its own listing. Pure.
#[must_use]
pub fn as_listing(facts: &[BlockedFact]) -> Vec<RestIssue> {
    facts
        .iter()
        .map(|fact| RestIssue {
            number: fact.number,
            title: None,
            labels: fact.labels.clone(),
            created_at: fact.created_at.clone(),
            updated_at: None,
            closed_at: None,
            state: "open".to_string(),
            body: None,
            author: None,
            author_association: None,
            is_pull_request: false,
            comments: 0,
        })
        .collect()
}

/// One repo's listings for one pass. `None` for a listing that failed.
#[derive(Debug, Clone, Default)]
pub struct RepoListings {
    /// Lowercased `owner/repo`.
    pub slug: String,
    /// One listing per operator label, in `labels` order (`star-facts`).
    pub star: Option<Vec<Vec<RestIssue>>>,
    /// The `loom:blocked` listing (`queue-blocked`).
    pub blocked: Option<Vec<RestIssue>>,
}

/// The `star-facts` job from one pass's listings. Pure.
#[must_use]
pub fn star_job(repos: &[RepoListings], labels: &[&str], at: DateTime<Utc>) -> JobFacts {
    let mut job = JobFacts {
        as_of: at,
        labels: labels.iter().map(|label| (*label).to_string()).collect(),
        ..JobFacts::default()
    };
    for repo in repos {
        let Some(listings) = &repo.star else {
            continue;
        };
        job.repos.insert(repo.slug.clone());
        let count = star_count(listings);
        if count > 0 {
            job.counts.insert(repo.slug.clone(), count);
        }
    }
    job
}

/// The `queue-blocked` job from one pass's listings. Pure.
#[must_use]
pub fn blocked_job(repos: &[RepoListings], at: DateTime<Utc>) -> JobFacts {
    let mut job = JobFacts {
        as_of: at,
        ..JobFacts::default()
    };
    for repo in repos {
        let Some(listing) = &repo.blocked else {
            continue;
        };
        job.repos.insert(repo.slug.clone());
        let rows = blocked_facts(listing);
        if !rows.is_empty() {
            job.blocked.insert(repo.slug.clone(), rows);
        }
    }
    job
}

/// Every listing of `labels` for `repo`, or `None` as soon as one fails.
async fn list_all<F, Fut>(
    list: &mut F,
    root: &Path,
    repo: &str,
    labels: &[&'static str],
) -> Option<Vec<Vec<RestIssue>>>
where
    F: FnMut(PathBuf, String, &'static str, &'static str) -> Fut,
    Fut: Future<Output = Option<Vec<RestIssue>>>,
{
    let mut out = Vec::with_capacity(labels.len());
    for label in labels {
        out.push(list(root.to_path_buf(), repo.to_string(), label, STAR_CALLER).await?);
    }
    Some(out)
}

/// The facts of one pass: `(star-facts, queue-blocked)`, each `None` when its
/// switch is off.
pub type PassFacts = (Option<JobFacts>, Option<JobFacts>);

/// One pass over `targets` (each provisioned root with the slug its facts are
/// published under), listing through `list(root, repo, label, caller)`.
///
/// - **The listing names the published repo.** `repo` is always the slug the
///   facts are published under, never left to the listing helper: that one
///   would list the checkout's `origin` (or `LOOM_REPO`), while the slug
///   resolves like `gh` (an `upstream` remote first, renames followed). On a
///   fork checkout the two differ, and a dispatcher would skip the upstream's
///   evaluation on the strength of the fork's star count. `root` only picks
///   the credential.
/// - **One listing per repo.** Roots that resolve to the same slug (compared
///   lowercased) are listed once; since the listing names the slug, which
///   root came first does not matter.
/// - **`as_of` is taken before the first listing.** A pass that takes minutes
///   must not claim a freshness its first listings do not have.
pub async fn gather<F, Fut>(
    star_facts: bool,
    queue_blocked: bool,
    labels: &[&'static str],
    targets: Vec<(PathBuf, String)>,
    mut list: F,
) -> PassFacts
where
    F: FnMut(PathBuf, String, &'static str, &'static str) -> Fut,
    Fut: Future<Output = Option<Vec<RestIssue>>>,
{
    if !star_facts && !queue_blocked {
        return (None, None);
    }
    let as_of = Utc::now();
    let mut repos: BTreeMap<String, RepoListings> = BTreeMap::new();
    for (root, slug) in targets {
        let key = slug.to_ascii_lowercase();
        if repos.contains_key(&key) {
            continue;
        }
        let mut listings = RepoListings {
            slug: key.clone(),
            ..RepoListings::default()
        };
        if star_facts {
            listings.star = list_all(&mut list, &root, &slug, labels).await;
        }
        if queue_blocked {
            listings.blocked = list(root, slug, BLOCKED_LABEL, BLOCKED_CALLER).await;
        }
        repos.insert(key, listings);
    }
    let repos: Vec<RepoListings> = repos.into_values().collect();
    (
        star_facts.then(|| star_job(&repos, labels, as_of)),
        queue_blocked.then(|| blocked_job(&repos, as_of)),
    )
}

/// The captain's facts pass: list what the switched-on jobs need for every
/// provisioned repo and record the result for the next heartbeat. Called only
/// on the armed captain.
pub(super) async fn produce(
    config: &Config,
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) {
    if !config.star_facts && !config.queue_blocked {
        return;
    }
    let labels = crate::operator_levels::operator_labels(crate::operator_levels::table());
    let mut targets = Vec::new();
    for root in crate::observability::collector::provisioned_roots(workspace_pool) {
        let root_str = root.to_string_lossy().to_string();
        if let Some(slug) =
            crate::observability::collector::resolve_repo_slug_cached(slug_cache, &root_str).await
        {
            targets.push((root, slug));
        }
    }
    let (star, blocked) = gather(
        config.star_facts,
        config.queue_blocked,
        &labels,
        targets,
        |root, repo, label, caller| queue_blocked::list_open_in(root, Some(repo), label, caller),
    )
    .await;
    if let Some(job) = star {
        super::note_facts(STAR_FACTS_JOB, job);
    }
    if let Some(job) = blocked {
        super::note_facts(QUEUE_BLOCKED_JOB, job);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "facts_tests.rs"]
mod tests;
