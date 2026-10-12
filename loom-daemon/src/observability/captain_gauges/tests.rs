//! Captain-produced fleet gauges (W12): config, role, freshness, gauges, and
//! the heartbeat's store round trip against an in-memory contents API.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};

use super::store::{self, FetchCache, Fetched, Heartbeat, JobFacts, PublishCache};
use super::{
    content_key, coverage, effective_publish_interval, fresh_job, publish_due, resolve_role,
    Config, Role, DEFAULT_MAX_AGE_SECS, DEFAULT_PUBLISH_INTERVAL_SECS,
    DEFAULT_STAR_FACTS_MAX_AGE_SECS, QUEUE_BLOCKED_JOB, STAGE_DWELL_JOB, STAND_DOWN_ENV,
    STAR_FACTS_JOB,
};
use crate::fleet_captain::CaptainGate;
use crate::fleet_store::fetch::{Reply, Transport};
use crate::fleet_store::propose::WriteTransport;
use crate::fleet_store::StoreLocation;
use crate::telemetry::ops::{MetricName, MetricPoint};

const CAPTAIN: &str = "captain-host";

fn at(mins: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap() + Duration::minutes(mins)
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// Every key at its default (`maxAgeSecs` 30 min, `starFactsMaxAgeSecs` 15).
fn defaults() -> Config {
    Config::from_effective(&json!({}), &no_env)
}

fn config(block: Value) -> Config {
    Config::from_effective(&json!({ "fleet": { "captainGauges": block } }), &no_env)
}

fn facts(as_of: DateTime<Utc>, repos: &[&str]) -> JobFacts {
    JobFacts::covering(as_of, repos.iter().map(|r| (*r).to_string()).collect())
}

/// The part 1 gauges: one job.
fn points(
    role: &Role,
    produced: &BTreeMap<String, JobFacts>,
    heartbeat: Option<&Heartbeat>,
    covered: &super::Coverage,
    now: DateTime<Utc>,
) -> Vec<MetricPoint> {
    super::points(&[STAGE_DWELL_JOB], role, produced, heartbeat, covered, now)
}

fn heartbeat(captain: &str, as_of: DateTime<Utc>, repos: &[&str]) -> Heartbeat {
    Heartbeat::new(captain, as_of, [(STAGE_DWELL_JOB.to_string(), facts(as_of, repos))].into())
}

// ---- config + role ---------------------------------------------------------

#[test]
fn an_unconfigured_host_keeps_todays_behaviour() {
    let cfg = Config::from_effective(&json!({}), &no_env);
    assert!(!cfg.enabled && !cfg.stand_down);
    assert_eq!(cfg.max_age, Duration::seconds(DEFAULT_MAX_AGE_SECS));
    assert_eq!(cfg.publish_interval, Duration::seconds(DEFAULT_PUBLISH_INTERVAL_SECS));
    // Whatever the captain gate says, nothing changes without the switches.
    for gate in [
        CaptainGate::Armed {
            captain: CAPTAIN.into(),
        },
        CaptainGate::Refused {
            captain: CAPTAIN.into(),
            current_host_id: "dispatcher".into(),
        },
        CaptainGate::NoCaptainDeclared,
    ] {
        assert!(matches!(resolve_role(&gate, &cfg, true), Role::Local { .. }), "{gate:?}");
    }
}

#[test]
fn config_reads_every_key_and_ignores_bad_values() {
    let cfg = config(json!({
        "enabled": true, "standDown": true, "maxAgeSecs": 900, "publishIntervalSecs": 300
    }));
    assert!(cfg.enabled && cfg.stand_down);
    assert_eq!(cfg.max_age, Duration::seconds(900));
    assert_eq!(cfg.publish_interval, Duration::seconds(300));
    let bad = config(json!({"enabled": "yes", "maxAgeSecs": 0, "publishIntervalSecs": -5}));
    assert!(!bad.enabled);
    assert_eq!(bad.max_age, Duration::seconds(DEFAULT_MAX_AGE_SECS));
    assert_eq!(bad.publish_interval, Duration::seconds(DEFAULT_PUBLISH_INTERVAL_SECS));
}

#[test]
fn the_env_switch_overrides_stand_down_both_ways() {
    let effective = json!({"fleet": {"captainGauges": {"standDown": true}}});
    let off = |k: &str| (k == STAND_DOWN_ENV).then(|| "0".to_string());
    assert!(!Config::from_effective(&effective, &off).stand_down);
    let on = |k: &str| (k == STAND_DOWN_ENV).then(|| "true".to_string());
    assert!(Config::from_effective(&json!({}), &on).stand_down);
    let junk = |k: &str| (k == STAND_DOWN_ENV).then(|| "maybe".to_string());
    assert!(Config::from_effective(&effective, &junk).stand_down);
}

#[test]
fn role_follows_the_gate_and_the_switches() {
    let both = config(json!({"enabled": true, "standDown": true}));
    let armed = CaptainGate::Armed {
        captain: CAPTAIN.into(),
    };
    let refused = CaptainGate::Refused {
        captain: CAPTAIN.into(),
        current_host_id: "dispatcher".into(),
    };
    assert_eq!(
        resolve_role(&armed, &both, false),
        Role::Captain {
            captain: CAPTAIN.into()
        },
        "the captain produces even with no store; it just cannot publish"
    );
    assert_eq!(
        resolve_role(&refused, &both, true),
        Role::Dispatcher {
            captain: CAPTAIN.into()
        }
    );
    assert_eq!(resolve_role(&refused, &both, false), Role::Local { reason: "no_store" });
    // No captain declared is fail-open: produce locally everywhere.
    assert_eq!(
        resolve_role(&CaptainGate::NoCaptainDeclared, &both, true),
        Role::Local {
            reason: "no_captain"
        }
    );
    // Captain arming and dispatcher stand-down are independent switches.
    let only_stand_down = config(json!({"standDown": true}));
    assert!(matches!(resolve_role(&armed, &only_stand_down, true), Role::Local { .. }));
    let only_enabled = config(json!({"enabled": true}));
    assert!(matches!(resolve_role(&refused, &only_enabled, true), Role::Local { .. }));
}

// ---- freshness + coverage --------------------------------------------------

#[test]
fn a_dispatcher_stands_down_only_on_fresh_data_from_the_declared_captain() {
    let max_age = Duration::seconds(DEFAULT_MAX_AGE_SECS);
    let hb = heartbeat(CAPTAIN, at(0), &["acme/app"]);
    // Fresh, up to and including the bound.
    assert!(hb.fresh(STAGE_DWELL_JOB, CAPTAIN, at(0), max_age).is_some());
    assert!(hb
        .fresh(STAGE_DWELL_JOB, CAPTAIN, at(30), max_age)
        .is_some());
    // Stale one second later: the captain is down, the dispatcher takes over.
    let stale = at(30) + Duration::seconds(1);
    assert!(hb.fresh(STAGE_DWELL_JOB, CAPTAIN, stale, max_age).is_none());
    // Published by a former captain: never trusted.
    assert!(hb
        .fresh(STAGE_DWELL_JOB, "other-host", at(1), max_age)
        .is_none());
    // An as_of from the future beyond the skew is refused.
    assert!(hb
        .fresh(STAGE_DWELL_JOB, CAPTAIN, at(-6), max_age)
        .is_none());
    assert!(hb
        .fresh(STAGE_DWELL_JOB, CAPTAIN, at(-4), max_age)
        .is_some());
    // A job the captain does not produce is never covered.
    assert!(hb.fresh("other-job", CAPTAIN, at(1), max_age).is_none());
}

#[test]
fn coverage_is_per_job_and_per_repo() {
    let hb = heartbeat(CAPTAIN, at(0), &["acme/app", "acme/lib"]);
    let covered = coverage(Some(&hb), CAPTAIN, at(10), &defaults());
    let repos: BTreeSet<String> = ["acme/app".to_string(), "acme/lib".to_string()].into();
    assert_eq!(covered.get(STAGE_DWELL_JOB), Some(&repos));
    assert!(coverage(Some(&hb), CAPTAIN, at(31), &defaults()).is_empty());
    assert!(coverage(None, CAPTAIN, at(1), &defaults()).is_empty());
}

// ---- gauges ----------------------------------------------------------------

fn point(name: MetricName, value: i64) -> MetricPoint {
    MetricPoint::int(name, value).label("task", STAGE_DWELL_JOB)
}

#[test]
fn a_dispatcher_reports_the_captains_age_and_whether_it_fell_back() {
    let role = Role::Dispatcher {
        captain: CAPTAIN.into(),
    };
    let hb = heartbeat(CAPTAIN, at(0), &["acme/app"]);
    let fresh = coverage(Some(&hb), CAPTAIN, at(10), &defaults());
    let out = points(&role, &BTreeMap::new(), Some(&hb), &fresh, at(10));
    assert_eq!(
        out,
        vec![
            point(MetricName::CaptainGaugeAgeSeconds, 600),
            point(MetricName::CaptainGaugeFallback, 0)
        ]
    );
    // Captain down: the age keeps growing and the fallback flag goes up.
    let stale = coverage(Some(&hb), CAPTAIN, at(45), &defaults());
    let out = points(&role, &BTreeMap::new(), Some(&hb), &stale, at(45));
    assert_eq!(
        out,
        vec![
            point(MetricName::CaptainGaugeAgeSeconds, 2700),
            point(MetricName::CaptainGaugeFallback, 1)
        ]
    );
    // Nothing published yet: no age point (unknown is not zero), fallback 1.
    let out = points(&role, &BTreeMap::new(), None, &BTreeMap::new(), at(1));
    assert_eq!(out, vec![point(MetricName::CaptainGaugeFallback, 1)]);
}

#[test]
fn the_captain_reports_its_own_age_and_a_local_host_reports_nothing() {
    let captain = Role::Captain {
        captain: CAPTAIN.into(),
    };
    let produced = [(STAGE_DWELL_JOB.to_string(), facts(at(0), &["acme/app"]))].into();
    let out = points(&captain, &produced, None, &BTreeMap::new(), at(5));
    assert_eq!(out, vec![point(MetricName::CaptainGaugeAgeSeconds, 300)]);
    assert!(points(&captain, &BTreeMap::new(), None, &BTreeMap::new(), at(5)).is_empty());
    let local = Role::Local {
        reason: "stand_down_off",
    };
    assert!(points(&local, &produced, None, &BTreeMap::new(), at(5)).is_empty());
}

// ---- store -----------------------------------------------------------------

fn loc(reference: &str) -> StoreLocation {
    StoreLocation {
        repo: "o/store".into(),
        reference: reference.into(),
    }
}

fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        etag: None,
        body: body.into(),
    }
}

