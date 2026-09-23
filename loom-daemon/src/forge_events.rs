//! Forge event-feed consumer — observe-only (ADR-0021, Epic #8764 Phase 1,
//! issue #8765).
//!
//! # What this is
//!
//! The daemon half of the forge event plane. The operator runs a Cloudflare
//! Worker that receives GitHub App webhook deliveries, verifies their HMAC,
//! and serves a **per-host cursor feed** behind a per-host bearer key:
//!
//! ```text
//! GET {endpoint}/v1/hosts/{host}/events?after={cursor}&limit={page}
//!     -> { host_id, cursor, clamped, events[], has_more }
//! ```
//!
//! This module polls that feed, journals each page durably, persists the
//! cursor atomically, and publishes exactly one `forge.event` in-process bus
//! prompt per non-empty page. **Nothing subscribes to that topic in this
//! phase** — the consumers (work-finder tick, queue-head wake, in-flight PR
//! watch) are Phase 2, issue #8766. Phase 1 is observe-only by construction,
//! so the whole mechanism can be run against a live feed and measured before
//! any dispatch path can be affected by it.
//!
//! The Worker itself is **operator infrastructure, not Loom**: nothing in
//! this repo carries an operator URL, App id, or key, exactly as
//! `.loom/docs/observability.md` §4 describes for the telemetry backend.
//!
//! # Invariants (inherited from ADR-0014, restated because they bound this file)
//!
//! 1. **The forge is authoritative.** A feed event is a *prompt to re-query*
//!    through the existing rate-limited forge clients — never the state
//!    itself. Nothing in this module reads or writes a label, a claim, or a
//!    PR, and the bus payload it publishes is routing hints only: counts,
//!    sequence bounds and event-type names, never a copy of an event body.
//! 2. **Polling is the correctness floor.** This module never touches any
//!    existing poll cadence. A permanently dead feed is indistinguishable
//!    from the pre-webhook fleet in every correctness property; only latency
//!    differs.
//! 3. **Cursors are only trusted from host-matching feeds.** A 200 echoing a
//!    different `host_id`, or a 404 for our host, is
//!    [`ForgeEventsState::HostMismatch`] and its cursor is never applied.
//! 4. **The key never crosses a trust boundary in the clear.** It lives in a
//!    file, is re-read **every poll** (a rotation is a file swap, not a
//!    daemon restart), is sent only as `Authorization: Bearer`, and appears
//!    in no log line and on no status surface. Only its *path* is ever named.
//!
//! # Off by default
//!
//! Precedence is **env > config > default** (`config_resolver.rs`), default
//! `enabled = false`. [`spawn_task`] returns `None` with zero side effects
//! when off: no directory created, no key read, no client constructed, no
//! syscall issued. The `forgeEvents` block is deliberately **not** committed
//! to `.loom/config.json` — a committed placeholder endpoint is exactly the
//! #7815 exposure, and [`prepare`] refuses reserved placeholder domains for
//! the same reason.
//!
//! # Testable split
//!
//! - [`prepare`] — the spawn decision. Resolution + every provisioning guard,
//!   no task, no network.
//! - [`spawn_task`] — wiring only: status registration and the cadence loop.
//! - [`FeedClient::poll_once`] — one poll's entire effect (request,
//!   classification, journal, cursor, bus publication).
//!
//! Tests live in `forge_events/tests.rs` (the `health/tests.rs` precedent),
//! so the file-size ratchet measures production code on its own terms.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;

use crate::event_bus::EventBus;
use crate::observability::endpoint_policy::{reserved_placeholder_host, valid_otlp_endpoint};
use crate::types::{Event, ForgeEventsState, ForgeEventsStatus};

#[cfg(test)]
mod tests;

// ============================================================================
// Env overrides + defaults
// ============================================================================

/// `forgeEvents.enabled` env override.
pub const ENABLED_ENV: &str = "LOOM_FORGE_EVENTS_ENABLED";
/// `forgeEvents.endpoint` env override.
pub const ENDPOINT_ENV: &str = "LOOM_FORGE_EVENTS_ENDPOINT";
/// `forgeEvents.hostId` env override.
pub const HOST_ID_ENV: &str = "LOOM_FORGE_EVENTS_HOST_ID";
/// `forgeEvents.eventKeyFile` env override.
pub const EVENT_KEY_FILE_ENV: &str = "LOOM_FORGE_EVENTS_EVENT_KEY_FILE";
/// `forgeEvents.pollIntervalSecs` env override.
pub const POLL_INTERVAL_SECS_ENV: &str = "LOOM_FORGE_EVENTS_POLL_INTERVAL_SECS";
/// `forgeEvents.pageSize` env override.
pub const PAGE_SIZE_ENV: &str = "LOOM_FORGE_EVENTS_PAGE_SIZE";

/// Default poll cadence. Short on purpose: the whole point of the feed is to
/// notice sooner than the minutes-scale loops already do, and a cursor poll
/// against the operator's own Worker costs nothing on the GitHub rate limit
/// (it never touches GitHub).
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 10;
/// Default page size (events per request).
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Stretched cadence once [`BACKOFF_FAILURE_STREAK`] consecutive polls have
/// failed. A feed that is down is usually down for a provisioning reason
/// (unminted key, wrong host id, Worker not deployed), which no amount of
/// 10-second retrying fixes — and the polling floor is unaffected either way.
pub const BACKOFF_POLL_INTERVAL_SECS: u64 = 300;
/// Consecutive failures that promote any error class to
/// [`ForgeEventsState::Backoff`] and stretch the cadence. Three, not one: a
/// single failed poll across a laptop's wifi transition is noise.
pub const BACKOFF_FAILURE_STREAK: u32 = 3;

