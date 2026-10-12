//! The `OTLP export:` line on `loom-daemon status` (Issue #11353).
//!
//! A sibling module (`status_render.rs` is on the file-size ratchet). Always
//! renders: a daemon that does not export to SigNoz must say so, not look
//! healthy by omission.

use loom_daemon::observability::otlp_health::{OtlpExportHealth, OtlpExportState};

/// Render the one-line OTLP-export summary. `None` is a pre-#11353 daemon,
/// reported as `unknown`, never as `ok`.
pub fn render(health: Option<&OtlpExportHealth>) -> String {
    let Some(h) = health else {
        return "OTLP export: unknown (older daemon binary — restart to pick up #11353)"
            .to_string();
    };
    let detail = h.detail.as_deref();
    match h.state {
        OtlpExportState::Ok => "OTLP export: ok (otlp_export: ok)".to_string(),
        OtlpExportState::Exempt => format!(
            "OTLP export: exempt (otlp_export: exempt) — {}",
            detail.unwrap_or("no reason given")
        ),
        OtlpExportState::NoExporter | OtlpExportState::Failing => format!(
            "OTLP export: {} (otlp_export: {}) — {}",
            h.state.as_str().to_uppercase(),
            h.state.as_str(),
            detail.unwrap_or("no detail")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(state: OtlpExportState, detail: Option<&str>) -> OtlpExportHealth {
        OtlpExportHealth {
            state,
            detail: detail.map(str::to_string),
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
        assert!(render(None).contains("unknown"));
    }
}
