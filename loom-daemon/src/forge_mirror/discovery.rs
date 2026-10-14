//! Discovery listings from the mirror, with today's path as the fallback.
//!
//! [`list_open`] stands in for one ETag-cached REST listing
//! ([`crate::forge_listing::list_issues_cached_as`] /
//! [`crate::forge_listing::list_issues_cached_paged_as`]): open items carrying
//! one label, PRs included, newest first. It returns `Some(rows)` only when
//! every page the mirror served was [`Freshness::Fresh`] for the caller's
//! bound, and `None` otherwise, which the caller answers by reading exactly as
//! it does today. `None` is also the answer, with no request at all, when:
//!
//! - the read is not [`ReadKind::Discovery`] (poll_gate invariant 3: a read
//!   that gates a claim, a label transition or a merge never comes from the
//!   mirror);
//! - no mirror is live ([`super::active`]);
//! - the repo cannot be named as a `github.com` `owner/name`;
//! - the repo read stale or unknown in the last [`STALE_HOLD`] (released
//!   early when the change stream reports a change for it), or any
//!   mirror request failed in the last [`ERROR_BACKOFF`] (so a down mirror
//!   costs one bounded wait per backoff, never one per listing).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::client::{self, Freshness, ItemsPage, MirrorError};
use super::Active;
use crate::forge_events::poll_gate::ReadKind;
use crate::forge_listing::RestIssue;

/// The discovery loops that read the mirror. A closed set: adding a caller is
/// a reviewed change here, and the source test in `super::tests` pins where
/// each is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// [`crate::work_finder`]'s candidate listings.
    WorkFinder,
    /// The stage-dwell sampler's stage listings.
    StageDwell,
}

impl Caller {
    /// The `caller` label (the same token `loom.forge.calls` uses).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkFinder => "work_finder",
            Self::StageDwell => "stage_dwell",
        }
    }
}

/// How much of the listing the REST path it replaces reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pages {
    /// Every page.
    All,
    /// The first REST page only ([`crate::forge_listing::PER_PAGE`] newest),
    /// so a caller sees the same rows whichever source served it.
    First,
}

/// How a mirror read ended, as the `outcome` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Served from the mirror.
    Hit,
    /// Fallback: the watermark was older than the bound.
    Stale,
    /// Fallback: the mirror cannot vouch for the repo.
    Unknown,
    /// Fallback: a request failed (transport, status, auth, malformed).
    Error,
    /// Fallback without a request: a recent stale/unknown verdict for the
    /// repo, or the error backoff.
    Skipped,
}

impl Outcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }
}

/// A repo that read stale or unknown is not asked again for this long, or
/// until the change stream reports a change for it ([`release_hold`]).
pub const STALE_HOLD: Duration = Duration::from_secs(120);

/// After any failed request, no listing asks the mirror for this long.
pub const ERROR_BACKOFF: Duration = Duration::from_secs(60);

/// Items per mirror page (the mirror's maximum).
pub const PAGE_LIMIT: u32 = 500;

/// Most mirror pages one listing reads before it falls back.
pub const MAX_PAGES: usize = 40;

/// Wall-clock budget for one listing, all pages included.
pub const LISTING_DEADLINE: Duration = Duration::from_secs(10);

/// One mirror request, injected so the decision logic is testable offline.
pub trait Fetch {
    /// `GET path?query` against the mirror.
    ///
    /// # Errors
    ///
    /// [`MirrorError`].
    fn get(
        &mut self,
        path: &str,
        query: &[(String, String)],
        deadline: Duration,
    ) -> Result<serde_json::Value, MirrorError>;
}

/// The live fetcher: [`client::get_json_blocking`].
pub struct HttpFetch<'a>(pub &'a Active);

impl Fetch for HttpFetch<'_> {
    fn get(
        &mut self,
        path: &str,
        query: &[(String, String)],
        deadline: Duration,
    ) -> Result<serde_json::Value, MirrorError> {
        client::get_json_blocking(self.0, path, query, deadline)
    }
}