/// Hard cap on a feed response body. **Over-limit is refused, never
/// truncated** — a truncated page would be a silently incomplete page, and
/// applying its cursor would skip the events that did not fit. The cursor is
/// left untouched so the next poll re-requests the same range (an operator
/// fixing `pageSize` downward then recovers without data loss).
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Journal size cap before a single rotation to `journal.1`. Two files, never
/// more: the journal is a diagnostic tail, not an archive — the durable
/// record that matters is the cursor, and the feed's own retention window is
/// the real recovery path.
pub const JOURNAL_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Per-request timeout. Short, because a wedged feed must not hold a poll
/// slot for longer than a couple of cadence ticks.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The single bus topic this module publishes — one `Event::Generic` per
/// non-empty page. Authorized for the taxonomy by issue #8767.
pub const BUS_TOPIC: &str = "forge.event";

/// The `source` discriminator every [`BUS_TOPIC`] payload carries.
///
/// One of the three conditions #8767 places on any `Generic` prompt topic (be
/// inventoried, carry a `source`, never drive an external write without a
/// forge-verified re-read), and the value that issue names. Kebab-case, like
/// the `transcript-ingest` / `monitor-db` source tokens already in this tree.
pub const PAYLOAD_SOURCE: &str = "forge-event-feed";

// ============================================================================
// Config
// ============================================================================

/// The `.loom/config.json` `forgeEvents` block, read but not yet resolved
/// against env/defaults — same split as
/// [`crate::observability::ObservabilityConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgeEventsConfig {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub host_id: Option<String>,
    pub event_key_file: Option<String>,
    pub poll_interval_secs: Option<u64>,
    pub page_size: Option<u32>,
}

/// Read the `forgeEvents` block from `root`'s resolved config.
#[must_use]
pub fn read_config(root: &Path) -> ForgeEventsConfig {
    let config = crate::config_resolver::resolve_effective_config(root);
    let Some(block) = crate::config_resolver::get_path(&config, "forgeEvents") else {
        return ForgeEventsConfig::default();
    };
    ForgeEventsConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        endpoint: string_key(block, "endpoint"),
        host_id: string_key(block, "hostId"),
        event_key_file: string_key(block, "eventKeyFile"),
        poll_interval_secs: block
            .get("pollIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0),
        page_size: block
            .get("pageSize")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0),
    }
}

fn string_key(block: &serde_json::Value, key: &str) -> Option<String> {
    block
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| {
        matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// **env > config > default** (`false`).
#[must_use]
pub fn resolve_enabled(config: &ForgeEventsConfig) -> bool {
    env_bool(ENABLED_ENV).or(config.enabled).unwrap_or(false)
}

/// **env > config**, no default — a missing endpoint is "not provisioned",
/// which [`prepare`] reports as [`ForgeEventsState::Misconfigured`].
///
/// Deliberately a *pure precedence* resolver, like
/// [`crate::observability::resolve_endpoint`]: it answers "which tier wins",
/// never "is that value fit to send a key to". That judgement lives at the
/// point of use in [`prepare`] so the resolved value stays reportable.
#[must_use]
pub fn resolve_endpoint(config: &ForgeEventsConfig) -> Option<String> {
    env_nonempty(ENDPOINT_ENV).or_else(|| config.endpoint.clone())
}

/// **env > config**, no default.
///
/// There is deliberately no fallback to
/// [`crate::sweep_registry::host_identity`]: the feed's `host_id` is whatever
/// identity the *operator* minted this host's key against, and silently
/// guessing it would turn a provisioning mistake into a permanent, quiet
/// [`ForgeEventsState::HostMismatch`] instead of a named misconfiguration.
#[must_use]
pub fn resolve_host_id(config: &ForgeEventsConfig) -> Option<String> {
    env_nonempty(HOST_ID_ENV).or_else(|| config.host_id.clone())
}

/// **env > config > default** (`$HOME/.loom/forge-events/key`).
///
/// The host-relative default is the #5336 lesson applied up front: a key path
/// copied verbatim from another host's `$HOME` into the shared, committed
/// config is unreadable everywhere else.
#[must_use]
pub fn resolve_event_key_file(config: &ForgeEventsConfig) -> Option<String> {
    env_nonempty(EVENT_KEY_FILE_ENV)
        .or_else(|| config.event_key_file.clone())
        .or_else(|| default_state_dir().map(|dir| key_file_in(&dir)))
}

/// **env > config > default** ([`DEFAULT_POLL_INTERVAL_SECS`]).
#[must_use]
pub fn resolve_poll_interval_secs(config: &ForgeEventsConfig) -> u64 {
    std::env::var(POLL_INTERVAL_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &u64| *v > 0)
        .or(config.poll_interval_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
}

/// **env > config > default** ([`DEFAULT_PAGE_SIZE`]).
#[must_use]
pub fn resolve_page_size(config: &ForgeEventsConfig) -> u32 {
    std::env::var(PAGE_SIZE_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &u32| *v > 0)
        .or(config.page_size)
        .unwrap_or(DEFAULT_PAGE_SIZE)
}

/// The conventional per-host state directory under `home`. Pure, so the
/// layout is unit-testable without touching a real `$HOME`.
#[must_use]
pub fn state_dir_under(home: &Path) -> PathBuf {
    home.join(".loom").join("forge-events")
}

/// The key file inside a resolved state directory.
#[must_use]
pub fn key_file_in(dir: &Path) -> String {
    dir.join("key").to_string_lossy().to_string()
}

/// `$HOME/.loom/forge-events`, or `None` when no home resolves.
///
/// Refused under `cfg(test)` for the same structural reason
/// [`crate::observability`]'s ingest-key default is: this crate links every
/// `#[test]` into one binary and several modules `set_var`/`remove_var`
/// `HOME` for their own isolation, so an ambient `$HOME` must never leak into
/// another module's resolved default mid-run. Tests pass an explicit
/// directory to [`prepare`] instead.
#[cfg(not(test))]
#[must_use]
pub fn default_state_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| state_dir_under(&home))
}

#[cfg(test)]
#[must_use]
pub fn default_state_dir() -> Option<PathBuf> {
    None
}

// ============================================================================
// Resolved feed + spawn decision
// ============================================================================

/// The on-disk files one feed consumer owns. All under one directory so a
/// host's whole feed state can be inspected — or discarded — as a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedPaths {
    pub dir: PathBuf,
    /// Append-only page journal (`journal.jsonl`).
    pub journal: PathBuf,
    /// The single rotation target (`journal.1`).
    pub journal_prev: PathBuf,
    /// Durable cursor (`state.json`), written atomically.
    pub state: PathBuf,
}

