//! `assess_auto_update`'s progress findings (Issue #9736): the wiring only the
//! whole section can answer. The clocks and thresholds are unit-tested in
//! `auto_update::freshness`.

use super::*;
use crate::auto_update::freshness::FreshnessReport;

fn report(inputs: &HealthInputs, last_success_mins_ago: Option<i64>) -> FreshnessReport {
    FreshnessReport {
        running_version: "0.19.1".to_string(),
        tracking_since: inputs.at - chrono::Duration::hours(10),
        last_success: last_success_mins_ago.map(|m| inputs.at - chrono::Duration::minutes(m)),
        pending_target: None,
        pending_since: None,
        next_check: None,
        blocking_reason: Some("fetch failed (attempt 7, backing off 900s)".to_string()),
        overdue_after_secs: 3600,
        converge_deadline_secs: 90_000,
    }
}

#[test]
fn a_backing_off_host_with_no_checkout_is_degraded_once_the_check_is_overdue() {
    let mut inputs = healthy_inputs();
    inputs.self_update = None; // no source checkout: the old rules see nothing
    let fresh = report(&inputs, Some(180));
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_backoff_secs = Some(900);
    status.auto_update_freshness = Some(fresh);
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    assert!(section.summary.contains("fetch failed"), "{}", section.summary);
}

#[test]
fn a_recent_successful_check_keeps_the_section_green() {
    let mut inputs = healthy_inputs();
    let fresh = report(&inputs, Some(5));
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_freshness = Some(fresh);
    assert_eq!(assess_auto_update(&inputs).verdict, Verdict::Green);
}

#[test]
fn a_disabled_loop_is_green_and_distinct_from_a_stalled_one() {
    let mut inputs = healthy_inputs();
    let fresh = report(&inputs, None);
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = false;
    status.auto_update_freshness = Some(fresh);
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Green);
    assert!(section.summary.contains("disabled"), "{}", section.summary);
}

#[test]
fn the_terminal_reason_keeps_its_own_diagnostic_over_the_progress_finding() {
    let mut inputs = healthy_inputs();
    let fresh = report(&inputs, Some(600));
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_terminal_reason = Some("bad signature".to_string());
    status.auto_update_freshness = Some(fresh);
    let section = assess_auto_update(&inputs);
    assert!(section.summary.contains("TERMINALLY"), "{}", section.summary);
}
