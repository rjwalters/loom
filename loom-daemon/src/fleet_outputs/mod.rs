//! Output registry + pure watchdog core for fleet singletons (#10916, slice 1).
//!
//! `fleet_captain` knows whether a singleton job is *armed* on a host, never
//! whether it still *produces* anything. This module is the single source of
//! truth for what each one-host fleet job must emit and how often, plus a pure
//! evaluator that judges observed output freshness. It is output-based: no
//! host identity is an input, so a job that moved to a host that does not
//! produce (or silently stopped) is caught the same way as a stalled one.
//!
//! Two gates elect the one producing host, and both are covered:
//! - [`Gate::SingletonJob`]: a named job in the `fleet_captain` registry,
//!   owned via `arm_singleton_job(<job>, ..)` (the captain) or, for the ETA
//!   jobs when `eta::job_owner` names an explicit authority (#10918),
//!   `record_owned_singleton_job(<job>, ..)`.
//! - [`Gate::EtaAuthority`]: `eta::authority::resolve{,_with}` (#10498). The
//!   10-07 incident (28 of 30 repos without `eta.estimate` for ~31h) was on
//!   this path, which records no named job at all.
//!
//! The `singleton_registry` tests scan the source tree so a new call site of
//! either gate cannot land without a row here or a reasoned exemption.
//!
//! Slice 1 only: no delivery wiring and no runtime behaviour change. Later
//! slices feed [`Condition`]s into `fleet_alert` and add a SigNoz rule.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::observability::captain_gauges::{QUEUE_BLOCKED_JOB, STAGE_DWELL_JOB, STAR_FACTS_JOB};

#[cfg(test)]
mod registry_tests;
#[cfg(test)]
mod tests;

/// Whether the output is one fleet-wide record or one per roster repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    FleetWide,
    /// One record per roster repo every pass (e.g. `eta.fleet_refresh`).
    PerRepo,
    /// Emitted only for repos with something to report (e.g. `eta.estimate` is
    /// per tracked item: an idle or all-abstaining repo legitimately emits
    /// nothing). Judged against [`OutputSource::expected_repos`], not the
    /// roster.
    PerActiveRepo,
}

/// How loudly a missing output should be reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Critical,
}

/// Which mechanism elects the single host that produces the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// A named `fleet_captain` singleton job; `job` is its name.
    SingletonJob,
    /// `eta::authority::resolve{,_with}`; `job` is [`ETA_AUTHORITY`].
    EtaAuthority,
}

/// The `job` name of every [`Gate::EtaAuthority`] row.
pub const ETA_AUTHORITY: &str = "eta-authority";

/// One output a singleton job is obliged to keep producing.
#[derive(Debug, Clone, Copy)]
pub struct SingletonOutput {
    /// The singleton job name, or [`ETA_AUTHORITY`].
    pub job: &'static str,
    pub gate: Gate,
    /// Record kind / heartbeat the job emits.
    pub record_kind: &'static str,
    pub scope: Scope,
    /// Expected emission period; the deadline is `2 * cadence` unless
    /// `deadline_override` is set.
    pub cadence: Duration,
    pub severity: Severity,
    /// Explicit deadline when the producer's own staleness contract is not
    /// `2 * cadence` (captain gauges: `maxAgeSecs`).
    pub deadline_override: Option<Duration>,
}

impl SingletonOutput {
    /// Age beyond which the output is stale.
    #[must_use]
    pub fn deadline(&self) -> Duration {
        self.deadline_override.unwrap_or(self.cadence * 2)
    }
}

const fn mins(m: u64) -> Duration {
    Duration::from_secs(m * 60)
}
const fn hours(h: u64) -> Duration {
    Duration::from_secs(h * 3600)
}