/// An in-memory contents API for one file on one branch.
#[derive(Default)]
struct Store {
    file: RefCell<Option<(String, u32)>>,
    branch: RefCell<bool>,
    down: RefCell<bool>,
    /// Make the next PUT answer 409 (a stale sha) once.
    conflict: RefCell<bool>,
    gets: RefCell<Vec<String>>,
    writes: RefCell<Vec<(String, String, Value)>>,
}

impl Transport for Store {
    fn get(
        &self,
        api_path: &str,
        accept: Option<&str>,
        etag: Option<&str>,
    ) -> anyhow::Result<Reply> {
        self.gets.borrow_mut().push(api_path.to_string());
        if *self.down.borrow() {
            anyhow::bail!("network down");
        }
        if api_path.contains("/git/ref/heads/") {
            return Ok(reply(if *self.branch.borrow() { 200 } else { 404 }, ""));
        }
        if api_path.contains("/commits/") {
            return Ok(reply(200, "a".repeat(40)));
        }
        assert!(api_path.contains(store::HEARTBEAT_PATH), "{api_path}");
        let file = self.file.borrow();
        let Some((body, version)) = file.as_ref() else {
            return Ok(reply(404, ""));
        };
        let tag = format!("\"v{version}\"");
        if etag == Some(tag.as_str()) {
            return Ok(reply(304, ""));
        }
        if accept.is_some_and(|a| a.contains("raw")) {
            return Ok(Reply {
                status: 200,
                etag: Some(tag),
                body: body.clone(),
            });
        }
        Ok(reply(200, json!({"sha": format!("sha{version}")}).to_string()))
    }
}

