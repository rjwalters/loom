//! The `Forge events:` line on `loom-daemon status` (ADR-0021, #8765).
//!
//! A sibling module rather than more lines in `status_render.rs` (an
//! over-threshold ledger entry), matching how `holds` and `model_class`
//! already live here.
//!
//! The line always renders. Its whole reason for existing is that the
//! question an operator asks about a feed — "why is my cursor not
//! advancing?" — has several materially different answers that all look
//! identical if the surface stays silent: off by choice, never provisioned,
//! wrong key, wrong host, or simply a quiet feed.

use chrono::{DateTime, Utc};
use loom_daemon::health::format_window;
use loom_daemon::types::{ForgeEventsState as State, ForgeEventsStatus};

/// Render the one-line feed summary. `None` means the answering daemon
/// predates ADR-0021 — reported as such, never as `disabled`.
pub fn render(status: Option<&ForgeEventsStatus>, now: DateTime<Utc>) -> String {
    let Some(s) = status else {
        return "Forge events: unknown (older daemon binary — restart to pick up ADR-0021)"
            .to_string();
    };
    let host = s.host_id.as_deref().unwrap_or("unknown-host");
    let endpoint = s.endpoint.as_deref().unwrap_or("(no endpoint)");
    let detail = s
        .last_error_detail
        .as_deref()
        .map_or_else(String::new, |d| format!(" ({d})"));
    let last_poll = s
        .last_poll_age_secs(now)
        .map_or_else(|| "never".to_string(), |age| format!("{} ago", format_window(age)));
    match s.state {
        State::Disabled => "Forge events: disabled (no event feed — set forgeEvents.enabled=true \
             to opt in; polling is unaffected either way)"
            .to_string(),
        State::Misconfigured => {
            format!("Forge events: MISCONFIGURED — enabled but not polling → {endpoint}{detail}")
        }
        State::Connecting => format!(
            "Forge events: connecting — polling {endpoint} as host_id={host} from cursor {}, \
             no poll completed yet",
            s.cursor
        ),
        State::Healthy => format!(
            "Forge events: OK — last poll {last_poll}, cursor {} ({} event(s) in {} page(s)) \
             as host_id={host} → {endpoint}",
            s.cursor, s.events_observed, s.pages_observed
        ),
        State::AuthFailed => format!(
            "Forge events: AUTH FAILED — this host's event key is not accepted as \
             host_id={host}; cursor stuck at {}{detail}",
            s.cursor
        ),
        State::HostMismatch => format!(
            "Forge events: HOST MISMATCH — the feed at {endpoint} is not this host's; cursor \
             stuck at {}{detail}",
            s.cursor
        ),
        State::Failing => format!(
            "Forge events: FAILING — {} consecutive failed poll(s) as host_id={host}, cursor \
             stuck at {}{detail}",
            s.consecutive_failures, s.cursor
        ),
        // Cadence, not cause: `last_error` still names the class, so the line
        // reports both rather than letting the backoff promotion erase why.
        State::Backoff => format!(
            "Forge events: BACKOFF — {} consecutive failed poll(s) ({}), retrying every {}s as \
             host_id={host}, cursor stuck at {}{detail}",
            s.consecutive_failures,
            s.last_error.as_deref().unwrap_or("unknown"),
            s.poll_interval_secs,
            s.cursor
        ),
        State::Unrecognized => {
            format!("Forge events: unrecognized state from a newer daemon binary → {endpoint}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn status(state: State) -> ForgeEventsStatus {
        ForgeEventsStatus {
            state,
            endpoint: Some("https://events.internal/".to_string()),
            host_id: Some("mac-studio".to_string()),
            cursor: 42,
            pages_observed: 3,
            events_observed: 9,
            consecutive_failures: 4,
            poll_interval_secs: 300,
            last_error: Some("auth_failed".to_string()),
            last_error_detail: Some("feed rejected this host's event key with HTTP 401".into()),
            last_poll_at: Some(now() - chrono::Duration::seconds(30)),
            ..ForgeEventsStatus::default()
        }
    }

    #[test]
    fn a_pre_adr_daemon_is_not_reported_as_disabled() {
        let line = render(None, now());
        assert!(line.contains("older daemon binary"), "{line}");
        assert!(!line.contains("disabled"), "{line}");
    }

    #[test]
    fn disabled_names_the_opt_in_and_reassures_about_polling() {
        let line = render(Some(&ForgeEventsStatus::disabled()), now());
        assert!(line.contains("forgeEvents.enabled=true"), "{line}");
        assert!(line.contains("polling is unaffected"), "{line}");
    }

    #[test]
    fn misconfigured_names_the_missing_piece() {
        let line = render(
            Some(&ForgeEventsStatus::misconfigured(
                None,
                "forgeEvents.hostId not configured".to_string(),
            )),
            now(),
        );
        assert!(line.contains("MISCONFIGURED"), "{line}");
        assert!(line.contains("forgeEvents.hostId not configured"), "{line}");
    }

    #[test]
    fn backoff_keeps_the_underlying_class_and_the_stretched_cadence() {
        let line = render(Some(&status(State::Backoff)), now());
        assert!(line.contains("BACKOFF"), "{line}");
        assert!(line.contains("auth_failed"), "{line}");
        assert!(line.contains("every 300s"), "{line}");
    }

    #[test]
    fn healthy_reports_cursor_and_counts() {
        let line = render(Some(&status(State::Healthy)), now());
        assert!(line.contains("OK"), "{line}");
        assert!(line.contains("cursor 42"), "{line}");
        assert!(line.contains("9 event(s) in 3 page(s)"), "{line}");
        assert!(line.contains("30s ago"), "{line}");
    }

    #[test]
    fn every_error_state_names_the_stuck_cursor() {
        for state in [State::AuthFailed, State::HostMismatch, State::Failing] {
            let line = render(Some(&status(state)), now());
            assert!(line.contains("cursor stuck at 42"), "{state:?}: {line}");
        }
    }

    // The event key must never reach an operator-visible surface, even
    // indirectly: the status type carries only paths and classes, and this
    // asserts the renderer adds nothing.
    #[test]
    fn no_state_renders_anything_but_the_recorded_detail() {
        let mut s = status(State::Failing);
        s.last_error_detail =
            Some("could not read event key file /home/x/.loom/forge-events/key".into());
        let line = render(Some(&s), now());
        assert!(line.contains("/home/x/.loom/forge-events/key"), "{line}");
    }
}
