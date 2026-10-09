//! The captain-side output watchdog's feed (#10916, slice 3a).
//!
//! The watchdog runs on the declared `fleet.captain` only (singleton job
//! [`SINGLETON_JOB_NAME`]), inside the `fleet_alert` thread. It reads the
//! fleet's outputs, never the job owners' own view, from two stores every
//! host feeds ([`crate::fleet_outputs::observed`]):
//!
//! - SigNoz logs, through the ClickHouse endpoint the ETA SigNoz reader
//!   already uses (`autonomous.eta.fleetRefresh.signoz.endpoint` / `user` /
//!   `credentialFile`; no new config surface);
//! - the captain's own `captain-gauges/v1` state: what its gauge passes last
//!   produced, read in-process (the watchdog runs on the captain, which is
//!   the host that publishes the heartbeat). No store read, so no forge call.
//!
//! **No forge calls anywhere in this module**, so a rate-limited `gh` can
//! neither block an alert nor make a gauge row fire as unreadable (pinned by
//! `no_forge_call_in_output_feed`). The one network read is SigNoz, so a
//! separate refresher thread reads every [`REFRESH`] and the alert thread
//! only takes its latest [`Reading`]. Failing loud, never silent:
//!
//! - a failed read is the affected rows' firing condition, once the last
//!   good read of that store is older than [`STALE_AFTER`] (so one transient
//!   failure inside the alert debounce does not page);
//! - a refresher that stalls, panics or never reads ages its reading past
//!   [`STALE_AFTER`], and every row fires as unreadable;
//! - an unconfigured endpoint is a read error, not a reason to skip.
//!
//! The roster is the fleet store's cached roster (`fleet.json`, else
//! `repos.yml`, as `fleet_sync` keeps it on disk): never fetched here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::fleet_outputs::observed::{self, Observed, Reading, Row};
use crate::observability::captain_gauges::store::Heartbeat;

/// The watchdog's singleton job name (armed on the captain only).
pub const SINGLETON_JOB_NAME: &str = "fleet-output-watchdog";
/// How often the refresher reads both stores.
pub const REFRESH: Duration = Duration::from_secs(300);
/// A store whose last good read is older than this is unreadable.
pub const STALE_AFTER: Duration = Duration::from_secs(3 * 300);

/// The reads, as a seam (production: [`LiveReader`]).
pub trait OutputReader {
    /// [`observed::OUTPUTS_SQL`]'s body for a read at `now`.
    ///
    /// # Errors
    ///
    /// Unconfigured, unreachable or refused.
    fn signoz(&mut self, now: DateTime<Utc>) -> Result<String, String>;
    /// What the captain's gauge passes last produced (`None`: nothing yet).
    ///
    /// # Errors
    ///
    /// The state could not be read.
    fn heartbeat(&mut self) -> Result<Option<Heartbeat>, String>;
    /// The fleet roster (lowercased `owner/repo`); empty when unknown.
    fn roster(&mut self) -> Vec<String>;
}

fn stale(at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    chrono::Duration::from_std(STALE_AFTER).is_ok_and(|limit| now - at > limit)
}

/// `fresh`, or on failure the last good value while it is not stale; the
/// error once it is. Updates `last_good` on success.
pub fn settle<T: Clone>(
    fresh: Result<T, String>,
    last_good: &mut Option<(DateTime<Utc>, T)>,
    now: DateTime<Utc>,
) -> Result<T, String> {
    match fresh {
        Ok(v) => {
            *last_good = Some((now, v.clone()));
            Ok(v)
        }
        Err(e) => match last_good {
            Some((at, v)) if !stale(*at, now) => {
                log::warn!(
                    "fleet_alert: output watchdog read failed, keeping the read of {at}: {e}"
                );
                Ok(v.clone())
            }
            _ => Err(e),
        },
    }
}

/// The refresher's carried state.
#[derive(Default)]
pub struct Refresher {
    signoz: Option<(DateTime<Utc>, Vec<Row>)>,
    heartbeat: Option<(DateTime<Utc>, Option<Heartbeat>)>,
}

