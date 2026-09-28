//! The `Observability: …` telemetry-export line on `loom-daemon status`
//! (Issue #5083; first-hop scope annotation, Issue #9015).
//!
//! A sibling module rather than more lines in `status_render.rs` (an
//! over-threshold ledger entry — `.loom/docs/file-size-policy.md`), matching
//! how [`super::forge_events_line`], [`super::holds`] and
//! [`super::model_class`] already live here. Moved verbatim apart from the
//! #9015 annotation described below.
//!
//! The line always renders, because the whole point of #5083 is that "healthy"
//! must be *stated*, not inferred from the absence of a warning. #9015 adds the
//! other half of that sentence: **what** is healthy. The daemon observes
//! exactly one hop — did the configured endpoint ack the batch — and on the
//! deployment this issue came from that endpoint is a *local* otel-edge
//! collector whose own egress was broken for 30h+. Every POST was accepted,
//! `rejected: 0`, `dropped: 0`, `state: healthy`, and not one record reached
//! SigNoz. So the `OK` line names its scope, and when the endpoint is on this
//! machine it says outright that delivery past that hop is unverified here.

use chrono::{DateTime, Utc};
use loom_daemon::health::format_window;
use loom_daemon::types::{ObservabilityExportState as State, ObservabilityExportStatus};

/// Render the one-line export summary as of `now` (passed in, so the tests are
/// deterministic).
///
/// `status = None` means a pre-#5083 daemon binary that never computed one —
/// reported as `unknown`, never silently as `disabled`, which would be an
/// invented fact about a daemon that said nothing.
pub fn render(status: Option<&ObservabilityExportStatus>, now: DateTime<Utc>) -> String {
    let Some(s) = status else {
        return "Observability: unknown (older daemon binary — restart to pick up #5083)"
            .to_string();
    };
    let host = s.host_id.as_deref().unwrap_or("unknown-host");
    let endpoint = s.endpoint.as_deref().unwrap_or("(no endpoint)");
    let uptime = s
        .uptime_secs(now)
        .map_or_else(|| "?".to_string(), format_window);
    let last_success = s
        .last_success_age_secs(now)
        .map_or_else(|| "never".to_string(), |age| format!("{} ago", format_window(age)));
    // Re-derived rather than trusting the daemon-stamped `state`, so a status
    // payload that sat in a pipe for a while still reads correctly across the
    // grace boundary — `classify` is the single shared rule (`types.rs`).
    match s.classify(now) {
        State::Disabled => {
            "Observability: disabled (no telemetry export — set observability.enabled=true to opt in)"
                .to_string()
        }
        // Distinct from `Disabled` (Issue #5337): `enabled: true` but a
        // required piece of config could not be resolved. `endpoint` reflects
        // whatever DID resolve rather than a blanket "(no endpoint)", and the
        // detail names the offending path plus the underlying error.
        State::Misconfigured => format!(
            "Observability: MISCONFIGURED — enabled but not exporting → {endpoint}{}",
            s.last_failure_detail
                .as_deref()
                .map_or_else(String::new, |d| format!(" ({d})")),
        ),
        State::Starting => format!(
            "Observability: starting — exporter up {uptime} as host_id={host}, no batch acked yet \
             (first flush due within {}s) → {endpoint}",
            s.flush_interval_secs.unwrap_or(0)
        ),
        State::NeverExported => format!(
            "Observability: NEVER EXPORTED — running {uptime} as host_id={host} and no batch has \
             EVER been acked; telemetry is not reaching {endpoint}{}",
            s.last_failure_detail
                .as_deref()
                .map_or_else(String::new, |d| format!(" (last error: {d})")),
        ),
        // #9015: the ONE state that reads as "all good", so the one that has to
        // carry its own scope. `(first hop only)` is unconditional — a remote
        // endpoint can be a collector too, and this daemon cannot tell.
        State::Healthy => format!(
            "Observability: OK (first hop only) — last export {last_success}, {} record(s) as \
             host_id={host} → {endpoint}{}",
            s.records_exported,
            downstream_caveat(s),
        ),
        State::HostIdMismatch => format!(
            "Observability: HOST-ID MISMATCH — telemetry is landing under host_id={}, not {host} \
             (last export {last_success}, {} record(s)) → {endpoint}",
            s.ingest_host_id.as_deref().unwrap_or("unknown"),
            s.records_exported
        ),
        State::Failing => format!(
            "Observability: FAILING — {} consecutive failed flush(es) as host_id={host}, last \
             success {last_success} → {endpoint}{}",
            s.consecutive_failures,
            s.last_failure_detail
                .as_deref()
                .map_or_else(String::new, |d| format!(" (last error: {d})")),
        ),
        // Only reachable from a NEWER daemon reporting a state this build does
        // not know. Say so plainly rather than collapsing it into one of the
        // known states — the same "degrade legibly, never mislabel" posture the
        // Safehouse block takes for an unknown state string (#4464).
        State::Unrecognized => format!(
            "Observability: unrecognized state from a newer daemon binary (host_id={host}) — \
             upgrade this client to read it"
        ),
    }
}

