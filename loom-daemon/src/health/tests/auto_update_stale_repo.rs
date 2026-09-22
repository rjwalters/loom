//! `assess_auto_update`'s wrong-repo-resolution escalation (Issue #8513).
//!
//! The sibling `health::auto_update_stale_repo` module unit-tests the
//! threshold and the wording; what is here is the wiring only the whole
//! section can answer — that the finding outranks the staleness rules, that
//! it reaches the overall verdict, and that the streak is reported in
//! `detail` even below the threshold.

use super::*;

#[test]
fn auto_update_is_green_below_the_stale_repo_tick_threshold() {
    // One or two consecutive stale-repo ticks could be forge-query jitter —
    // must not page on that alone.
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_stale_repo_ticks = 2;
    status.auto_update_stale_repo = Some("consumer-owner/consumer-repo".to_string());
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
}

#[test]
fn auto_update_is_degraded_and_names_the_repo_past_the_stale_repo_tick_threshold() {
    // The exact #8513 shape: a daemon stuck resolving a consumer repo's own
    // (lower-versioned) releases across several consecutive ticks.
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_note = Some(
        "resolved release 0.1.0 from consumer-owner/consumer-repo is OLDER than the installed \
         0.19.24 — probable wrong-repo resolution (queried consumer-owner/consumer-repo); \
         nothing to fetch"
            .to_string(),
    );
    status.auto_update_stale_repo_ticks = 3;
    status.auto_update_stale_repo = Some("consumer-owner/consumer-repo".to_string());
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
    assert!(section.summary.contains("consumer-owner/consumer-repo"), "{}", section.summary);
    assert!(section.summary.contains("no progress"), "{}", section.summary);
    // Overall must escalate too — this is a hard finding, not an FYI.
    assert_eq!(assess(&inputs).overall, Verdict::Degraded);
}

#[test]
fn auto_update_stale_repo_escalation_is_independent_of_source_staleness() {
    // Must fire even when the SOURCE checkout is perfectly current — the
    // staleness-magnitude checks compare against source HEAD, which says
    // nothing about a release-artifact-path wrong-repo resolution.
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_stale_repo_ticks = 5;
    status.auto_update_stale_repo = Some("consumer-owner/consumer-repo".to_string());
    inputs.self_update = Some(healthy_self_update());
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Degraded, "{}", section.summary);
}

#[test]
fn a_disabled_loop_still_reports_green_regardless_of_the_streak() {
    // A deliberate opt-out outranks every finding in this section; a stale
    // streak left over from before the opt-out must not resurrect it.
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = false;
    status.auto_update_stale_repo_ticks = 9;
    status.auto_update_stale_repo = Some("consumer-owner/consumer-repo".to_string());
    let section = assess_auto_update(&inputs);
    assert_eq!(section.verdict, Verdict::Green, "{}", section.summary);
}

#[test]
fn auto_update_detail_carries_the_stale_repo_fields() {
    // Reported even below the threshold: an operator debugging a host wants
    // the streak visible before it escalates, not only after.
    let mut inputs = healthy_inputs();
    let status = inputs.status.as_mut().unwrap();
    status.auto_update_enabled = true;
    status.auto_update_stale_repo_ticks = 1;
    status.auto_update_stale_repo = Some("consumer-owner/consumer-repo".to_string());
    let detail = assess_auto_update(&inputs).detail;
    assert_eq!(detail["stale_repo_ticks"], serde_json::json!(1));
    assert_eq!(detail["stale_repo"], serde_json::json!("consumer-owner/consumer-repo"));
}