impl Refresher {
    /// One refresh at `now`.
    pub fn read(
        &mut self,
        reader: &mut dyn OutputReader,
        now: DateTime<Utc>,
    ) -> (Reading, Vec<String>) {
        let rows = reader
            .signoz(now)
            .and_then(|body| observed::parse_rows(&body));
        let reading = Reading {
            at: now,
            signoz: settle(rows, &mut self.signoz, now),
            heartbeat: settle(reader.heartbeat(), &mut self.heartbeat, now),
        };
        (reading, reader.roster())
    }
}

/// The latest reading and the roster read with it.
#[derive(Default)]
struct Slot {
    latest: Option<(Reading, Vec<String>)>,
}

/// The source the alert thread judges at `now`: `None` only while warming up
/// (armed for less than [`STALE_AFTER`] with no reading yet).
#[must_use]
pub fn source_at(
    latest: Option<&(Reading, Vec<String>)>,
    armed_since: DateTime<Utc>,
    disabled: BTreeSet<&'static str>,
    now: DateTime<Utc>,
) -> Option<Observed> {
    match latest {
        Some((reading, roster)) if !stale(reading.at, now) => {
            Some(observed::build(reading, roster.clone(), disabled, now))
        }
        Some((reading, roster)) => Some(Observed::unreadable_all(
            &format!("the output reader last read at {}; it has stalled", reading.at),
            roster.clone(),
            disabled,
        )),
        None if !stale(armed_since, now) => None,
        None => Some(Observed::unreadable_all(
            &format!("no output read since the watchdog armed at {armed_since}"),
            Vec::new(),
            disabled,
        )),
    }
}

/// The registry toggles that are off on this host.
fn disabled_here(root: &Path) -> BTreeSet<&'static str> {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let ci = crate::ci_telemetry::resolve(&crate::ci_telemetry::read_config(root)).enabled;
    observed::disabled_from(&effective, &|k| std::env::var(k).ok(), ci)
}

/// The alert thread's handle on the refresher.
pub struct Feed {
    root: PathBuf,
    slot: Arc<Mutex<Slot>>,
    armed_since: Option<DateTime<Utc>>,
    refused: Option<String>,
}

impl Feed {
    /// Start the refresher thread for `root` on `host`. If the thread cannot
    /// start, nothing ever reads and every row fires once warm-up ends.
    #[must_use]
    pub fn start(root: PathBuf, host: String) -> Self {
        let slot = Arc::new(Mutex::new(Slot::default()));
        let shared = Arc::clone(&slot);
        let thread_root = root.clone();
        let spawned = std::thread::Builder::new()
            .name("fleet-output-watch".to_string())
            .spawn(move || refresh_loop(&thread_root, &host, &shared));
        if let Err(e) = spawned {
            log::warn!("fleet_alert: output watchdog reader could not start: {e}");
        }
        Self {
            root,
            slot,
            armed_since: None,
            refused: None,
        }
    }

    /// The source for this tick, or `None` when the watchdog does not run
    /// here (not the captain) or is warming up.
    pub fn observed(&mut self, host: &str, now: DateTime<Utc>) -> Option<Observed> {
        match crate::fleet_captain::arm_singleton_job(SINGLETON_JOB_NAME, &self.root, host) {
            Ok(()) => self.refused = None,
            Err(msg) => {
                if self.refused.as_deref() != Some(msg.as_str()) {
                    log::info!("fleet_alert: {msg}");
                    self.refused = Some(msg);
                }
                self.armed_since = None;
                return None;
            }
        }
        let armed_since = *self.armed_since.get_or_insert(now);
        let slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        source_at(slot.latest.as_ref(), armed_since, disabled_here(&self.root), now)
    }
}

impl Feed {
    /// [`Feed::observed`], with a panic in the read path judged as every row
    /// unreadable instead of killing the alert thread.
    pub fn observed_guarded(&mut self, host: &str, now: DateTime<Utc>) -> Option<Observed> {
        let run = std::panic::AssertUnwindSafe(|| self.observed(host, now));
        std::panic::catch_unwind(run).unwrap_or_else(|_| {
            Some(Observed::unreadable_all(
                "the output watchdog's read path panicked",
                Vec::new(),
                BTreeSet::new(),
            ))
        })
    }
}

