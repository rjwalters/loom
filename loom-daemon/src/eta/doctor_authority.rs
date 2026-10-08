//! The `config.authority` and `config.last_emit` doctor checks (#10498,
//! #10898), split out of [`super::doctor`] to keep that file from growing.

use chrono::{DateTime, Utc};

use super::doctor::{Check, Status};
use super::emit_heartbeat::{self, Heartbeat};

/// The ETA authority resolution as the doctor sees it (#10498).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityFacts {
    /// The authority host id, `None` when no host qualifies.
    pub host: Option<String>,
    /// Why (`explicit`, `fleet_refresh`, `lowest_id_fallback`, `no_candidate`).
    pub reason: String,
    /// This host is the authority.
    pub is_local: bool,
    /// Other qualifying hosts: a conflict when non-empty.
    pub others: Vec<String>,
    /// The resolver's one-line explanation.
    pub detail: String,
    /// The last authority pass against the cached fleet roster (#10897);
    /// [`State::Unknown`](crate::eta::coverage::State::Unknown) when no pass
    /// was recorded here or the roster is unknown.
    pub coverage: crate::eta::coverage::Coverage,
    /// The host whose pass `coverage` describes (this host's view).
    pub coverage_host: Option<String>,
    /// The emit heartbeat this host last wrote (#10898); `None` when none.
    pub emit: Option<Heartbeat>,
}

/// #10498: which host is the fleet's one ETA authority, and why.
pub(super) fn authority(a: &AuthorityFacts) -> Check {
    if a.host.is_none() || !a.others.is_empty() {
        return Check::bad(
            "config",
            "authority",
            Status::Warn,
            format!("ETA authority: {}", a.detail),
            "set fleet.etaAuthority (or LOOM_ETA_AUTHORITY) to the one host that should emit \
             eta.* records",
        );
    }
    Check::ok("config", "authority", format!("ETA authority: {}", a.detail))
}

/// #10897: the authority's last pass against the fleet roster. Prints the
/// host the view belongs to, since a non-authority host only has its own.
pub(super) fn authority_coverage(a: &AuthorityFacts) -> Check {
    use crate::eta::coverage::State;
    let whose = a.coverage_host.as_deref().unwrap_or("this host");
    match a.coverage.state {
        State::Unknown => Check::skip(
            "config",
            "authority_coverage",
            format!(
                "authority coverage unknown: no authority pass recorded on this host or no \
                 fleet roster cached (fleet.repo); reporting {whose}'s view"
            ),
        ),
        State::Full => Check::ok(
            "config",
            "authority_coverage",
            format!("ETA authority covers {} ({whose}'s last pass)", a.coverage.describe()),
        ),
        State::Short => Check::bad(
            "config",
            "authority_coverage",
            Status::Warn,
            format!("ETA authority covers {} ({whose}'s last pass)", a.coverage.describe()),
            "move fleet.etaAuthority to a host that manages every roster repo (other hosts \
             keep emitting the uncovered repos meanwhile; declare fleet.etaAuthorityCovers once \
             it is fixed), or wait for the workspace-less authority (#10897 Slice 2)",
        ),
    }
}

/// #10898: when did this authority last *emit*? FAIL past
/// [`emit_heartbeat::SILENT_AFTER`] (the 2 h of the incident), WARN when it
/// has never recorded one. Only meaningful on the authority; elsewhere SKIP.
pub(super) fn last_emit(a: &AuthorityFacts, now: DateTime<Utc>) -> Check {
    if !a.is_local {
        return Check::skip("config", "last_emit", "this host is not the ETA authority");
    }
    let Some(hb) = &a.emit else {
        return Check::bad(
            "config",
            "last_emit",
            Status::Warn,
            "last emit: none recorded on this host",
            "wait one ETA pass (5 min) after the daemon starts; if this persists, check that an \
             OTLP exporter is configured (`config.otlp_exporter`) and the daemon is running",
        );
    };
    let Some(at) = hb.last_emit_at else {
        return Check::bad(
            "config",
            "last_emit",
            Status::Warn,
            format!(
                "last emit: never (last pass {}, {} repo(s) covered)",
                hb.pass_at, hb.repos_covered
            ),
            "the authority has run passes but offered no record to an exporter: configure an \
             OTLP exporter (observability exporters) so eta.estimate reaches SigNoz",
        );
    };
    let age = now - at;
    let detail = format!(
        "last emit: {at} ({}m ago); {} repo(s) covered, {} open PR(s)",
        age.num_minutes(),
        hb.repos_covered,
        hb.open_prs
    );
    if age > emit_heartbeat::SILENT_AFTER {
        return Check::bad(
            "config",
            "last_emit",
            Status::Fail,
            detail,
            "the authority has not emitted ETA records for over 2 h: check the OTLP exporter and \
             `loom-daemon status` task liveness (eta_pass); records are dropped without an exporter",
        );
    }
    Check::ok("config", "last_emit", detail)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
    }

    fn facts(is_local: bool, last_emit_mins_ago: Option<i64>, heartbeat: bool) -> AuthorityFacts {
        AuthorityFacts {
            host: Some("robb-studio".into()),
            reason: "explicit".into(),
            is_local,
            others: Vec::new(),
            detail: "robb-studio".into(),
            coverage: crate::eta::coverage::Coverage::default(),
            coverage_host: None,
            emit: heartbeat.then(|| Heartbeat {
                host: "robb-studio".into(),
                pass_at: now(),
                last_emit_at: last_emit_mins_ago.map(|m| now() - Duration::minutes(m)),
                repos_covered: 2,
                open_prs: 3,
            }),
        }
    }

    #[test]
    fn a_recent_emit_is_ok() {
        let c = last_emit(&facts(true, Some(30), true), now());
        assert_eq!(c.status, Status::Ok, "{}", c.render());
        assert!(c.detail.contains("30m ago") && c.detail.contains("2 repo(s)"));
    }

    #[test]
    fn an_emit_older_than_two_hours_fails() {
        let c = last_emit(&facts(true, Some(121), true), now());
        assert_eq!(c.status, Status::Fail, "{}", c.render());
        assert!(c.remedy.unwrap().contains("OTLP exporter"));
        let edge = last_emit(&facts(true, Some(120), true), now());
        assert_eq!(edge.status, Status::Ok, "exactly 2 h is not yet silent");
    }

    #[test]
    fn passes_without_any_emit_warn_and_name_the_exporter() {
        let c = last_emit(&facts(true, None, true), now());
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("never"));
        let none = last_emit(&facts(true, None, false), now());
        assert_eq!(none.status, Status::Warn);
    }

    #[test]
    fn a_follower_skips() {
        let c = last_emit(&facts(false, Some(9999), true), now());
        assert_eq!(c.status, Status::Skip);
    }
}
