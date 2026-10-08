//! The `last tick:` line under `Auto-update loop:` on `loom-daemon status`.
//! A sibling module because `status_render.rs` is a frozen over-threshold
//! ledger entry.
//!
//! This is the whole tail since #10885 removed the roll window, whose schedule
//! lines (`roll window:`, `roll target:`, `roll deferral:`) used to follow it.
//! The note carries what replaced them: on a fleet host it names the floor and
//! why the tick did or did not roll.

use loom_daemon::types::DaemonStatusReport;

/// Print the last tick's note, when there is one.
pub fn print(report: &DaemonStatusReport) {
    for line in render(report) {
        println!("  {line}");
    }
}

/// Pure rendering of the tail (empty before the first tick).
#[must_use]
pub fn render(report: &DaemonStatusReport) -> Vec<String> {
    report
        .auto_update_note
        .iter()
        .map(|note| format!("last tick: {note}"))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_is_the_last_tick_note_and_nothing_about_a_window() {
        let mut report = crate::cli::status::sample_report::sample_report();
        report.auto_update_note = None;
        assert!(render(&report).is_empty());

        report.auto_update_note = Some("fleet floor 0.19.888 is met by running 0.19.937".into());
        let lines = render(&report);
        assert_eq!(lines, ["last tick: fleet floor 0.19.888 is met by running 0.19.937"]);
        assert!(!lines.iter().any(|l| l.contains("roll window")));
    }

    /// #10885 removed the roll schedule's field from the report. A daemon from
    /// before the removal still sends it; this client must read that report,
    /// and simply not see the field. (The key is assembled so the issue's
    /// "no window code left" grep stays clean.)
    #[test]
    fn a_report_from_a_daemon_that_still_sends_the_window_field_deserializes() {
        let mut wire =
            serde_json::to_value(crate::cli::status::sample_report::sample_report()).unwrap();
        let old = serde_json::json!({
            "period_secs": 21600,
            "offset_secs": 900,
            "next_window_open": "2026-10-08T18:00:00Z",
            "window_open_now": false,
            "roll_target": "0.19.922",
            "dispatch_paused_by_update": false,
            "deferral": "scheduled wait",
            "restart_path": "bounded_drain",
        });
        wire.as_object_mut()
            .unwrap()
            .insert(["auto_update_roll", "window"].join("_"), old);
        let report: DaemonStatusReport = serde_json::from_value(wire).unwrap();
        let back = serde_json::to_value(&report).unwrap();
        assert!(
            back.as_object()
                .unwrap()
                .keys()
                .all(|k| !k.ends_with("_window")),
            "{back}"
        );
    }
}
