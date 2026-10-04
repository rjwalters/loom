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
//! * The cursor is the max `updated_at` seen. `closed_at <= updated_at`, so an
//!   item closed after the cursor is always in the next window, whatever
//!   page-cap truncation did. "Newly closed" is `closed_at >= since`; a
//!   re-listed old close is harmless because the notify marker dedupes.
//! * The first run looks back [`LOOKBACK_HOURS`] only, and a pass reads at
//!   most [`MAX_PAGES`] pages.
//! * The cursor advances only after a successful scan, so a failed tick
//!   retries. With nothing newly closed there is no `list_blocked` call.
//! * Two hosts polling one repo can both post in the check-then-post window;
//!   a duplicate notice is harmless and accepted by design.
//! * Never edits a label; every failure is logged, never fatal.
//!
//! Config: `autonomous.closedWatch.{enabled,intervalSecs}`, env
//! `LOOM_CLOSED_WATCH` / `LOOM_CLOSED_WATCH_INTERVAL_SECS` (env > config >
//! default; default off).

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use serde::Deserialize;

use loom_daemon::cmd_out::CmdOutcome;
use loom_daemon::script_helpers::run_gh;

use super::notify_cleared_blockers::{expand_pr, scan_cleared, ScanOptions};
use super::stale_blocked::DEFAULT_LIMIT;

pub(crate) const ENABLE_ENV: &str = "LOOM_CLOSED_WATCH";
pub(crate) const INTERVAL_ENV: &str = "LOOM_CLOSED_WATCH_INTERVAL_SECS";
pub(crate) const DEFAULT_INTERVAL_SECS: u64 = 300;
/// First-run lookback.
pub(crate) const LOOKBACK_HOURS: i64 = 24;
const PAGE_SIZE: usize = 100;
/// Hard bound on pages read in one pass.
const MAX_PAGES: usize = 5;
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
    pub closed_at: String,
    pub updated_at: String,
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

/// Parse one REST page. Pure. Items with no `closed_at` are dropped.
pub(crate) fn parse_page(stdout: &[u8]) -> Result<Vec<ClosedItem>, String> {
    let raw: Vec<RawItem> =
        serde_json::from_slice(stdout).map_err(|e| format!("unreadable closed-items JSON: {e}"))?;
    Ok(raw
        .into_iter()
        .filter_map(|r| {
            let closed_at = r.closed_at?;
            let updated_at = r.updated_at.unwrap_or_else(|| closed_at.clone());
            Some(ClosedItem {
                number: r.number,
                closed_at,
                updated_at,
                merged_pr: r.pull_request.is_some_and(|p| p.merged_at.is_some()),
            })
        })
        .collect())
}

fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn cursor_path(root: &Path) -> PathBuf {
    root.join(".loom").join(CURSOR_FILE)
}

/// Persisted cursor, if present and parseable.
pub(crate) fn load_cursor(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cursor_path(root)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let c = v.get("cursor")?.as_str()?.to_string();
    DateTime::parse_from_rfc3339(&c).ok()?;
    Some(c)
}

/// Atomically persist the cursor (tmp + rename).
pub(crate) fn save_cursor(root: &Path, cursor: &str) -> std::io::Result<()> {
    let path = cursor_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::json!({ "cursor": cursor }).to_string())?;
    std::fs::rename(&tmp, &path)
}

/// List closed items updated since `since`, oldest-updated first, bounded by
/// [`MAX_PAGES`]. `Err` is a failed read (the caller must not advance).
fn list_closed_rest(
    root: &Path,
    repo: Option<&str>,
    since: &str,
) -> Result<Vec<ClosedItem>, String> {
    let slug = repo.map_or_else(|| "{owner}/{repo}".to_string(), str::to_string);
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let endpoint = format!(
            "repos/{slug}/issues?state=closed&since={since}&sort=updated&direction=asc\
             &per_page={PAGE_SIZE}&page={page}"
        );
        let out = match run_gh(&["api", &endpoint], root, false) {
            CmdOutcome::Ran(o) if o.status.success() => o.stdout,
            CmdOutcome::Ran(o) => return Err(format!("gh api exited {}", o.status)),
            CmdOutcome::Unavailable(u) => return Err(format!("gh api could not be run: {u:?}")),
        };
        let raw_len = serde_json::from_slice::<Vec<serde_json::Value>>(&out)
            .map_err(|e| format!("unreadable closed-items JSON: {e}"))?
            .len();
        all.extend(parse_page(&out)?);
        if raw_len < PAGE_SIZE {
            break;
        }
    }
    Ok(all)
}