/// Per-repo holds and the global backoff.
#[derive(Debug, Default)]
pub struct Holds {
    repos: HashMap<String, Instant>,
    backoff_until: Option<Instant>,
}

impl Holds {
    fn blocked(&mut self, repo: &str, now: Instant) -> bool {
        if self.backoff_until.is_some_and(|t| now < t) {
            return true;
        }
        self.repos.retain(|_, until| now < *until);
        self.repos.contains_key(repo)
    }

    fn hold(&mut self, repo: &str, now: Instant) {
        self.repos.insert(repo.to_string(), now + STALE_HOLD);
    }

    fn back_off(&mut self, now: Instant) {
        self.backoff_until = Some(now + ERROR_BACKOFF);
    }
}

static HOLDS: Mutex<Option<Holds>> = Mutex::new(None);

/// The change stream saw `repo` change: its watermark has moved, so the next
/// listing asks the mirror again instead of waiting out [`STALE_HOLD`].
pub fn release_hold(repo: &str) {
    with_holds(|h| {
        h.repos.remove(repo);
    });
}

fn with_holds<T>(f: impl FnOnce(&mut Holds) -> T) -> T {
    let mut guard = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Holds::default))
}

/// The lowercase `owner/name` a listing from `cwd` (or of `repo`) targets,
/// resolved the way [`crate::forge_listing`] resolves it (`repo`, else
/// `LOOM_REPO`, else `cwd`'s `origin`), or `None` when it is not a
/// `github.com` repo the mirror could hold.
#[must_use]
pub fn resolve_repo(cwd: Option<&Path>, repo: Option<&str>) -> Option<String> {
    let env_repo = std::env::var("LOOM_REPO").ok().filter(|r| !r.is_empty());
    let target = crate::forge_etag_store::resolve_target(cwd, repo.or(env_repo.as_deref()));
    if !crate::forge_etag_store::host_scope(&target).eq_ignore_ascii_case("github.com") {
        return None;
    }
    let slug = target.repo?.to_ascii_lowercase();
    valid_slug(&slug).then_some(slug)
}

