//! The `ci_telemetry` health section (issue #9014): whether the opted-in
//! GitHub Actions CI poller is actually polling on this host.
//!
//! The incident class it closes: with `autonomous.ciTelemetry.enabled=true`
//! and no `fleet.captain` declared, the fleet-captain gate refuses the poller
//! on every tick. Before this section the refusal showed up only as a
//! per-tick WARN in the daemon log. `ci-telemetry status` read
//! `stale`/`never-polled` with no reason, and SigNoz simply stopped
//! receiving CI data.
//!
//! Split into its own file because `health.rs` sits at its file-size
//! ratchet, like [`super::transcript_ingest_section`].

use super::{HealthInputs, HealthSection, Verdict};

/// Assess the collected [`crate::ci_telemetry::CiTelemetryHealth`] into the `ci_telemetry`
/// section. Pure: the collector (`cli/health.rs`) did the I/O via
/// [`crate::ci_telemetry::collect_health`].
///
/// Renders **no section** when not collected or when the poller is not
/// enabled on this host (the FLAGS-OFF default), so an ordinary host's
/// report is unchanged. When enabled, the section is non-green for a
/// no-captain refusal (the poller runs nowhere), a failing poller, and a
/// stale one. A not-this-host refusal is the routine case on every
/// non-captain host, so it stays green and names the captain.
#[must_use]
pub fn assess_ci_telemetry(inputs: &HealthInputs) -> Option<HealthSection> {
    const KEY: &str = "ci_telemetry";
    let health = inputs.ci_telemetry.as_ref()?;
    if !health.enabled {
        return None;
    }
    let detail = serde_json::to_value(health).unwrap_or_default();
    let (verdict, summary) = match (health.state.as_str(), &health.captain_refusal) {
        ("refused", Some(refusal)) if refusal.no_captain_declared => (
            Verdict::Degraded,
            format!(
                "CI poller enabled but REFUSED since {}: no fleet.captain declared, so it runs \
                 on no host and no CI data reaches the backend — declare `fleet.captain` in \
                 .loom/config.json (#9014)",
                refusal.since.to_rfc3339()
            ),
        ),
        ("refused", Some(refusal)) => (
            Verdict::Green,
            format!("CI poller not armed here (routine): {}", refusal.reason),
        ),
        ("failing", _) => (
            Verdict::Degraded,
            "CI poller FAILING — see `loom-daemon ci-telemetry status`".to_string(),
        ),
        ("stale", _) => (
            Verdict::Degraded,
            "CI poller STALE — no successful poll in over 3 intervals; see \
             `loom-daemon ci-telemetry status`"
                .to_string(),
        ),
        (state, _) => (Verdict::Green, format!("CI poller {state}")),
    };
    Some(HealthSection {
        key: KEY,
        verdict,
        summary,
        detail,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ci_telemetry::state::CaptainRefusal;
    use crate::ci_telemetry::CiTelemetryHealth;

    fn inputs(state: &str, refusal: Option<(bool, &str)>, enabled: bool) -> HealthInputs {
        HealthInputs {
            ci_telemetry: Some(CiTelemetryHealth {
                enabled,
                state: state.to_string(),
                captain_refusal: refusal.map(|(no_captain_declared, reason)| CaptainRefusal {
                    reason: reason.to_string(),
                    no_captain_declared,
                    since: chrono::Utc::now(),
                    last_at: chrono::Utc::now(),
                }),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_or_uncollected_renders_no_section() {
        assert!(assess_ci_telemetry(&HealthInputs::default()).is_none());
        assert!(assess_ci_telemetry(&inputs("never-polled", None, false)).is_none());
    }

    #[test]
    fn no_captain_refusal_is_degraded_and_names_the_fix() {
        let section =
            assess_ci_telemetry(&inputs("refused", Some((true, "no fleet.captain")), true))
                .unwrap();
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("fleet.captain"), "{}", section.summary);
        assert_eq!(section.detail["captain_refusal"]["no_captain_declared"], true);
        let report = crate::health::assess(&inputs("refused", Some((true, "x")), true));
        assert!(report.sections.iter().any(|s| s.key == "ci_telemetry"));
    }

    #[test]
    fn not_this_host_refusal_is_green_and_names_the_reason() {
        let section =
            assess_ci_telemetry(&inputs("refused", Some((false, "fleet captain is host-b")), true))
                .unwrap();
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("host-b"), "{}", section.summary);
    }

    #[test]
    fn failing_and_stale_are_degraded_ok_is_green() {
        for state in ["failing", "stale"] {
            let section = assess_ci_telemetry(&inputs(state, None, true)).unwrap();
            assert_eq!(section.verdict, Verdict::Degraded, "{state}");
        }
        assert_eq!(
            assess_ci_telemetry(&inputs("ok", None, true))
                .unwrap()
                .verdict,
            Verdict::Green
        );
    }
}
