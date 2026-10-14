//! Discovery reads from a GitHub state mirror, and wakes from its change
//! stream (2AMLogic/2am#4367 P5).
//!
//! A state mirror is an operator-run service that holds, per repo, what the
//! fleet asks about (issues and PRs with their labels, state, author and body;
//! check outcomes per head) kept current from GitHub App webhooks and a slow
//! reconcile. Every answer carries the repo's **freshness watermark**
//! (`last_event_at`, `last_reconciled_at`, `served_at`). It also serves a
//! per-repo, sequence-numbered **change stream** that a consumer pulls with its
//! own cursor.
//!
//! # Config
//!
//! `forge.mirror` in the effective config (a fleet-wide value goes in the
//! fleet tier, a canary in one host's tier), the same tiering as
//! [`crate::forge_gateway`]:
//!
//! ```json
//! "forge": { "mirror": {
//!   "origin": "https://events.example.com",
//!   "keyRef": "ssm:///events/keys/host-a",
//!   "region": "example-region-1",
//!   "maxStalenessSecs": 600
//! } }
//! ```
//!
//! `keyRef` (`key_ref` is accepted too) is `file:<path>` or `ssm://<parameter>`
//! exactly as for the gateway ([`crate::forge_gateway::KeyRef`]): the key is
//! read into memory and sent only as `Authorization: Bearer`, never in argv,
//! a log line, a span or a status surface. `maxStalenessSecs` (default
//! [`DEFAULT_MAX_STALENESS_SECS`], clamped to
//! `[`[`MIN_STALENESS_SECS`]`, `[`MAX_STALENESS_CEILING_SECS`]`]`) is the
//! discovery bound: the oldest watermark a listing may be served at.
//! `changePollSecs` (default [`DEFAULT_CHANGE_POLL_SECS`]) is how often the
//! change stream is pulled.
//!
//! # What it changes when set
//!
//! - **Discovery reads** ([`discovery`]): the work finder's candidate
//!   listings and the stage-dwell sampler's stage listings are read from the
//!   mirror when the repo's watermark is within the bound, and from today's
//!   path (the ETag-cached REST listing, through the gateway when that is
//!   set) otherwise. A stale, unknown, never-reconciled or moved repo, an
//!   error, a timeout or a malformed answer is a fallback for that repo,
//!   never a failure and never an empty queue.
//! - **Wakes** ([`wake`]): the daemon pulls the change stream for the repos
//!   it serves, keeps its own cursor (persisted), and publishes each
//!   non-empty pull as a `forge.event` bus prompt, which ticks the loops
//!   whose `forgeEvents.events.*` consumer is armed early
//!   ([`crate::forge_events::wake`]). A change is a nudge, never a command.
//!
//! # What it never changes
//!
//! Reads that gate a write. Claim, label-transition and merge reads stay on
//! the gateway or the forge (ADR-0021's poll_gate invariant 3):
//! [`discovery::list_open`] answers only
//! [`crate::forge_events::poll_gate::ReadKind::Discovery`] and returns `None`
//! (today's path) for every other kind without a request; and only the
//! discovery listings call it (a source test pins the call sites).
//!
//! # Break-glass
//!
//! With `forge.mirror` unset, [`active`] is `None`: no discovery listing makes
//! a request or even resolves its repo, no change stream is pulled, nothing is
//! published, and every read is exactly today's. The daemon re-reads the
//! block every [`CHECK_INTERVAL`], so unsetting it takes effect within a
//! minute. The fleet store fetch never depends on the mirror.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::forge_gateway::KeyRef;

pub mod client;
pub mod discovery;
pub mod wake;

#[cfg(test)]
mod tests;

/// The config block.
pub const CONFIG_KEY: &str = "forge.mirror";

/// Default discovery bound.
pub const DEFAULT_MAX_STALENESS_SECS: u64 = 600;

