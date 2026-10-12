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
//! local production. The hand-back is not seamless: a dispatcher notices a
//! dead captain only after `maxAgeSecs` plus the clock-skew allowance plus one
//! collector pass (about 40 minutes at the defaults), stage transitions inside
//! that window are not sampled by anyone, and the dispatcher then starts from
//! a baseline. After the gap the gauges are per-host copies again until the
//! captain returns.
//!
//! Per-host gauges and every dispatch gate stay local and untouched
//! (`role_queue_gate`, `role_demand`, the work finder's own listings).
//!
//! # Part 2: forge facts, not only gauges ([`facts`])
//!
//! Two more per-host forge reads are the same on every host, and each has its
//! own switch (off by default, so a part 1 fleet is unchanged):
//!
//! - **`star-facts`** (`starFacts`). The starred-issue liveness pass
//!   ([`crate::star_liveness`]) evaluator lists every operator label of every
//!   managed repo every two minutes, and does nothing else for a repo with no
//!   open starred issue (the level step after it lists its own labels and is
//!   not covered). The captain publishes, per repo, how many open starred
//!   issues there are; a dispatcher skips its evaluator for a repo the captain
//!   freshly reports as having none ([`star_free`]). A repo with a star is
//!   evaluated locally by every host exactly as before: landing rows, blocker
//!   inheritance (a dispatch input) and escalation never come from the
//!   captain.
//! - **`queue-blocked`** (`queueBlocked`). The `loom:blocked` rows of
//!   `queue.snapshot` ([`super::queue_blocked`]) come from one listing per
//!   repo per snapshot. The captain publishes the listing reduced to number,
//!   creation time and label names; a dispatcher builds its own rows from it
//!   ([`captain_blocked`]) and appends them to its own snapshot.
//!
//! Both are read **at the time of use**, with the caller's clock: the
//! liveness thread has its own cadence and must not trust a verdict a stalled
//! collector left behind. Missing, stale or uncovered always means today's
//! local read.
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
//! | `fleet.captainGauges.enabled` | `false` | on the declared captain: arm the singleton jobs ([`Config::jobs`]) and publish the heartbeat |
//! | `fleet.captainGauges.standDown` | `false` | on a dispatcher: stop producing a job for the repos a fresh heartbeat covers (env `LOOM_CAPTAIN_GAUGES_STAND_DOWN` overrides) |
//! | `fleet.captainGauges.maxAgeSecs` | `1800` | a job's `as_of` older than this is stale: the dispatcher falls back to local production |
//! | `fleet.captainGauges.publishIntervalSecs` | `600` | how often the captain writes the heartbeat |
//! | `fleet.captainGauges.ref` | `eta-fit` (legacy fallback: `fleet.etaFitRef`) | the store branch the heartbeat lives on |
//! | `fleet.captainGauges.starFacts` | `false` | captain: produce `star-facts`; dispatcher: skip the liveness evaluator for repos it reports star-free |
//! | `fleet.captainGauges.queueBlocked` | `false` | captain: produce `queue-blocked`; dispatcher: build its snapshot's blocked rows from it |
//! | `fleet.captainGauges.starFactsMaxAgeSecs` | `900` | the staleness bound for `star-facts` alone (see below) |
//!
//! The default staleness bound follows the fleet refresh task's liveness rule
//! (`task_liveness::default_stale_after`: two intervals plus slack): two
//! publish intervals plus two collector passes (`2 × 600 + 2 × 300`), so one
//! missed publish never flaps a dispatcher back to local production.
//!
//! `star-facts` has its own, tighter bound (`starFactsMaxAgeSecs`, 900 s: two
//! collector passes plus slack). A "no star here" a dispatcher believes is a
//! liveness pass it skips, so its age is the delay before a brand-new star is
//! evaluated when the captain stops reporting, not just a gauge's lag. The
//! dispatcher ages its last read heartbeat against it at every liveness pass,
//! so a failed heartbeat read does not extend it. While it produces
//! `star-facts` the captain republishes at least every `starFactsMaxAgeSecs`
//! minus [`READ_LAG_SECS`] ([`effective_publish_interval`]), so a healthy
//! captain's facts never age out between publishes.
//!
//! # Gating (re-read every collector pass, so an edit needs no restart)
//!
//! - **This host is the declared captain** and `enabled`: [`Role::Captain`],
//!   its jobs ([`Config::jobs`]) armed (`host.health.armed_singleton_jobs`); it
//!   produces as before and publishes the heartbeat. Never stands down.
//! - **Another host is the captain** and `standDown` with a store configured:
//!   [`Role::Dispatcher`]; per job, the repos the fresh heartbeat covers are
//!   skipped, everything else is produced locally.
//! - **Anything else** (no captain declared, switches off, no store):
//!   [`Role::Local`], exactly today's behaviour. No captain declared is
//!   fail-open, like the ETA fleet refresh (#10329): these are gauges, a
//!   duplicate costs budget, never correctness.

