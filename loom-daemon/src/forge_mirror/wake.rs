//! Wakes from the mirror's change stream (loom#10892; 2AMLogic/2am#4367 P5).
//!
//! While a mirror is live, the daemon pulls `GET /v1/changes` for the repos it
//! serves every `changePollSecs`, keeping its own cursor per repo (persisted
//! under `$HOME/.loom/forge-mirror/`). Each pull that returns a change becomes
//! one `forge.event` bus prompt ([`crate::forge_events::BUS_TOPIC`], `source`
//! [`MIRROR_SOURCE`]), so every loop whose `forgeEvents.events.*` consumer is
//! armed ticks early through the same [`crate::forge_events::wake`] seam the
//! per-host feed uses, under the same spacing floor and coalescing.
//!
//! ADR-0021's invariants hold as written:
//!
//! 1. **A change is a nudge, never a command.** The prompt carries routing
//!    hints only (`count`, webhook-style `types`, and the `repos` that changed
//!    as invalidation keys). No title, number, label or state reaches a
//!    decision: the loop it ticks re-reads through its own path.
//! 2. **Polling is the floor.** Nothing here changes any loop's cadence. A
//!    dead stream, a stale cursor or an unset mirror leaves every loop on its
//!    timer, so the failure mode is latency only.
//! 3. **Decision reads are never served from here.** Nothing in this file
//!    reads state for a claim, a label transition or a merge.
//! 4. **The key never leaves the process** except as the bearer header.
//!
//! Delivery is at-least-once: a restart resumes from the persisted cursor and
//! may re-read changes it already saw, which costs at most one extra early
//! tick. A repo with no cursor yet (first run, a new repo, a different
//! mirror origin) is baselined at its latest change without a prompt, so a
//! restart never replays history. A cursor the stream truncated, or one past
//! the repo's latest change (the mirror's log was reset), re-baselines and
//! prompts once, since changes may have been missed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::client::{self, ChangesPage, MirrorError};
use super::{Active, Refresher, CHECK_INTERVAL};
use crate::event_bus::EventBus;
use crate::types::Event;

/// The `source` of a `forge.event` prompt built from the change stream.
pub const MIRROR_SOURCE: &str = "github-events-mirror";

/// Repos per `/v1/changes` request (the mirror's maximum).
pub const MAX_REPOS_PER_REQUEST: usize = 20;

/// Changes per repo per request.
pub const CHANGE_LIMIT: u32 = 200;

/// Most follow-up pages one chunk reads per pass; the rest wait one cadence.
pub const MAX_PAGES_PER_PASS: usize = 5;

/// The persisted cursors: one `seq` per repo, for one mirror origin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursors {
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub repos: BTreeMap<String, u64>,
}

impl Cursors {
    /// Empty cursors for `origin`.
    #[must_use]
    pub fn new(origin: &str) -> Self {
        Self {
            origin: origin.to_string(),
            repos: BTreeMap::new(),
        }
    }
}

/// `$HOME/.loom/forge-mirror/cursors.json`; `None` under `cfg(test)` (tests
/// share one process `HOME`).
#[cfg(not(test))]
#[must_use]
pub fn default_cursor_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".loom").join("forge-mirror").join("cursors.json"))
}

#[cfg(test)]
#[must_use]
pub fn default_cursor_path() -> Option<PathBuf> {
    None
}

/// The cursors at `path` for `origin`. Missing, unreadable, malformed or
/// another origin's file reads as empty (every repo is baselined).
#[must_use]
pub fn load_cursors(path: Option<&Path>, origin: &str) -> Cursors {
    path.and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Cursors>(&b).ok())
        .filter(|c| c.origin == origin)
        .unwrap_or_else(|| Cursors::new(origin))
}

/// Persist `cursors` atomically.
///
/// # Errors
///
/// Any I/O failure.
pub fn save_cursors(path: &Path, cursors: &Cursors) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(cursors).map_err(std::io::Error::other)?;
    crate::roll_pause::write_atomic(path, &body)
}

/// The webhook event name a change kind stands for, so the existing
/// consumers' type filters ([`crate::forge_events::wake::WORK_FINDER_EVENT_TYPES`]
/// and friends) apply unchanged.
#[must_use]
pub fn webhook_type(kind: &str, item_type: Option<&str>) -> Option<&'static str> {
    match kind {
        "issue" => Some("issues"),
        "label" if item_type == Some("pr") => Some("pull_request"),
        "label" => Some("issues"),
        "pr" | "pr_merged" => Some("pull_request"),
        "check" => Some("check_run"),
        _ => None,
    }
}

