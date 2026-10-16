//! The Mirror tier: one worklist request per tick, conditional on the last
//! ETag (module docs of [`super`]). The cache is one in-memory
//! `(etag, body)`, replaced on every `200`, dropped on restart, never merged
//! with change-stream events and never persisted. It only ever answers a
//! `304`: a failed request does not reuse it, the tick falls to the next tier
//! instead ([`super::fallback`]).

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::snapshot::{RepoSnapshot, RepoStatus};
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
    /// Why there is no worklist this tick.
    failure: Option<Failure>,
    pub latency_ms: u64,
    pub not_modified: bool,
    max_staleness: Duration,
}

/// A failed worklist request: its class (the span's `.worklist.error_class`)
/// and its text (logged, never put on a span).
#[derive(Debug, Clone)]
pub struct Failure {
    pub class: &'static str,
    pub detail: String,
}

impl Failure {
    fn of(e: &MirrorError) -> Self {
        let class = match e {
            MirrorError::Transport(_) => "transport",
            MirrorError::Auth(_) => "auth",
            MirrorError::Status(_) => "status",
            MirrorError::Malformed(_) => "malformed",
        };
        Self {
            class,
            detail: e.to_string(),
        }
    }
}

/// Why the worklist could not answer for a repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Miss {
    /// The request failed, or nothing is cached for a `304`.
    NoWorklist(&'static str),
    /// The worklist omits the repo: the key may not read it.
    NotInWorklist,
    /// The worklist names the repo with a `status` that is not `ok`.
    Status(RepoStatus),
}

impl Miss {
    /// The tick's `fallback_reason` for this miss.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NoWorklist(class) => class,
            Self::NotInWorklist | Self::Status(_) => "repo_status",
        }
    }

    /// The `status` bucket the miss counts under.
    #[must_use]
    pub fn status(&self) -> RepoStatus {
        match self {
            Self::NoWorklist(_) => RepoStatus::Error,
            Self::NotInWorklist => RepoStatus::Unknown,
            Self::Status(s) => *s,
        }
    }
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
            failure: None,
            latency_ms,
            not_modified: false,
            max_staleness,
        };
        let failure = match outcome {
            Ok(Conditional::NotModified) => match &self.last {
                Some(last) => {
                    tick.worklist = Some(last.worklist.clone());
                    tick.not_modified = true;
                    return tick;
                }
                None => Failure {
                    class: "malformed",
                    detail: "worklist answered 304 with nothing cached".to_string(),
                },
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
                Err(e) => Failure::of(&e),
            },
            Err(e) => Failure::of(&e),
        };
        log::warn!(
            "discovery: worklist request failed ({}); this tick falls to the next tier",
            failure.detail
        );
        tick.failure = Some(failure);
        tick
    }
}

impl TickWorklist {
    /// One repo's snapshot from the worklist.
    ///
    /// # Errors
    ///
    /// [`Miss`]: the caller falls to the next tier for this repo alone.
    pub fn repo(&self, slug: &str, _now: DateTime<Utc>) -> Result<Arc<RepoSnapshot>, Miss> {
        let Some(list) = &self.worklist else {
            return Err(Miss::NoWorklist(self.failure.as_ref().map_or("transport", |f| f.class)));
        };
        let repo = list
            .repos
            .get(&slug.to_ascii_lowercase())
            .ok_or(Miss::NotInWorklist)?;
        match repo.status {
            RepoStatus::Ok => Ok(repo.clone()),
            other => Err(Miss::Status(other)),
        }
    }

    /// `ok`, `not_modified` or `error`.
    #[must_use]
    pub fn outcome(&self) -> &'static str {
        if self.failure.is_some() {
            "error"
        } else if self.not_modified {
            "not_modified"
        } else {
            "ok"
        }
    }

    #[must_use]
    pub fn error_class(&self) -> Option<&'static str> {
        self.failure.as_ref().map(|f| f.class)
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
