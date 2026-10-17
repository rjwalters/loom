//! The `Host update:` line on `loom-daemon status` (#12420, epic #10698).
//!
//! One line, every `status`, whether or not the update loop runs: the host's
//! update state (H0–H5, H7 or `unknown`), the running and on-disk versions, the
//! fleet floor when one is set, and the roll target with who chose it. The
//! state is classified daemon-side
//! ([`loom_daemon::auto_update::update_observation`]); this only renders it.
//! The per-workspace resync lines are unchanged.

use loom_daemon::types::DaemonStatusReport;

/// The line for `report`, or `None` from a daemon that predates the field.
#[must_use]
pub fn render(report: &DaemonStatusReport) -> Option<String> {
    let host = report.host_update.as_ref()?;
    let mut line = format!(
        "Host update: {} ({}) — running {}, on disk {}",
        host.state.as_str(),
        host.state.label(),
        host.running_version,
        host.on_disk_version.as_deref().unwrap_or("unknown"),
    );
    if let Some(floor) = &host.floor {
        line.push_str(&format!(", floor {floor}"));
    }
    line.push_str(&format!(
        ", target {} (source {})",
        host.target_version.as_deref().unwrap_or("none"),
        host.target_source.as_str()
    ));
    Some(line)
}

/// Print the line, or nothing.
pub fn print(report: &DaemonStatusReport) {
    if let Some(line) = render(report) {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_daemon::auto_update::update_observation::{
        HostState, HostUpdateObservation, ObservedTargetSource,
    };

    fn report_with(host: Option<HostUpdateObservation>) -> DaemonStatusReport {
        let mut report = crate::cli::status::sample_report::sample_report();
        report.host_update = host;
        report
    }

    #[test]
    fn an_idle_up_to_date_host_gets_one_current_line() {
        let report = report_with(Some(HostUpdateObservation {
            state: HostState::Current,
            running_version: "0.19.10".into(),
            on_disk_version: Some("0.19.10".into()),
            floor: None,
            target_version: None,
            target_source: ObservedTargetSource::None,
        }));
        assert_eq!(
            render(&report).as_deref(),
            Some(
                "Host update: H0 (current) — running 0.19.10, on disk 0.19.10, target none \
                 (source none)"
            )
        );
    }

    #[test]
    fn a_pausing_host_shows_floor_target_and_source() {
        let report = report_with(Some(HostUpdateObservation {
            state: HostState::Pausing,
            running_version: "0.19.10".into(),
            on_disk_version: Some("0.19.11".into()),
            floor: Some("0.19.11".into()),
            target_version: Some("0.19.11".into()),
            target_source: ObservedTargetSource::Floor,
        }));
        assert_eq!(
            render(&report).as_deref(),
            Some(
                "Host update: H4 (pausing) — running 0.19.10, on disk 0.19.11, floor 0.19.11, \
                 target 0.19.11 (source floor)"
            )
        );
    }

    #[test]
    fn an_unreadable_on_disk_version_reads_unknown_not_the_running_version() {
        let report = report_with(Some(HostUpdateObservation {
            state: HostState::Unknown,
            running_version: "0.19.10".into(),
            on_disk_version: None,
            floor: None,
            target_version: None,
            target_source: ObservedTargetSource::AutoUpdate,
        }));
        let line = render(&report).expect("line");
        assert!(line.contains("on disk unknown"), "{line}");
        assert!(line.starts_with("Host update: unknown (unknown)"), "{line}");
    }

    #[test]
    fn a_daemon_without_the_field_adds_no_line() {
        assert_eq!(render(&report_with(None)), None);
    }
}