impl FeedPaths {
    #[must_use]
    pub fn in_dir(dir: PathBuf) -> Self {
        FeedPaths {
            journal: dir.join("journal.jsonl"),
            journal_prev: dir.join("journal.1"),
            state: dir.join("state.json"),
            dir,
        }
    }
}

/// Everything [`FeedClient`] needs, with every guard already passed.
#[derive(Debug, Clone)]
pub struct ResolvedFeed {
    pub endpoint: String,
    pub host_id: String,
    pub key_file: PathBuf,
    pub poll_interval_secs: u64,
    pub page_size: u32,
    pub paths: FeedPaths,
}

/// The spawn decision. Pure with respect to the network: [`prepare`] reads
/// config, env and (last) the key file, and never opens a socket or creates a
/// directory.
#[derive(Debug, Clone)]
pub enum Prepared {
    /// Off by choice — no block, or `enabled: false`.
    Disabled,
    /// Opted in, but a required piece of provisioning is missing. `detail`
    /// names it and reaches `loom-daemon status` verbatim.
    Misconfigured {
        endpoint: Option<String>,
        detail: String,
    },
    /// Fully provisioned; the loop may start.
    Ready(Box<ResolvedFeed>),
}

/// `true` when `host_id` is safe to interpolate into the feed URL path.
///
/// Anything outside this set would have to be percent-encoded to mean what it
/// says, and an id that needs encoding is an id the operator did not mint —
/// so this refuses rather than encodes. Path traversal (`..`, `/`) is the
/// concrete thing being excluded.
fn host_id_is_url_safe(host_id: &str) -> bool {
    !host_id.is_empty()
        && host_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !host_id.contains("..")
}

/// Read `path` and return its trimmed contents as the event key.
///
/// Every failure (missing, unreadable, empty after trimming) yields `Err`
/// with a detail naming **the path only** — the key's contents never reach a
/// log line, an error string, or the status surface.
pub fn read_event_key(path: &Path) -> Result<String, String> {
    let shown = path.display();
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let key = contents.trim().to_string();
            if key.is_empty() {
                Err(format!("event key file {shown} is empty after trimming whitespace"))
            } else {
                Ok(key)
            }
        }
        Err(error) => Err(format!("could not read event key file {shown}: {error}")),
    }
}

