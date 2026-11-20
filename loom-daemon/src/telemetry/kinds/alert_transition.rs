//! `alert.transition` (#12806): one alert opening, updating or resolving on
//! this host, as an OTLP log record.
//!
//! The wire schema is loom-ui's, not ours: 2AMLogic/loom-ui
//! `docs/alerts/external.md` § "As telemetry (shadow)" (loom-ui#5379). The
//! record's `body` and its `event.name` attribute are both
//! [`ALERT_TRANSITION_EVENT_NAME`]; every field below is one `alert.*` string
//! attribute. The values are fitted to that schema's charsets and lengths by
//! [`crate::alerts::sanitize`] before a record is built, so a record never
//! carries a value the reader would drop.
//!
//! - `state` is `open` (opens, updates or reopens: an idempotent upsert) or
//!   `resolved`. There is no `update` state.
//! - `severity` is `warning` or `critical`, on `resolved` too.
//! - `observed_at` is when the daemon observed the transition, RFC 3339 UTC
//!   with milliseconds: the reader's ordering and idempotency clock, with
//!   (`source`, `rule`, `key`, `state`, `observed_at`) the idempotency key.
//! - An `open` is re-sent at least every 12 h while it holds
//!   ([`crate::alerts::HEARTBEAT`]), because the reader closes an incident
//!   with no observation for 24 h.
//!
//! `host.id` and `service.name` (`loom-daemon`, never `loom-ui`) ride on the
//! OTLP Resource like every other kind.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// The record's `body` and its `event.name` attribute.
pub const ALERT_TRANSITION_EVENT_NAME: &str = "alert.transition";

/// Every log attribute key `alert.transition` exports beyond the shared
/// `loom.kind` / `loom.record_id`: the schema's required keys, then its
/// optional ones (`alert.detail`, `alert.link`; present only when set). The
/// schema's optional `alert.event_id` is not set by the daemon.
pub const ALERT_TRANSITION_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "event.name",
    "alert.source",
    "alert.rule",
    "alert.key",
    "alert.state",
    "alert.severity",
    "alert.title",
    "alert.observed_at",
    "alert.detail",
    "alert.link",
];

/// The `alert.transition` record. Every string is already fitted to the
/// schema (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertTransitionRecord {
    /// This host's id (`[A-Za-z0-9][A-Za-z0-9._-]{0,63}`).
    pub source: String,
    /// The check's name, same pattern.
    pub rule: String,
    /// The entity (`[A-Za-z0-9][A-Za-z0-9._/@+-]{0,127}`).
    pub key: String,
    /// `open` or `resolved`.
    pub state: String,
    /// `warning` or `critical`.
    pub severity: String,
    /// 1–200 characters, one line.
    pub title: String,
    /// When the transition was observed.
    pub observed_at: DateTime<Utc>,
    /// At most 2000 characters of plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// An `https://` URL of at most 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl AlertTransitionRecord {
    /// `observed_at` as the schema wants it: `2026-10-19T12:00:00.000Z`.
    #[must_use]
    pub fn observed_at_wire(&self) -> String {
        self.observed_at
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn observed_at_is_utc_with_milliseconds() {
        let r = AlertTransitionRecord {
            source: "h".into(),
            rule: "r".into(),
            key: "k".into(),
            state: "open".into(),
            severity: "warning".into(),
            title: "t".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap(),
            detail: None,
            link: None,
        };
        assert_eq!(r.observed_at_wire(), "2026-10-19T12:00:00.000Z");
    }
}
