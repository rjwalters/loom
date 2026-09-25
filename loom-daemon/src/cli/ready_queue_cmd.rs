//! `loom-daemon queue` — the work finder's ready queue in dispatch order
//! (Issue #8852).
//!
//! A thin view over the `DaemonStatus` round-trip `status` already performs:
//! `last_work_finder_tick.queue` carries one row per ready issue from the
//! most recent tick, ranked by the work finder's own comparator. This command
//! only renders it, with a freshness line so "no tick yet" and "ticked but the
//! queue is empty" never look the same.

use anyhow::Result;
use chrono::{DateTime, Utc};

use loom_daemon::types::{DaemonStatusReport, ReadyQueueRow};

use super::common::resolve_socket_path;
use super::status::{query_daemon_status, resolve_status_timeout};

/// A tick is stale after this many missed intervals (never under
/// [`STALE_FLOOR_SECS`]).
const STALE_AFTER_INTERVALS: u64 = 5;

/// Minimum stale threshold, so a short interval does not flap.
const STALE_FLOOR_SECS: u64 = 300;

/// Seconds after which the last tick counts as stale, derived from the
/// daemon's own reported tick interval (clients must not assume the default).
pub(crate) fn stale_after_secs(report: &DaemonStatusReport) -> i64 {
    let interval = report
        .work_finder_interval_secs
        .filter(|s| *s > 0)
        .unwrap_or(loom_daemon::work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS);
    let secs = interval
        .saturating_mul(STALE_AFTER_INTERVALS)
        .max(STALE_FLOOR_SECS);
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Handle `loom-daemon queue [--json]`. Exits 1 only when the daemon is
/// unreachable; an empty queue is a normal answer.
pub(crate) async fn handle_queue_command(json: bool) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    let timeout_info = resolve_status_timeout(None);
    let report = match query_daemon_status(&socket_path, &timeout_info).await {
        Ok(report) => report,
        Err(e) => {
            let msg = format!("could not reach loom-daemon at {}: {e}", socket_path.display());
            if json {
                println!("{}", serde_json::json!({ "error": msg }));
            } else {
                eprintln!("{msg}");
            }
            std::process::exit(1);
        }
    };
    let now = Utc::now();
    if json {
        println!("{}", serde_json::to_string_pretty(&queue_json(&report, now))?);
    } else {
        print!("{}", render_queue(&report, now));
    }
    Ok(())
}

/// Freshness of the queue: `(state, age_secs)`, where state is one of
/// `fresh`, `stale`, `no_tick` (the loop has not ticked in this daemon
/// process), or `disabled` (the work finder is off).
fn freshness(report: &DaemonStatusReport, now: DateTime<Utc>) -> (&'static str, Option<i64>) {
    match &report.last_work_finder_tick {
        Some(tick) => {
            let age = (now - tick.at).num_seconds().max(0);
            (
                if age > stale_after_secs(report) {
                    "stale"
                } else {
                    "fresh"
                },
                Some(age),
            )
        }
        None if report.work_finder_enabled == Some(false) => ("disabled", None),
        None => ("no_tick", None),
    }
}

/// The `--json` document: the tick timestamp, freshness, and the rows.
pub(crate) fn queue_json(report: &DaemonStatusReport, now: DateTime<Utc>) -> serde_json::Value {
    let (state, age) = freshness(report, now);
    let tick = report.last_work_finder_tick.as_ref();
    serde_json::json!({
        "freshness": state,
        "tick_at": tick.map(|t| t.at),
        "tick_age_secs": age,
        "stale_after_secs": stale_after_secs(report),
        "max_concurrent": tick.map(|t| t.max_concurrent),
        "seen": tick.map(|t| t.seen),
        "errors": tick.map(|t| t.errors),
        // Non-empty => the queue is incomplete: these repos' backlogs are missing.
        "listing_failed": tick.map(|t| t.listing_failed.as_slice()).unwrap_or_default(),
        "complete": tick.is_some_and(|t| t.listing_failed.is_empty()),
        "ordering": "workspace_priority asc, loom:urgent first, created_at oldest first, issue number",
        "queue": tick.map(|t| t.queue.as_slice()).unwrap_or_default(),
    })
}

