//! Forge event-plane client (`loom-daemon` ↔ webhook worker feed), ADR-0021.
//!
//! The operator's webhook worker (see 2am `infra/loom-events`) turns GitHub
//! App deliveries into a per-host cursor feed. This module is the daemon
//! half: it pulls that feed on an operator-configured cadence and turns each
//! page into (1) a durable journal (one JSON line per event, size-capped and
//! rotated), (2) a status snapshot on the daemon `status` surface (cursor,
//! state, last error, live error count), and (3) a `forge.event` publication
//! on the in-process event bus when a non-empty page lands.
//!
//! # Invariants (ADR-0014, unchanged by this module)
//! - The forge (labels/claims via the API) is **authoritative**. A feed
//!   event is a *prompt to re-query*, never the truth: nothing here writes
//!   to GitHub. Consumers re-read issue/PR state through the existing
//!   rate-limited clients, exactly as the timer tick already does.
//! - Polling is the **correction floor**. This module only makes early-tick
//!   prompts possible; it never stretches, replaces, or disables an existing
//!   poll cadence. Phase 1 is observe-only by construction (the early-tick
//!   consumers are Phase 2, ADR-0021).
//!
//! # Configuration (`forgeEvents` block, `.loom/config.json`)
//! | key | env override | default |
//! |-----|--------------|---------|
//! | `enabled` | `LOOM_FORGE_EVENTS_ENABLED` | `false` |
//! | `endpoint` | `LOOM_FORGE_EVENTS_ENDPOINT` | — (required when enabled) |
//! | `hostId` | `LOOM_FORGE_EVENTS_HOST_ID` | — (required when enabled) |
//! | `eventKeyFile` | `LOOM_FORGE_EVENTS_KEY_FILE` | `~/.loom/forge-events/key` |
//! | `pollIntervalSecs` | `LOOM_FORGE_EVENTS_POLL_INTERVAL_SECS` | `10` |
//! | `pageSize` | `LOOM_FORGE_EVENTS_PAGE_SIZE` | `100` |
//!
//! The event key itself is secrets-only: a file (chmod 600 at mint time),
//! re-read on **every** poll so a rotation is a file swap, never a daemon
//! restart. It is never logged and never leaves the daemon except as the
//! `Authorization: Bearer <key>` header on the feed request.
//!
//! # Failure posture
//! `state` on the status surface is always an answer: `disabled` (off by
//! config), `misconfigured` (on, but a spawn-time check failed —
//! endpoint/hostId missing at every tier, a reserved-placeholder endpoint,
//! or an unreadable key file), `connecting` (spawned, first poll not yet
//! completed), `failing` (transport/5xx/malformed-body classes while the
//! streak is below the stretch threshold), `auth_failed` (401/403 — key
//! mismatch; the key file is re-read every poll, so a rotation heals it),
//! `host_mismatch` (404, or the feed echoes a different host id — **no
//! cursor from it is trusted**), `backoff` (the *promotion* of any error
//! class once its streak reaches [`BACKOFF_AFTER_ERRORS`] — cadence
//! stretched to [`BACKOFF_INTERVAL`], class kept in `last_error`), or
//! `healthy` (last poll succeeded — an empty page is a success). Every error
//! class self-heals on the next success without a restart.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::event_bus::EventBus;

// ----------------------------------------------------------------------
// defaults and policy constants

/// `forgeEvents.enabled` env override.
pub const ENABLED_ENV: &str = "LOOM_FORGE_EVENTS_ENABLED";
/// `forgeEvents.endpoint` env override.
pub const ENDPOINT_ENV: &str = "LOOM_FORGE_EVENTS_ENDPOINT";
/// `forgeEvents.hostId` env override.
pub const HOST_ID_ENV: &str = "LOOM_FORGE_EVENTS_HOST_ID";
/// `forgeEvents.eventKeyFile` env override.
pub const KEY_FILE_ENV: &str = "LOOM_FORGE_EVENTS_KEY_FILE";
/// `forgeEvents.pollIntervalSecs` env override.
pub const POLL_INTERVAL_ENV: &str = "LOOM_FORGE_EVENTS_POLL_INTERVAL_SECS";
/// `forgeEvents.pageSize` env override.
pub const PAGE_SIZE_ENV: &str = "LOOM_FORGE_EVENTS_PAGE_SIZE";

