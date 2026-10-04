//! The forge reads behind the ETA queue-friction features (#10193); the
//! parsing and the point-in-time rules are [`crate::eta::friction`]'s.
//!
//! Per pass, before the pass's `as_of` is taken (so every reading is
//! knowable at the estimates it feeds):
//!
//! - **Repo** (each repo with a tracked item, at most every
//!   [`REPO_REFRESH_SECS`]): `pulls?state=open` for the open-PR count and
//!   `actions/runs` for the typical CI duration. The `pr-open-skip` lockout
//!   comes from the work finder's last tick, no read.
//! - **PR** (each PR under a review label, at most every
//!   [`PR_REFRESH_SECS`], at most [`PR_READ_BUDGET`] per pass, never-read and
//!   oldest first): `pulls/{n}` for mergeability and the head SHA, then
//!   `commits/{sha}/check-runs` for CI.
//!
//! Every read is a conditional (`If-None-Match`) request against the shared
//! ETag store, so an unchanged answer is a `304`. A failed read is a reading
//! too: its features are omitted with `read_failed`, and the next due pass
//! retries.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::eta::friction::{
    ci_status, open_pr_count, pr_mergeability, typical_ci_duration, FrictionBook, PrFriction,
    Reading, RepoFriction,
};
use crate::forge_call_stats::{ops, ForgeOp};
use crate::forge_etag_store as store;
use crate::types::QueueDisposition;

/// Seconds between repo-level reads of one repo.
pub const REPO_REFRESH_SECS: i64 = 900;

/// Seconds between reads of one PR.
pub const PR_REFRESH_SECS: i64 = 600;

/// PRs read per pass, across all repos (two calls each).
pub const PR_READ_BUDGET: usize = 12;

const CALLER: &str = "eta_friction";

/// Accounting rows (#9831) for the reads the inventory has no row for yet.
const OPEN_PRS: ForgeOp = ForgeOp::uninventoried("open-PR listing has no inventory row");
const CHECK_RUNS: ForgeOp = ForgeOp::uninventoried("check-runs read has no inventory row");
const PR_RUNS: ForgeOp =
    ForgeOp::uninventoried("repo PR workflow-runs listing has no inventory row");

/// One conditional GET of `url` against the ETag store, as JSON, recorded
/// as `op`. `None` on any failure.
fn cached_get(root: &Path, slug: &str, url: &str, op: ForgeOp) -> Option<Value> {
    let gh = PathBuf::from(crate::gh_invocation::gh_bin());
    let target = store::resolve_target(Some(root), Some(slug));
    let path = store::disk_cache_path(&store::cache_key(Some(root), &target, url));
    let prior = store::read_disk_entry(&path);
    let etag = prior.as_ref().map(|e| e.etag.as_str());
    let (_, response, _) = store::fetch_conditional(
        store::ConditionalRead::new(CALLER, op),
        &gh,
        Some(root),
        &target,
        url,
        etag,
    )
    .ok()?;
    let response = response?;
    match response.status {
        304 => serde_json::from_str(&prior?.body).ok(),
        200 => {
            let value = serde_json::from_str(&response.body).ok()?;
            if let Some(etag) = response.etag {
                let entry = store::DiskEntry {
                    etag,
                    body: response.body,
                };
                store::write_disk_entry(&path, &entry);
            }
            Some(value)
        }
        _ => None,
    }
}

fn read<T>(value: Option<&Value>, parse: impl Fn(&Value) -> Reading<T>) -> Reading<T> {
    value.map_or(Err("read_failed"), parse)
}

/// Read one repo's friction; `lockout` comes from the work finder's tick.
fn read_repo(root: &Path, slug: &str, lockout: Reading<bool>) -> RepoFriction {
    let open =
        cached_get(root, slug, &format!("repos/{slug}/pulls?state=open&per_page=100"), OPEN_PRS);
    let runs = cached_get(
        root,
        slug,
        &format!("repos/{slug}/actions/runs?status=completed&event=pull_request&per_page=50"),
        PR_RUNS,
    );
    let observed_at = Utc::now();
    RepoFriction {
        observed_at,
        open_prs: read(open.as_ref(), open_pr_count),
        lockout,
        ci_typical_sec: read(runs.as_ref(), |page| typical_ci_duration(page, observed_at)),
    }
}

/// Read one PR's friction.
fn read_pr(root: &Path, slug: &str, number: u32) -> PrFriction {
    let pull = cached_get(root, slug, &format!("repos/{slug}/pulls/{number}"), ops::PR_VIEW_STATE);
    let (behind, conflict) = pull
        .as_ref()
        .map_or((Err("read_failed"), Err("read_failed")), pr_mergeability);
    let ci = match pull.as_ref().and_then(|p| p["head"]["sha"].as_str()) {
        Some(sha) => {
            let url = format!("repos/{slug}/commits/{sha}/check-runs?per_page=100");
            read(cached_get(root, slug, &url, CHECK_RUNS).as_ref(), ci_status)
        }
        None => Err("read_failed"),
    };
    PrFriction {
        observed_at: Utc::now(),
        ci,
        behind,
        conflict,
    }
}