/// `owner/name` as GitHub allows them.
#[must_use]
pub fn valid_slug(slug: &str) -> bool {
    let Some((owner, name)) = slug.split_once('/') else {
        return false;
    };
    crate::forge_bucket_book::valid_owner(owner)
        && !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Open items carrying `label` from the mirror, or `None` for today's path.
/// See the module doc.
#[must_use]
pub fn list_open(
    kind: ReadKind,
    caller: Caller,
    cwd: Option<&Path>,
    repo: Option<&str>,
    label: &str,
    pages: Pages,
) -> Option<Vec<RestIssue>> {
    // Invariant 3 first: nothing below runs for a read that gates a write.
    if kind != ReadKind::Discovery {
        return None;
    }
    let active = super::active()?;
    let slug = resolve_repo(cwd, repo)?;
    let mut fetch = HttpFetch(&active);
    let now = Instant::now();
    let (rows, outcome) = if with_holds(|h| h.blocked(&slug, now)) {
        (None, Outcome::Skipped)
    } else {
        let r = list_open_via(kind, &active, &mut fetch, &slug, label, pages, Utc::now());
        with_holds(|h| match r.1 {
            Outcome::Stale | Outcome::Unknown => h.hold(&slug, now),
            Outcome::Error => h.back_off(now),
            Outcome::Hit | Outcome::Skipped => {}
        });
        r
    };
    crate::observability::ops::forge_mirror::record_read(caller.as_str(), &slug, outcome);
    rows
}

/// [`list_open`]'s decision, with the mirror, the fetcher and the clock
/// given. `None` (with why) unless every page was fresh.
#[must_use]
pub fn list_open_via(
    kind: ReadKind,
    active: &Active,
    fetch: &mut dyn Fetch,
    slug: &str,
    label: &str,
    pages: Pages,
    now: DateTime<Utc>,
) -> (Option<Vec<RestIssue>>, Outcome) {
    if kind != ReadKind::Discovery {
        return (None, Outcome::Skipped);
    }
    let started = Instant::now();
    let mut items = Vec::new();
    let mut after = 0u64;
    for _ in 0..MAX_PAGES {
        let Some(left) = LISTING_DEADLINE.checked_sub(started.elapsed()) else {
            return (None, Outcome::Error);
        };
        let query = vec![
            ("repo".to_string(), slug.to_string()),
            ("type".to_string(), "all".to_string()),
            ("state".to_string(), "open".to_string()),
            ("label".to_string(), label.to_string()),
            ("include".to_string(), "body".to_string()),
            ("limit".to_string(), PAGE_LIMIT.to_string()),
            ("after".to_string(), after.to_string()),
        ];
        let page: ItemsPage = match fetch
            .get("/v1/mirror/items", &query, left.min(client::REQUEST_TIMEOUT))
            .and_then(client::decode)
        {
            Ok(p) => p,
            Err(e) => {
                log::debug!("forge_mirror: {slug} {label} listing failed ({e}); reading the forge");
                return (None, Outcome::Error);
            }
        };
        match page.watermark.freshness(slug, active.max_staleness, now) {
            Freshness::Fresh => {}
            Freshness::Stale => return (None, Outcome::Stale),
            Freshness::Unknown => return (None, Outcome::Unknown),
        }
        items.extend(page.items);
        match page.next_after {
            Some(n) if n > after => after = n,
            Some(_) => return (None, Outcome::Error), // a cursor that does not advance
            None => return (Some(rows_of(items, label, pages)), Outcome::Hit),
        }
    }
    (None, Outcome::Error)
}

/// The REST rows for `items`: open, carrying `label` (the mirror filtered;
/// this re-checks), each number once, newest first like the REST listing,
/// and only its first page for [`Pages::First`].
fn rows_of(items: Vec<client::MirrorItem>, label: &str, pages: Pages) -> Vec<RestIssue> {
    let mut rows: Vec<RestIssue> = items
        .iter()
        .filter_map(client::MirrorItem::to_rest)
        .filter(|r| {
            r.state.eq_ignore_ascii_case("open")
                && r.labels.iter().any(|l| l.eq_ignore_ascii_case(label))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.number));
    rows.dedup_by_key(|r| r.number);
    if pages == Pages::First {
        rows.truncate(crate::forge_listing::PER_PAGE);
    }
    rows
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    #[test]
    fn a_hold_blocks_one_repo_and_a_backoff_blocks_all_until_they_lapse() {
        let mut h = Holds::default();
        let t0 = Instant::now();
        assert!(!h.blocked("o/a", t0));
        h.hold("o/a", t0);
        assert!(h.blocked("o/a", t0 + Duration::from_secs(1)));
        assert!(!h.blocked("o/b", t0 + Duration::from_secs(1)));
        assert!(!h.blocked("o/a", t0 + STALE_HOLD), "the hold lapses");
        h.hold("o/a", t0);
        h.repos.remove("o/a"); // what release_hold does
        assert!(!h.blocked("o/a", t0));
        h.back_off(t0);
        assert!(h.blocked("o/b", t0 + Duration::from_secs(1)));
        assert!(!h.blocked("o/b", t0 + ERROR_BACKOFF));
    }

    #[test]
    fn only_github_com_slugs_are_mirror_repos() {
        assert!(valid_slug("acme/widgets"));
        assert!(valid_slug("acme/w.i_d-gets"));
        for bad in [
            "acme", "acme/", "/w", "acme/..", "acme/a b", "-x/y", "a/b/c",
        ] {
            assert!(!valid_slug(bad), "{bad}");
        }
        assert_eq!(resolve_repo(None, Some("Acme/Widgets")).as_deref(), Some("acme/widgets"));
    }
}