/// Default poll cadence (seconds). The feed is a prompt source, not a
/// correctness source, so the default errs fast; operators with a chatty
/// fleet can widen it (it never replaces an existing poll cadence).
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 10;
/// Default page size. The worker caps pages at `EVENT_PAGE_MAX` (500); 100
/// keeps a single poll small even for a bursty fleet.
pub const DEFAULT_PAGE_SIZE: usize = 100;
/// Any single feed response above this is refused, never truncated (a
/// truncated JSON page would desync the cursor).
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Per-request timeout — a wedged endpoint must never hold a feed tick.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive failures before the cadence stretches to [`BACKOFF_INTERVAL`].
const BACKOFF_AFTER_ERRORS: u32 = 3;
/// Stretched cadence while failing: a dead endpoint is not worth ten-second
/// error spam, and a rotated key is not going to land in the next minute.
pub const BACKOFF_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Journal size cap before rotation. 10 MiB is hours of a healthy fleet at
/// 100 events/page; `journal.1` keeps the most-recent full file as a
/// best-effort audit tail.
pub const JOURNAL_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Relative state directory under `~/.loom`.
const STATE_DIR_REL: &str = "forge-events";
const JOURNAL_FILE: &str = "journal.jsonl";
const JOURNAL_ROTATED: &str = "journal.1";
const STATE_FILE: &str = "state.json";
const DEFAULT_KEY_FILE_REL: &str = "key";
/// Response classes meaning "this host's credential is wrong" (not network
/// flake, and not "worker doesn't know this host" — that is 404).
const AUTH_HTTP: [u16; 2] = [401, 403];
/// Response class meaning "valid credential, unknown host".
const HOST_HTTP: u16 = 404;

// ----------------------------------------------------------------------
// configuration (read, then resolved)

/// The `.loom/config.json` `forgeEvents` block, read but not yet resolved
/// against env/defaults (see the `resolve_*` functions — the same
/// read-then-resolve split as [`crate::observability::ObservabilityConfig`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgeEventsConfig {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub host_id: Option<String>,
    pub event_key_file: Option<String>,
    pub poll_interval_secs: Option<u64>,
    pub page_size: Option<usize>,
}

/// Read the `forgeEvents` block from `root`'s resolved config
/// (`config_resolver::resolve_effective_config`), same pattern as
/// [`crate::observability::read_config`].
#[must_use]
pub fn read_config(root: &Path) -> ForgeEventsConfig {
    let config = crate::config_resolver::resolve_effective_config(root);
    let Some(block) = crate::config_resolver::get_path(&config, "forgeEvents") else {
        return ForgeEventsConfig::default();
    };
    ForgeEventsConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        endpoint: string_field(block.get("endpoint")),
        host_id: string_field(block.get("hostId")),
        event_key_file: string_field(block.get("eventKeyFile")),
        poll_interval_secs: block
            .get("pollIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0),
        page_size: block
            .get("pageSize")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v > 0),
    }
}

fn string_field(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(serde_json::Value::as_str)
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

/// **env > config > default** (`false`). A fleet that never opts in must
/// behave exactly as it did before this module existed.
#[must_use]
pub fn resolve_enabled(config: &ForgeEventsConfig) -> bool {
    env_bool(ENABLED_ENV).or(config.enabled).unwrap_or(false)
}

/// **env > config**, no built-in default — a missing endpoint means "not
/// configured", which [`spawn_task`] reports as `misconfigured` (when
/// enabled) rather than guessing a URL.
#[must_use]
pub fn resolve_endpoint(config: &ForgeEventsConfig) -> Option<String> {
    env_nonempty(ENDPOINT_ENV).or(config.endpoint.clone())
}

/// **env > config**, no built-in default. A host id is an operator-provisioned
/// identity, never something a daemon may synthesize: the worker keys feeds
/// on it, and a wrong identity is a `host_mismatch` with its cursors untrusted.
#[must_use]
pub fn resolve_host_id(config: &ForgeEventsConfig) -> Option<String> {
    env_nonempty(HOST_ID_ENV).or(config.host_id.clone())
}

/// **env > config > default** (`~/.loom/forge-events/key`).
#[must_use]
pub fn resolve_key_file(config: &ForgeEventsConfig) -> Option<PathBuf> {
    env_nonempty(KEY_FILE_ENV).map(PathBuf::from).or_else(|| {
        config
            .event_key_file
            .clone()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| {
                    Path::new(&home)
                        .join(STATE_DIR_REL)
                        .join(DEFAULT_KEY_FILE_REL)
                })
            })
    })
}

