//! The worker's side of the captain-hosted issue-filing lock (#13918).
//!
//! The filing lock (#6714) is two tiers. The host tier is a local `mkdir`
//! lock ([`crate::filing_lock`]) and stays: it is what serialises processes on
//! one machine. This module is the fleet tier. After the local lock is taken,
//! the filer asks the captain for the one fleet-wide slot
//! ([`proto::FILING_LOCK_PATH`]) and gives it back when the burst ends
//! ([`proto::FILING_UNLOCK_PATH`]).
//!
//! - **Off** (`fleetDispatch.enabled` unset): no call, no span; the lock is
//!   exactly what it was.
//! - **Granted**: the caller files, then [`release`]s.
//! - **Held**: the caller polls, within the bounded wait it was given. On
//!   expiry the answer is [`Tier::Deferred`]: the burst must not file, as for
//!   the local lock (rc 75 for `create-issue.sh`).
//! - **Fail open**: no answer, or this host is in fallback ([`super::fallback`],
//!   the same hysteresis claims use): the burst files under the local lock and
//!   the pacing it already has, and the fallback is counted ([`fallbacks`]) and
//!   spanned (`reason` `fallback`, or the failed call's token).
//!
//! One `loom.fleet_dispatch.call` span per acquire (its `latency_ms` is the
//! whole wait) and one per release. The captain's spans carry the hold time.
//! The shell path runs in the one-shot `filing-lock` subcommand, which
//! registers the standalone ops sink at startup and flushes its queue once
//! at exit (#14129); without that setup this process would emit nothing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::client::{self, CallFacts};
use super::fallback::Mode;
use super::proto::{self, AckResponse, FilingLockRequest, FilingLockResponse, FilingUnlockRequest};
use super::worker;

/// The `reason` of a filing acquire that skipped the captain because this host
/// is in fallback.
pub const REASON_FALLBACK: &str = "fallback";
/// The `reason` of an acquire that gave up waiting.
pub const REASON_DEFERRED: &str = "deferred";

static FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// How many filing acquires in this process fell open to the local lock
/// because the captain could not be asked.
#[must_use]
pub fn fallbacks() -> u64 {
    FALLBACKS.load(Ordering::Relaxed)
}

/// A grant the caller must hand back with [`release`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    url: String,
    /// The holder id the grant was made to.
    pub holder: String,
}

impl Hold {
    /// A hold on `holder` at the captain `url`, for a caller that took the
    /// grant in another process (the shell's `release`).
    #[must_use]
    pub fn new(url: &str, holder: &str) -> Self {
        Self {
            url: url.to_string(),
            holder: holder.to_string(),
        }
    }
}

/// What the captain tier said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier {
    /// Fleet mode is off for this host.
    Off,
    /// The caller holds the fleet slot.
    Granted(Hold),
    /// The slot stayed held past the wait. The caller must not file.
    Deferred { reason: String },
    /// The captain could not be asked: file under the local lock alone.
    FailOpen { reason: String },
}

impl Tier {
    /// Whether the caller may file.
    #[must_use]
    pub fn may_file(&self) -> bool {
        !matches!(self, Self::Deferred { .. })
    }
}