/// What one poll did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PollOutcome {
    /// Nothing newly closed; no scan was run.
    Idle,
    /// Scanned this many newly closed numbers; cursor advanced.
    Scanned { closed: usize },
    /// The listing or the scan failed; cursor unchanged (retry next tick).
    Failed(String),
}

/// One poll against injected seams: `list(since)` returns closed items,
/// `scan(closed_numbers)` runs the re-check and returns `Err` if it should be
/// retried. Cursor advances only on success.
pub(crate) fn poll_with(
    root: &Path,
    now: DateTime<Utc>,
    list: impl FnOnce(&str) -> Result<Vec<ClosedItem>, String>,
    scan: impl FnOnce(&[ClosedItem]) -> Result<(), String>,
) -> PollOutcome {
    let since =
        load_cursor(root).unwrap_or_else(|| fmt_ts(now - ChronoDuration::hours(LOOKBACK_HOURS)));
    let items = match list(&since) {
        Ok(i) => i,
        Err(e) => return PollOutcome::Failed(e),
    };
    // `since` was already filtered server-side on updated_at; closed_at picks
    // out real closes from mere comments on old closed items.
    let fresh: Vec<ClosedItem> = items
        .iter()
        .filter(|i| i.closed_at.as_str() >= since.as_str())
        .cloned()
        .collect();
    let next = items
        .iter()
        .map(|i| i.updated_at.as_str())
        .max()
        .unwrap_or(&since)
        .max(&since)
        .to_string();
    if !fresh.is_empty() {
        if let Err(e) = scan(&fresh) {
            return PollOutcome::Failed(e);
        }
    }
    if next != since {
        if let Err(e) = save_cursor(root, &next) {
            return PollOutcome::Failed(format!("could not persist cursor: {e}"));
        }
    }
    if fresh.is_empty() {
        PollOutcome::Idle
    } else {
        PollOutcome::Scanned {
            closed: fresh.len(),
        }
    }
}

/// One real poll for `root`. Blocking (spawns `gh`); call from a blocking
/// context. Failures are logged here and returned, never raised.
pub(crate) fn poll_once(root: &Path) -> PollOutcome {
    let repo: Option<&str> = None;
    let outcome = poll_with(
        root,
        Utc::now(),
        |since| list_closed_rest(root, repo, since),
        |fresh| {
            let mut unread = Vec::new();
            let mut closed: Vec<i64> = Vec::new();
            for it in fresh {
                if it.merged_pr {
                    closed.extend(expand_pr(it.number, repo, root, &mut unread));
                } else {
                    closed.push(it.number);
                }
            }
            closed.sort_unstable();
            closed.dedup();
            let opts = ScanOptions {
                repo,
                root,
                limit: DEFAULT_LIMIT,
                no_prs: false,
                dry_run: false,
            };
            let rep = scan_cleared(&closed, &opts);
            for u in unread.iter().chain(rep.unread.iter()) {
                log::warn!("closed_watch: not evaluated (unknown, not clear): {u}");
            }
            if rep.posted() > 0 {
                log::info!("closed_watch: posted {} cleared-blocker notice(s)", rep.posted());
            }
            if rep.needs_retry(false) {
                Err("enumeration or comment post failed".to_string())
            } else {
                Ok(())
            }
        },
    );
    if let PollOutcome::Failed(why) = &outcome {
        log::warn!("closed_watch: poll failed, cursor held for retry: {why}");
    }
    outcome
}

#[cfg(test)]
mod tests;
