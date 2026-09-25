//! `loom-daemon status`'s drain block — the JSON object and the human line
//! that explain a host sitting paused behind a drain-and-restart roll (#8514).
//!
//! Before #8514 the only evidence was `drain.note`: written once, at the
//! instant of a transition, and overwritten by the next one. An operator asking
//! "is this host idle because of a roll, and for how long?" had to catch that
//! note live or read the daemon log. The `roll` sub-object below answers it
//! from any single `status --json`, and [`roll_line`] says the same thing in
//! the human renderer.
//!
//! Rendering lives here rather than in `status_render.rs` (over
//! `.loom/docs/file-size-policy.md`'s threshold, and frozen) and returns its
//! line instead of printing it, so tests assert on the text directly —
//! the same shape as the [`super::holds`] sibling.

use loom_daemon::types::DaemonStatusReport;

/// The `drain` object for `status --json`.
///
/// `draining`/`deadline`/`note` are unchanged from #4090 — scripted consumers
/// keep working verbatim. `roll` is #8514's addition: `null` whenever no drain
/// is active (and from a pre-#8514 daemon, which never computed one).
#[must_use]
pub fn drain_json(report: &DaemonStatusReport) -> serde_json::Value {
    serde_json::json!({
        "draining": report.draining,
        "deadline": report.drain_deadline,
        "note": report.drain_note,
        "roll": report.drain_roll.as_ref().map(|r| serde_json::json!({
            "roll_pending": r.roll_pending,
            "started_at": r.started_at,
            "paused_secs": r.paused_secs,
            "budget_secs": r.budget_secs,
            "refusals": r.refusals,
            "in_flight": r.in_flight,
            "target": r.target,
            "then_exit": r.then_exit,
        })),
        // #8652: `{ "YYYY-MM-DD": secs }`, UTC days, live pause included. `{}`
        // when nothing was recorded (and from a pre-#8652 daemon).
        "paused_by_day": report.drain_paused_by_day,
    })
}

/// The human line for #8652's per-day paused totals: today, the trailing 7
/// days, and the whole retained window. `None` when the ledger is empty, so an
/// idle host that never rolled prints nothing new.
#[must_use]
pub fn paused_by_day_line(report: &DaemonStatusReport) -> Option<String> {
    let days = &report.drain_paused_by_day;
    if days.is_empty() {
        return None;
    }
    let today = chrono::Utc::now().date_naive();
    let since = |n: u64| {
        today
            .checked_sub_days(chrono::Days::new(n))
            .unwrap_or(today)
    };
    let sum_from = |from: chrono::NaiveDate| days.range(from..).map(|(_, s)| *s).sum::<u64>();
    Some(format!(
        "Roll pauses (UTC): today {}, last 7d {}, last {} recorded day(s) {}",
        human_secs(days.get(&today).copied().unwrap_or(0)),
        human_secs(sum_from(since(6))),
        days.len(),
        human_secs(days.values().sum()),
    ))
}

/// The human line for an in-progress roll, or `None` when the report carries no
/// live roll state (no drain active, or a pre-#8514 daemon).
#[must_use]
pub fn roll_line(report: &DaemonStatusReport) -> Option<String> {
    let roll = report.drain_roll.as_ref()?;
    let state = if roll.roll_pending {
        "PENDING"
    } else {
        "armed"
    };
    let since = roll
        .started_at
        .map_or_else(|| "unknown start".to_string(), |t| format!("since {t}"));
    let budget = if roll.budget_secs > 0 {
        format!(" of a {} budget", human_secs(roll.budget_secs))
    } else {
        String::new()
    };
    let target = roll.target.as_deref().map_or_else(
        || " [no artifact target — not an auto-update roll]".to_string(),
        |t| format!(" [target {t}]"),
    );
    Some(format!(
        "       roll {state} {since} — dispatch paused {}{budget}, {} in flight, {} refusal(s){target}",
        human_secs(roll.paused_secs),
        roll.in_flight,
        roll.refusals,
    ))
}

