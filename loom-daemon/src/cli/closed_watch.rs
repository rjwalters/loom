//! Daemon-side "recently closed" poll that drives the cleared-blocker
//! re-check on ANY issue/PR close (issue #10150).
//!
//! `notify-cleared-blockers` (#9102) only ran from `merge-pr.sh`, so a merge
//! made in the GitHub UI, with the gh CLI, or by hand never told the
//! `loom:blocked` artifacts citing it. This module lists what closed since a
//! persisted cursor and feeds that batch to the same
//! [`scan_cleared`](super::notify_cleared_blockers::scan_cleared) core.
//!
//! * REST only (`gh api repos/.../issues?state=closed&since=...`; the endpoint
//!   returns issues and PRs together), so it works under GraphQL exhaustion.
//! * One ordering key for everything: `(updated_at, number)`. The listing is
//!   sorted by `updated_at`, the [`Cursor`] is the highest key processed (the
//!   max `updated_at` plus the numbers already taken AT that second), and an
//!   item is in the batch iff its key is past the cursor. `closed_at` is
//!   deliberately not a filter: a capped listing ordered by `updated_at`
//!   cannot be cut by `closed_at` without dropping an unseen close whose
//!   later update pushed it past the cap (#10638, Judge P1 on #10180). The
//!   cost is that a comment on an old closed item rescans its number; the
//!   per-number `<!-- loom:blocker-cleared:#N -->` marker makes that rescan
//!   post nothing it already posted.
//! * Keyset paging, so truncation and reordering lose nothing: each request
//!   re-anchors `since` at the cursor (less 1s, so an exclusive `since` also
//!   works) instead of walking page numbers over a list that moves under
//!   updates; page numbers only advance inside one same-second run. A pass
//!   takes at most [`MAX_PAGES`] pages of new rows (and [`MAX_REQUESTS`]
//!   requests); whatever it did not reach is still past the cursor next tick.
//! * The first run looks back [`LOOKBACK_HOURS`] only.
//! * The cursor advances only after a successful scan, and only to the key of
//!   what was scanned, so a failed tick retries. "Successful" means every
//!   required read answered: the listing, each merged PR's closing
//!   references, the `loom:blocked` listing, every candidate's text/evidence
//!   read, and every owed comment post. Any one unanswered holds the cursor;
//!   the marker keeps the retry from re-posting what this pass already
//!   posted. With nothing past the cursor there is no `loom:blocked` read.
//! * Two hosts polling one repo can both post in the check-then-post window;
//!   a duplicate notice is harmless and accepted by design.
//! * Never edits a label; every failure is logged, never fatal.
//!
//! Config: `autonomous.closedWatch.{enabled,intervalSecs}`, env
//! `LOOM_CLOSED_WATCH` / `LOOM_CLOSED_WATCH_INTERVAL_SECS` (env > config >
//! default; default off).

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use serde::Deserialize;

use loom_daemon::cmd_out::CmdOutcome;
use loom_daemon::script_helpers::run_gh;

use super::notify_cleared_blockers::{pr_close_targets, scan_cleared, ScanOptions, ScanReport};
use super::stale_blocked::DEFAULT_LIMIT;

pub(crate) const ENABLE_ENV: &str = "LOOM_CLOSED_WATCH";
pub(crate) const INTERVAL_ENV: &str = "LOOM_CLOSED_WATCH_INTERVAL_SECS";
pub(crate) const DEFAULT_INTERVAL_SECS: u64 = 300;
/// First-run lookback.
pub(crate) const LOOKBACK_HOURS: i64 = 24;
const PAGE_SIZE: usize = 100;
/// Bound on pages carrying new rows taken in one pass.
const MAX_PAGES: usize = 5;
/// Hard bound on requests in one pass, counting the pages that only re-read
/// rows already at the cursor (a same-second run longer than a page).
const MAX_REQUESTS: usize = MAX_PAGES * 4;
const CURSOR_FILE: &str = "closed-watch-cursor.json";

