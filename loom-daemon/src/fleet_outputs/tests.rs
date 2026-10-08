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

fn kinds(c: &[Condition]) -> Vec<&'static str> {
    c.iter().map(|c| c.record_kind).collect()
}

fn item(repo: &str, opened: DateTime<Utc>, refused: Option<bool>) -> OpenItem {
    OpenItem {
        repo: repo.to_string(),
        opened_at: opened,
        newest_refused: refused,
    }
}

fn grace() -> Duration {
    row_for("eta.estimate").deadline()
}

/// An idle repo (no open item) and an all-abstaining repo (every newest
/// record a refusal) owe nothing; one estimable item flips it back.
#[test]
fn estimate_owed_skips_idle_and_all_abstaining_repos() {
    let r = roster(3);
    let mut open = vec![
        item(&r[0], ago(600), Some(false)),
        item(&r[1], ago(60 * 48), Some(true)),
        item(&r[1], ago(60 * 48), Some(true)),
    ];
    // r[2] has no open item at all.
    assert_eq!(estimate_owed(&open, now(), grace()), [r[0].clone()]);
    open.push(item(&r[1], ago(600), Some(false)));
    assert_eq!(estimate_owed(&open, now(), grace()), [r[0].clone(), r[1].clone()]);
    assert!(estimate_owed(&[], now(), grace()).is_empty());
}

/// A just-opened item has no record yet: it counts only after one deadline.
#[test]
fn new_item_gets_a_deadline_of_grace() {
    let r = roster(1);
    let open = [item(&r[0], ago(10), None)];
    assert!(estimate_owed(&open, now(), grace()).is_empty());
    let late = now() + chrono::Duration::from_std(grace()).unwrap();
    assert_eq!(estimate_owed(&open, late, grace()), [r[0].clone()]);
}

/// End to end through `estimate_owed`: the idle and all-abstaining repos are
/// quiet, and the fleet stays quiet.
#[test]
fn idle_and_abstaining_repos_stay_quiet_end_to_end() {
    let r = roster(30);
    let mut f = whole_fleet_at(ago(1), &r);
    // repo0 idle, repo1 all-abstaining: both stale or absent, neither owes.
    let est = f.repos.get_mut("eta.estimate").unwrap();
    est.insert(r[0].clone(), ago(60 * 72));
    est.remove(&r[1]);
    let mut open: Vec<OpenItem> = r[2..]
        .iter()
        .map(|n| item(n, ago(600), Some(false)))
        .collect();
    open.push(item(&r[1], ago(60 * 48), Some(true)));
    f.expected
        .insert("eta.estimate", estimate_owed(&open, now(), grace()));
    let c = evaluate(SINGLETON_OUTPUTS, &f, &r, now());
    assert!(c.is_empty(), "{c:?}");
}

/// 10-07 through `estimate_owed`: live items in every repo, 28 gone silent.
#[test]
fn replay_10_07_fires_through_estimate_owed() {
    let row = row_for("eta.estimate");
    let r = roster(30);
    let open: Vec<OpenItem> = r
        .iter()
        .map(|n| item(n, ago(60 * 40), Some(false)))
        .collect();
    let mut f = Fake::default();
    f.expected
        .insert("eta.estimate", estimate_owed(&open, now(), grace()));
    f.repos.insert(
        "eta.estimate",
        r.iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), if i < 2 { ago(5) } else { ago(31 * 60) }))
            .collect(),
    );
    let c = evaluate(&[row], &f, &r, now());
    assert_eq!(c.len(), 1);
    assert!(c[0].headline.contains("2 of 30"), "{}", c[0].headline);
}

/// A caller that never supplied the owing set keeps main's roster fallback:
/// the 10-07 shape (2 of 30 fresh) fires at 2x cadence + 1 min.
#[test]
fn replay_10_07_without_an_owed_set_still_fires() {
    let row = row_for("eta.estimate");
    let r = roster(30);
    let stop = now();
    let mut f = Fake::default();
    let deadline = chrono::Duration::from_std(row.deadline()).unwrap();
    let late = stop + deadline + chrono::Duration::minutes(1);
    f.repos.insert(
        "eta.estimate",
        r.iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), if i < 2 { late } else { stop }))
            .collect(),
    );
    let c = evaluate(&[row], &f, &r, late);
    assert_eq!(c.len(), 1);
    assert!(c[0].headline.contains("2 of 30"), "{}", c[0].headline);
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
    // ETA off: every ETA row is skipped even with no ETA data at all.
    let off = Fake {
        disabled: BTreeSet::from([gauges::ENABLED_KEY, CI_TELEMETRY_KEY, ETA_ENABLED_KEY]),
        ..Fake::default()
    };
    assert!(evaluate(SINGLETON_OUTPUTS, &off, &r, now()).is_empty());
}

/// `eta.backtest.fold` is stamped at the folded day's cutoff (end of UTC day
/// D) and folded after `RUN_AFTER` on D+1 by an hourly check. The worst
/// healthy observed age is a day plus that run window, which must leave at
/// least half a day of slack for one late run.
#[test]
fn fold_stamp_lag_fits_the_deadline() {
    use crate::eta::nightly_folds::RUN_AFTER;
    use crate::observability::eta_nightly_folds::CHECK_INTERVAL;
    let row = row_for("eta.backtest.fold");
    let run_after =
        Duration::from_secs(u64::from(RUN_AFTER.0) * 3600 + u64::from(RUN_AFTER.1) * 60);
    let worst_healthy = hours(24) + run_after + CHECK_INTERVAL;
    assert!(row.deadline() >= worst_healthy + hours(12), "{worst_healthy:?}");
}
