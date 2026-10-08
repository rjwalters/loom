use super::*;
use chrono::TimeZone;

#[derive(Default)]
struct Fake {
    fleet: BTreeMap<&'static str, DateTime<Utc>>,
    repos: BTreeMap<&'static str, BTreeMap<String, DateTime<Utc>>>,
}
impl OutputSource for Fake {
    fn last_seen(&self, k: &str) -> Option<DateTime<Utc>> {
        self.fleet.get(k).copied()
    }
    fn last_seen_per_repo(&self, k: &str) -> BTreeMap<String, DateTime<Utc>> {
        self.repos.get(k).cloned().unwrap_or_default()
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
            Scope::PerRepo => {
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