/// **env > config > default** ([`DEFAULT_POLL_INTERVAL_SECS`]).
#[must_use]
pub fn resolve_poll_interval(config: &ForgeEventsConfig) -> u64 {
    std::env::var(POLL_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &u64| *v > 0)
        .or(config.poll_interval_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
}

/// **env > config > default** ([`DEFAULT_PAGE_SIZE`]).
#[must_use]
pub fn resolve_page_size(config: &ForgeEventsConfig) -> usize {
    std::env::var(PAGE_SIZE_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &usize| *v > 0)
        .or(config.page_size)
        .unwrap_or(DEFAULT_PAGE_SIZE)
}

// ----------------------------------------------------------------------
// status surface

/// What the fetch loop can truthfully report about the feed link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeEventsState {
    /// Off by config (`enabled: false` / absent block) — the deliberately
    /// silent case, the same answer-class as observability's `disabled`.
    Disabled,
    /// Enabled, but a spawn-time check failed: endpoint/hostId missing at
    /// every tier, a reserved-placeholder endpoint, or an unreadable key
    /// file — never fetches. The status detail names the missing piece.
    Misconfigured,
    /// Spawned; the first poll has not completed yet.
    Connecting,
    /// Repeated fetch failures of an uncategorised kind (transport error,
    /// 5xx, non-JSON body, over-limit response, a backwards cursor — the
    /// classes that self-correct on the next try) while the streak is below
    /// the stretch threshold. `last_error` carries the class.
    Failing,
    /// Last poll answered 401/403: key mismatch (or the worker's key table
    /// does not contain this host). Self-heals on rotation — the key file is
    /// re-read every poll.
    AuthFailed,
    /// Last poll answered 404, or the feed echoed a `host_id` that differs
    /// from ours: identity is wrong, and **no cursor from it is trusted**.
    HostMismatch,
    /// The error streak (any class) reached [`BACKOFF_AFTER_ERRORS`]: a
    /// *promotion* of the class underneath, which stays readable in
    /// `last_error`; cadence stretched to [`BACKOFF_INTERVAL`] until the
    /// next success (a dead endpoint is not worth ten-second error spam).
    Backoff,
    /// Present and healthy: last poll succeeded (an empty page is a success).
    Healthy,
}

/// The daemon `status` surface for the forge event plane. Always an answer:
/// a daemon of this vintage with the feed off reports `disabled` — never
/// silence — so a watch loop can assert "off" instead of guessing.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ForgeEventsStatus {
    pub state: ForgeEventsState,
    /// Operator-provisioned identity, echoed for cross-checks.
    pub host_id: Option<String>,
    /// Feed base URL. Deliberately not the key — the key never appears on
    /// the status surface.
    pub endpoint: Option<String>,
    /// Last durable cursor (persisted; survives restarts). `0` before the
    /// first successful poll.
    pub cursor: u64,
    /// RFC 3339 UTC of the last successful poll, or `null`.
    pub last_success_at: Option<String>,
    /// Human-facing reason for the most recent failure, or `null` when
    /// healthy. Bounded — this lands in status JSON.
    pub last_error: Option<String>,
    /// Consecutive failures without a success in between (0 when healthy).
    pub consecutive_errors: u32,
    /// Events journaled since process start (the journal itself is on disk;
    /// this is the live counter an operator watches during bring-up).
    pub events_journaled: u64,
    /// The cadence this loop currently sleeps (normal or backoff).
    pub poll_interval_secs: u64,
}

impl ForgeEventsStatus {
    fn new(
        state: ForgeEventsState,
        host_id: Option<String>,
        endpoint: Option<String>,
        poll_interval_secs: u64,
    ) -> Self {
        Self {
            state,
            host_id,
            endpoint,
            cursor: 0,
            last_success_at: None,
            last_error: None,
            consecutive_errors: 0,
            events_journaled: 0,
            poll_interval_secs,
        }
    }
}

