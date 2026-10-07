//! `loom-daemon health`'s `session_containers` section (issue #10600):
//! whether this host's Codex session containers can take work.
//!
//! Conditional, like the `codex` section: a host without a session-managed
//! Codex account (no [`crate::session_status::SessionContainersReport`] on
//! the status payload) renders no section, so its report is unchanged. The
//! verdict is the status section's own: degraded when a seat is not running,
//! has stale mounts or a standing drift-removal record, or when the
//! containers cannot be observed. An operator-held seat never degrades it.

use super::{HealthInputs, HealthSection, Verdict};

/// The section, or `None` when the daemon reported no session seats.
#[must_use]
pub fn assess(inputs: &HealthInputs) -> Option<HealthSection> {
    let report = inputs.status.as_ref()?.session_containers.as_ref()?;
    let seats = report.accounts.len();
    let (verdict, summary) = match &report.degraded_reason {
        Some(reason) => (Verdict::Degraded, reason.clone()),
        None => (Verdict::Green, format!("all {seats} Codex session seat(s) serving")),
    };
    Some(HealthSection {
        key: "session_containers",
        verdict,
        summary,
        detail: serde_json::to_value(report).unwrap_or_default(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::session_status::{SeatStatus, SessionContainersReport};
    use crate::types::DaemonStatusReport;

    fn inputs_with(report: Option<SessionContainersReport>) -> HealthInputs {
        HealthInputs {
            status: Some(DaemonStatusReport {
                session_containers: report,
                ..DaemonStatusReport::default()
            }),
            ..HealthInputs::default()
        }
    }

    #[test]
    fn no_seats_no_section() {
        assert!(assess(&inputs_with(None)).is_none());
        assert!(assess(&HealthInputs::default()).is_none(), "daemon unreachable");
    }

    #[test]
    fn a_down_seat_degrades_naming_it() {
        let report = SessionContainersReport {
            degraded: true,
            degraded_reason: Some("1 of 1 session seat(s) not serving: agent-1 stopped".into()),
            accounts: vec![SeatStatus::default()],
            ..SessionContainersReport::default()
        };
        let section = assess(&inputs_with(Some(report))).unwrap();
        assert_eq!(section.key, "session_containers");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("agent-1 stopped"));
        assert_eq!(section.detail["degraded"], true);
    }

    #[test]
    fn serving_seats_are_green() {
        let report = SessionContainersReport {
            accounts: vec![SeatStatus::default(), SeatStatus::default()],
            ..SessionContainersReport::default()
        };
        let section = assess(&inputs_with(Some(report))).unwrap();
        assert_eq!(section.verdict, Verdict::Green);
        assert_eq!(section.summary, "all 2 Codex session seat(s) serving");
    }

    #[test]
    fn the_overall_report_degrades_with_it() {
        let mut inputs = crate::health::tests::healthy_inputs();
        let green = crate::health::assess(&inputs).overall;
        inputs.status.as_mut().unwrap().session_containers = Some(SessionContainersReport {
            degraded: true,
            degraded_reason: Some("session containers unobservable: no snapshot".into()),
            ..SessionContainersReport::default()
        });
        let report = crate::health::assess(&inputs);
        assert_eq!(green, Verdict::Green);
        assert_eq!(report.overall, Verdict::Degraded);
        assert!(report
            .sections
            .iter()
            .any(|s| s.key == "session_containers"));
    }
}
