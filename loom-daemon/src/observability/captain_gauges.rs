//! Fleet observability gauges as fleet-captain singletons (W12 of the fleet
//! GitHub-API reduction plan).
//!
//! # The problem
//!
//! Some collector gauges describe the *forge*, not the host: forge label-stage
//! dwell ([`super::ops::stage_dwell`], `loom.forge.stage_dwell` /
//! `loom.forge.stage_items`) lists the same stage labels of the same repos on
//! every host that manages them, every 5 minutes, and reads the same per-item
//! events. N hosts spend N times the reader budget to export N copies of one
//! fact (the sums even scale with the host count).
//!
//! # The fix: one producer, assigned
//!
//! The declared `fleet.captain` (#8848) produces these gauges for the fleet;
//! every other host — a **dispatcher** — stops producing them for the repos
//! the captain covers, **only while the captain's output is fresh**. Assigned,
//! not elected: there is no standby producer and no lease. A captain that
//! goes down is visible as an ageing `as_of` on every dispatcher
//! (`loom.captain.gauge_age_seconds`) and, once that passes the staleness
//! bound, as `loom.captain.gauge_fallback = 1` while the dispatchers resume
//! local production — so the gauges never go missing, they only go back to
//! per-host copies until the captain returns.
//!
//! Per-host gauges and every dispatch gate stay local and untouched
//! (`role_queue_gate`, `role_demand`, the work finder's own listings).
//!
//! # How a dispatcher learns the captain is fresh
//!
//! Hosts have no channel to each other's telemetry, so the captain publishes
//! a small heartbeat to the fleet store beside its ETA fit
//! ([`store`], `captain-gauges/v1`): per job, the `as_of` of its last
//! finished pass and the repos it covered. Each dispatcher pass makes one
//! conditional read of it (`304` when unchanged). This is the same transport,
//! credential and branch discipline as #10395's fit distribution. Without
//! `fleet.repo` there is no heartbeat, and every host stays local.
//!
//! # Config (all opt-in; an unconfigured or older host keeps today's behaviour)
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `fleet.captainGauges.enabled` | `false` | on the declared captain: arm the singleton jobs ([`JOBS`]) and publish the heartbeat |
//! | `fleet.captainGauges.standDown` | `false` | on a dispatcher: stop producing a job for the repos a fresh heartbeat covers (env `LOOM_CAPTAIN_GAUGES_STAND_DOWN` overrides) |
//! | `fleet.captainGauges.maxAgeSecs` | `1800` | a job's `as_of` older than this is stale: the dispatcher falls back to local production |
//! | `fleet.captainGauges.publishIntervalSecs` | `600` | how often the captain writes the heartbeat |
//! | `fleet.captainGauges.ref` | `fleet.etaFitRef` | the store branch the heartbeat lives on |
//!
//! The default staleness bound follows the fleet refresh task's liveness rule
//! (`task_liveness::default_stale_after`: two intervals plus slack): two
//! publish intervals plus two collector passes (`2 × 600 + 2 × 300`), so one
//! missed publish never flaps a dispatcher back to local production.
//!
//! # Gating (re-read every collector pass, so an edit needs no restart)
//!
//! - **This host is the declared captain** and `enabled`: [`Role::Captain`],
//!   every job in [`JOBS`] armed (`host.health.armed_singleton_jobs`); it
//!   produces as before and publishes the heartbeat. Never stands down.
//! - **Another host is the captain** and `standDown` with a store configured:
//!   [`Role::Dispatcher`]; per job, the repos the fresh heartbeat covers are
//!   skipped, everything else is produced locally.
//! - **Anything else** (no captain declared, switches off, no store):
//!   [`Role::Local`], exactly today's behaviour. No captain declared is
//!   fail-open, like the ETA fleet refresh (#10329): these are gauges, a
//!   duplicate costs budget, never correctness.

pub mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use self::store::{FetchCache, Fetched, Heartbeat, JobFacts, PublishCache};
use crate::fleet_captain::{self as captain, CaptainGate};
use crate::telemetry::ops::{MetricName, MetricPoint};

/// Forge label-stage dwell ([`super::ops::stage_dwell`]).
pub const STAGE_DWELL_JOB: &str = "stage-dwell";
/// Every job this module arms on the captain.
pub const JOBS: [&str; 1] = [STAGE_DWELL_JOB];