/// Each repo's `pr-open-skip` lockout on the work finder's last tick: locked
/// when any ready row of it was refused by the open-PR guard. A repo whose
/// ready listing failed on that tick is unknown, never "unlocked".
async fn lockouts(slug_cache: &mut HashMap<String, String>) -> Option<Tick> {
    let summary = crate::work_finder::last_tick_summary()?;
    let mut locked: BTreeMap<String, bool> = BTreeMap::new();
    for row in &summary.queue {
        let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, &row.repo).await
        else {
            continue;
        };
        *locked.entry(slug.to_ascii_lowercase()).or_default() |=
            row.disposition == QueueDisposition::OpenPr;
    }
    let mut failed = BTreeSet::new();
    for root in &summary.listing_failed {
        if let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, root).await {
            failed.insert(slug.to_ascii_lowercase());
        }
    }
    Some((locked, failed))
}

/// One work-finder tick's lockouts: locked per repo, and the repos whose
/// ready listing failed (both lowercased slugs).
type Tick = (BTreeMap<String, bool>, BTreeSet<String>);

/// Repo `key`'s lockout reading from `tick`. A repo with no ready row on the
/// tick is `nothing_ready`, not "unlocked": the open-PR guard had nothing to
/// refuse, so the tick says nothing about it.
fn lockout_reading(tick: Option<&Tick>, key: &str) -> Reading<bool> {
    match tick {
        None => Err("no_work_finder_tick"),
        Some((_, failed)) if failed.contains(key) => Err("listing_failed"),
        Some((locked, _)) => locked.get(key).copied().ok_or("nothing_ready"),
    }
}

/// Refresh the due part of `book` for `repos` — `(root, slug, open PRs under
/// review)` — and the repos of tracked items, `tracked`. Blocking reads run
/// off the async workers.
pub(super) async fn refresh(
    book: FrictionBook,
    repos: Vec<(PathBuf, String, Vec<u32>)>,
    tracked: BTreeSet<String>,
    slug_cache: &mut HashMap<String, String>,
) -> FrictionBook {
    let tick = lockouts(slug_cache).await;
    let at: DateTime<Utc> = Utc::now();
    let candidates: Vec<String> = repos
        .iter()
        .filter(|(_, slug, prs)| !prs.is_empty() || tracked.contains(&slug.to_ascii_lowercase()))
        .map(|(_, slug, _)| slug.clone())
        .collect();
    let due_repos = book.due_repos(&candidates, at, REPO_REFRESH_SECS);
    let pr_candidates: Vec<(String, u32)> = repos
        .iter()
        .flat_map(|(_, slug, prs)| prs.iter().map(move |n| (slug.clone(), *n)))
        .collect();
    let due_prs = book.due_prs(&pr_candidates, at, PR_REFRESH_SECS, PR_READ_BUDGET);
    let roots: BTreeMap<String, PathBuf> = repos
        .iter()
        .map(|(root, slug, _)| (slug.clone(), root.clone()))
        .collect();
    let open: Vec<(String, Vec<u32>)> = repos
        .into_iter()
        .map(|(_, slug, prs)| (slug, prs))
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut book = book;
        for slug in due_repos {
            let key = slug.to_ascii_lowercase();
            let lockout = lockout_reading(tick.as_ref(), &key);
            let friction = read_repo(&roots[&slug], &slug, lockout);
            book.set_repo(&slug, friction);
        }
        for (slug, number) in due_prs {
            let friction = read_pr(&roots[&slug], &slug, number);
            book.set_pr(&slug, number, friction);
        }
        for (slug, prs) in &open {
            book.retain_prs(slug, prs);
        }
        book
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{lockout_reading, Tick};

    #[test]
    fn lockout_reading_tells_unlocked_from_nothing_ready() {
        let tick: Tick = (
            [("a/locked".to_string(), true), ("a/open".to_string(), false)].into(),
            ["a/failed".to_string()].into(),
        );
        assert_eq!(lockout_reading(None, "a/open"), Err("no_work_finder_tick"));
        assert_eq!(lockout_reading(Some(&tick), "a/locked"), Ok(true));
        assert_eq!(lockout_reading(Some(&tick), "a/open"), Ok(false));
        assert_eq!(lockout_reading(Some(&tick), "a/failed"), Err("listing_failed"));
        assert_eq!(lockout_reading(Some(&tick), "a/idle"), Err("nothing_ready"));
    }
}
