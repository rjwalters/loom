//! The `Forge calls` section on `loom-daemon status` (Issue #9251).
//!
//! A sibling module rather than more lines in `status_render.rs` (an
//! over-threshold ledger entry). Answers "is the ETag cache earning its free
//! `304`s, or do we just make too many calls?": per caller and pool, the
//! 200 / 304 / limited / error counts over the last hour host-wide, this
//! daemon's own since-start totals, and the latest free budget reading.

use chrono::{DateTime, Utc};
use loom_daemon::health::format_window;
use loom_daemon::types::{DaemonStatusReport, ForgeBudgetReading, ForgeCallCounts};

/// Every line of the section; empty for a pre-#9251 daemon (no field).
pub fn render_forge_calls_lines(report: &DaemonStatusReport, now: DateTime<Utc>) -> Vec<String> {
    let Some(fc) = report.forge_calls.as_deref() else {
        return Vec::new();
    };
    let window = format_window(fc.window_secs);
    let mut lines = Vec::new();
    match &fc.host_window {
        None => lines.push(format!(
            "Forge calls (last {window}, host-wide): unavailable — call-stats sink disabled"
        )),
        Some(rows) if rows.is_empty() => {
            lines.push(format!("Forge calls (last {window}, host-wide): none recorded"));
        }
        Some(rows) => {
            lines.push(format!("Forge calls (last {window}, host-wide):"));
            lines.push(format!(
                "  {:<22} {:<8} {:>6} {:>6} {:>8} {:>6} {:>6}",
                "CALLER", "POOL", "200", "304", "LIMITED", "ERROR", "304%"
            ));
            lines.extend(rows.iter().map(render_row));
        }
    }
    let totals = fc.since_start.iter().fold([0_u64; 4], |mut acc, r| {
        acc[0] += r.ok;
        acc[1] += r.not_modified;
        acc[2] += r.rate_limited;
        acc[3] += r.error;
        acc
    });
    let since = fc
        .since
        .map_or_else(|| "start".to_string(), |s| s.format("%Y-%m-%d %H:%M UTC").to_string());
    lines.push(format!(
        "  this daemon since {since}: {} × 200, {} × 304, {} limited, {} error",
        totals[0], totals[1], totals[2], totals[3]
    ));
    if !fc.budget.is_empty() {
        let own = |pool: &str| -> Option<u64> {
            fc.own_window
                .as_ref()
                .and_then(|rows| rows.iter().find(|r| r.pool == pool))
                .map(|r| r.consumed)
        };
        let readings: Vec<String> = fc
            .budget
            .iter()
            .map(|b| render_budget(b, own(&b.pool), now))
            .collect();
        lines.push(format!("  budget: {}", readings.join(" · ")));
    }
    lines
}

fn render_row(r: &ForgeCallCounts) -> String {
    let answered = r.ok + r.not_modified;
    let hit = (r.not_modified * 100)
        .checked_div(answered)
        .map_or_else(|| "-".to_string(), |pct| format!("{pct}%"));
    format!(
        "  {:<22} {:<8} {:>6} {:>6} {:>8} {:>6} {:>6}",
        r.caller, r.pool, r.ok, r.not_modified, r.rate_limited, r.error, hit
    )
}

fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