/// Process-global status cell, registered by [`spawn_task`] for every daemon
/// of this vintage — including the disabled and misconfigured shapes, so the
/// `status` command is always answered with a reason, never silence.
static GLOBAL_STATUS: std::sync::OnceLock<Arc<Mutex<ForgeEventsStatus>>> =
    std::sync::OnceLock::new();

/// Register the status cell. Idempotent: first registration wins (one feed
/// client per process).
fn register_global_status(cell: Arc<Mutex<ForgeEventsStatus>>) {
    let _ = GLOBAL_STATUS.set(cell);
}

/// This process's forge-event-plane status — always an answer. Unset only
/// before [`spawn_task`] ran (daemon start ordering); reads as `disabled`.
#[must_use]
pub fn global_status() -> ForgeEventsStatus {
    match GLOBAL_STATUS.get() {
        Some(cell) => cell.lock().expect("forge_events status poisoned").clone(),
        None => ForgeEventsStatus::new(
            ForgeEventsState::Disabled,
            None,
            None,
            DEFAULT_POLL_INTERVAL_SECS,
        ),
    }
}

// ----------------------------------------------------------------------
// durable state (under `~/.loom/forge-events/`)

/// Atomically write `state.json` (the cursor). A crash mid-write must never
/// leave a torn cursor behind: write `state.json.tmp`, `rename` over.
fn persist_cursor(dir: &Path, cursor: u64) -> std::io::Result<()> {
    let body = serde_json::json!({ "cursor": cursor, "updated_at": rfc3339_now() });
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    std::fs::write(&tmp, body.to_string())?;
    std::fs::rename(&tmp, dir.join(STATE_FILE))
}

/// Read the durable cursor; a missing or torn file is `0` (replay from the
/// worker's oldest retained event — the worker clamps, this is the
/// documented "cursor fell out of retention" shape).
fn load_cursor(dir: &Path) -> u64 {
    let Ok(raw) = std::fs::read_to_string(dir.join(STATE_FILE)) else {
        return 0;
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("cursor").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

/// Read the event key, trimmed (a trailing newline in a minted key would
/// otherwise 401 forever — keys are exactly `key-<64 hex>`). Re-read on
/// every poll by the caller, so a rotation is a file swap.
fn load_key(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("key file {} unreadable: {e}", path.display()))?;
    let key = raw.trim();
    if key.is_empty() {
        return Err(format!("key file {} is empty", path.display()));
    }
    Ok(key.to_string())
}

fn rfc3339_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `s` clipped to `max` bytes (on a char boundary), `…` appended when cut.
/// Status-bearing strings are operator-visible; keep them report-shaped.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ----------------------------------------------------------------------
// feed client (one poll's worth of effect, unit-testable without a daemon)

/// One poll's worth of effect, parameterized over the few things production
/// fixes: the endpoint URL, the host id, the key file, and the state dir.
/// [`spawn_task`] builds the production one; tests build their own against
/// an in-process HTTP server.
pub(crate) struct FeedClient {
    pub(crate) http: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) host_id: String,
    pub(crate) key_file: PathBuf,
    pub(crate) state_dir: PathBuf,
    pub(crate) page_size: usize,
    /// Journal size cap before rotation (production: [`JOURNAL_MAX_BYTES`]).
    pub(crate) journal_max_bytes: u64,
    /// The in-process bus this client publishes non-empty pages to. Cloning
    /// an `EventBus` clones its `broadcast::Sender` (see the impl in
    /// [`event_bus`]), so holding it here is cheap.
    pub(crate) bus: crate::event_bus::EventBus,
    status: Arc<Mutex<ForgeEventsStatus>>,
    consecutive_errors: AtomicU32,
    events_journaled: AtomicU64,
    cursor: AtomicU64,
}

impl FeedClient {
    /// Current sleep cadence: backoff after [`BACKOFF_AFTER_ERRORS`]
    /// consecutive failures, normal otherwise.
    fn effective_interval(&self, normal_secs: u64) -> Duration {
        if self.consecutive_errors.load(Ordering::Relaxed) >= BACKOFF_AFTER_ERRORS {
            Duration::from_secs(BACKOFF_INTERVAL.as_secs())
        } else {
            Duration::from_secs(normal_secs)
        }
    }

