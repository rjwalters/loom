//! Fleet discovery (#12306, slice 2 of #12304): what work exists, read once
//! per step tick into one [`TickSnapshot`].
//!
//! One interface, two backends, chosen per host by
//! `autonomous.workFinder.discovery` ([`BackendKind::from_effective`]):
//!
//! - **Direct** (the default; standalone installs and Gitea): today's
//!   per-repo listings ([`crate::step_dispatch::listing`] and the open-PR
//!   listing, both ETag-cached), moved behind the interface unchanged
//!   ([`direct`]).
//! - **Mirror** (fleet): one `GET /v1/mirror/worklist` per tick, conditional
//!   on the last ETag; a `304` reuses the previous snapshot ([`mirror`]). It
//!   makes **no** per-repo forge listing and has **no** fallback to the forge
//!   for a stale repo: the repo is served from the snapshot, its watermark
//!   recorded in [`SnapshotStats`]. A failed request reuses the previous
//!   snapshot; with none, every repo reads as unavailable (fail closed).
//!
//! The step gather reads only the tick's snapshot ([`TickSnapshot::repo`]).
//! The snapshot is a plain value ([`RepoSnapshot`]) that other readers can
//! consume later (#12307). Claim, lease, label-transition and merge reads stay
//! live (`forge_events/poll_gate.rs` invariant 3): this module never serves
//! them.

pub mod direct;
pub mod mirror;
pub mod snapshot;
pub mod worklist;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::{DateTime, Utc};

pub use snapshot::{Checks, ItemExtras, RepoSnapshot, SnapshotStats, Source, WatermarkView};

/// The config key choosing the backend.
pub const CONFIG_KEY: &str = "autonomous.workFinder.discovery";

/// Which backend a host discovers through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// Per-repo listings with an ETag cache (the default).
    #[default]
    Direct,
    /// One worklist read per tick.
    Mirror,
}

impl BackendKind {
    /// `autonomous.workFinder.discovery` from an effective config:
    /// `"mirror"` or `"direct"`; anything else (including unset) is Direct.
    /// Pure.
    #[must_use]
    pub fn from_effective(effective: &serde_json::Value) -> Self {
        match crate::config_resolver::get_path(effective, CONFIG_KEY)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
        {
            Some(s) if s.eq_ignore_ascii_case("mirror") => Self::Mirror,
            _ => Self::Direct,
        }
    }

    /// The backend for the daemon rooted at `root`, read live.
    #[must_use]
    pub fn for_root(root: &std::path::Path) -> Self {
        Self::from_effective(&crate::config_resolver::resolve_effective_config(root))
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Mirror => "mirror",
        }
    }
}

/// One repo a tick reads: what Direct needs to list it.
#[derive(Debug, Clone)]
pub struct Target {
    /// The forge slug (`owner/name`).
    pub slug: String,
    pub gh_bin: PathBuf,
    pub root_dir: PathBuf,
}

/// The discovery state across ticks: the Mirror backend's last worklist and
/// ETag. Owned by the step driver.
#[derive(Default)]
pub struct Discovery {
    mirror: mirror::MirrorBackend,
}

impl Discovery {
    /// Start a tick's snapshot from `kind`. Mirror makes its one worklist
    /// request now; Direct reads each repo when the gather asks for it (its
    /// listings keep the gather's per-repo read deadline and concurrency).
    pub fn begin_tick(&mut self, kind: BackendKind, now: DateTime<Utc>) -> TickSnapshot {
        let state = match kind {
            BackendKind::Direct => State::Direct,
            BackendKind::Mirror => State::Mirror(self.mirror.fetch(now)),
        };
        TickSnapshot::new(state, now)
    }

    /// Like [`Self::begin_tick`] for Mirror with `fetch` given, so tests
    /// need no network.
    pub fn begin_mirror_tick_with(
        &mut self,
        now: DateTime<Utc>,
        fetch: impl FnOnce(Option<&str>) -> mirror::Fetched,
    ) -> TickSnapshot {
        TickSnapshot::new(State::Mirror(self.mirror.fetch_with(now, fetch)), now)
    }
}

enum State {
    Direct,
    Mirror(mirror::TickWorklist),
}

/// The one snapshot of a step tick (module docs).
pub struct TickSnapshot {
    state: State,
    now: DateTime<Utc>,
    repos: Mutex<BTreeMap<String, Arc<RepoSnapshot>>>,
    direct_reads: AtomicU64,
}

impl TickSnapshot {
    fn new(state: State, now: DateTime<Utc>) -> Self {
        Self {
            state,
            now,
            repos: Mutex::new(BTreeMap::new()),
            direct_reads: AtomicU64::new(0),
        }
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
    /// Direct: the item listing's (all-or-nothing). Mirror: no worklist
    /// answered this tick or before, or the key may not read this repo. The
    /// caller does not fall back to the forge for either.
    pub fn repo(&self, target: &Target) -> Result<Arc<RepoSnapshot>> {
        let key = target.slug.to_ascii_lowercase();
        if let Some(hit) = self.lock().get(&key) {
            return Ok(hit.clone());
        }
        // Not under the lock: Direct's listings are forge reads, and several
        // repos are gathered at once (#12256).
        let repo = match &self.state {
            State::Direct => Arc::new(direct::read_repo(target, &self.direct_reads)?),
            State::Mirror(w) => w.repo(&target.slug, self.now)?,
        };
        self.lock().insert(key, repo.clone());
        Ok(repo)
    }

    /// Every repo read so far this tick, for other readers (#12307).
    #[must_use]
    pub fn repos(&self) -> Vec<Arc<RepoSnapshot>> {
        self.lock().values().cloned().collect()
    }

    /// What the tick span records.
    #[must_use]
    pub fn stats(&self) -> SnapshotStats {
        let repos = self.repos();
        let mut stats = SnapshotStats {
            backend: self.kind().as_str(),
            repos: repos.len(),
            items: repos.iter().map(|r| r.rows.len()).sum(),
            discovery_forge_reads: self.direct_reads.load(Ordering::Relaxed),
            ..SnapshotStats::default()
        };
        if let State::Mirror(w) = &self.state {
            stats.worklist_latency_ms = Some(w.latency_ms);
            stats.not_modified = Some(w.not_modified);
            stats.reused_previous = w.reused_previous;
            stats.repos = w.repo_count();
            stats.items = w.item_count();
            stats.oldest_watermark_age_secs = w.oldest_watermark_age_secs(self.now);
            stats.stale_repos = w.stale_repos(self.now);
        }
        stats
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Arc<RepoSnapshot>>> {
        self.repos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