/// `autonomous.closedWatch` as read from config.
#[derive(Debug, Default, Clone)]
pub(crate) struct ClosedWatchConfig {
    pub enabled: Option<bool>,
    pub interval_secs: Option<u64>,
}

pub(crate) fn read_config(root: &Path) -> ClosedWatchConfig {
    let config = loom_daemon::config_resolver::resolve_effective_config(root);
    let Some(block) = loom_daemon::config_resolver::get_path(&config, "autonomous.closedWatch")
    else {
        return ClosedWatchConfig::default();
    };
    ClosedWatchConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0),
    }
}

/// env > config > default (off).
pub(crate) fn resolve_enabled(config: &ClosedWatchConfig) -> bool {
    resolve_enabled_with(std::env::var(ENABLE_ENV).ok().as_deref(), config)
}

fn resolve_enabled_with(env: Option<&str>, config: &ClosedWatchConfig) -> bool {
    if let Some(v) = env {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(false)
}

pub(crate) fn resolve_interval(config: &ClosedWatchConfig) -> Duration {
    let env = std::env::var(INTERVAL_ENV).ok();
    Duration::from_secs(resolve_interval_secs(env.as_deref(), config))
}

fn resolve_interval_secs(env: Option<&str>, config: &ClosedWatchConfig) -> u64 {
    env.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .or(config.interval_secs)
        .unwrap_or(DEFAULT_INTERVAL_SECS)
}

/// One closed issue/PR from the REST listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosedItem {
    pub number: i64,
    pub updated_at: DateTime<Utc>,
    pub merged_pr: bool,
}

#[derive(Deserialize)]
struct RawItem {
    number: i64,
    #[serde(default)]
    closed_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    pull_request: Option<RawPr>,
}

#[derive(Deserialize)]
struct RawPr {
    #[serde(default)]
    merged_at: Option<String>,
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| format!("unreadable timestamp {s:?}: {e}"))
}

/// Parse one REST page. Pure. Items with no `closed_at` are dropped; an
/// unreadable `updated_at` fails the page (the cursor must not guess).
pub(crate) fn parse_page(stdout: &[u8]) -> Result<Vec<ClosedItem>, String> {
    let raw: Vec<RawItem> =
        serde_json::from_slice(stdout).map_err(|e| format!("unreadable closed-items JSON: {e}"))?;
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let Some(closed_at) = r.closed_at else {
            continue;
        };
        out.push(ClosedItem {
            number: r.number,
            updated_at: parse_ts(r.updated_at.as_deref().unwrap_or(&closed_at))?,
            merged_pr: r.pull_request.is_some_and(|p| p.merged_at.is_some()),
        });
    }
    Ok(out)
}

fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn cursor_path(root: &Path) -> PathBuf {
    root.join(".loom").join(CURSOR_FILE)
}

/// Position in the `(updated_at, number)` order: everything at or before it
/// has been scanned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cursor {
    /// Highest `updated_at` scanned.
    pub at: DateTime<Utc>,
    /// Numbers already scanned whose `updated_at` is exactly [`Self::at`], so
    /// the rest of a same-second run cut by the page cap is still taken and
    /// the part already taken is not.
    pub seen: BTreeSet<i64>,
}

impl Cursor {
    fn starting_at(at: DateTime<Utc>) -> Self {
        Self {
            at,
            seen: BTreeSet::new(),
        }
    }

    /// Whether `item`'s key is past this cursor.
    pub(crate) fn admits(&self, item: &ClosedItem) -> bool {
        item.updated_at > self.at
            || (item.updated_at == self.at && !self.seen.contains(&item.number))
    }

    /// Move past `item` (which this cursor admits).
    fn take(&mut self, item: &ClosedItem) {
        if item.updated_at > self.at {
            self.at = item.updated_at;
            self.seen.clear();
        }
        self.seen.insert(item.number);
    }
}

