//! One read-routing entry point (W4-B): [`route_read`].
//!
//! Before W4-B three walks chose a reader independently —
//! `reader_for_at`, `read_credential_in` and
//! `forge_read_pool::select_for_repo_at` — each `hash(owner/repo) mod N`
//! then forward past withdrawn readers. That kept every repo on one reader,
//! so one hot repo could drain one reader's bucket while the other sat idle.
//! [`route_read`] keeps that home placement and adds two things:
//!
//! - **Split** (`forge.readPool.routing.splitRepos`): for a configured hot
//!   repo the start reader is chosen per *request*,
//!   [`forge_read_pool::split_index`] of the request's affinity key, so the
//!   repo's reads spread across the pool while each URL stays on one reader.
//!   GitHub ETags are credential-specific, so a URL that moved readers would
//!   lose its `304`; a deterministic per-URL placement never moves one.
//! - **Spill latch**: when a request's home bucket is projected to run dry
//!   ([`crate::forge_bucket_book::projected_used_pct`]) or is withdrawn, part
//!   (≥ `spillProjectedPct`) or all (≥ `spillFullPct`, or withdrawn) of its
//!   requests move to the next reader that has headroom
//!   (< `targetMaxPct`, or unknown). The latch holds until the home bucket's
//!   reset, so a URL moves at most once away and once back per window. An
//!   unknown home reading never engages it, and no target with headroom
//!   means the request stays home.
//!
//! With no `splitRepos` and no home reading at or above `spillProjectedPct`
//! the choice is byte-identical to the pre-W4-B walk.
//! `LOOM_READ_ROUTING=legacy` restores that walk exactly (no owners filter,
//! no scoped withdrawal, no egress check — `read_credential` never had one),
//! `LOOM_READ_POOL_SPILL=0` disables the split and the latch, and
//! `LOOM_READ_POOL_SPLIT=0` the split alone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{Identity, Roster, RoutingMode};
use crate::forge_bucket_book::{self, BucketKey, Resource};
use crate::forge_read_pool;

/// Env override: `0` keeps every read on its home reader (no split, no
/// spill latch).
pub const SPILL_ENV: &str = "LOOM_READ_POOL_SPILL";
/// Env override: `0` disables the per-request split only.
pub const SPLIT_ENV: &str = "LOOM_READ_POOL_SPLIT";

/// The config block this module reads.
pub const ROUTING_KEY: &str = "forge.readPool.routing";

/// The longest a latch is held: one GitHub window plus a minute of skew.
pub const MAX_LATCH: Duration = Duration::from_secs(3660);
/// A latch with no known home reset releases this long after it engaged.
pub const DEFAULT_LATCH: Duration = Duration::from_secs(3600);

/// How a read may be treated when its readers run dry. W4-B records it on
/// the request only; W4-C acts on it (shedding is opt-in per site, and
/// [`ReadClass::Gate`] — the default — keeps today's writer fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadClass {
    /// Gates a decision (dispatch, claim, merge, reap): never shed.
    #[default]
    Gate,
    /// Housekeeping that may be deferred.
    Hygiene,
    /// Telemetry that may be deferred.
    Observability,
}

/// Why a reader was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The repo's hashed reader ([`forge_read_pool::assignment_index`]).
    Home,
    /// The request's split reader ([`forge_read_pool::split_index`]).
    Split,
    /// Moved off its placement: by the spill latch, or past a withdrawn or
    /// stale placement.
    Spill,
}

impl Placement {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Split => "split",
            Self::Spill => "spill",
        }
    }
}

/// One read to route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteRequest<'a> {
    /// The repository read, `owner/repo`.
    pub owner_repo: &'a str,
    /// The forge host; anything but `github.com` has no reader pool.
    pub host: Option<&'a str>,
    /// The bucket the read spends.
    pub resource: Resource,
    /// The request's identity ([`crate::gh_invocation::affinity_key`], or a
    /// conditional read's URL). `None` keeps the home placement.
    pub affinity_key: Option<&'a str>,
    pub class: ReadClass,
}