/// The human-readable table.
pub(crate) fn render_queue(report: &DaemonStatusReport, now: DateTime<Utc>) -> String {
    let (state, age) = freshness(report, now);
    let mut out = String::new();
    let Some(tick) = &report.last_work_finder_tick else {
        out.push_str(match state {
            "disabled" => "Ready queue: work finder is disabled on this daemon.\n",
            _ => "Ready queue: no work-finder tick yet in this daemon process (unknown, not empty).\n",
        });
        return out;
    };
    let age = age.unwrap_or_default();
    let stale = if state == "stale" { " — STALE" } else { "" };
    out.push_str(&format!(
        "Ready queue as of {} ({age}s ago{stale}), cap {}: {}\n",
        tick.at.format("%Y-%m-%d %H:%M:%SZ"),
        tick.max_concurrent,
        tick.reason_summary()
    ));
    out.push_str(
        "Order: workspace priority, then loom:urgent, then oldest first (tier:* labels do not affect order)\n",
    );
    if !tick.listing_failed.is_empty() {
        out.push_str(&format!(
            "  INCOMPLETE: listing ready issues failed for {} — their backlog is missing below\n",
            tick.listing_failed.join(", ")
        ));
    }
    if tick.queue.is_empty() {
        if !tick.listing_failed.is_empty() {
            return out;
        }
        out.push_str(if tick.seen == 0 {
            "  (no ready loom:issue work)\n"
        } else {
            "  (this daemon did not record per-issue rows)\n"
        });
        return out;
    }
    for row in &tick.queue {
        out.push_str(&render_row(row));
    }
    out
}

fn render_row(row: &ReadyQueueRow) -> String {
    let repo = std::path::Path::new(&row.repo)
        .file_name()
        .map_or_else(|| row.repo.clone(), |n| n.to_string_lossy().into_owned());
    let mut flags = Vec::new();
    if row.urgent {
        flags.push("urgent".to_string());
    }
    if let Some(tier) = &row.tier {
        flags.push(tier.clone());
    }
    let flags = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };
    let detail = row
        .detail
        .as_ref()
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    format!(
        "  {:>3}. {repo}#{:<6} p{:<3} {:<7} {}{detail}{flags}\n",
        row.rank,
        row.issue,
        row.workspace_priority,
        row.disposition.state(),
        row.disposition.reason(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use loom_daemon::types::{QueueDisposition, WorkFinderTickSummary};

    fn report_with(queue: Vec<ReadyQueueRow>, at: DateTime<Utc>) -> DaemonStatusReport {
        DaemonStatusReport {
            work_finder_enabled: Some(true),
            last_work_finder_tick: Some(WorkFinderTickSummary {
                at,
                max_concurrent: 2,
                seen: queue.len(),
                queue,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn row(rank: usize, issue: u32, d: QueueDisposition) -> ReadyQueueRow {
        ReadyQueueRow {
            rank,
            repo: "/src/loom".into(),
            issue,
            workspace_priority: 100,
            urgent: rank == 1,
            created_at: None,
            tier: None,
            disposition: d,
            detail: None,
        }
    }

    #[test]
    fn renders_rows_in_rank_order_with_reasons() {
        let now = Utc::now();
        let r = report_with(
            vec![
                row(1, 10, QueueDisposition::Dispatched),
                row(2, 11, QueueDisposition::DeferredCapacity),
            ],
            now,
        );
        let text = render_queue(&r, now);
        let first = text.find("loom#10").unwrap();
        assert!(first < text.find("loom#11").unwrap());
        assert!(text.contains("waiting: concurrency cap full"));
        assert!(text.contains("[urgent]"));
        assert!(!text.contains("STALE"));
    }

    #[test]
    fn distinguishes_no_tick_empty_and_stale() {
        let now = Utc::now();
        let none = DaemonStatusReport::default();
        assert!(render_queue(&none, now).contains("unknown, not empty"));
        assert_eq!(queue_json(&none, now)["freshness"], "no_tick");

        let empty = report_with(vec![], now);
        assert!(render_queue(&empty, now).contains("no ready loom:issue work"));
        assert_eq!(queue_json(&empty, now)["freshness"], "fresh");

        let old = report_with(vec![], now - chrono::Duration::minutes(30));
        assert!(render_queue(&old, now).contains("STALE"));
        assert_eq!(queue_json(&old, now)["freshness"], "stale");

        // A 10-minute interval moves the threshold to 50 minutes.
        let mut slow = report_with(vec![], now - chrono::Duration::minutes(30));
        slow.work_finder_interval_secs = Some(600);
        assert_eq!(stale_after_secs(&slow), 3000);
        assert_eq!(queue_json(&slow, now)["freshness"], "fresh");

        let off = DaemonStatusReport {
            work_finder_enabled: Some(false),
            ..Default::default()
        };
        assert_eq!(queue_json(&off, now)["freshness"], "disabled");
    }

    #[test]
    fn a_failed_listing_is_incomplete_not_empty() {
        let now = Utc::now();
        let mut r = report_with(vec![], now);
        if let Some(t) = r.last_work_finder_tick.as_mut() {
            t.errors = 1;
            t.listing_failed = vec!["/src/loom".into()];
        }
        let text = render_queue(&r, now);
        assert!(text.contains("INCOMPLETE"), "{text}");
        assert!(!text.contains("no ready loom:issue work"), "{text}");
        let j = queue_json(&r, now);
        assert_eq!(j["complete"], false);
        assert_eq!(j["listing_failed"][0], "/src/loom");
        assert_eq!(j["errors"], 1);
    }
}
