//! Fleet discovery (#12306, #12311; slices of #12304): what work exists, read
//! once per step tick into one [`TickSnapshot`].
//!
//! One interface, a config-chosen chain of tiers ([`chain`]):
//!
//! - **Mirror** (`forge.mirror.discovery = "worklist"`): one
//!   `GET /v1/mirror/worklist` per tick, conditional on the last ETag; a
//!   `304` reuses the one in-memory `(etag, body)` ([`mirror`]). A repo the
//!   worklist answers with `status: ok` costs no forge read.
//! - **Gateway / `gh`** (the fleet's fallback tiers, and a standalone
//!   install's only tier): the direct backend's per-repo listings
//!   ([`direct`]), ETag-cached. On the fleet they are outage modes and
//!   **capped** ([`fallback`]): a repo the worklist cannot answer for (the
//!   request failed, or its `status` is not `ok`) is listed at the backstop
//!   pace, not every tick; between listings the tick draws from the last
//!   one. A gateway reserve refusal ends that tick's discovery without
//!   tripping the rate-limit breaker.
//! - **None**: every configured tier failed for a repo; no draw from it this
//!   tick.
//!
//! There is never a writer tier. The step gather reads only the tick's
//! snapshot ([`TickSnapshot::repo`]), which is a plain value
//! ([`RepoSnapshot`]) other readers can consume later (#12307). Claim,
//! lease, label-transition and merge reads stay live
//! (`forge_events/poll_gate.rs` invariant 3): this module never serves them.

pub mod chain;
pub mod direct;
pub mod fallback;
pub mod mirror;
pub mod snapshot;
pub mod worklist;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};

pub use chain::{BackendKind, Chain, Mode, Tier};
pub use snapshot::{
    Checks, ItemExtras, RepoSnapshot, RepoStatus, SnapshotStats, Source, WatermarkView,
};

use fallback::{ListFailure, Served};

/// One repo a tick reads: what Direct needs to list it.
#[derive(Debug, Clone)]
pub struct Target {
    /// The forge slug (`owner/name`).
    pub slug: String,
    pub gh_bin: PathBuf,
    pub root_dir: PathBuf,
}

/// Lists one repo through the direct backend: `(target, reads counter,
/// per-call mirror leg allowed, tier to record)`.
type Lister =
    dyn Fn(&Target, &AtomicU64, bool, Tier) -> Result<RepoSnapshot> + Send + Sync + 'static;

/// The discovery state across ticks: the Mirror tier's last worklist and
/// ETag, and the fallback tiers' last listings. Both in memory only, owned
/// by the step driver, dropped on restart.
pub struct Discovery {
    mirror: mirror::MirrorBackend,
    fallback: Arc<Mutex<fallback::Fallback>>,
}

impl Default for Discovery {
    fn default() -> Self {
        Self::with_backstop(crate::work_finder::resolve_interval())
    }
}

impl Discovery {
    /// A discovery whose fallback tiers list a repo at most once per
    /// `backstop` (the work finder's base cadence).
    #[must_use]
    pub fn with_backstop(backstop: std::time::Duration) -> Self {
        Self {
            mirror: mirror::MirrorBackend::default(),
            fallback: Arc::new(Mutex::new(fallback::Fallback::new(backstop))),
        }
    }

    /// Start a tick's snapshot from `chain`. The Mirror tier makes its one
    /// worklist request now; the direct backend reads each repo when the
    /// gather asks for it (its listings keep the gather's per-repo read
    /// deadline and concurrency).
    pub fn begin_tick(&mut self, chain: Chain, now: DateTime<Utc>) -> TickSnapshot {
        let state = match chain.backend() {
            BackendKind::Direct => State::Direct,
            BackendKind::Mirror => State::Mirror(self.mirror.fetch(now)),
        };
        self.snapshot(chain, state, now)
    }

    /// Like [`Self::begin_tick`] for a worklist chain with the request
    /// given, so tests need no network.
    pub fn begin_mirror_tick_with(
        &mut self,
        chain: Chain,
        now: DateTime<Utc>,
        fetch: impl FnOnce(Option<&str>) -> mirror::Fetched,
    ) -> TickSnapshot {
        let state = State::Mirror(self.mirror.fetch_with(now, fetch));
        self.snapshot(chain, state, now)
    }

