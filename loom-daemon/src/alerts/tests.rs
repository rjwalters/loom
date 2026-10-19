//! Tests for the alert output (#11295, #12806): the schema's charsets, the
//! `alert.transition` record, the buffer before start and the heartbeat.

use std::time::{Duration, Instant};

use chrono::TimeZone;

use super::*;

fn alert(rule: &str, key: &str, state: State) -> Alert {
    Alert::new(rule, key, "title", Severity::Warning, state)
}

// ---------------------------------------------------------------- sanitize

#[test]
fn identifiers_are_fitted_to_the_contract_charset() {
    assert_eq!(sanitize::ident("fleet.dispatch-halted"), "fleet.dispatch-halted");
    assert_eq!(sanitize::ident("  -my host:1 "), "my-host-1");
    assert_eq!(sanitize::ident("…"), "unknown");
    let long = "a".repeat(100);
    let fitted = sanitize::ident(&long);
    assert_eq!(fitted.chars().count(), sanitize::IDENT_MAX);
    assert_eq!(fitted, sanitize::ident(&long), "stable for the same input");
}

#[test]
fn keys_keep_their_extra_characters_and_stay_distinct_when_cut() {
    assert_eq!(sanitize::key("owner/repo#12 x@y+z"), "owner/repo-12-x@y+z");
    let a = sanitize::key(&format!("{}a", "k".repeat(200)));
    let b = sanitize::key(&format!("{}b", "k".repeat(200)));
    assert_eq!(a.chars().count(), sanitize::KEY_MAX);
    assert_ne!(a, b, "the hash suffix keeps two long keys apart");
}

#[test]
fn titles_are_one_printable_line_of_at_most_200_characters() {
    assert_eq!(sanitize::title("a\nb\tc\u{7}", "r"), "a b c");
    assert_eq!(sanitize::title("  \n ", "fleet.x"), "fleet.x");
    let t = sanitize::title(&"é".repeat(300), "r");
    assert_eq!(t.chars().count(), sanitize::TITLE_MAX);
    assert!(t.ends_with('…'));
}

#[test]
fn detail_keeps_newlines_and_is_bounded() {
    assert_eq!(sanitize::detail("a\nb\u{1b}[0m"), "a\nb[0m");
    assert_eq!(sanitize::detail(&"x".repeat(5000)).chars().count(), sanitize::DETAIL_MAX);
}

#[test]
fn only_plain_https_links_are_sent() {
    assert_eq!(
        sanitize::link("https://github.com/o/r/issues/1").as_deref(),
        Some("https://github.com/o/r/issues/1")
    );
    assert_eq!(sanitize::link("http://github.com/x"), None);
    assert_eq!(sanitize::link("https://user:pw@github.com/x"), None);
    assert_eq!(sanitize::link(""), None);
    assert_eq!(sanitize::link(&format!("https://e.com/{}", "a".repeat(600))), None);
}

#[test]
fn the_record_has_exactly_the_schema_fields() {
    let at = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
    let a = Alert::new(
        "fleet.capacity-limited",
        "fleet",
        "Cap held below max",
        Severity::Critical,
        State::Open,
    )
    .with_detail("why")
    .with_link("https://github.com/o/r/issues/2");
    let r = record(&a, "host-1", at);
    assert_eq!(
        r,
        AlertTransitionRecord {
            source: "host-1".into(),
            rule: "fleet.capacity-limited".into(),
            key: "fleet".into(),
            state: "open".into(),
            severity: "critical".into(),
            title: "Cap held below max".into(),
            observed_at: at,
            detail: Some("why".into()),
            link: Some("https://github.com/o/r/issues/2".into()),
        }
    );
    assert_eq!(r.observed_at_wire(), "2026-10-19T12:00:00.000Z");
    let bare = record(
        &alert("r", "k", State::Resolved)
            .with_link("ftp://x")
            .with_detail(" "),
        "h",
        at,
    );
    assert!(bare.detail.is_none() && bare.link.is_none());
    assert_eq!(bare.state, "resolved");
    assert_eq!(bare.severity, "warning", "a resolve carries its severity too");
}