/// Persisted cursor, if present and parseable. A file without `seen` (the
/// first #10150 format) loads with an empty set: the items at that second are
/// rescanned once, which the marker makes a no-op.
pub(crate) fn load_cursor(root: &Path) -> Option<Cursor> {
    let text = std::fs::read_to_string(cursor_path(root)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let at = parse_ts(v.get("cursor")?.as_str()?).ok()?;
    let seen = match v.get("seen") {
        None => BTreeSet::new(),
        Some(s) => serde_json::from_value(s.clone()).ok()?,
    };
    Some(Cursor { at, seen })
}

/// Atomically persist the cursor (tmp + rename).
pub(crate) fn save_cursor(root: &Path, cursor: &Cursor) -> std::io::Result<()> {
    let path = cursor_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::json!({ "cursor": fmt_ts(cursor.at), "seen": cursor.seen });
    std::fs::write(&tmp, body.to_string())?;
    std::fs::rename(&tmp, &path)
}

/// One REST page of closed items updated at or after `since`, oldest-updated
/// first. `Err` is a failed read (the caller must not advance).
fn fetch_closed_page(
    root: &Path,
    repo: Option<&str>,
    since: &str,
    page: usize,
) -> Result<Vec<ClosedItem>, String> {
    let slug = repo.map_or_else(|| "{owner}/{repo}".to_string(), str::to_string);
    let endpoint = format!(
        "repos/{slug}/issues?state=closed&since={since}&sort=updated&direction=asc\
         &per_page={PAGE_SIZE}&page={page}"
    );
    match run_gh(&["api", &endpoint], root, false) {
        CmdOutcome::Ran(o) if o.status.success() => parse_page(&o.stdout),
        CmdOutcome::Ran(o) => Err(format!("gh api exited {}", o.status)),
        CmdOutcome::Unavailable(u) => Err(format!("gh api could not be run: {u:?}")),
    }
}

/// Keyset walk from `start`: everything the listing holds past `start`, in
/// key order, bounded by [`MAX_PAGES`] / [`MAX_REQUESTS`], plus the cursor
/// that covers exactly what was returned. `fetch(since, page)` is one page of
/// the forge listing. Each request re-anchors `since` at the walk's cursor, so
/// rows the cap cut off, or that an update moved later mid-walk, are still past
/// the returned cursor. A same-second run longer than a page cannot move the
/// anchor, so only then does the page number advance.
pub(crate) fn walk(
    start: &Cursor,
    mut fetch: impl FnMut(&str, usize) -> Result<Vec<ClosedItem>, String>,
) -> Result<(Vec<ClosedItem>, Cursor), String> {
    let mut cur = start.clone();
    let mut batch: Vec<ClosedItem> = Vec::new();
    let mut in_batch: HashSet<i64> = HashSet::new();
    let (mut page, mut pages_taken) = (1, 0);
    for _ in 0..MAX_REQUESTS {
        // Less 1s: correct whether the forge's `since` is inclusive or not.
        let since = fmt_ts(cur.at - ChronoDuration::seconds(1));
        let mut rows = fetch(&since, page)?;
        let full = rows.len() >= PAGE_SIZE;
        rows.sort_by_key(|r| (r.updated_at, r.number));
        let anchor = cur.at;
        let mut took = false;
        for r in rows {
            if !cur.admits(&r) {
                continue;
            }
            cur.take(&r);
            took = true;
            // Updated again mid-walk: one scan of the number is enough.
            if in_batch.insert(r.number) {
                batch.push(r);
            }
        }
        pages_taken += usize::from(took);
        if !full || pages_taken >= MAX_PAGES {
            break;
        }
        page = if cur.at == anchor { page + 1 } else { 1 };
    }
    Ok((batch, cur))
}

/// What one poll did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PollOutcome {
    /// Nothing past the cursor; no scan was run.
    Idle,
    /// Scanned this many closed numbers; cursor advanced past them.
    Scanned { closed: usize },
    /// The listing or the scan failed; cursor unchanged (retry next tick).
    Failed(String),
}