/// What one pass learned: the prompt to publish, and the telemetry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Nudge {
    /// Changes read (each repo's `kinds`, before dedupe).
    pub count: usize,
    /// Webhook-style type names of the changes.
    pub types: BTreeSet<&'static str>,
    /// The repos that changed, or must be re-read.
    pub repos: BTreeSet<String>,
    /// Some repo's cursor was truncated or reset: changes may have been missed.
    pub missed: bool,
    /// `(repo, kind, seconds from the change to this pull)`.
    pub latencies: Vec<(String, String, f64)>,
}

impl Nudge {
    /// Whether this pass prompts the loops.
    #[must_use]
    pub fn prompts(&self) -> bool {
        self.count > 0 || self.missed
    }

    /// The `forge.event` payload: routing hints and invalidation keys only.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "source": MIRROR_SOURCE,
            "count": self.count.max(usize::from(self.missed)),
            "types": self.types.iter().collect::<Vec<_>>(),
            "repos": self.repos.iter().collect::<Vec<_>>(),
            "missed": self.missed,
        })
    }
}

/// Set every repo `page` names to its latest change, without a prompt.
pub fn baseline(cursors: &mut Cursors, page: &ChangesPage) {
    for (repo, rc) in &page.repos {
        cursors
            .repos
            .insert(repo.to_ascii_lowercase(), rc.watermark.latest_seq);
    }
}

/// Fold one `/v1/changes` answer for `asked` into `cursors` and `nudge`.
pub fn apply(
    cursors: &mut Cursors,
    asked: &[String],
    page: &ChangesPage,
    now: DateTime<Utc>,
    nudge: &mut Nudge,
) {
    for change in &page.changes {
        let repo = change.repo.to_ascii_lowercase();
        if !asked.contains(&repo) {
            continue;
        }
        let Some(kind) = webhook_type(&change.kind, change.item_type.as_deref()) else {
            continue;
        };
        nudge.count += 1;
        nudge.types.insert(kind);
        if let Some(at) = change
            .at
            .as_deref()
            .and_then(|a| DateTime::parse_from_rfc3339(a).ok())
        {
            let secs = (now - at.with_timezone(&Utc)).num_milliseconds().max(0) as f64 / 1000.0;
            nudge
                .latencies
                .push((repo.clone(), change.kind.clone(), secs));
        }
        nudge.repos.insert(repo);
    }
    for (repo, rc) in &page.repos {
        let repo = repo.to_ascii_lowercase();
        if !asked.contains(&repo) {
            continue;
        }
        let prev = cursors.repos.get(&repo).copied().unwrap_or(0);
        let latest = rc.watermark.latest_seq;
        if rc.truncated || (rc.watermark.known && latest < prev) {
            // Missed changes, or the log restarted: re-read once, from now on.
            cursors.repos.insert(repo.clone(), latest);
            nudge.missed = true;
            nudge.repos.insert(repo);
            continue;
        }
        cursors.repos.insert(repo, prev.max(rc.next_after));
    }
}

