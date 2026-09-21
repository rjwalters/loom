//! Unit tests for the saturation admission brake's contribution to
//! `host.health` (Issue #8478) — [`super::brake_summary_from_snapshot`],
//! [`super::sample_admission_brake`], and the brake arm of
//! [`super::dispatch_halt_from_breaker`].
//!
//! A sibling module rather than more lines in `collector/tests.rs`, which sits
//! exactly at the 1000-code-line ratchet threshold
//! (`scripts/check-file-size-budget.sh`) — the file-size policy's preferred
//! remedy is "put the new code in a new sibling module", not "trim something
//! unrelated to make room".

use super::*;

/// A tripped/closed host-distress breaker snapshot, for the cases where the
/// breaker and the brake interact. Mirrors `collector/tests.rs`'s own helper
/// rather than importing it: the two modules are independent `#[cfg(test)]`
/// trees, and a shared fixture between them would couple the brake tests to
/// edits in the breaker ones.
fn breaker_snapshot(
    phase: crate::host_breaker::BreakerPhase,
    reason: Option<&str>,
) -> crate::host_breaker::BreakerSnapshot {
    crate::host_breaker::BreakerSnapshot {
        enabled: true,
        phase,
        suppressed: phase.suppresses_dispatch(),
        reason: reason.map(str::to_string),
        tripped_at: None,
        releases_at: None,
        last_load_per_core: Some(4.24),
        load_per_core_threshold: 2.5,
        sustain_ticks: 3,
        cooldown_secs: 300,
        consecutive_over: 3,
    }
}

/// A brake snapshot with `starvation_warn_secs = 300` (the shipped default),
/// held, and starving for `starving_secs` — or not starving at all when
/// `starving_secs` is `None`.
fn brake_snapshot(
    held: bool,
    starving_secs: Option<i64>,
    now: DateTime<Utc>,
) -> crate::admission_brake::BrakeSnapshot {
    crate::admission_brake::BrakeSnapshot {
        enabled: true,
        held,
        load_per_core: Some(2.9),
        load_per_core_threshold: 0.95,
        held_since: held.then(|| now - chrono::Duration::seconds(starving_secs.unwrap_or(0))),
        held_ticks: 12,
        starving_since: starving_secs.map(|secs| now - chrono::Duration::seconds(secs)),
        starving_ticks: starving_secs.map_or(0, |_| 12),
        escape_hatch_grants: 3,
        starvation_warn_secs: 300,
        starvation_escape_secs: 900,
    }
}

#[test]
fn brake_summary_is_absent_when_no_brake_is_registered() {
    assert!(
        brake_summary_from_snapshot(None, Utc::now()).is_none(),
        "absent must mean 'not reported', never a fabricated not-suppressed verdict"
    );
}

#[test]
fn brake_summary_reports_a_healthy_brake_without_claiming_suppression() {
    let now = Utc::now();
    let summary = brake_summary_from_snapshot(Some(brake_snapshot(false, None, now)), now)
        .expect("a registered brake always reports");
    assert!(!summary.held);
    assert_eq!(summary.starving_secs, None);
    assert!(
        !summary.dispatch_suppressed_by_foreign_load,
        "an admitting brake must never read as suppressed"
    );
    assert!(summary.top_cpu_consumers.is_none());
}

#[test]
fn brake_summary_exposes_the_starvation_duration_not_just_its_start() {
    let now = Utc::now();
    let summary = brake_summary_from_snapshot(Some(brake_snapshot(true, Some(43_440), now)), now)
        .expect("registered");
    assert_eq!(
        summary.starving_secs,
        Some(43_440),
        "the 12h incident behind #8478 is a DURATION question; starving_since alone \
         forced every consumer to do remote-clock arithmetic"
    );
    assert!(summary.dispatch_suppressed_by_foreign_load);
}

#[test]
fn brake_summary_withholds_the_suppressed_verdict_below_this_hosts_own_warn_threshold() {
    let now = Utc::now();
    // 299s of starvation against this host's own 300s warn threshold: the
    // brake is held, but it has not yet held long enough for the host itself
    // to consider it alarming, so the fleet must not alert either.
    let summary =
        brake_summary_from_snapshot(Some(brake_snapshot(true, Some(299), now)), now).expect("x");
    assert_eq!(summary.starving_secs, Some(299));
    assert!(
        !summary.dispatch_suppressed_by_foreign_load,
        "the verdict is evaluated against the EMITTING host's threshold, not a \
         hardcoded fleet constant"
    );
    let at_threshold =
        brake_summary_from_snapshot(Some(brake_snapshot(true, Some(300), now)), now).expect("x");
    assert!(
        at_threshold.dispatch_suppressed_by_foreign_load,
        "exactly at the threshold counts — the local STARVING line fires here too"
    );
}

