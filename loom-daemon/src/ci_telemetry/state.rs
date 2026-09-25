//! Poller bookkeeping under `.loom/state/ci-telemetry/`: the health/status
//! record (`status.json`), the repo-discovery ETag cache
//! (`discovery-cache.json`), and the per-host cycle lock (`poll.lock`).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What the last completed cycle did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleSummary {
    pub repos_polled: usize,
    pub runs_emitted: usize,
    pub jobs_emitted: usize,
    pub recovered_units: usize,
    pub requests: usize,
    #[serde(default)]
    pub repo_errors: usize,
    /// Jobs whose logs were captured this cycle (#8825).
    #[serde(default)]
    pub logs_captured: usize,
    /// `ci.job.log` chunk records emitted this cycle.
    #[serde(default)]
    pub job_logs_emitted: usize,
    /// Jobs whose logs hit the per-job cap and were emitted truncated.
    #[serde(default)]
    pub logs_truncated: usize,
    /// Log downloads that failed this cycle (each retried next cycle, up to
    /// `logs::MAX_ATTEMPTS`).
    #[serde(default)]
    pub log_failures: usize,
    /// Wanted job logs left for the next cycle by the per-cycle download cap.
    #[serde(default)]
    pub logs_deferred: usize,
}

/// `status.json` — every field needed to tell never-polled / ok / stale /
/// failing apart, so silence can never read as healthy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Org-wide rate-limit backoff: no request is made before this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cycle: Option<CycleSummary>,
    /// When a job log was last successfully downloaded (#8825). Distinct from
    /// `last_ok_at`: a poller can be cycling happily while log capture has
    /// been stuck for hours, and silence there must not read as healthy
    /// either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_log_fetch_at: Option<DateTime<Utc>>,
}

fn status_path(dir: &Path) -> PathBuf {
    dir.join("status.json")
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic JSON write: temp file + fsync + rename.
fn save_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        use std::io::Write;
        let mut file = File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)
}

#[must_use]
pub fn load_status(dir: &Path) -> PollStatus {
    load_json(&status_path(dir))
}

pub fn save_status(dir: &Path, status: &PollStatus) -> io::Result<()> {
    save_json(&status_path(dir), status)
}

/// The poller's health, as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// No cycle has ever been attempted on this host.
    NeverPolled,
    /// The last cycle succeeded, `age_secs` ago.
    Ok { age_secs: i64 },
    /// The last cycle succeeded, but longer ago than three poll intervals —
    /// the poller has gone quiet, which must not read as healthy.
    Stale { age_secs: i64 },
    /// The last cycle failed.
    Failing {
        since: Option<DateTime<Utc>>,
        error: String,
        consecutive_failures: u32,
        backoff_until: Option<DateTime<Utc>>,
    },
}

impl Health {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Health::NeverPolled => "never-polled",
            Health::Ok { .. } => "ok",
            Health::Stale { .. } => "stale",
            Health::Failing { .. } => "failing",
        }
    }
}

/// Classify `status` as of `now` against the poll `interval_secs`.
#[must_use]
pub fn classify(status: &PollStatus, now: DateTime<Utc>, interval_secs: u64) -> Health {
    if status.last_attempt_at.is_none() {
        return Health::NeverPolled;
    }
    let failed_last = match (status.last_error_at, status.last_ok_at) {
        (Some(err), Some(ok)) => err >= ok,
        (Some(_), None) => true,
        _ => false,
    };
    if failed_last {
        return Health::Failing {
            since: status.last_ok_at,
            error: status.last_error.clone().unwrap_or_default(),
            consecutive_failures: status.consecutive_failures,
            backoff_until: status.backoff_until.filter(|until| *until > now),
        };
    }
    let Some(last_ok) = status.last_ok_at else {
        return Health::NeverPolled;
    };
    let age_secs = (now - last_ok).num_seconds().max(0);
    let stale_after = i64::try_from(interval_secs.saturating_mul(3)).unwrap_or(i64::MAX);
    if age_secs > stale_after {
        Health::Stale { age_secs }
    } else {
        Health::Ok { age_secs }
    }
}

/// One cached discovery page: the validator ETag, the body it validated,
/// and that page's `next` link (a 304 carries no reliable `Link` header).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPage {
    pub etag: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// The persisted repo-discovery ETag cache, keyed by request path.
pub type DiscoveryCache = BTreeMap<String, CachedPage>;

fn discovery_path(dir: &Path) -> PathBuf {
    dir.join("discovery-cache.json")
}

#[must_use]
pub fn load_discovery_cache(dir: &Path) -> DiscoveryCache {
    load_json(&discovery_path(dir))
}

pub fn save_discovery_cache(dir: &Path, cache: &DiscoveryCache) -> io::Result<()> {
    save_json(&discovery_path(dir), cache)
}

/// A held, exclusive, per-host cycle lock (`flock` on `poll.lock`),
/// released when dropped. Serialises the CLI `--once` and the daemon poller
/// on one host so two writers never race the ledger.
#[derive(Debug)]
pub struct CycleLock {
    _file: File,
}

impl CycleLock {
    /// Try to take the lock without blocking. `Ok(None)` means another cycle
    /// on this host holds it.
    pub fn try_acquire(dir: &Path) -> io::Result<Option<Self>> {
        std::fs::create_dir_all(dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("poll.lock"))?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: `flock` on a descriptor we own for the duration of the
            // call; no memory is shared with the kernel.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(error);
            }
        }
        Ok(Some(CycleLock { _file: file }))
    }
}
