use super::*;
use chrono::TimeZone;

#[derive(Default)]
struct Fake {
    fleet: BTreeMap<&'static str, DateTime<Utc>>,
    repos: BTreeMap<&'static str, BTreeMap<String, DateTime<Utc>>>,
    expected: BTreeMap<&'static str, Vec<String>>,
}
impl OutputSource for Fake {
    fn last_seen(&self, k: &str) -> Option<DateTime<Utc>> {
        self.fleet.get(k).copied()
    }
    fn last_seen_per_repo(&self, k: &str) -> BTreeMap<String, DateTime<Utc>> {
        self.repos.get(k).cloned().unwrap_or_default()
    }
    fn expected_repos(&self, k: &str) -> Option<Vec<String>> {
        self.expected.get(k).cloned()
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}
fn ago(m: i64) -> DateTime<Utc> {
    now() - chrono::Duration::minutes(m)
}
fn roster(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("org/repo{i}")).collect()
}
const FLEET: SingletonOutput = job("j", "k", Scope::FleetWide, mins(30));
const PER_REPO: SingletonOutput = job("j", "e", Scope::PerRepo, mins(30));

fn row_for(kind: &str) -> SingletonOutput {
    *SINGLETON_OUTPUTS
        .iter()
        .find(|o| o.record_kind == kind)
        .unwrap()
}

/// Every registry output observed at `at` for every roster repo.
fn whole_fleet_at(at: DateTime<Utc>, r: &[String]) -> Fake {
    let mut f = Fake::default();
    for o in SINGLETON_OUTPUTS {
        match o.scope {
            Scope::FleetWide => {
                f.fleet.insert(o.record_kind, at);
            }
            Scope::PerRepo | Scope::PerActiveRepo => {
                f.repos
                    .insert(o.record_kind, r.iter().map(|n| (n.clone(), at)).collect());
            }
        }
    }
    f
}

#[test]
fn stale_beyond_twice_cadence_is_critical() {
    let mut f = Fake::default();
    f.fleet.insert("k", ago(61));
    let c = evaluate(&[FLEET], &f, &[], now());
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].severity, Severity::Critical);
    assert_eq!(c[0].key, "output-missing:j:k");
}

#[test]
fn within_twice_cadence_is_quiet() {
    let mut f = Fake::default();
    f.fleet.insert("k", ago(59));
    assert!(evaluate(&[FLEET], &f, &[], now()).is_empty());
}

#[test]
fn absent_data_is_firing() {
    let c = evaluate(&[FLEET], &Fake::default(), &[], now());
    assert_eq!(c.len(), 1);
    assert!(c[0].headline.contains("never observed"));
}

#[test]
fn absent_data_fires_for_every_registry_row() {
    let c = evaluate(SINGLETON_OUTPUTS, &Fake::default(), &roster(30), now());
    assert_eq!(c.len(), SINGLETON_OUTPUTS.len());
}

#[test]
fn per_repo_two_of_thirty_is_firing() {
    let r = roster(30);
    let mut f = Fake::default();
    f.repos
        .insert("e", r.iter().take(2).map(|n| (n.clone(), ago(5))).collect());
    let c = evaluate(&[PER_REPO], &f, &r, now());
    assert_eq!(c.len(), 1);
    assert!(c[0].headline.contains("2 of 30"));
}

#[test]
fn per_repo_full_coverage_is_quiet() {
    let r = roster(30);
    let mut f = Fake::default();
    f.repos
        .insert("e", r.iter().map(|n| (n.clone(), ago(5))).collect());
    assert!(evaluate(&[PER_REPO], &f, &r, now()).is_empty());
}

#[test]
fn per_repo_unknown_roster_still_needs_one_fresh_repo() {
    assert_eq!(evaluate(&[PER_REPO], &Fake::default(), &[], now()).len(), 1);
    let mut f = Fake::default();
    f.repos
        .insert("e", BTreeMap::from([("org/a".to_string(), ago(5))]));
    assert!(evaluate(&[PER_REPO], &f, &[], now()).is_empty());
}

#[test]
fn healthy_fleet_has_no_conditions() {
    let r = roster(30);
    assert!(evaluate(SINGLETON_OUTPUTS, &whole_fleet_at(ago(1), &r), &r, now()).is_empty());
}

/// 10-07: `eta.estimate` stopped for 28 of 30 repos for ~31h while the ETA
/// authority stayed "armed". The watchdog must fire by 2x cadence after the
/// stop and keep firing for the whole outage.
#[test]
fn replays_the_10_07_incident_within_twice_cadence() {
    let row = row_for("eta.estimate");
    assert_eq!(row.gate, Gate::EtaAuthority);
    let r = roster(30);
    let stop = now();
    let mut f = Fake::default();
    // Every repo emitted last at `stop`.
    f.repos
        .insert("eta.estimate", r.iter().map(|n| (n.clone(), stop)).collect());
    let deadline = chrono::Duration::from_std(row.cadence * 2).unwrap();
    let step = chrono::Duration::minutes(1);
    // Quiet up to the deadline.
    assert!(evaluate(&[row], &f, &r, stop + deadline).is_empty());
    // The two surviving repos keep emitting; the 28 never do again.
    let mut fired_at = None;
    let mut t = stop;
    while t <= stop + chrono::Duration::hours(31) {
        let survivors = f.repos.get_mut("eta.estimate").unwrap();
        for n in r.iter().take(2) {
            survivors.insert(n.clone(), t);
        }
        let c = evaluate(&[row], &f, &r, t);
        if fired_at.is_none() && !c.is_empty() {
            assert!(c[0].headline.contains("2 of 30"), "{}", c[0].headline);
            assert_eq!(c[0].key, "output-missing:eta-authority:eta.estimate");
            fired_at = Some(t);
        }
        if fired_at.is_some() {
            assert_eq!(c.len(), 1, "stopped firing at {t}");
        }
        t += step;
    }
    assert_eq!(fired_at, Some(stop + deadline + step));
}

