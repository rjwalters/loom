//! The roll-schedule lines under `Auto-update loop:` on `loom-daemon status`
//! (#9132). A sibling module because `status_render.rs` is a frozen
//! over-threshold ledger entry.
//!
//! Renders the last tick's note (unchanged from before) and, only when a roll
//! window is configured, the schedule: whether a window is open, when the next one
//! opens, the roll target, whether an update drain holds dispatch paused, and the
//! last deferral reason — so a scheduled wait reads differently from a stall.
//! With no window configured the output is byte-identical to before.

use loom_daemon::auto_update::roll_window::RollWindowStatus;
use loom_daemon::types::DaemonStatusReport;

/// Print the `last tick:` note and, when scheduled, the roll-window lines.
pub fn print_tail(report: &DaemonStatusReport) {
    if let Some(note) = &report.auto_update_note {
        println!("  last tick: {note}");
    }
    for line in render(report.auto_update_roll_window.as_ref()) {
        println!("  {line}");
    }
}

/// Pure rendering of the schedule lines (empty when no window is configured).
#[must_use]
pub fn render(window: Option<&RollWindowStatus>) -> Vec<String> {
    let Some(w) = window else {
        return Vec::new();
    };
    let next = w.next_window_open.format("%Y-%m-%dT%H:%M:%SZ");
    let mut lines = vec![format!(
        "roll window: every {}s (host offset {}s) — {}next opens {next}; restart path: {}",
        w.period_secs,
        w.offset_secs,
        if w.window_open_now {
            "OPEN now, "
        } else {
            "closed, "
        },
        w.restart_path,
    )];
    lines.push(format!(
        "roll target: {}; dispatch paused by update: {}",
        w.roll_target.as_deref().unwrap_or("none"),
        if w.dispatch_paused_by_update {
            "yes"
        } else {
            "no"
        },
    ));
    if let Some(reason) = &w.deferral {
        lines.push(format!("roll deferral: {reason}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn window() -> RollWindowStatus {
        RollWindowStatus {
            period_secs: 21_600,
            offset_secs: 3_600,
            next_window_open: Utc.with_ymd_and_hms(2026, 10, 4, 7, 0, 0).unwrap(),
            window_open_now: false,
            roll_target: Some("0.19.654".to_string()),
            dispatch_paused_by_update: false,
            deferral: Some("scheduled wait: 0.19.654 waits for the roll window".to_string()),
            restart_path: "bounded_drain".to_string(),
        }
    }

    #[test]
    fn no_window_renders_nothing() {
        assert!(render(None).is_empty());
    }

    #[test]
    fn a_scheduled_wait_shows_next_window_target_pause_flag_and_reason() {
        let lines = render(Some(&window()));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("every 21600s"), "{}", lines[0]);
        assert!(lines[0].contains("closed, next opens 2026-10-04T07:00:00Z"), "{}", lines[0]);
        assert!(lines[0].contains("restart path: bounded_drain"), "{}", lines[0]);
        assert_eq!(lines[1], "roll target: 0.19.654; dispatch paused by update: no");
        assert!(lines[2].starts_with("roll deferral: scheduled wait"), "{}", lines[2]);
    }

    #[test]
    fn an_open_window_with_a_paused_dispatch_and_no_deferral() {
        let mut w = window();
        w.window_open_now = true;
        w.dispatch_paused_by_update = true;
        w.deferral = None;
        let lines = render(Some(&w));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("OPEN now"));
        assert!(lines[1].ends_with("dispatch paused by update: yes"));
    }

    #[test]
    fn a_timed_out_drain_deferral_is_distinguishable_from_a_scheduled_wait() {
        let mut w = window();
        w.deferral = Some("drain timed out, waiting for next window: 0.19.654".to_string());
        let lines = render(Some(&w));
        assert!(lines[2].contains("drain timed out"));
        assert!(!lines[2].contains("scheduled wait"));
    }
}