impl<'a> RouteRequest<'a> {
    /// A [`ReadClass::Gate`] read with no affinity key (the legacy wrappers).
    #[must_use]
    pub fn gate(owner_repo: &'a str, host: Option<&'a str>, resource: Resource) -> Self {
        Self {
            owner_repo,
            host,
            resource,
            affinity_key: None,
            class: ReadClass::Gate,
        }
    }

    /// With `key` as the affinity key.
    #[must_use]
    pub fn affinity(mut self, key: Option<&'a str>) -> Self {
        self.affinity_key = key;
        self
    }
}

/// Where a read goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// No reader pool for this read: no readers, a non-github host, no
    /// workspace, or (v2 only) the egress gateway owns the pool. The caller
    /// uses the writer exactly as before.
    NoPool,
    /// Serve it under this reader.
    Reader {
        dir: PathBuf,
        app_id: String,
        placement: Placement,
    },
    /// Readers exist, but each is withdrawn or holds a stale token for this
    /// owner and resource; `until` is the earliest any is expected back.
    Exhausted { until: SystemTime },
}

impl RouteDecision {
    /// `(GH_CONFIG_DIR, app id)` for a reader, else `None` — what the
    /// pre-W4-B `read_credential` returned (a [`ReadClass::Gate`] caller
    /// sends both [`RouteDecision::NoPool`] and
    /// [`RouteDecision::Exhausted`] to the writer).
    #[must_use]
    pub fn into_credential(self) -> Option<(PathBuf, String)> {
        match self {
            Self::Reader { dir, app_id, .. } => Some((dir, app_id)),
            Self::NoPool | Self::Exhausted { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// `forge.readPool.routing`, validated.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingConfig {
    /// The spill latch on/off.
    pub spill: bool,
    /// Repos (lowercased `owner/repo`) whose requests are split.
    pub split_repos: Vec<String>,
    pub spill_projected_pct: f64,
    pub spill_full_pct: f64,
    pub target_max_pct: f64,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            spill: true,
            split_repos: Vec::new(),
            spill_projected_pct: 70.0,
            spill_full_pct: 90.0,
            target_max_pct: 60.0,
        }
    }
}

impl RoutingConfig {
    /// Parse `forge.readPool.routing` from the effective config, with the
    /// problems found. Thresholds must satisfy `0 < targetMaxPct <
    /// spillProjectedPct < spillFullPct ≤ 100`; an invalid set falls back to
    /// the defaults (all three, so a half-applied set never mixes).
    #[must_use]
    pub fn parse(effective: &Value) -> (Self, Vec<String>) {
        let mut cfg = Self::default();
        let mut warnings = Vec::new();
        let Some(block) = crate::config_resolver::get_path(effective, ROUTING_KEY) else {
            return (cfg, warnings);
        };
        if !block.is_object() {
            warnings.push(format!("{ROUTING_KEY} must be an object; using the defaults"));
            return (cfg, warnings);
        }
        match block.get("spill") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(b)) => cfg.spill = *b,
            Some(other) => warnings.push(format!(
                "{ROUTING_KEY}.spill must be true or false, not {other}; using true"
            )),
        }
        match block.get("splitRepos") {
            None | Some(Value::Null) => {}
            Some(Value::Array(list)) => {
                for entry in list {
                    match entry.as_str().map(str::trim).filter(|s| is_owner_repo(s)) {
                        Some(r) => {
                            let lc = r.to_ascii_lowercase();
                            if !cfg.split_repos.contains(&lc) {
                                cfg.split_repos.push(lc);
                            }
                        }
                        None => warnings.push(format!(
                            "{ROUTING_KEY}.splitRepos entry {entry} is not owner/repo; dropped"
                        )),
                    }
                }
            }
            Some(other) => warnings.push(format!(
                "{ROUTING_KEY}.splitRepos must be an array of owner/repo, not {other}; no repo is split"
            )),
        }
        let pct = |key: &str, default: f64| -> Result<f64, String> {
            match block.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(v) => v
                    .as_f64()
                    .filter(|f| f.is_finite())
                    .ok_or_else(|| format!("{ROUTING_KEY}.{key} must be a number, not {v}")),
            }
        };
        let d = Self::default();
        match (
            pct("spillProjectedPct", d.spill_projected_pct),
            pct("spillFullPct", d.spill_full_pct),
            pct("targetMaxPct", d.target_max_pct),
        ) {
            (Ok(projected), Ok(full), Ok(target))
                if 0.0 < target && target < projected && projected < full && full <= 100.0 =>
            {
                cfg.spill_projected_pct = projected;
                cfg.spill_full_pct = full;
                cfg.target_max_pct = target;
            }
            (Ok(projected), Ok(full), Ok(target)) => warnings.push(format!(
                "{ROUTING_KEY}: need 0 < targetMaxPct ({target}) < spillProjectedPct ({projected}) < spillFullPct ({full}) <= 100; using the defaults 60/70/90"
            )),
            (a, b, c) => {
                for e in [a.err(), b.err(), c.err()].into_iter().flatten() {
                    warnings.push(format!("{e}; using the defaults 60/70/90"));
                }
            }
        }
        (cfg, warnings)
    }

    /// Apply the env overrides: [`SPILL_ENV`]`=0` turns off the latch and
    /// the split, [`SPLIT_ENV`]`=0` the split.
    #[must_use]
    pub fn with_env(mut self, spill_env: Option<&str>, split_env: Option<&str>) -> Self {
        if spill_env.is_some_and(env_off) {
            self.spill = false;
            self.split_repos.clear();
        }
        if split_env.is_some_and(env_off) {
            self.split_repos.clear();
        }
        self
    }

    /// Whether `owner_repo`'s requests are split.
    #[must_use]
    pub fn splits(&self, owner_repo: &str) -> bool {
        self.split_repos
            .iter()
            .any(|r| r.eq_ignore_ascii_case(owner_repo))
    }
}

