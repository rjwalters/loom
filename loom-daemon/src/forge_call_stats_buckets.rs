//! Per-bucket rollups of the host sink (W1 of the forge API reduction plan).
//!
//! GitHub bills each credential's installation separately, per resource
//! (`core`, `graphql`, `search`). A row's `caller` says *what* spent budget;
//! this module answers *whose* budget it was: the rows the `gh` facade
//! attributed ([`super::CallAttribution`]) grouped by billed bucket — the
//! credential's account, the owner its installation covers, the resource and
//! the window (`rst`) — or by caller, identity role or repository.
//!
//! **Charged** is what a row cost GitHub: `ok` rows × `max(pages, 1)`. A
//! `304` is free, a rate-limited call spent nothing, a known-free request
//! (`fr`, the `gh api rate_limit` probe) is counted under `free` instead, and
//! an error is counted separately (it may or may not have been billed). A `--paginate` row
//! without `--include` cannot know its pages and counts as one (`pu`); the
//! rollup reports how many such rows it saw, so the lower bound is visible.
//!
//! Reads only: a sink file read, no forge call. Lines written by an older
//! binary carry no attribution and roll up under `unknown`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use super::{Outcome, SinkLine};

/// The counter name for rows whose `origin`-derived repo disagreed with the
/// repo `gh` itself would resolve from the same checkout (`rd`).
pub const CWD_ROUTE_DISAGREE: &str = "facade.cwd_route.disagree";

static CWD_ROUTE_DISAGREEMENTS: AtomicU64 = AtomicU64::new(0);

/// Count one [`CWD_ROUTE_DISAGREE`] row in this process.
pub fn bump_cwd_route_disagree() {
    CWD_ROUTE_DISAGREEMENTS.fetch_add(1, Ordering::Relaxed);
}

/// This process's [`CWD_ROUTE_DISAGREE`] count since start.
#[must_use]
pub fn cwd_route_disagreements() -> u64 {
    CWD_ROUTE_DISAGREEMENTS.load(Ordering::Relaxed)
}

/// What [`aggregate_since`] groups rows by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// `(account, cred_owner, resource, rst)` — one billed bucket window.
    Bucket,
    /// The facade operation name (`caller`).
    Caller,
    /// The identity role (`reader` / `writer` / `writer-fallback`).
    Role,
    /// The `owner/repo` the row was for.
    Repo,
}

impl GroupBy {
    /// Parse a `--by` value.
    ///
    /// # Errors
    ///
    /// On anything but `bucket`, `caller`, `role` or `repo`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "bucket" => Ok(Self::Bucket),
            "caller" => Ok(Self::Caller),
            "role" => Ok(Self::Role),
            "repo" => Ok(Self::Repo),
            other => Err(format!("--by must be bucket, caller, role or repo (got {other:?})")),
        }
    }

    /// Column headings of the group key.
    #[must_use]
    pub fn columns(self) -> &'static [&'static str] {
        match self {
            Self::Bucket => &["ACCOUNT", "CRED_OWNER", "RESOURCE", "RESET"],
            Self::Caller => &["CALLER"],
            Self::Role => &["ROLE"],
            Self::Repo => &["REPO"],
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bucket => "bucket",
            Self::Caller => "caller",
            Self::Role => "role",
            Self::Repo => "repo",
        }
    }
}

/// One group of rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GroupRow {
    /// The group key, one value per [`GroupBy::columns`] entry. `rst` is the
    /// window's reset epoch as text, `-` when no header was seen.
    pub key: Vec<String>,
    /// Sink rows in the group.
    pub rows: u64,
    /// Requests GitHub charged: `ok` rows × `max(pages, 1)`, known-free
    /// requests excluded.
    pub charged: u64,
    /// Rows for a request GitHub does not charge (`fr`): sent, observed in
    /// `rows`, never in `charged`.
    pub free: u64,
    pub not_modified: u64,
    pub rate_limited: u64,
    pub error: u64,
    /// Rows whose page count is unknown (`--paginate` without `--include`).
    pub pages_unknown: u64,
}

/// [`aggregate_since`]'s result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CallsAggregate {
    /// Every parsed line at or after `since`.
    pub lines: u64,
    /// Groups, in key order.
    pub groups: Vec<GroupRow>,
    /// Lines with no repository (`rp`).
    pub no_repo: u64,
    /// Lines with no credential attribution (`ca`) — older binaries.
    pub no_account: u64,
    /// Lines flagged [`CWD_ROUTE_DISAGREE`] (`rd`).
    pub cwd_route_disagree: u64,
}

/// What one line cost GitHub (see the module docs).
pub(super) fn charged(line: &SinkLine) -> u64 {
    if line.o == Outcome::Ok && line.at.fr != Some(true) {
        u64::from(line.at.pg.unwrap_or(1).max(1))
    } else {
        0
    }
}