/// The bound's floor: below this a healthy mirror would read stale between
/// two deliveries.
pub const MIN_STALENESS_SECS: u64 = 30;

/// The bound's ceiling: ADR-0021's hard cap on discovery staleness ("never
/// more than 15 minutes"), the same ceiling the poll gate holds.
pub const MAX_STALENESS_CEILING_SECS: u64 =
    crate::forge_events::poll_gate::MAX_STALENESS_CEILING_SECS;

/// Default change-stream pull cadence.
pub const DEFAULT_CHANGE_POLL_SECS: u64 = 30;

/// Change-stream pull cadence bounds.
pub const MIN_CHANGE_POLL_SECS: u64 = 10;
pub const MAX_CHANGE_POLL_SECS: u64 = 300;

/// How often the block is re-read.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// How often the key is re-read even when nothing changed, so a rotation
/// (a store edit plus apply) is picked up.
const KEY_REFRESH: Duration = crate::credential_preflight::GITHUB_APP_REFRESH_INTERVAL;

/// `forge.mirror`, validated. Holds a key *location*, never a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorConfig {
    /// The mirror's host (`https://` is implied).
    pub host: String,
    /// Where the subscriber key is read.
    pub key_ref: KeyRef,
    /// `forge.mirror.region` for an `ssm://` ref, when set.
    pub region: Option<String>,
    /// The discovery bound.
    pub max_staleness: Duration,
    /// The change-stream pull cadence.
    pub change_poll: Duration,
}

impl MirrorConfig {
    /// A stable digest of what the active state depends on, the key excluded.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{:?}|{:?}|{}|{}",
            self.host,
            self.key_ref,
            self.region,
            self.max_staleness.as_secs(),
            self.change_poll.as_secs()
        )
    }
}

fn secs_field(
    block: &serde_json::Map<String, Value>,
    names: &[&str],
    default: u64,
    (lo, hi): (u64, u64),
    warnings: &mut Vec<String>,
) -> u64 {
    let Some((name, raw)) = names
        .iter()
        .find_map(|n| block.get(*n).map(|v| (*n, v)))
        .filter(|(_, v)| !v.is_null())
    else {
        return default;
    };
    match raw.as_u64() {
        Some(v) if (lo..=hi).contains(&v) => v,
        Some(v) => {
            let clamped = v.clamp(lo, hi);
            warnings.push(format!(
                "{CONFIG_KEY}.{name} = {v} is outside [{lo}, {hi}]; using {clamped}"
            ));
            clamped
        }
        None => {
            warnings.push(format!(
                "{CONFIG_KEY}.{name} must be a whole number of seconds; using {default}"
            ));
            default
        }
    }
}