/// One poll against injected seams: `fetch(since, page)` is one page of the
/// closed listing (see [`walk`]), `scan(batch)` runs the re-check and returns
/// `Err` if it should be retried. The cursor advances only on success, and
/// only to the key of the batch it scanned.
pub(crate) fn poll_with(
    root: &Path,
    now: DateTime<Utc>,
    fetch: impl FnMut(&str, usize) -> Result<Vec<ClosedItem>, String>,
    scan: impl FnOnce(&[ClosedItem]) -> Result<(), String>,
) -> PollOutcome {
    let start = load_cursor(root)
        .unwrap_or_else(|| Cursor::starting_at(now - ChronoDuration::hours(LOOKBACK_HOURS)));
    let (batch, next) = match walk(&start, fetch) {
        Ok(w) => w,
        Err(e) => return PollOutcome::Failed(e),
    };
    if batch.is_empty() {
        return PollOutcome::Idle;
    }
    if let Err(e) = scan(&batch) {
        return PollOutcome::Failed(e);
    }
    if let Err(e) = save_cursor(root, &next) {
        return PollOutcome::Failed(format!("could not persist cursor: {e}"));
    }
    PollOutcome::Scanned {
        closed: batch.len(),
    }
}

/// One real poll for `root`. Blocking (spawns `gh`); call from a blocking
/// context. Failures are logged here and returned, never raised.
pub(crate) fn poll_once(root: &Path) -> PollOutcome {
    let repo: Option<&str> = None;
    let outcome = poll_with(
        root,
        Utc::now(),
        |since, page| fetch_closed_page(root, repo, since, page),
        |fresh| {
            scan_fresh(fresh, &mut |pr| pr_close_targets(pr, repo, root), |closed| {
                let opts = ScanOptions {
                    repo,
                    root,
                    limit: DEFAULT_LIMIT,
                    no_prs: false,
                    dry_run: false,
                };
                scan_cleared(closed, &opts)
            })
        },
    );
    if let PollOutcome::Failed(why) = &outcome {
        log::warn!("closed_watch: poll failed, cursor held for retry: {why}");
    }
    outcome
}

/// The re-check for one batch of closed items past the cursor: expand each merged PR to the
/// issues it closed (`close_targets`), run `scan` once over the whole closed
/// set, and return `Err` when the cursor must hold.
pub(crate) fn scan_fresh(
    fresh: &[ClosedItem],
    close_targets: &mut dyn FnMut(i64) -> Result<Vec<i64>, String>,
    scan: impl FnOnce(&[i64]) -> ScanReport,
) -> Result<(), String> {
    let mut unread = Vec::new();
    let mut closed: Vec<i64> = Vec::new();
    for it in fresh {
        closed.push(it.number);
        if it.merged_pr {
            match close_targets(it.number) {
                Ok(targets) => closed.extend(targets),
                Err(why) => unread.push(format!("PR #{} closing references: {why}", it.number)),
            }
        }
    }
    closed.sort_unstable();
    closed.dedup();
    let rep = scan(&closed);
    for u in unread.iter().chain(rep.unread.iter()) {
        log::warn!("closed_watch: not evaluated (unknown, not clear): {u}");
    }
    if rep.posted() > 0 {
        log::info!("closed_watch: posted {} cleared-blocker notice(s)", rep.posted());
    }
    // Any unanswered read holds the cursor: an unexpanded merged PR may have
    // closed an issue someone cites, and an unread candidate may cite a
    // closed number. Advancing past either would drop that close event
    // until the item happens to be updated again.
    if !unread.is_empty() || rep.needs_retry(false) {
        Err(format!(
            "{} read(s) unanswered or a comment post failed",
            unread.len() + rep.unread.len()
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