const fn job(
    job: &'static str,
    kind: &'static str,
    scope: Scope,
    cadence: Duration,
) -> SingletonOutput {
    SingletonOutput {
        job,
        gate: Gate::SingletonJob,
        record_kind: kind,
        scope,
        cadence,
        severity: Severity::Critical,
        deadline_override: None,
    }
}

const fn gauge(job_name: &'static str, kind: &'static str) -> SingletonOutput {
    SingletonOutput {
        deadline_override: Some(Duration::from_secs(
            crate::observability::captain_gauges::DEFAULT_MAX_AGE_SECS as u64,
        )),
        ..job(job_name, kind, Scope::FleetWide, mins(10))
    }
}

const fn authority(kind: &'static str, scope: Scope, cadence: Duration) -> SingletonOutput {
    SingletonOutput {
        job: ETA_AUTHORITY,
        gate: Gate::EtaAuthority,
        record_kind: kind,
        scope,
        cadence,
        severity: Severity::Critical,
        deadline_override: None,
    }
}

/// Every singleton output the fleet must keep producing.
pub const SINGLETON_OUTPUTS: &[SingletonOutput] = &[
    // ETA jobs: owned by the explicit ETA authority if set, else the captain
    // (#10918). One refresh record per repo per cycle (default 3600s).
    job(
        crate::observability::eta_fleet_refresh::SINGLETON_JOB_NAME,
        "eta.fleet_refresh",
        Scope::PerRepo,
        hours(1),
    ),
    job(
        crate::eta::nightly_folds::SINGLETON_JOB_NAME,
        "eta.backtest.fold",
        Scope::FleetWide,
        hours(24),
    ),
    // Runs complete fleet-wide around the clock, but a quiet spell is legal:
    // a long cadence and Warning, not Critical.
    SingletonOutput {
        severity: Severity::Warning,
        ..job(crate::ci_telemetry::SINGLETON_JOB_NAME, "ci.run", Scope::FleetWide, hours(3))
    },
    // Captain gauges: the per-job `as_of` in the `captain-gauges/v1`
    // heartbeat, published every 600s by default.
    // The deadline is the heartbeat's own `maxAgeSecs` (1800s), not 2x cadence
    // (1200s): one missed publish puts `as_of` near 1500s and must not fire.
    gauge(STAGE_DWELL_JOB, "captain-gauges/v1:stage-dwell"),
    gauge(STAR_FACTS_JOB, "captain-gauges/v1:star-facts"),
    gauge(QUEUE_BLOCKED_JOB, "captain-gauges/v1:queue-blocked"),
    // The ETA authority: the daily fit check (emitted fitted or skipped) and
    // per-repo estimates (the 10-07 incident).
    authority("eta.fit", Scope::FleetWide, hours(24)),
    authority("eta.estimate", Scope::PerActiveRepo, mins(30)),
];

/// Singleton job names deliberately absent from [`SINGLETON_OUTPUTS`], each with
/// a reason.
pub const EXEMPT: &[(&str, &str)] = &[(
    crate::intake_reconcile::singleton::SINGLETON_JOB_NAME,
    "emits no record kind yet: its output is forge labels, and a pass that finds nothing to \
     label is healthy; needs a pass heartbeat before it can be watched (#10916 follow-up)",
)];

/// Every source file (relative to `loom-daemon/src`) that calls
/// `eta::authority::resolve{,_with}`, and the registry `record_kind` that call
/// gates, or `None` with the reason it gates no output.
pub const AUTHORITY_SITES: &[(&str, Option<&str>, &str)] = &[
    (
        "observability/eta_fleet_refresh.rs",
        Some("eta.fit"),
        "fit_authority: only the authority fits",
    ),
    (
        "observability/eta/authority.rs",
        Some("eta.estimate"),
        "restore/refresh: a non-authority host drops pending estimates and emits none",
    ),
    ("eta/doctor_facts.rs", None, "read-only `loom-daemon doctor` diagnostic"),
];