/// Parse `forge.mirror` from the effective config. `None` when it is absent,
/// `null`, or unusable (with the reasons).
#[must_use]
pub fn parse(effective: &Value) -> (Option<MirrorConfig>, Vec<String>) {
    let mut warnings = Vec::new();
    let block = match crate::config_resolver::get_path(effective, CONFIG_KEY) {
        None | Some(Value::Null) => return (None, warnings),
        Some(Value::Object(m)) => m,
        Some(_) => {
            warnings.push(format!("{CONFIG_KEY} must be an object; the mirror stays off"));
            return (None, warnings);
        }
    };
    let host = match block
        .get("origin")
        .and_then(Value::as_str)
        .map(crate::forge_gateway::api_host_of)
    {
        Some(Ok(h)) => h,
        Some(Err(e)) => {
            warnings.push(format!("{CONFIG_KEY}.origin: {e}; the mirror stays off"));
            return (None, warnings);
        }
        None => {
            warnings.push(format!("{CONFIG_KEY}.origin is required; the mirror stays off"));
            return (None, warnings);
        }
    };
    let key_ref = match block
        .get("keyRef")
        .or_else(|| block.get("key_ref"))
        .map(|v| v.as_str().map(KeyRef::parse))
    {
        Some(Some(Ok(r))) => r,
        Some(Some(Err(e))) => {
            warnings.push(format!("{CONFIG_KEY}.keyRef: {e}; the mirror stays off"));
            return (None, warnings);
        }
        Some(None) => {
            warnings.push(format!("{CONFIG_KEY}.keyRef must be a string; the mirror stays off"));
            return (None, warnings);
        }
        None => {
            warnings.push(format!("{CONFIG_KEY}.keyRef is required; the mirror stays off"));
            return (None, warnings);
        }
    };
    let region = match block.get("region") {
        None | Some(Value::Null) => None,
        Some(Value::String(r)) if crate::forge_gateway::valid_region(r.trim()) => {
            Some(r.trim().to_string())
        }
        Some(other) => {
            warnings.push(format!(
                "{CONFIG_KEY}.region must be an AWS region name, not {other}; ignored"
            ));
            None
        }
    };
    let max_staleness = secs_field(
        block,
        &["maxStalenessSecs", "max_staleness_secs"],
        DEFAULT_MAX_STALENESS_SECS,
        (MIN_STALENESS_SECS, MAX_STALENESS_CEILING_SECS),
        &mut warnings,
    );
    let change_poll = secs_field(
        block,
        &["changePollSecs", "change_poll_secs"],
        DEFAULT_CHANGE_POLL_SECS,
        (MIN_CHANGE_POLL_SECS, MAX_CHANGE_POLL_SECS),
        &mut warnings,
    );
    (
        Some(MirrorConfig {
            host,
            key_ref,
            region,
            max_staleness: Duration::from_secs(max_staleness),
            change_poll: Duration::from_secs(change_poll),
        }),
        warnings,
    )
}

// ---------------------------------------------------------------------------
// The live mirror in this process
// ---------------------------------------------------------------------------

/// The mirror this process reads, as the last refresh left it.
#[derive(Clone)]
pub struct Active {
    /// `https://<host>` (a loopback `http://` origin only in tests).
    pub base_url: String,
    key: String,
    /// The discovery bound.
    pub max_staleness: Duration,
    /// The change-stream pull cadence.
    pub change_poll: Duration,
}

impl std::fmt::Debug for Active {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Active")
            .field("base_url", &self.base_url)
            .field("key", &"<redacted>")
            .field("max_staleness", &self.max_staleness)
            .field("change_poll", &self.change_poll)
            .finish()
    }
}

impl Active {
    /// The active state for `cfg` with its key read.
    #[must_use]
    pub fn new(cfg: &MirrorConfig, key: String) -> Self {
        Self {
            base_url: format!("https://{}", cfg.host),
            key,
            max_staleness: cfg.max_staleness,
            change_poll: cfg.change_poll,
        }
    }

