//! `loom-daemon forge calls [--since 1h|3h] [--by bucket|caller|role|repo]`
//! (W1 of the forge API reduction plan): this host's forge-call sink rolled
//! up per billed GitHub bucket — or per caller, identity role or repo — with
//! the bucket book's newest readings beside it.
//!
//! Reads only: the per-host sink files and the daemon's bucket-book
//! snapshot. It makes no forge call, so it is safe to run while a host is
//! rate-limited. Arg docs live here so `forge_action.rs` pays one variant.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};
use clap::Args;
use loom_daemon::forge_bucket_book::{self, BucketKey, Reading};
use loom_daemon::forge_call_stats::buckets::{self, CallsAggregate, GroupBy};

/// `loom-daemon forge calls` arguments.
#[derive(Args, Debug)]
pub(crate) struct CallsArgs {
    /// How far back to read: `<n>m` or `<n>h`. The sink keeps about 3 hours.
    #[arg(long, default_value = "1h", value_name = "DURATION")]
    since: String,

    /// Group rows by `bucket` (account, cred owner, resource, window),
    /// `caller`, `role` or `repo`.
    #[arg(long, default_value = "bucket", value_name = "GROUP")]
    by: String,

    /// Read this sink directory instead of the host's
    /// (`LOOM_FORGE_CALL_STATS_DIR`, else the default).
    #[arg(long, value_name = "DIR")]
    sink_dir: Option<PathBuf>,
}

/// Parse `--since`: `<n>m` or `<n>h`, at most 24 h.
fn parse_since(value: &str) -> Result<i64> {
    let value = value.trim();
    let (digits, unit) = value.split_at(value.len().saturating_sub(1));
    let n: i64 = digits
        .parse()
        .map_err(|_| anyhow!("--since must look like 90m or 3h (got {value:?})"))?;
    let secs = match unit {
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(anyhow!("--since must look like 90m or 3h (got {value:?})")),
    };
    if !(60..=86_400).contains(&secs) {
        return Err(anyhow!("--since must be between 1m and 24h (got {value:?})"));
    }
    Ok(secs)
}

/// Run the verb.
pub(crate) fn handle(args: CallsArgs) -> Result<()> {
    let secs = parse_since(&args.since)?;
    let by = GroupBy::parse(&args.by).map_err(|e| anyhow!(e))?;
    let Some(dir) = args
        .sink_dir
        .or_else(loom_daemon::forge_call_stats::host_sink_dir)
    else {
        println!("forge calls: the call-stats sink is disabled (LOOM_FORGE_CALL_STATS_DIR=off)");
        return Ok(());
    };
    let now = Utc::now();
    let ts = now.timestamp();
    let agg = buckets::aggregate_since(&dir, ts - secs, ts, by);
    let book = forge_bucket_book::load(&dir, ts);
    print!("{}", render(&agg, &book, by, &args.since, now));
    Ok(())
}

fn hhmm(epoch: i64) -> String {
    Utc.timestamp_opt(epoch, 0)
        .single()
        .map_or_else(|| epoch.to_string(), |t| t.format("%H:%MZ").to_string())
}

/// The whole report. Pure, so the output is golden-tested.
pub(crate) fn render(
    agg: &CallsAggregate,
    book: &[(BucketKey, Reading)],
    by: GroupBy,
    since: &str,
    now: DateTime<Utc>,
) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(format!(
        "Forge calls, last {since} by {} — {} row(s) as of {}",
        by.as_str(),
        agg.lines,
        now.format("%Y-%m-%d %H:%M UTC")
    ));
    let columns = by.columns();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            agg.groups
                .iter()
                .map(|g| cell(by, i, &g.key).len())
                .chain(std::iter::once(c.len()))
                .max()
                .unwrap_or(c.len())
        })
        .collect();
    let key_text = |values: Vec<String>| -> String {
        values
            .iter()
            .zip(&widths)
            .map(|(v, w)| format!("{v:<w$}"))
            .collect::<Vec<_>>()
            .join("  ")
    };
    if agg.groups.is_empty() {
        line("  no rows recorded in this window".to_string());
    } else {
        line(format!(
            "  {}  {:>8} {:>6} {:>8} {:>6}",
            key_text(columns.iter().map(|c| (*c).to_string()).collect()),
            "CHARGED",
            "304",
            "LIMITED",
            "ERROR"
        ));
        for g in &agg.groups {
            let cells = (0..columns.len()).map(|i| cell(by, i, &g.key)).collect();
            line(format!(
                "  {}  {:>8} {:>6} {:>8} {:>6}",
                key_text(cells),
                g.charged,
                g.not_modified,
                g.rate_limited,
                g.error
            ));
        }
    }
    let pages_unknown: u64 = agg.groups.iter().map(|g| g.pages_unknown).sum();
    line(format!(
        "  unattributed: {} without a repo, {} without a credential (older binary); \
         {} paginated row(s) with unknown pages; {} cwd-route disagreement(s)",
        agg.no_repo, agg.no_account, pages_unknown, agg.cwd_route_disagree
    ));
    if book.is_empty() {
        line("Bucket readings: none believed (no probe or header reading in the last 10m)".into());
    } else {
        line("Bucket readings (newest, < 10m old):".into());
        line(format!(
            "  {:<14} {:<14} {:<8} {:>7} {:>7} {:>9}  {:<6}  {}",
            "ACCOUNT", "OWNER", "RESOURCE", "USED", "LIMIT", "REMAINING", "RESET", "SOURCE"
        ));
        let opt = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |n| n.to_string());
        for (key, r) in book {
            line(format!(
                "  {:<14} {:<14} {:<8} {:>7} {:>7} {:>9}  {:<6}  {} ({}s ago)",
                key.account,
                key.owner,
                key.resource.as_str(),
                opt(r.used),
                opt(r.limit),
                opt(r.remaining),
                hhmm(r.reset_epoch),
                r.source.as_str(),
                (now.timestamp() - r.observed_at).max(0)
            ));
        }
    }
    out
}

/// One key cell; a bucket's `rst` renders as its reset time.
fn cell(by: GroupBy, index: usize, key: &[String]) -> String {
    let raw = key.get(index).cloned().unwrap_or_default();
    if by == GroupBy::Bucket && index == 4 {
        return raw.parse::<i64>().map_or(raw, hhmm);
    }
    raw
}

#[cfg(test)]
#[path = "forge_calls_cmd_tests.rs"]
mod tests;
