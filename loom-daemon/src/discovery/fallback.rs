//! The fleet's fallback tiers, capped (#12311).
//!
//! When the worklist cannot answer for a repo, the tick lists that repo
//! through the direct backend (the gateway tier, then `gh`). On the fleet
//! that is an outage mode, so it must not bring back the load that tripped
//! the read pool when several hosts drop to it at once:
//!
//! - **Backstop pace.** A repo is listed at most once per `backstop` (the
//!   work finder's base cadence, ADR-0021's polling floor), not every tick.
//!   Between listings the tick draws from the repo's last listing. This
//!   covers the failed worklist and a repo that stays `unknown` or `error`.
//! - **A failed listing is paced too**: it is not retried until the backstop
//!   passes, and the repo reads `none` meanwhile (no draw). Only a `reserve`
//!   refusal (the gateway protecting its pool) draws from the last listing,
//!   and it is retried on the next tick.
//! - **Held in memory only**, like the 304 cache: dropped on restart, never
//!   persisted.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::snapshot::RepoSnapshot;

/// A fallback listing that did not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListFailure {
    /// The `loom.discovery.fallback_reason`: `reserve`, `breaker`,
    /// `listing`.
    pub reason: &'static str,
    pub detail: String,
}

/// What serving a repo from the fallback tier did.
#[derive(Debug)]
pub enum Served {
    /// Listed now: one forge listing was made.
    Listed(Arc<RepoSnapshot>),
    /// Inside the backstop window: the last listing, no request.
    Paced(Arc<RepoSnapshot>),
    /// The listing failed with `reason` and the last good listing (if any)
    /// stands in. Only a `reserve` refusal draws from it; any other failure
    /// is `None` below.
    Stale(Arc<RepoSnapshot>, ListFailure),
    /// No listing and nothing to draw from: tier `none` for the repo.
    Failed(ListFailure),
}

struct Entry {
    attempted: Instant,
    good: Option<Arc<RepoSnapshot>>,
    failure: Option<ListFailure>,
}

/// The fallback listings across ticks.
pub struct Fallback {
    backstop: Duration,
    repos: BTreeMap<String, Entry>,
}

impl Fallback {
    #[must_use]
    pub fn new(backstop: Duration) -> Self {
        Self {
            backstop,
            repos: BTreeMap::new(),
        }
    }

    pub fn set_backstop(&mut self, backstop: Duration) {
        self.backstop = backstop;
    }

    #[must_use]
    pub fn backstop(&self) -> Duration {
        self.backstop
    }

    /// `key`'s answer without a request, when it is inside the backstop
    /// window of its last attempt; `None`: list it.
    #[must_use]
    pub fn paced(&self, key: &str, now: Instant) -> Option<Served> {
        let entry = self.repos.get(key)?;
        if now.saturating_duration_since(entry.attempted) >= self.backstop {
            return None;
        }
        Some(match (&entry.good, &entry.failure) {
            (_, Some(f)) => Served::Failed(f.clone()),
            (Some(good), None) => Served::Paced(good.clone()),
            (None, None) => Served::Failed(ListFailure {
                reason: "listing",
                detail: "no listing".into(),
            }),
        })
    }

    /// Record the listing made for `key` at `now` and say what it serves.
    pub fn finish(
        &mut self,
        key: &str,
        now: Instant,
        listed: Result<RepoSnapshot, ListFailure>,
    ) -> Served {
        match listed {
            Ok(snap) => {
                let snap = Arc::new(snap);
                self.repos.insert(
                    key.to_string(),
                    Entry {
                        attempted: now,
                        good: Some(snap.clone()),
                        failure: None,
                    },
                );
                Served::Listed(snap)
            }
            Err(failure) => self.failed(key, now, failure),
        }
    }

    /// The tick's discovery ended before this repo was listed (a reserve
    /// refusal earlier in the tick): draw from the last listing, send
    /// nothing.
    pub fn without_listing(&self, key: &str, failure: ListFailure) -> Served {
        match self.repos.get(key).and_then(|e| e.good.clone()) {
            Some(good) => Served::Stale(good, failure),
            None => Served::Failed(failure),
        }
    }

    fn failed(&mut self, key: &str, now: Instant, failure: ListFailure) -> Served {
        let good = self.repos.get(key).and_then(|e| e.good.clone());
        let reserve = failure.reason == "reserve";
        // A reserve refusal is the gateway protecting its pool, not this
        // repo failing: it is retried the next tick (the first tier is tried
        // again each tick) and the last listing stands in. Every other
        // failure is paced.
        if !reserve {
            self.repos.insert(
                key.to_string(),
                Entry {
                    attempted: now,
                    good: good.clone(),
                    failure: Some(failure.clone()),
                },
            );
        }
        match good {
            Some(good) if reserve => Served::Stale(good, failure),
            _ => Served::Failed(failure),
        }
    }
}