/// `0`, `false`, `off` or `no` (any case).
fn env_off(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
}

fn is_owner_repo(s: &str) -> bool {
    s.split_once('/').is_some_and(|(o, r)| {
        forge_bucket_book::valid_owner(o)
            && !r.is_empty()
            && !r.contains('/')
            && r.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    })
}

/// The `forge.readPool.routing` problems [`super::config_warnings`] reports.
#[must_use]
pub fn routing_config_warnings(effective: &Value) -> Vec<String> {
    RoutingConfig::parse(effective).1
}

/// The routing config for `root`, re-read at most every
/// [`super::ROSTER_TTL`]; the env overrides are applied on every call.
#[must_use]
pub fn cached_routing(root: &Path) -> RoutingConfig {
    static CACHE: OnceLock<Mutex<Option<(Instant, PathBuf, RoutingConfig)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let hit = cache.lock().ok().and_then(|g| {
        g.as_ref()
            .filter(|(at, r, _)| r == root && at.elapsed() < super::ROSTER_TTL)
            .map(|(_, _, c)| c.clone())
    });
    let cfg = hit.unwrap_or_else(|| {
        let effective = crate::config_resolver::resolve_effective_config(root);
        let (cfg, warnings) = RoutingConfig::parse(&effective);
        for w in &warnings {
            log::warn!("forge_identity: {w} (W4-B)");
        }
        if let Ok(mut g) = cache.lock() {
            *g = Some((Instant::now(), root.to_path_buf(), cfg.clone()));
        }
        cfg
    });
    cfg.with_env(
        std::env::var(SPILL_ENV).ok().as_deref(),
        std::env::var(SPLIT_ENV).ok().as_deref(),
    )
}

// ---------------------------------------------------------------------------
// The spill latch
// ---------------------------------------------------------------------------

/// How much of a repo's traffic a latch moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LatchMode {
    /// The requests whose spill hash is 1 (about half).
    Partial,
    /// Every request.
    Full,
}

impl LatchMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Full => "full",
        }
    }
}

/// One engaged latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Latch {
    pub mode: LatchMode,
    /// When it (last) engaged or escalated.
    pub entered_at: SystemTime,
    /// When it releases, whatever the readings say meanwhile.
    pub release_at: SystemTime,
}

/// What the latch sees of a home bucket at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HomeState {
    /// [`forge_bucket_book::projected_used_pct`]; `None` = unknown.
    pub projected_pct: Option<f64>,
    /// The bucket's reset, when a reading is believed.
    pub reset: Option<SystemTime>,
    /// The end of a live scoped withdrawal of home for this owner/resource.
    pub withdrawn_until: Option<SystemTime>,
}

