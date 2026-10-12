//! The `OTLP export:` line on `loom-daemon status` (Issue #11353).
//!
//! A sibling module (`status_render.rs` is on the file-size ratchet). Always
//! renders: a daemon that does not export to SigNoz must say so, not look
//! healthy by omission. An attention state carries
//! `otlp: failing since <T> (<reason>)` (Issue #11557).

use loom_daemon::observability::otlp_health::{OtlpExportHealth, OtlpExportState};

/// Render the one-line OTLP-export summary. `None` is a pre-#11353 daemon,
/// reported as `unknown`, never as `ok`.
pub fn render(health: Option<&OtlpExportHealth>) -> String {
    let Some(h) = health else {
        return "OTLP export: unknown (older daemon binary — restart to pick up #11353)"
            .to_string();
    };
    match h.state {
        OtlpExportState::Ok => "OTLP export: ok (otlp_export: ok)".to_string(),
        OtlpExportState::Exempt => format!(
            "OTLP export: exempt (otlp_export: exempt) — {}",
            h.detail.as_deref().unwrap_or("no reason given")
        ),
        OtlpExportState::NoExporter | OtlpExportState::Failing => format!(
            "OTLP export: {} (otlp_export: {}) — {}",
            h.state.as_str().to_uppercase(),
            h.state.as_str(),
            h.summary().unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use loom_daemon::observability::otlp_health::{OtlpHealthInputs, OtlpHealthMonitor};

    fn h(state: OtlpExportState, detail: Option<&str>) -> OtlpExportHealth {
        OtlpExportHealth {
            state,
            detail: detail.map(str::to_string),
            since: None,
        }
    }

    #[test]
    fn renders_each_state() {
        assert!(render(Some(&h(OtlpExportState::Ok, None))).contains("otlp_export: ok"));
        let exempt = render(Some(&h(OtlpExportState::Exempt, Some("2am#3649"))));
        assert!(exempt.contains("otlp_export: exempt") && exempt.contains("2am#3649"));
        let none = render(Some(&h(OtlpExportState::NoExporter, Some("no usable otlp"))));
        assert!(none.contains("otlp_export: no_exporter") && none.contains("NO_EXPORTER"));
        let failing = render(Some(&h(OtlpExportState::Failing, Some("no success in 15m"))));
        assert!(failing.contains("otlp_export: failing"));
        assert!(failing.contains("otlp: failing (no success in 15m)"));
        assert!(render(None).contains("unknown"));
    }

    /// Monitor inputs mirroring an always-403 sink: started at t0, never
    /// acked, the exporter's last failure a credential refusal. `now` is
    /// injected, so nothing waits.
    #[test]
    fn always_403_renders_failing_since_first_failing_sample() {
        let t = |min: i64| {
            Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap() + Duration::minutes(min)
        };
        let inputs = OtlpHealthInputs {
            otlp_planned: true,
            exempt_reason: None,
            started_at: Some(t(0)),
            last_success_at: None,
            last_drop_growth_at: None,
            window: Duration::minutes(15),
            last_failure_detail: Some(
                "OTLP HTTP 403 (credential refused; batch held until the credential is fixed)"
                    .to_string(),
            ),
        };
        let mut monitor = OtlpHealthMonitor::default();
        for min in [1, 16, 30, 45] {
            monitor.observe(inputs.clone(), Some(0), t(min));
        }
        let health = monitor.observe(inputs, Some(0), t(60)).health;
        let line = render(Some(&health));
        assert!(line.contains("otlp: failing since 2026-10-12T00:16:00Z ("), "{line}");
        assert!(line.contains("OTLP HTTP 403 (credential refused"), "{line}");
    }
}
