use super::*;
use crate::auto_update::ArtifactInfo;
use chrono::TimeZone;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 16, 0, 0, 0).unwrap()
}

fn mins(n: i64) -> chrono::Duration {
    chrono::Duration::minutes(n)
}

fn hours(n: i64) -> chrono::Duration {
    chrono::Duration::hours(n)
}

fn started() -> Freshness {
    let mut f = Freshness::default();
    f.set_tuning(Thresholds::default(), t0());
    f
}

fn ok(pending: Option<&str>) -> Observation {
    Observation {
        success: true,
        pending: pending.map(str::to_string),
    }
}

const ERR: Observation = Observation {
    success: false,
    pending: None,
};

fn finding(f: &Freshness, at: DateTime<Utc>) -> Option<String> {
    f.report(at, "note").finding(at)
}

#[test]
fn defaults_are_multiples_of_the_interval_and_clear_the_settle_ceiling() {
    let t = Thresholds::default();
    assert_eq!(t.overdue_after, t.interval * 4);
    assert!(t.converge_deadline > Duration::from_secs(6 * 600));
}

#[test]
fn never_succeeded_is_fine_within_the_startup_allowance_and_degraded_after() {
    let mut f = started();
    f.observe(t0() + mins(10), &ERR);
    assert_eq!(finding(&f, t0() + mins(30)), None);
    let why = finding(&f, t0() + mins(61)).expect("overdue");
    assert!(why.contains("has not completed a successful release check"), "{why}");
}

#[test]
fn errors_never_count_as_a_fresh_check() {
    let mut f = started();
    f.observe(t0(), &ok(None));
    for i in 1..=6 {
        f.observe(t0() + mins(15 * i), &ERR);
    }
    let why = finding(&f, t0() + mins(95)).expect("last success is 95m old");
    assert!(why.contains("last successful release check"), "{why}");
    assert!(why.contains("errors do not count"), "{why}");
    // One real answer clears it.
    f.observe(t0() + mins(96), &ok(None));
    assert_eq!(finding(&f, t0() + mins(97)), None);
}

#[test]
fn a_newer_release_is_a_finding_only_past_the_deadline_and_follows_no_checkout() {
    let mut f = started();
    f.observe(t0(), &ok(Some("0.20.0")));
    // Settling / draining: well inside the deadline.
    f.observe(t0() + hours(1), &ok(Some("0.20.0")));
    assert_eq!(finding(&f, t0() + hours(1)), None);
    let deadline = Thresholds::default().converge_deadline;
    let past = t0() + chrono::Duration::from_std(deadline).unwrap() + mins(1);
    f.observe(past, &ok(Some("0.20.0")));
    let why = finding(&f, past).expect("unapplied past the deadline");
    assert!(why.contains("0.20.0"), "{why}");
    assert!(why.contains(env!("CARGO_PKG_VERSION")), "{why}");
}

#[test]
fn a_release_every_tick_does_not_reset_the_deadline() {
    let mut f = started();
    let deadline = chrono::Duration::from_std(Thresholds::default().converge_deadline).unwrap();
    let mut at = t0();
    let mut n = 0;
    while at < t0() + deadline + mins(30) {
        n += 1;
        f.observe(at, &ok(Some(&format!("0.20.{n}"))));
        at += mins(15);
    }
    assert!(finding(&f, at).is_some(), "origin must survive 0.20.1 .. 0.20.{n}");
    assert_eq!(f.record().pending_target.as_deref(), Some(format!("0.20.{n}").as_str()));
}

#[test]
fn a_failed_check_keeps_the_pending_clock_and_a_current_answer_clears_it() {
    let mut f = started();
    f.observe(t0(), &ok(Some("0.20.0")));
    f.observe(t0() + hours(1), &ERR);
    assert_eq!(f.record().pending_since, Some(t0()));
    f.observe(t0() + hours(2), &ok(None));
    assert_eq!(f.record().pending_since, None);
    assert_eq!(f.record().pending_target, None);
}

#[test]
fn restart_while_stale_keeps_the_clocks_but_a_roll_resets_convergence() {
    let mut f = started();
    f.observe(t0(), &ok(Some("0.20.0")));
    let saved = f.record();
    let later = t0() + hours(40);

    let mut same = Freshness::default();
    same.set_tuning(Thresholds::default(), later);
    same.restore(saved.clone(), later, true);
    same.observe(later, &ERR);
    assert!(finding(&same, later).is_some(), "stale across a restart of the same binary");

    let mut rolled = Freshness::default();
    rolled.set_tuning(Thresholds::default(), later);
    rolled.restore(saved, later, false);
    assert_eq!(rolled.record().pending_since, None);
}

#[test]
fn clock_steps_never_produce_a_negative_age_or_a_phantom_finding() {
    let mut f = started();
    f.observe(t0() + hours(5), &ok(Some("0.20.0")));
    // Wall clock stepped back before the recorded times.
    assert_eq!(finding(&f, t0()), None);
    // A saved time in the future is read as now on restore.
    let mut r = Freshness::default();
    r.set_tuning(Thresholds::default(), t0());
    r.restore(f.record(), t0(), true);
    assert_eq!(r.record().last_success, Some(t0()));
    assert_eq!(finding(&r, t0() + mins(1)), None);
}

#[test]
fn one_alert_per_incident_and_one_recovery() {
    let mut f = started();
    f.observe(t0(), &ok(None));
    let mut edges = Vec::new();
    for i in 1..=12 {
        let at = t0() + mins(15 * i);
        f.observe(at, &ERR);
        let r = f.report(at, "backing off");
        edges.extend(f.edge(at, &r));
    }
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert!(matches!(&edges[0], Edge::Opened(s) if s.contains("backing off")));
    let at = t0() + hours(4);
    f.observe(at, &ok(None));
    let r = f.report(at, "ok");
    assert_eq!(f.edge(at, &r), Some(Edge::Recovered));
    assert_eq!(f.edge(at + mins(15), &r), None);
}

fn info(version: &str) -> ArtifactInfo {
    ArtifactInfo {
        version: version.to_string(),
        tag: format!("v{version}"),
        ..Default::default()
    }
}

fn unchecked() -> UpdateCheck {
    UpdateCheck {
        update_available: None,
        source_commit: None,
        commits_behind: None,
        hours_behind: None,
    }
}

#[test]
fn classify_separates_answers_from_attempts_without_a_checkout() {
    let err = ArtifactResolution::Unresolved("rate limited".into());
    assert_eq!(
        classify(&err, &unchecked(), true),
        Observation {
            success: false,
            pending: None
        }
    );
    let newer = ArtifactResolution::Resolved(info("999.0.0"));
    let o = classify(&newer, &unchecked(), true);
    assert!(o.success);
    assert_eq!(o.pending.as_deref(), Some("999.0.0"));
    // A fleet host moves with the floor, not the newest release.
    assert_eq!(classify(&newer, &unchecked(), false).pending, None);
    let stale_source = UpdateCheck {
        update_available: Some(true),
        ..unchecked()
    };
    let o = classify(&err, &stale_source, true);
    assert!(o.success && o.pending.is_some());
}

#[test]
fn disabled_updates_have_no_report_to_assess() {
    let status = DaemonStatusReport {
        auto_update_enabled: false,
        ..Default::default()
    };
    assert_eq!(finding_for(&status, t0()), None);
}