pub mod facts;
pub mod store;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use self::store::{BlockedFact, FetchCache, Fetched, Heartbeat, JobFacts, PublishCache};
use crate::fleet_captain::{self as captain, CaptainGate};
use crate::telemetry::ops::{MetricName, MetricPoint};

/// Forge label-stage dwell ([`super::ops::stage_dwell`]).
pub const STAGE_DWELL_JOB: &str = "stage-dwell";
/// Whether each repo has an open starred issue ([`facts`], for
/// [`crate::star_liveness`]).
pub const STAR_FACTS_JOB: &str = "star-facts";
/// Each repo's open `loom:blocked` issues ([`facts`], for
/// [`super::queue_blocked`]).
pub const QUEUE_BLOCKED_JOB: &str = "queue-blocked";
/// Every job this module can arm on the captain. Which ones a host runs is
/// [`Config::jobs`].
pub const JOBS: [&str; 3] = [STAGE_DWELL_JOB, STAR_FACTS_JOB, QUEUE_BLOCKED_JOB];

/// Config key: arm and publish on the declared captain.
pub const ENABLED_KEY: &str = "fleet.captainGauges.enabled";
/// Config key: stand down on a dispatcher while the captain is fresh.
pub const STAND_DOWN_KEY: &str = "fleet.captainGauges.standDown";
/// Config key: the staleness bound, in seconds.
pub const MAX_AGE_KEY: &str = "fleet.captainGauges.maxAgeSecs";
/// Config key: the captain's heartbeat cadence, in seconds.
pub const PUBLISH_INTERVAL_KEY: &str = "fleet.captainGauges.publishIntervalSecs";
/// Config key: the `star-facts` job, on both roles.
pub const STAR_FACTS_KEY: &str = "fleet.captainGauges.starFacts";
/// Config key: the `queue-blocked` job, on both roles.
pub const QUEUE_BLOCKED_KEY: &str = "fleet.captainGauges.queueBlocked";
/// Config key: the `star-facts` staleness bound, in seconds.
pub const STAR_FACTS_MAX_AGE_KEY: &str = "fleet.captainGauges.starFactsMaxAgeSecs";
/// Env override of [`STAND_DOWN_KEY`] (`0`/`false` forces local production).
pub const STAND_DOWN_ENV: &str = "LOOM_CAPTAIN_GAUGES_STAND_DOWN";
/// Default [`MAX_AGE_KEY`].
pub const DEFAULT_MAX_AGE_SECS: i64 = 1800;
/// Default [`PUBLISH_INTERVAL_KEY`].
pub const DEFAULT_PUBLISH_INTERVAL_SECS: i64 = 600;
/// Default [`STAR_FACTS_MAX_AGE_KEY`]: two collector passes (`2 × 300`) plus
/// 300 s of slack. The captain republishes often enough to stay inside it
/// ([`effective_publish_interval`]).
pub const DEFAULT_STAR_FACTS_MAX_AGE_SECS: i64 = 900;

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
    /// `starFacts`.
    pub star_facts: bool,
    /// `queueBlocked`.
    pub queue_blocked: bool,
    /// `starFactsMaxAgeSecs`: the `star-facts` bound, in place of `max_age`.
    pub star_facts_max_age: Duration,
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
            star_facts: flag(STAR_FACTS_KEY),
            queue_blocked: flag(QUEUE_BLOCKED_KEY),
            star_facts_max_age: secs(STAR_FACTS_MAX_AGE_KEY, DEFAULT_STAR_FACTS_MAX_AGE_SECS),
        }
    }

    /// The staleness bound for `job`: `starFactsMaxAgeSecs` for `star-facts`,
    /// `maxAgeSecs` for everything else.
    #[must_use]
    pub fn max_age_for(&self, job: &str) -> Duration {
        if job == STAR_FACTS_JOB {
            self.star_facts_max_age
        } else {
            self.max_age
        }
    }

    /// The jobs this host takes part in: `stage-dwell` always, the part 2
    /// jobs by their own switches.
    #[must_use]
    pub fn jobs(&self) -> Vec<&'static str> {
        let mut jobs = vec![STAGE_DWELL_JOB];
        if self.star_facts {
            jobs.push(STAR_FACTS_JOB);
        }
        if self.queue_blocked {
            jobs.push(QUEUE_BLOCKED_JOB);
        }
        jobs
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