#[test]
fn brake_summary_never_reports_a_negative_duration_under_clock_skew() {
    let now = Utc::now();
    // `starving_since` momentarily ahead of `now` (a clock adjustment
    // mid-tick). "Just started" is the only honest answer; a negative
    // duration would read as a valid past instant to a consumer.
    let summary = brake_summary_from_snapshot(Some(brake_snapshot(true, Some(-60), now)), now)
        .expect("registered");
    assert_eq!(summary.starving_secs, Some(0));
    assert!(!summary.dispatch_suppressed_by_foreign_load);
}

#[test]
fn a_sustained_starving_brake_halts_dispatch_fleet_wide_with_no_breaker_trip() {
    // THE incident (#8478): the host-distress breaker never tripped, so
    // pre-#8478 this host reported `dispatch_halted: false` and rendered
    // fleet-wide as healthy and idle for 12 hours.
    let now = Utc::now();
    let summary = brake_summary_from_snapshot(Some(brake_snapshot(true, Some(43_440), now)), now)
        .expect("registered");
    let (halted, reason) = dispatch_halt_from_breaker(None, Some(&summary));
    assert!(halted, "a brake starving past its own threshold IS this host refusing work");
    let reason = reason.expect("a halt must always say why");
    assert!(reason.contains("43440s"), "reason was: {reason}");
    assert!(
        reason.contains("load Loom does not own"),
        "the fleet view's whole job here is to say WHOSE load it is; reason was: {reason}"
    );
}

#[test]
fn a_brake_held_below_its_warn_threshold_is_not_a_fleet_wide_halt() {
    // Ordinary backpressure. Reporting this as a halt would flag every busy
    // host in the fleet, which is the failure mode #4975 explicitly avoided
    // ("busy != degraded").
    let now = Utc::now();
    let summary =
        brake_summary_from_snapshot(Some(brake_snapshot(true, Some(30), now)), now).expect("x");
    assert_eq!(dispatch_halt_from_breaker(None, Some(&summary)), (false, None));
}

#[test]
fn the_breaker_keeps_priority_over_the_brake_when_both_fire() {
    let now = Utc::now();
    let summary = brake_summary_from_snapshot(Some(brake_snapshot(true, Some(43_440), now)), now)
        .expect("registered");
    let breaker = breaker_snapshot(
        crate::host_breaker::BreakerPhase::Open,
        Some("load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)"),
    );
    let (halted, reason) = dispatch_halt_from_breaker(Some(breaker), Some(&summary));
    assert!(halted);
    assert_eq!(
        reason.as_deref(),
        Some("load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)"),
        "the stickier, more severe condition leads; #8478 adds a second cause, \
         it does not displace the first"
    );
}

#[test]
fn the_halt_reason_carries_the_foreign_load_attribution_when_one_was_sampled() {
    let now = Utc::now();
    let mut summary =
        brake_summary_from_snapshot(Some(brake_snapshot(true, Some(43_440), now)), now)
            .expect("registered");
    summary.top_cpu_consumers = Some(
        " \u{2014} TOP CPU (best-effort host-wide `ps` sample, includes work Loom does not \
         own): ngspice \u{d7}25 (1843% cpu, parent launchd[1], reparented to pid 1)"
            .to_string(),
    );
    let (_, reason) = dispatch_halt_from_breaker(None, Some(&summary));
    let reason = reason.expect("halted");
    assert!(
        reason.contains("ngspice \u{d7}25"),
        "a fleet operator must be able to name the culprit without ssh-ing to the \
         host; reason was: {reason}"
    );
}

#[tokio::test]
async fn sample_admission_brake_skips_the_ps_probe_on_a_healthy_host() {
    // The probe gate is on `dispatch_suppressed_by_foreign_load`, not on
    // `held`: a merely-busy host must never pay for a subprocess on every
    // `host.health` flush.
    let now = Utc::now();
    let summary = sample_admission_brake(Some(brake_snapshot(true, Some(30), now)))
        .await
        .expect("registered");
    assert!(
        summary.top_cpu_consumers.is_none(),
        "no attribution is sampled while the host is not suppressed"
    );
}