#[test]
fn an_emitted_alert_is_one_alert_transition_record() {
    let at = Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap();
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        emit(&alert("r", "k", State::Open), at);
    });
    assert_eq!(captured.records.len(), 1);
    let TelemetryRecord::AlertTransition(r) = &captured.records[0] else {
        panic!("not an alert.transition: {:?}", captured.records[0]);
    };
    assert_eq!((r.rule.as_str(), r.key.as_str(), r.state.as_str()), ("r", "k", "open"));
    assert_eq!(r.source, sanitize::ident(&source_host()));
    assert_eq!(r.observed_at, at);
}

// ------------------------------------------------- buffer and heartbeat

#[test]
fn held_alerts_are_bounded_and_drop_the_oldest() {
    let mut out = Output::default();
    let at = Utc::now();
    for i in 0..PENDING_CAPACITY {
        assert!(out
            .hold(alert("r", &i.to_string(), State::Open), at)
            .is_none());
    }
    let lost = out.hold(alert("r", "new", State::Open), at).unwrap();
    assert_eq!(lost.key, "0");
    assert_eq!(out.pending.len(), PENDING_CAPACITY);
    assert_eq!(out.pending.back().unwrap().0.key, "new");
}

#[test]
fn an_open_is_re_sent_every_heartbeat_until_resolved() {
    let mut out = Output::default();
    let t0 = Instant::now();
    out.track(&alert("r", "k", State::Open), t0);
    assert!(out.due(t0 + HEARTBEAT - Duration::from_secs(1)).is_empty());
    let due = out.due(t0 + HEARTBEAT);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].state, State::Open);
    // Marked sent: not due again until another heartbeat has passed.
    assert!(out.due(t0 + HEARTBEAT + Duration::from_secs(60)).is_empty());
    assert_eq!(out.due(t0 + HEARTBEAT * 2).len(), 1);
    out.track(&alert("r", "k", State::Resolved), t0 + HEARTBEAT * 2);
    assert!(out.due(t0 + HEARTBEAT * 4).is_empty());
}

#[test]
fn a_re_posted_open_restarts_its_heartbeat_with_the_new_text() {
    let mut out = Output::default();
    let t0 = Instant::now();
    out.track(&alert("r", "k", State::Open), t0);
    let mut update = alert("r", "k", State::Open);
    update.title = "worse".into();
    out.track(&update, t0 + HEARTBEAT / 2);
    assert!(out.due(t0 + HEARTBEAT).is_empty());
    assert_eq!(out.due(t0 + HEARTBEAT / 2 + HEARTBEAT)[0].title, "worse");
}

#[test]
fn the_heartbeat_is_well_inside_the_schemas_twelve_hours() {
    assert!(HEARTBEAT + HEARTBEAT_CHECK <= Duration::from_secs(12 * 60 * 60));
}

#[test]
fn an_adopted_incident_does_not_replace_a_live_one() {
    let mut out = Output::default();
    let t0 = Instant::now();
    let mut live = alert("r", "k", State::Open);
    live.title = "live".into();
    out.track(&live, t0);
    out.adopt(alert("r", "k", State::Open), t0 + HEARTBEAT);
    out.adopt(alert("r", "other", State::Open), t0);
    let due = out.due(t0 + HEARTBEAT);
    assert_eq!(due.len(), 2);
    assert!(due.iter().any(|a| a.title == "live"));
}

#[test]
fn post_before_start_is_held() {
    // The global is never started in the unit-test binary: the alert is held
    // for a start that has not happened, never emitted.
    assert!(!is_active());
    assert!(!is_off());
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        post(alert("r", "held-before-start", State::Open));
    });
    assert!(captured.records.is_empty());
    assert!(output()
        .pending
        .iter()
        .any(|(a, _)| a.key == "held-before-start"));
}
