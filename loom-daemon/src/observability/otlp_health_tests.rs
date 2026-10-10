use super::*;
use chrono::TimeZone;

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 0).unwrap() + Duration::minutes(min)
}

fn base() -> OtlpHealthInputs {
    OtlpHealthInputs {
        feature_otlp: true,
        otlp_planned: true,
        exempt_reason: None,
        started_at: Some(t(0)),
        last_success_at: None,
        last_drop_growth_at: None,
        window: Duration::minutes(15),
    }
}

#[test]
fn https_only_is_no_exporter() {
    let mut i = base();
    i.otlp_planned = false;
    assert_eq!(evaluate(&i, t(1)).state, OtlpExportState::NoExporter);
}

#[test]
fn build_without_otlp_feature_is_no_exporter() {
    let mut i = base();
    i.feature_otlp = false;
    let h = evaluate(&i, t(1));
    assert_eq!(h.state, OtlpExportState::NoExporter);
    assert!(h.detail.unwrap().contains("feature"));
}

#[test]
fn startup_grace_then_failing_then_recovers() {
    let i = base();
    assert_eq!(evaluate(&i, t(10)).state, OtlpExportState::Ok);
    assert_eq!(evaluate(&i, t(16)).state, OtlpExportState::Failing);
    let mut ok = i.clone();
    ok.last_success_at = Some(t(16));
    assert_eq!(evaluate(&ok, t(17)).state, OtlpExportState::Ok);
    // A success that has itself aged out is failing again.
    assert_eq!(evaluate(&ok, t(32)).state, OtlpExportState::Failing);
}

#[test]
fn growing_drops_fail_even_with_recent_success() {
    let mut i = base();
    i.last_success_at = Some(t(10));
    i.last_drop_growth_at = Some(t(9));
    assert_eq!(evaluate(&i, t(11)).state, OtlpExportState::Failing);
    assert_eq!(evaluate(&i, t(30)).state, OtlpExportState::Failing); // stale success
    i.last_success_at = Some(t(29));
    assert_eq!(evaluate(&i, t(30)).state, OtlpExportState::Ok); // growth aged out
}

#[test]
fn exempt_wins_and_carries_reason() {
    let mut i = base();
    i.otlp_planned = false;
    i.exempt_reason = Some("robb-studio per 2am#3649".to_string());
    let h = evaluate(&i, t(1));
    assert_eq!(h.state, OtlpExportState::Exempt);
    assert_eq!(h.detail.as_deref(), Some("robb-studio per 2am#3649"));
}

#[test]
fn empty_reason_is_not_an_exemption() {
    let mut cfg = ObservabilityConfig {
        otlp_required: Some(false),
        otlp_exempt_reason: Some("   ".to_string()),
        ..Default::default()
    };
    let (reason, warning) = exemption(&cfg);
    assert!(reason.is_none() && warning.is_some());
    cfg.otlp_exempt_reason = Some("why".to_string());
    assert_eq!(exemption(&cfg).0.as_deref(), Some("why"));
    cfg.otlp_required = Some(true);
    assert_eq!(exemption(&cfg), (None, None));
}

#[test]
fn window_defaults_and_overrides() {
    let mut cfg = ObservabilityConfig::default();
    assert_eq!(failure_window(&cfg), Duration::minutes(15));
    cfg.otlp_failure_window_minutes = Some(5);
    assert_eq!(failure_window(&cfg), Duration::minutes(5));
    cfg.otlp_failure_window_minutes = Some(0);
    assert_eq!(failure_window(&cfg), Duration::minutes(15));
}

#[test]
fn monitor_reports_change_once_per_state() {
    let mut m = OtlpHealthMonitor::default();
    let mut no_exp = base();
    no_exp.otlp_planned = false;
    no_exp.started_at = None;
    let mut warns = 0;
    for min in 0..5 {
        let o = m.observe(no_exp.clone(), None, t(min));
        if o.changed && warn_line(&o.health).is_some() {
            warns += 1;
        }
    }
    assert_eq!(warns, 1);
    // State change (now planned, past grace since first seen) warns again.
    let mut planned = base();
    planned.started_at = None;
    let o = m.observe(planned.clone(), None, t(20));
    assert_eq!(o.health.state, OtlpExportState::Failing);
    assert!(o.changed);
    assert!(!m.observe(planned, None, t(21)).changed);
}

#[test]
fn monitor_drop_latch_is_idempotent_and_ignores_reset() {
    let mut m = OtlpHealthMonitor::default();
    let mut i = base();
    i.last_success_at = Some(t(1));
    assert_eq!(m.observe(i.clone(), Some(5), t(1)).health.state, OtlpExportState::Ok);
    assert_eq!(m.observe(i.clone(), Some(7), t(2)).health.state, OtlpExportState::Failing);
    // Same counter again: latch unchanged (still within window from t(2)).
    assert_eq!(m.observe(i.clone(), Some(7), t(3)).health.state, OtlpExportState::Failing);
    i.last_success_at = Some(t(20));
    assert_eq!(m.observe(i.clone(), Some(7), t(20)).health.state, OtlpExportState::Ok);
    // Counter reset (restart) is not growth.
    assert_eq!(m.observe(i, Some(0), t(21)).health.state, OtlpExportState::Ok);
}

#[test]
fn serializes_snake_case_and_omits_empty_detail() {
    let v = serde_json::to_value(OtlpExportHealth {
        state: OtlpExportState::NoExporter,
        detail: None,
    })
    .unwrap();
    assert_eq!(v, serde_json::json!({"state": "no_exporter"}));
}