    /// A loopback mirror for tests.
    #[cfg(test)]
    #[must_use]
    pub fn for_test(base_url: &str, max_staleness: Duration) -> Self {
        Self {
            base_url: base_url.to_string(),
            key: "2ge_test_key_0123456789abcdef".to_string(),
            max_staleness,
            change_poll: Duration::from_secs(DEFAULT_CHANGE_POLL_SECS),
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

static ACTIVE: RwLock<Option<Arc<Active>>> = RwLock::new(None);

/// Set when the mirror refused the key (`401`), so the next refresh re-reads
/// it instead of waiting for [`KEY_REFRESH`].
static KEY_REJECTED: AtomicBool = AtomicBool::new(false);

/// The live mirror, or `None` while `forge.mirror` is unset (break-glass) or
/// its key cannot be read.
#[must_use]
pub fn active() -> Option<Arc<Active>> {
    ACTIVE.read().ok()?.clone()
}

/// Whether a mirror is live in this process. A caller checks this before any
/// work of its own (a thread hop, a repo resolution), so an unset mirror
/// costs one lock read.
#[must_use]
pub fn is_active() -> bool {
    ACTIVE.read().is_ok_and(|g| g.is_some())
}

pub(crate) fn set_active(active: Option<Arc<Active>>) {
    if let Ok(mut g) = ACTIVE.write() {
        *g = active;
    }
}

/// The mirror answered `401`: re-read the key on the next refresh.
pub(crate) fn note_key_rejected() {
    KEY_REJECTED.store(true, Ordering::Relaxed);
}

/// What one refresh pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassOutcome {
    /// `forge.mirror` is unset or unusable: nothing is live.
    Off,
    /// The key could not be read: nothing is live (reason never names a key).
    KeyFailed(String),
    /// The mirror is live (re)built from the config and a freshly read key.
    Active,
    /// Nothing was due.
    Unchanged,
}

/// Reads a key from its ref (and region): the reason on failure never names
/// the key.
pub type KeyReader<'a> = dyn Fn(&KeyRef, Option<&str>) -> Result<String, String> + 'a;

/// The refresh loop's memory between passes.
#[derive(Debug, Default)]
pub struct Refresher {
    fingerprint: Option<String>,
    key_at: Option<Instant>,
    logged: HashSet<String>,
}

impl Refresher {
    fn log_once(&mut self, key: String, f: impl FnOnce(&str)) {
        if self.logged.insert(key.clone()) {
            f(&key);
        }
    }

    /// One pass against `effective`, with the key reader injected.
    pub fn pass_with(
        &mut self,
        effective: &Value,
        now: Instant,
        read_key: &KeyReader<'_>,
    ) -> PassOutcome {
        let (cfg, warnings) = parse(effective);
        for w in warnings {
            self.log_once(w, |w| log::warn!("forge_mirror: {w} — 2am#4367"));
        }
        let Some(cfg) = cfg else {
            if self.fingerprint.take().is_some() {
                log::info!(
                    "forge_mirror: forge.mirror is unset; discovery reads and wakes use \
                     today's paths — 2am#4367"
                );
            }
            self.key_at = None;
            set_active(None);
            return PassOutcome::Off;
        };
        let fp = cfg.fingerprint();
        let rejected = KEY_REJECTED.swap(false, Ordering::Relaxed);
        let due = self.fingerprint.as_deref() != Some(fp.as_str())
            || self
                .key_at
                .is_none_or(|at| now.duration_since(at) >= KEY_REFRESH)
            || rejected
            || !is_active();
        if !due {
            return PassOutcome::Unchanged;
        }
        let region = match &cfg.key_ref {
            KeyRef::Ssm(_) => crate::forge_gateway::resolve_region(cfg.region.as_deref(), &|k| {
                std::env::var(k).ok()
            }),
            KeyRef::File(_) => None,
        };
        match read_key(&cfg.key_ref, region.as_deref()) {
            Ok(key) => {
                if self.fingerprint.as_deref() != Some(fp.as_str()) {
                    log::info!(
                        "forge_mirror: discovery reads from {} within {}s, change stream every \
                         {}s — 2am#4367",
                        cfg.host,
                        cfg.max_staleness.as_secs(),
                        cfg.change_poll.as_secs()
                    );
                }
                self.fingerprint = Some(fp);
                self.key_at = Some(now);
                set_active(Some(Arc::new(Active::new(&cfg, key))));
                PassOutcome::Active
            }
            Err(e) => {
                // Keep the fingerprint unset so the next pass retries.
                self.fingerprint = None;
                self.key_at = None;
                set_active(None);
                self.log_once(format!("the mirror key could not be read: {e}"), |m| {
                    log::warn!("forge_mirror: {m}; the mirror stays off — 2am#4367");
                });
                PassOutcome::KeyFailed(e)
            }
        }
    }

    /// One pass for the workspace at `ws` against its live config.
    pub fn pass(&mut self, ws: &std::path::Path, now: Instant) -> PassOutcome {
        let effective = crate::config_resolver::resolve_effective_config(ws);
        self.pass_with(&effective, now, &|r, region| r.read(region))
    }
}