/// The release instant of a latch engaged at `entry`: the home bucket's
/// reset — or, if later, the withdrawal's end — capped at `entry +`
/// [`MAX_LATCH`]; with no known reset, `entry +` [`DEFAULT_LATCH`].
///
/// A withdrawal's own end counts only beside a known reset: a withdrawal
/// with no reading behind it (a secondary limit, a refused credential)
/// says nothing about when the bucket refills, so the latch holds for the
/// default window rather than snapping back after a 60 s back-off.
#[must_use]
pub fn release_at(
    entry: SystemTime,
    reset: Option<SystemTime>,
    withdrawn_until: Option<SystemTime>,
) -> SystemTime {
    match reset {
        Some(r) => r.max(withdrawn_until.unwrap_or(r)).min(entry + MAX_LATCH),
        None => entry + DEFAULT_LATCH,
    }
}

/// One latch transition, as the `forge.reader.spill` span reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Engaged(LatchMode),
    Released,
}

impl Transition {
    #[must_use]
    pub fn mode_str(self) -> &'static str {
        match self {
            Self::Engaged(m) => m.as_str(),
            Self::Released => "off",
        }
    }
}

/// The pure latch step: the latch after observing `home` at `now`, and the
/// transition taken, if any. Only ever escalates (off → partial → full)
/// until `release_at`; an unknown reading with no withdrawal never engages.
#[must_use]
pub fn step(
    held: Option<Latch>,
    home: &HomeState,
    cfg: &RoutingConfig,
    now: SystemTime,
) -> (Option<Latch>, Vec<Transition>) {
    let mut transitions = Vec::new();
    let held = match held {
        Some(l) if now >= l.release_at => {
            transitions.push(Transition::Released);
            None
        }
        other => other,
    };
    let wanted = if home.withdrawn_until.is_some()
        || home.projected_pct.is_some_and(|p| p >= cfg.spill_full_pct)
    {
        Some(LatchMode::Full)
    } else if home
        .projected_pct
        .is_some_and(|p| p >= cfg.spill_projected_pct)
    {
        Some(LatchMode::Partial)
    } else {
        None
    };
    let next = match (held, wanted) {
        (held, None) => held,
        (Some(l), Some(m)) if m <= l.mode => Some(l),
        (held, Some(m)) => {
            let release = release_at(now, home.reset, home.withdrawn_until)
                .max(held.map_or(now, |l| l.release_at));
            transitions.push(Transition::Engaged(m));
            Some(Latch {
                mode: m,
                entered_at: now,
                release_at: release,
            })
        }
    };
    (next, transitions)
}

/// `(owner/repo lowercased, resource, home app id)`: per home reader, so a
/// split repo's two homes latch independently (an unsplit repo has one).
type LatchKey = (String, Resource, String);

fn latches() -> &'static Mutex<HashMap<LatchKey, Latch>> {
    static LATCHES: OnceLock<Mutex<HashMap<LatchKey, Latch>>> = OnceLock::new();
    LATCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Advance the latch for `(owner_repo, resource, home)` and return it with
/// the transitions taken.
fn advance(
    owner_repo: &str,
    resource: Resource,
    home_app: &str,
    home: &HomeState,
    cfg: &RoutingConfig,
    now: SystemTime,
) -> (Option<Latch>, Vec<Transition>) {
    let Ok(mut map) = latches().lock() else {
        return (None, Vec::new()); // a poisoned lock must not move reads
    };
    let key = (owner_repo.to_ascii_lowercase(), resource, home_app.to_string());
    let (next, transitions) = step(map.get(&key).copied(), home, cfg, now);
    match next {
        Some(l) => {
            map.insert(key, l);
        }
        None => {
            map.remove(&key);
        }
    }
    (next, transitions)
}