    /// Read a response body bounded to `limit` bytes; over-limit is an error,
    /// never a silent truncation (a truncated JSON page would desync the
    /// cursor, and the cursor must never advance on untrusted data).
    async fn bounded_body(response: reqwest::Response, limit: usize) -> Result<String, String> {
        let mut response = response;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > limit {
                return Err(format!(
                    "feed response exceeds {limit} bytes; refused (cursor untouched)"
                ));
            }
        }
        String::from_utf8(bytes).map_err(|_| "feed response is not UTF-8".to_string())
    }

    /// One feed poll. `Ok(())` on a successful fetch — **including an empty
    /// page** (a quiet forge is a success, not an error) — and
    /// `Err((state, reason))` for everything classified (auth / host /
    /// network / shape). The status cell is updated in both shapes; the
    /// cursor becomes durable only after a verified, host-matching page.
    async fn poll_once(&self) -> Result<(), (ForgeEventsState, String)> {
        let cursor = self.cursor.load(Ordering::Relaxed);
        let key = match load_key(&self.key_file) {
            Ok(k) => k,
            Err(e) => return Err(self.fail(ForgeEventsState::AuthFailed, e)),
        };
        let url = format!(
            "{}/v1/hosts/{}/events?after={}&limit={}",
            self.base_url.trim_end_matches('/'),
            self.host_id,
            cursor,
            self.page_size
        );
        let response = match self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {key}"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(self.fail(ForgeEventsState::Failing, format!("fetch failed: {e}")))
            }
        };
        let status_code = response.status();
        let status = status_code.as_u16();
        if status == HOST_HTTP {
            let body = Self::bounded_body(response, 4096).await.unwrap_or_default();
            return Err(self.fail(
                ForgeEventsState::HostMismatch,
                format!("worker does not know host `{}`: {}", self.host_id, truncate(&body, 200)),
            ));
        }
        if AUTH_HTTP.contains(&status) {
            return Err(self.fail(
                ForgeEventsState::AuthFailed,
                format!("feed rejected the key (HTTP {status}); rotate via mint-host-key.sh"),
            ));
        }
        if !status_code.is_success() {
            return Err(
                self.fail(ForgeEventsState::Failing, format!("feed answered HTTP {status}"))
            );
        }
        let body = match Self::bounded_body(response, MAX_RESPONSE_BYTES).await {
            Ok(b) => b,
            Err(e) => return Err(self.fail(ForgeEventsState::Failing, e)),
        };
        let parsed: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return Err(
                    self.fail(ForgeEventsState::Failing, format!("feed body is not JSON: {e}"))
                )
            }
        };
        let echoed = parsed
            .get("host_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if echoed != self.host_id {
            return Err(self.fail(
                ForgeEventsState::HostMismatch,
                format!(
                    "feed echoed host_id `{echoed}` but we are `{}`; cursor is not trusted",
                    self.host_id
                ),
            ));
        }
        if parsed.get("clamped").and_then(serde_json::Value::as_bool) == Some(true) {
            // Our cursor fell out of the worker's retention window and the
            // feed replayed from its oldest retained event. Informational,
            // not an error class (ADR-0021 degradation rung 5): the page is
            // replay-safe, so we consume it normally.
            log::info!("forge_events: feed clamped cursor {cursor} to its oldest retained event");
        }
        let events = parsed
            .get("events")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let next_cursor = parsed
            .get("cursor")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(cursor);
        if next_cursor < cursor {
            return Err(self.fail(
                ForgeEventsState::Failing,
                format!("feed cursor went backwards ({next_cursor} < {cursor}); refusing"),
            ));
        }
        // Durable, in this order: journal the page, then persist the
        // cursor (a crash before the persist loses nothing — the next poll
        // re-renders from the old cursor), then the bus publish (a lost
        // publish is a lost *prompt*, corrected by the next timer tick —
        // ADR-0014 invariant 3). Journal and state file are both
        // *diagnostic*: neither blocks forward progress — a failing one is
        // logged and the page still advances (a restart re-renders from the
        // last persisted cursor, still within the feed's retention window).
        for event in &events {
            let line = serde_json::json!({
                "received_at": rfc3339_now(),
                "event": event,
            });
            if let Err(e) = self.journal_append(&line) {
                log::warn!("forge_events: journal append failed: {e} — remaining lines of this page are skipped; the feed still advances (the journal is a diagnostic aid, not state of record)");
                break;
            }
            self.events_journaled.fetch_add(1, Ordering::Relaxed);
        }
        if let Err(e) = persist_cursor(&self.state_dir, next_cursor) {
            log::warn!("forge_events: cursor persist failed: {e} — in-memory cursor advances anyway; a restart re-renders from the last persisted cursor");
        }
        self.cursor.store(next_cursor, Ordering::Relaxed);
        self.report_success(&events);
        Ok(())
    }

    /// Append one journal line, rotating `journal.jsonl` → `journal.1`
    /// (overwrite) when over `journal_max_bytes`. `journal.1` is the
    /// most-recent full file — a best-effort audit tail, never required for
    /// forward progress.
    fn journal_append(&self, line: &serde_json::Value) -> std::io::Result<()> {
        let size = std::fs::metadata(self.state_dir.join(JOURNAL_FILE))
            .map(|m| m.len())
            .unwrap_or(0);
        if size > self.journal_max_bytes {
            let _ = std::fs::rename(
                self.state_dir.join(JOURNAL_FILE),
                self.state_dir.join(JOURNAL_ROTATED),
            );
        }
        let line = format!("{}\n", line);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state_dir.join(JOURNAL_FILE))?;
        use std::io::Write as _;
        f.write_all(line.as_bytes())
    }

    /// Healthy path: clear the error streak, record the success in the status
    /// cell, and publish one `forge.event` prompt per non-empty page (not per
    /// event — the prompt-topics dedup discipline, ADR-0021 §D3). Phase 1
    /// ships no subscriber; Phase 2's early-tick consumers attach to this
    /// topic. Synchronous: it only touches the status cell and the in-process
    /// bus — no I/O — so it takes no `.await`.
    fn report_success(&self, events: &[serde_json::Value]) {
        self.consecutive_errors.store(0, Ordering::Relaxed);
        let cursor = self.cursor.load(Ordering::Relaxed);
        let journaled = self.events_journaled.load(Ordering::Relaxed);
        let (first_seq, last_seq) = first_last_seq(events);
        let types = event_types(events);
        {
            let mut st = self.status.lock().expect("forge_events status poisoned");
            *st = ForgeEventsStatus::new(
                ForgeEventsState::Healthy,
                Some(self.host_id.clone()),
                Some(self.base_url.clone()),
                st.poll_interval_secs,
            );
            st.cursor = cursor;
            st.last_success_at = Some(rfc3339_now());
            st.events_journaled = journaled;
        }
        if events.is_empty() {
            return;
        }
        let payload = serde_json::json!({
            "source": "forge-event-feed",
            "host_id": self.host_id,
            "count": events.len(),
            "first_seq": first_seq,
            "last_seq": last_seq,
            "types": types,
        });
        let _ = self.bus.publish_generic("forge.event", payload);
    }

    fn fail(&self, state: ForgeEventsState, reason: String) -> (ForgeEventsState, String) {
        let n = self.consecutive_errors.fetch_add(1, Ordering::Relaxed) + 1;
        // The *reported* state is the error class, promoted to `backoff`
        // once the streak reaches the stretch threshold — the class stays
        // readable in `last_error` either way.
        let reported = if n >= BACKOFF_AFTER_ERRORS {
            ForgeEventsState::Backoff
        } else {
            state
        };
        let cursor = self.cursor.load(Ordering::Relaxed);
        let journaled = self.events_journaled.load(Ordering::Relaxed);
        let (prev_success, poll_secs) = {
            let st = self.status.lock().expect("forge_events status poisoned");
            (st.last_success_at.clone(), st.poll_interval_secs)
        };
        *self.status.lock().expect("forge_events status poisoned") = ForgeEventsStatus {
            state: reported,
            host_id: Some(self.host_id.clone()),
            endpoint: Some(self.base_url.clone()),
            cursor,
            last_success_at: prev_success,
            last_error: Some(truncate(&reason, 300)),
            consecutive_errors: n,
            events_journaled: journaled,
            poll_interval_secs: poll_secs,
        };
        (reported, reason)
    }
}