/// Resolve the whole `forgeEvents` surface and apply every provisioning
/// guard, in the order that keeps the key safest:
///
/// 1. disabled ⇒ [`Prepared::Disabled`], nothing else is even read;
/// 2. missing `endpoint` / `hostId`, or a `hostId` that is not URL-safe;
/// 3. an endpoint that is not an HTTP(S) URL, or that carries credentials /
///    query / fragment (the key travels in a header, never in the URL);
/// 4. a **reserved placeholder** endpoint (`example.com` and friends, #7815)
///    — refused *before* the key file is opened, so a copy-pasted sample
///    config can never leak a real key to a documentation domain;
/// 5. an unresolvable state directory (no `$HOME`);
/// 6. an unreadable or empty key file.
///
/// `state_dir` is injected rather than resolved here so the whole decision is
/// testable against a temp directory; [`spawn_task`] passes
/// [`default_state_dir`].
#[must_use]
pub fn prepare(config: &ForgeEventsConfig, state_dir: Option<PathBuf>) -> Prepared {
    if !resolve_enabled(config) {
        return Prepared::Disabled;
    }
    let endpoint = resolve_endpoint(config);
    let Some(endpoint) = endpoint else {
        return Prepared::Misconfigured {
            endpoint: None,
            detail: "forgeEvents.endpoint not configured (set forgeEvents.endpoint or \
                     $LOOM_FORGE_EVENTS_ENDPOINT)"
                .to_string(),
        };
    };
    let Some(host_id) = resolve_host_id(config) else {
        return Prepared::Misconfigured {
            endpoint: Some(endpoint),
            detail: "forgeEvents.hostId not configured (set forgeEvents.hostId or \
                     $LOOM_FORGE_EVENTS_HOST_ID to the id the operator minted this host's \
                     event key against)"
                .to_string(),
        };
    };
    if !host_id_is_url_safe(&host_id) {
        return Prepared::Misconfigured {
            endpoint: Some(endpoint),
            detail: format!(
                "forgeEvents.hostId {host_id:?} is not URL-safe (use only letters, digits, \
                 '-', '_' and '.')"
            ),
        };
    }
    if !valid_otlp_endpoint(&endpoint) {
        // Shared with the telemetry exporter deliberately: the predicate is
        // "an endpoint whose only credential channel is a Bearer header",
        // which is exactly this feed's shape too. The OTLP-era name is
        // historical; the rule is not OTLP-specific.
        return Prepared::Misconfigured {
            endpoint: Some(endpoint),
            detail: "forgeEvents.endpoint must be an http(s) URL with no embedded credentials, \
                     query or fragment (the event key travels in the Authorization header)"
                .to_string(),
        };
    }
    if let Some(host) = reserved_placeholder_host(&endpoint) {
        return Prepared::Misconfigured {
            endpoint: Some(endpoint.clone()),
            detail: format!(
                "forgeEvents.endpoint {endpoint} points at the reserved placeholder domain \
                 {host} (RFC 2606/6761) — refusing to poll so the event key is never sent \
                 there; set a real endpoint via $LOOM_FORGE_EVENTS_ENDPOINT or \
                 .loom-local/local.json, or leave forgeEvents.enabled=false"
            ),
        };
    }
    let Some(dir) = state_dir else {
        return Prepared::Misconfigured {
            endpoint: Some(endpoint),
            detail: "could not resolve a home directory for $HOME/.loom/forge-events".to_string(),
        };
    };
    let key_file = resolve_event_key_file(config).unwrap_or_else(|| key_file_in(&dir));
    let key_file = PathBuf::from(key_file);
    if let Err(detail) = read_event_key(&key_file) {
        return Prepared::Misconfigured {
            endpoint: Some(endpoint),
            detail,
        };
    }
    Prepared::Ready(Box::new(ResolvedFeed {
        endpoint,
        host_id,
        key_file,
        poll_interval_secs: resolve_poll_interval_secs(config),
        page_size: resolve_page_size(config),
        paths: FeedPaths::in_dir(dir),
    }))
}

// ============================================================================
// Status cell
// ============================================================================

/// How a poll failed. The class is what answers "why is my cursor not
/// advancing"; [`ForgeEventsState::Backoff`] only ever narrows the *cadence*
/// question, never replaces this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedErrorClass {
    /// The request never completed (DNS, connect, TLS, timeout), or the body
    /// could not be read.
    Transport,
    /// The feed answered, but not with a page this daemon can apply: a non-2xx
    /// that is not 401/403/404, an over-cap body, a body that is not a valid
    /// page, or a cursor that moved backwards.
    Protocol,
    /// The credential is unusable — the feed answered 401/403, or the key file
    /// became unreadable/empty between polls (it is re-read every poll, so a
    /// rotation gone wrong lands here rather than looking like a transport
    /// fault).
    AuthFailed,
    /// This feed is not ours: a 404 for our host, or a 200 echoing a different
    /// `host_id`. Its cursor is never applied.
    HostMismatch,
}

impl FeedErrorClass {
    /// The stable token recorded in [`ForgeEventsStatus::last_error`].
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            FeedErrorClass::Transport => "transport",
            FeedErrorClass::Protocol => "protocol",
            FeedErrorClass::AuthFailed => "auth_failed",
            FeedErrorClass::HostMismatch => "host_mismatch",
        }
    }

    /// The un-promoted state this class maps to (below the backoff streak).
    #[must_use]
    pub fn state(self) -> ForgeEventsState {
        match self {
            FeedErrorClass::Transport | FeedErrorClass::Protocol => ForgeEventsState::Failing,
            FeedErrorClass::AuthFailed => ForgeEventsState::AuthFailed,
            FeedErrorClass::HostMismatch => ForgeEventsState::HostMismatch,
        }
    }
}

/// What one [`FeedClient::poll_once`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// A non-empty page was journaled, its cursor persisted, and one
    /// `forge.event` published.
    Page { count: usize, cursor: u64 },
    /// A successful poll with no new events. Not a fault: a quiet feed is the
    /// steady state.
    Empty { cursor: u64 },
    /// Classified failure. The cursor was not advanced.
    Failed(FeedErrorClass),
}

/// Live, always-readable record of the consumer's state, written by
/// [`FeedClient::poll_once`] and read back by
/// [`crate::ipc::build_daemon_status`].
///
/// Mirrors [`crate::observability::ExportStatus`]'s process-global pattern
/// rather than threading an `Arc` through the IPC server.
#[derive(Debug)]
pub struct FeedStatus {
    inner: Mutex<ForgeEventsStatus>,
    /// The configured (un-stretched) cadence, kept outside the wire type so
    /// the backoff promotion has a base to return to.
    base_interval_secs: u64,
}

