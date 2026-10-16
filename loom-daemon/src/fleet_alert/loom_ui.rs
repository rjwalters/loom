//! The loom-ui output for fleet alerts (#11295): every `Started` / `Cleared`
//! edge becomes an `open` / `resolved` incident on loom-ui's Alerts page,
//! beside the event-bus (Matrix) output and independent of it.
//!
//! Rule `fleet.<condition>` (`fleet.dispatch-halted`, `fleet.roles-persistent`,
//! `fleet.output-missing` …). A `prefix:rest` condition key splits into rule
//! `fleet.prefix` and incident key `rest`; a plain one uses key `fleet`. The
//! source is this host, so the incident is per host already.
//!
//! **One source per rule.** `tokens-zero-healthy` is not sent: loom-ui derives
//! the same fact itself (`provider-pool-exhausted`, from the daemon's
//! `loom.pool.accounts` gauge), and two incidents for one outage is noise.

use super::state::{Kind, Transition};
use super::task::AlertSink;
use crate::loom_ui_alerts::{self, Alert, Severity, State};

/// Condition keys loom-ui already alerts on from its own evidence.
pub const DERIVED_BY_LOOM_UI: &[&str] = &[super::KEY_TOKENS];

/// The loom-ui alert for `t`, or `None` for a key loom-ui derives itself.
/// A reminder re-posts `open`, which loom-ui treats as idempotent.
#[must_use]
pub fn to_alert(t: &Transition) -> Option<Alert> {
    if DERIVED_BY_LOOM_UI.contains(&t.key.as_str()) {
        return None;
    }
    let (condition, key) = t.key.split_once(':').unwrap_or((t.key.as_str(), "fleet"));
    let condition = condition.trim_end_matches('-');
    let state = if t.kind == Kind::Cleared {
        State::Resolved
    } else {
        State::Open
    };
    let severity = if t.critical {
        Severity::Critical
    } else {
        Severity::Warning
    };
    let title = t.headline.lines().next().unwrap_or(&t.headline);
    let detail = if state == State::Resolved {
        format!("Cleared: {}", t.headline)
    } else {
        format!("{}\nFix: {}", t.headline, t.fix)
    };
    Some(
        Alert::new(
            format!("fleet.{condition}"),
            if key.is_empty() { "fleet" } else { key },
            title,
            severity,
            state,
        )
        .with_detail(detail),
    )
}

/// [`AlertSink`] queuing onto [`loom_ui_alerts::post`]; a no-op when that
/// output is off.
pub struct LoomUiSink;

impl AlertSink for LoomUiSink {
    fn name(&self) -> &'static str {
        "loom-ui"
    }
    fn deliver(&self, t: &Transition, _host: &str) -> Result<(), String> {
        if let Some(alert) = to_alert(t) {
            loom_ui_alerts::post(alert);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(kind: Kind, key: &str, critical: bool) -> Transition {
        Transition {
            kind,
            key: key.to_string(),
            headline: "Dispatch is HALTED.\nsecond line".to_string(),
            fix: "Fix main.".to_string(),
            critical,
        }
    }

    #[test]
    fn started_opens_and_cleared_resolves_the_same_incident() {
        let open = to_alert(&t(Kind::Started, "dispatch-halted", false)).unwrap();
        let resolved = to_alert(&t(Kind::Cleared, "dispatch-halted", false)).unwrap();
        assert_eq!(open.rule, "fleet.dispatch-halted");
        assert_eq!(open.key, "fleet");
        assert_eq!(open.state, State::Open);
        assert_eq!(open.severity, Severity::Warning);
        assert_eq!(open.title, "Dispatch is HALTED.");
        assert!(open.detail.as_deref().unwrap().contains("Fix: Fix main."));
        assert_eq!((resolved.rule, resolved.key), (open.rule, open.key));
        assert_eq!(resolved.state, State::Resolved);
    }

    #[test]
    fn a_reminder_re_posts_open() {
        let a = to_alert(&t(Kind::Reminder, "roles-persistent", false)).unwrap();
        assert_eq!(a.state, State::Open);
        assert_eq!(a.rule, "fleet.roles-persistent");
    }

    #[test]
    fn a_prefixed_key_splits_into_rule_and_entity() {
        let a = to_alert(&t(Kind::Started, "output-missing:release-notes/repo", true)).unwrap();
        assert_eq!(a.rule, "fleet.output-missing");
        assert_eq!(a.key, "release-notes/repo");
        assert_eq!(a.severity, Severity::Critical);
    }

    #[test]
    fn a_rule_loom_ui_derives_itself_is_not_sent() {
        assert!(to_alert(&t(Kind::Started, super::super::KEY_TOKENS, false)).is_none());
        assert!(to_alert(&t(Kind::Cleared, super::super::KEY_TOKENS, false)).is_none());
    }
}
