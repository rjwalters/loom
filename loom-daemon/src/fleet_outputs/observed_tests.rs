//! The production [`OutputSource`] (#10916 slice 3a), judged end to end
//! through [`evaluate`] against the real registry.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration as Cd, TimeZone, Utc};
use serde_json::json;

use super::observed::{self, build, parse_rows, Observed, Reading, Row};
use super::*;
use crate::observability::captain_gauges::store::{Heartbeat, JobFacts};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap()
}

fn roster(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("org/repo-{i:02}")).collect()
}

fn row(kind: &str, repo: &str, issue: u32, last: DateTime<Utc>) -> Row {
    Row {
        kind: kind.to_string(),
        repo: repo.to_string(),
        issue,
        first: last - Cd::minutes(5),
        last,
        refused: false,
        landed: false,
    }
}

fn heartbeat(as_of: DateTime<Utc>) -> Heartbeat {
    let facts: JobFacts = serde_json::from_value(json!({ "as_of": as_of })).unwrap();
    let jobs: BTreeMap<String, JobFacts> = [STAGE_DWELL_JOB, STAR_FACTS_JOB, QUEUE_BLOCKED_JOB]
        .into_iter()
        .map(|j| (j.to_string(), facts.clone()))
        .collect();
    Heartbeat::new("captain-host", as_of, jobs)
}

/// What a healthy fleet records at `at` for `repos`: every fleet-wide kind,
/// a refresh per repo, and an open item per repo with a fresh estimate.
fn healthy_rows(repos: &[String], at: DateTime<Utc>) -> Vec<Row> {
    let mut rows: Vec<Row> = ["eta.backtest.fold", "ci.run", "eta.fit"]
        .into_iter()
        .map(|k| row(k, "", 0, at))
        .collect();
    for r in repos {
        rows.push(row("eta.fleet_refresh", r, 0, at));
        rows.push(row(observed::SWEEP_STARTED, r, 7, at - Cd::hours(6)));
        rows.push(row(observed::ESTIMATE, r, 7, at));
    }
    rows
}

fn reading(rows: Result<Vec<Row>, String>, hb: Result<Option<Heartbeat>, String>) -> Reading {
    Reading {
        at: now(),
        signoz: rows,
        heartbeat: hb,
    }
}

fn judge(o: &Observed, at: DateTime<Utc>) -> Vec<Condition> {
    evaluate(SINGLETON_OUTPUTS, o, &o.roster, at)
}

fn keys(c: &[Condition]) -> BTreeSet<&str> {
    c.iter().map(|c| c.record_kind).collect()
}

#[test]
fn healthy_store_is_silent() {
    let repos = roster(30);
    let r = reading(Ok(healthy_rows(&repos, now())), Ok(Some(heartbeat(now()))));
    let o = build(&r, repos, BTreeSet::new(), now());
    assert_eq!(judge(&o, now()), Vec::new());
}

/// 2026-10-07: the producer covered 2 of 30 roster repos; the other 28 were
/// silent for ~31h (> 2x every cadence). Critical, naming the shortfall.
#[test]
fn replay_2026_10_07_two_of_thirty_repos_fires_critical() {
    let repos = roster(30);
    let stale = now() - Cd::hours(31);
    let mut rows = healthy_rows(&repos[..2], now());
    for r in &repos[2..] {
        rows.push(row("eta.fleet_refresh", r, 0, stale));
        rows.push(row(observed::SWEEP_STARTED, r, 9, stale - Cd::hours(1)));
        rows.push(row(observed::ESTIMATE, r, 9, stale));
    }
    let o = build(&reading(Ok(rows), Ok(Some(heartbeat(now())))), repos, BTreeSet::new(), now());
    let fired = judge(&o, now());
    let estimate = fired
        .iter()
        .find(|c| c.record_kind == "eta.estimate")
        .expect("eta.estimate must fire");
    assert_eq!(estimate.severity, Severity::Critical);
    assert!(estimate.headline.contains("fresh for 2 of 30 repos"), "{}", estimate.headline);
    let refresh = fired
        .iter()
        .find(|c| c.record_kind == "eta.fleet_refresh")
        .expect("eta.fleet_refresh must fire");
    assert!(refresh.headline.contains("fresh for 2 of 30 repos"), "{}", refresh.headline);
    assert_eq!(keys(&fired), BTreeSet::from(["eta.estimate", "eta.fleet_refresh"]));
}

