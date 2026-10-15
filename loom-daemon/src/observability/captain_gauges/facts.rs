//! The captain's forge facts (W12 part 2): the per-host `loom:blocked` forge
//! read that says the same thing on every host, read once and published in the
//! heartbeat ([`super::store`]).
//!
//! | job | the captain reads | a dispatcher stops reading |
//! |---|---|---|
//! | `queue-blocked` | the open `loom:blocked` listing, per repo (caller `queue_blocked`) | the same listing, for every covered repo |
//!
//! The read is recorded under the caller it replaces, so
//! `loom-daemon forge calls --by caller` shows it once on one host instead of
//! on every host.
//!
//! # Coverage is all-or-nothing per repo
//!
//! A repo is covered only when its listing succeeded this pass. A failed
//! listing (the reader was rate-limited or withdrawn, the forge was down)
//! leaves the repo out, so each dispatcher reads it itself. The failure is
//! logged by the listing helper and reported nowhere else: it never reaches
//! the host-wide rate-limit breaker.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;

use chrono::{DateTime, Utc};

use super::store::{BlockedFact, JobFacts};
use super::{Config, QUEUE_BLOCKED_JOB};
use crate::forge_listing::RestIssue;
use crate::observability::queue_blocked::{self, BLOCKED_LABEL};
use crate::workspace_pool::WorkspacePool;

/// The caller the captain's `loom:blocked` listings are recorded under.
pub const BLOCKED_CALLER: &str = "queue_blocked";

/// Whether a label name may be published: Loom's own vocabulary, which is
/// everything a snapshot row is derived from.
fn publishable(label: &str) -> bool {
    label.starts_with("loom:")
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
            open_sub_issues: None,
            labels_at: None,
        })
        .collect()
}

/// One repo's listings for one pass. `None` for a listing that failed.
#[derive(Debug, Clone, Default)]
pub struct RepoListings {
    /// Lowercased `owner/repo`.
    pub slug: String,
    /// The `loom:blocked` listing (`queue-blocked`).
    pub blocked: Option<Vec<RestIssue>>,
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

/// One pass over `targets` (each provisioned root with the slug its facts are
/// published under), listing through `list(root, repo, label, caller)`.
///
/// - **The listing names the published repo.** `repo` is always the slug the
///   facts are published under, never left to the listing helper: that one
///   would list the checkout's `origin` (or `LOOM_REPO`), while the slug
///   resolves like `gh` (an `upstream` remote first, renames followed). On a
///   fork checkout the two differ, and a dispatcher would skip the upstream's
///   listing on the strength of the fork's rows. `root` only picks the
///   credential.
/// - **One listing per repo.** Roots that resolve to the same slug (compared
///   lowercased) are listed once; since the listing names the slug, which
///   root came first does not matter.
/// - **`as_of` is taken before the first listing.** A pass that takes minutes
///   must not claim a freshness its first listings do not have.
pub async fn gather<F, Fut>(targets: Vec<(PathBuf, String)>, mut list: F) -> JobFacts
where
    F: FnMut(PathBuf, String, &'static str, &'static str) -> Fut,
    Fut: Future<Output = Option<Vec<RestIssue>>>,
{
    let as_of = Utc::now();
    let mut repos: BTreeMap<String, RepoListings> = BTreeMap::new();
    for (root, slug) in targets {
        let key = slug.to_ascii_lowercase();
        if repos.contains_key(&key) {
            continue;
        }
        let blocked = list(root, slug, BLOCKED_LABEL, BLOCKED_CALLER).await;
        repos.insert(key.clone(), RepoListings { slug: key, blocked });
    }
    let repos: Vec<RepoListings> = repos.into_values().collect();
    blocked_job(&repos, as_of)
}

/// The captain's facts pass: list what the switched-on job needs for every
/// provisioned repo and record the result for the next heartbeat. Called only
/// on the armed captain.
pub(super) async fn produce(
    config: &Config,
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) {
    if !config.queue_blocked {
        return;
    }
    let mut targets = Vec::new();
    for root in crate::observability::collector::provisioned_roots(workspace_pool) {
        let root_str = root.to_string_lossy().to_string();
        if let Some(slug) =
            crate::observability::collector::resolve_repo_slug_cached(slug_cache, &root_str).await
        {
            targets.push((root, slug));
        }
    }
    let job = gather(targets, |root, repo, label, caller| {
        queue_blocked::list_open_in(root, Some(repo), label, caller)
    })
    .await;
    super::note_facts(QUEUE_BLOCKED_JOB, job);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "facts_tests.rs"]
mod tests;
