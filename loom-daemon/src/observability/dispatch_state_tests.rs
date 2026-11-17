//! Pure tracker/builder tests for `dispatch_state` (Issue #12918).

use chrono::{Duration, TimeZone, Utc};

use super::*;
use crate::telemetry::queue_repo::QueueRepoRef;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
}

fn row(ws_idx: usize, number: u32) -> StateRow {
    StateRow {
        ws_idx,
        number,
        artifact: Artifact::Issue,
        step: Some("builder".into()),
        in_pool: true,
        exclusion: None,
        gate_until: None,
        reeval_rung: None,
        reeval_next: None,
        iw: Some(10),
        weight: Some(100),
        issue: None,
        doctor_rejections: None,
        doctor_escalated: None,
    }
}

fn resolved(slug: &str, row: StateRow) -> ResolvedRow {
    ResolvedRow {
        slug: slug.into(),
        visibility: RepoVisibility::Public,
        row,
    }
}

fn observed(slugs: &[&str]) -> HashSet<String> {
    slugs.iter().map(|s| (*s).to_string()).collect()
}

const REFRESH: i64 = 600;

#[test]
fn first_sight_emits_changed_and_an_unchanged_item_stays_quiet() {
    let mut tracker = StateTracker::default();
    let rows = vec![resolved("o/r", row(0, 1))];
    let first = tracker.diff(&rows, &observed(&["o/r"]), at(0), Duration::seconds(REFRESH));
    assert_eq!(first, vec![(0, StateTransition::Changed)]);
    let second = tracker.diff(&rows, &observed(&["o/r"]), at(60), Duration::seconds(REFRESH));
    assert!(second.is_empty(), "{second:?}");
}

#[test]
fn weight_drift_alone_is_not_a_change() {
    let mut tracker = StateTracker::default();
    let mut a = row(0, 1);
    tracker.diff(
        &[resolved("o/r", a.clone())],
        &observed(&["o/r"]),
        at(0),
        Duration::seconds(REFRESH),
    );
    a.iw = Some(99);
    a.weight = Some(999);
    let out = tracker.diff(
        &[resolved("o/r", a)],
        &observed(&["o/r"]),
        at(60),
        Duration::seconds(REFRESH),
    );
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn each_signature_field_change_emits() {
    let base = row(0, 1);
    let mut cases = Vec::new();
    let mut c = base.clone();
    c.in_pool = false;
    cases.push(("in_pool", c));
    let mut c = base.clone();
    c.exclusion = Some("cooling_down".into());
    cases.push(("exclusion", c));
    let mut c = base.clone();
    c.gate_until = Some(at(900));
    cases.push(("gate_until", c));
    let mut c = base.clone();
    c.reeval_rung = Some(3);
    cases.push(("rung", c));
    for (name, changed) in cases {
        let mut tracker = StateTracker::default();
        tracker.diff(
            &[resolved("o/r", base.clone())],
            &observed(&["o/r"]),
            at(0),
            Duration::seconds(REFRESH),
        );
        let out = tracker.diff(
            &[resolved("o/r", changed)],
            &observed(&["o/r"]),
            at(60),
            Duration::seconds(REFRESH),
        );
        assert_eq!(out, vec![(0, StateTransition::Changed)], "{name}");
    }
}

#[test]
fn unchanged_item_is_re_emitted_by_the_periodic_reconciliation() {
    let mut tracker = StateTracker::default();
    let rows = vec![resolved("o/r", row(0, 1))];
    tracker.diff(&rows, &observed(&["o/r"]), at(0), Duration::seconds(REFRESH));
    let early =
        tracker.diff(&rows, &observed(&["o/r"]), at(REFRESH - 1), Duration::seconds(REFRESH));
    assert!(early.is_empty());
    let due = tracker.diff(&rows, &observed(&["o/r"]), at(REFRESH), Duration::seconds(REFRESH));
    assert_eq!(due, vec![(0, StateTransition::Refresh)]);
    // The window restarts from the refresh.
    let after =
        tracker.diff(&rows, &observed(&["o/r"]), at(REFRESH + 1), Duration::seconds(REFRESH));
    assert!(after.is_empty());
}

#[test]
fn no_row_cap_more_than_256_items_all_emit() {
    let mut tracker = StateTracker::default();
    let rows: Vec<ResolvedRow> = (1..=600).map(|n| resolved("o/r", row(0, n))).collect();
    let out = tracker.diff(&rows, &observed(&["o/r"]), at(0), Duration::seconds(REFRESH));
    assert_eq!(out.len(), 600);
}

#[test]
fn a_departed_item_is_forgotten_but_an_unobserved_repo_is_kept() {
    let mut tracker = StateTracker::default();
    let rows = vec![resolved("o/r", row(0, 1)), resolved("o/held", row(1, 2))];
    tracker.diff(&rows, &observed(&["o/r", "o/held"]), at(0), Duration::seconds(REFRESH));
    // Next tick: `o/r` observed with no rows; `o/held` not observed at all.
    let out = tracker.diff(&[], &observed(&["o/r"]), at(60), Duration::seconds(REFRESH));
    assert!(out.is_empty());
    // `o/held` returns unchanged: still quiet (not re-announced).
    let held = tracker.diff(
        &[resolved("o/held", row(1, 2))],
        &observed(&["o/held"]),
        at(120),
        Duration::seconds(REFRESH),
    );
    assert!(held.is_empty(), "{held:?}");
    // `o/r`'s item reappears: first sight again.
    let back = tracker.diff(
        &[resolved("o/r", row(0, 1))],
        &observed(&["o/r"]),
        at(180),
        Duration::seconds(REFRESH),
    );
    assert_eq!(back, vec![(0, StateTransition::Changed)]);
}

#[test]
fn resolve_rows_drops_unresolved_workspaces_and_names_observed_slugs() {
    let resolve = |idx: usize| {
        (idx == 0).then(|| QueueRepoRef {
            repo: "o/r".into(),
            visibility: RepoVisibility::Private,
        })
    };
    let observed_ws: BTreeSet<usize> = [0, 1].into_iter().collect();
    let (rows, slugs) = resolve_rows(vec![row(0, 1), row(1, 2)], &observed_ws, resolve);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].slug, "o/r");
    assert_eq!(rows[0].visibility, RepoVisibility::Private);
    assert_eq!(slugs, observed(&["o/r"]));
}