/// Every latch engaged at `now`: `(owner/repo, resource, home app, latch)`.
#[must_use]
pub fn live_latches(now: SystemTime) -> Vec<(String, Resource, String, Latch)> {
    let Ok(map) = latches().lock() else {
        return Vec::new();
    };
    let mut out: Vec<_> = map
        .iter()
        .filter(|(_, l)| now < l.release_at)
        .map(|((repo, res, app), l)| (repo.clone(), *res, app.clone(), *l))
        .collect();
    out.sort_by(|a, b| (&a.0, a.1, &a.2).cmp(&(&b.0, b.1, &b.2)));
    out
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// The readers that may serve `owner` (W4-B `owners` filter), in roster
/// order. `n` for every hash is this list's length, so a reader limited to
/// other owners never shifts this owner's placement.
#[must_use]
pub fn eligible_readers<'a>(roster: &'a Roster, owner: &str) -> Vec<&'a Identity> {
    roster
        .readers
        .iter()
        .filter(|r| r.serves_owner(owner))
        .collect()
}

/// What [`route_read_in`] needs beyond the roster.
#[derive(Debug, Clone, Copy)]
pub struct RouteEnv<'a> {
    pub mode: RoutingMode,
    /// `forge_egress::publication::github_credential_forbidden` (honoured
    /// in v2 only).
    pub egress_forbidden: bool,
    pub cfg: &'a RoutingConfig,
}

/// Route one read for the daemon's workspace (see the module docs).
#[must_use]
pub fn route_read(req: &RouteRequest<'_>, now: SystemTime) -> RouteDecision {
    if !is_github(req.host) {
        return RouteDecision::NoPool;
    }
    let Some(ws) = super::workspace_root() else {
        return RouteDecision::NoPool;
    };
    let mode = RoutingMode::current();
    let egress_forbidden = mode == RoutingMode::Scoped
        && crate::forge_egress::publication::github_credential_forbidden(Some(ws));
    let cfg = cached_routing(ws);
    route_read_in(
        ws,
        &super::cached(ws),
        req,
        &RouteEnv {
            mode,
            egress_forbidden,
            cfg: &cfg,
        },
        now,
    )
}

fn is_github(host: Option<&str>) -> bool {
    host.is_none_or(|h| h.eq_ignore_ascii_case("github.com"))
}

