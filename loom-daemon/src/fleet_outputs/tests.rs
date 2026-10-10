use super::*;
use chrono::TimeZone;

#[derive(Default)]
struct Fake {
    fleet: BTreeMap<&'static str, DateTime<Utc>>,
    repos: BTreeMap<&'static str, BTreeMap<String, DateTime<Utc>>>,
    expected: BTreeMap<&'static str, Vec<String>>,
    disabled: BTreeSet<&'static str>,
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
    fn disabled(&self) -> BTreeSet<&'static str> {
        self.disabled.clone()
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
const FLEET: SingletonOutput = job("j", "k", Scope::FleetWide, mins(30), &[]);
const PER_REPO: SingletonOutput = job("j", "e", Scope::PerRepo, mins(30), &[]);

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

fn kinds(c: &[Condition]) -> Vec<&'static str> {
    c.iter().map(|c| c.record_kind).collect()
}

/// Every captain-gauges deadline is at least the bound the dispatchers use
/// for that job.
#[test]
fn captain_gauge_deadlines_cover_the_heartbeat_contract() {
    for (job, bound) in [
        (STAGE_DWELL_JOB, gauges::DEFAULT_MAX_AGE_SECS),
        (QUEUE_BLOCKED_JOB, gauges::DEFAULT_MAX_AGE_SECS),
        (STAR_FACTS_JOB, gauges::DEFAULT_STAR_FACTS_MAX_AGE_SECS),
    ] {
        let row = SINGLETON_OUTPUTS.iter().find(|o| o.job == job).unwrap();
        assert!(row.deadline().as_secs() >= bound.unsigned_abs(), "{job}");
    }
}

/// Opt-in jobs: a fleet with them off is not owed their output; the rest of
/// the registry is still judged, and a source that never read the config
/// (empty `disabled`) stays loud.
#[test]
fn disabled_opt_in_jobs_do_not_fire() {
    let r = roster(30);
    let mut f = whole_fleet_at(ago(1), &r);
    for o in SINGLETON_OUTPUTS {
        if o.record_kind.starts_with("captain-gauges/v1:") || o.record_kind == "ci.run" {
            f.fleet.remove(o.record_kind);
        }
    }
    assert_eq!(evaluate(SINGLETON_OUTPUTS, &f, &r, now()).len(), 4);
    f.disabled = BTreeSet::from([gauges::ENABLED_KEY, CI_TELEMETRY_KEY]);
    assert!(evaluate(SINGLETON_OUTPUTS, &f, &r, now()).is_empty());
    // Only the sub-switches off: stage-dwell is still owed.
    f.disabled = BTreeSet::from([
        gauges::STAR_FACTS_KEY,
        gauges::QUEUE_BLOCKED_KEY,
        CI_TELEMETRY_KEY,
    ]);
    assert_eq!(
        kinds(&evaluate(SINGLETON_OUTPUTS, &f, &r, now())),
        ["captain-gauges/v1:stage-dwell"]
    );
}
