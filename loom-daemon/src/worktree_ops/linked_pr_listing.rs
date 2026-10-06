//! Leg 0 of both open-linked-PR probes (#10514): answer "is there an open PR
//! for issue N?" from the ETag-cached REST open-PR listing instead of the
//! GraphQL closes-graph plus a paginated REST timeline walk.
//!
//! Both copies of the probe — `sweep_registry::guards::SweepRegistry::
//! probe_open_linked_pr` (dispatch guard, no-progress predicate, crash-resume)
//! and [`super::gh::probe_open_linked_pr`] (`forge check-claim`,
//! `check-open-pr`, `check-branch`, orphan recovery) — call [`probe`] first and
//! keep their old GraphQL-then-timeline union only as the fallback for a
//! listing that could not be read. One classifier serves both, so they cannot
//! drift apart (#8116).
//!
//! The listing is [`crate::forge_pull_listing::list_open_pulls_cached_as`],
//! read with the same page budget and URL as the claim-reconciliation passes
//! ([`crate::claim_reconciliation::open_pr_listing`]), so on an unchanged repo
//! every probe is a free `304` against the ETag those passes already hold.
//!
//! # Matching
//!
//! An open PR is linked to issue `N` when it survives the H14 trust rule
//! (#9548: drop a FORK PR whose author is known and untrusted) and either
//!
//! - its body carries a #6216 linking phrase for `#N`
//!   ([`super::gh::linkage_phrase_regex`]: `Closes/Fixes/Resolves #N`, `Part
//!   of #N`, `Contributes to #N`) — the filter the timeline leg already applied
//!   to the same body, so a bare `#N` mention still does not count (#8940); or
//! - its head is the Builder branch `feature/issue-N` in this same repo. This
//!   is a deliberate widening over the closes-graph/timeline union: a Builder
//!   PR whose body lost its `Closes #N` still holds the guard.
//!
//! # Coverage the listing does not have
//!
//! The closes-graph also sees Development-sidebar manual links, `Closes
//! owner/repo#N` / URL-form closing references, and cross-repo closing PRs.
//! The listing sees none of them. The fleet always writes `Closes #N` in the
//! PR body, and the timeline leg already missed all three, so this is
//! accepted rather than paid for with a GraphQL call on every probe.
//!
//! # Failure
//!
//! Only a successfully read listing is a verdict. A failed read (forge error,
//! breaker, more than [`MAX_PAGES`] pages, a listing that moved mid-walk) is
//! `None`, and the caller falls back to the legacy union — never a "no PR"
//! (#7863). After a failed read the listing is skipped for that root for
//! [`LISTING_RETRY_AFTER`], so an outage or a wedged `gh` costs one extra
//! bounded call, not one per probe on top of the fallback's own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::claim_reconciliation::open_pr_listing::MAX_PAGES;
use crate::comment_trust::TrustPolicy;
use crate::forge_pull_listing::{list_open_pulls_cached_within, RestPull};

use super::gh::{linkage_phrase_regex, OpenPrProbe};
use super::naming::branch_name;

/// Leg 0 for `issue` in `owner_repo` (`owner/name`): `Some(Open(pr))` /
/// `Some(NoneOpen)` from a listing that was read, `None` when it could not be
/// read (the caller then runs its GraphQL-then-timeline fallback).
///
/// `repo_override` is passed through to the listing: the registry guard
/// passes `None` (the listing then resolves `LOOM_REPO` / `root`'s remote
/// exactly like the reconciliation passes, sharing their ETag); the
/// `worktree_ops` probe passes its own resolved repo because it must never
/// answer from a `LOOM_REPO` that names a different repo (#5511).
/// `timeout` bounds each page read (`None` = the conditional-read default);
/// the registry guard passes its `reap_gh_timeout`, like its other legs.
pub(crate) fn probe(
    caller: &'static str,
    gh_bin: &Path,
    root: &Path,
    repo_override: Option<&str>,
    (owner_repo, issue): (&str, u32),
    timeout: Option<Duration>,
) -> Option<OpenPrProbe> {
    if listing_down(root) {
        return None;
    }
    let listed = list_open_pulls_cached_within(
        caller,
        gh_bin,
        Some(root),
        repo_override,
        MAX_PAGES,
        timeout,
    );
    mark_listing(root, listed.is_ok());
    let rows = match listed {
        Ok(rows) => rows,
        Err(e) => {
            log::debug!(
                "issue #{issue}: open-PR listing unavailable ({e:#}); falling back to the \
                 closes-graph + timeline union for {}s (#10514)",
                LISTING_RETRY_AFTER.as_secs()
            );
            return None;
        }
    };
    let policy = TrustPolicy::for_root(root);
    match classify_open_linked_pr_rows(&rows, issue, owner_repo, &policy) {
        OpenPrProbe::ProbeFailed => None,
        verdict => Some(verdict),
    }
}

