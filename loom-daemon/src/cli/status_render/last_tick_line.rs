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
}