fn refresh_loop(root: &Path, host: &str, slot: &Mutex<Slot>) {
    let mut reader = LiveReader::new(root.to_path_buf(), host.to_string());
    let mut refresher = Refresher::default();
    loop {
        // A pure config read: only the captain spends the reads.
        if crate::fleet_captain::resolve_gate_for_root(root, host).is_armed() {
            let read = refresher.read(&mut reader, Utc::now());
            slot.lock().unwrap_or_else(PoisonError::into_inner).latest = Some(read);
            std::thread::sleep(REFRESH);
        } else {
            slot.lock().unwrap_or_else(PoisonError::into_inner).latest = None;
            std::thread::sleep(Duration::from_secs(30));
        }
    }
}

/// The production reader: ClickHouse over HTTP, the gauge heartbeat from the
/// captain's in-process state, the roster from the store cache. Config is
/// re-resolved on every read.
pub struct LiveReader {
    root: PathBuf,
    host: String,
}

impl LiveReader {
    #[must_use]
    pub fn new(root: PathBuf, host: String) -> Self {
        Self { root, host }
    }
}

impl OutputReader for LiveReader {
    fn signoz(&mut self, now: DateTime<Utc>) -> Result<String, String> {
        use crate::eta::fleet_signoz_refresh::{ClickhouseHttp, ReadError};
        let effective = crate::config_resolver::resolve_effective_config(&self.root);
        let config = crate::eta::config::resolve(&effective, |k| std::env::var(k).ok())
            .fleet_refresh
            .signoz;
        let endpoint = config.endpoint.ok_or_else(|| {
            "no SigNoz endpoint configured (autonomous.eta.fleetRefresh.signoz.endpoint)"
                .to_string()
        })?;
        let http = ClickhouseHttp {
            endpoint,
            user: config.user,
            credential_file: config.credential_file,
            timeout: Duration::from_secs(60),
        };
        http.post_with(observed::OUTPUTS_SQL, &observed::params(now))
            .map_err(|e| match e {
                ReadError::Unavailable(why) | ReadError::Refused(why) => why,
            })
    }

    fn heartbeat(&mut self) -> Result<Option<Heartbeat>, String> {
        Ok(crate::observability::captain_gauges::produced_heartbeat(&self.host, Utc::now()))
    }

    fn roster(&mut self) -> Vec<String> {
        match cached_roster(&self.root) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("fleet_alert: output watchdog roster unknown: {e}");
                Vec::new()
            }
        }
    }
}

/// The fleet roster's desired repos from the on-disk store cache. `Ok(empty)`
/// with no store or no cache yet.
fn cached_roster(root: &Path) -> Result<Vec<String>, String> {
    use crate::fleet_store::{self as store, fetch, roster};
    let effective = crate::config_resolver::resolve_effective_config(root);
    let location = store::resolve_location(&effective, &|k| std::env::var(k).ok())
        .map_err(|e| format!("{e:#}"))?;
    let Some(location) = location else {
        return Ok(Vec::new());
    };
    let cache = store::default_cache_dir(&location).map_err(|e| format!("{e:#}"))?;
    let Some(snapshot) = fetch::read_cache(&cache, &location).map_err(|e| format!("{e:#}"))? else {
        return Ok(Vec::new());
    };
    let home = dirs::home_dir().ok_or("no home directory")?;
    let Some(parsed) = roster::from_snapshot(&snapshot, &home).map_err(|e| format!("{e:#}"))?
    else {
        return Ok(Vec::new());
    };
    let revision =
        crate::eta::repo_priority::RosterRevision::from_roster(&parsed, Utc::now(), None);
    let repos: BTreeSet<String> = revision
        .members
        .into_iter()
        .filter_map(|m| m.repo)
        .collect();
    Ok(repos.into_iter().collect())
}
