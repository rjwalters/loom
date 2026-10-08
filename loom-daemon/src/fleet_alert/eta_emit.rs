//! "ETA authority silent" fleet condition (#10898).
//!
//! On 2026-10-07/08 the ETA authority covered two repos for ~31 h without
//! emitting a record, and the only signals ("the loop finished", the fit and
//! refresh checks) all read healthy. This is the on-host detector: the
//! authority ages its own emit heartbeat ([`crate::eta::emit_heartbeat`]) and
//! raises one de-duplicated [`Condition`] when it has not emitted for the
//! threshold while review PRs are open. It is the same check `loom-daemon eta
//! doctor` prints as `config.last_emit`; the cross-host detector is the SigNoz
//! rule `alerts/eta-not-emitted.json`.
//!
//! No forge calls: it reads the config and one local file.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::Condition;
use crate::eta::emit_heartbeat::{self, Heartbeat};

/// Condition key (stable; used in the inbox mail key and persisted state).
pub const KEY_ETA_SILENT: &str = "eta-authority-silent";

/// The condition for an authority at `now`, from its heartbeat. `started` is
/// the process start (the grace origin). Silence only matters while there is
/// something to estimate: a heartbeat whose last pass saw no open PRs is a
/// quiet fleet, but one whose last *pass* is itself older than the threshold
/// says nothing about open PRs and so does not excuse the silence. Pure.
#[must_use]
pub fn condition(
    hb: Option<&Heartbeat>,
    now: DateTime<Utc>,
    started: DateTime<Utc>,
) -> Option<Condition> {
    let silence = emit_heartbeat::assess(hb, now, started, emit_heartbeat::SILENT_AFTER);
    if !silence.silent {
        return None;
    }
    let pass_current = hb.is_some_and(|h| now - h.pass_at <= emit_heartbeat::SILENT_AFTER);
    if pass_current && hb.is_some_and(|h| h.open_prs == 0) {
        return None;
    }
    let age = silence.age_secs.unwrap_or(0);
    let since = if silence.ever_emitted {
        format!("no ETA record emitted for {}h{:02}m", age / 3600, age % 3600 / 60)
    } else {
        format!(
            "no ETA record emitted since the daemon started {}h{:02}m ago",
            age / 3600,
            age % 3600 / 60
        )
    };
    let covered = hb.map_or(0, |h| h.repos_covered);
    Some(Condition {
        key: KEY_ETA_SILENT,
        headline: format!(
            "This host is the ETA authority but {since} ({covered} repo(s) covered). ETAs are \
             not reaching SigNoz or the dashboard."
        ),
        fix: "Run `loom-daemon eta doctor` and read config.last_emit; the usual cause is no OTLP \
              exporter (observability.exporters). `eta.estimate` is OTLP-only."
            .to_string(),
    })
}

/// Evaluate for the workspace at `root` as `host_id`: only a host that is the
/// ETA authority with ETA enabled can raise it.
#[must_use]
pub fn evaluate(root: &Path, host_id: &str, now: DateTime<Utc>) -> Option<Condition> {
    if !crate::eta::config::read(root).enabled {
        return None;
    }
    let resolution = crate::eta::authority::resolve_with(root, host_id, |k| std::env::var(k).ok());
    if !resolution.is_authority() {
        return None;
    }
    condition(emit_heartbeat::read(root).as_ref(), now, emit_heartbeat::process_started())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 0, 0, 0).unwrap()
    }

    fn hb(last_emit: Option<DateTime<Utc>>, pass: DateTime<Utc>, open_prs: u64) -> Heartbeat {
        Heartbeat {
            host: "h".into(),
            pass_at: pass,
            last_emit_at: last_emit,
            repos_covered: 2,
            open_prs,
        }
    }

    #[test]
    fn silent_past_the_threshold_with_open_prs_raises_the_condition() {
        let started = t0() - Duration::hours(40);
        let now = t0() + Duration::hours(3);
        let c = condition(Some(&hb(Some(t0()), now, 4)), now, started).unwrap();
        assert_eq!(c.key, KEY_ETA_SILENT);
        assert!(c.headline.contains("3h00m") && c.headline.contains("2 repo(s)"));
        assert!(c.fix.contains("eta doctor"));
    }

    #[test]
    fn a_fresh_emit_or_a_quiet_fleet_raises_nothing() {
        let started = t0() - Duration::hours(40);
        let now = t0() + Duration::minutes(119);
        assert!(condition(Some(&hb(Some(t0()), now, 4)), now, started).is_none());
        let later = t0() + Duration::hours(5);
        assert!(condition(Some(&hb(Some(t0()), later, 0)), later, started).is_none());
    }

    #[test]
    fn a_stale_pass_does_not_excuse_the_silence() {
        // The last pass is itself older than the threshold: its "0 open PRs"
        // is no longer evidence, so the silence stands.
        let started = t0() - Duration::hours(40);
        let now = t0() + Duration::hours(5);
        assert!(condition(Some(&hb(Some(t0()), t0(), 0)), now, started).is_some());
    }

    #[test]
    fn an_authority_that_never_emitted_is_silent_after_the_threshold_of_uptime() {
        let started = t0();
        assert!(condition(None, t0() + Duration::hours(1), started).is_none());
        let c = condition(None, t0() + Duration::hours(3), started).unwrap();
        assert!(c.headline.contains("since the daemon started"));
    }

    #[test]
    fn a_restart_gives_the_authority_a_fresh_window() {
        let now = t0() + Duration::hours(31);
        let started = now - Duration::minutes(2);
        assert!(condition(Some(&hb(Some(t0()), t0(), 4)), now, started).is_none());
    }
}