// Allow expect_used: a poisoned status mutex means another thread panicked
// while holding it — unrecoverable, matching the crash-on-poison policy
// `observability::ExportStatus` and `ipc` already use.
#[allow(clippy::expect_used)]
impl FeedStatus {
    /// A cell for a consumer starting now against `feed`, resuming from
    /// `cursor`.
    #[must_use]
    pub fn started(feed: &ResolvedFeed, cursor: u64) -> Self {
        FeedStatus {
            inner: Mutex::new(ForgeEventsStatus {
                state: ForgeEventsState::Connecting,
                endpoint: Some(feed.endpoint.clone()),
                host_id: Some(feed.host_id.clone()),
                cursor,
                started_at: Some(Utc::now()),
                poll_interval_secs: feed.poll_interval_secs,
                ..ForgeEventsStatus::default()
            }),
            base_interval_secs: feed.poll_interval_secs,
        }
    }

    /// A cell for a consumer that never started because provisioning could
    /// not be resolved.
    #[must_use]
    pub fn misconfigured(endpoint: Option<String>, detail: String) -> Self {
        FeedStatus {
            inner: Mutex::new(ForgeEventsStatus::misconfigured(endpoint, detail)),
            base_interval_secs: 0,
        }
    }

    /// The current record.
    #[must_use]
    pub fn snapshot(&self) -> ForgeEventsStatus {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ForgeEventsStatus> {
        self.inner
            .lock()
            .expect("forge_events status mutex poisoned")
    }

    /// Record a successful poll that applied `count` events and left the
    /// durable cursor at `cursor`. Clears the failure streak and returns the
    /// cadence to its configured value — the backoff promotion is symmetric,
    /// one success is enough to come back.
    pub fn record_success(&self, count: usize, cursor: u64) {
        let now = Utc::now();
        let mut guard = self.lock();
        guard.state = ForgeEventsState::Healthy;
        guard.cursor = cursor;
        guard.last_poll_at = Some(now);
        guard.consecutive_failures = 0;
        guard.last_error = None;
        guard.last_error_detail = None;
        guard.poll_interval_secs = self.base_interval_secs;
        if count > 0 {
            guard.last_page_at = Some(now);
            guard.pages_observed = guard.pages_observed.saturating_add(1);
            guard.events_observed = guard
                .events_observed
                .saturating_add(count.try_into().unwrap_or(u64::MAX));
        }
    }

    /// Record a failed poll. At [`BACKOFF_FAILURE_STREAK`] the state is
    /// promoted to [`ForgeEventsState::Backoff`] and the cadence stretches to
    /// [`BACKOFF_POLL_INTERVAL_SECS`]; `last_error` keeps the class either
    /// way, so the cause survives the promotion.
    pub fn record_failure(&self, class: FeedErrorClass, detail: String) {
        let now = Utc::now();
        let mut guard = self.lock();
        guard.consecutive_failures = guard.consecutive_failures.saturating_add(1);
        guard.last_poll_at = Some(now);
        guard.last_error = Some(class.token().to_string());
        guard.last_error_detail = Some(detail);
        guard.last_error_at = Some(now);
        if guard.consecutive_failures >= BACKOFF_FAILURE_STREAK {
            guard.state = ForgeEventsState::Backoff;
            guard.poll_interval_secs = BACKOFF_POLL_INTERVAL_SECS;
        } else {
            guard.state = class.state();
            guard.poll_interval_secs = self.base_interval_secs;
        }
    }

    /// The cadence to sleep for before the next poll.
    #[must_use]
    pub fn current_interval_secs(&self) -> u64 {
        let guard = self.lock();
        if guard.poll_interval_secs == 0 {
            self.base_interval_secs
        } else {
            guard.poll_interval_secs
        }
    }
}

/// Process-global status handle, registered by [`spawn_task`] when the loop
/// starts **or** when provisioning failed. Unset means off by choice.
static GLOBAL_STATUS: std::sync::OnceLock<Arc<FeedStatus>> = std::sync::OnceLock::new();

/// Register the consumer's status handle as the process-global. Idempotent:
/// the first registration wins (one consumer per process).
pub fn register_global_status(status: Arc<FeedStatus>) {
    let _ = GLOBAL_STATUS.set(status);
}

/// This process's feed status — **always** an answer, never silence.
/// Unregistered ⇒ [`ForgeEventsStatus::disabled`].
#[must_use]
pub fn global_status() -> ForgeEventsStatus {
    GLOBAL_STATUS
        .get()
        .map_or_else(ForgeEventsStatus::disabled, |status| status.snapshot())
}

// ============================================================================
// Durable state: cursor + journal
// ============================================================================

/// The durable cursor record. Diagnostic in the sense that losing it costs
/// re-delivery, not correctness: a restart from an older cursor re-renders
/// pages that are still inside the feed's retention window, and every
/// consumer of `forge.event` is a *prompt* to re-query an authoritative
/// forge, so a replayed prompt is at worst a redundant re-check.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
struct CursorState {
    host_id: String,
    cursor: u64,
    updated_at: String,
}

/// Read the durable cursor for `host_id`.
///
/// **A torn, absent, or foreign file reads as `0`** — start of the retention
/// window — rather than as an error: there is no state worth failing a daemon
/// start over here, and a cursor belonging to a *different* `host_id` must
/// never be adopted (that is invariant 3 applied to our own disk).
#[must_use]
pub fn read_cursor(path: &Path, host_id: &str) -> u64 {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return 0;
    };
    match serde_json::from_str::<CursorState>(&raw) {
        Ok(state) if state.host_id == host_id => state.cursor,
        Ok(state) => {
            log::warn!(
                "forge_events: cursor file {} belongs to host_id {:?}, not {host_id:?} — \
                 restarting from the beginning of the feed's retention window",
                path.display(),
                state.host_id
            );
            0
        }
        Err(_) => 0,
    }
}

