//! The `Pending restart:` line on `loom-daemon status` (#9597).
//!
//! A sibling module because `status_render.rs` is a frozen over-threshold
//! ledger entry, matching how [`super::fleet_store_line`] and
//! [`super::operator_priority_line`] already live here.
//!
//! **Read client-side, not over IPC.** The marker this renders
//! (`<loom_dir>/fleet-config-pending-restart.json`, written by
//! `fleet-config render` via
//! [`loom_daemon::fleet_store::pending_restart`]) is a fact about the host,
//! reconciled here against the CURRENTLY-answering daemon's own pid: a
//! different pid than the one the marker recorded means the daemon that saw
//! the drift has already restarted, so the pending state is resolved and the
//! marker is cleared as a side effect of printing status. This is the read
//! side of #9597's "restart-required changes show up in `status` until
//! actually applied" acceptance criterion.
//!
//! **Silent by default.** A host that has never rendered a restart-required
//! change (or whose daemon has already restarted since) has no marker, so
//! both entry points below render nothing and `status` output is
//! byte-identical to a build without this feature.

use loom_daemon::fleet_store::pending_restart::{self, PendingRestart};

/// Print the line, or nothing when there is no pending restart against the
/// currently-answering daemon.
pub fn print(current_daemon_pid: Option<u32>) {
    if let Some(line) = render(current_daemon_pid, chrono::Utc::now()) {
        println!("{line}");
    }
}

/// Pure rendering, split out from [`print`] (which also reconciles/clears
/// the on-disk marker) so the message shape is unit-testable without
/// touching a real file.
#[must_use]
fn render_marker(marker: &PendingRestart, now: chrono::DateTime<chrono::Utc>) -> String {
    let age_mins = (now - marker.rendered_at).num_minutes().max(0);
    format!(
        "Pending restart: {} — rendered {age_mins}m ago (daemon pid {}); restart the daemon to \
         apply",
        marker.paths.join(", "),
        marker.observed_pid
    )
}

/// [`pending_restart::reconcile`] against `current_daemon_pid`, then render —
/// the reconcile call is what clears a resolved marker (a different pid now
/// answering) as a side effect, so a subsequent `status` never repeats it.
fn render(current_daemon_pid: Option<u32>, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    pending_restart::reconcile(current_daemon_pid).map(|m| render_marker(&m, now))
}

/// The `--json` counterpart: the still-pending marker verbatim (after the
/// same reconcile), or `Null` when there is none. Callers assign it to a
/// `pending_restart` key only when non-null, so the payload of a host
/// without this feature is unchanged.
#[must_use]
pub fn json(current_daemon_pid: Option<u32>) -> serde_json::Value {
    pending_restart::reconcile(current_daemon_pid)
        .and_then(|m| serde_json::to_value(m).ok())
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn renders_paths_pid_and_age() {
        let marker = PendingRestart {
            paths: vec!["autonomous.hostBreaker.enabled".to_string()],
            observed_pid: 4242,
            rendered_at: at(2026, 9, 30, 12, 0, 0),
        };
        let line = render_marker(&marker, at(2026, 9, 30, 12, 5, 0));
        assert!(line.starts_with("Pending restart: autonomous.hostBreaker.enabled"), "{line}");
        assert!(line.contains("5m ago"), "{line}");
        assert!(line.contains("pid 4242"), "{line}");
    }
}