impl WriteTransport for Store {
    fn write(&self, method: &str, api_path: &str, body: &Value) -> anyhow::Result<Reply> {
        self.writes
            .borrow_mut()
            .push((method.to_string(), api_path.to_string(), body.clone()));
        if api_path.ends_with("/git/refs") {
            *self.branch.borrow_mut() = true;
            return Ok(reply(201, "{}"));
        }
        let mut file = self.file.borrow_mut();
        let version = file.as_ref().map_or(0, |(_, v)| *v);
        let expected = (version > 0).then(|| format!("sha{version}"));
        let sent = body.get("sha").and_then(Value::as_str).map(str::to_string);
        if std::mem::take(&mut *self.conflict.borrow_mut()) || sent != expected {
            return Ok(reply(409, "{}"));
        }
        let content = body["content"].as_str().unwrap();
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content)
            .unwrap();
        let next = version + 1;
        *file = Some((String::from_utf8(bytes).unwrap(), next));
        Ok(reply(200, json!({"content": {"sha": format!("sha{next}")}}).to_string()))
    }
}

fn puts(s: &Store) -> usize {
    s.writes
        .borrow()
        .iter()
        .filter(|(m, ..)| m == "PUT")
        .count()
}

#[test]
fn the_heartbeat_round_trips_and_reuses_the_blob_sha() {
    let s = Store::default();
    let mut cache = PublishCache::default();
    let hb = heartbeat(CAPTAIN, at(0), &["acme/app"]);
    store::publish(&s, &s, &loc("eta-fit"), "main", &hb, &mut cache).unwrap();
    assert!(*s.branch.borrow(), "the publication branch is created from the base");
    let hb2 = heartbeat(CAPTAIN, at(10), &["acme/app"]);
    let gets_before = s.gets.borrow().len();
    store::publish(&s, &s, &loc("eta-fit"), "main", &hb2, &mut cache).unwrap();
    assert_eq!(s.gets.borrow().len(), gets_before, "the second publish makes no read at all");
    assert_eq!(puts(&s), 2);

    let mut fetch = FetchCache::default();
    assert_eq!(store::fetch(&s, &loc("eta-fit"), &mut fetch), Fetched::Updated);
    assert_eq!(fetch.heartbeat.as_ref(), Some(&hb2));
    // Unchanged: a 304, and the cached heartbeat is still served.
    assert_eq!(store::fetch(&s, &loc("eta-fit"), &mut fetch), Fetched::NotModified);
    assert_eq!(fetch.heartbeat.as_ref(), Some(&hb2));
}