/// What a dispatcher may leave to `captain` at `now`, from `heartbeat`, each
/// job judged against its own bound ([`Config::max_age_for`]). Pure.
#[must_use]
pub fn coverage(
    heartbeat: Option<&Heartbeat>,
    captain: &str,
    now: DateTime<Utc>,
    config: &Config,
) -> Coverage {
    let Some(hb) = heartbeat else {
        return Coverage::new();
    };
    JOBS.iter()
        .filter_map(|job| {
            hb.fresh(job, captain, now, config.max_age_for(job))
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
    jobs: &[&str],
    role: &Role,
    produced: &BTreeMap<String, JobFacts>,
    heartbeat: Option<&Heartbeat>,
    covered: &Coverage,
    now: DateTime<Utc>,
) -> Vec<MetricPoint> {
    let age = |at: DateTime<Utc>| (now - at).num_seconds().max(0);
    let mut out = Vec::new();
    for job in jobs.iter().copied() {
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

/// `job`'s facts when this host may rely on them at `now`: it is a dispatcher,
/// the job is switched on here, and the declared captain's heartbeat is fresh
/// for it. Pure.
#[must_use]
pub fn fresh_job<'a>(
    role: Option<&Role>,
    config: Option<&Config>,
    heartbeat: Option<&'a Heartbeat>,
    job: &str,
    now: DateTime<Utc>,
) -> Option<&'a JobFacts> {
    let (Some(Role::Dispatcher { captain }), Some(config)) = (role, config) else {
        return None;
    };
    if !config.jobs().contains(&job) {
        return None;
    }
    heartbeat?.fresh(job, captain, now, config.max_age_for(job))
}

/// What the captain published last, with its `as_of` zeroed: two heartbeats
/// with the same key say the same thing about the fleet. A change is
/// published at once instead of waiting for the interval.
#[must_use]
pub fn content_key(produced: &BTreeMap<String, JobFacts>) -> String {
    let mut content = produced.clone();
    for facts in content.values_mut() {
        facts.as_of = DateTime::<Utc>::UNIX_EPOCH;
    }
    serde_json::to_string(&content).unwrap_or_default()
}

/// Whether the captain should write the heartbeat now: nothing published
/// yet, the interval elapsed, or the content changed. Pure.
#[must_use]
pub fn publish_due(
    last: Option<(DateTime<Utc>, &str)>,
    key: &str,
    now: DateTime<Utc>,
    interval: Duration,
) -> bool {
    last.is_none_or(|(at, published)| now - at >= interval || published != key)
}

/// How long a published `as_of` takes to reach a dispatcher's check: its next
/// collector pass reads the heartbeat (300 s), and the captain's own pass
/// takes as long again at most.
pub const READ_LAG_SECS: i64 = 600;

/// How often the captain must republish: `publishIntervalSecs`, shortened
/// while `star-facts` is produced so its published `as_of` stays inside
/// `starFactsMaxAgeSecs` for as long as the captain keeps producing
/// (`starFactsMaxAgeSecs − READ_LAG_SECS`, 300 s at the defaults). Without
/// it a quiet fleet's facts would go stale between publishes and every
/// dispatcher would flap back to its own listings. Pure.
#[must_use]
pub fn effective_publish_interval(
    config: &Config,
    produced: &BTreeMap<String, JobFacts>,
) -> Duration {
    if !produced.contains_key(STAR_FACTS_JOB) {
        return config.publish_interval;
    }
    let star = (config.star_facts_max_age - Duration::seconds(READ_LAG_SECS)).max(Duration::zero());
    config.publish_interval.min(star)
}

#[derive(Debug, Default)]
struct State {
    role: Option<Role>,
    /// The config the role was resolved from.
    config: Option<Config>,
    /// The content key of the last published heartbeat.
    published_key: Option<String>,
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
    note_facts(job, JobFacts::covering(at, repos));
}

/// [`note_produced`] for a job that publishes more than its coverage.
fn note_facts(job: &str, facts: JobFacts) {
    with(|s| {
        s.produced.insert(job.to_string(), facts);
    });
}

/// The repos (lowercased slugs) the captain freshly reports as having **no
/// open starred issue**, each with the `as_of` of that report, for a liveness
/// pass at `now` whose level table lists `labels`. Empty unless this host is
/// a dispatcher with `starFacts` on and the captain listed every one of
/// `labels`: the caller then evaluates every repo itself.
#[must_use]
pub fn star_free(now: DateTime<Utc>, labels: &[&str]) -> HashMap<String, DateTime<Utc>> {
    with(|s| {
        fresh_job(
            s.role.as_ref(),
            s.config.as_ref(),
            s.fetch.heartbeat.as_ref(),
            STAR_FACTS_JOB,
            now,
        )
        .map(|job| facts::star_free(job, labels))
        .unwrap_or_default()
    })
}

/// The captain's `loom:blocked` rows for `slug` when they are fresh at `now`
/// and cover it (`Some(vec![])`: covered, none blocked). `None`: list it
/// locally.
#[must_use]
pub fn captain_blocked(slug: &str, now: DateTime<Utc>) -> Option<Vec<BlockedFact>> {
    let slug = slug.to_ascii_lowercase();
    with(|s| {
        let job = fresh_job(
            s.role.as_ref(),
            s.config.as_ref(),
            s.fetch.heartbeat.as_ref(),
            QUEUE_BLOCKED_JOB,
            now,
        )?;
        job.repos
            .contains(&slug)
            .then(|| job.blocked.get(&slug).cloned().unwrap_or_default())
    })
}

/// On the captain, how long a repo may go unsampled before its sampler state
/// is too old to diff against (`maxAgeSecs`): by then every dispatcher has
/// fallen back, baselined and emitted the transitions itself, so replaying
/// them from the pre-outage state would double count. `None` on every other
/// host, which keeps catching up as before.
#[must_use]
pub fn replay_bound() -> Option<Duration> {
    with(|s| match (&s.role, &s.config) {
        (Some(Role::Captain { .. }), Some(config)) => Some(config.max_age),
        _ => None,
    })
}

/// The forge-gauge stage of the collector pass: resolve this pass's role,
/// produce the captain's facts, publish or read the heartbeat, then sample
/// the stage dwell, which consults [`captain_covers`].
pub(super) async fn forge_gauges(
    workspace_root: &Path,
    workspace_pool: &crate::workspace_pool::WorkspacePool,
    slug_cache: &mut std::collections::HashMap<String, String>,
) {
    let root = workspace_root.to_path_buf();
    let host = crate::sweep_registry::host_identity();
    let resolved = tokio::task::spawn_blocking({
        let root = root.clone();
        move || resolve(&root, &host)
    })
    .await;
    if let Ok(pass) = resolved {
        if pass.role.is_captain() {
            // Before the exchange, so this pass's facts are what it publishes.
            facts::produce(&pass.config, workspace_pool, slug_cache).await;
        }
        let _ = tokio::task::spawn_blocking(move || exchange(&root, &pass, Utc::now())).await;
    }
    super::ops::stage_dwell::record(workspace_pool, slug_cache).await;
}

/// One pass's resolved gating.
struct Pass {
    role: Role,
    config: Config,
    location: Option<(crate::fleet_store::StoreLocation, String)>,
}

/// Resolve this pass's role and arm or disarm the jobs. Blocking (config and
/// registry reads only; no forge call).
fn resolve(root: &Path, host: &str) -> Pass {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let env = |k: &str| std::env::var(k).ok();
    let config = Config::from_effective(&effective, &env);
    let gate = captain::resolve_gate_for_root(root, host);
    // Only a host that takes part reads the store's location: with its
    // switch off a misconfigured `fleet.repo` is not this feature's warning.
    let wants_store = match &gate {
        CaptainGate::Armed { .. } => config.enabled,
        CaptainGate::Refused { .. } => config.stand_down,
        CaptainGate::NoCaptainDeclared => false,
    };
    let location = wants_store
        .then(|| store::location_for(&effective, &env))
        .flatten();
    let mut role = resolve_role(&gate, &config, location.is_some());
    let jobs = config.jobs();
    if role.is_captain() {
        for job in &jobs {
            if let Err(refusal) = captain::arm_singleton_job(job, root, host) {
                // `fleet.captain` changed between the two reads: local this pass.
                log::debug!("captain gauges: {refusal}");
                role = Role::Local {
                    reason: "captain_changed",
                };
            }
        }
    }
    for job in JOBS {
        if !role.is_captain() || !jobs.contains(&job) {
            captain::disarm_singleton_job(job);
        }
    }
    with(|s| {
        if s.role.as_ref() != Some(&role) {
            log_role(&role, &jobs, host, location.is_some());
        }
        s.role = Some(role.clone());
        s.config = Some(config);
        // A job switched off stops being published.
        s.produced.retain(|job, _| jobs.contains(&job.as_str()));
    });
    Pass {
        role,
        config,
        location,
    }
}

/// The store half of a pass: the captain publishes, a dispatcher reads; then
/// coverage and the pass's gauges. Blocking (the store read or write runs
/// here, outside the state lock).
fn exchange(root: &Path, pass: &Pass, now: DateTime<Utc>) {
    let Pass {
        role,
        config,
        location,
    } = pass;
    let jobs = config.jobs();
    // Take the publish cache out so the store call runs without the lock.
    // The read cache is copied, not taken: [`star_free`] and
    // [`captain_blocked`] are asked from other threads meanwhile.
    let (mut fetch, mut publish, produced, mut published, mut published_key, previous) =
        with(|s| {
            (
                s.fetch.clone(),
                std::mem::take(&mut s.publish),
                s.produced.clone(),
                s.last_published,
                s.published_key.clone(),
                s.covered.clone(),
            )
        });
    let mut covered = Coverage::new();
    match role {
        Role::Captain { captain } => {
            fetch = FetchCache::default();
            let key = content_key(&produced);
            let last = published.zip(published_key.as_deref());
            let due = publish_due(last, &key, now, effective_publish_interval(config, &produced));
            if let Some((loc, base)) = location.as_ref().filter(|_| due && !produced.is_empty()) {
                let hb = Heartbeat::new(captain, now, produced.clone());
                match store::publish_heartbeat(root, loc, base, &hb, &mut publish) {
                    Ok(()) => {
                        published = Some(now);
                        published_key = Some(key);
                    }
                    Err(e) => log::warn!("captain gauges: could not publish the heartbeat: {e:#}"),
                }
            }
        }
        Role::Dispatcher { captain } => {
            if let Some((loc, _)) = location {
                match store::fetch_heartbeat(root, loc, &mut fetch) {
                    Fetched::Failed(why) => {
                        log::debug!("captain gauges: heartbeat read failed: {why}");
                    }
                    Fetched::Absent => log::debug!("captain gauges: no heartbeat published"),
                    Fetched::Updated | Fetched::NotModified => {}
                }
            }
            covered = coverage(fetch.heartbeat.as_ref(), captain, now, config);
            covered.retain(|job, _| jobs.contains(&job.as_str()));
            log_fallback(&jobs, &previous, &covered);
        }
        Role::Local { .. } => fetch = FetchCache::default(),
    }
    let out = points(&jobs, role, &produced, fetch.heartbeat.as_ref(), &covered, now);
    with(|s| {
        s.covered = covered;
        s.fetch = fetch;
        s.publish = publish;
        s.last_published = published;
        s.published_key = published_key;
    });
    if !out.is_empty() {
        super::ops::emit_metrics(out);
    }
}

fn log_role(role: &Role, jobs: &[&str], host: &str, store: bool) {
    match role {
        Role::Captain { .. } if !store => log::warn!(
            "captain gauges: this host ({host}) is the fleet captain with \
             fleet.captainGauges.enabled, but no fleet store (fleet.repo) is configured: no \
             heartbeat is published, so every dispatcher keeps producing {jobs:?} locally"
        ),
        Role::Captain { .. } => log::info!(
            "captain gauges: this host ({host}) is the fleet captain — it produces {jobs:?} for \
             the fleet and publishes their freshness to the fleet store"
        ),
        Role::Dispatcher { captain } => log::info!(
            "captain gauges: the fleet captain is {captain}; this host ({host}) stops producing \
             {jobs:?} for the repos the captain covers while its heartbeat is fresh"
        ),
        Role::Local { reason } => {
            log::info!("captain gauges: this host ({host}) produces {jobs:?} locally ({reason})")
        }
    }
}

/// Log a job moving between standing down and falling back, once per change.
fn log_fallback(jobs: &[&str], previous: &Coverage, now: &Coverage) {
    for job in jobs.iter().copied() {
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
