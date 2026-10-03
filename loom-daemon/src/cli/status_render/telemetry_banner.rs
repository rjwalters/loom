//! The loud telemetry banner on `loom-daemon status` (issue #9950).
//!
//! A sibling module rather than more lines in `status_render.rs` (an
//! over-threshold ledger entry — `.loom/docs/file-size-policy.md`), matching
//! how [`super::observability_line`] already lives here.
//!
//! Why this exists beside the one-line `Observability: …` render: the line is
//! the *data* — one row in a table an operator or agent may be skimming for
//! something else entirely. The 2026-10-01 store-tunnel outage (2AMLogic/2am
//! #1878) ran ~15h with every `status` invocation rendering exactly one such
//! line — accurate, and ignorable. The common-tool rule that came out of it:
//! **a status invocation while telemetry is demonstrably broken must
//! complain** — a banner that says what is broken, what it means (records
//! are accumulating locally, not vanishing silently), and what to fix — on
//! every invocation, for as long as it lasts. `loom-daemon status` is the
//! surface operators and agents actually run; this is where the complaint
//! lives.
//!
//! The host-id mismatch has its own WARNING block further up in
//! `print_status_human` (#4830) and is deliberately NOT duplicated here.
//! Healthy / disabled / starting / unrecognized render nothing: relay
//! pressure applies to actual findings, never to a deliberate off (the
//! anomaly-only rule, #4830).

use chrono::{DateTime, Utc};
use loom_daemon::types::{ObservabilityExportState as State, ObservabilityExportStatus};

/// Render the banner, or `None` when there is nothing to complain about.
///
/// `status = None` (a pre-#5083 daemon) renders nothing here: the
/// `Observability: unknown` line already says the daemon could not answer,
/// and inventing a complaint from absence would be a new false alarm.
pub fn render(status: Option<&ObservabilityExportStatus>, now: DateTime<Utc>) -> Option<String> {
    let s = status?;
    // Re-derived rather than trusting the wire state, so a payload that sat
    // in a pipe for a while still reads correctly across the grace boundary
    // — `classify` is the single shared rule (`types.rs`).
    match s.classify(now) {
        State::Failing => Some(block(
            &format!(
                "TELEMETRY IS NOT REACHING {} — {} consecutive failed flush(es) as host_id={}, \
                 last successful export {}{}",
                s.endpoint.as_deref().unwrap_or("(no endpoint)"),
                s.consecutive_failures,
                s.host_id.as_deref().unwrap_or("unknown-host"),
                s.last_success_age_secs(now).map_or_else(
                    || "never".to_string(),
                    |age| format!("{} ago", loom_daemon::health::format_window(age)),
                ),
                s.last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" (last error: {d})")),
            ),
            s,
        )),
        State::NeverExported => Some(block(
            &format!(
                "TELEMETRY HAS NEVER REACHED {} — running {} as host_id={} with no batch ever \
                 acked{}",
                s.endpoint.as_deref().unwrap_or("(no endpoint)"),
                s.uptime_secs(now)
                    .map_or_else(|| "?".to_string(), |u| loom_daemon::health::format_window(u)),
                s.host_id.as_deref().unwrap_or("unknown-host"),
                s.last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" (last error: {d})")),
            ),
            s,
        )),
        State::Misconfigured => Some(block(
            &format!(
                "TELEMETRY IS MISCONFIGURED — enabled but not exporting → {}{}",
                s.endpoint.as_deref().unwrap_or("(no endpoint)"),
                s.last_failure_detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" ({d})")),
            ),
            s,
        )),
        _ => None,
    }
}

/// The banner body: headline, meaning, fix. The same three parts the alert
/// issues and log escalations carry, because an operator should meet the
/// same complaint whichever surface they happen to be looking at (#9950).
fn block(headline: &str, s: &ObservabilityExportStatus) -> String {
    let local_caveat = if s.endpoint_is_loopback() {
        "\n    the daemon can only see its FIRST hop; a local collector that acks does not prove \
         delivery past it"
    } else {
        ""
    };
    format!(
        "{}\n⚠️  {}\n    every record the daemon produces is accumulating locally until this is \
         fixed\n    → fix: check this host's OTel egress (edge collector / tunnel / ingest key) \
         and the endpoint itself\n    → `loom-daemon health` has the verdict; `loom-daemon \
         status --json` is machine-readable{}",
        "=".repeat(72),
        headline,
        local_caveat,
    )
}

#[cfg(test)]
mod tests {
    use super::render;
    use chrono::{DateTime, Utc};
    use loom_daemon::types::ObservabilityExportStatus;