#[test]
fn refresh_secs_falls_back_on_garbage() {
    assert_eq!(refresh_secs(None), DEFAULT_REFRESH_SECS);
    assert_eq!(refresh_secs(Some("0")), DEFAULT_REFRESH_SECS);
    assert_eq!(refresh_secs(Some("x")), DEFAULT_REFRESH_SECS);
    assert_eq!(refresh_secs(Some(" 30 ")), 30);
}

#[test]
fn build_record_carries_the_gate_and_omits_what_is_absent() {
    let mut r = row(0, 5);
    r.in_pool = false;
    r.exclusion = Some("cooling_down".into());
    r.gate_until = Some(at(300));
    r.iw = None;
    r.weight = None;
    let record = build_record(&resolved("o/r", r), StateTransition::Changed, "host-a", at(0), None);
    assert_eq!(record.repo, "o/r");
    assert_eq!(record.gate_until, Some(at(300)));
    assert_eq!(record.iw, None);
    let value = serde_json::to_value(&record).unwrap();
    assert!(value.get("weight").is_none());
}

#[test]
fn claimed_since_is_the_first_observation_and_resets_when_unclaimed() {
    let claimed = |n| {
        let mut r = row(0, n);
        r.in_pool = false;
        r.exclusion = Some("claimed".into());
        resolved("o/r", r)
    };
    let free = |n| resolved("o/r", row(0, n));
    let seen = observed(&["o/r"]);
    let refresh = Duration::seconds(REFRESH);
    let mut t = StateTracker::default();
    t.diff(&[free(1)], &seen, at(0), refresh);
    assert_eq!(t.claimed_since("o/r", 1), None, "unclaimed is absent, not now");
    t.diff(&[claimed(1)], &seen, at(60), refresh);
    t.diff(&[claimed(1)], &seen, at(120), refresh);
    assert_eq!(t.claimed_since("o/r", 1), Some(at(60)));
    t.diff(&[free(1)], &seen, at(180), refresh);
    assert_eq!(t.claimed_since("o/r", 1), None);
    t.diff(&[claimed(1)], &seen, at(240), refresh);
    assert_eq!(t.claimed_since("o/r", 1), Some(at(240)), "a re-claim restarts the bound");
}

#[test]
fn doctor_state_is_scoped_to_its_pr() {
    let cp =
        br#"{"phase":"doctor-done","doctor_escalation":{"pr":7,"rejections":2,"escalated":true}}"#;
    assert_eq!(doctor_state_from(cp, 7), Some((2, true)));
    assert_eq!(doctor_state_from(cp, 8), None, "another PR's history never applies");
    assert_eq!(doctor_state_from(br#"{"phase":"x"}"#, 7), None);
    assert_eq!(doctor_state_from(b"not json", 7), None);
}