fn key_of(line: &SinkLine, by: GroupBy) -> Vec<String> {
    let or_unknown = |v: &Option<String>| v.clone().unwrap_or_else(|| "unknown".to_string());
    match by {
        GroupBy::Bucket => vec![
            or_unknown(&line.at.ca),
            line.at.co.clone().unwrap_or_else(|| "-".to_string()),
            line.at
                .rr
                .clone()
                .unwrap_or_else(|| line.p.as_str().to_string()),
            line.rst.map_or_else(|| "-".to_string(), |r| r.to_string()),
        ],
        GroupBy::Caller => vec![line.c.clone()],
        GroupBy::Role => vec![or_unknown(&line.ir)],
        GroupBy::Repo => vec![or_unknown(&line.rp)],
    }
}

/// Group sink `lines` with `t >= since` by `by`. Unparseable lines are
/// skipped. Pure.
#[must_use]
pub fn aggregate_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    since: i64,
    by: GroupBy,
) -> CallsAggregate {
    let mut agg = CallsAggregate::default();
    let mut groups: BTreeMap<Vec<String>, GroupRow> = BTreeMap::new();
    for raw in lines {
        let Ok(line) = serde_json::from_str::<SinkLine>(raw) else {
            continue;
        };
        if line.t < since {
            continue;
        }
        agg.lines += 1;
        agg.no_repo += u64::from(line.rp.is_none());
        agg.no_account += u64::from(line.at.ca.is_none());
        agg.cwd_route_disagree += u64::from(line.at.rd == Some(true));
        let key = key_of(&line, by);
        let g = groups.entry(key.clone()).or_insert_with(|| GroupRow {
            key,
            ..GroupRow::default()
        });
        g.rows += 1;
        g.charged += charged(&line);
        g.free += u64::from(line.at.fr == Some(true));
        match line.o {
            Outcome::Ok => {}
            Outcome::NotModified => g.not_modified += 1,
            Outcome::RateLimited => g.rate_limited += 1,
            Outcome::Error => g.error += 1,
        }
        g.pages_unknown += u64::from(line.at.pu == Some(true));
    }
    agg.groups = groups.into_values().collect();
    agg
}

/// Group the sink in `dir` from `since` to `now` (epoch seconds) by `by`.
/// The sink keeps the last few hours only, so an older `since` reads what
/// is left. A missing directory aggregates nothing.
#[must_use]
pub fn aggregate_since(dir: &Path, since: i64, now: i64, by: GroupBy) -> CallsAggregate {
    aggregate_lines(super::read_since(dir, since, now).lines(), since, by)
}

/// The `status` per-bucket block: the window's rows grouped by
/// `(account, cred_owner, resource)` (windows merged), each beside the
/// bucket book's believed reading — this process's, else the snapshot the
/// daemon left in `dir`. A believed reading with no rows still shows.
#[must_use]
pub fn status_rows(dir: &Path, now: i64) -> Vec<crate::types::ForgeBucketStatus> {
    let since = now - super::WINDOW_SECS;
    let agg = aggregate_since(dir, since, now, GroupBy::Bucket);
    let mut rows: BTreeMap<(String, String, String), crate::types::ForgeBucketStatus> =
        BTreeMap::new();
    for g in agg.groups {
        let [account, owner, resource, _] = <[String; 4]>::try_from(g.key).unwrap_or_default();
        let row = rows
            .entry((account.clone(), owner.clone(), resource.clone()))
            .or_insert_with(|| crate::types::ForgeBucketStatus {
                account,
                cred_owner: owner,
                resource,
                ..Default::default()
            });
        row.charged += g.charged;
        row.not_modified += g.not_modified;
        row.rate_limited += g.rate_limited;
    }
    let mut book = crate::forge_bucket_book::snapshot(now);
    if book.is_empty() {
        book = crate::forge_bucket_book::load(dir, now);
    }
    for (key, reading) in book {
        let row = rows
            .entry((key.account.clone(), key.owner.clone(), key.resource.as_str().to_string()))
            .or_insert_with(|| crate::types::ForgeBucketStatus {
                account: key.account.clone(),
                cred_owner: key.owner.clone(),
                resource: key.resource.as_str().to_string(),
                ..Default::default()
            });
        row.used = reading.used;
        row.limit = reading.limit;
        row.reset_at =
            chrono::TimeZone::timestamp_opt(&chrono::Utc, reading.reset_epoch, 0).single();
    }
    rows.into_values().collect()
}

#[cfg(test)]
#[path = "forge_call_stats_buckets_tests.rs"]
mod tests;