/// Persist `cursor` atomically: write a sibling temp file, then rename over
/// the target. A crash mid-write therefore leaves either the old file or the
/// new one, never a half-written one — and even a torn file reads as `0`.
pub fn persist_cursor(path: &Path, host_id: &str, cursor: u64) -> Result<(), String> {
    let Some(dir) = path.parent() else {
        return Err(format!("cursor path {} has no parent", path.display()));
    };
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let state = CursorState {
        host_id: host_id.to_string(),
        cursor,
        updated_at: Utc::now().to_rfc3339(),
    };
    let body = serde_json::to_string(&state).map_err(|e| format!("serialize cursor: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename into {}: {e}", path.display()))
}

/// Append one page record to the journal, rotating first when the file would
/// exceed [`JOURNAL_MAX_BYTES`].
///
/// Rotation is a single rename: the full `journal.jsonl` becomes `journal.1`
/// (replacing any previous one) and a fresh journal starts. The most recent
/// *full* file is therefore always retained — the cap bounds disk use without
/// ever discarding the newest history first.
pub fn append_journal(paths: &FeedPaths, line: &str) -> Result<(), String> {
    use std::io::Write;

    std::fs::create_dir_all(&paths.dir)
        .map_err(|e| format!("create {}: {e}", paths.dir.display()))?;
    let current = std::fs::metadata(&paths.journal)
        .map(|m| m.len())
        .unwrap_or(0);
    if current > 0 && current + line.len() as u64 + 1 > JOURNAL_MAX_BYTES {
        std::fs::rename(&paths.journal, &paths.journal_prev)
            .map_err(|e| format!("rotate {}: {e}", paths.journal.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.journal)
        .map_err(|e| format!("open {}: {e}", paths.journal.display()))?;
    writeln!(file, "{line}").map_err(|e| format!("append {}: {e}", paths.journal.display()))
}

// ============================================================================
// Feed client
// ============================================================================

/// The subset of a feed page this daemon reads. Unknown fields are ignored
/// and every field is `#[serde(default)]`, so a Worker that grows its
/// response never breaks an older daemon.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FeedPage {
    /// The `host_id` the feed says this page belongs to. `None` is treated as
    /// a mismatch, not as "unchecked" — see [`FeedClient::apply_page`].
    #[serde(default)]
    pub host_id: Option<String>,
    /// The cursor to send as `after` on the next poll.
    #[serde(default)]
    pub cursor: Option<u64>,
    /// The feed clamped our `after` because it fell outside its retention
    /// window: some events were never delivered. Journaled and warned about;
    /// the polling floor is what covers the gap.
    #[serde(default)]
    pub clamped: bool,
    /// The page's events, journaled verbatim and summarized on the bus.
    #[serde(default)]
    pub events: Vec<serde_json::Value>,
    /// More events are already queued past this page. Phase 1 simply catches
    /// up on the next tick.
    #[serde(default)]
    pub has_more: bool,
}

fn event_seq(event: &serde_json::Value) -> u64 {
    event
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn event_type(event: &serde_json::Value) -> Option<&str> {
    event.get("type").and_then(serde_json::Value::as_str)
}

/// Build the `forge.event` bus payload for one page: **routing hints only**.
///
/// Counts, sequence bounds and the distinct event-type names — enough for a
/// Phase-2 subscriber to decide *whether* to re-query the forge, and nothing
/// it could mistake for the forge's own state. No repo, issue or PR number,
/// no title, no body, no actor: a subscriber that wanted those would have to
/// go ask GitHub, which is the entire point (ADR-0014 invariant 1).
#[must_use]
pub fn page_payload(host_id: &str, events: &[serde_json::Value]) -> serde_json::Value {
    let mut types: Vec<&str> = events.iter().filter_map(event_type).collect();
    types.sort_unstable();
    types.dedup();
    serde_json::json!({
        "source": PAYLOAD_SOURCE,
        "host_id": host_id,
        "count": events.len(),
        "first_seq": events.first().map(event_seq).unwrap_or(0),
        "last_seq": events.last().map(event_seq).unwrap_or(0),
        "types": types,
    })
}

/// One host's feed consumer: the HTTP client, the durable paths, the cursor,
/// the bus handle and the status cell.
#[derive(Debug)]
pub struct FeedClient {
    http: reqwest::Client,
    feed: ResolvedFeed,
    bus: EventBus,
    status: Arc<FeedStatus>,
    cursor: u64,
}

/// Build the outbound HTTP client: bounded timeout, **redirects disabled**.
///
/// Redirects are off because a redirect is an instruction from the network to
/// re-send an `Authorization: Bearer` header somewhere the operator did not
/// configure. A 3xx therefore falls through to the ordinary non-2xx path and
/// is reported as a protocol failure, which is what it is.
pub fn build_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("could not construct HTTP client: {error}"))
}