#[test]
fn job_moved_to_non_producing_host_fires_without_host_input() {
    // eta-fleet-refresh was re-armed on a new host that never refreshes: the
    // captain gate reads healthy, but the output stops. The evaluator has no
    // host parameter: the output's age alone fires it.
    let row = row_for("eta.fleet_refresh");
    let r = roster(30);
    let moved =
        now() - chrono::Duration::from_std(row.cadence * 2).unwrap() - chrono::Duration::minutes(1);
    let f = whole_fleet_at(moved, &r);
    let c = evaluate(&[row], &f, &r, now());
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].job, crate::observability::eta_fleet_refresh::SINGLETON_JOB_NAME);
    assert!(c[0].headline.contains("0 of 30"));
}

#[test]
fn future_timestamp_is_not_stale() {
    let mut f = Fake::default();
    f.fleet.insert("k", now() + chrono::Duration::minutes(1));
    assert!(evaluate(&[FLEET], &f, &[], now()).is_empty());
}

#[test]
fn far_future_timestamp_does_not_mask_an_outage() {
    let mut f = Fake::default();
    f.fleet.insert("k", now() + chrono::Duration::days(1));
    assert_eq!(evaluate(&[FLEET], &f, &[], now()).len(), 1);
}

/// Finding 1: `eta.estimate` is per tracked item, so repos with nothing to
/// estimate emit nothing on a healthy fleet.
#[test]
fn idle_repo_does_not_fire_eta_estimate() {
    let row = row_for("eta.estimate");
    assert_eq!(row.scope, Scope::PerActiveRepo);
    let r = roster(30);
    let mut f = Fake::default();
    // 29 repos emit; repo29 is idle and has no estimable item.
    f.repos
        .insert("eta.estimate", r.iter().take(29).map(|n| (n.clone(), ago(5))).collect());
    f.expected
        .insert("eta.estimate", r.iter().take(29).cloned().collect());
    assert!(evaluate(&[row], &f, &r, now()).is_empty());
}

#[test]
fn all_abstaining_repo_does_not_fire_eta_estimate() {
    let row = row_for("eta.estimate");
    let r = roster(30);
    let mut f = Fake::default();
    // Only repo0 has an estimable item; the other 29 only abstain/refuse.
    f.repos
        .insert("eta.estimate", BTreeMap::from([(r[0].clone(), ago(5))]));
    f.expected.insert("eta.estimate", vec![r[0].clone()]);
    assert!(evaluate(&[row], &f, &r, now()).is_empty());
    // Fully idle fleet: nothing expected, nothing emitted.
    let mut idle = Fake::default();
    idle.expected.insert("eta.estimate", Vec::new());
    assert!(evaluate(&[row], &idle, &r, now()).is_empty());
}

#[test]
fn expected_repo_gone_silent_still_fires_eta_estimate() {
    let row = row_for("eta.estimate");
    let r = roster(30);
    let mut f = Fake::default();
    f.repos
        .insert("eta.estimate", r.iter().take(29).map(|n| (n.clone(), ago(5))).collect());
    // repo29 HAS estimable items but is silent.
    f.expected.insert("eta.estimate", r.clone());
    let c = evaluate(&[row], &f, &r, now());
    assert_eq!(c.len(), 1);
    assert!(c[0].headline.contains("29 of 30"));
}

#[test]
fn unknown_expected_set_falls_back_to_roster() {
    let row = row_for("eta.estimate");
    let r = roster(30);
    let mut f = Fake::default();
    f.repos
        .insert("eta.estimate", r.iter().take(29).map(|n| (n.clone(), ago(5))).collect());
    assert_eq!(evaluate(&[row], &f, &r, now()).len(), 1);
}

/// 10-07 with an expected set: 28 of 30 repos had live items and went silent.
#[test]
fn replay_10_07_fires_with_expected_set() {
    let row = row_for("eta.estimate");
    let r = roster(30);
    let mut f = Fake::default();
    f.repos.insert(
        "eta.estimate",
        r.iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), if i < 2 { ago(5) } else { ago(31 * 60) }))
            .collect(),
    );
    f.expected.insert("eta.estimate", r.clone());
    let c = evaluate(&[row], &f, &r, now());
    assert_eq!(c.len(), 1);
    assert!(c[0].headline.contains("2 of 30"));
}

/// Finding 2: gauges are judged against `maxAgeSecs` (1800s), not 2x the
/// 600s publish cadence.
#[test]
fn captain_gauges_tolerate_one_missed_publish() {
    for kind in [
        "captain-gauges/v1:stage-dwell",
        "captain-gauges/v1:star-facts",
        "captain-gauges/v1:queue-blocked",
    ] {
        let row = row_for(kind);
        assert_eq!(row.deadline(), Duration::from_secs(1800));
        let mut f = Fake::default();
        f.fleet
            .insert(kind, now() - chrono::Duration::seconds(1500));
        assert!(evaluate(&[row], &f, &[], now()).is_empty(), "{kind}");
        f.fleet
            .insert(kind, now() - chrono::Duration::seconds(1801));
        assert_eq!(evaluate(&[row], &f, &[], now()).len(), 1, "{kind}");
    }
}