/// How long a failed listing read keeps leg 0 off for its root.
pub(crate) const LISTING_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Roots whose last listing read failed, and when.
fn failed_reads() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    static FAILED: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();
    FAILED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The listing failed for `root` less than [`LISTING_RETRY_AFTER`] ago.
fn listing_down(root: &Path) -> bool {
    let guard = failed_reads().lock().unwrap_or_else(|p| p.into_inner());
    guard
        .get(root)
        .is_some_and(|at| at.elapsed() < LISTING_RETRY_AFTER)
}

/// Record the outcome of a listing read for `root`.
fn mark_listing(root: &Path, ok: bool) {
    let mut guard = failed_reads().lock().unwrap_or_else(|p| p.into_inner());
    if ok {
        guard.remove(root);
    } else {
        guard.insert(root.to_path_buf(), Instant::now());
    }
}

/// The verdict for `issue` over the open-PR listing `rows` of `owner_repo`:
/// the lowest-numbered linked PR (see the module docs for what links), else
/// [`OpenPrProbe::NoneOpen`]. [`OpenPrProbe::ProbeFailed`] only when the
/// phrase regex cannot be built.
#[must_use]
pub(crate) fn classify_open_linked_pr_rows(
    rows: &[RestPull],
    issue: u32,
    owner_repo: &str,
    policy: &TrustPolicy,
) -> OpenPrProbe {
    let Some(phrase) = linkage_phrase_regex(issue) else {
        return OpenPrProbe::ProbeFailed;
    };
    let branch = branch_name(issue);
    let linked = rows
        .iter()
        .filter(|row| row.state.eq_ignore_ascii_case("open"))
        .filter(|row| {
            let same_repo = is_same_repo(row, owner_repo);
            if !same_repo && policy.known_untrusted(&author_json(row)) {
                return false;
            }
            let by_body = row.body.as_deref().is_some_and(|b| phrase.is_match(b));
            let by_branch = same_repo && row.head_ref.as_deref() == Some(branch.as_str());
            by_body || by_branch
        })
        .map(|row| row.number)
        .min();
    linked.map_or(OpenPrProbe::NoneOpen, OpenPrProbe::Open)
}

/// The PR's head lives in `owner_repo` itself. An absent head repo (GitHub
/// omits it for a deleted fork) is not known to be same-repo.
fn is_same_repo(row: &RestPull, owner_repo: &str) -> bool {
    row.head_repo
        .as_deref()
        .is_some_and(|r| r.eq_ignore_ascii_case(owner_repo))
}

/// The row's author in the REST shape [`TrustPolicy::known_untrusted`] reads.
/// Absent fields stay absent, so a row with no author is *unknown* (counted),
/// never untrusted — the same rule as the closes-graph leg.
fn author_json(row: &RestPull) -> serde_json::Value {
    let mut v = serde_json::Map::new();
    if let Some(login) = &row.author {
        let kind = if row.author_is_bot { "Bot" } else { "User" };
        v.insert("user".to_string(), serde_json::json!({ "login": login, "type": kind }));
    }
    if let Some(assoc) = &row.author_association {
        v.insert("author_association".to_string(), serde_json::json!(assoc));
    }
    serde_json::Value::Object(v)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "linked_pr_listing_tests.rs"]
mod tests;
