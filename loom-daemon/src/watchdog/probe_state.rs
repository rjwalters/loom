//! Memory across ticks for the IPC probe: a consecutive-failure streak (#4398)
//! and a rolling window (#5944).
//!
//! The watchdog owns no long-lived process — the supervisor re-runs it every
//! interval — so anything it remembers lives in a file.
//!
//! **Both are keyed to the live pid, and that is load-bearing.** A streak or a
//! window describes one daemon process. When the pid changes, the daemon was
//! restarted and the old process's failures say nothing about the new one:
//! carrying them over would let a fresh daemon inherit a nearly-tripped breaker
//! and get reported as confirmed-wedged on its first bad tick. So a pid
//! mismatch reads as "no history", not as an error.

use std::path::Path;

/// The consecutive-failure streak for `pid`, or 0 when there is no usable
/// record — absent file, unreadable, a different pid, or a malformed count.
#[must_use]
pub fn fail_streak(path: &Path, pid: u32) -> u64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let mut it = text.split_whitespace();
    match (it.next(), it.next()) {
        (Some(saved_pid), Some(count)) if saved_pid == pid.to_string() => {
            count.parse().unwrap_or(0)
        }
        _ => 0,
    }
}

/// Record the streak for `pid`.
pub fn write_fail_streak(path: &Path, pid: u32, count: u64) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{pid} {count}\n"));
}

/// Forget the streak. Called on a healthy round-trip.
pub fn clear_fail_streak(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The rolling window's outcome after recording this tick.
pub struct Window {
    /// Ticks retained, at most `window_ticks`. Fewer early on — the rate signal
    /// is deliberately reported over the ticks actually observed rather than
    /// assuming an unobserved past was healthy.
    pub len: u64,
    /// Failures among them.
    pub fails: u64,
}

/// Record this tick's outcome and return the window.
///
/// History is a string of `0` (healthy) and `1` (unresponsive), oldest first,
/// truncated to the newest `window_ticks`.
pub fn record(path: &Path, pid: u32, unresponsive: bool, window_ticks: u64) -> Window {
    let previous = read_history(path, pid);
    let mut hist = previous;
    hist.push(if unresponsive { '1' } else { '0' });

    let ticks = window_ticks.max(1) as usize;
    if hist.len() > ticks {
        hist = hist.split_off(hist.len() - ticks);
    }

    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{pid} {hist}\n"));

    Window {
        len: hist.len() as u64,
        fails: hist.chars().filter(|c| *c == '1').count() as u64,
    }
}

/// The recorded history for `pid`, or empty when it belongs to another process.
fn read_history(path: &Path, pid: u32) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let mut it = text.split_whitespace();
    match (it.next(), it.next()) {
        (Some(saved_pid), Some(hist)) if saved_pid == pid.to_string() => {
            // Anything that is not a recorded outcome is dropped rather than
            // counted: a corrupt file must not manufacture failures.
            hist.chars().filter(|c| *c == '0' || *c == '1').collect()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("nested").join("state");
        (d, p)
    }

    #[test]
    fn a_streak_round_trips() {
        let (_d, p) = tmp();
        write_fail_streak(&p, 42, 3);
        assert_eq!(fail_streak(&p, 42), 3);
    }

    #[test]
    fn a_streak_from_another_pid_is_not_inherited() {
        // A restarted daemon must not inherit a nearly-tripped breaker and get
        // reported as confirmed-wedged on its first bad tick.
        let (_d, p) = tmp();
        write_fail_streak(&p, 42, 2);
        assert_eq!(fail_streak(&p, 99), 0);
    }

    #[test]
    fn a_missing_or_corrupt_streak_file_reads_as_zero() {
        let (_d, p) = tmp();
        assert_eq!(fail_streak(&p, 42), 0);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "garbage\n").unwrap();
        assert_eq!(fail_streak(&p, 42), 0);
        std::fs::write(&p, "42 notanumber\n").unwrap();
        assert_eq!(fail_streak(&p, 42), 0);
    }

    #[test]
    fn the_window_keeps_the_newest_ticks_only() {
        let (_d, p) = tmp();
        // 8 ticks into a 6-tick window: the two oldest roll off.
        for unresponsive in [true, true, false, false, false, false, false, false] {
            record(&p, 7, unresponsive, 6);
        }
        let w = record(&p, 7, false, 6);
        assert_eq!(w.len, 6);
        assert_eq!(w.fails, 0, "both failures aged out of the window");
    }

    #[test]
    fn the_window_counts_failures_among_the_retained_ticks() {
        let (_d, p) = tmp();
        let mut last = None;
        for unresponsive in [true, false, true, false, true] {
            last = Some(record(&p, 7, unresponsive, 6));
        }
        let w = last.expect("recorded");
        assert_eq!(w.len, 5);
        assert_eq!(w.fails, 3, "an intermittent probe: never 3 consecutive, but 3 of 5");
    }

    #[test]
    fn the_window_reports_over_ticks_actually_observed() {
        // Early on there are fewer than `window_ticks` samples. The rate is
        // reported over what was seen, not over an assumed-healthy past.
        let (_d, p) = tmp();
        let w = record(&p, 7, true, 6);
        assert_eq!(w.len, 1);
        assert_eq!(w.fails, 1);
    }

    #[test]
    fn a_window_from_another_pid_starts_fresh() {
        let (_d, p) = tmp();
        for _ in 0..4 {
            record(&p, 7, true, 6);
        }
        let w = record(&p, 8, false, 6);
        assert_eq!(w.len, 1, "a new process starts its own window");
        assert_eq!(w.fails, 0);
    }

    #[test]
    fn a_corrupt_history_does_not_manufacture_failures() {
        let (_d, p) = tmp();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "7 1x1!!0\n").unwrap();
        let w = record(&p, 7, false, 6);
        // Only the real outcomes survive: 1,1,0 plus this tick's 0.
        assert_eq!(w.len, 4);
        assert_eq!(w.fails, 2);
    }

    #[test]
    fn a_window_of_one_still_works() {
        let (_d, p) = tmp();
        record(&p, 7, true, 1);
        let w = record(&p, 7, false, 1);
        assert_eq!(w.len, 1);
        assert_eq!(w.fails, 0);
    }

    #[test]
    fn a_zero_window_is_treated_as_one_rather_than_dividing_by_nothing() {
        let (_d, p) = tmp();
        let w = record(&p, 7, true, 0);
        assert_eq!(w.len, 1, "a misconfigured window must not retain nothing");
        assert_eq!(w.fails, 1);
    }
}