fn render_budget(b: &ForgeBudgetReading, own: Option<u64>, now: DateTime<Utc>) -> String {
    let resets = b
        .reset_at
        .map(|r| format!(", resets in {}", ago((r - now).num_seconds())))
        .unwrap_or_default();
    let source = if b.source == "headers" {
        "headers"
    } else {
        "breaker probe"
    };
    let seen = ago((now - b.observed_at).num_seconds());
    // Attribution (Issue #9855): the pool's spend by every client of the
    // credential, split into this host's own consumption and the external
    // share (other machines, agents, apps — the number that answered nothing
    // during the 2026-10-01 exhaustion).
    let attribution = match (b.used, own) {
        (Some(used), Some(consumed)) => format!(
            ", used {} (own≈{}, external≈{})",
            used,
            consumed,
            used.saturating_sub(consumed)
        ),
        (Some(used), None) => format!(", used {} (own n/a — sink off)", used),
        (None, _) => String::new(),
    };
    format!(
        "{} {} left{attribution}{resets} ({source}, {seen} ago)",
        b.pool, b.remaining
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_daemon::types::ForgeCallsStatus;

    fn report(fc: Option<ForgeCallsStatus>) -> DaemonStatusReport {
        DaemonStatusReport {
            forge_calls: fc.map(Box::new),
            ..Default::default()
        }
    }

    fn row(caller: &str, ok: u64, not_modified: u64) -> ForgeCallCounts {
        ForgeCallCounts {
            caller: caller.into(),
            pool: "core".into(),
            ok,
            not_modified,
            ..Default::default()
        }
    }

    #[test]
    fn a_pre_9251_daemon_renders_nothing() {
        assert!(render_forge_calls_lines(&report(None), Utc::now()).is_empty());
    }

    #[test]
    fn renders_per_caller_counts_hit_ratio_totals_and_budget() {
        let now = Utc::now();
        let fc = ForgeCallsStatus {
            window_secs: 3600,
            host_window: Some(vec![row("work_finder", 1, 3), row("role_collision", 0, 0)]),
            since_start: vec![row("work_finder", 2, 5)],
            since: Some(now),
            budget: vec![ForgeBudgetReading {
                pool: "core".into(),
                remaining: 4034,
                used: None,
                reset_at: Some(now + chrono::Duration::minutes(30)),
                observed_at: now - chrono::Duration::seconds(12),
                source: "headers".into(),
            }],
            own_window: None,
        };
        let lines = render_forge_calls_lines(&report(Some(fc)), now);
        let text = lines.join("\n");
        assert!(lines[0].starts_with("Forge calls (last 1h, host-wide):"), "{text}");
        let wf = lines.iter().find(|l| l.contains("work_finder")).unwrap();
        assert!(wf.contains("core") && wf.trim_end().ends_with("75%"), "{wf}");
        let rc = lines.iter().find(|l| l.contains("role_collision")).unwrap();
        assert!(rc.trim_end().ends_with('-'), "{rc}");
        assert!(text.contains("2 × 200, 5 × 304, 0 limited, 0 error"), "{text}");
        assert!(
            text.contains("budget: core 4034 left, resets in 30m (headers, 12s ago)"),
            "{text}"
        );
    }

    #[test]
    fn budget_line_splits_used_into_own_and_external_when_known() {
        let now = Utc::now();
        let fc = ForgeCallsStatus {
            window_secs: 3600,
            budget: vec![ForgeBudgetReading {
                pool: "core".into(),
                remaining: 40,
                used: Some(4_960),
                reset_at: Some(now + chrono::Duration::minutes(9)),
                observed_at: now - chrono::Duration::seconds(5),
                source: "headers".into(),
            }],
            own_window: Some(vec![loom_daemon::types::ForgePoolSpend {
                pool: "core".into(),
                consumed: 36,
            }]),
            ..Default::default()
        };
        let lines = render_forge_calls_lines(&report(Some(fc)), now);
        let budget = lines.iter().find(|l| l.contains("budget:")).unwrap();
        assert!(budget.contains("core 40 left"), "{budget}");
        assert!(budget.contains("used 4960 (own≈36, external≈4924)"), "{budget}");
    }

    #[test]
    fn budget_line_marks_own_unknown_when_the_sink_is_off() {
        let now = Utc::now();
        let fc = ForgeCallsStatus {
            window_secs: 3600,
            budget: vec![ForgeBudgetReading {
                pool: "core".into(),
                remaining: 40,
                used: Some(4_960),
                reset_at: None,
                observed_at: now,
                source: "breaker_probe".into(),
            }],
            own_window: None,
            ..Default::default()
        };
        let lines = render_forge_calls_lines(&report(Some(fc)), now);
        let budget = lines.iter().find(|l| l.contains("budget:")).unwrap();
        assert!(budget.contains("used 4960 (own n/a — sink off)"), "{budget}");
    }

    #[test]
    fn a_disabled_sink_says_so() {
        let fc = ForgeCallsStatus {
            window_secs: 3600,
            ..Default::default()
        };
        let lines = render_forge_calls_lines(&report(Some(fc)), Utc::now());
        assert!(lines[0].contains("unavailable"), "{lines:?}");
    }
}