/// Read access to when outputs were last observed (fleet store / local queue
/// in production, a fake in tests).
pub trait OutputSource {
    /// Newest observation of a fleet-wide `record_kind`; `None` = no data.
    fn last_seen(&self, record_kind: &str) -> Option<DateTime<Utc>>;
    /// Newest observation per repo for a per-repo `record_kind`.
    fn last_seen_per_repo(&self, record_kind: &str) -> BTreeMap<String, DateTime<Utc>>;
    /// Repos expected to emit a [`Scope::PerActiveRepo`] `record_kind` (e.g.
    /// those with at least one estimable tracked item). `None` = unknown,
    /// which is judged against the whole roster (fail loud). `Some(empty)`
    /// = nothing expected, so silence is healthy. Slice 2 must derive this
    /// independently of the silent output (never from `eta.estimate` itself).
    fn expected_repos(&self, _record_kind: &str) -> Option<Vec<String>> {
        None
    }
}

/// One output currently missing or stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    /// Stable per-output identity, e.g. `output-missing:eta-authority:eta.estimate`.
    pub key: String,
    pub job: &'static str,
    pub record_kind: &'static str,
    pub severity: Severity,
    pub headline: String,
}

/// Judge `registry` against `observed` at `now`. Absent data is firing.
///
/// A per-repo output must be fresh for every `roster` repo. An empty roster
/// (unknown, not "no repos") cannot excuse silence: the output must then be
/// fresh for at least one repo.
#[must_use]
pub fn evaluate(
    registry: &[SingletonOutput],
    observed: &dyn OutputSource,
    roster: &[String],
    now: DateTime<Utc>,
) -> Vec<Condition> {
    registry
        .iter()
        .filter_map(|o| {
            let headline = judge(o, observed, roster, now)?;
            Some(Condition {
                key: format!("output-missing:{}:{}", o.job, o.record_kind),
                job: o.job,
                record_kind: o.record_kind,
                severity: o.severity,
                headline,
            })
        })
        .collect()
}

/// The headline when `o` is missing or stale, `None` when healthy.
fn judge(
    o: &SingletonOutput,
    observed: &dyn OutputSource,
    roster: &[String],
    now: DateTime<Utc>,
) -> Option<String> {
    let deadline = o.deadline();
    // A timestamp in the future counts as fresh (small clock skew), but only
    // up to one deadline ahead: a producer clock a day fast must not mask an
    // outage.
    let fresh = |t: DateTime<Utc>| match now.signed_duration_since(t).to_std() {
        Ok(age) => age <= deadline,
        Err(_) => t
            .signed_duration_since(now)
            .to_std()
            .is_ok_and(|ahead| ahead <= deadline),
    };
    let kind = o.record_kind;
    let secs = deadline.as_secs();
    match o.scope {
        Scope::FleetWide => match observed.last_seen(kind) {
            None => Some(format!("{kind} never observed")),
            Some(t) if !fresh(t) => Some(format!("{kind} stale since {t} (deadline {secs}s)")),
            Some(_) => None,
        },
        Scope::PerRepo | Scope::PerActiveRepo => {
            let seen = observed.last_seen_per_repo(kind);
            let expected;
            let roster = if o.scope == Scope::PerActiveRepo {
                match observed.expected_repos(kind) {
                    Some(e) if e.is_empty() => return None,
                    Some(e) => {
                        expected = e;
                        &expected[..]
                    }
                    None => roster,
                }
            } else {
                roster
            };
            if roster.is_empty() {
                return (!seen.values().any(|t| fresh(*t))).then(|| {
                    format!("{kind} fresh for no repo (roster unknown, deadline {secs}s)")
                });
            }
            let ok = roster
                .iter()
                .filter(|r| seen.get(*r).is_some_and(|t| fresh(*t)))
                .count();
            (ok < roster.len()).then(|| {
                format!("{kind} fresh for {ok} of {} repos (deadline {secs}s)", roster.len())
            })
        }
    }
}