/// The workspace whose config names the captain: `LOOM_WORKSPACE_ROOT`, else
/// the current directory.
#[must_use]
pub fn workspace_root() -> PathBuf {
    std::env::var("LOOM_WORKSPACE_ROOT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// A fresh holder id, one per burst.
#[must_use]
pub fn new_holder() -> String {
    format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple())
}

/// The captain's URL when this host is in fleet mode (module docs).
#[must_use]
pub fn endpoint(root: &Path) -> Option<String> {
    worker::host_endpoint(root)
}

/// Ask the captain for the fleet filing slot as `holder`, waiting at most
/// `wait` while another host holds it (one request is always made, even at a
/// zero wait). Blocking: never call it inline on a tokio runtime worker.
#[must_use]
pub fn acquire(root: &Path, label: &str, holder: &str, wait: Duration) -> Tier {
    let Some(url) = worker::host_endpoint(root) else {
        return Tier::Off;
    };
    acquire_at(&url, label, holder, wait)
}

/// [`acquire`] against a known captain `url`.
#[must_use]
pub fn acquire_at(url: &str, label: &str, holder: &str, wait: Duration) -> Tier {
    let started = Instant::now();
    let mut facts = CallFacts {
        endpoint: proto::FILING_LOCK_PATH,
        ..CallFacts::default()
    };
    if worker::mode() == Mode::Fallback {
        return fall_open(&mut facts, started, REASON_FALLBACK, None);
    }
    let request = FilingLockRequest {
        host_id: worker::host_id(),
        holder: holder.to_string(),
        label: label.to_string(),
    };
    let deadline = started + wait;
    loop {
        match client::post_blocking::<_, FilingLockResponse>(url, proto::FILING_LOCK_PATH, &request)
        {
            Ok(answer) if answer.granted => {
                worker::observe(true, None);
                facts.status = Some(200);
                facts.result = client::RESULT_GRANTED;
                facts.reason = Some("granted".to_string());
                facts.server_epoch = Some(answer.server_epoch);
                facts.latency = started.elapsed();
                client::record(&facts);
                return Tier::Granted(Hold::new(url, holder));
            }
            Ok(answer) => {
                worker::observe(true, None);
                let retry = Duration::from_secs(answer.retry_after_s.unwrap_or(1).clamp(1, 2));
                facts.status = Some(200);
                facts.result = client::RESULT_REFUSED;
                facts.reason = Some(REASON_DEFERRED.to_string());
                facts.server_epoch = Some(answer.server_epoch);
                facts.held_by = answer.held_by.filter(|h| !h.is_empty());
                facts.retry_after_s = answer.retry_after_s;
                let now = Instant::now();
                if now >= deadline {
                    facts.latency = started.elapsed();
                    client::record(&facts);
                    let blocker = facts.held_by.as_deref().unwrap_or("another host");
                    let reason = format!(
                        "the captain's fleet filing lock is held by {blocker} (waited {}s)",
                        wait.as_secs()
                    );
                    log::warn!("filing_lock: DEFERRING '{label}' — {reason}");
                    return Tier::Deferred { reason };
                }
                std::thread::sleep(retry.min(deadline - now));
            }
            Err(e) => {
                worker::observe(false, Some(&e));
                facts.status = e.status();
                return fall_open(&mut facts, started, e.token(), Some(&e.to_string()));
            }
        }
    }
}

/// Count and span a fallback to the local lock.
fn fall_open(facts: &mut CallFacts, started: Instant, token: &str, detail: Option<&str>) -> Tier {
    FALLBACKS.fetch_add(1, Ordering::Relaxed);
    facts.result = client::RESULT_ERROR;
    facts.reason = Some(token.to_string());
    facts.latency = started.elapsed();
    client::record(facts);
    log::warn!(
        "filing_lock: the captain could not be asked ({}); filing under the local lock only \
         (fallback #{}, #13918)",
        detail.unwrap_or(token),
        fallbacks()
    );
    Tier::FailOpen {
        reason: token.to_string(),
    }
}

/// Hand a grant back. Best-effort: a failed unlock leaves the captain's TTL
/// ([`super::filing_slot::FILING_LOCK_TTL`]) to free the slot. In fallback the
/// call is skipped for the same reason.
pub fn release(hold: &Hold) {
    if worker::mode() == Mode::Fallback {
        return;
    }
    let started = Instant::now();
    let request = FilingUnlockRequest {
        host_id: worker::host_id(),
        holder: hold.holder.clone(),
    };
    let mut facts = CallFacts {
        endpoint: proto::FILING_UNLOCK_PATH,
        ..CallFacts::default()
    };
    match client::post_blocking::<_, AckResponse>(&hold.url, proto::FILING_UNLOCK_PATH, &request) {
        Ok(ack) => {
            worker::observe(true, None);
            facts.status = Some(200);
            facts.result = client::RESULT_OK;
            facts.reason = Some(if ack.known { "released" } else { "not_holder" }.to_string());
            facts.server_epoch = Some(ack.server_epoch);
        }
        Err(e) => {
            worker::observe(false, Some(&e));
            facts.status = e.status();
            facts.result = client::RESULT_ERROR;
            facts.reason = Some(e.token().to_string());
            log::info!("filing_lock: unlock got no answer ({e}); the captain's TTL frees the slot");
        }
    }
    facts.latency = started.elapsed();
    client::record(&facts);
}

#[cfg(test)]
#[path = "filing_tests.rs"]
mod tests;