    fn now() -> DateTime<Utc> {
        "2026-10-02T06:00:00Z".parse().unwrap()
    }

    fn status(mutate: impl FnOnce(&mut ObservabilityExportStatus)) -> ObservabilityExportStatus {
        let mut s = ObservabilityExportStatus {
            state: loom_daemon::types::ObservabilityExportState::Starting,
            host_id: Some("joseph-air".to_string()),
            ingest_host_id: None,
            endpoint: Some("http://127.0.0.1:14318".to_string()),
            exporter: Some("otlp".to_string()),
            started_at: Some(now() - chrono::Duration::hours(4)),
            last_success_at: None,
            last_failure_at: None,
            last_failure_detail: None,
            records_exported: 0,
            consecutive_failures: 0,
            flush_interval_secs: Some(30),
            ..Default::default()
        };
        mutate(&mut s);
        s.refresh_endpoint_scope();
        s
    }

    #[test]
    fn a_failing_exporter_gets_the_banner_with_endpoint_streak_and_fix() {
        let s = status(|e| {
            e.state = loom_daemon::types::ObservabilityExportState::Failing;
            e.consecutive_failures = 12;
            e.last_failure_detail = Some("connection refused".to_string());
        });
        let banner = render(Some(&s), now()).expect("failing must render the banner");
        for want in [
            "TELEMETRY IS NOT REACHING http://127.0.0.1:14318",
            "12 consecutive failed flush(es)",
            "host_id=joseph-air",
            "connection refused",
            "accumulating locally",
            "edge collector / tunnel / ingest key",
            "loom-daemon health",
        ] {
            assert!(banner.contains(want), "banner missing {want:?}:\n{banner}");
        }
    }

    #[test]
    fn never_exported_gets_the_banner() {
        let s = status(|e| {
            e.state = loom_daemon::types::ObservabilityExportState::NeverExported;
        });
        let banner = render(Some(&s), now()).expect("never-exported must render");
        assert!(banner.contains("TELEMETRY HAS NEVER REACHED"), "{banner}");
        assert!(banner.contains("4h"), "uptime must be human: {banner}");
    }

    #[test]
    fn misconfigured_gets_the_banner_with_the_offending_detail() {
        let s = ObservabilityExportStatus::misconfigured(
            Some("http://127.0.0.1:14318".to_string()),
            "could not read ingest key file /Users/x/.loom/observability/ingest.key: stream did \
             not contain valid UTF-8"
                .to_string(),
        );
        let banner = render(Some(&s), now()).expect("misconfigured must render");
        assert!(banner.contains("MISCONFIGURED"), "{banner}");
        assert!(banner.contains("ingest.key"), "{banner}");
    }

    #[test]
    fn healthy_disabled_starting_render_nothing() {
        let mut healthy = status(|e| {
            e.state = loom_daemon::types::ObservabilityExportState::Healthy;
            e.last_success_at = Some(now() - chrono::Duration::seconds(12));
        });
        healthy.refresh_endpoint_scope();
        assert!(render(Some(&healthy), now()).is_none(), "healthy is quiet");

        let disabled = ObservabilityExportStatus::disabled();
        assert!(render(Some(&disabled), now()).is_none(), "disabled is quiet");

        // A daemon rolled 20 seconds ago is still Starting (within the
        // never-exported grace window) — quiet, like observability_line.
        let starting = status(|e| {
            e.started_at = Some(now() - chrono::Duration::seconds(20));
        });
        assert!(render(Some(&starting), now()).is_none(), "starting is quiet");
    }

    #[test]
    fn a_pre_surface_daemon_renders_no_banner() {
        assert!(render(None, now()).is_none());
    }

    #[test]
    fn a_loopback_endpoint_gets_the_first_hop_caveat() {
        let s = status(|e| {
            e.state = loom_daemon::types::ObservabilityExportState::Failing;
            e.consecutive_failures = 3;
        });
        // The endpoint above is loopback, so the caveat must be present…
        assert!(render(Some(&s), now()).unwrap().contains("FIRST hop"),);
        // …and a remote endpoint must not carry it.
        let mut remote = status(|e| {
            e.state = loom_daemon::types::ObservabilityExportState::Failing;
            e.endpoint = Some("https://telemetry.example.com/ingest".to_string());
        });
        remote.refresh_endpoint_scope();
        let banner = render(Some(&remote), now()).unwrap();
        assert!(!banner.contains("FIRST hop"), "remote endpoint: {banner}");
    }
}
