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
//!
//! # Reuse of the last good listing (#12025)
//!
//! [`probe_many`] alone (the work finder's pre-filter) may stand in the last
//! listing it read whole for a failed read, for up to [`LISTING_REUSE_FOR`],
//! and says so ([`ManyVerdicts::reused`]). The pre-filter fails closed on an
//! unreadable listing, so without this one failed page would hold a whole
//! workspace's dispatch for the tick. [`probe`] never reuses: its `NoneOpen`
//! is the dispatch guard's permit, and a stale permit is a guess.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::claim_reconciliation::open_pr_listing::MAX_PAGES;
use crate::comment_trust::TrustPolicy;
use crate::forge_pull_listing::{list_open_pulls_cached_within, RestPull};

use super::gh::{linkage_phrase_regex, LinkageKind, OpenPrProbe};
use super::naming::branch_name;

/// Leg 0 for `issue` in `owner_repo` (`owner/name`): `Some(Open(pr))` /
/// `Some(NoneOpen)` from a listing that was read, `None` when it could not be
/// read (the caller then runs its GraphQL-then-timeline fallback).
///
/// The listing is always read for `owner_repo` — the repo the caller resolved
/// and asks its fallback legs about — never re-resolved from `root`'s
/// `origin` or `LOOM_REPO`. In a fork clone (or after `gh repo set-default`)
/// `origin` names a different repo than the closes-graph is asked about, and
/// a successful read of the fork's (empty) listing would be a false "no PR"
/// that also skips the fallback (#10514 review). For the usual clone, where
/// `origin` is that same repo on github.com, the cache key and URL equal the
/// claim-reconciliation passes', so the ETag is still shared. The
/// `worktree_ops` probe's repo additionally never comes from `LOOM_REPO`
/// (#5511). `timeout` bounds each page read (`None` = the conditional-read
/// default); the registry guard passes its `reap_gh_timeout`, like its other
/// legs.
pub(crate) fn probe(
    caller: &'static str,
    gh_bin: &Path,
    root: &Path,
    (owner_repo, issue): (&str, u32),
    timeout: Option<Duration>,
) -> Option<OpenPrProbe> {
    let rows = read_rows(caller, gh_bin, root, owner_repo, timeout)?;
    let policy = TrustPolicy::for_root(root);
    match classify_open_linked_pr_rows(&rows, issue, owner_repo, &policy) {
        OpenPrProbe::ProbeFailed => None,
        verdict => Some(verdict),
    }
}

/// [`probe_many`]'s answer: the verified verdict of each asked issue, and
/// whether a recent listing stood in for a failed read (#12025).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManyVerdicts {
    /// `Open(pr)` or `NoneOpen` per asked issue the classifier answered.
    pub(crate) verdicts: Vec<(u32, OpenPrProbe)>,
    /// `None` for a listing read just now. `Some(age)` when the read failed
    /// and the last good listing of this root and repo, read `age` ago (at
    /// most [`LISTING_REUSE_FOR`]), answered instead.
    pub(crate) reused: Option<Duration>,
}

/// [`probe`] for many issues from ONE listing read (#8865): the verified
/// verdict of each issue in `issues`, in that order. `None` when the listing
/// could not be read and no good listing of the last [`LISTING_REUSE_FOR`]
/// can stand in (#12025), which is no verdict for any of them.
///
/// The read, the repo, the classifier and the trust rule are [`probe`]'s, so
/// an issue's verdict here is exactly what [`probe`] over the same rows would
/// answer for it: `Open(pr)` or `NoneOpen`. An issue the classifier could not
/// answer for (`ProbeFailed`) is left out, so every pair returned is a
/// verified one. `NoneOpen` is the same verified absence the 2.6 guard's leg
/// 0 acts on; the work finder's pre-filter, the one caller, uses it to
/// invalidate the #6788 memo (fresh reads only) and to let a candidate go on
/// to `dispatch()`, whose own guard still decides.
pub(crate) fn probe_many(
    caller: &'static str,
    gh_bin: &Path,
    root: &Path,
    (owner_repo, issues): (&str, &[u32]),
    timeout: Option<Duration>,
) -> Option<ManyVerdicts> {
    let (rows, reused) = match read_rows(caller, gh_bin, root, owner_repo, timeout) {
        Some(rows) => {
            let rows = Arc::new(rows);
            remember_good(root, owner_repo, Arc::clone(&rows));
            (rows, None)
        }
        None => {
            let (age, rows) = recent_good(root, owner_repo, Instant::now())?;
            (rows, Some(age))
        }
    };
    let policy = TrustPolicy::for_root(root);
    let verified = issues.iter().filter_map(|&issue| {
        match classify_open_linked_pr_rows(&rows, issue, owner_repo, &policy) {
            OpenPrProbe::ProbeFailed => None,
            verdict => Some((issue, verdict)),
        }
    });
    Some(ManyVerdicts {
        verdicts: verified.collect(),
        reused,
    })
}