#[test]
fn a_stale_sha_is_reread_once() {
    let s = Store::default();
    *s.branch.borrow_mut() = true;
    let mut cache = PublishCache::default();
    store::publish(&s, &s, &loc("eta-fit"), "main", &heartbeat(CAPTAIN, at(0), &[]), &mut cache)
        .unwrap();
    *s.conflict.borrow_mut() = true;
    store::publish(&s, &s, &loc("eta-fit"), "main", &heartbeat(CAPTAIN, at(1), &[]), &mut cache)
        .unwrap();
    assert_eq!(puts(&s), 3, "one conflict, one retry");
}

#[test]
fn the_heartbeat_never_lands_on_the_reviewed_branch_or_main() {
    let s = Store::default();
    let hb = heartbeat(CAPTAIN, at(0), &[]);
    for (reference, base) in [
        ("main", "main"),
        ("stable", "stable"),
        ("Main", "x"),
        ("refs/heads/g", "main"),
    ] {
        let err = store::publish(&s, &s, &loc(reference), base, &hb, &mut PublishCache::default());
        assert!(err.is_err(), "{reference} vs {base}");
    }
    assert!(s.writes.borrow().is_empty());
}

#[test]
fn a_read_failure_keeps_the_last_heartbeat_ageing_and_a_404_clears_it() {
    let s = Store::default();
    let hb = heartbeat(CAPTAIN, at(0), &["acme/app"]);
    store::publish(&s, &s, &loc("eta-fit"), "main", &hb, &mut PublishCache::default()).unwrap();
    let mut fetch = FetchCache::default();
    store::fetch(&s, &loc("eta-fit"), &mut fetch);
    *s.down.borrow_mut() = true;
    assert!(matches!(store::fetch(&s, &loc("eta-fit"), &mut fetch), Fetched::Failed(_)));
    // Still standing down while the captain's last known output is fresh ...
    assert!(!coverage(fetch.heartbeat.as_ref(), CAPTAIN, at(20), &defaults()).is_empty());
    // ... and back to local production once it is stale: no gap either way.
    assert!(coverage(fetch.heartbeat.as_ref(), CAPTAIN, at(31), &defaults()).is_empty());

    *s.down.borrow_mut() = false;
    *s.file.borrow_mut() = None;
    assert_eq!(store::fetch(&s, &loc("eta-fit"), &mut fetch), Fetched::Absent);
    assert!(fetch.heartbeat.is_none());
}