/// [`route_read`] against an explicit workspace, roster and environment.
#[must_use]
pub fn route_read_in(
    workspace_root: &Path,
    roster: &Roster,
    req: &RouteRequest<'_>,
    env: &RouteEnv<'_>,
    now: SystemTime,
) -> RouteDecision {
    if !is_github(req.host) {
        return RouteDecision::NoPool;
    }
    let legacy = env.mode == RoutingMode::Legacy;
    if !legacy && env.egress_forbidden {
        return RouteDecision::NoPool;
    }
    let owner = crate::credential_preflight::owner_of_nwo(req.owner_repo);
    if owner.is_empty() {
        return RouteDecision::NoPool;
    }
    // Legacy: the pre-W4 walk exactly — every reader, no owners filter.
    let readers: Vec<&Identity> = if legacy {
        roster.readers.iter().collect()
    } else {
        eligible_readers(roster, owner)
    };
    let n = readers.len();
    let Some(home) = forge_read_pool::assignment_index(req.owner_repo, n) else {
        return RouteDecision::NoPool;
    };
    let eligible = |r: &Identity| {
        !forge_read_pool::is_withdrawn_at(&r.app_id, now)
            && !super::repo_withdrawn_at(&r.app_id, req.owner_repo, now)
            && (legacy
                || !forge_read_pool::is_withdrawn_scoped_at(&r.app_id, owner, req.resource, now))
    };
    let usable = |r: &Identity| -> Option<PathBuf> {
        if !eligible(r) {
            return None;
        }
        let dir = super::reader_dir(workspace_root, owner, r);
        super::dir_is_fresh(&dir, now).then_some(dir)
    };
    let pick = |r: &Identity, dir: PathBuf, placement| RouteDecision::Reader {
        dir,
        app_id: r.app_id.clone(),
        placement,
    };

    let (start, placement) = match req.affinity_key {
        Some(key) if !legacy && env.cfg.splits(req.owner_repo) => (
            forge_read_pool::split_index(req.owner_repo, key, n).unwrap_or(home),
            Placement::Split,
        ),
        _ => (home, Placement::Home),
    };

    if !legacy && env.cfg.spill && n > 1 {
        let nominal = readers[start];
        let now_epoch = epoch(now);
        let key = bucket_key(&nominal.app_id, owner, req.resource);
        let reading = key
            .as_ref()
            .and_then(|k| forge_bucket_book::reading(k, now_epoch));
        let state = HomeState {
            projected_pct: reading
                .as_ref()
                .and_then(|r| forge_bucket_book::projected_pct_of(r, now_epoch)),
            reset: reading.as_ref().and_then(|r| system_time(r.reset_epoch)),
            withdrawn_until: forge_read_pool::scoped_withdrawal_until(
                &nominal.app_id,
                owner,
                req.resource,
                now,
            ),
        };
        let (latch, transitions) =
            advance(req.owner_repo, req.resource, &nominal.app_id, &state, env.cfg, now);
        // The first reader after home, in walk order, that is usable and
        // has headroom (projected below targetMaxPct, or unknown).
        let target = || {
            (1..n).find_map(|off| {
                let r = readers[(start + off) % n];
                let headroom = bucket_key(&r.app_id, owner, req.resource)
                    .and_then(|k| forge_bucket_book::projected_used_pct(&k, now_epoch))
                    .is_none_or(|p| p < env.cfg.target_max_pct);
                if !headroom {
                    return None;
                }
                usable(r).map(|dir| (r, dir))
            })
        };
        for t in &transitions {
            let to = match t {
                Transition::Released => None,
                Transition::Engaged(_) => target().map(|(r, _)| r.app_id.clone()),
            };
            crate::observability::ops::reader_spill::record_spill(
                &crate::observability::ops::reader_spill::Spill {
                    owner_repo: req.owner_repo,
                    resource: req.resource.as_str(),
                    from: &nominal.app_id,
                    to: to.as_deref().unwrap_or("home"),
                    mode: t.mode_str(),
                    until: latch.map_or(now, |l| l.release_at).into(),
                },
            );
        }
        let moves = latch.is_some_and(|l| match l.mode {
            LatchMode::Full => true,
            LatchMode::Partial => {
                req.affinity_key
                    .and_then(|k| forge_read_pool::spill_index(req.owner_repo, k, 2))
                    == Some(1)
            }
        });
        if moves {
            if let Some((r, dir)) = target() {
                return pick(r, dir, Placement::Spill);
            }
        }
    }

    for off in 0..n {
        let r = readers[(start + off) % n];
        if let Some(dir) = usable(r) {
            return pick(
                r,
                dir,
                if off == 0 {
                    placement
                } else {
                    Placement::Spill
                },
            );
        }
    }
    RouteDecision::Exhausted {
        until: exhausted_until(&readers, req, owner, legacy, now),
    }
}

/// The earliest instant any of `readers` may serve `req` again: the end of
/// its withdrawals, or the next token refresh for one that is only stale.
fn exhausted_until(
    readers: &[&Identity],
    req: &RouteRequest<'_>,
    owner: &str,
    legacy: bool,
    now: SystemTime,
) -> SystemTime {
    let refresh = now + crate::credential_preflight::GITHUB_APP_REFRESH_INTERVAL;
    readers
        .iter()
        .map(|r| {
            let ends = [
                forge_read_pool::withdrawn_until(&r.app_id, now),
                super::repo_withdrawn_until(&r.app_id, req.owner_repo, now),
                if legacy {
                    None
                } else {
                    forge_read_pool::scoped_withdrawal_until(&r.app_id, owner, req.resource, now)
                },
            ];
            ends.into_iter().flatten().max().unwrap_or(refresh)
        })
        .min()
        .unwrap_or(refresh)
}

/// The bucket a reader's reads of `owner`'s `resource` spend; `None` for an
/// app id that is not a plain number (no bucket is booked for it).
fn bucket_key(app_id: &str, owner: &str, resource: Resource) -> Option<BucketKey> {
    let label = crate::observability::ops::ratelimit::app_account_label(app_id);
    (label != "unknown").then(|| BucketKey::new(&label, owner, resource))
}

fn epoch(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn system_time(epoch: i64) -> Option<SystemTime> {
    u64::try_from(epoch)
        .ok()
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