/// Config key: arm and publish on the declared captain.
pub const ENABLED_KEY: &str = "fleet.captainGauges.enabled";
/// Config key: stand down on a dispatcher while the captain is fresh.
pub const STAND_DOWN_KEY: &str = "fleet.captainGauges.standDown";
/// Config key: the staleness bound, in seconds.
pub const MAX_AGE_KEY: &str = "fleet.captainGauges.maxAgeSecs";
/// Config key: the captain's heartbeat cadence, in seconds.
pub const PUBLISH_INTERVAL_KEY: &str = "fleet.captainGauges.publishIntervalSecs";
/// Env override of [`STAND_DOWN_KEY`] (`0`/`false` forces local production).
pub const STAND_DOWN_ENV: &str = "LOOM_CAPTAIN_GAUGES_STAND_DOWN";
/// Default [`MAX_AGE_KEY`].
pub const DEFAULT_MAX_AGE_SECS: i64 = 1800;
/// Default [`PUBLISH_INTERVAL_KEY`].
pub const DEFAULT_PUBLISH_INTERVAL_SECS: i64 = 600;

/// The resolved `fleet.captainGauges` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// `enabled`.
    pub enabled: bool,
    /// `standDown` (after [`STAND_DOWN_ENV`]).
    pub stand_down: bool,
    /// `maxAgeSecs`.
    pub max_age: Duration,
    /// `publishIntervalSecs`.
    pub publish_interval: Duration,
}

fn env_bool(raw: Option<String>) -> Option<bool> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Config {
    /// Resolve from an effective config and an environment lookup. Every
    /// missing or malformed value is its default; a non-positive duration is
    /// ignored.
    #[must_use]
    pub fn from_effective(effective: &Value, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| crate::config_resolver::get_path(effective, key);
        let flag = |key: &str| get(key).and_then(Value::as_bool).unwrap_or(false);
        let secs = |key: &str, default: i64| {
            Duration::seconds(
                get(key)
                    .and_then(Value::as_i64)
                    .filter(|s| *s > 0)
                    .unwrap_or(default),
            )
        };
        Self {
            enabled: flag(ENABLED_KEY),
            stand_down: env_bool(env(STAND_DOWN_ENV)).unwrap_or_else(|| flag(STAND_DOWN_KEY)),
            max_age: secs(MAX_AGE_KEY, DEFAULT_MAX_AGE_SECS),
            publish_interval: secs(PUBLISH_INTERVAL_KEY, DEFAULT_PUBLISH_INTERVAL_SECS),
        }
    }
}

/// This host's part in producing the fleet gauges, for one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// The declared captain with `enabled`: armed, produces and publishes.
    Captain { captain: String },
    /// Another host is the captain and this one opted in to `standDown`.
    Dispatcher { captain: String },
    /// Produce locally, as before; `reason` says why.
    Local { reason: &'static str },
}

impl Role {
    /// Whether the singleton jobs are armed on this host.
    #[must_use]
    pub fn is_captain(&self) -> bool {
        matches!(self, Self::Captain { .. })
    }
}

/// The role for one pass. Pure.
#[must_use]
pub fn resolve_role(gate: &CaptainGate, config: &Config, store_configured: bool) -> Role {
    match gate {
        CaptainGate::Armed { captain } if config.enabled => Role::Captain {
            captain: captain.clone(),
        },
        CaptainGate::Armed { .. } => Role::Local {
            reason: "captain_not_enabled",
        },
        CaptainGate::Refused { .. } if !config.stand_down => Role::Local {
            reason: "stand_down_off",
        },
        CaptainGate::Refused { .. } if !store_configured => Role::Local { reason: "no_store" },
        CaptainGate::Refused { captain, .. } => Role::Dispatcher {
            captain: captain.clone(),
        },
        CaptainGate::NoCaptainDeclared => Role::Local {
            reason: "no_captain",
        },
    }
}

/// Per job, the lowercased repos the captain freshly covers.
pub type Coverage = BTreeMap<String, BTreeSet<String>>;

/// What a dispatcher may leave to `captain` at `now`, from `heartbeat`. Pure.
#[must_use]
pub fn coverage(
    heartbeat: Option<&Heartbeat>,
    captain: &str,
    now: DateTime<Utc>,
    max_age: Duration,
) -> Coverage {
    let Some(hb) = heartbeat else {
        return Coverage::new();
    };
    JOBS.iter()
        .filter_map(|job| {
            hb.fresh(job, captain, now, max_age)
                .map(|facts| ((*job).to_string(), facts.repos.clone()))
        })
        .collect()
}

