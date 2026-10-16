//! The Mirror backend: one worklist request per tick, conditional on the
//! last ETag (module docs of [`super`]). No fallback to the forge.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};

use super::snapshot::RepoSnapshot;
use super::worklist::{self, Worklist};
use crate::forge_mirror::client::{self, Conditional, MirrorError};

/// The worklist path.
pub const PATH: &str = "/v1/mirror/worklist";

/// The worklist request's deadline. It carries every readable repo, so it is
/// allowed longer than the per-repo reads' [`client::REQUEST_TIMEOUT`].
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(20);

/// What one worklist request answered, and how long it took.
pub struct Fetched {
    pub outcome: Result<Conditional, MirrorError>,
    pub latency: Duration,
}

struct Cached {
    etag: Option<String>,
    worklist: Arc<Worklist>,
}

/// The last good worklist and its ETag, across ticks.
#[derive(Default)]
pub struct MirrorBackend {
    last: Option<Cached>,
}

/// One tick's view of the worklist.
pub struct TickWorklist {
    worklist: Option<Arc<Worklist>>,
    error: Option<String>,
    pub latency_ms: u64,
    pub not_modified: bool,
    pub reused_previous: bool,
    max_staleness: Duration,
}

fn request(etag: Option<&str>) -> Fetched {
    let started = Instant::now();
    let outcome = match crate::forge_mirror::active() {
        None => Err(MirrorError::Transport("the mirror is not live".into())),
        Some(active) => {
            client::get_json_conditional_blocking(&active, PATH, etag, REQUEST_DEADLINE)
        }
    };
    Fetched {
        outcome,
        latency: started.elapsed(),
    }
}

impl MirrorBackend {
    /// Make this tick's one request.
    pub fn fetch(&mut self, now: DateTime<Utc>) -> TickWorklist {
        self.fetch_with(now, request)
    }

    /// [`Self::fetch`] with the request given.
    pub fn fetch_with(
        &mut self,
        _now: DateTime<Utc>,
        send: impl FnOnce(Option<&str>) -> Fetched,
    ) -> TickWorklist {
        let etag = self.last.as_ref().and_then(|c| c.etag.clone());
        let Fetched { outcome, latency } = send(etag.as_deref());
        let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
        let max_staleness = crate::forge_mirror::active()
            .map_or(Duration::from_secs(crate::forge_mirror::DEFAULT_MAX_STALENESS_SECS), |a| {
                a.max_staleness
            });
        let mut tick = TickWorklist {
            worklist: None,
            error: None,
            latency_ms,
            not_modified: false,
            reused_previous: false,
            max_staleness,
        };
        let failure = match outcome {
            Ok(Conditional::NotModified) => match &self.last {
                Some(last) => {
                    tick.worklist = Some(last.worklist.clone());
                    tick.not_modified = true;
                    return tick;
                }
                None => "worklist answered 304 with nothing cached".to_string(),
            },
            Ok(Conditional::Modified { etag, body }) => match worklist::decode(body) {
                Ok(list) => {
                    let list = Arc::new(list);
                    tick.worklist = Some(list.clone());
                    self.last = Some(Cached {
                        etag,
                        worklist: list,
                    });
                    return tick;
                }
                Err(e) => e.to_string(),
            },
            Err(e) => e.to_string(),
        };
        // No fallback to the forge: the previous snapshot stands in, or
        // every repo reads as unavailable this tick.
        if let Some(last) = &self.last {
            log::warn!(
                "discovery: worklist request failed ({failure}); using the previous snapshot"
            );
            tick.worklist = Some(last.worklist.clone());
            tick.reused_previous = true;
        } else {
            log::warn!("discovery: worklist request failed ({failure}); no previous snapshot");
        }
        tick.error = Some(failure);
        tick
    }
}

impl TickWorklist {
    /// One repo's snapshot.
    ///
    /// # Errors
    ///
    /// No worklist at all, or the repo is not in it (the key may read a
    /// subset of repos).
    pub fn repo(&self, slug: &str, _now: DateTime<Utc>) -> Result<Arc<RepoSnapshot>> {
        let Some(list) = &self.worklist else {
            return Err(anyhow!(
                "mirror discovery: no worklist ({})",
                self.error.as_deref().unwrap_or("unavailable")
            ));
        };
        list.repos
            .get(&slug.to_ascii_lowercase())
            .cloned()
            .ok_or_else(|| {
                anyhow!("mirror discovery: {slug} is not in the worklist (the key may not read it)")
            })
    }

    #[must_use]
    pub fn repo_count(&self) -> usize {
        self.worklist.as_ref().map_or(0, |w| w.repos.len())
    }

    #[must_use]
    pub fn item_count(&self) -> usize {
        self.worklist
            .as_ref()
            .map_or(0, |w| w.repos.values().map(|r| r.rows.len()).sum())
    }

    /// Age of the stalest repo's newest evidence.
    #[must_use]
    pub fn oldest_watermark_age_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        self.worklist
            .as_ref()?
            .repos
            .values()
            .filter_map(|r| r.watermark.as_ref()?.age_secs(now))
            .max()
    }

    /// Repos whose newest evidence is older than the mirror's bound.
    #[must_use]
    pub fn stale_repos(&self, now: DateTime<Utc>) -> usize {
        let bound = self.max_staleness.as_secs();
        self.worklist.as_ref().map_or(0, |w| {
            w.repos
                .values()
                .filter(|r| {
                    r.watermark
                        .as_ref()
                        .and_then(|m| m.age_secs(now))
                        .is_none_or(|age| age > bound)
                })
                .count()
        })
    }
}