/// The trailing clause for an endpoint that is on **this machine** (#9015):
/// almost always an edge collector that forwards onward, so the acked first hop
/// says nothing whatsoever about the backend, and the operator needs a second,
/// external check. Empty for any other endpoint.
///
/// Derived live from the endpoint via
/// [`ObservabilityExportStatus::endpoint_is_loopback`] rather than read from the
/// wire's `endpoint_loopback` flag, so a payload from a pre-#9015 daemon — the
/// binaries actually running when this was filed — still renders the caveat.
fn downstream_caveat(s: &ObservabilityExportStatus) -> String {
    if !s.endpoint_is_loopback() {
        return String::new();
    }
    " (LOCAL collector: delivery to the backend PAST this hop is not verified here — check the \
     backend read-back, or the collector's own otelcol_exporter_send_failed_* counters)"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::render;
    use chrono::{DateTime, Utc};
    use loom_daemon::types::{ObservabilityExportState, ObservabilityExportStatus};

    fn render_now() -> DateTime<Utc> {
        "2026-08-03T12:00:00Z".parse().unwrap()
    }

    /// A running HTTPS exporter, up four hours, that has never been touched by
    /// a flush attempt — the base the individual states mutate from.
    fn export_status(
        mutate: impl FnOnce(&mut ObservabilityExportStatus),
    ) -> ObservabilityExportStatus {
        let mut status = ObservabilityExportStatus {
            state: ObservabilityExportState::Starting,
            host_id: Some("robb-studio".to_string()),
            ingest_host_id: None,
            endpoint: Some("https://dashboard.example/ingest".to_string()),
            exporter: Some("https".to_string()),
            started_at: Some(render_now() - chrono::Duration::hours(4)),
            last_success_at: None,
            records_exported: 0,
            consecutive_failures: 0,
            flush_interval_secs: Some(30),
            ..Default::default()
        };
        mutate(&mut status);
        status.refresh_endpoint_scope();
        status
    }

    /// A healthy exporter — the state every #9015 assertion is about.
    fn healthy(endpoint: &str) -> ObservabilityExportStatus {
        let endpoint = endpoint.to_string();
        export_status(|e| {
            e.endpoint = Some(endpoint);
            e.last_success_at = Some(render_now() - chrono::Duration::seconds(12));
            e.records_exported = 3481;
        })
    }

    #[test]
    fn observability_line_states_health_positively_with_the_host_id() {
        // AC1 of #5083: an operator can confirm telemetry is flowing, and under
        // which host_id, without reading logs.
        let line = render(Some(&healthy("https://dashboard.example/ingest")), render_now());
        assert!(line.starts_with("Observability: OK"), "{line}");
        assert!(line.contains("12s ago"), "{line}");
        assert!(line.contains("host_id=robb-studio"), "{line}");
        assert!(line.contains("3481 record(s)"), "{line}");
    }

    #[test]
    fn a_healthy_line_names_the_first_hop_scope_even_for_a_remote_endpoint() {
        // #9015 AC1: the line must not be readable as "the data is in the
        // backend". A remote endpoint can be a collector too — this daemon
        // cannot tell — so the scope is stated unconditionally.
        let line = render(Some(&healthy("https://dashboard.example/ingest")), render_now());
        assert!(line.contains("OK (first hop only)"), "{line}");
    }

    #[test]
    fn a_healthy_line_into_a_local_collector_says_downstream_is_unverified() {
        // The incident shape: the endpoint is the local otel-edge, whose egress
        // was dead for 30h+ while this line said `OK`.
        let line = render(Some(&healthy("http://127.0.0.1:14318/v1/logs")), render_now());
        assert!(line.contains("first hop only"), "{line}");
        assert!(line.contains("LOCAL collector"), "{line}");
        assert!(line.contains("not verified"), "{line}");
        assert!(
            line.contains("otelcol_exporter_send_failed_*"),
            "must name the end-to-end check to run: {line}"
        );
    }

    #[test]
    fn the_local_collector_caveat_is_derived_from_the_endpoint_not_the_wire_flag() {
        // A pre-#9015 daemon sends `endpoint_loopback: false` (absent ⇒
        // default) alongside a loopback endpoint. The renderer must still warn,
        // because those are exactly the binaries running today.
        let mut stale_payload = healthy("http://localhost:14318/v1/logs");
        stale_payload.endpoint_loopback = false;
        let line = render(Some(&stale_payload), render_now());
        assert!(line.contains("LOCAL collector"), "{line}");

        // …and it is not bolted onto a remote endpoint by a bogus wire flag.
        let mut mislabelled = healthy("https://dashboard.example/ingest");
        mislabelled.endpoint_loopback = true;
        assert!(!render(Some(&mislabelled), render_now()).contains("LOCAL collector"));
    }

    #[test]
    fn observability_line_distinguishes_disabled_from_healthy() {
        // AC2 of #5083: a host with observability disabled must not read like a
        // healthy one (before #5083 both rendered as nothing at all).
        let disabled = render(Some(&ObservabilityExportStatus::disabled()), render_now());
        assert!(disabled.contains("disabled"), "{disabled}");
        assert!(!disabled.contains("OK"), "{disabled}");
    }

    #[test]
    fn observability_line_distinguishes_misconfigured_from_disabled() {
        // Issue #5337: `enabled: true` with an unreadable ingestKeyFile must
        // NOT read like the deliberate-off `disabled` state, and must name
        // the offending path + errno rather than reporting `endpoint: null`.
        let misconfigured = ObservabilityExportStatus::misconfigured(
            Some("https://ingest.example.com/v1/telemetry".to_string()),
            "could not read ingest key file /etc/loom/ingest.key: No such file or directory (os error 2)"
                .to_string(),
        );
        let line = render(Some(&misconfigured), render_now());
        assert!(line.contains("MISCONFIGURED"), "{line}");
        assert!(!line.contains("Observability: disabled"), "{line}");
        assert!(line.contains("https://ingest.example.com/v1/telemetry"), "{line}");
        assert!(
            line.contains("/etc/loom/ingest.key") && line.contains("os error 2"),
            "must name the offending path and errno: {line}"
        );

        let disabled = render(Some(&ObservabilityExportStatus::disabled()), render_now());
        assert!(!disabled.contains("MISCONFIGURED"), "{disabled}");
    }

    #[test]
    fn observability_line_surfaces_never_exported_as_a_problem() {
        // AC3 of #5083: the silent failure mode — configured, running for
        // hours, and nothing has ever landed.
        let line = render(Some(&export_status(|_| {})), render_now());
        assert!(line.contains("NEVER EXPORTED"), "{line}");
        assert!(line.contains("host_id=robb-studio"), "{line}");
        assert!(line.contains("dashboard.example"), "{line}");
    }

    #[test]
    fn observability_line_does_not_alarm_during_the_startup_grace_window() {
        // A daemon rolled 20 seconds ago must not read as broken.
        let status = export_status(|e| {
            e.started_at = Some(render_now() - chrono::Duration::seconds(20));
        });
        let line = render(Some(&status), render_now());
        assert!(line.contains("starting"), "{line}");
        assert!(!line.contains("NEVER EXPORTED"), "{line}");
    }

    #[test]
    fn observability_line_reports_a_failing_exporter_with_its_error() {
        let status = export_status(|e| {
            e.last_success_at = Some(render_now() - chrono::Duration::hours(2));
            e.consecutive_failures = 4;
            e.last_failure_detail = Some("sink rejected batch: HTTP 401 — denied".to_string());
        });
        let line = render(Some(&status), render_now());
        assert!(line.contains("FAILING"), "{line}");
        assert!(line.contains("HTTP 401"), "{line}");
        assert!(line.contains("2h ago"), "{line}");
    }

    #[test]
    fn observability_line_names_both_identities_on_a_mismatch() {
        let status = export_status(|e| {
            e.last_success_at = Some(render_now() - chrono::Duration::seconds(12));
            e.ingest_host_id = Some("robb-pro".to_string());
            e.records_exported = 77;
        });
        let line = render(Some(&status), render_now());
        assert!(line.contains("HOST-ID MISMATCH"), "{line}");
        assert!(line.contains("robb-pro") && line.contains("robb-studio"), "{line}");
    }

    #[test]
    fn observability_line_from_an_older_daemon_is_unknown_not_disabled() {
        // A `None` field means the daemon could not answer — reporting it as
        // "disabled" would invent a fact about a daemon that said nothing.
        let line = render(None, render_now());
        assert!(line.contains("unknown"), "{line}");
        assert!(line.contains("older daemon binary"), "{line}");
    }
}
