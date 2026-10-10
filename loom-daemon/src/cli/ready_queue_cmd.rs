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

use loom_daemon::types::{
    DaemonStatusReport, DispatchPlanContext, PlanState, ReadyQueueRow, WorkFinderTickSummary,
};

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

/// The comparator key names the queue is ordered by (Issue #9288): the
/// daemon's own `plan.ordering`, else the key names on its rows. `None` when
/// the daemon published neither (a pre-#9288 daemon) — never a local copy of
/// the comparator.
fn ordering(tick: &WorkFinderTickSummary) -> Option<Vec<String>> {
    if let Some(plan) = tick.plan.as_ref().filter(|p| !p.ordering.is_empty()) {
        return Some(plan.ordering.clone());
    }
    tick.queue
        .iter()
        .find(|r| !r.plan.keys.is_empty())
        .map(|r| r.plan.keys.iter().map(|k| k.name.clone()).collect())
}

/// The `--json` document: the tick timestamp, freshness, the plan block and
/// the rows.
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
        // Non-empty => these repos' listings came back partial (#11139): some
        // rows are present, but not all of them.
        "listing_incomplete": tick.map(|t| t.listing_incomplete.as_slice()).unwrap_or_default(),
        // `false` when any repo's listing failed OR came back partial.
        "complete": tick.is_some_and(|t| t.listing_not_whole().next().is_none()),
        "ordering": tick.and_then(ordering),
        "plan": tick.and_then(|t| t.plan.as_ref()),
        "queue": tick.map(|t| t.queue.as_slice()).unwrap_or_default(),
        // Every starred issue's landing state (#9244 C), including the ones
        // not in the ready listing (building, in review, parked).
        "operator_priority_landing": report.operator_priority_landing,
    })
}