impl FeedClient {
    /// Construct a consumer, resuming from whatever cursor is durably
    /// recorded for this `host_id`.
    pub fn new(feed: ResolvedFeed, bus: EventBus, status: Arc<FeedStatus>) -> Result<Self, String> {
        Ok(Self::with_http(build_http_client()?, feed, bus, status))
    }

    /// As [`FeedClient::new`], with an already-constructed HTTP client, so
    /// [`spawn_task`] can fail *before* registering a running status.
    #[must_use]
    pub fn with_http(
        http: reqwest::Client,
        feed: ResolvedFeed,
        bus: EventBus,
        status: Arc<FeedStatus>,
    ) -> Self {
        let cursor = read_cursor(&feed.paths.state, &feed.host_id);
        FeedClient {
            http,
            feed,
            bus,
            status,
            cursor,
        }
    }

    /// The cursor this client would next poll from.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The URL of the next poll.
    #[must_use]
    pub fn request_url(&self) -> String {
        format!(
            "{}/v1/hosts/{}/events?after={}&limit={}",
            self.feed.endpoint.trim_end_matches('/'),
            self.feed.host_id,
            self.cursor,
            self.feed.page_size
        )
    }

    fn fail(&self, class: FeedErrorClass, detail: String) -> PollOutcome {
        log::debug!("forge_events: poll failed ({}) — {detail}", class.token());
        self.status.record_failure(class, detail);
        PollOutcome::Failed(class)
    }

    /// Perform exactly one poll and apply its entire effect.
    ///
    /// The key is re-read here, on every call, so a key rotation is a file
    /// swap rather than a daemon restart.
    ///
    /// On a non-empty page the order is **journal append → atomic cursor
    /// persist → bus publish**, and it matters: the journal is the record of
    /// what arrived, the cursor is the promise not to ask for it again, and
    /// the prompt is only worth sending once both are on disk. Both writes
    /// are diagnostic — a failure in either is logged and the page still
    /// advances in memory, because a restart re-renders from the last durable
    /// cursor and the feed's retention window covers the gap.
    pub async fn poll_once(&mut self) -> PollOutcome {
        let key = match read_event_key(&self.feed.key_file) {
            Ok(key) => key,
            Err(detail) => return self.fail(FeedErrorClass::AuthFailed, detail),
        };
        let url = self.request_url();
        let response = match self.http.get(&url).bearer_auth(&key).send().await {
            Ok(response) => response,
            Err(error) => return self.fail(FeedErrorClass::Transport, error.to_string()),
        };
        let status = response.status();
        if !status.is_success() {
            return match status.as_u16() {
                401 | 403 => self.fail(
                    FeedErrorClass::AuthFailed,
                    format!(
                        "feed rejected this host's event key with HTTP {} (key file: {})",
                        status.as_u16(),
                        self.feed.key_file.display()
                    ),
                ),
                404 => self.fail(
                    FeedErrorClass::HostMismatch,
                    format!(
                        "feed has no host {:?} (HTTP 404) — this host's key may be minted \
                         under a different id",
                        self.feed.host_id
                    ),
                ),
                other => self.fail(FeedErrorClass::Protocol, format!("feed answered HTTP {other}")),
            };
        }
        let body = match read_capped_body(response, MAX_RESPONSE_BYTES).await {
            Ok(body) => body,
            Err(BodyError::TooLarge) => {
                return self.fail(
                    FeedErrorClass::Protocol,
                    format!(
                        "feed response exceeded the {MAX_RESPONSE_BYTES}-byte cap — refusing \
                         the page rather than truncating it; the cursor is unchanged (lower \
                         forgeEvents.pageSize)"
                    ),
                )
            }
            Err(BodyError::Transport(detail)) => {
                return self.fail(FeedErrorClass::Transport, detail)
            }
        };
        let page: FeedPage = match serde_json::from_str(&body) {
            Ok(page) => page,
            Err(error) => {
                return self.fail(
                    FeedErrorClass::Protocol,
                    format!("feed response is not a valid page: {error}"),
                )
            }
        };
        self.apply_page(page)
    }