/// The job moved at `t0` to a host that produces nothing. No host identity
/// is an input: the outputs simply stop, and every row fires by 2x cadence.
#[test]
fn job_moved_to_non_producing_host_fires_within_two_cadences() {
    let repos = roster(3);
    let t0 = now();
    let r = Reading {
        at: t0,
        signoz: Ok(healthy_rows(&repos, t0)),
        heartbeat: Ok(Some(heartbeat(t0))),
    };
    for o in SINGLETON_OUTPUTS {
        let deadline = Cd::from_std(o.deadline()).unwrap();
        let quiet_at = t0 + deadline - Cd::seconds(1);
        let late_at = t0 + deadline + Cd::seconds(1);
        let quiet = build(
            &Reading {
                at: quiet_at,
                ..r.clone()
            },
            repos.clone(),
            BTreeSet::new(),
            quiet_at,
        );
        assert!(
            !keys(&judge(&quiet, quiet_at)).contains(o.record_kind),
            "{} fired before its deadline",
            o.record_kind
        );
        let late = build(
            &Reading {
                at: late_at,
                ..r.clone()
            },
            repos.clone(),
            BTreeSet::new(),
            late_at,
        );
        assert!(
            keys(&judge(&late, late_at)).contains(o.record_kind),
            "{} silent past its deadline",
            o.record_kind
        );
    }
}

#[test]
fn store_read_error_fires_every_row_it_backs() {
    let repos = roster(4);
    let r = reading(Err("HTTP 503".into()), Err("forge 502".into()));
    let o = build(&r, repos, BTreeSet::new(), now());
    let fired = judge(&o, now());
    let all: BTreeSet<&str> = SINGLETON_OUTPUTS.iter().map(|o| o.record_kind).collect();
    assert_eq!(keys(&fired), all);
    assert!(fired
        .iter()
        .filter(|c| !observed::is_gauge_kind(c.record_kind))
        .all(|c| c.headline.contains("unreadable") && c.headline.contains("HTTP 503")));
    assert!(fired
        .iter()
        .filter(|c| observed::is_gauge_kind(c.record_kind))
        .all(|c| c.headline.contains("forge 502")));
}

#[test]
fn heartbeat_error_fires_only_gauge_rows() {
    let repos = roster(2);
    let r = reading(Ok(healthy_rows(&repos, now())), Err("forge 502".into()));
    let o = build(&r, repos, BTreeSet::new(), now());
    let fired = judge(&o, now());
    assert!(!fired.is_empty());
    assert!(fired.iter().all(|c| observed::is_gauge_kind(c.record_kind)));
}

#[test]
fn absent_heartbeat_is_never_observed() {
    let repos = roster(2);
    let r = reading(Ok(healthy_rows(&repos, now())), Ok(None));
    let fired = judge(&build(&r, repos, BTreeSet::new(), now()), now());
    assert_eq!(fired.len(), 3);
    assert!(fired.iter().all(|c| c.headline.contains("never observed")));
}

#[test]
fn disabled_toggles_skip_their_rows() {
    let r = reading(Err("down".into()), Err("down".into()));
    let off = BTreeSet::from([ETA_ENABLED_KEY, CI_TELEMETRY_KEY, gauges::ENABLED_KEY]);
    assert!(judge(&build(&r, roster(2), off, now()), now()).is_empty());
}

#[test]
fn expected_estimate_repos_come_from_open_items_not_estimates() {
    let repos = roster(4);
    // repo-00: open, estimated. repo-01: open, never estimated (owes, past
    // grace). repo-02: landed after it started (owes nothing). repo-03:
    // newest estimate is a refusal (owes nothing). Outside the roster: ignored.
    let rows = vec![
        row(observed::SWEEP_STARTED, &repos[0], 1, now() - Cd::hours(2)),
        row(observed::ESTIMATE, &repos[0], 1, now()),
        row(observed::SWEEP_STARTED, &repos[1], 2, now() - Cd::hours(2)),
        row(observed::SWEEP_STARTED, &repos[2], 3, now() - Cd::hours(5)),
        Row {
            landed: true,
            ..row(observed::OUTCOME, &repos[2], 3, now() - Cd::hours(1))
        },
        row(observed::SWEEP_STARTED, &repos[3], 4, now() - Cd::hours(2)),
        Row {
            refused: true,
            ..row(observed::ESTIMATE, &repos[3], 4, now() - Cd::hours(2))
        },
        row(observed::SWEEP_STARTED, "other/outside", 5, now() - Cd::hours(2)),
    ];
    let o = build(&reading(Ok(rows), Ok(None)), repos.clone(), BTreeSet::new(), now());
    assert_eq!(
        o.expected_repos(observed::ESTIMATE),
        Some(vec![repos[0].clone(), repos[1].clone()])
    );
}

