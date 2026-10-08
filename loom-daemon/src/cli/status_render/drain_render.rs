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
            "origin": r.origin,
            "timed_out": r.timed_out,
            "startup_hold": r.startup_hold,
            // #10831: the H4 pause's step, budgets, per-reason requeue counts
            // and observed durations. `null` unless this is a pause roll.
            "pause": r.pause,
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
    // #9588: say WHY dispatch is paused before anything else.
    let state = if roll.startup_hold {
        "HELD at startup (operator stop on record — `restart --abort-drain` releases it)"
    } else if roll.timed_out {
        "TIMED OUT, dispatch held PAUSED until the stragglers finish (operator drain — \
         `--abort-drain` resumes, `--drain --force-after-timeout` cancels them)"
    } else if let Some(pause) = &roll.pause {
        // #10831: a pause roll does not wait for in-flight work; say what it
        // is doing instead.
        return Some(pause_line(roll, pause));
    } else if roll.roll_pending {
        // Only a pre-#10831 daemon still reports a retained roll.
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
    let origin = if roll.origin.is_empty() {
        String::new()
    } else {
        format!(" [origin: {}]", roll.origin)
    };
    Some(format!(
        "       roll {state} {since} — dispatch paused {}{budget}, {} in flight, {} refusal(s){target}{origin}",
        human_secs(roll.paused_secs),
        roll.in_flight,
        roll.refusals,
    ))
}

/// The human line for a pause roll's H4 pause (#10831).
fn pause_line(
    roll: &loom_daemon::ipc::DrainRollStatus,
    pause: &loom_daemon::ipc::PauseRollStatus,
) -> String {
    let requeued: u32 = pause.requeued_by_reason.values().sum();
    let reasons = if pause.requeued_by_reason.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            pause
                .requeued_by_reason
                .iter()
                .map(|(reason, n)| format!("{reason}: {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "       roll PAUSING agents (H4 step {}/10{}) — {} of a {} pause budget, {} agent(s): {} \
         paused, {requeued} requeued{reasons}, {} exited{}{}",
        pause.step,
        if pause.stopped {
            ", committed: cannot be aborted"
        } else {
            ", nothing stopped yet"
        },
        human_secs(roll.paused_secs),
        human_secs(pause.budget_secs),
        pause.items,
        pause.paused,
        pause.exited,
        pause
            .to_version
            .as_deref()
            .map_or_else(String::new, |v| format!(" [to {v}]")),
        pause
            .target_source
            .as_deref()
            .map_or_else(String::new, |s| format!(" [source: {s}]")),
    )
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
            origin: "auto-update".to_string(),
            timed_out: false,
            startup_hold: false,
            pause: None,
        }
    }

    /// #10831: a pause roll reports its H4 progress in `--json` and renders a
    /// line that says what it is doing, not "waiting for in-flight".
    #[test]
    fn a_pause_roll_reports_its_step_budget_and_per_reason_counts() {
        let mut roll = pending_roll();
        roll.roll_pending = false;
        roll.refusals = 0;
        roll.budget_secs = 120;
        roll.paused_secs = 40;
        roll.origin = "pause-roll".to_string();
        roll.pause = Some(loom_daemon::ipc::PauseRollStatus {
            step: 5,
            stopped: true,
            budget_secs: 120,
            items: 3,
            paused: 1,
            to_version: Some("0.19.900".to_string()),
            target_source: Some("floor".to_string()),
            requeued_by_reason: [("young-agent-reset".to_string(), 1)].into_iter().collect(),
            ..Default::default()
        });
        let value = drain_json(&report_with(Some(roll.clone())));
        assert_eq!(value["roll"]["roll_pending"], serde_json::json!(false));
        assert_eq!(value["roll"]["refusals"], serde_json::json!(0));
        assert_eq!(value["roll"]["pause"]["step"], serde_json::json!(5));
        assert_eq!(value["roll"]["pause"]["budget_secs"], serde_json::json!(120));
        assert_eq!(
            value["roll"]["pause"]["requeued_by_reason"]["young-agent-reset"],
            serde_json::json!(1)
        );
        let line = roll_line(&report_with(Some(roll))).unwrap();
        assert!(line.contains("PAUSING agents (H4 step 5/10, committed"), "{line}");
        assert!(line.contains("40s of a 2m0s pause budget"), "{line}");
        assert!(line.contains("young-agent-reset: 1"), "{line}");
        assert!(line.contains("[source: floor]"), "{line}");
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

    /// #9588: a timed-out operator drain says dispatch is HELD, why, and how
    /// to end it — in both the human line and `--json`.
    #[test]
    fn a_timed_out_operator_drain_says_it_is_held_paused() {
        let mut roll = pending_roll();
        roll.roll_pending = false;
        roll.target = None;
        roll.origin = "operator".to_string();
        roll.timed_out = true;
        let line = roll_line(&report_with(Some(roll.clone()))).unwrap();
        assert!(line.contains("TIMED OUT, dispatch held PAUSED"), "{line}");
        assert!(line.contains("--abort-drain"), "{line}");
        assert!(line.contains("[origin: operator]"), "{line}");
        let value = drain_json(&report_with(Some(roll)));
        assert_eq!(value["roll"]["timed_out"], serde_json::json!(true));
        assert_eq!(value["roll"]["origin"], serde_json::json!("operator"));
    }

    #[test]
    fn a_startup_hold_says_an_operator_stop_is_on_record() {
        let mut roll = pending_roll();
        roll.startup_hold = true;
        roll.origin = "operator".to_string();
        let line = roll_line(&report_with(Some(roll))).unwrap();
        assert!(line.contains("HELD at startup"), "{line}");
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