    fn snapshot(&self, chain: Chain, state: State, now: DateTime<Utc>) -> TickSnapshot {
        TickSnapshot {
            state,
            chain,
            now,
            repos: Mutex::new(BTreeMap::new()),
            notes: Mutex::new(Notes::default()),
            direct_reads: AtomicU64::new(0),
            reserve_hit: AtomicBool::new(false),
            fallback: self.fallback.clone(),
            lister: Box::new(direct::read_repo),
        }
    }
}

enum State {
    Direct,
    Mirror(mirror::TickWorklist),
}

/// What the tick's span records, gathered as repos are read.
#[derive(Default)]
struct Notes {
    /// Per repo read: the tier that answered.
    tiers: BTreeMap<String, Tier>,
    /// Per repo read: what the worklist said (Mirror only).
    statuses: BTreeMap<String, RepoStatus>,
    /// Repos listed through a fallback tier this tick.
    listed: Vec<String>,
    /// Why a repo left the first tier: the first reserve or breaker refusal
    /// if any, else the first miss.
    reason: Option<&'static str>,
}

impl Notes {
    fn reason(&mut self, reason: &'static str) {
        let strong = matches!(reason, "reserve" | "breaker");
        let held_strong = matches!(self.reason, Some("reserve" | "breaker"));
        if self.reason.is_none() || (strong && !held_strong) {
            self.reason = Some(reason);
        }
    }
}

/// The one snapshot of a step tick (module docs).
pub struct TickSnapshot {
    state: State,
    chain: Chain,
    now: DateTime<Utc>,
    repos: Mutex<BTreeMap<String, Arc<RepoSnapshot>>>,
    notes: Mutex<Notes>,
    direct_reads: AtomicU64,
    /// The gateway refused a listing for its reserve: no more listings this
    /// tick.
    reserve_hit: AtomicBool,
    fallback: Arc<Mutex<fallback::Fallback>>,
    lister: Box<Lister>,
}

impl TickSnapshot {
    /// Replace the direct listing, for tests.
    #[cfg(test)]
    fn with_lister(
        mut self,
        lister: impl Fn(&Target, &AtomicU64, bool, Tier) -> Result<RepoSnapshot> + Send + Sync + 'static,
    ) -> Self {
        self.lister = Box::new(lister);
        self
    }

    #[must_use]
    pub fn kind(&self) -> BackendKind {
        match self.state {
            State::Direct => BackendKind::Direct,
            State::Mirror(_) => BackendKind::Mirror,
        }
    }

    /// One repo's part of the snapshot.
    ///
    /// # Errors
    ///
    /// Direct: the item listing's (all-or-nothing). Worklist chain: every
    /// tier failed for this repo (tier `none`): the caller draws nothing
    /// from it this tick.
    pub fn repo(&self, target: &Target) -> Result<Arc<RepoSnapshot>> {
        let key = target.slug.to_ascii_lowercase();
        if let Some(hit) = self.lock(&self.repos).get(&key) {
            return Ok(hit.clone());
        }
        // Not under a lock: listings are forge reads, and several repos are
        // gathered at once (#12256).
        let read = match &self.state {
            State::Direct => self.read_direct(target, &key),
            State::Mirror(w) => match w.repo(&target.slug, self.now) {
                Ok(repo) => {
                    let mut notes = self.lock(&self.notes);
                    notes.tiers.insert(key.clone(), Tier::Mirror);
                    notes.statuses.insert(key.clone(), RepoStatus::Ok);
                    Ok(repo)
                }
                Err(miss) => self.read_fallback(target, &key, &miss),
            },
        }?;
        self.lock(&self.repos).insert(key, read.clone());
        Ok(read)
    }

    fn read_direct(&self, target: &Target, key: &str) -> Result<Arc<RepoSnapshot>> {
        let listed = (self.lister)(
            target,
            &self.direct_reads,
            self.chain.per_call_mirror(),
            self.chain.listing_tier(),
        );
        let mut notes = self.lock(&self.notes);
        match listed {
            Ok(repo) => {
                notes.tiers.insert(key.to_string(), repo.tier);
                Ok(Arc::new(repo))
            }
            Err(e) => {
                notes.tiers.insert(key.to_string(), Tier::None);
                Err(e)
            }
        }
    }

