//! The forge reads behind dependency-aware ETAs (#10510); the graph, the
//! point-in-time rule and the composition are [`crate::eta::dependency`]'s.
//!
//! Per pass, before the pass's `as_of` is taken (so every edge is knowable
//! at the estimates it feeds), at most [`READ_BUDGET`] `(item, source)`
//! reads, never-read and oldest first, each at most every
//! [`REFRESH_SECS`]:
//!
//! - **Parked** (an unstarted item refused `blocked` or `no_dispatch_plan`):
//!   `issues/{n}` for its park records (`Blocked by: #N`, same repo), and
//!   `issues/{n}/dependencies/blocked_by` for the forge-native "blocked by"
//!   issues, any repo, each with its state. Each park-record parent the
//!   tracker does not hold open is read once (`issues/{N}`) for whether, and
//!   when, it closed.
//! - **Sequenced** (a PR under `loom:sequenced`): its trusted comment bodies
//!   for the newest live `<!-- loom:sequence after=N -->` marker; the
//!   predecessor PR maps to the tracked issue it closes. A predecessor the
//!   tracker does not hold is skipped: the sequencing pass releases a hold
//!   whose predecessor merged.
//!
//! A failed read changes nothing: the edges already observed stand until a
//! read succeeds. An edge's `known_at` is the instant it was first read.
//! Sub-issues, `loom:epic-phase` order and stacked PRs are edge sources the
//! composition understands but this pass does not read yet.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::eta::dependency::{DependencyBook, EdgeSource, NodeKey};
use crate::eta::tracker::{DependencyCandidate, Tracker};
use crate::forge_call_stats::{ops, ForgeOp};

/// Seconds between reads of one `(item, source)`.
pub const REFRESH_SECS: i64 = 900;

/// `(item, source)` reads per pass, across all repos.
pub const READ_BUDGET: usize = 8;

/// Accounting row for the native dependency listing (no inventory row yet).
const BLOCKED_BY: ForgeOp =
    ForgeOp::uninventoried("issue blocked_by dependency listing has no inventory row");