/// `3720` → `1h2m`. Compact because this is appended to an already-long line.
fn human_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use loom_daemon::ipc::DrainRollStatus;

    fn report_with(roll: Option<DrainRollStatus>) -> DaemonStatusReport {
        let mut report = crate::cli::status::sample_report::sample_report();
        report.draining = roll.is_some();
        report.drain_roll = roll;
        report
    }

    fn pending_roll() -> DrainRollStatus {
        DrainRollStatus {
            roll_pending: true,
            started_at: Some(
                chrono::DateTime::parse_from_rfc3339("2026-09-22T12:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ),
            paused_secs: 3720,
            budget_secs: 7200,
            refusals: 2,
            in_flight: 3,
            target: Some("v0.19.24@abcd".to_string()),
            then_exit: false,
        }
    }

    #[test]
    fn json_carries_every_live_roll_field() {
        let value = drain_json(&report_with(Some(pending_roll())));
        let roll = &value["roll"];
        assert_eq!(roll["roll_pending"], serde_json::json!(true));
        assert_eq!(roll["paused_secs"], serde_json::json!(3720));
        assert_eq!(roll["budget_secs"], serde_json::json!(7200));
        assert_eq!(roll["refusals"], serde_json::json!(2));
        assert_eq!(roll["in_flight"], serde_json::json!(3));
        assert_eq!(roll["target"], serde_json::json!("v0.19.24@abcd"));
        assert_eq!(roll["then_exit"], serde_json::json!(false));
        assert!(roll["started_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-09-22T12:00:00"));
    }

    #[test]
    fn the_pre_8514_fields_are_untouched_and_roll_is_null_when_idle() {
        let mut report = report_with(None);
        report.drain_note = Some("drain aborted by operator — dispatch resumed".to_string());
        let value = drain_json(&report);
        assert_eq!(value["draining"], serde_json::json!(false));
        assert_eq!(value["roll"], serde_json::Value::Null);
        assert_eq!(
            value["note"],
            serde_json::json!("drain aborted by operator — dispatch resumed")
        );
    }

    #[test]
    fn the_human_line_names_the_pause_duration_budget_and_target() {
        let line = roll_line(&report_with(Some(pending_roll()))).unwrap();
        assert!(line.contains("roll PENDING"), "{line}");
        assert!(line.contains("since 2026-09-22 12:00:00"), "{line}");
        assert!(line.contains("dispatch paused 1h2m of a 2h0m budget"), "{line}");
        assert!(line.contains("3 in flight"), "{line}");
        assert!(line.contains("2 refusal(s)"), "{line}");
        assert!(line.contains("target v0.19.24@abcd"), "{line}");
    }

    #[test]
    fn a_first_attempt_roll_does_not_claim_to_be_pending() {
        let mut roll = pending_roll();
        roll.roll_pending = false;
        roll.refusals = 0;
        roll.paused_secs = 45;
        roll.target = None;
        let line = roll_line(&report_with(Some(roll))).unwrap();
        assert!(line.contains("roll armed"), "{line}");
        assert!(!line.contains("PENDING"), "{line}");
        assert!(line.contains("dispatch paused 45s"), "{line}");
        assert!(line.contains("no artifact target"), "{line}");
    }

    #[test]
    fn no_live_roll_renders_no_line() {
        assert!(roll_line(&report_with(None)).is_none());
    }

    #[test]
    fn json_carries_paused_by_day_keyed_by_utc_date() {
        let mut report = report_with(None);
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        report.drain_paused_by_day.insert(day, 3000);
        let value = drain_json(&report);
        assert_eq!(value["paused_by_day"]["2026-09-22"], serde_json::json!(3000));
    }

    #[test]
    fn paused_by_day_is_absent_safe_from_a_pre_8652_daemon() {
        // A pre-#8652 daemon's report JSON has no such key at all.
        let mut wire = serde_json::to_value(report_with(None)).unwrap();
        wire.as_object_mut().unwrap().remove("drain_paused_by_day");
        let report: DaemonStatusReport = serde_json::from_value(wire).unwrap();
        assert!(report.drain_paused_by_day.is_empty());
        assert_eq!(drain_json(&report)["paused_by_day"], serde_json::json!({}));
        assert!(paused_by_day_line(&report).is_none());
    }

    #[test]
    fn the_paused_by_day_line_sums_today_the_week_and_the_window() {
        let mut report = report_with(None);
        let today = chrono::Utc::now().date_naive();
        let ago = |n| today.checked_sub_days(chrono::Days::new(n)).unwrap();
        report.drain_paused_by_day.insert(today, 3720);
        report.drain_paused_by_day.insert(ago(3), 600);
        report.drain_paused_by_day.insert(ago(20), 7200);
        let line = paused_by_day_line(&report).unwrap();
        assert!(line.contains("today 1h2m"), "{line}");
        assert!(line.contains("last 7d 1h12m"), "{line}");
        assert!(line.contains("last 3 recorded day(s) 3h12m"), "{line}");
    }

    #[test]
    fn durations_render_compactly() {
        assert_eq!(human_secs(0), "0s");
        assert_eq!(human_secs(45), "45s");
        assert_eq!(human_secs(125), "2m5s");
        assert_eq!(human_secs(7200), "2h0m");
    }
}