#[test]
fn a_malformed_heartbeat_is_never_trusted() {
    let s = Store::default();
    *s.file.borrow_mut() = Some((r#"{"schema": "captain-gauges/v0"}"#.to_string(), 1));
    let mut fetch = FetchCache::default();
    assert!(matches!(store::fetch(&s, &loc("eta-fit"), &mut fetch), Fetched::Failed(_)));
    assert!(fetch.heartbeat.is_none());
}

#[test]
fn the_publication_branch_defaults_to_the_eta_fit_branch() {
    assert_eq!(store::resolve_ref(&json!({})), "eta-fit");
    assert_eq!(store::resolve_ref(&json!({"fleet": {"etaFitRef": "pub"}})), "pub");
    let own = json!({"fleet": {"etaFitRef": "pub", "captainGauges": {"ref": "gauges"}}});
    assert_eq!(store::resolve_ref(&own), "gauges");
    // No fleet store: the feature is off.
    assert!(store::location_for(&json!({}), &no_env).is_none());
    let configured = json!({"fleet": {"repo": "o/store"}});
    let (location, base) = store::location_for(&configured, &no_env).unwrap();
    assert_eq!((location.repo.as_str(), location.reference.as_str()), ("o/store", "eta-fit"));
    assert_eq!(base, "main");
}

// ---- part 2: the fact jobs -------------------------------------------------

fn dispatcher() -> Role {
    Role::Dispatcher {
        captain: CAPTAIN.into(),
    }
}

#[test]
fn the_part_two_jobs_are_off_until_their_own_switches() {
    // A part 1 fleet (`enabled` / `standDown` only) runs one job.
    let part_one = config(json!({"enabled": true, "standDown": true}));
    assert!(!part_one.star_facts && !part_one.queue_blocked);
    assert_eq!(part_one.jobs(), [STAGE_DWELL_JOB]);
    let both = config(json!({"standDown": true, "starFacts": true, "queueBlocked": true}));
    assert_eq!(both.jobs(), [STAGE_DWELL_JOB, STAR_FACTS_JOB, QUEUE_BLOCKED_JOB]);
    let one = config(json!({"queueBlocked": true, "starFacts": "yes"}));
    assert_eq!(one.jobs(), [STAGE_DWELL_JOB, QUEUE_BLOCKED_JOB]);
}

#[test]
fn a_job_is_relied_on_only_by_a_dispatcher_with_its_switch_and_fresh_data() {
    let on = config(json!({"standDown": true, "starFacts": true}));
    let off = config(json!({"standDown": true}));
    let hb = Heartbeat::new(
        CAPTAIN,
        at(0),
        [(STAR_FACTS_JOB.to_string(), facts(at(0), &["acme/app"]))].into(),
    );
    let role = dispatcher();
    let ask = |role: Option<&Role>, cfg: Option<&Config>, hb: Option<&Heartbeat>, now| {
        fresh_job(role, cfg, hb, STAR_FACTS_JOB, now).is_some()
    };
    assert!(ask(Some(&role), Some(&on), Some(&hb), at(10)));
    // Judged at the time of use: the same heartbeat is stale later, with no
    // collector pass in between.
    assert!(!ask(Some(&role), Some(&on), Some(&hb), at(31)));
    // The switch is off on this host, or the pass has not resolved yet.
    assert!(!ask(Some(&role), Some(&off), Some(&hb), at(10)));
    assert!(!ask(None, Some(&on), Some(&hb), at(10)));
    assert!(!ask(Some(&role), None, Some(&hb), at(10)));
    assert!(!ask(Some(&role), Some(&on), None, at(10)));
    // The captain itself, and a local host, never read their own heartbeat.
    let captain = Role::Captain {
        captain: CAPTAIN.into(),
    };
    assert!(!ask(Some(&captain), Some(&on), Some(&hb), at(10)));
    let local = Role::Local { reason: "no_store" };
    assert!(!ask(Some(&local), Some(&on), Some(&hb), at(10)));
    // Published by a former captain.
    let other = Role::Dispatcher {
        captain: "new-captain".into(),
    };
    assert!(!ask(Some(&other), Some(&on), Some(&hb), at(10)));
    // A job the heartbeat does not carry (an older captain).
    assert!(fresh_job(Some(&role), Some(&on), Some(&hb), QUEUE_BLOCKED_JOB, at(10)).is_none());
}

#[test]
fn a_changed_fact_is_published_at_once_and_an_unchanged_one_waits() {
    let interval = Duration::seconds(DEFAULT_PUBLISH_INTERVAL_SECS);
    let quiet: BTreeMap<String, JobFacts> =
        [(STAR_FACTS_JOB.to_string(), facts(at(0), &["acme/app"]))].into();
    let key = content_key(&quiet);
    assert!(publish_due(None, &key, at(0), interval), "nothing published yet");
    // The same facts five minutes later differ only in `as_of`: not due.
    let later: BTreeMap<String, JobFacts> =
        [(STAR_FACTS_JOB.to_string(), facts(at(5), &["acme/app"]))].into();
    assert_eq!(content_key(&later), key);
    assert!(!publish_due(Some((at(0), &key)), &content_key(&later), at(5), interval));
    assert!(publish_due(Some((at(0), &key)), &key, at(10), interval), "the interval elapsed");
    // A star appears, or a repo drops out of coverage: due now.
    let mut starred = later.clone();
    starred
        .get_mut(STAR_FACTS_JOB)
        .unwrap()
        .counts
        .insert("acme/app".into(), 1);
    assert!(publish_due(Some((at(0), &key)), &content_key(&starred), at(5), interval));
    let dropped: BTreeMap<String, JobFacts> =
        [(STAR_FACTS_JOB.to_string(), facts(at(5), &[]))].into();
    assert!(publish_due(Some((at(0), &key)), &content_key(&dropped), at(5), interval));
}

#[test]
fn the_fact_fields_round_trip_and_stay_out_of_a_part_one_heartbeat() {
    let s = Store::default();
    let part_one = heartbeat(CAPTAIN, at(0), &["acme/app"]);
    store::publish(&s, &s, &loc("eta-fit"), "main", &part_one, &mut PublishCache::default())
        .unwrap();
    let body = s.file.borrow().as_ref().unwrap().0.clone();
    for field in ["labels", "counts", "blocked"] {
        assert!(!body.contains(field), "{field} is omitted when empty: {body}");
    }

    let mut star = facts(at(1), &["acme/app", "acme/lib"]);
    star.labels = ["loom:operator-priority".to_string()].into();
    star.counts.insert("acme/lib".into(), 2);
    let mut blocked = facts(at(1), &["acme/app"]);
    blocked.blocked.insert(
        "acme/app".into(),
        vec![store::BlockedFact {
            number: 12,
            created_at: Some("2026-09-01T00:00:00Z".into()),
            labels: vec!["loom:blocked".into(), "tier:2".into()],
        }],
    );
    let hb = Heartbeat::new(
        CAPTAIN,
        at(1),
        [
            (STAR_FACTS_JOB.to_string(), star),
            (QUEUE_BLOCKED_JOB.to_string(), blocked),
        ]
        .into(),
    );
    let mut cache = PublishCache::default();
    store::publish(&s, &s, &loc("eta-fit"), "main", &hb, &mut cache).unwrap();
    let mut fetch = FetchCache::default();
    assert_eq!(store::fetch(&s, &loc("eta-fit"), &mut fetch), Fetched::Updated);
    assert_eq!(fetch.heartbeat.as_ref(), Some(&hb));
}

#[test]
fn a_reader_that_predates_the_fact_fields_still_reads_the_heartbeat() {
    // The part 1 reader's view of a job: `as_of` and `repos`, nothing else.
    #[derive(serde::Deserialize)]
    struct OldJob {
        as_of: DateTime<Utc>,
        #[serde(default)]
        repos: BTreeSet<String>,
    }
    #[derive(serde::Deserialize)]
    struct OldHeartbeat {
        schema: String,
        jobs: BTreeMap<String, OldJob>,
    }
    let mut star = facts(at(1), &["acme/app"]);
    star.counts.insert("acme/app".into(), 1);
    let hb = Heartbeat::new(
        CAPTAIN,
        at(1),
        [
            (STAGE_DWELL_JOB.to_string(), facts(at(1), &["acme/app"])),
            (STAR_FACTS_JOB.to_string(), star),
        ]
        .into(),
    );
    let old: OldHeartbeat = serde_json::from_str(&serde_json::to_string(&hb).unwrap()).unwrap();
    assert_eq!(old.schema, store::SCHEMA);
    assert_eq!(old.jobs[STAGE_DWELL_JOB].as_of, at(1));
    assert!(old.jobs[STAGE_DWELL_JOB].repos.contains("acme/app"));
    // And this reader takes a part 1 heartbeat (no fact fields at all).
    let part_one = format!(
        r#"{{"schema":"{}","captain_host":"{CAPTAIN}","published_at":"2026-10-06T12:00:00Z",
            "jobs":{{"stage-dwell":{{"as_of":"2026-10-06T12:00:00Z","repos":["acme/app"]}}}}}}"#,
        store::SCHEMA
    );
    let parsed = store::parse(part_one.as_bytes()).unwrap();
    assert!(parsed.jobs[STAGE_DWELL_JOB].counts.is_empty());
    assert!(!parsed.jobs.contains_key(STAR_FACTS_JOB), "so every dispatcher stays local");
}

