//! The `Session containers:` block on `loom-daemon status` (#10600). A
//! sibling module because `status_render.rs` is a frozen over-threshold
//! ledger entry.
//!
//! One header line (healthy, or `DEGRADED` with the one-line reason), then one
//! line per session-managed Codex account: state, mounts, posture, an
//! operator hold, a drift-removal record and the reconciler's last action.
//! Nothing is printed on a host without a session-managed account.

use loom_daemon::session_status::SessionContainersReport;

/// Print the block (nothing when `report` is `None`).
pub fn print(report: Option<&SessionContainersReport>) {
    for line in render(report) {
        println!("{line}");
    }
}

fn reconciler(report: &SessionContainersReport) -> &'static str {
    match report.reconciler_enabled {
        Some(true) => "reconciler on",
        Some(false) => "reconciler OFF (opted out)",
        None => "reconciler state unknown",
    }
}

/// Pure rendering of the block.
#[must_use]
pub fn render(report: Option<&SessionContainersReport>) -> Vec<String> {
    let Some(report) = report else {
        return Vec::new();
    };
    let seats = report.accounts.len();
    let header = match (&report.degraded_reason, report.snapshot_age_secs) {
        (Some(reason), _) => format!("Session containers: DEGRADED — {reason}"),
        (None, Some(age)) => format!(
            "Session containers: all {seats} seat(s) serving (snapshot {age}s old; {})",
            reconciler(report)
        ),
        (None, None) => format!("Session containers: all {seats} seat(s) serving"),
    };
    let mut lines = vec![header];
    if report.degraded_reason.is_some() {
        lines.push(format!("  ({})", reconciler(report)));
    }
    for seat in &report.accounts {
        lines.push(format!("  {}: {}", seat.account, seat.describe()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_daemon::session_status::{LastReconcile, SeatMounts, SeatRemoval, SeatStatus};
    use std::path::PathBuf;

    fn seat(account: &str, state: &str, verdict: &str) -> SeatStatus {
        SeatStatus {
            account: account.into(),
            container: format!("loom-codex-session-{account}"),
            state: state.into(),
            mounts: SeatMounts {
                verdict: verdict.into(),
                ..SeatMounts::default()
            },
            posture: "host".into(),
            ..SeatStatus::default()
        }
    }

    #[test]
    fn no_seat_prints_nothing() {
        assert!(render(None).is_empty());
    }

    #[test]
    fn a_healthy_host_reads_serving() {
        let report = SessionContainersReport {
            observation: "available".into(),
            snapshot_age_secs: Some(12),
            reconciler_enabled: Some(true),
            accounts: vec![seat("agent-1", "running", "ok")],
            ..SessionContainersReport::default()
        };
        assert_eq!(
            render(Some(&report)),
            [
                "Session containers: all 1 seat(s) serving (snapshot 12s old; reconciler on)",
                "  agent-1: running, mounts ok, posture host, last reconcile: none",
            ]
        );
    }

    #[test]
    fn a_degraded_host_names_the_reason_and_every_seat() {
        let mut stale = seat("agent-2", "running", "stale");
        stale.mounts.denied = vec![PathBuf::from("/srv/secret")];
        stale.last_reconcile = Some(LastReconcile {
            action: "deferred (in-flight)".into(),
            at_unix: 0,
        });
        let mut removed = seat("agent-3", "missing", "n/a");
        removed.posture = "unverified".into();
        removed.removal = Some(SeatRemoval {
            denied: vec![PathBuf::from("/srv/secret")],
            ..SeatRemoval::default()
        });
        let mut held = seat("agent-4", "stopped", "ok");
        held.held = true;
        let report = SessionContainersReport {
            observation: "available".into(),
            degraded: true,
            degraded_reason: Some("2 of 3 session seat(s) not serving: …".into()),
            reconciler_enabled: Some(false),
            accounts: vec![stale, removed, held],
            ..SessionContainersReport::default()
        };
        let lines = render(Some(&report));
        assert_eq!(
            lines[0],
            "Session containers: DEGRADED — 2 of 3 session seat(s) not serving: …"
        );
        assert_eq!(lines[1], "  (reconciler OFF (opted out))");
        assert_eq!(
            lines[2],
            "  agent-2: running, mounts stale (missing 0, extra 0, denied 1), posture host, last \
             reconcile: deferred (in-flight)"
        );
        assert_eq!(
            lines[3],
            "  agent-3: missing, posture unverified, removed (denied mount: /srv/secret), last \
             reconcile: none"
        );
        assert_eq!(
            lines[4],
            "  agent-4: stopped, mounts ok, posture host, held (operator stop), last reconcile: none"
        );
    }
}