/// The pass's gauges. Pure. Emitted only while the feature is engaged
/// (captain or dispatcher), and only measurable readings: an unknown age is
/// no point, never zero.
///
/// - `loom.captain.gauge_age_seconds{task}`: on the captain, the age of its
///   own last production; on a dispatcher, the age of the captain's `as_of`
///   as last read. Ages past `maxAgeSecs` mean the captain stopped producing.
/// - `loom.captain.gauge_fallback{task}` (dispatchers only): `1` while the
///   captain's data for the job is stale or absent and this host produces it
///   locally, `0` while it stands down.
#[must_use]
pub fn points(
    role: &Role,
    produced: &BTreeMap<String, JobFacts>,
    heartbeat: Option<&Heartbeat>,
    covered: &Coverage,
    now: DateTime<Utc>,
) -> Vec<MetricPoint> {
    let age = |at: DateTime<Utc>| (now - at).num_seconds().max(0);
    let mut out = Vec::new();
    for job in JOBS {
        match role {
            Role::Captain { .. } => {
                if let Some(facts) = produced.get(job) {
                    out.push(
                        MetricPoint::int(MetricName::CaptainGaugeAgeSeconds, age(facts.as_of))
                            .label("task", job),
                    );
                }
            }
            Role::Dispatcher { captain } => {
                let seen = heartbeat
                    .filter(|hb| hb.captain_host == *captain)
                    .and_then(|hb| hb.jobs.get(job));
                if let Some(facts) = seen {
                    out.push(
                        MetricPoint::int(MetricName::CaptainGaugeAgeSeconds, age(facts.as_of))
                            .label("task", job),
                    );
                }
                let fallback = i64::from(!covered.contains_key(job));
                out.push(
                    MetricPoint::int(MetricName::CaptainGaugeFallback, fallback).label("task", job),
                );
            }
            Role::Local { .. } => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Process state
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct State {
    role: Option<Role>,
    /// Dispatcher: what the captain freshly covers this pass.
    covered: Coverage,
    /// This host's last finished pass per job (the captain publishes it).
    produced: BTreeMap<String, JobFacts>,
    fetch: FetchCache,
    publish: PublishCache,
    last_published: Option<DateTime<Utc>>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    f(guard.get_or_insert_with(State::default))
}

/// Whether this host should leave `job` for `slug` to the captain this pass:
/// it is a dispatcher, opted in, and the captain's heartbeat covers the repo
/// with a fresh `as_of`. `false` in every other case (the safe default).
#[must_use]
pub fn captain_covers(job: &str, slug: &str) -> bool {
    let slug = slug.to_ascii_lowercase();
    with(|s| {
        s.covered
            .get(job)
            .is_some_and(|repos| repos.contains(&slug))
    })
}

/// Record that this host finished producing `job` for `repos` at `at` (its
/// points are with the sink). Cheap and unconditional; only the captain
/// publishes it.
pub fn note_produced(job: &str, repos: impl IntoIterator<Item = String>, at: DateTime<Utc>) {
    let repos = repos.into_iter().map(|r| r.to_ascii_lowercase()).collect();
    with(|s| {
        s.produced
            .insert(job.to_string(), JobFacts { as_of: at, repos });
    });
}

/// The forge-gauge stage of the collector pass: resolve this pass's role
/// (publishing or reading the heartbeat), then sample the stage dwell, which
/// consults [`captain_covers`].
pub(super) async fn forge_gauges(
    workspace_root: &Path,
    workspace_pool: &crate::workspace_pool::WorkspacePool,
    slug_cache: &mut std::collections::HashMap<String, String>,
) {
    let root = workspace_root.to_path_buf();
    let host = crate::sweep_registry::host_identity();
    let _ = tokio::task::spawn_blocking(move || tick(&root, &host, Utc::now())).await;
    super::ops::stage_dwell::record(workspace_pool, slug_cache).await;
}

/// One pass: role, arming, heartbeat, coverage, gauges. Blocking (the store
/// read or write runs here, outside the state lock).
fn tick(root: &Path, host: &str, now: DateTime<Utc>) {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let env = |k: &str| std::env::var(k).ok();
    let config = Config::from_effective(&effective, &env);
    let location = store::location_for(&effective, &env);
    let gate = captain::resolve_gate_for_root(root, host);
    let mut role = resolve_role(&gate, &config, location.is_some());
    if role.is_captain() {
        for job in JOBS {
            if let Err(refusal) = captain::arm_singleton_job(job, root, host) {
                // `fleet.captain` changed between the two reads: local this pass.
                log::debug!("captain gauges: {refusal}");
                role = Role::Local {
                    reason: "captain_changed",
                };
            }
        }
    }
    if !role.is_captain() {
        JOBS.iter()
            .for_each(|job| captain::disarm_singleton_job(job));
    }
    // Take the caches out so the store call runs without the lock.
    let (mut fetch, mut publish, produced, mut published, previous) = with(|s| {
        (
            std::mem::take(&mut s.fetch),
            std::mem::take(&mut s.publish),
            s.produced.clone(),
            s.last_published,
            s.covered.clone(),
        )
    });
    let mut covered = Coverage::new();
    match &role {
        Role::Captain { captain } => {
            fetch = FetchCache::default();
            let due = published.is_none_or(|at| now - at >= config.publish_interval);
            if let Some((loc, base)) = location.as_ref().filter(|_| due && !produced.is_empty()) {
                let hb = Heartbeat::new(captain, now, produced.clone());
                match store::publish_heartbeat(root, loc, base, &hb, &mut publish) {
                    Ok(()) => published = Some(now),
                    Err(e) => log::warn!("captain gauges: could not publish the heartbeat: {e:#}"),
                }
            }
        }
        Role::Dispatcher { captain } => {
            if let Some((loc, _)) = &location {
                match store::fetch_heartbeat(root, loc, &mut fetch) {
                    Fetched::Failed(why) => {
                        log::debug!("captain gauges: heartbeat read failed: {why}");
                    }
                    Fetched::Absent => log::debug!("captain gauges: no heartbeat published"),
                    Fetched::Updated | Fetched::NotModified => {}
                }
            }
            covered = coverage(fetch.heartbeat.as_ref(), captain, now, config.max_age);
            log_fallback(&previous, &covered);
        }
        Role::Local { .. } => fetch = FetchCache::default(),
    }
    let out = points(&role, &produced, fetch.heartbeat.as_ref(), &covered, now);
    with(|s| {
        if s.role.as_ref() != Some(&role) {
            log_role(&role, host, location.is_some());
        }
        s.role = Some(role);
        s.covered = covered;
        s.fetch = fetch;
        s.publish = publish;
        s.last_published = published;
    });
    if !out.is_empty() {
        super::ops::emit_metrics(out);
    }
}

fn log_role(role: &Role, host: &str, store: bool) {
    match role {
        Role::Captain { .. } if !store => log::warn!(
            "captain gauges: this host ({host}) is the fleet captain with \
             fleet.captainGauges.enabled, but no fleet store (fleet.repo) is configured: no \
             heartbeat is published, so every dispatcher keeps producing {JOBS:?} locally"
        ),
        Role::Captain { .. } => log::info!(
            "captain gauges: this host ({host}) is the fleet captain — it produces {JOBS:?} for \
             the fleet and publishes their freshness to the fleet store"
        ),
        Role::Dispatcher { captain } => log::info!(
            "captain gauges: the fleet captain is {captain}; this host ({host}) stops producing \
             {JOBS:?} for the repos the captain covers while its heartbeat is fresh"
        ),
        Role::Local { reason } => {
            log::info!("captain gauges: this host ({host}) produces {JOBS:?} locally ({reason})")
        }
    }
}

/// Log a job moving between standing down and falling back, once per change.
fn log_fallback(previous: &Coverage, now: &Coverage) {
    for job in JOBS {
        match (previous.contains_key(job), now.contains_key(job)) {
            (true, false) => log::warn!(
                "captain gauges: the captain's {job} data is stale or missing; producing it \
                 locally until it is fresh again"
            ),
            (false, true) => log::info!(
                "captain gauges: the captain's {job} data is fresh; standing down for the {} \
                 repo(s) it covers",
                now.get(job).map_or(0, BTreeSet::len)
            ),
            _ => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