#[test]
fn a_dispatcher_reports_fallback_per_job_it_takes_part_in() {
    let hb = Heartbeat::new(
        CAPTAIN,
        at(0),
        [(STAGE_DWELL_JOB.to_string(), facts(at(0), &["acme/app"]))].into(),
    );
    let covered = coverage(Some(&hb), CAPTAIN, at(10), &defaults());
    let jobs = [STAGE_DWELL_JOB, STAR_FACTS_JOB];
    let out = super::points(&jobs, &dispatcher(), &BTreeMap::new(), Some(&hb), &covered, at(10));
    let star_fallback =
        MetricPoint::int(MetricName::CaptainGaugeFallback, 1).label("task", STAR_FACTS_JOB);
    assert!(
        out.contains(&star_fallback),
        "an older captain publishes no star-facts: {out:?}"
    );
    assert!(out.contains(&point(MetricName::CaptainGaugeFallback, 0)));
    assert!(!out.iter().any(|p| *p
        == MetricPoint::int(MetricName::CaptainGaugeFallback, 1).label("task", QUEUE_BLOCKED_JOB)));
}

// ---- the star-facts bound (S1) ---------------------------------------------

#[test]
fn star_facts_have_their_own_tighter_bound() {
    let cfg = defaults();
    assert_eq!(cfg.star_facts_max_age, Duration::seconds(DEFAULT_STAR_FACTS_MAX_AGE_SECS));
    assert!(cfg.star_facts_max_age < cfg.max_age);
    assert_eq!(cfg.max_age_for(STAR_FACTS_JOB), Duration::minutes(15));
    assert_eq!(cfg.max_age_for(QUEUE_BLOCKED_JOB), cfg.max_age);
    assert_eq!(cfg.max_age_for(STAGE_DWELL_JOB), cfg.max_age);
    let set = config(json!({"starFactsMaxAgeSecs": 600}));
    assert_eq!(set.max_age_for(STAR_FACTS_JOB), Duration::seconds(600));
    assert_eq!(set.max_age, Duration::seconds(DEFAULT_MAX_AGE_SECS), "maxAgeSecs is untouched");
    let bad = config(json!({"starFactsMaxAgeSecs": 0}));
    assert_eq!(bad.star_facts_max_age, Duration::seconds(DEFAULT_STAR_FACTS_MAX_AGE_SECS));
}

