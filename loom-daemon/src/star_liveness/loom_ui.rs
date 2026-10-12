//! The loom-ui output for starred-issue escalations (#11295).
//!
//! Every [`Notice`] the ledger produces — one per escalation it posts, one per
//! escalation it announced that cleared — becomes an `open` / `resolved`
//! incident on loom-ui's Alerts page, beside the forge comment and the Matrix
//! relay. It rides the ledger's existing once-per-cause dedupe, so no second
//! dedupe exists to drift.
//!
//! Rule `star.<ask kind>` (`star.operator-only`, `star.no-progress` …), key
//! `<owner>/<repo>/<issue>/<ask key>` (sanitized), link the issue. The
//! recovery half is only as durable as the ledger's: a resolve owed across a
//! daemon restart is not sent, and the incident is resolved by hand.

use super::escalate::Notice;
use crate::loom_ui_alerts::{Alert, Severity, State};

/// The loom-ui alert for `notice`. Open and resolved share rule and key.
#[must_use]
pub fn to_alert(notice: &Notice) -> Alert {
    let kind = notice.kind.as_str();
    let (state, title) = if notice.resolved {
        (
            State::Resolved,
            format!("Starred {}#{}: {kind} escalation resolved", notice.repo, notice.issue),
        )
    } else {
        let ask = notice.text.lines().next().unwrap_or_default();
        (
            State::Open,
            format!("Starred {}#{} needs the operator ({kind}): {ask}", notice.repo, notice.issue),
        )
    };
    let mut alert = Alert::new(
        format!("star.{kind}"),
        format!("{}/{}/{}", notice.repo, notice.issue, notice.key),
        title,
        Severity::Warning,
        state,
    )
    .with_link(notice.url.clone());
    if !notice.resolved {
        let mut detail = format!("Stage: {}\n{}", notice.stage.as_str(), notice.text);
        if let Some(parent) = notice.inherited_from {
            detail.push_str(&format!("\nStar inherited from #{parent}."));
        }
        alert = alert.with_detail(detail);
    }
    alert
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loom_ui_alerts::payload;
    use crate::types::{AskKind, LandingStage};

    fn notice(resolved: bool) -> Notice {
        Notice {
            repo: "rjwalters/loom".to_string(),
            issue: 42,
            key: "operator-only:loom:operator-only".to_string(),
            kind: AskKind::OperatorOnly,
            stage: LandingStage::Building,
            text: if resolved {
                String::new()
            } else {
                "Needs a decision on X.\nMore.".to_string()
            },
            url: "https://github.com/rjwalters/loom/issues/42".to_string(),
            inherited_from: None,
            resolved,
        }
    }

    #[test]
    fn escalation_opens_and_recovery_resolves_one_incident() {
        let open = to_alert(&notice(false));
        let done = to_alert(&notice(true));
        assert_eq!(open.rule, "star.operator-only");
        assert_eq!(open.state, State::Open);
        assert_eq!(done.state, State::Resolved);
        let (a, b) = (payload(&open, "host-1"), payload(&done, "host-1"));
        assert_eq!(a["rule"], b["rule"]);
        assert_eq!(a["key"], b["key"]);
        assert_eq!(a["key"], "rjwalters/loom/42/operator-only-loom-operator-only");
        assert_eq!(a["link"], "https://github.com/rjwalters/loom/issues/42");
        assert!(a["title"]
            .as_str()
            .unwrap()
            .contains("Needs a decision on X."));
        assert!(a["detail"].as_str().unwrap().contains("More."));
        assert!(b.get("detail").is_none());
    }
}