/// (first seq, last seq) of a page, `None` when empty.
fn first_last_seq(events: &[serde_json::Value]) -> (Option<u64>, Option<u64>) {
    match (events.first(), events.last()) {
        (Some(f), Some(l)) => (
            f.get("seq").and_then(serde_json::Value::as_u64),
            l.get("seq").and_then(serde_json::Value::as_u64),
        ),
        _ => (None, None),
    }
}

/// Sorted, deduplicated event-type list of a page (the prompt's routing hint).
fn event_types(events: &[serde_json::Value]) -> Vec<String> {
    let mut types: Vec<String> = events
        .iter()
        .filter_map(|e| e.get("type").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    types.sort();
    types.dedup();
    types
}

// ----------------------------------------------------------------------
// the loop and spawn

/// The production loop: poll, then sleep the effective (backoff-aware)
/// interval. Runs until process exit, like every daemon task — a daemon
/// restart is how this task dies, never an internal one.
pub(crate) async fn run_loop(feed: Arc<FeedClient>) {
    loop {
        let _ = feed.poll_once().await;
        let secs = feed
            .status
            .lock()
            .expect("forge_events status poisoned")
            .poll_interval_secs;
        tokio::time::sleep(feed.effective_interval(secs)).await;
    }
}

/// Wire the feed client for this daemon. `None` when the plane is off or
/// misconfigured (the status surface still answers — [`global_status`]
/// carries the reason); a [`JoinHandle`] when the loop is running.
///
/// Called once from daemon start, after the event bus exists — the same
/// wiring contract as [`crate::observability::spawn_task`].
pub fn spawn_task(config: &ForgeEventsConfig, event_bus: &EventBus) -> Option<JoinHandle<()>> {
    let poll_interval = resolve_poll_interval(config);
    match prepare(config) {
        SpawnPlan::Off => {
            register_global_status(Arc::new(Mutex::new(ForgeEventsStatus::new(
                ForgeEventsState::Disabled,
                resolve_host_id(config),
                resolve_endpoint(config),
                poll_interval,
            ))));
            None
        }
        SpawnPlan::Misconfigured {
            detail,
            host_id,
            endpoint,
        } => {
            log::error!("forge_events: {detail}");
            register_global_status(Arc::new(Mutex::new(ForgeEventsStatus {
                state: ForgeEventsState::Misconfigured,
                host_id,
                endpoint,
                cursor: 0,
                last_success_at: None,
                last_error: Some(detail),
                consecutive_errors: 0,
                events_journaled: 0,
                poll_interval_secs: poll_interval,
            })));
            None
        }
        SpawnPlan::Live {
            host_id,
            endpoint,
            key_file,
            state_dir,
            page_size,
            start_cursor,
        } => {
            let _ = std::fs::create_dir_all(&state_dir);
            let client = reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default();
            let status = Arc::new(Mutex::new(ForgeEventsStatus {
                state: ForgeEventsState::Connecting,
                host_id: Some(host_id.clone()),
                endpoint: Some(endpoint.clone()),
                cursor: start_cursor,
                last_success_at: None,
                last_error: None,
                consecutive_errors: 0,
                events_journaled: 0,
                poll_interval_secs: poll_interval,
            }));
            register_global_status(status.clone());
            let feed = Arc::new(FeedClient {
                http: client,
                base_url: endpoint,
                host_id,
                key_file,
                state_dir,
                page_size,
                journal_max_bytes: JOURNAL_MAX_BYTES,
                bus: event_bus.clone(),
                status: status.clone(),
                consecutive_errors: AtomicU32::new(0),
                events_journaled: AtomicU64::new(0),
                cursor: AtomicU64::new(start_cursor),
            });
            Some(tokio::spawn(async move { run_loop(feed).await }))
        }
    }
}

/// The spawn-time decision, split out of [`spawn_task`] so it is testable
/// without globals (process-global status is a first-registration-wins
/// `OnceLock`) or side effects (no directory created, no key read, no
/// client built here). `prepare` reads env + config only.
#[derive(Debug)]
pub(crate) enum SpawnPlan {
    /// Off by config — the deliberately silent case.
    Off,
    /// On, but not fetchable: missing endpoint/hostId, or a reserved
    /// placeholder endpoint (never a destination the key may reach).
    Misconfigured {
        detail: String,
        host_id: Option<String>,
        endpoint: Option<String>,
    },
    /// Fully resolved; ready to build the client and spawn the loop.
    Live {
        host_id: String,
        endpoint: String,
        key_file: PathBuf,
        state_dir: PathBuf,
        page_size: usize,
        /// Durable cursor from disk (`0` on first run / torn file).
        start_cursor: u64,
    },
}

/// The decision logic behind [`spawn_task`] (see its docs for the wiring
/// role). Pure with respect to process state: no env writes, no fs beyond
/// the cursor read and the spawn-time key check, no network.
pub(crate) fn prepare(config: &ForgeEventsConfig) -> SpawnPlan {
    if !resolve_enabled(config) {
        return SpawnPlan::Off;
    }
    let endpoint = match resolve_endpoint(config) {
        Some(e) => e,
        None => {
            return SpawnPlan::Misconfigured {
                detail: format!(
                    "forgeEvents is enabled but no endpoint is configured at any tier \n                     (config `forgeEvents.endpoint` or {ENDPOINT_ENV}); the plane stays off until one is"
                ),
                host_id: resolve_host_id(config),
                endpoint: None,
            };
        }
    };
    // Refuse reserved placeholder domains BEFORE the key or any directory is
    // touched (the #7815 guard, reusing the observability exporter's shared
    // policy): a placeholder is *not configured*, not a destination, and the
    // event key must never be sent there.
    if let Some(host) = crate::observability::endpoint_policy::reserved_placeholder_host(&endpoint)
    {
        return SpawnPlan::Misconfigured {
            detail: format!(
                "forgeEvents.endpoint {endpoint} points at the reserved placeholder domain \n                 {host} (RFC 2606/6761) — refusing to fetch so the event key is never sent there; \n                 set a real endpoint via {ENDPOINT_ENV} or .loom-local/local.json, or leave forgeEvents.enabled=false"
            ),
            host_id: resolve_host_id(config),
            endpoint: Some(endpoint),
        };
    }
    let host_id = match resolve_host_id(config) {
        Some(h) => h,
        None => {
            return SpawnPlan::Misconfigured {
                detail: "forgeEvents is enabled but no hostId is configured at any tier (config `forgeEvents.hostId` or {HOST_ID_ENV}); a host id is operator-provisioned and never synthesized, so the plane stays off until one is".into(),
                host_id: None,
                endpoint: Some(endpoint),
            };
        }
    };
    // `$HOME/.loom/forge-events`: the subsystem's whole local footprint
    // (key, cursor, journal) lives in one directory — the same convention
    // as observability's `~/.loom/observability/`.
    let home = std::env::var_os("HOME");
    let state_dir = home
        .map(|h| Path::new(&h).join(STATE_DIR_REL))
        .unwrap_or_else(|| PathBuf::from(STATE_DIR_REL));
    let key_file = resolve_key_file(config).unwrap_or_else(|| state_dir.join(DEFAULT_KEY_FILE_REL));
    // Key provisioning gate: a key that is unreadable at spawn is "never
    // provisioned", not "mid-rotation" — misconfigured, with the path named
    // in the detail. (A key that goes missing *later* is reported by the
    // per-poll re-read as `auth_failed` and self-heals; only the spawn-time
    // gap is latched here, and latching it is what makes a fresh host's
    // missing provisioning step visible on the status surface immediately.)
    if let Err(e) = load_key(&key_file) {
        return SpawnPlan::Misconfigured {
            detail: format!(
                "event key is not readable yet: {e} — install the operator-minted key at that path and restart the daemon"
            ),
            host_id: Some(host_id),
            endpoint: Some(endpoint),
        };
    }
    SpawnPlan::Live {
        start_cursor: load_cursor(&state_dir),
        host_id,
        endpoint,
        key_file,
        state_dir,
        page_size: resolve_page_size(config),
    }
}

// ----------------------------------------------------------------------
// tests

#[cfg(test)]
mod tests;