/// The human-readable table, then the starred issues' landing states
/// (#9244 C).
pub(crate) fn render_queue(report: &DaemonStatusReport, now: DateTime<Utc>) -> String {
    let mut out = render_ready(report, now);
    for line in loom_daemon::star_liveness::render::lines(report.operator_priority_landing.as_ref())
    {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn render_ready(report: &DaemonStatusReport, now: DateTime<Utc>) -> String {
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
    if let Some(keys) = ordering(tick) {
        out.push_str(&format!(
            "Order: {} (tier:* labels do not affect order)\n",
            keys.join(", then ")
        ));
    }
    if let Some(plan) = &tick.plan {
        out.push_str(&render_plan(plan));
    }
    if !tick.listing_failed.is_empty() {
        out.push_str(&format!(
            "  INCOMPLETE: listing ready issues failed for {} — their backlog is missing below\n",
            tick.listing_failed.join(", ")
        ));
    }
    if !tick.listing_incomplete.is_empty() {
        out.push_str(&format!(
            "  INCOMPLETE: listing ready issues came back partial for {} — only part of their backlog is below\n",
            tick.listing_incomplete.join(", ")
        ));
    }
    if tick.queue.is_empty() {
        // A failed or partial listing makes an empty table "unknown", never
        // "no ready work" (#11139).
        if tick.listing_not_whole().next().is_some() {
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

/// The plan's one-line slot summary.
fn render_plan(plan: &DispatchPlanContext) -> String {
    let s = &plan.slots;
    let opt = |v: Option<usize>| v.map_or_else(|| "?".to_string(), |n| n.to_string());
    let mut line = format!(
        "Plan: {} free of {} slot(s), {} admission(s)/tick",
        opt(s.free),
        s.max_concurrent,
        opt(s.max_admissions_per_tick)
    );
    if let Some(secs) = plan.tick_interval_secs {
        line.push_str(&format!(", every {secs}s"));
    }
    if let (true, Some(host), Some(count)) =
        (plan.shard.configured, plan.shard.host_shard, plan.shard.shard_count)
    {
        line.push_str(&format!(", shard {host}/{count}"));
    }
    if s.saturation_held {
        line.push_str(", SATURATION-HELD");
    }
    line.push('\n');
    line
}

/// A row's plan column: `#<position> <plan_state>[/<gate>]`, or the bare
/// coarse state for a pre-#9288 row.
fn plan_column(row: &ReadyQueueRow) -> String {
    let p = &row.plan;
    if p.plan_state == PlanState::Unknown {
        return row.disposition.state().to_string();
    }
    let state = serde_json::to_value(p.plan_state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let gate = p
        .gate
        .and_then(|g| serde_json::to_value(g).ok())
        .and_then(|v| v.as_str().map(|g| format!("/{g}")))
        .unwrap_or_default();
    let pos = p
        .position
        .map_or_else(|| "-".to_string(), |n| format!("#{n}"));
    format!("{pos} {state}{gate}")
}

fn render_row(row: &ReadyQueueRow) -> String {
    let repo = std::path::Path::new(&row.repo)
        .file_name()
        .map_or_else(|| row.repo.clone(), |n| n.to_string_lossy().into_owned());
    let mut flags = Vec::new();
    if row.operator_priority {
        flags.push("starred".to_string());
    }
    if row.main_red_fix {
        flags.push("red-main-fix".to_string());
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
        "  {:>3}. {repo}#{:<6} p{:<3} {:<20} {}{detail}{flags}\n",
        row.rank,
        row.issue,
        row.workspace_priority,
        plan_column(row),
        row.disposition.reason(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use loom_daemon::types::{PlanGate, PlanKey, PlanSlots, QueueDisposition, RowPlan};

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
            urgent: false,
            operator_priority: rank == 1,
            operator_priority_at: None,
            main_red_fix: false,
            created_at: None,
            tier: None,
            story_points: None,
            disposition: d,
            detail: None,
            state: d.state().into(),
            reason: d.reason().into(),
            plan: Default::default(),
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
        assert!(text.contains("[starred]"));
        assert!(!text.contains("[urgent]"), "urgent is no longer a row flag (#9244)");
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

    #[test]
    fn a_partial_listing_is_incomplete_not_whole() {
        // #11139: a later page failed / the page cap / a mid-walk change. The
        // rows read are kept, but neither the JSON nor the human view may
        // present them as the whole queue.
        let now = Utc::now();
        let mut r = report_with(vec![], now);
        if let Some(t) = r.last_work_finder_tick.as_mut() {
            t.listing_incomplete = vec!["/src/loom".into()];
        }
        let text = render_queue(&r, now);
        assert!(text.contains("INCOMPLETE"), "{text}");
        assert!(text.contains("partial for /src/loom"), "{text}");
        assert!(!text.contains("no ready loom:issue work"), "{text}");
        let j = queue_json(&r, now);
        assert_eq!(j["complete"], false);
        assert_eq!(j["listing_incomplete"][0], "/src/loom");
        assert_eq!(j["listing_failed"].as_array().map(Vec::len), Some(0));

        // With rows present the view still flags the partial listing.
        let mut r = report_with(vec![row(1, 10, QueueDisposition::DeferredCapacity)], now);
        if let Some(t) = r.last_work_finder_tick.as_mut() {
            t.listing_incomplete = vec!["/src/loom".into()];
        }
        assert!(render_queue(&r, now).contains("INCOMPLETE"));
        assert_eq!(queue_json(&r, now)["complete"], false);

        // A whole listing stays complete.
        let whole = report_with(vec![row(1, 10, QueueDisposition::DeferredCapacity)], now);
        assert_eq!(queue_json(&whole, now)["complete"], true);
        assert!(!render_queue(&whole, now).contains("INCOMPLETE"));
    }

    #[test]
    fn plan_fields_render_and_ordering_comes_from_the_daemon() {
        let now = Utc::now();
        let mut r = report_with(
            vec![
                row(1, 10, QueueDisposition::Dispatched),
                row(2, 11, QueueDisposition::DeferredRampCap),
            ],
            now,
        );
        // A pre-#9288 daemon: no plan, no keys => no ordering, no invented copy.
        assert!(queue_json(&r, now)["ordering"].is_null());
        assert!(!render_queue(&r, now).contains("Order:"));

        let tick = r.last_work_finder_tick.as_mut().unwrap();
        tick.queue[0].plan = RowPlan {
            position: Some(1),
            plan_state: PlanState::Running,
            keys: vec![PlanKey {
                name: "workspace_priority".into(),
                value: serde_json::json!(100),
            }],
            ..RowPlan::default()
        };
        tick.queue[1].plan = RowPlan {
            position: Some(2),
            plan_state: PlanState::Next,
            gate: Some(PlanGate::Ramp),
            ..RowPlan::default()
        };
        // Row keys alone are enough to name the ordering.
        assert_eq!(queue_json(&r, now)["ordering"], serde_json::json!(["workspace_priority"]));

        let tick = r.last_work_finder_tick.as_mut().unwrap();
        tick.plan = Some(DispatchPlanContext {
            slots: PlanSlots {
                max_concurrent: 2,
                occupancy: Some(2),
                free: Some(0),
                max_admissions_per_tick: Some(1),
                ..PlanSlots::default()
            },
            tick_interval_secs: Some(60),
            ordering: vec!["workspace_priority".into(), "operator_priority".into()],
            ..DispatchPlanContext::default()
        });
        let j = queue_json(&r, now);
        assert_eq!(j["ordering"], serde_json::json!(["workspace_priority", "operator_priority"]));
        assert_eq!(j["plan"]["slots"]["max_concurrent"], 2);
        assert_eq!(j["queue"][1]["plan_state"], "next");
        assert_eq!(j["queue"][1]["gate"], "ramp");
        assert_eq!(j["queue"][1]["position"], 2);
        let text = render_queue(&r, now);
        assert!(text.contains("Order: workspace_priority, then operator_priority"), "{text}");
        assert!(
            text.contains("Plan: 0 free of 2 slot(s), 1 admission(s)/tick, every 60s"),
            "{text}"
        );
        assert!(text.contains("#2 next/ramp"), "{text}");
        assert!(text.contains("#1 running"), "{text}");
    }
}