/// How long [`probe_many`] may answer from the last good listing of a root
/// after a failed read (#12025). Short: an `Open` verdict it reuses only
/// removes a candidate, but a reused `NoneOpen` lets one reach `dispatch()`,
/// whose own guard is then the only check of a PR opened since.
pub(crate) const LISTING_REUSE_FOR: Duration = Duration::from_secs(300);

/// The last listing [`probe_many`] read whole, per root and repo, and when.
type GoodListings = HashMap<(PathBuf, String), (Instant, Arc<Vec<RestPull>>)>;

fn good_listings() -> &'static Mutex<GoodListings> {
    static GOOD: OnceLock<Mutex<GoodListings>> = OnceLock::new();
    GOOD.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record `rows` as the last good listing of `owner_repo` at `root`.
fn remember_good(root: &Path, owner_repo: &str, rows: Arc<Vec<RestPull>>) {
    let mut good = good_listings().lock().unwrap_or_else(|p| p.into_inner());
    good.insert((root.to_path_buf(), owner_repo.to_string()), (Instant::now(), rows));
}

/// The last good listing of `owner_repo` at `root` and its age at `now`, if
/// it is at most [`LISTING_REUSE_FOR`] old. An older one is dropped.
fn recent_good(
    root: &Path,
    owner_repo: &str,
    now: Instant,
) -> Option<(Duration, Arc<Vec<RestPull>>)> {
    let mut good = good_listings().lock().unwrap_or_else(|p| p.into_inner());
    let key = (root.to_path_buf(), owner_repo.to_string());
    let (at, rows) = good.get(&key)?;
    let age = now.saturating_duration_since(*at);
    if age > LISTING_REUSE_FOR {
        good.remove(&key);
        return None;
    }
    Some((age, Arc::clone(rows)))
}

/// Test hook: make the last good listing of `owner_repo` at `root` `by`
/// older, as if it had been read that much earlier.
#[cfg(test)]
pub(crate) fn age_good_listing(root: &Path, owner_repo: &str, by: Duration) {
    let mut good = good_listings().lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, _)) = good.get_mut(&(root.to_path_buf(), owner_repo.to_string())) {
        *at = at.checked_sub(by).unwrap_or(*at);
    }
}

/// The open-PR listing of `owner_repo`, or `None` when it could not be read
/// (or failed for `root` less than [`LISTING_RETRY_AFTER`] ago).
fn read_rows(
    caller: &'static str,
    gh_bin: &Path,
    root: &Path,
    owner_repo: &str,
    timeout: Option<Duration>,
) -> Option<Vec<RestPull>> {
    if listing_down(root) {
        return None;
    }
    let listed = list_open_pulls_cached_within(
        caller,
        gh_bin,
        Some(root),
        Some(owner_repo),
        MAX_PAGES,
        timeout,
    );
    mark_listing(root, listed.is_ok());
    match listed {
        Ok(rows) => Some(rows),
        Err(e) => {
            log::debug!(
                "{caller}: open-PR listing of {owner_repo} unavailable ({e:#}); no listing \
                 verdict for {}s (#10514)",
                LISTING_RETRY_AFTER.as_secs()
            );
            None
        }
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

/// Every issue one open PR links to under [`classify_open_linked_pr_rows`]'s
/// rules, by issue number, with how it links: the issues its body names with
/// a linking phrase ([`super::gh::linkage_refs`], the same phrase families as
/// [`linkage_phrase_regex`]), plus the Builder branch's issue when the head is
/// in this same repo. The branch counts only when [`branch_name`] reproduces
/// it exactly (`feature/issue-0123` names no issue), and it links as
/// [`LinkageKind::Closes`]: it is that issue's Builder PR. Nothing for a fork
/// PR by a known-untrusted author (H14). The step engine's item join (#11895,
/// design #11894) reads it, so the join and the open-PR guard cannot disagree
/// on what links.
#[must_use]
pub(crate) fn linked_issues(
    body: Option<&str>,
    head_ref: Option<&str>,
    same_repo: bool,
    known_untrusted: bool,
) -> std::collections::BTreeMap<u32, LinkageKind> {
    let mut linked = std::collections::BTreeMap::new();
    if !same_repo && known_untrusted {
        return linked;
    }
    linked.extend(body.map(super::gh::linkage_refs).unwrap_or_default());
    let branch_issue =
        head_ref.and_then(|h| super::naming::issue_from_branch(h).filter(|&n| branch_name(n) == h));
    if let (true, Some(n)) = (same_repo, branch_issue) {
        linked.insert(n, LinkageKind::Closes);
    }
    linked
}

/// The two trust facts [`linked_issues`] reads, from a listing row of
/// `owner_repo`: `(same_repo, known_untrusted)`.
#[must_use]
pub(crate) fn link_facts(row: &RestPull, owner_repo: &str, policy: &TrustPolicy) -> (bool, bool) {
    (is_same_repo(row, owner_repo), policy.known_untrusted(&author_json(row)))
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