    /// The worklist could not answer for this repo: the next tier, capped.
    fn read_fallback(
        &self,
        target: &Target,
        key: &str,
        miss: &mirror::Miss,
    ) -> Result<Arc<RepoSnapshot>> {
        {
            let mut notes = self.lock(&self.notes);
            notes.statuses.insert(key.to_string(), miss.status());
            notes.reason(miss.reason());
        }
        let served = self.serve_fallback(target, key);
        let mut notes = self.lock(&self.notes);
        match served {
            Served::Listed(repo) => {
                notes.tiers.insert(key.to_string(), repo.tier);
                notes.listed.push(key.to_string());
                Ok(repo)
            }
            Served::Paced(repo) => {
                notes.tiers.insert(key.to_string(), repo.tier);
                Ok(repo)
            }
            Served::Stale(repo, failure) => {
                notes.tiers.insert(key.to_string(), repo.tier);
                notes.reason(failure.reason);
                Ok(repo)
            }
            Served::Failed(failure) => {
                notes.tiers.insert(key.to_string(), Tier::None);
                notes.reason(failure.reason);
                Err(anyhow!(
                    "discovery: no tier answered for {} (worklist: {}; fallback: {}: {})",
                    target.slug,
                    miss.reason(),
                    failure.reason,
                    failure.detail
                ))
            }
        }
    }

    fn serve_fallback(&self, target: &Target, key: &str) -> Served {
        let now = Instant::now();
        if self.reserve_hit.load(Ordering::Relaxed) {
            return self.lock(&self.fallback).without_listing(
                key,
                ListFailure {
                    reason: "reserve",
                    detail: "discovery ended for the tick".into(),
                },
            );
        }
        if let Some(served) = self.lock(&self.fallback).paced(key, now) {
            return served;
        }
        let listed = self.list_fallback(target);
        self.lock(&self.fallback).finish(key, now, listed)
    }

    /// One fallback listing, unless the rate-limit breaker is open.
    fn list_fallback(&self, target: &Target) -> Result<RepoSnapshot, ListFailure> {
        if crate::rate_limit_breaker::global_skip_pass("discovery_fallback") {
            return Err(ListFailure {
                reason: "breaker",
                detail: "the rate-limit breaker is open".into(),
            });
        }
        (self.lister)(target, &self.direct_reads, false, self.chain.listing_tier()).map_err(|e| {
            // A reserve refusal is its own outcome: it ends the tick's
            // discovery and the breaker is left alone (#12310).
            let reserve = crate::gh_invocation::purpose::discovery_ended();
            if reserve {
                self.reserve_hit.store(true, Ordering::Relaxed);
            }
            ListFailure {
                reason: if reserve { "reserve" } else { "listing" },
                detail: format!("{e:#}"),
            }
        })
    }

    /// Every repo read so far this tick, for other readers (#12307).
    #[must_use]
    pub fn repos(&self) -> Vec<Arc<RepoSnapshot>> {
        self.lock(&self.repos).values().cloned().collect()
    }

    /// What the tick span records.
    #[must_use]
    pub fn stats(&self) -> SnapshotStats {
        let repos = self.repos();
        let notes = self.lock(&self.notes);
        let worst = notes.tiers.values().copied().max();
        let mut stats = SnapshotStats {
            backend: self.kind().as_str(),
            tier: worst
                .unwrap_or(match &self.state {
                    State::Direct => self.chain.listing_tier(),
                    State::Mirror(w) if w.error_class().is_none() => Tier::Mirror,
                    State::Mirror(_) => Tier::None,
                })
                .as_str(),
            repos: notes.tiers.len(),
            items: repos.iter().map(|r| r.rows.len()).sum(),
            discovery_forge_reads: self.direct_reads.load(Ordering::Relaxed),
            ..SnapshotStats::default()
        };
        if let State::Mirror(w) = &self.state {
            stats.fallback_reason = notes.reason.map(str::to_string);
            stats.fallback_repos = notes.listed.clone();
            stats.worklist_outcome = Some(w.outcome());
            stats.worklist_error_class = w.error_class();
            stats.worklist_latency_ms = Some(w.latency_ms);
            stats.not_modified = Some(w.not_modified);
            for status in notes.statuses.values() {
                match status {
                    RepoStatus::Ok => stats.repos_ok += 1,
                    RepoStatus::Unknown => stats.repos_unknown += 1,
                    RepoStatus::Backfilling => stats.repos_backfilling += 1,
                    RepoStatus::Error => stats.repos_error += 1,
                }
            }
            stats.oldest_watermark_age_secs = w.oldest_watermark_age_secs(self.now);
            stats.stale_repos = w.stale_repos(self.now);
        }
        stats
    }

    fn lock<'a, T>(&self, m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
