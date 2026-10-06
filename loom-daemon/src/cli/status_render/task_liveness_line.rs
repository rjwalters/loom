//! The `Task liveness:` lines on `loom-daemon status` (#10414). A sibling
//! module because `status_render.rs` is a frozen over-threshold ledger entry.
//!
//! One line per registered long-running loop. A loop that went silent past its
//! staleness window, or marked itself dead, reads `DEAD`, with how long it has
//! been silent and why. That is the state that went unseen on 2026-10-05.
//! Nothing is printed when no loop registered (a pre-#10414 daemon, or every
//! loop configured off).

use loom_daemon::task_liveness::TaskLivenessEntry;

/// Print the block (nothing when `entries` is empty).
pub fn print(entries: &[TaskLivenessEntry]) {
    for line in render(entries) {
        println!("{line}");
    }
}

/// Pure rendering of the block.
#[must_use]
pub fn render(entries: &[TaskLivenessEntry]) -> Vec<String> {
    if entries.is_empty() {
        return Vec::new();
    }
    let dead = entries.iter().filter(|e| !e.alive).count();
    let mut lines = vec![if dead == 0 {
        format!("Task liveness: all {} long-running loop(s) alive", entries.len())
    } else {
        format!("Task liveness: {dead} of {} long-running loop(s) DEAD", entries.len())
    }];
    for e in entries {
        let beat = e.last_beat.map_or_else(
            || "never beat".to_string(),
            |at| format!("last beat {}", at.format("%Y-%m-%dT%H:%M:%SZ")),
        );
        let state = if e.alive { "alive" } else { "DEAD" };
        let mut line = format!(
            "  {}: {state} ({beat}, silent {}s, window {}s)",
            e.task, e.silent_secs, e.stale_after_secs
        );
        if let Some(reason) = &e.dead_reason {
            line.push_str(&format!(" — {reason}"));
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn entry(task: &str, alive: bool) -> TaskLivenessEntry {
        TaskLivenessEntry {
            task: task.to_string(),
            alive,
            last_beat: Some(Utc.with_ymd_and_hms(2026, 10, 5, 1, 30, 0).unwrap()),
            silent_secs: if alive { 30 } else { 40_000 },
            interval_secs: 900,
            stale_after_secs: 3_660,
            dead_reason: None,
        }
    }

    #[test]
    fn nothing_registered_prints_nothing() {
        assert!(render(&[]).is_empty());
    }

    #[test]
    fn a_silent_loop_reads_dead_with_its_silence() {
        let lines = render(&[entry("auto_update", false), entry("eta_pass", true)]);
        assert_eq!(lines[0], "Task liveness: 1 of 2 long-running loop(s) DEAD");
        assert_eq!(
            lines[1],
            "  auto_update: DEAD (last beat 2026-10-05T01:30:00Z, silent 40000s, window 3660s)"
        );
        assert!(lines[2].starts_with("  eta_pass: alive"));
    }

    #[test]
    fn a_self_declared_death_names_its_reason() {
        let mut e = entry("auto_update", false);
        e.dead_reason = Some("tick task failed".to_string());
        assert!(render(&[e])[1].ends_with(" — tick task failed"));
    }
}