/// The `owner/repo` an API `repository_url` names.
fn repo_of_url(url: &str) -> Option<String> {
    let rest = url.split("/repos/").nth(1)?;
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

fn closed_at(issue: &Value) -> Option<DateTime<Utc>> {
    (issue["state"].as_str() == Some("closed"))
        .then(|| issue["closed_at"].as_str()?.parse().ok())
        .flatten()
}

/// Parse a `dependencies/blocked_by` page into `(parent, closed_at)`.
#[must_use]
pub fn native_blockers(page: &Value) -> Option<Vec<(NodeKey, Option<DateTime<Utc>>)>> {
    page.as_array().map(|issues| {
        issues
            .iter()
            .filter_map(|issue| {
                let number = u32::try_from(issue["number"].as_u64()?).ok()?;
                let repo = repo_of_url(issue["repository_url"].as_str()?)?;
                Some((NodeKey::new(&repo, number), closed_at(issue)))
            })
            .collect()
    })
}

/// The same-repo park-record parents an issue body declares.
#[must_use]
pub fn park_blockers(slug: &str, body: &str) -> Vec<NodeKey> {
    let mut out: Vec<NodeKey> = crate::park_record::parse(body)
        .into_iter()
        .filter_map(|r| u32::try_from(r.blocker?).ok())
        .map(|n| NodeKey::new(slug, n))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every `(item, source)` worth reading, by the candidates' state.
fn wanted(candidates: &[DependencyCandidate]) -> Vec<(NodeKey, EdgeSource)> {
    let mut out = Vec::new();
    for c in candidates {
        let key = NodeKey::new(&c.key.repo, c.key.issue);
        if c.parked {
            out.push((key.clone(), EdgeSource::ParkRecord));
            out.push((key.clone(), EdgeSource::NativeDependency));
        }
        if c.sequenced {
            out.push((key, EdgeSource::Sequence));
        }
    }
    out
}

/// One read's outcome, applied to the book under the lock.
enum Read {
    Edges {
        child: NodeKey,
        source: EdgeSource,
        parents: Vec<NodeKey>,
        landed: Vec<(NodeKey, DateTime<Utc>)>,
        at: DateTime<Utc>,
    },
    Landed(NodeKey, DateTime<Utc>),
}

fn read_park(root: &Path, slug: &str, child: &NodeKey) -> Option<Read> {
    let issue = super::eta_friction::cached_get(
        root,
        slug,
        &format!("repos/{slug}/issues/{}", child.issue),
        ops::ISSUE_VIEW_STATE,
    )?;
    Some(Read::Edges {
        child: child.clone(),
        source: EdgeSource::ParkRecord,
        parents: park_blockers(slug, issue["body"].as_str().unwrap_or_default()),
        landed: Vec::new(),
        at: Utc::now(),
    })
}

fn read_native(root: &Path, slug: &str, child: &NodeKey) -> Option<Read> {
    let url = format!("repos/{slug}/issues/{}/dependencies/blocked_by?per_page=100", child.issue);
    let blockers =
        native_blockers(&super::eta_friction::cached_get(root, slug, &url, BLOCKED_BY)?)?;
    Some(Read::Edges {
        child: child.clone(),
        source: EdgeSource::NativeDependency,
        parents: blockers.iter().map(|(k, _)| k.clone()).collect(),
        landed: blockers
            .into_iter()
            .filter_map(|(k, at)| Some((k, at?)))
            .collect(),
        at: Utc::now(),
    })
}

fn read_sequence(
    root: &Path,
    slug: &str,
    child: &NodeKey,
    pr: u32,
    pr_issues: &BTreeMap<u32, u32>,
) -> Option<Read> {
    let gh = crate::gh_invocation::gh_bin();
    let bodies = crate::merge_pr::sequence::fetch_trusted_bodies(&gh, root, slug, pr)?;
    let parents = crate::merge_pr::sequence::parse_live(&bodies)
        .and_then(|m| pr_issues.get(&m.after))
        .map(|issue| vec![NodeKey::new(slug, *issue)])
        .unwrap_or_default();
    Some(Read::Edges {
        child: child.clone(),
        source: EdgeSource::Sequence,
        parents,
        landed: Vec::new(),
        at: Utc::now(),
    })
}

fn read_landed(root: &Path, parent: &NodeKey) -> Option<Read> {
    let issue = super::eta_friction::cached_get(
        root,
        &parent.repo,
        &format!("repos/{}/issues/{}", parent.repo, parent.issue),
        ops::ISSUE_VIEW_STATE,
    )?;
    Some(Read::Landed(parent.clone(), closed_at(&issue)?))
}

/// What a pass reads of the tracker, under its lock, before refreshing:
/// the book so far and the items whose edges may be read.
pub(super) fn seed(tracker: &Tracker) -> (DependencyBook, Vec<DependencyCandidate>) {
    (tracker.dependencies.clone(), tracker.dependency_candidates())
}

/// Refresh the due part of the seeded book; `repos` are the pass's
/// `(checkout, slug, ..)` rows, keyed here by lowercased slug. Blocking
/// reads run off the async workers.
pub(super) async fn refresh<A, B>(
    (book, candidates): (DependencyBook, Vec<DependencyCandidate>),
    repos: &[(PathBuf, String, A, B)],
) -> DependencyBook {
    let roots: BTreeMap<String, PathBuf> = repos
        .iter()
        .map(|(root, slug, ..)| (slug.to_ascii_lowercase(), root.clone()))
        .collect();
    let at = Utc::now();
    let mut pr_issues: BTreeMap<String, BTreeMap<u32, u32>> = BTreeMap::new();
    for c in &candidates {
        if let Some(pr) = c.pr_number {
            pr_issues
                .entry(c.key.repo.clone())
                .or_default()
                .insert(pr, c.key.issue);
        }
    }
    let wanted = wanted(&candidates);
    let due = book.due(&wanted, at, REFRESH_SECS, READ_BUDGET);
    let prs: BTreeMap<NodeKey, u32> = candidates
        .iter()
        .filter_map(|c| Some((NodeKey::new(&c.key.repo, c.key.issue), c.pr_number?)))
        .collect();
    let open: BTreeSet<NodeKey> = candidates
        .iter()
        .map(|c| NodeKey::new(&c.key.repo, c.key.issue))
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut book = book;
        for (child, source) in due {
            let Some(root) = roots.get(&child.repo) else {
                continue;
            };
            let slug = child.repo.clone();
            let read = match source {
                EdgeSource::ParkRecord => read_park(root, &slug, &child),
                EdgeSource::NativeDependency => read_native(root, &slug, &child),
                EdgeSource::Sequence => prs
                    .get(&child)
                    .and_then(|pr| read_sequence(root, &slug, &child, *pr, pr_issues.get(&slug)?)),
                _ => None,
            };
            let Some(Read::Edges {
                child,
                source,
                parents,
                landed,
                at,
            }) = read
            else {
                continue;
            };
            book.set(&child, source, &parents, at);
            for (parent, closed) in landed {
                book.set_landed(&parent, closed);
            }
            if source == EdgeSource::ParkRecord {
                for parent in parents.iter().filter(|p| !open.contains(p)) {
                    let root = roots.get(&parent.repo).unwrap_or(root);
                    if let Some(Read::Landed(p, closed)) = read_landed(root, parent) {
                        book.set_landed(&p, closed);
                    }
                }
            }
        }
        book.retain(&open);
        book
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{native_blockers, park_blockers, repo_of_url};
    use crate::eta::dependency::NodeKey;
    use serde_json::json;

    #[test]
    fn park_records_name_same_repo_parents_and_ignore_reason_mentions() {
        let body = "text\n<!-- loom:park Blocked by: #12 by=curator reason=\"after #99\" -->\n\
                    <!-- loom:park Blocked by: #7 -->";
        assert_eq!(
            park_blockers("Owner/Repo", body),
            vec![
                NodeKey::new("owner/repo", 7),
                NodeKey::new("owner/repo", 12)
            ]
        );
        assert!(park_blockers("o/r", "no record").is_empty());
    }

    #[test]
    fn native_blockers_carry_repo_and_closing_time() {
        let page = json!([
            {"number": 5, "repository_url": "https://api.github.com/repos/a/b", "state": "open"},
            {"number": 6, "repository_url": "https://api.github.com/repos/c/d",
             "state": "closed", "closed_at": "2026-10-01T00:00:00Z"},
            {"number": 7}
        ]);
        let got = native_blockers(&page).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (NodeKey::new("a/b", 5), None));
        assert_eq!(got[1].0, NodeKey::new("c/d", 6));
        assert!(got[1].1.is_some());
        assert!(native_blockers(&json!({"message": "Not Found"})).is_none());
        assert_eq!(repo_of_url("https://x/repos/o/r").as_deref(), Some("o/r"));
    }
}
