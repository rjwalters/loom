use super::*;
use chrono::TimeZone;

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 0).unwrap() + Duration::minutes(min)
}

fn base() -> OtlpHealthInputs {
    OtlpHealthInputs {
        otlp_planned: true,
        exempt_reason: None,
        started_at: Some(t(0)),
        last_success_at: None,
        last_drop_growth_at: None,
        window: Duration::minutes(15),
        last_failure_detail: None,
        kind_last_export: Default::default(),
    }
}

#[test]
fn https_only_is_no_exporter() {
    let mut i = base();
    i.otlp_planned = false;
    assert_eq!(evaluate(&i, t(1)).state, OtlpExportState::NoExporter);
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
fn every_transition_is_logged_once_and_repeats_are_silent() {
    let mut m = OtlpHealthMonitor::default();
    let mut lines = Vec::new();
    let mut step = |m: &mut OtlpHealthMonitor, i: OtlpHealthInputs, min: i64| {
        let o = m.observe(i, None, t(min));
        if let Some((level, line)) = log_line(&o) {
            lines.push((o.health.state, level, line));
        }
    };
    // Startup in a steady-good state (exempt) still logs once.
    let mut exempt = base();
    exempt.exempt_reason = Some("lab".into());
    step(&mut m, exempt.clone(), 0);
    step(&mut m, exempt, 1);
    // exempt -> no_exporter (WARN)
    let mut none = base();
    none.otlp_planned = false;
    step(&mut m, none, 2);
    // no_exporter -> failing (WARN), then failing -> ok (recovery INFO)
    let mut failing = base();
    failing.started_at = Some(t(-60));
    step(&mut m, failing.clone(), 3);
    step(&mut m, failing.clone(), 4);
    let mut ok = failing;
    ok.last_success_at = Some(t(5));
    step(&mut m, ok.clone(), 5);
    step(&mut m, ok, 6);
    let seen: Vec<_> = lines.iter().map(|(s, l, _)| (*s, *l)).collect();
    assert_eq!(
        seen,
        vec![
            (OtlpExportState::Exempt, log::Level::Info),
            (OtlpExportState::NoExporter, log::Level::Warn),
            (OtlpExportState::Failing, log::Level::Warn),
            (OtlpExportState::Ok, log::Level::Info),
        ]
    );
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
fn monitor_out_of_order_reading_does_not_double_count_growth() {
    let mut m = OtlpHealthMonitor::default();
    let i = OtlpHealthInputs {
        last_success_at: Some(t(30)),
        ..base()
    };
    m.observe(i.clone(), Some(5), t(1));
    // Stale lower reading applied late, then the true reading again: only the
    // first rise above the high-water mark counts, and 5 -> 5 is not growth.
    m.observe(i.clone(), Some(3), t(30));
    assert_eq!(m.observe(i, Some(5), t(31)).health.state, OtlpExportState::Ok);
}

#[test]
fn serializes_snake_case_and_omits_empty_detail() {
    let v = serde_json::to_value(OtlpExportHealth {
        state: OtlpExportState::NoExporter,
        detail: None,
        since: None,
        kinds: Vec::new(),
    })
    .unwrap();
    assert_eq!(v, serde_json::json!({"state": "no_exporter"}));
}

// ---- Issue #11557: `since`, failure cause, degraded start ----------------

fn failing_inputs(cause: &str) -> OtlpHealthInputs {
    OtlpHealthInputs {
        started_at: Some(t(-60)),
        last_failure_detail: Some(cause.to_string()),
        ..base()
    }
}

#[test]
fn repeated_failing_samples_keep_the_original_since() {
    let mut m = OtlpHealthMonitor::default();
    let i = failing_inputs("OTLP HTTP 503");
    for min in 0..5 {
        let h = m.observe(i.clone(), None, t(min)).health;
        assert_eq!(h.state, OtlpExportState::Failing);
        assert_eq!(h.since, Some(t(0)));
    }
}

#[test]
fn failure_detail_updates_do_not_reset_since() {
    let mut m = OtlpHealthMonitor::default();
    m.observe(failing_inputs("OTLP HTTP 503"), None, t(0));
    let h = m
        .observe(failing_inputs("OTLP HTTP 403 (credential refused …)"), None, t(3))
        .health;
    assert_eq!(h.since, Some(t(0)));
    assert!(h.detail.unwrap().contains("403"));
    // Failing -> no_exporter is still the same stretch of attention.
    let mut none = base();
    none.otlp_planned = false;
    assert_eq!(m.observe(none, None, t(4)).health.since, Some(t(0)));
}

#[test]
fn recovery_clears_since_and_a_later_failure_starts_a_new_interval() {
    let mut m = OtlpHealthMonitor::default();
    m.observe(failing_inputs("OTLP HTTP 503"), None, t(0));
    let mut ok = failing_inputs("OTLP HTTP 503");
    ok.last_success_at = Some(t(10));
    let h = m.observe(ok.clone(), None, t(10)).health;
    assert_eq!((h.state, h.since), (OtlpExportState::Ok, None));
    assert_eq!(h.summary(), None);
    // The success ages out: a new interval opens at the first failing sample.
    let h = m.observe(ok.clone(), None, t(26)).health;
    assert_eq!((h.state, h.since), (OtlpExportState::Failing, Some(t(26))));
    // Exempt also closes the interval.
    let mut exempt = ok;
    exempt.exempt_reason = Some("lab".into());
    assert_eq!(m.observe(exempt, None, t(27)).health.since, None);
}

#[test]
fn failing_detail_names_the_exporters_last_failure() {
    let h = evaluate(&failing_inputs("OTLP HTTP 403 (credential refused; batch held)"), t(0));
    let detail = h.detail.unwrap();
    assert!(detail.contains("no successful OTLP export since start"), "{detail}");
    assert!(detail.contains("last failure: OTLP HTTP 403 (credential refused"), "{detail}");
    // No exporter failure recorded: the symptom alone, as before.
    let mut i = base();
    i.started_at = Some(t(-60));
    assert!(!evaluate(&i, t(0)).detail.unwrap().contains("last failure"));
}

#[test]
fn warn_line_says_since() {
    let mut m = OtlpHealthMonitor::default();
    let o = m.observe(failing_inputs("OTLP HTTP 403"), None, t(0));
    let (level, line) = log_line(&o).unwrap();
    assert_eq!(level, log::Level::Warn);
    assert!(line.contains("otlp: failing since 2026-10-10T00:00:00Z ("), "{line}");
}

#[test]
fn startup_verdict_is_degraded_only_for_no_exporter() {
    let mut m = OtlpHealthMonitor::default();
    // A misconfigured OTLP cell reaches the monitor as not-planned plus its
    // own detail; `since` is this first (daemon-start) observation.
    let mut misconfigured = base();
    misconfigured.otlp_planned = false;
    misconfigured.started_at = None;
    misconfigured.last_failure_detail =
        Some("observability.ingestKeyFile not configured".to_string());
    let h = m.observe(misconfigured, None, t(0)).health;
    assert_eq!(h.since, Some(t(0)));
    let StartupVerdict::Degraded(detail) = startup_verdict(&h) else {
        panic!("no_exporter must be a degraded start");
    };
    assert!(detail.contains("no_exporter since 2026-10-10T00:00:00Z"), "{detail}");
    assert!(detail.contains("ingestKeyFile not configured"), "{detail}");
    assert!(degraded_line(&detail).starts_with(
        "observability: DEGRADED — this daemon's facts are not reaching SigNoz: otlp: no_exporter"
    ));
    // A fresh exporter in grace, and an exempt host, start fine.
    assert_eq!(startup_verdict(&evaluate(&base(), t(1))), StartupVerdict::Ok);
    let mut exempt = base();
    exempt.otlp_planned = false;
    exempt.exempt_reason = Some("lab".into());
    assert_eq!(startup_verdict(&evaluate(&exempt, t(1))), StartupVerdict::Ok);
}

#[test]
fn since_serializes_when_set() {
    let h = OtlpExportHealth {
        state: OtlpExportState::Failing,
        detail: None,
        since: Some(t(0)),
        kinds: Vec::new(),
    };
    let v = serde_json::to_value(&h).unwrap();
    assert_eq!(v["since"], serde_json::json!("2026-10-10T00:00:00Z"));
    let back: OtlpExportHealth = serde_json::from_value(v).unwrap();
    assert_eq!(back, h);
}

#[test]
fn failure_detail_is_scrubbed_of_credential_values() {
    let raw = "transport error: error sending request for url \
               (https://ingest.example/v1/logs?token=s3cr3tvalue123): Authorization: Bearer abcdefghijklmnopqrstuvwxyz";
    let clean = sanitize_failure_detail(raw);
    assert!(!clean.contains("s3cr3tvalue123"), "{clean}");
    assert!(!clean.contains("abcdefghijklmnopqrstuvwxyz"), "{clean}");
    assert!(clean.contains("[REDACTED:"), "{clean}");
    // A plain status detail passes through unchanged.
    let plain = "OTLP HTTP 403 (credential refused; batch held until the credential is fixed)";
    assert_eq!(sanitize_failure_detail(plain), plain);
}

// ---- Issue #12120: per-kind silence -------------------------------------

fn all_exported_at(at: DateTime<Utc>) -> std::collections::BTreeMap<String, DateTime<Utc>> {
    crate::telemetry::kinds::exported_kinds_for(&["otlp".to_string()])
        .into_iter()
        .map(|k| (k, at))
        .collect()
}

fn fresh_inputs(now: DateTime<Utc>) -> OtlpHealthInputs {
    OtlpHealthInputs {
        started_at: Some(now - Duration::hours(100)),
        last_success_at: Some(now),
        kind_last_export: all_exported_at(now),
        ..base()
    }
}

#[test]
fn fresh_kinds_are_ok_and_listed() {
    let now = t(60 * 100);
    let h = evaluate(&fresh_inputs(now), now);
    assert_eq!(h.state, OtlpExportState::Ok);
    assert!(h
        .kinds
        .iter()
        .any(|k| k.kind == "host.health" && k.last_export_at == Some(now)));
    assert!(h.silent_kinds().is_empty());
}

#[test]
fn a_stale_watched_kind_flags_silent_kinds() {
    let now = t(60 * 100);
    let mut i = fresh_inputs(now);
    i.kind_last_export
        .insert("host.health".to_string(), now - Duration::hours(25));
    let h = evaluate(&i, now);
    assert_eq!(h.state, OtlpExportState::SilentKinds);
    assert_eq!(h.silent_kinds(), vec!["host.health"]);
    assert!(h.needs_attention());
    assert!(h.summary().unwrap().contains("host.health"));
}

#[test]
fn a_kind_just_inside_the_threshold_is_not_flagged() {
    let now = t(60 * 100);
    let mut i = fresh_inputs(now);
    i.kind_last_export
        .insert("host.health".to_string(), now - Duration::hours(23));
    assert_eq!(evaluate(&i, now).state, OtlpExportState::Ok);
}

#[test]
fn an_exempt_kind_is_never_flagged() {
    let now = t(60 * 100);
    let mut i = fresh_inputs(now);
    i.kind_last_export.remove("sweep.phase");
    i.started_at = Some(now - Duration::hours(1000));
    assert_eq!(evaluate(&i, now).state, OtlpExportState::Ok);
}

#[test]
fn restart_with_no_export_yet_is_healthy_inside_24h_and_flagged_after() {
    let now = t(60 * 100);
    let mut i = fresh_inputs(now);
    i.kind_last_export.remove("host.health");
    i.started_at = Some(now - Duration::hours(2));
    let young = evaluate(&i, now);
    assert_eq!(young.state, OtlpExportState::Ok);
    assert_eq!(
        young
            .kinds
            .iter()
            .find(|k| k.kind == "host.health")
            .unwrap()
            .last_export_at,
        None
    );
    i.started_at = Some(now - Duration::hours(30));
    assert_eq!(evaluate(&i, now).state, OtlpExportState::SilentKinds);
}

#[test]
fn not_configured_reports_no_exporter_with_no_kinds() {
    let now = t(60 * 100);
    let mut i = fresh_inputs(now);
    i.otlp_planned = false;
    let h = evaluate(&i, now);
    assert_eq!(h.state, OtlpExportState::NoExporter);
    assert!(h.kinds.is_empty());
}

#[test]
fn failing_outranks_silent_kinds_but_still_lists_kinds() {
    let now = t(60 * 100);
    let mut i = fresh_inputs(now);
    i.last_success_at = Some(now - Duration::hours(2));
    let h = evaluate(&i, now);
    assert_eq!(h.state, OtlpExportState::Failing);
    assert!(!h.kinds.is_empty());
}

#[test]
fn an_older_record_without_kinds_decodes() {
    let h: OtlpExportHealth = serde_json::from_str(r#"{"state":"ok"}"#).unwrap();
    assert!(h.kinds.is_empty());
}