#[test]
fn outputs_sql_names_every_log_kind() {
    for o in SINGLETON_OUTPUTS
        .iter()
        .filter(|o| !observed::is_gauge_kind(o.record_kind))
    {
        assert!(
            observed::OUTPUTS_SQL.contains(&format!("'{}'", o.record_kind)),
            "{}",
            o.record_kind
        );
    }
    for k in [observed::SWEEP_STARTED, observed::OUTCOME] {
        assert!(observed::OUTPUTS_SQL.contains(&format!("'{k}'")), "{k}");
    }
    // Every gauge row is a heartbeat job.
    for o in SINGLETON_OUTPUTS
        .iter()
        .filter(|o| observed::is_gauge_kind(o.record_kind))
    {
        assert!(o.record_kind.starts_with("captain-gauges/v1:"));
    }
}

#[test]
fn parse_rows_reads_number_and_string_columns() {
    let ns = now().timestamp_nanos_opt().unwrap();
    let body = format!(
        "{{\"kind\":\"eta.estimate\",\"repo\":\"Org/Repo\",\"issue\":12,\"first_ns\":\"{ns}\",\"last_ns\":\"{ns}\",\"refused\":1,\"landed\":0}}\n\n"
    );
    let rows = parse_rows(&body).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].repo, "org/repo");
    assert_eq!(rows[0].issue, 12);
    assert_eq!(rows[0].last, now());
    assert!(rows[0].refused);
}

/// A row with no readable observation timestamp fails the whole read (absent
/// data fires), never a silent skip.
#[test]
fn parse_rows_fails_loud_on_a_row_without_a_timestamp() {
    let ns = now().timestamp_nanos_opt().unwrap();
    let good = format!("{{\"kind\":\"ci.run\",\"repo\":\"\",\"issue\":0,\"first_ns\":\"{ns}\",\"last_ns\":\"{ns}\"}}");
    let bad = "{\"kind\":\"eta.fit\",\"repo\":\"\",\"issue\":0,\"first_ns\":\"1\"}";
    let err = parse_rows(&format!("{good}\n{bad}\n")).unwrap_err();
    assert!(err.contains("line 2") && err.contains("last_ns"), "{err}");
    assert!(parse_rows("not json").is_err());
    let o = build(
        &reading(parse_rows(bad), Ok(Some(heartbeat(now())))),
        roster(2),
        BTreeSet::new(),
        now(),
    );
    assert!(keys(&judge(&o, now())).contains("eta.fit"));
}

#[test]
fn disabled_from_reads_the_real_toggles() {
    let none = |_: &str| None;
    let all_on = json!({
        "autonomous": {"eta": {"enabled": true}},
        "fleet": {"captainGauges": {"enabled": true, "starFacts": true, "queueBlocked": true}}
    });
    assert!(observed::disabled_from(&all_on, &none, true).is_empty());
    let off =
        json!({"autonomous": {"eta": {"enabled": false, "nightlyFolds": {"enabled": false}}}});
    let d = observed::disabled_from(&off, &none, false);
    for k in [
        ETA_ENABLED_KEY,
        ETA_NIGHTLY_FOLDS_KEY,
        CI_TELEMETRY_KEY,
        gauges::ENABLED_KEY,
    ] {
        assert!(d.contains(k), "{k} not off in {d:?}");
    }
    // Every reported key gates some registry row.
    let gating: BTreeSet<&str> = SINGLETON_OUTPUTS
        .iter()
        .flat_map(|o| o.enabled_by.iter().copied())
        .collect();
    assert!(d.iter().all(|k| gating.contains(k)), "{d:?}");
}