    /// Validate and apply a parsed page. Split out of [`Self::poll_once`] so
    /// the host-echo, cursor-monotonicity and durability rules are testable
    /// without a socket.
    pub fn apply_page(&mut self, page: FeedPage) -> PollOutcome {
        // Invariant 3: a feed that is not ours cannot move our cursor, even
        // when it answers 200 and looks perfectly well-formed.
        match page.host_id.as_deref() {
            Some(echoed) if echoed != self.feed.host_id => {
                return self.fail(
                    FeedErrorClass::HostMismatch,
                    format!(
                        "feed echoed host_id {echoed:?} but this daemon polls as {:?} — \
                         refusing the page and its cursor",
                        self.feed.host_id
                    ),
                )
            }
            None => {
                return self.fail(
                    FeedErrorClass::HostMismatch,
                    "feed page carried no host_id to cross-check — refusing the page and its \
                     cursor"
                        .to_string(),
                )
            }
            Some(_) => {}
        }
        let Some(cursor) = page.cursor else {
            return self.fail(FeedErrorClass::Protocol, "feed page carried no cursor".to_string());
        };
        if cursor < self.cursor {
            return self.fail(
                FeedErrorClass::Protocol,
                format!(
                    "feed cursor moved backwards ({} -> {cursor}) — refusing the page",
                    self.cursor
                ),
            );
        }
        if page.events.is_empty() {
            // A quiet feed is a healthy feed. A forward cursor on an empty
            // page (the feed clamped us past expired events) is still worth
            // persisting: it is exactly the range we must not re-request.
            if cursor > self.cursor {
                self.cursor = cursor;
                if let Err(detail) =
                    persist_cursor(&self.feed.paths.state, &self.feed.host_id, cursor)
                {
                    log::warn!("forge_events: cursor not persisted — {detail}");
                }
            }
            self.status.record_success(0, self.cursor);
            return PollOutcome::Empty {
                cursor: self.cursor,
            };
        }
        let count = page.events.len();
        let record = serde_json::json!({
            "at": Utc::now().to_rfc3339(),
            "host_id": self.feed.host_id,
            "after": self.cursor,
            "cursor": cursor,
            "count": count,
            "clamped": page.clamped,
            "has_more": page.has_more,
            "events": page.events,
        });
        if let Err(detail) = append_journal(&self.feed.paths, &record.to_string()) {
            log::warn!("forge_events: page not journaled — {detail}");
        }
        self.cursor = cursor;
        if let Err(detail) = persist_cursor(&self.feed.paths.state, &self.feed.host_id, cursor) {
            log::warn!("forge_events: cursor not persisted — {detail}");
        }
        let payload = page_payload(&self.feed.host_id, &page.events);
        // `NoSubscribers` is the *expected* result in Phase 1 — there is no
        // subscriber yet, by design — so it is not an error and not logged
        // above debug.
        if let Err(error) = self.bus.publish(Event::Generic {
            topic: BUS_TOPIC.to_string(),
            payload,
        }) {
            log::debug!("forge_events: {BUS_TOPIC} published with {error}");
        }
        if page.clamped {
            log::warn!(
                "forge_events: feed clamped our cursor — events older than the retention \
                 window were skipped (the polling floor still covers them)"
            );
        }
        self.status.record_success(count, cursor);
        PollOutcome::Page { count, cursor }
    }
}

enum BodyError {
    TooLarge,
    Transport(String),
}

/// Read at most `limit` bytes, **refusing** anything longer.
///
/// Streams chunk by chunk rather than calling `Response::text()`, which would
/// buffer the whole body first and make the cap a post-hoc truncation instead
/// of a real bound. Exceeding the cap is an error, not a truncation: half a
/// page is not a page.
async fn read_capped_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<String, BodyError> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > limit {
                    return Err(BodyError::TooLarge);
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => return Err(BodyError::Transport(error.to_string())),
        }
    }
    String::from_utf8(buf).map_err(|_| BodyError::Transport("response body is not UTF-8".into()))
}

// ============================================================================
// Wiring
// ============================================================================

/// Start the feed consumer, or return `None` when it is off or unprovisioned.
///
/// Wiring only — every decision it makes belongs to [`prepare`]. The loop
/// polls first and sleeps after, so a daemon start is visible on the feed
/// immediately rather than one cadence later.
#[must_use]
pub fn spawn_task(
    config: &ForgeEventsConfig,
    bus: &EventBus,
) -> Option<tokio::task::JoinHandle<()>> {
    match prepare(config, default_state_dir()) {
        Prepared::Disabled => {
            log::debug!("forge_events: disabled (set forgeEvents.enabled=true to consume a feed)");
            None
        }
        Prepared::Misconfigured { endpoint, detail } => {
            log::warn!("forge_events: enabled but {detail} — not polling");
            register_global_status(Arc::new(FeedStatus::misconfigured(endpoint, detail)));
            None
        }
        Prepared::Ready(feed) => {
            // The HTTP client is built BEFORE the running status is
            // registered, so a client-construction failure reports
            // `misconfigured` rather than a phantom `connecting` loop.
            let http = match build_http_client() {
                Ok(http) => http,
                Err(detail) => {
                    log::warn!("forge_events: {detail} — not polling");
                    register_global_status(Arc::new(FeedStatus::misconfigured(
                        Some(feed.endpoint.clone()),
                        detail,
                    )));
                    return None;
                }
            };
            let cursor = read_cursor(&feed.paths.state, &feed.host_id);
            let status = Arc::new(FeedStatus::started(&feed, cursor));
            register_global_status(status.clone());
            log::info!(
                "forge_events: enabled (endpoint={}, host_id={}, cursor={cursor}, \
                 poll_interval={}s, page_size={})",
                feed.endpoint,
                feed.host_id,
                feed.poll_interval_secs,
                feed.page_size
            );
            let mut client = FeedClient::with_http(http, *feed, bus.clone(), status.clone());
            Some(tokio::spawn(async move {
                loop {
                    client.poll_once().await;
                    tokio::time::sleep(Duration::from_secs(status.current_interval_secs())).await;
                }
            }))
        }
    }
}