fn after_of(cursors: &Cursors, chunk: &[String]) -> String {
    chunk
        .iter()
        .map(|r| format!("{r}:{}", cursors.repos.get(r).copied().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(",")
}

async fn changes(
    http: &reqwest::Client,
    active: &Active,
    chunk: &[String],
    after: String,
    limit: u32,
) -> Result<ChangesPage, MirrorError> {
    let query = vec![
        ("repos".to_string(), chunk.join(",")),
        ("after".to_string(), after),
        ("limit".to_string(), limit.to_string()),
    ];
    let page = client::get_json(http, active, "/v1/changes", &query).await;
    crate::observability::ops::forge_mirror::record_read(
        "wake",
        "-",
        if page.is_ok() {
            super::discovery::Outcome::Hit
        } else {
            super::discovery::Outcome::Error
        },
    );
    client::decode(page?)
}

/// One pull of the change stream for `repos`: baseline the repos with no
/// cursor, then read every other repo's new changes (bounded pages).
///
/// # Errors
///
/// The first failed request; cursors already advanced stay advanced.
pub async fn pass(
    http: &reqwest::Client,
    active: &Active,
    repos: &[String],
    cursors: &mut Cursors,
) -> Result<Nudge, MirrorError> {
    let mut nudge = Nudge::default();
    let (known, new): (Vec<String>, Vec<String>) = repos
        .iter()
        .cloned()
        .partition(|r| cursors.repos.contains_key(r));
    for chunk in new.chunks(MAX_REPOS_PER_REQUEST) {
        let page = changes(http, active, chunk, "0".to_string(), 1).await?;
        baseline(cursors, &page);
    }
    for chunk in known.chunks(MAX_REPOS_PER_REQUEST) {
        for _ in 0..MAX_PAGES_PER_PASS {
            let page = changes(http, active, chunk, after_of(cursors, chunk), CHANGE_LIMIT).await?;
            apply(cursors, chunk, &page, Utc::now(), &mut nudge);
            if !page.more {
                break;
            }
        }
    }
    Ok(nudge)
}

/// Publish a pass's prompt and invalidate the poll gate's holds for the repos
/// that changed (over-reporting costs calls, never correctness).
pub fn publish(bus: &EventBus, nudge: &Nudge) {
    if !nudge.prompts() {
        return;
    }
    for repo in &nudge.repos {
        super::discovery::release_hold(repo);
    }
    let events: Vec<serde_json::Value> = nudge.repos.iter().map(|r| json!({ "repo": r })).collect();
    crate::forge_events::poll_gate::ingest_page(&events, nudge.missed && events.is_empty());
    for (repo, kind, secs) in &nudge.latencies {
        crate::observability::ops::forge_mirror::record_change(repo, kind, *secs);
    }
    if let Err(error) = bus.publish(Event::Generic {
        topic: crate::forge_events::BUS_TOPIC.to_string(),
        payload: nudge.payload(),
    }) {
        // No subscriber is the normal case while every consumer is off.
        log::debug!("forge_mirror: forge.event published with {error}");
    }
}

/// The repos this daemon serves: its workspace and every provisioned root,
/// as `github.com` slugs.
fn served_repos(workspace_root: &Path, pool: &crate::workspace_pool::WorkspacePool) -> Vec<String> {
    let mut roots = vec![workspace_root.to_path_buf()];
    for registry in pool.provisioned_registries() {
        let root = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config()
            .workspace_root
            .clone();
        roots.push(root);
    }
    let set: BTreeSet<String> = roots
        .iter()
        .filter_map(|r| super::discovery::resolve_repo(Some(r), None))
        .collect();
    set.into_iter().collect()
}

/// The daemon's mirror task: refresh `forge.mirror` every [`CHECK_INTERVAL`]
/// (publishing the live mirror for the discovery reads) and, while one is
/// live, pull the change stream every `changePollSecs`. With `forge.mirror`
/// unset it only re-reads the config: no request, no file, no prompt.
pub fn spawn(
    workspace_root: PathBuf,
    bus: EventBus,
    pool: Arc<crate::workspace_pool::WorkspacePool>,
) -> tokio::task::JoinHandle<()> {
    super::set_active(None);
    tokio::spawn(async move {
        let mut refresher = Refresher::default();
        let mut refreshed: Option<Instant> = None;
        let mut http: Option<reqwest::Client> = None;
        let mut cursors: Option<Cursors> = None;
        let mut last_error: Option<String> = None;
        let path = default_cursor_path();
        loop {
            if refreshed.is_none_or(|t| t.elapsed() >= CHECK_INTERVAL) {
                let ws = workspace_root.clone();
                refresher = tokio::task::spawn_blocking(move || {
                    let _ = refresher.pass(&ws, Instant::now());
                    refresher
                })
                .await
                .unwrap_or_default();
                refreshed = Some(Instant::now());
            }
            let Some(active) = super::active() else {
                cursors = None;
                tokio::time::sleep(CHECK_INTERVAL).await;
                continue;
            };
            if http.is_none() {
                http = client::build_http()
                    .map_err(|e| log::warn!("forge_mirror: {e}; no wakes"))
                    .ok();
            }
            let Some(h) = http.as_ref() else {
                tokio::time::sleep(CHECK_INTERVAL).await;
                continue;
            };
            let c = cursors.get_or_insert_with(|| load_cursors(path.as_deref(), &active.base_url));
            if c.origin != active.base_url {
                *c = Cursors::new(&active.base_url);
            }
            let (ws, p) = (workspace_root.clone(), pool.clone());
            let repos = tokio::task::spawn_blocking(move || served_repos(&ws, &p))
                .await
                .unwrap_or_default();
            c.repos.retain(|r, _| repos.contains(r));
            let before = c.clone();
            match pass(h, &active, &repos, c).await {
                Ok(nudge) => {
                    last_error = None;
                    publish(&bus, &nudge);
                }
                Err(e) => {
                    let class = e.to_string();
                    if last_error.as_deref() != Some(class.as_str()) {
                        log::warn!(
                            "forge_mirror: change stream pull failed ({class}); loops keep \
                             their cadence — 2am#4367"
                        );
                        last_error = Some(class);
                    }
                }
            }
            if *c != before {
                if let Some(p) = path.as_deref() {
                    if let Err(e) = save_cursors(p, c) {
                        log::debug!("forge_mirror: cursors not persisted ({e})");
                    }
                }
            }
            tokio::time::sleep(active.change_poll).await;
        }
    })
}