#[test]
fn star_facts_past_their_bound_are_not_trusted_even_while_the_gauges_are() {
    // The captain published all three jobs at 0 and then stopped, and this
    // dispatcher's later heartbeat reads fail: the cached copy keeps ageing.
    let s = Store::default();
    let jobs = [STAGE_DWELL_JOB, STAR_FACTS_JOB, QUEUE_BLOCKED_JOB]
        .map(|job| (job.to_string(), facts(at(0), &["acme/app"])));
    let hb = Heartbeat::new(CAPTAIN, at(0), jobs.into());
    store::publish(&s, &s, &loc("eta-fit"), "main", &hb, &mut PublishCache::default()).unwrap();
    let mut fetch = FetchCache::default();
    store::fetch(&s, &loc("eta-fit"), &mut fetch);
    *s.down.borrow_mut() = true;
    assert!(matches!(store::fetch(&s, &loc("eta-fit"), &mut fetch), Fetched::Failed(_)));

    let cfg = config(json!({"standDown": true, "starFacts": true, "queueBlocked": true}));
    let role = dispatcher();
    let ask = |job: &str, now| {
        fresh_job(Some(&role), Some(&cfg), fetch.heartbeat.as_ref(), job, now).is_some()
    };
    assert!(ask(STAR_FACTS_JOB, at(15)), "inside the star bound");
    // Older than the star bound, younger than `maxAgeSecs`.
    for now in [at(15) + Duration::seconds(1), at(16), at(29)] {
        assert!(!ask(STAR_FACTS_JOB, now), "{now}: the dispatcher lists itself");
        assert!(ask(QUEUE_BLOCKED_JOB, now), "{now}: the other jobs keep maxAgeSecs");
        let covered = coverage(fetch.heartbeat.as_ref(), CAPTAIN, now, &cfg);
        assert!(!covered.contains_key(STAR_FACTS_JOB), "and reports the fallback");
        assert!(covered.contains_key(STAGE_DWELL_JOB) && covered.contains_key(QUEUE_BLOCKED_JOB));
    }
}

#[test]
fn a_captain_producing_star_facts_republishes_inside_their_bound() {
    let cfg = defaults();
    let gauges: BTreeMap<String, JobFacts> =
        [(STAGE_DWELL_JOB.to_string(), facts(at(0), &["acme/app"]))].into();
    assert_eq!(effective_publish_interval(&cfg, &gauges), cfg.publish_interval);
    let mut with_star = gauges.clone();
    with_star.insert(STAR_FACTS_JOB.to_string(), facts(at(0), &["acme/app"]));
    let interval = effective_publish_interval(&cfg, &with_star);
    assert_eq!(interval, Duration::seconds(300));
    // A published `as_of` reaches a dispatcher's check at most one read lag
    // after the next publish: still inside the bound.
    assert!(interval + Duration::seconds(super::READ_LAG_SECS) <= cfg.star_facts_max_age);
    // A bound tighter than the read lag publishes every pass.
    let tight = config(json!({"starFactsMaxAgeSecs": 300}));
    assert_eq!(effective_publish_interval(&tight, &with_star), Duration::zero());
    // A shorter configured interval is kept.
    let short = config(json!({"publishIntervalSecs": 120}));
    assert_eq!(effective_publish_interval(&short, &with_star), Duration::seconds(120));
}
