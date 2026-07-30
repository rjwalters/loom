//! Optional safehouse fleet-comms narration sink (issue #3997, phase 1).
//!
//! Safehouse (`rjwalters/safehouse`) is an end-to-end-encrypted Matrix room a
//! human watches in Element to follow a multi-host agent fleet. A per-host
//! daemon (`safehoused`) owns the Matrix device and exposes a keyless
//! `AF_UNIX` RPC to local agents. This module lets `loom-daemon` **narrate**
//! sweep-lifecycle transitions into that room as an optional, additive
//! side-channel — forge labels remain the sole coordination source of truth.
//!
//! # Contract: byte-for-byte no-op when disabled
//!
//! This module mirrors the claude-monitor optional-integration pattern
//! (`tokens_pool::monitor`): there is **no hard dependency** on safehouse. When
//! `safehouse.enabled` is false or absent [`spawn_sink`] returns `None` without
//! subscribing to the bus or touching a socket — zero syscalls, behavior
//! identical to today. When enabled but the peer is absent, refuses the
//! connection, rejects the persona, or drops mid-run, every failure degrades to
//! a `warn!` and the sweep proceeds unaffected. **Loom never blocks a sweep on
//! safehouse.**
//!
//! # Design: subscribe to the existing bus, add no call sites
//!
//! The sink is an [`EventBus`] subscriber, not a scattering of new emit calls.
//! It maps the **existing** frozen event taxonomy to envelope-v1 messages and
//! adds no new topics (`event_bus.rs` "Topic taxonomy frozen for v0.10.0").
//!
//! # Wire protocol (envelope v1, verified against safehoused @ 2026-07-27)
//!
//! - `AF_UNIX`, **newline-delimited JSON**, one object per line, bidirectional.
//! - Mandatory first request `{"id":0,"op":"hello","persona":"<name>"}`; any op
//!   before `hello` is rejected. `persona` must be in safehoused's boot-time
//!   allowlist (a static TOML array — adding one needs a safehoused restart), so
//!   phase 1 uses a single static operator-provisioned persona (`loom_daemon`).
//! - `send` carries `to`/`type`/`body` and optional `task_id`/`room`. The
//!   daemon **stamps `from`** from the socket identity and ignores any `from`
//!   the client sends, so this client never sends one.
//! - Replies echo the request `id`. **Async push lines are interleaved on the
//!   same connection and carry an `event` key with no `id`** — the client
//!   demultiplexes by skipping any line with an `event` key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc::error::TryRecvError;

use crate::activity::ActivityDb;
use crate::event_bus::{EventBus, RecvError};
use crate::peer_claims::{ClaimAd, PeerClaimView};
use crate::types::{Event, SweepKind};

// ============================================================================
// Constants
// ============================================================================

/// Config-block env overrides (precedence **env > config > default**).
const ENABLED_ENV: &str = "LOOM_SAFEHOUSE_ENABLED";
const SOCKET_ENV: &str = "LOOM_SAFEHOUSE_SOCKET";
const ROOM_ENV: &str = "LOOM_SAFEHOUSE_ROOM";
const PERSONA_ENV: &str = "LOOM_SAFEHOUSE_PERSONA";

/// Attention-class room routing (#4225): the signal room id, and the per-repo
/// firehose map as a `repo=room[,repo=room…]` list. Either one present is enough
/// to switch the daemon out of single-room mode — see [`RoomMap`].
const ROOM_SIGNAL_ENV: &str = "LOOM_SAFEHOUSE_ROOM_SIGNAL";
const ROOMS_BY_REPO_ENV: &str = "LOOM_SAFEHOUSE_ROOMS_BY_REPO";

/// Alias prefix for a lazily-created per-repo firehose room (#4225): the
/// `vibesql` workspace's firehose is `fleet-vibesql`. Deliberately **not** the
/// signal room's own name (`loom-fleet`) — the prefix reads as "the fleet's view
/// of one repo", and the inverted word order keeps the two visually distinct in
/// an Element room list.
const REPO_ROOM_ALIAS_PREFIX: &str = "fleet-";

/// Convention for discovering the socket when neither env nor config sets one
/// (matches safehoused clients, which read `$SAFEHOUSED_SOCKET`).
const SAFEHOUSED_SOCKET_ENV: &str = "SAFEHOUSED_SOCKET";

/// Test/internal override for the `gh` binary the sink shells out to for the
/// dispatch-line title lookup (issue #4201). Mirrors the test-injection
/// pattern `SweepRegistryConfig::gh_bin` already uses in `sweep_registry.rs`
/// (a fake-`gh` script path), but as an env var since the sink has no
/// analogous per-registry config struct to carry a field on. Not part of the
/// public `safehouse` config block — this is a plumbing seam for tests, not an
/// operator-facing setting.
const GH_BIN_ENV: &str = "LOOM_SAFEHOUSE_GH_BIN";

/// Timeout for the sink-side `gh issue view --json title` lookup used to
/// enrich the dispatch line's body (issue #4201). Best-effort: on
/// timeout/failure the dispatch line is still narrated, just without a title,
/// rather than blocking narration (or, worse, the sweep the event describes).
const TITLE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a fetched issue title is cached before being looked up again — a
/// re-dispatch of the same issue (e.g. a Doctor-cycle re-run) reuses the
/// cached title instead of re-shelling to `gh`. Titles rarely change, so a
/// generous TTL is fine; this is the "short cache" tradeoff issue #4201 calls
/// for as the lighter alternative to threading the title through a
/// `SweepGlobalDispatch` payload amendment.
const TITLE_CACHE_TTL: Duration = Duration::from_secs(600);

/// The static operator-provisioned persona used when none is configured. Must
/// be present in safehoused's boot-time `personas` allowlist.
const DEFAULT_PERSONA: &str = "loom_daemon";

/// The single envelope version this client speaks (protocol §9).
pub const ENVELOPE_VERSION: u32 = 1;

/// The closed `type` enum (`envelope.rs:10`). A `send` outside this set is
/// rejected by safehoused, so we reject it before sending.
///
/// `completion` (#4426) is the machine-consumed, public-feed-eligible member:
/// safehoused's egress subsystem mirrors well-formed `completion` envelopes out
/// of allowlisted rooms to a `sink_url` (the 2amlogic.com fleet feed). It MUST
/// carry a strictly-valid `completion-v1` `meta` — safehoused **silently
/// degrades a malformed `meta` to `chat`**, which never reaches the feed and
/// produces no error here, so this client validates before sending
/// ([`validate_completion_meta`]) rather than relying on the server.
pub const KNOWN_TYPES: [&str; 5] = ["chat", "task", "handoff", "ack", "completion"];

/// The one `meta.schema` value this client emits (safehouse
/// `docs/protocol/envelope-v1.md` §4a).
pub const COMPLETION_SCHEMA: &str = "completion-v1";

/// Required `completion-v1` `meta` keys. Every one must be a non-empty string
/// for the envelope to be built or sent (#4426).
const COMPLETION_REQUIRED_KEYS: [&str; 7] = [
    "schema",
    "agent",
    "repo",
    "ref",
    "result",
    "started_at",
    "completed_at",
];

/// Timeout for the sink-side forge lookups that confirm a merge and resolve the
/// `owner/repo` slug for a `completion` envelope (#4426). Two short `gh` calls;
/// on timeout the completion is simply not narrated — the sweep is long over by
/// then and nothing downstream waits on it.
const MERGE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// `gh pr list --json` fields the completion emit point requests.
/// `number,url,mergedAt` are load-bearing (a missing one degrades the whole
/// lookup, and with it the completion); `title,additions,deletions` are the
/// #4497 feed display fields, harvested from the same call and degrading
/// per-field.
const MERGED_PR_FIELDS: &str = "number,url,mergedAt,title,additions,deletions";

/// The pre-#4497 field set, retried only when a `gh` too old to know one of the
/// display fields rejects [`MERGED_PR_FIELDS`] outright — so an older host keeps
/// publishing completions instead of losing them to a cosmetic field.
const MERGED_PR_FIELDS_BASE: &str = "number,url,mergedAt";

/// Timeout for the sink-side activity-DB token rollup behind a `completion`
/// envelope (#4497). The query is a single indexed SQLite aggregate on a local
/// file, but it contends with the IPC handler's writes for the DB mutex, so it
/// runs on the blocking pool under this cap; on timeout `tokens` is simply
/// omitted.
const TOKEN_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Reconnect backoff floor and ceiling. The floor keeps a burst of events from
/// hammering an absent peer (one `warn`, not a hot loop); the ceiling caps the
/// wait so a peer that comes back is picked up promptly.
const DEFAULT_MIN_BACKOFF: Duration = Duration::from_secs(2);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);

// ============================================================================
// Config
// ============================================================================

/// The attention-class room map (#4225): the operator's signal room plus the
/// per-repo firehose rooms. **`None` on [`SafehouseConfig::rooms`] is the
/// migration default** and means "single-room mode" — every message goes to
/// [`SafehouseConfig::room`] exactly as it did before #4225.
///
/// A present-but-*empty* map (no `signal`, no `byRepo` entries) is normalized
/// back to `None` by [`rooms_from_value`] / [`apply_room_env_overrides`]: an
/// operator who leaves `"rooms": {}` in config gets the unchanged single-room
/// behavior rather than a routing mode with nothing to route to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomMap {
    /// The everyone/signal room (`loom-fleet`): operator ↔ fleet conversation,
    /// every `handoff`, and terminal outcomes (`ack`/`completion`). Low volume,
    /// notifications on, cross-repo by design. `None` ⇒ fall back to the legacy
    /// [`SafehouseConfig::room`] (see [`SafehouseConfig::signal_room`]).
    pub signal: Option<String>,
    /// Per-repo firehose rooms keyed by the **workspace-root basename** — the
    /// same narration repo convention #4201 established for `task_id`/body
    /// prefixes (`/Users/x/GitHub/vibesql` ⇒ `vibesql`). A repo absent from the
    /// map is created lazily as `fleet-<repo>` on first narration (see
    /// [`RoomRouter`]). `BTreeMap` for deterministic iteration in tests/logs.
    pub by_repo: std::collections::BTreeMap<String, String>,
}

impl RoomMap {
    /// Whether this map carries no usable routing target at all, in which case
    /// callers normalize it to `None` (single-room mode).
    #[must_use]
    fn is_empty(&self) -> bool {
        self.signal.is_none() && self.by_repo.is_empty()
    }
}

/// Resolved `safehouse` config block. `enabled: false` is the default and a
/// byte-for-byte no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafehouseConfig {
    pub enabled: bool,
    /// Socket path; `None` ⇒ resolve `$SAFEHOUSED_SOCKET` at connect time.
    pub socket: Option<PathBuf>,
    /// Room name/id; `None` is valid only when safehoused joined exactly one
    /// room (it then resolves the sole room server-side).
    ///
    /// Once [`rooms`](Self::rooms) is present this is only a **fallback** for
    /// the signal room ([`signal_room`](Self::signal_room)) — and once the bot
    /// is in several rooms, `null` no longer resolves server-side at all, so
    /// explicit ids become required (documented migration note, #4225).
    pub room: Option<String>,
    /// Attention-class room routing (#4225). `None` ⇒ single-room mode (the
    /// pre-#4225 behavior, byte-identical).
    pub rooms: Option<RoomMap>,
    /// Persona to authenticate as; must be in safehoused's allowlist.
    pub persona: String,
}

impl Default for SafehouseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket: None,
            room: None,
            rooms: None,
            persona: DEFAULT_PERSONA.to_owned(),
        }
    }
}

impl SafehouseConfig {
    /// The room signal-class traffic goes to (#4225): `rooms.signal` when the
    /// map configures one, else the legacy scalar [`room`](Self::room). In
    /// single-room mode this **is** `room`, which is what keeps the absent-map
    /// path byte-identical.
    ///
    /// Doubles as the room the peer-claim coordination connection advertises
    /// into — see [`run_coordination`] for why claim ads deliberately stay on
    /// the signal room.
    #[must_use]
    pub fn signal_room(&self) -> Option<&str> {
        self.rooms
            .as_ref()
            .and_then(|rooms| rooms.signal.as_deref())
            .or(self.room.as_deref())
    }

    /// Whether attention-class routing is active (a `rooms` map resolved).
    #[must_use]
    pub fn routes_by_attention(&self) -> bool {
        self.rooms.is_some()
    }
}

/// Resolve the effective `safehouse` config for `repo_root` with precedence
/// **env > config > default(disabled)**. Never panics: a missing/malformed
/// config tree resolves to [`SafehouseConfig::default`] (disabled).
#[must_use]
pub fn resolve_config(repo_root: &Path) -> SafehouseConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let block = crate::config_resolver::get_path(&effective, "safehouse");
    apply_env_overrides(config_from_value(block))
}

/// Read the config layer only (no env), so unit tests can assert
/// config-over-default without mutating process env.
#[must_use]
fn config_from_value(block: Option<&Value>) -> SafehouseConfig {
    let mut cfg = SafehouseConfig::default();
    let Some(block) = block.and_then(Value::as_object) else {
        return cfg;
    };
    if let Some(enabled) = block.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(socket) = block.get("socket").and_then(Value::as_str) {
        if !socket.trim().is_empty() {
            cfg.socket = Some(PathBuf::from(socket));
        }
    }
    if let Some(room) = block.get("room").and_then(Value::as_str) {
        if !room.trim().is_empty() {
            cfg.room = Some(room.to_owned());
        }
    }
    if let Some(persona) = block.get("persona").and_then(Value::as_str) {
        if !persona.trim().is_empty() {
            cfg.persona = persona.to_owned();
        }
    }
    cfg.rooms = rooms_from_value(block.get("rooms"));
    cfg
}

/// Parse the `safehouse.rooms` sub-block (#4225). Every malformed shape — a
/// non-object `rooms`, a non-object `byRepo`, non-string/blank ids — degrades to
/// "that key was not configured" rather than erroring, and a map with nothing
/// usable in it normalizes to `None` (single-room mode, unchanged behavior).
#[must_use]
fn rooms_from_value(block: Option<&Value>) -> Option<RoomMap> {
    let rooms = block?.as_object()?;
    let mut map = RoomMap::default();
    if let Some(signal) = rooms.get("signal").and_then(Value::as_str) {
        if !signal.trim().is_empty() {
            map.signal = Some(signal.trim().to_owned());
        }
    }
    if let Some(by_repo) = rooms.get("byRepo").and_then(Value::as_object) {
        for (repo, room) in by_repo {
            let Some(room) = room.as_str() else { continue };
            let (repo, room) = (repo.trim(), room.trim());
            if !repo.is_empty() && !room.is_empty() {
                map.by_repo.insert(repo.to_owned(), room.to_owned());
            }
        }
    }
    (!map.is_empty()).then_some(map)
}

/// Apply the env layer on top of a config-resolved [`SafehouseConfig`]. Env
/// wins over config for every key.
#[must_use]
fn apply_env_overrides(mut cfg: SafehouseConfig) -> SafehouseConfig {
    if let Some(enabled) = env_bool(ENABLED_ENV) {
        cfg.enabled = enabled;
    }
    if let Some(socket) = env_nonempty(SOCKET_ENV) {
        cfg.socket = Some(PathBuf::from(socket));
    }
    if let Some(room) = env_nonempty(ROOM_ENV) {
        cfg.room = Some(room);
    }
    if let Some(persona) = env_nonempty(PERSONA_ENV) {
        cfg.persona = persona;
    }
    cfg.rooms = apply_room_env_overrides(cfg.rooms);
    cfg
}

/// Apply the env layer to the [`RoomMap`] (#4225), preserving **env > config >
/// default**:
///
/// - `LOOM_SAFEHOUSE_ROOM_SIGNAL` overrides `rooms.signal` alone.
/// - `LOOM_SAFEHOUSE_ROOMS_BY_REPO` (`repo=room,repo=room…`) replaces the
///   **whole** `byRepo` map rather than merging into it, so an operator can
///   override a stale committed map from the environment without editing config
///   (the same wholesale-replacement semantics `LOOM_SAFEHOUSE_WORKER_PERSONAS`
///   uses for its list).
/// - Either env var alone is enough to *enable* routing on a config that has no
///   `rooms` block at all.
/// - Neither set ⇒ the config-layer map is returned untouched (so the absent-map
///   single-room default stays byte-identical).
#[must_use]
fn apply_room_env_overrides(rooms: Option<RoomMap>) -> Option<RoomMap> {
    let signal = env_nonempty(ROOM_SIGNAL_ENV);
    let by_repo = env_nonempty(ROOMS_BY_REPO_ENV).map(|raw| parse_by_repo_env(&raw));
    if signal.is_none() && by_repo.is_none() {
        return rooms;
    }
    let mut map = rooms.unwrap_or_default();
    if let Some(signal) = signal {
        map.signal = Some(signal);
    }
    if let Some(by_repo) = by_repo {
        map.by_repo = by_repo;
    }
    (!map.is_empty()).then_some(map)
}

/// Parse `repo=room,repo=room…` into a [`RoomMap::by_repo`] map. Entries without
/// a `=`, or with a blank half, are skipped (a malformed env var degrades to the
/// entries it *can* parse — never a panic, never a hard failure).
#[must_use]
fn parse_by_repo_env(raw: &str) -> std::collections::BTreeMap<String, String> {
    raw.split(',')
        .filter_map(|pair| {
            let (repo, room) = pair.split_once('=')?;
            let (repo, room) = (repo.trim(), room.trim());
            (!repo.is_empty() && !room.is_empty()).then(|| (repo.to_owned(), room.to_owned()))
        })
        .collect()
}

/// Resolve the socket path at connect time: configured value, else
/// `$SAFEHOUSED_SOCKET`. `None` ⇒ no socket known (warn + skip).
#[must_use]
fn resolve_socket(cfg: &SafehouseConfig) -> Option<PathBuf> {
    cfg.socket
        .clone()
        .or_else(|| env_nonempty(SAFEHOUSED_SOCKET_ENV).map(PathBuf::from))
}

fn env_bool(key: &str) -> Option<bool> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

// ============================================================================
// Connection state (issue #4345 — new-host onboarding visibility)
// ============================================================================
//
// Before #4345, `safehouse.enabled` false/absent, enabled-but-unreachable, and
// enabled-and-connected all looked identical to an operator: silence. The
// narration sink and peer-claim coordination task both already know their own
// live connection state; this cell is how that knowledge reaches
// `loom-daemon status` without a second, status-time connection attempt (a
// CLI-side probe can't know "room joined" the way the daemon's own live
// connection can).

/// Live safehouse connection state, shared between the narration sink
/// ([`run_sink`]) and the peer-claim coordination task ([`run_coordination`])
/// via a [`SharedSafehouseState`] cell — the same "shared `Arc<Mutex<..>>`
/// updated by the task that owns the connection" shape [`PeerClaimView`]
/// already uses. Both tasks connect to the same `safehoused` peer off the same
/// resolved config, so in steady state they agree; a transient disagreement
/// (one connection drops, the other has not yet) resolves to whichever task
/// transitions last, which self-heals on the next reconnect attempt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SafehouseState {
    /// `safehouse.enabled` is false/absent (the byte-for-byte no-op path), or
    /// enabled with no socket resolving at all. No connection has ever been
    /// attempted for this transition.
    #[default]
    NotConfigured,
    /// Enabled with a socket resolved, but the most recent connect attempt
    /// failed, refused, or dropped. `socket` carries the path that was tried.
    Unreachable { socket: PathBuf },
    /// The most recent connect attempt completed the `hello` handshake
    /// successfully. `room` is the configured room name — `None` when
    /// [`SafehouseConfig::room`] is unset, which is only valid when safehoused
    /// joined exactly one room (resolved server-side; this client is never
    /// told the resolved name in that case).
    Connected {
        socket: PathBuf,
        room: Option<String>,
    },
    /// The `hello` handshake succeeded, but the most recent `send` was rejected
    /// at the protocol layer (`ok:false`) — the socket is reachable and the
    /// connection is healthy, only the send was refused (#4464). The canonical
    /// case is a multi-room safehoused with [`SafehouseConfig::room`] unset,
    /// whose `send` returns `'room' required: N rooms joined`. Distinct from
    /// [`Unreachable`](Self::Unreachable) so `loom-daemon status` points the
    /// operator at `safehouse.room` rather than at the socket/persona. `reason`
    /// carries the raw safehoused `error` string. **Sticky**: a reconnect whose
    /// `hello` succeeds does not clear it; only a `send` that is accepted
    /// returns the state to [`Connected`](Self::Connected).
    SendRejected { socket: PathBuf, reason: String },
}

impl SafehouseState {
    /// Render into the wire [`crate::types::SafehouseStatus`] shape consumed by
    /// `DaemonStatusReport` (#4345).
    #[must_use]
    pub fn to_status(&self) -> crate::types::SafehouseStatus {
        match self {
            Self::NotConfigured => crate::types::SafehouseStatus {
                state: "not_configured".to_owned(),
                socket: None,
                room: None,
                reason: None,
            },
            Self::Unreachable { socket } => crate::types::SafehouseStatus {
                state: "unreachable".to_owned(),
                socket: Some(socket.clone()),
                room: None,
                reason: None,
            },
            Self::Connected { socket, room } => crate::types::SafehouseStatus {
                state: "connected".to_owned(),
                socket: Some(socket.clone()),
                room: room.clone(),
                reason: None,
            },
            Self::SendRejected { socket, reason } => crate::types::SafehouseStatus {
                state: "send_rejected".to_owned(),
                socket: Some(socket.clone()),
                room: None,
                reason: Some(reason.clone()),
            },
        }
    }
}

/// A shared, `Arc`-wrapped connection-state cell, injected into
/// [`spawn_sink`]/[`spawn_peer_coordination`] (and their `run_*` loops) so
/// [`WorkspacePool`](crate::workspace_pool::WorkspacePool) can hold one cell
/// per daemon and read it back for `loom-daemon status` without a second
/// connection. Mirrors [`PeerClaimView`]'s `Arc<Mutex<..>>` injection shape.
pub type SharedSafehouseState = Arc<Mutex<SafehouseState>>;

/// Construct a fresh cell defaulted to [`SafehouseState::NotConfigured`] — the
/// correct starting value for a daemon that has not yet called
/// [`spawn_sink`]/[`spawn_peer_coordination`] for this cell.
#[must_use]
pub fn new_shared_state() -> SharedSafehouseState {
    Arc::new(Mutex::new(SafehouseState::default()))
}

/// Overwrite `cell` with `state`. Recovers a poisoned mutex (a panic on some
/// other thread while holding the lock must never permanently blind `status`
/// to connection state) rather than propagating the poison.
fn set_state(cell: &SharedSafehouseState, state: SafehouseState) {
    match cell.lock() {
        Ok(mut guard) => *guard = state,
        Err(poisoned) => *poisoned.into_inner() = state,
    }
}

/// Snapshot the current connection state out of `cell`.
#[must_use]
pub fn snapshot_state(cell: &SharedSafehouseState) -> SafehouseState {
    match cell.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Set `cell` to [`SafehouseState::NotConfigured`] (#4345). Exposed (unlike
/// [`set_state`]) for `workspace_pool.rs`'s disabled-config fast path in
/// `start_peer_coordination`, which returns before ever calling
/// [`spawn_peer_coordination`] — the one caller outside this module that needs
/// to report a transition directly rather than through a `spawn_*`/`run_*`
/// entry point.
pub fn set_not_configured(cell: &SharedSafehouseState) {
    set_state(cell, SafehouseState::NotConfigured);
}

// ============================================================================
// Envelope
// ============================================================================

/// A logical outbound narration message. Deliberately omits `from` — safehoused
/// stamps it from the socket identity (§6), and this client never sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Recipient persona, `"*"` (everyone), or a `@matrix:id`.
    pub to: String,
    /// One of [`KNOWN_TYPES`].
    pub kind: String,
    /// Task thread key; must be `[A-Za-z0-9_]` (a bare issue number is fine).
    pub task_id: Option<String>,
    pub body: String,
    /// Structured machine payload (#4426). Present **iff** `kind` is
    /// `completion`, where it carries a `completion-v1` object —
    /// [`build_send_request`] enforces both directions and re-validates the
    /// contents. `body` stays required human prose regardless: a human reading
    /// the room sees a sentence, `meta` is the machine view the public fleet
    /// feed is derived from.
    pub meta: Option<Value>,
}

/// The `completion-v1` `result` values. An enum rather than a string so a
/// caller cannot construct an unknown result that safehoused would reject (or,
/// worse, degrade to `chat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionResult {
    Success,
    Failure,
}

impl CompletionResult {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

/// The typed source for a `completion-v1` `meta` object (#4426). Building the
/// JSON goes through [`CompletionMeta::to_meta_value`], which **validates and
/// can fail** — there is deliberately no way to get a `completion` envelope
/// onto the wire without passing that gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionMeta {
    /// Persona that did the work; becomes `meta.agent` and must mirror the
    /// `from` safehoused stamps from the socket identity.
    pub agent: String,
    /// Forge `owner/repo` slug (e.g. `rjwalters/loom`) — **not** the
    /// path-basename narration convention (#4201): the feed links `ref` and
    /// displays the forge identity.
    pub repo_slug: String,
    /// Canonical web URL of the merged PR; becomes `meta.ref`.
    pub pr_url: String,
    pub result: CompletionResult,
    /// RFC3339 timestamps; `completed_at` must not precede `started_at`.
    pub started_at: String,
    pub completed_at: String,
    /// Optional extension fields. envelope-v1 preserves unknown `meta` keys and
    /// safehoused's egress publishes the raw redacted `meta`, so these need no
    /// schema revision downstream. Omitted entirely when `None` — the feed
    /// handles absence, and the issue's rule is "omit rather than guess".
    pub issue: Option<u32>,
    /// Best-effort total (input + output) tokens the activity DB attributes to
    /// this issue (#4497), for the feed's cost-of-quality-code trend. **Known
    /// imperfect** — see [`fetch_issue_tokens`] for the attribution caveats
    /// (issue-number-only, so no repo qualification, and dependent on the
    /// activity DB having a per-issue rollup at all). Imperfect-but-consistent
    /// beats absent for trend purposes; a zero/absent rollup still omits the
    /// key rather than publishing a bogus `0`.
    pub tokens: Option<u64>,
    /// Merged PR title (#4497) — the feed renders rows as
    /// `<repo>#<issue>: <title> +A −D`. Trimmed; an empty title is treated as
    /// absent.
    pub title: Option<String>,
    /// Merged PR diff size (#4497), from the same `gh pr list` call that
    /// verifies the merge. Unlike `tokens`, a real `0` is **meaningful** for a
    /// merge (a docs-only revert legitimately adds nothing), so zeros are
    /// published rather than filtered.
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
}

impl CompletionMeta {
    /// Render (and validate) the `completion-v1` `meta` object. Fails rather
    /// than emitting a degradable envelope.
    pub fn to_meta_value(&self) -> Result<Value> {
        let mut meta = json!({
            "schema": COMPLETION_SCHEMA,
            "agent": self.agent,
            "repo": self.repo_slug,
            "ref": self.pr_url,
            "result": self.result.as_str(),
            "started_at": self.started_at,
            "completed_at": self.completed_at,
        });
        let obj = meta.as_object_mut().expect("json object literal");
        if let Some(issue) = self.issue {
            obj.insert("issue".into(), json!(issue));
        }
        // A zero token count is indistinguishable from "accounting had
        // nothing", so it is omitted rather than published as a real zero.
        if let Some(tokens) = self.tokens.filter(|t| *t > 0) {
            obj.insert("tokens".into(), json!(tokens));
        }
        // Display fields (#4497). `title` is trimmed and an empty one is
        // dropped (an empty string would render as a blank row label); the
        // counts publish real zeros because `0 additions` is a fact about the
        // merge, not the "no data" sentinel a `0` token count would be.
        if let Some(title) = self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            obj.insert("title".into(), json!(title));
        }
        if let Some(additions) = self.additions {
            obj.insert("additions".into(), json!(additions));
        }
        if let Some(deletions) = self.deletions {
            obj.insert("deletions".into(), json!(deletions));
        }
        validate_completion_meta(&meta)?;
        Ok(meta)
    }
}

/// Whether `slug` is a forge `owner/repo` slug: exactly one `/`, both halves
/// non-empty and made of forge-legal name characters.
fn valid_repo_slug(slug: &str) -> bool {
    let mut parts = slug.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let legal = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    legal(owner) && legal(name)
}

/// Validate a `completion-v1` `meta` object **before** it can reach the wire
/// (#4426). safehoused silently degrades a malformed `meta` to a `chat` — the
/// event then vanishes from the public feed with no error anywhere — so this
/// client refuses to send one instead of relying on server-side validation.
///
/// Checks: every [`COMPLETION_REQUIRED_KEYS`] entry present and a non-empty
/// string, `schema == "completion-v1"`, `agent` a valid persona, `repo` an
/// `owner/repo` slug, `ref` an absolute `http(s)` URL, `result` one of
/// `success`/`failure`, and both timestamps RFC3339 with
/// `completed_at >= started_at`.
pub fn validate_completion_meta(meta: &Value) -> Result<()> {
    let Some(obj) = meta.as_object() else {
        bail!("completion `meta` must be a JSON object, got {meta}");
    };
    for key in COMPLETION_REQUIRED_KEYS {
        match obj.get(key).and_then(Value::as_str) {
            Some(v) if !v.trim().is_empty() => {}
            _ => bail!("completion `meta` is missing required non-empty string field {key:?}"),
        }
    }
    let get = |key: &str| obj.get(key).and_then(Value::as_str).unwrap_or_default();

    if get("schema") != COMPLETION_SCHEMA {
        bail!(
            "completion `meta.schema` must be {COMPLETION_SCHEMA:?}, got {:?}",
            get("schema")
        );
    }
    if !valid_persona(get("agent")) {
        bail!("completion `meta.agent` {:?} is not a valid persona", get("agent"));
    }
    if !valid_repo_slug(get("repo")) {
        bail!("completion `meta.repo` {:?} is not a forge owner/repo slug", get("repo"));
    }
    let pr_ref = get("ref");
    if !(pr_ref.starts_with("https://") || pr_ref.starts_with("http://")) {
        bail!("completion `meta.ref` {pr_ref:?} must be an absolute http(s) URL");
    }
    let result = get("result");
    if result != CompletionResult::Success.as_str() && result != CompletionResult::Failure.as_str()
    {
        bail!("completion `meta.result` must be \"success\" or \"failure\", got {result:?}");
    }
    let started = DateTime::parse_from_rfc3339(get("started_at")).with_context(|| {
        format!("completion `meta.started_at` {:?} is not RFC3339", get("started_at"))
    })?;
    let completed = DateTime::parse_from_rfc3339(get("completed_at")).with_context(|| {
        format!("completion `meta.completed_at` {:?} is not RFC3339", get("completed_at"))
    })?;
    if completed < started {
        bail!(
            "completion `meta.completed_at` ({}) precedes `started_at` ({})",
            get("completed_at"),
            get("started_at")
        );
    }
    // `issue`/`tokens`/`additions`/`deletions` are optional extension fields;
    // when present they must still be non-negative integers rather than
    // strings/floats.
    for key in ["issue", "tokens", "additions", "deletions"] {
        if let Some(v) = obj.get(key) {
            if v.as_u64().is_none() {
                bail!("completion `meta.{key}` must be a non-negative integer, got {v}");
            }
        }
    }
    // `title` (#4497) is an optional string; a present-but-blank one would
    // render as an empty row label on the feed, so it is rejected here rather
    // than published (the builder omits it instead — see `to_meta_value`).
    if let Some(v) = obj.get("title") {
        match v.as_str() {
            Some(s) if !s.trim().is_empty() => {}
            _ => {
                bail!("completion `meta.title`, when present, must be a non-empty string, got {v}")
            }
        }
    }
    Ok(())
}

/// `[a-z0-9_]`, 1..=64 chars (persona charset, `envelope.rs` `valid_persona`).
fn valid_persona(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Normalize a `to` value: `"*"` and `@matrix:id` pass through; a persona is
/// lowercased and hyphens folded to underscores (hyphens are a render-time
/// cosmetic — the wire form is underscored, and safehoused does **not**
/// normalize `to`, so a hyphenated `to` would route nowhere). Rejects anything
/// that is still not a valid persona after normalization.
fn normalize_to(to: &str) -> Result<String> {
    if to == "*" || to.starts_with('@') {
        return Ok(to.to_owned());
    }
    let normalized = to.to_ascii_lowercase().replace('-', "_");
    if valid_persona(&normalized) {
        Ok(normalized)
    } else {
        bail!("invalid `to` {to:?}: not \"*\", a @matrix-id, or a [a-z0-9_] persona")
    }
}

/// Build the `send` RPC request for `env`, validating **before** sending:
/// `type` must be a known type, `task_id` must be `[A-Za-z0-9_]`, and `to` is
/// normalized. Emits `v: 1`, never a `from`, and omits `task_id`/`room` when
/// absent. safehoused ignores the extra `v` and re-stamps it — carrying it here
/// makes the request self-describe as envelope-v1.
///
/// `meta` (#4426) is serialized only for `completion`, and only after
/// [`validate_completion_meta`] accepts it: an incomplete or malformed
/// `completion` is **refused here** (never sent), because safehoused would
/// otherwise degrade it to a `chat` and it would silently vanish from the
/// public feed. A `meta` on any other type is likewise an error rather than a
/// silently-dropped field.
pub fn build_send_request(env: &Envelope, id: u64, room: Option<&str>) -> Result<Value> {
    if !KNOWN_TYPES.contains(&env.kind.as_str()) {
        bail!("invalid envelope type {:?} (v1 types: {:?})", env.kind, KNOWN_TYPES);
    }
    match (env.kind.as_str(), env.meta.as_ref()) {
        ("completion", Some(meta)) => validate_completion_meta(meta)
            .context("refusing to send a completion envelope with invalid completion-v1 meta")?,
        ("completion", None) => {
            bail!("envelope type \"completion\" requires a completion-v1 `meta` object")
        }
        (kind, Some(_)) => bail!("`meta` is only valid on a \"completion\" envelope, not {kind:?}"),
        (_, None) => {}
    }
    if let Some(task_id) = &env.task_id {
        if task_id.is_empty()
            || !task_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!("invalid task_id {task_id:?}: must be [A-Za-z0-9_]");
        }
    }
    let to = normalize_to(&env.to)?;

    let mut req = json!({
        "id": id,
        "op": "send",
        "v": ENVELOPE_VERSION,
        "to": to,
        "type": env.kind,
        "body": env.body,
    });
    let obj = req.as_object_mut().expect("json object literal");
    if let Some(task_id) = &env.task_id {
        obj.insert("task_id".into(), json!(task_id));
    }
    if let Some(room) = room {
        obj.insert("room".into(), json!(room));
    }
    if let Some(meta) = &env.meta {
        obj.insert("meta".into(), meta.clone());
    }
    Ok(req)
}

// ============================================================================
// Attention-class room routing (#4225)
// ============================================================================
//
// One room carrying everything (operator conversation + human-must-act handoffs
// + the full narration firehose) drowns the signal it exists to deliver: at full
// concurrency the operator's primary interface takes hundreds of messages a
// night. #4225 routes by **attention class first, repo second**:
//
// | Tier | Room | Carries | Notifications |
// |---|---|---|---|
// | 1 | `loom-fleet` (signal) | operator ↔ fleet, every `handoff`, terminal `ack`/`completion` | on |
// | 2 | `fleet-<repo>` (firehose) | `task` (dispatch/phase) + `chat` (worker chatter) | muted, opened when watching |
//
// **Severity routes, never duplicates** — every message resolves to exactly one
// room. The Matrix Space grouping the rooms is out of scope here (tracked in the
// safehouse repo).

/// The closed envelope `type` enum as a Rust enum, so the kind → room routing
/// table ([`EnvelopeKind::attention_class`]) is a **compile-time-exhaustive**
/// `match` with no wildcard arm: a sixth envelope type cannot be introduced
/// without the compiler pointing straight at the routing decision, instead of
/// silently defaulting into the wrong room.
///
/// [`KNOWN_TYPES`] remains the wire-level source of truth ([`build_send_request`]
/// validates against it); the `known_types_and_envelope_kind_stay_in_lockstep`
/// test pins the two representations together so adding a member to one without
/// the other fails a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeKind {
    Chat,
    Task,
    Handoff,
    Ack,
    Completion,
}

impl EnvelopeKind {
    /// Every member, in [`KNOWN_TYPES`] order.
    pub const ALL: [Self; 5] = [
        Self::Chat,
        Self::Task,
        Self::Handoff,
        Self::Ack,
        Self::Completion,
    ];

    /// The wire string for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Task => "task",
            Self::Handoff => "handoff",
            Self::Ack => "ack",
            Self::Completion => "completion",
        }
    }

    /// Parse a wire `type` string; `None` for anything outside [`KNOWN_TYPES`]
    /// (which [`build_send_request`] refuses to send anyway).
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == kind)
    }

    /// **The routing table** (#4225). Exhaustive by construction — no wildcard
    /// arm — so a future sixth type (e.g. the wave-digest kind #4217 folds into
    /// this layer, which would slot in as another [`AttentionClass::Signal`] arm)
    /// fails to compile here rather than defaulting into the wrong room.
    #[must_use]
    pub const fn attention_class(self) -> AttentionClass {
        match self {
            // Terminal / human-attention outcomes → the signal room.
            Self::Handoff | Self::Ack | Self::Completion => AttentionClass::Signal,
            // Dispatch, phase transitions, worker chatter → the repo firehose.
            Self::Task | Self::Chat => AttentionClass::Firehose,
        }
    }
}

/// Which attention tier a message belongs to (#4225).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionClass {
    /// The everyone/signal room — low volume, notifications on, cross-repo.
    Signal,
    /// The per-repo firehose room — muted by default, opened when watching a repo.
    Firehose,
}

/// The room `alias` a repo's firehose is created under: `fleet-<repo>` from the
/// workspace-root basename (#4201's narration repo convention).
#[must_use]
fn repo_room_alias(repo: &str) -> String {
    format!("{REPO_ROOM_ALIAS_PREFIX}{repo}")
}

/// Where one envelope should be sent, as decided by [`RoomRouter::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomDecision {
    /// Send with this `room` value verbatim. `None` means "send no `room` key
    /// and let safehoused resolve its sole joined room" — the pre-#4225
    /// single-room convenience, which stops resolving once the bot joins several
    /// rooms (see the migration note in `.loom/docs/safehouse.md`).
    Send(Option<String>),
    /// This repo's firehose room is not configured yet: create `alias` first,
    /// then send there. On any creation failure, send to `fallback` (the signal
    /// room) instead — narration is never lost and never blocks.
    Create {
        /// Workspace-root basename, the [`RoomMap::by_repo`] key.
        repo: String,
        /// `fleet-<repo>`, the alias to create.
        alias: String,
        /// Degradation target when creation fails.
        fallback: Option<String>,
    },
}

/// Resolves each envelope's room by attention class (#4225), remembering rooms it
/// lazily created and repos whose creation failed.
///
/// Owned by the narration sink ([`run_sink`]) for the daemon's lifetime — the
/// `created`/`degraded` memory is per-daemon-run, so an operator who fixes room
/// permissions restarts the daemon (the same restart discipline the rest of this
/// module's config already has).
pub struct RoomRouter {
    /// `None` ⇒ single-room mode: every envelope resolves to `single`.
    map: Option<RoomMap>,
    /// The legacy scalar `safehouse.room`.
    single: Option<String>,
    /// Repos whose firehose room this run created (repo basename → room id).
    created: HashMap<String, String>,
    /// Repos whose firehose room could not be created — routed to the signal
    /// room from then on, with **one** warning ever (never one per message).
    degraded: std::collections::HashSet<String>,
}

impl RoomRouter {
    #[must_use]
    pub fn new(config: &SafehouseConfig) -> Self {
        Self {
            map: config.rooms.clone(),
            single: config.room.clone(),
            created: HashMap::new(),
            degraded: std::collections::HashSet::new(),
        }
    }

    /// The signal room: `rooms.signal`, else the legacy scalar `room`.
    #[must_use]
    pub fn signal_room(&self) -> Option<String> {
        self.map
            .as_ref()
            .and_then(|map| map.signal.clone())
            .or_else(|| self.single.clone())
    }

    /// Decide where an envelope of `kind` narrating workspace `repo` (an absolute
    /// workspace root, basename-reduced per #4201) goes.
    ///
    /// Pure — the caller performs any room creation and reports the outcome back
    /// via [`record_created`](Self::record_created) /
    /// [`record_degraded`](Self::record_degraded).
    #[must_use]
    pub fn resolve(&self, kind: &str, repo: Option<&str>) -> RoomDecision {
        // Absent `rooms` map ⇒ the pre-#4225 behavior, byte-identical: one room
        // for everything, `None` included. This is the migration default and the
        // single most important invariant of this change.
        let Some(map) = self.map.as_ref() else {
            return RoomDecision::Send(self.single.clone());
        };
        // An unparseable kind cannot reach the wire (`build_send_request` refuses
        // it), but if one ever did, the operator-visible room is the safer place
        // for it than a muted firehose.
        let class =
            EnvelopeKind::parse(kind).map_or(AttentionClass::Signal, EnvelopeKind::attention_class);
        if matches!(class, AttentionClass::Signal) {
            return RoomDecision::Send(self.signal_room());
        }
        // Firehose class, but the firehose is *per repo* — an event with no repo
        // stamped (a synthetic/test event, or `DaemonIdleExit`-shaped daemon-wide
        // news) has no firehose to go to, so it degrades to the signal room
        // rather than inventing a room name.
        let Some(repo) = repo_basename(repo) else {
            return RoomDecision::Send(self.signal_room());
        };
        if let Some(room) = map.by_repo.get(&repo).or_else(|| self.created.get(&repo)) {
            return RoomDecision::Send(Some(room.clone()));
        }
        if self.degraded.contains(&repo) {
            return RoomDecision::Send(self.signal_room());
        }
        RoomDecision::Create {
            alias: repo_room_alias(&repo),
            repo,
            fallback: self.signal_room(),
        }
    }

    /// Record a successful lazy room creation so later messages for `repo` route
    /// straight there with no further `create_room` op.
    pub fn record_created(&mut self, repo: &str, room: String) {
        self.created.insert(repo.to_owned(), room);
    }

    /// Record a failed lazy room creation. Returns `true` **only the first time**
    /// for a given repo, which is how the caller warns once per repo instead of
    /// once per message.
    pub fn record_degraded(&mut self, repo: &str) -> bool {
        self.degraded.insert(repo.to_owned())
    }
}

/// The workspace root stamped onto a bus event, if any — the input firehose
/// routing keys on (reduced to a basename by [`repo_basename`]). Events with no
/// `repo` field (or `None`) route to the signal room.
#[must_use]
fn event_repo(event: &Event) -> Option<&str> {
    match event {
        Event::SweepPhase { repo, .. }
        | Event::SweepBlocker { repo, .. }
        | Event::SweepExited { repo, .. }
        | Event::SweepCrashed { repo, .. }
        | Event::SweepResumeDispatched { repo, .. }
        | Event::SweepGlobalDispatch { repo, .. } => repo.as_deref(),
        _ => None,
    }
}

// ============================================================================
// Repo qualification + body-grammar helpers (issue #4201)
// ============================================================================

/// Convention (issue #4201, documented in `.loom/docs/safehouse.md`): the
/// narration-friendly repo name is the **basename of the workspace-root
/// filesystem path** stamped onto the event's `repo` field by
/// `SweepRegistry::emit_event` (e.g. `/Users/x/GitHub/vibesql` → `vibesql`).
/// This is a path-derived directory name, not a forge `owner/repo` slug — it
/// needs no network call, and the daemon's workspace registry already
/// guarantees at most one managed registry per path. Returns `None` when
/// `repo` is absent (a pre-#3929/#4201 event, or a synthetic test event) so
/// callers can fall back to the old unqualified form.
fn repo_basename(repo: Option<&str>) -> Option<String> {
    repo.and_then(|r| Path::new(r).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}

/// Fold `s` into the `task_id` charset (`[A-Za-z0-9_]`, enforced in
/// [`build_send_request`]) by replacing every other character with `_` —
/// mirrors the hyphen→underscore fold [`normalize_to`] already applies to
/// personas, generalized to any non-alphanumeric byte (repo basenames may
/// contain `-`, `.`, etc.).
fn sanitize_task_id_segment(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Build the repo-qualified `task_id` for a narrated event: `<repo>_<issue>`
/// using the sanitized workspace basename, so the same issue number in two
/// managed repos (e.g. loom #4201 vs vibesql #4201) threads into **distinct**
/// Matrix threads instead of colliding (issue #4201, problem 1 — the bug this
/// module previously had). Falls back to the bare issue number when no `repo`
/// is known, preserving the pre-#4201 behavior for synthetic/test events and
/// any future event variant that is never stamped.
fn qualify_task_id(repo: Option<&str>, issue: u32) -> String {
    match repo_basename(repo) {
        Some(name) => format!("{}_{issue}", sanitize_task_id_segment(&name)),
        None => issue.to_string(),
    }
}

/// Build the `<repo>#<issue>` prefix that starts every narrated body (issue
/// #4201's body grammar). Falls back to a bare `#<issue>` when no `repo` is
/// known.
fn repo_issue_prefix(repo: Option<&str>, issue: u32) -> String {
    match repo_basename(repo) {
        Some(name) => format!("{name}#{issue}"),
        None => format!("#{issue}"),
    }
}

/// Format a duration given in whole seconds as `<m>m<s>s`, dropping the
/// minutes segment when it is zero — e.g. `415` → `6m55s`, `24` → `24s`
/// (matches issue #4201's grammar examples). Negative input (never produced by
/// the reaper, but `duration_sec` is a plain `i64`) clamps to zero rather than
/// rendering a negative duration.
fn format_narrated_duration(sec: i64) -> String {
    let sec = sec.max(0);
    let minutes = sec / 60;
    let seconds = sec % 60;
    if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Decode a **well-known** exit code into a short parenthetical meaning, e.g.
/// `78` → the `sysexits.h` `EX_CONFIG` code the token pool uses for an
/// exhausted/missing pool (`.loom/docs/token-pool.md`). Every other code
/// prints raw with no annotation — issue #4201 deliberately does not attempt a
/// full sysexits decode table, only the one code operators actually hit.
fn decode_exit_code_annotation(code: i32) -> &'static str {
    match code {
        78 => " (EX_CONFIG: token pool)",
        _ => "",
    }
}

// ============================================================================
// Event → envelope mapping (existing frozen taxonomy only)
// ============================================================================

/// Map an existing bus [`Event`] to a narration [`Envelope`], or `None` for
/// events phase 1 does not narrate.
///
/// Every narrated body starts with the repo-qualified `<repo>#<issue>` prefix
/// ([`repo_issue_prefix`]) and every narrated `task_id` is likewise
/// repo-qualified ([`qualify_task_id`]) — issue #4201, problem 1 — so the same
/// issue number in two managed repos threads into distinct Matrix threads
/// instead of colliding:
///
/// | Event | type | body |
/// |---|---|---|
/// | `SweepGlobalDispatch(Issue n)` | `task` | `<repo>#n · dispatch` (the sink, [`run_sink`], best-effort appends ` — "<issue title>"`) |
/// | `SweepPhase` | `task` | `<repo>#n · <phase>` (+ ` · PR #m open` when present) |
/// | `SweepBlocker` | `handoff` | `<repo>#n · BLOCKED — <reason>` |
/// | `SweepExited` | `ack` | `<repo>#n · done ✓ · <dur>` or `<repo>#n · failed ✗ · exit <code>[ (decoded)] · <dur>` |
/// | `SweepCrashed` | `handoff` | `<repo>#n · crashed ✗ at <checkpoint_phase> — resumable (checkpoint kept)` |
/// | `SweepResumeDispatched` (#4256) | `handoff` | `<repo>#n · reaper resumed crashed sweep at <phase> (open PR #m) — resuming without operator intervention` (or a "still stranded" variant when the resume dispatch itself failed) |
///
/// `SweepGlobalCompleted` is intentionally **not** narrated: it carries only a
/// `sweep_id` (no issue number), and `SweepExited` already emits the completion
/// `ack` with richer data — narrating both would double-post per completion.
///
/// This mapping is 1:1 and pure. The **second** envelope a `SweepExited` can
/// produce — the public-feed `completion` (#4426) — is built by
/// [`completion_for_exit`] instead, since it needs an async forge lookup to
/// confirm the merge; [`run_sink`] emits it after this one.
#[must_use]
pub fn event_to_envelope(event: &Event) -> Option<Envelope> {
    match event {
        Event::SweepGlobalDispatch {
            kind: SweepKind::Issue(issue),
            repo,
            ..
        } => Some(Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
            body: format!("{} · dispatch", repo_issue_prefix(repo.as_deref(), *issue)),
            meta: None,
        }),
        Event::SweepPhase {
            issue,
            phase,
            pr_number,
            repo,
        } => {
            let mut body = format!("{} · {phase}", repo_issue_prefix(repo.as_deref(), *issue));
            if let Some(pr) = pr_number {
                body.push_str(&format!(" · PR #{pr} open"));
            }
            Some(Envelope {
                to: "*".to_owned(),
                kind: "task".to_owned(),
                task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
                body,
                meta: None,
            })
        }
        Event::SweepBlocker {
            issue,
            reason,
            repo,
            ..
        } => Some(Envelope {
            to: "*".to_owned(),
            kind: "handoff".to_owned(),
            task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
            body: format!("{} · BLOCKED — {reason}", repo_issue_prefix(repo.as_deref(), *issue)),
            meta: None,
        }),
        Event::SweepExited {
            issue,
            exit_code,
            duration_sec,
            no_progress,
            death_class: _,
            repo,
        } => {
            let prefix = repo_issue_prefix(repo.as_deref(), *issue);
            let dur = format_narrated_duration(*duration_sec);
            let body = match exit_code {
                // #4366: a clean exit with zero lifecycle progress (parked on
                // a monitored background task) narrates distinctly from an
                // ordinary benign self-skip so operators can see the failure
                // class at a glance.
                Some(0) if *no_progress => {
                    format!("{prefix} · no progress ⚠ · exit 0, no checkpoint/PR · {dur}")
                }
                Some(0) => format!("{prefix} · done ✓ · {dur}"),
                Some(code) => format!(
                    "{prefix} · failed ✗ · exit {code}{} · {dur}",
                    decode_exit_code_annotation(*code)
                ),
                None => format!("{prefix} · failed ✗ · exit ? · {dur}"),
            };
            Some(Envelope {
                to: "*".to_owned(),
                kind: "ack".to_owned(),
                task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
                body,
                meta: None,
            })
        }
        Event::SweepCrashed {
            issue,
            checkpoint_phase,
            classification: _,
            death_class: _,
            repo,
        } => {
            let phase = checkpoint_phase.as_deref().unwrap_or("unknown");
            Some(Envelope {
                to: "*".to_owned(),
                kind: "handoff".to_owned(),
                task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
                body: format!(
                    "{} · crashed ✗ at {phase} — resumable (checkpoint kept)",
                    repo_issue_prefix(repo.as_deref(), *issue)
                ),
                meta: None,
            })
        }
        Event::SweepResumeDispatched {
            issue,
            pr,
            checkpoint_phase,
            dispatched,
            repo,
        } => {
            let phase = checkpoint_phase.as_deref().unwrap_or("unknown");
            let prefix = repo_issue_prefix(repo.as_deref(), *issue);
            let body = if *dispatched {
                format!(
                    "{prefix} · reaper resumed crashed sweep at {phase} (open PR #{pr}) — \
                     resuming without operator intervention"
                )
            } else {
                format!(
                    "{prefix} · reaper attempted resume at {phase} (open PR #{pr}) but the \
                     dispatch itself failed — still stranded, needs a look"
                )
            };
            Some(Envelope {
                to: "*".to_owned(),
                kind: "handoff".to_owned(),
                task_id: Some(qualify_task_id(repo.as_deref(), *issue)),
                body,
                meta: None,
            })
        }
        Event::DaemonIdleExit {
            trigger,
            idle_minutes,
            in_flight_sweeps,
            active_role_runs,
            healthy_tokens,
            total_tokens,
            message,
        } => Some(Envelope {
            to: "*".to_owned(),
            kind: "handoff".to_owned(),
            task_id: Some("daemon-idle-exit".to_owned()),
            body: message.clone(),
            meta: Some(serde_json::json!({
                "trigger": trigger,
                "idle_minutes": idle_minutes,
                "in_flight_sweeps": in_flight_sweeps,
                "active_role_runs": active_role_runs,
                "healthy_tokens": healthy_tokens,
                "total_tokens": total_tokens,
            })),
        }),
        // SweepGlobalCompleted (no issue number — SweepExited covers it),
        // SweepGlobalDispatch(PrSet), EpicAction, CapacityAdvisory, TopicLag,
        // Generic: not narrated in phase 1.
        _ => None,
    }
}

// ============================================================================
// Completion envelope (#4426) — the public-feed emit point
// ============================================================================

/// Forge facts about the merged PR behind a completed sweep, read from `gh`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MergedPr {
    number: u32,
    /// Canonical web URL (`completion-v1` `ref`).
    url: String,
    /// Display fields (#4497), read from the *same* `gh pr list` call that
    /// verifies the merge — zero extra round-trips. Each degrades to `None`
    /// independently: a row missing one of them still yields a `MergedPr`, so a
    /// completion is never lost over a cosmetic field.
    title: Option<String>,
    additions: Option<u64>,
    deletions: Option<u64>,
}

/// Build the `completion` envelope for a merged sweep. Pure and total: every
/// field is supplied by the caller and the `completion-v1` `meta` is validated
/// by [`CompletionMeta::to_meta_value`], so an envelope that would degrade to
/// `chat` server-side becomes an `Err` here instead.
///
/// The `body` is the human sentence Element renders (`<repo>#N · merged ✓ · PR
/// #M · <dur>`, following #4201's body grammar); `meta` is the machine view the
/// egress feed publishes. `task_id` reuses the repo-qualified narration thread
/// key so the completion lands in the same Matrix thread as that issue's
/// dispatch/phase/exit lines.
pub fn build_completion_envelope(
    repo: Option<&str>,
    issue: u32,
    pr: u32,
    duration_sec: i64,
    meta: &CompletionMeta,
) -> Result<Envelope> {
    let meta_value = meta.to_meta_value()?;
    Ok(Envelope {
        to: "*".to_owned(),
        kind: "completion".to_owned(),
        task_id: Some(qualify_task_id(repo, issue)),
        body: format!(
            "{} · merged ✓ · PR #{pr} · {}",
            repo_issue_prefix(repo, issue),
            format_narrated_duration(duration_sec)
        ),
        meta: Some(meta_value),
    })
}

/// Best-effort `gh pr list --state merged` lookup confirming that the sweep's
/// branch actually landed (#4426). Mirrors the forge-truth check
/// `worktree_ops::clean::check_pr_merged` performs, but async (the sink runs on
/// the daemon runtime and must never block it) and returning the PR `url` the
/// `completion-v1` `ref` needs.
///
/// **Exit 0 is not a merge**: a sweep that ends cleanly with its PR still open
/// (awaiting a Judge, or merged via `--auto` after the sweep exits) returns
/// `None` here and narrates no completion, so `result: "success"` is never
/// claimed for unmerged work. Every failure — missing `gh`, no network,
/// unauthenticated, timeout, malformed JSON — also degrades to `None`.
///
/// The same single call also harvests the feed's display fields (#4497:
/// `title`/`additions`/`deletions`), so enriching the completion costs **zero**
/// extra forge round-trips on the happy path.
async fn fetch_merged_pr(workspace_root: &Path, issue: u32) -> Option<MergedPr> {
    let gh_bin = env_nonempty(GH_BIN_ENV).unwrap_or_else(|| "gh".to_owned());
    let branch = crate::worktree_ops::naming::branch_name(issue);
    let mut output =
        run_merged_pr_query(&gh_bin, &branch, MERGED_PR_FIELDS, workspace_root).await?;
    if !output.status.success() && rejects_unknown_json_field(&output.stderr) {
        // A `gh` old enough not to know one of the #4497 display fields rejects
        // the *whole* request rather than omitting that field, which would have
        // silently cost us every completion. Retry the pre-#4497 field set so
        // such a host keeps publishing completions, just without the extras.
        log::debug!(
            "safehouse: gh rejected the completion display fields; \
             retrying with the base field set (completion will omit title/additions/deletions)"
        );
        output =
            run_merged_pr_query(&gh_bin, &branch, MERGED_PR_FIELDS_BASE, workspace_root).await?;
    }
    if !output.status.success() {
        return None;
    }
    let rows: Value = serde_json::from_slice(&output.stdout).ok()?;
    let row = rows.as_array()?.first()?;
    // `--state merged` should already guarantee this, but a null `mergedAt`
    // means the merge is unconfirmed — treat it as "not merged" rather than
    // publishing a success to a public feed on a guess.
    let merged_at = row.get("mergedAt").and_then(Value::as_str)?;
    if merged_at.trim().is_empty() {
        return None;
    }
    let number = u32::try_from(row.get("number")?.as_u64()?).ok()?;
    let url = row.get("url")?.as_str()?.to_owned();
    // Display fields are read with `and_then`/`filter` rather than `?`: an
    // absent or wrongly-typed one must degrade that field alone, never the
    // merge verification.
    let title = row
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(ToOwned::to_owned);
    let additions = row.get("additions").and_then(Value::as_u64);
    let deletions = row.get("deletions").and_then(Value::as_u64);
    (!url.is_empty()).then_some(MergedPr {
        number,
        url,
        title,
        additions,
        deletions,
    })
}

/// One `gh pr list --head <branch> --state merged --json <fields>` invocation,
/// bounded by [`MERGE_CHECK_TIMEOUT`]. `None` means the process could not be
/// run or did not finish in time; a nonzero exit is returned to the caller so it
/// can inspect `stderr`.
async fn run_merged_pr_query(
    gh_bin: &str,
    branch: &str,
    fields: &str,
    workspace_root: &Path,
) -> Option<std::process::Output> {
    let run = tokio::process::Command::new(gh_bin)
        .arg("pr")
        .arg("list")
        .arg("--head")
        .arg(branch)
        .arg("--state")
        .arg("merged")
        .arg("--json")
        .arg(fields)
        .arg("--limit")
        .arg("1")
        .current_dir(workspace_root)
        .output();
    tokio::time::timeout(MERGE_CHECK_TIMEOUT, run)
        .await
        .ok()?
        .ok()
}

/// Whether `gh` failed specifically because it does not recognize one of the
/// requested `--json` fields (`unknown JSON field: "additions"`), as opposed to
/// the ordinary degradations (no auth, no network, not a repo). Only this case
/// is worth a retry with a narrower field set.
fn rejects_unknown_json_field(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .to_ascii_lowercase()
        .contains("unknown json field")
}

/// Best-effort `gh repo view --json nameWithOwner` lookup for the forge
/// `owner/repo` slug (#4426). The `completion-v1` `repo` field is the forge
/// identity the feed links and displays — deliberately **not** the
/// path-basename narration convention (#4201) used for `task_id`/body
/// prefixes, which is a local directory name with no forge meaning.
async fn fetch_repo_slug(workspace_root: &Path) -> Option<String> {
    let gh_bin = env_nonempty(GH_BIN_ENV).unwrap_or_else(|| "gh".to_owned());
    let run = tokio::process::Command::new(&gh_bin)
        .arg("repo")
        .arg("view")
        .arg("--json")
        .arg("nameWithOwner")
        .arg("--jq")
        .arg(".nameWithOwner")
        .current_dir(workspace_root)
        .output();
    let output = tokio::time::timeout(MERGE_CHECK_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let slug = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    valid_repo_slug(&slug).then_some(slug)
}

/// [`fetch_repo_slug`] with a process-lifetime cache keyed by workspace root.
/// A repo's slug does not change while the daemon runs, so this is one `gh`
/// call per managed workspace rather than one per sweep completion.
async fn fetch_repo_slug_cached(
    cache: &mut HashMap<String, String>,
    workspace_root: &str,
) -> Option<String> {
    if let Some(slug) = cache.get(workspace_root) {
        return Some(slug.clone());
    }
    let slug = fetch_repo_slug(Path::new(workspace_root)).await?;
    cache.insert(workspace_root.to_owned(), slug.clone());
    Some(slug)
}

/// Best-effort per-issue token total for a `completion` envelope (#4497):
/// `sum(input + output)` over the activity DB's per-issue cost rollup
/// ([`ActivityDb::get_cost_by_issue`]). The DB is already in-process, so this
/// needs no forge call and no new accounting.
///
/// **Attribution is imperfect, by explicit operator decision** — for a cost
/// *trend*, imperfect-but-consistent beats absent. Two known limits, documented
/// here rather than papered over:
///
/// 1. **Not repo-qualified.** The activity DB's forge-correlation table keys on
///    a bare issue number with no repo column, so a daemon managing several
///    repos conflates identical issue numbers across them.
/// 2. **Only as good as the prompt↔usage linkage.** The rollup joins recorded
///    token samples to an issue through `agent_inputs`; samples recorded without
///    that link contribute nothing, so the figure is a **floor**, not a full
///    accounting — and is empty on hosts where nothing has established the link
///    yet, in which case the key is simply omitted.
///
/// Every failure — no DB handle, a poisoned mutex, a query error, a timeout, an
/// empty rollup — degrades to `None`. A zero total is likewise indistinguishable
/// from "accounting had nothing" and is filtered out downstream by
/// [`CompletionMeta::to_meta_value`], so no bogus `0` reaches the feed.
async fn fetch_issue_tokens(
    activity_db: Option<&Arc<Mutex<ActivityDb>>>,
    issue: u32,
) -> Option<u64> {
    let db = activity_db?.clone();
    let issue_number = i32::try_from(issue).ok()?;
    // The rusqlite handle sits behind a std mutex shared with the IPC recorder's
    // writes, so lock+query goes to the blocking pool: the sink must never park
    // a daemon-runtime worker on it (and a std guard cannot cross an `await`).
    let query = tokio::task::spawn_blocking(move || {
        let rollup = {
            let guard = db.lock().ok()?;
            guard.get_cost_by_issue(Some(issue_number)).ok()?
        };
        let total = rollup.iter().fold(0_i64, |acc, row| {
            acc.saturating_add(
                row.total_input_tokens
                    .saturating_add(row.total_output_tokens),
            )
        });
        u64::try_from(total).ok()
    });
    tokio::time::timeout(TOKEN_LOOKUP_TIMEOUT, query)
        .await
        .ok()?
        .ok()?
}

/// The Option-B emit point (#4426): on a `SweepExited`, verify against forge
/// truth that the sweep's PR actually merged and, if so, build the
/// public-feed `completion` envelope.
///
/// Runs for **every** exit code, not just `0`: a sweep can land its PR and
/// still exit nonzero on post-merge cleanup, and the merge — not the exit
/// status — is what the feed reports. `already_narrated` keeps that to one
/// completion per `(workspace, issue)` for the life of the daemon, so a
/// resumed sweep's second `SweepExited` does not double-post (downstream ingest
/// is additionally idempotent on `event_id`, which covers daemon restarts).
///
/// Returns `None` — silently, and without ever touching the sweep — when the
/// PR did not merge, when any `gh` lookup fails, or when the assembled `meta`
/// fails validation (that last case warns, since it is a client bug rather
/// than an expected outcome). A built completion is marked as narrated even if
/// the subsequent send fails: dropped narration is never retried (the module's
/// standing contract), and a retry would risk a double-post instead.
///
/// `result: "failure"` is deliberately **not** emitted in v1: `completion-v1`
/// requires a `ref`, and a sweep that produced no merged PR has no meaningful
/// one (an open PR is un-finished, not failed, and is usually resumed). The
/// wire support exists ([`CompletionResult::Failure`]) for a follow-up that
/// identifies a genuinely terminal negative outcome.
///
/// The feed's display fields (#4497) ride along here: `title`/`additions`/
/// `deletions` come out of the merge-verification call itself (no extra forge
/// round-trip) and `tokens` out of the in-process activity DB when a handle was
/// threaded in. All four are optional and independently degradable — with all
/// four absent the envelope is byte-identical to the pre-#4497 one.
#[allow(clippy::too_many_arguments)]
async fn completion_for_exit(
    persona: &str,
    workspace_root: &str,
    issue: u32,
    duration_sec: i64,
    exited_at: DateTime<Utc>,
    slug_cache: &mut HashMap<String, String>,
    already_narrated: &mut std::collections::HashSet<(String, u32)>,
    activity_db: Option<&Arc<Mutex<ActivityDb>>>,
) -> Option<Envelope> {
    let key = (workspace_root.to_owned(), issue);
    if already_narrated.contains(&key) {
        return None;
    }
    let merged = fetch_merged_pr(Path::new(workspace_root), issue).await?;
    let slug = fetch_repo_slug_cached(slug_cache, workspace_root).await?;
    // Only reached once the merge is confirmed, so a completion is never delayed
    // by a token lookup it would not have published.
    let tokens = fetch_issue_tokens(activity_db, issue).await;

    // Sweep timing comes from the one clock that produced `duration_sec` (the
    // reaper's), so the pair is always self-consistent — mixing in the forge's
    // `mergedAt` could render a `completed_at` before `started_at`.
    let started_at = exited_at - chrono::Duration::seconds(duration_sec.max(0));
    let meta = CompletionMeta {
        agent: persona.to_owned(),
        repo_slug: slug,
        pr_url: merged.url,
        result: CompletionResult::Success,
        started_at: started_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        completed_at: exited_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        issue: Some(issue),
        // Best-effort, knowingly imperfect attribution (#4497) — see
        // `fetch_issue_tokens`. Absent/zero ⇒ the key is omitted, never guessed.
        tokens,
        title: merged.title,
        additions: merged.additions,
        deletions: merged.deletions,
    };
    match build_completion_envelope(Some(workspace_root), issue, merged.number, duration_sec, &meta)
    {
        Ok(envelope) => {
            already_narrated.insert(key);
            Some(envelope)
        }
        Err(err) => {
            log::warn!(
                "safehouse: refusing to narrate completion for issue #{issue} \
                 ({err:#}); sweep unaffected"
            );
            None
        }
    }
}

// ============================================================================
// Client
// ============================================================================

/// Why a [`SafehouseClient::send`] failed — split so the sink can tell a
/// **protocol rejection** (the connection is healthy; retrying identically will
/// be rejected identically) apart from a **transport failure** (the connection
/// is gone; reconnect) without string-matching an untyped `anyhow` chain
/// (#4464).
#[derive(Debug)]
pub enum SendError {
    /// safehoused accepted the request over the wire but refused it at the
    /// protocol layer (`ok:false`). `reason` is the raw `error` string — the
    /// canonical case is `'room' required: N rooms joined` on a multi-room host
    /// with [`SafehouseConfig::room`] unset. The connection stays usable.
    Rejected { reason: String },
    /// A transport-level failure (write/read I/O, closed connection, or a reply
    /// `id` desync) — the connection is unusable and the caller should
    /// reconnect.
    Transport(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { reason } => write!(f, "safehoused rejected send: {reason}"),
            Self::Transport(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for SendError {}

/// A blocking-free async envelope-v1 client over one `AF_UNIX` connection.
pub struct SafehouseClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    room: Option<String>,
}

impl SafehouseClient {
    /// Connect, perform the mandatory `hello` handshake, and verify the persona
    /// was accepted. Errors (socket absent/refused, persona rejected) are
    /// returned for the caller to degrade to a `warn`.
    pub async fn connect(socket: &Path, persona: &str, room: Option<String>) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to safehoused at {}", socket.display()))?;
        let (read_half, write_half) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            next_id: 0,
            room,
        };

        let hello = json!({"id": 0, "op": "hello", "persona": persona});
        client.write_line(&hello).await?;
        let reply = client.read_reply().await?;
        if !reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let err = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("safehoused rejected persona {persona:?}: {err}");
        }
        client.next_id = 1;
        Ok(client)
    }

    /// Serialize and send one narration envelope into this connection's default
    /// room, then read + `id`-match the reply (skipping any interleaved push
    /// line).
    pub async fn send(&mut self, env: &Envelope) -> std::result::Result<(), SendError> {
        let room = self.room.clone();
        self.send_to(env, room.as_deref()).await
    }

    /// [`send`](Self::send) addressed at an explicit `room`, which is how the
    /// attention-class router (#4225) puts one connection's envelopes into
    /// different rooms. `None` sends no `room` key at all (the single-room
    /// convenience). The connection's own default room is ignored.
    pub async fn send_to(
        &mut self,
        env: &Envelope,
        room: Option<&str>,
    ) -> std::result::Result<(), SendError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        // A malformed envelope is a transport-class caller bug (the connection
        // is fine) but is not a server rejection either; surface it as
        // Transport so the sink logs it rather than treating it as sticky.
        let req = build_send_request(env, id, room).map_err(SendError::Transport)?;
        self.write_line(&req).await.map_err(SendError::Transport)?;
        let reply = self.read_reply().await.map_err(SendError::Transport)?;
        if !reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let reason = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned();
            return Err(SendError::Rejected { reason });
        }
        // Replies echo the request id (handle_conn stamps it). A mismatch means
        // the stream desynced — treat it as a transport error so the sink
        // reconnects.
        if let Some(reply_id) = reply.get("id").and_then(Value::as_u64) {
            if reply_id != id {
                return Err(SendError::Transport(anyhow::anyhow!(
                    "safehoused reply id {reply_id} != request id {id} (stream desync)"
                )));
            }
        }
        Ok(())
    }

    /// Lazily create (or resolve) the room named `alias` via the socket
    /// `create_room` op and return the value later `send`s should address
    /// (#4225's tier-2 firehose rooms are created on a repo's **first**
    /// narration, never eagerly for every managed repo).
    ///
    /// The op/reply shape is owned by the external `rjwalters/safehouse` repo and
    /// is not verifiable from this repository, so this is deliberately lenient in
    /// both directions: the request names the room with both `name` and `alias`
    /// (safehoused ignores unknown keys the same way it ignores our `v`), and the
    /// reply's room identity is read from whichever of the plausible keys is
    /// present, falling back to the alias we asked for (safehoused accepts an
    /// alias anywhere it accepts a room id). Every failure is an `Err` the caller
    /// degrades from; nothing here can block or fail a sweep.
    ///
    /// The error is the same [`SendError`] split the send path uses, and for the
    /// same reason: a [`SendError::Rejected`] (safehoused said no — unsupported
    /// op, no permission to create) will be refused identically forever, so the
    /// caller gives up on that room permanently and warns once, while a
    /// [`SendError::Transport`] is a dead connection that says nothing about
    /// whether the room is creatable — so the next event retries after the
    /// reconnect instead of writing the repo off.
    pub async fn create_room(&mut self, alias: &str) -> std::result::Result<String, SendError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let req = json!({
            "id": id,
            "op": "create_room",
            "name": alias,
            "alias": alias,
        });
        self.write_line(&req).await.map_err(SendError::Transport)?;
        let reply = self.read_reply().await.map_err(SendError::Transport)?;
        if !reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let reason = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned();
            return Err(SendError::Rejected { reason });
        }
        let room = ["room_id", "room", "alias", "name"]
            .iter()
            .find_map(|key| reply.get(*key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|room| !room.is_empty())
            .map_or_else(|| alias.to_owned(), ToOwned::to_owned);
        Ok(room)
    }

    async fn write_line(&mut self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Read the next reply line, **skipping push lines** — any line carrying an
    /// `event` key (and no `id`) is an async inbound room event, not a reply.
    async fn read_reply(&mut self) -> Result<Value> {
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                bail!("safehoused closed the connection");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value =
                serde_json::from_str(trimmed).context("bad reply from safehoused")?;
            if value.get("event").is_some() {
                // Interleaved async push (inbound room event) — demultiplex it
                // out. The **narration** connection is emit-only and discards
                // inbound; peer-claim consumption (#4028) runs on a *dedicated*
                // read task ([`run_coordination`]) on its own connection, so an
                // idle daemon that emits no narration still observes peer
                // advertisements promptly (Gap 1a).
                continue;
            }
            return Ok(value);
        }
    }

    /// Consume `self` into its raw halves so a caller (the peer-coordination
    /// task) can read inbound room events and write outbound claim ads
    /// **concurrently** on one connection — the narration [`send`](Self::send)
    /// path reads its own reply inline and cannot be driven by a `select!` loop.
    #[must_use]
    pub fn into_parts(self) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf, u64, Option<String>) {
        (self.reader, self.writer, self.next_id, self.room)
    }
}

// ============================================================================
// Sink
// ============================================================================

/// Spawn the narration sink on `runtime` when enabled. Returns the task handle,
/// or `None` (a byte-for-byte no-op: no bus subscription, no socket) when
/// disabled or when no socket path can be resolved. `state` (#4345) is updated
/// with the resolved config-only state immediately (before any connection
/// attempt) and further updated by [`run_sink`] as connect/disconnect
/// transitions happen — see [`SafehouseState`].
///
/// `activity_db` (#4497) is the optional in-process handle the completion emit
/// point uses for its best-effort per-issue `tokens` rollup; `None` simply omits
/// that one field.
#[must_use]
pub fn spawn_sink(
    config: SafehouseConfig,
    bus: &EventBus,
    runtime: &tokio::runtime::Handle,
    state: SharedSafehouseState,
    activity_db: Option<Arc<Mutex<ActivityDb>>>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        // Disabled ⇒ do not even subscribe. No syscalls, no behavior change.
        set_state(&state, SafehouseState::NotConfigured);
        return None;
    }
    let Some(socket) = resolve_socket(&config) else {
        log::warn!(
            "safehouse: enabled but no socket path resolved \
             (set safehouse.socket, $LOOM_SAFEHOUSE_SOCKET, or $SAFEHOUSED_SOCKET) — narration off"
        );
        // No socket ⇒ nothing to report as "unreachable at <path>"; the
        // degradation contract's "not configured" bucket also covers this
        // (AC: "not configured (no safehouse block / disabled)").
        set_state(&state, SafehouseState::NotConfigured);
        return None;
    };
    log::info!(
        "safehouse: narration enabled (persona={}, socket={})",
        config.persona,
        socket.display()
    );
    // Empty topic set ⇒ receive every event; we filter in `event_to_envelope`.
    let subscription = bus.subscribe(Vec::<String>::new());
    Some(runtime.spawn(async move {
        run_sink(
            config,
            socket,
            subscription,
            DEFAULT_MIN_BACKOFF,
            DEFAULT_MAX_BACKOFF,
            state,
            activity_db,
        )
        .await;
    }))
}

/// Best-effort `gh issue view --json title` lookup used to enrich the
/// dispatch line's body with the issue title (issue #4201). This is the
/// "documented sink-side fetch" tradeoff called for when threading the title
/// through a `SweepGlobalDispatch` payload amendment is judged too heavy: the
/// `repo` field earned its amendment because it fixes an actual cross-repo
/// collision bug, but the title is a pure UX nicety, so it is fetched here
/// instead, scoped entirely to this sink.
///
/// Bounded by [`TITLE_FETCH_TIMEOUT`] and swallows every failure — missing
/// `gh`, no network, unauthenticated, a nonexistent issue, a timeout — into
/// `None`. The caller narrates the dispatch line without a title rather than
/// blocking or dropping the narration entirely; this never affects the sweep
/// the event describes (the sink is a pure bus subscriber with no back-channel
/// to dispatch).
async fn fetch_issue_title(workspace_root: &Path, issue: u32) -> Option<String> {
    let gh_bin = env_nonempty(GH_BIN_ENV).unwrap_or_else(|| "gh".to_owned());
    let run = tokio::process::Command::new(&gh_bin)
        .arg("issue")
        .arg("view")
        .arg(issue.to_string())
        .arg("--json")
        .arg("title")
        .arg("--jq")
        .arg(".title")
        .current_dir(workspace_root)
        .output();
    let output = tokio::time::timeout(TITLE_FETCH_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let title = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!title.is_empty()).then_some(title)
}

/// [`fetch_issue_title`] with a short TTL cache (issue #4201) keyed by
/// `(workspace_root, issue)`, so a re-dispatch of the same issue within
/// [`TITLE_CACHE_TTL`] reuses the cached title instead of re-shelling to `gh`.
async fn fetch_title_cached(
    cache: &mut HashMap<(String, u32), (String, Instant)>,
    workspace_root: &str,
    issue: u32,
) -> Option<String> {
    let key = (workspace_root.to_owned(), issue);
    if let Some((title, fetched_at)) = cache.get(&key) {
        if fetched_at.elapsed() < TITLE_CACHE_TTL {
            return Some(title.clone());
        }
    }
    let title = fetch_issue_title(Path::new(workspace_root), issue).await?;
    cache.insert(key, (title.clone(), Instant::now()));
    Some(title)
}

/// The sink loop. Consumes bus events, maps them to envelopes, and best-effort
/// narrates them, reconnecting lazily with capped exponential backoff. A
/// connection failure never blocks or fails a sweep — it degrades to a single
/// `warn` per outage (not per event) and drops that narration.
#[allow(clippy::too_many_arguments)]
async fn run_sink(
    config: SafehouseConfig,
    socket: PathBuf,
    mut subscription: crate::event_bus::Subscription,
    min_backoff: Duration,
    max_backoff: Duration,
    state: SharedSafehouseState,
    activity_db: Option<Arc<Mutex<ActivityDb>>>,
) {
    // Report "configured, not yet connected" immediately — the sink connects
    // lazily on the first narrated event (below), so without this a daemon
    // that starts before any sweep activity would keep reading whatever the
    // cell held before this task existed (#4345 edge case: "daemon starts
    // before safehoused").
    set_state(
        &state,
        SafehouseState::Unreachable {
            socket: socket.clone(),
        },
    );
    let mut client: Option<SafehouseClient> = None;
    // Attention-class room routing (#4225). With no `rooms` map configured this
    // resolves every envelope to `config.room` — the pre-#4225 single-room
    // behavior, byte-identical.
    let mut router = RoomRouter::new(&config);
    // The room this connection reports as "connected to" and defaults sends to:
    // the signal room, which in single-room mode *is* `config.room`.
    let signal_room = router.signal_room();
    // Next instant a reconnect may be attempted, and the current backoff.
    let mut next_attempt = Instant::now();
    let mut backoff = min_backoff;
    // Suppress duplicate outage warnings — one warn per outage, not per event.
    let mut warned = false;
    // Sticky protocol-rejection state (#4464): `Some(reason)` once a `send` is
    // rejected at the protocol layer (e.g. `'room' required`). Survives
    // reconnects (a fresh `hello` does not clear it) and is only cleared by a
    // `send` that is actually accepted. Also dedups the rejection WARN.
    let mut send_rejected: Option<String> = None;
    // Short-TTL cache for the dispatch-line title lookup (issue #4201).
    let mut title_cache: HashMap<(String, u32), (String, Instant)> = HashMap::new();
    // Forge `owner/repo` slugs, and the (workspace, issue) pairs already
    // narrated as completions — both process-lifetime (#4426).
    let mut slug_cache: HashMap<String, String> = HashMap::new();
    let mut completed: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();

    loop {
        let event = match subscription.recv().await {
            Ok(event) => event,
            Err(RecvError::Closed) => {
                log::debug!("safehouse: event bus closed; narration sink stopping");
                break;
            }
            // Empty/Lagged are surfaced as events by `recv`; only Closed ends it.
            Err(_) => continue,
        };

        // One bus event can narrate more than one envelope: a `SweepExited`
        // whose PR merged emits its `ack` **and** the public-feed `completion`
        // (#4426), in that order.
        let mut outbox: Vec<Envelope> = Vec::new();

        if let Some(mut envelope) = event_to_envelope(&event) {
            // Best-effort dispatch-title enrichment (issue #4201, AC3). Only the
            // dispatch event needs it, and only when a repo is known (needed to
            // resolve the `gh` working directory) — every other event is narrated
            // exactly as `event_to_envelope` built it.
            if let Event::SweepGlobalDispatch {
                kind: SweepKind::Issue(issue),
                repo: Some(workspace_root),
                ..
            } = &event
            {
                if let Some(title) =
                    fetch_title_cached(&mut title_cache, workspace_root, *issue).await
                {
                    envelope.body.push_str(&format!(" — \"{title}\""));
                }
            }
            outbox.push(envelope);
        }

        // Public-feed completion (#4426). Needs the workspace root to resolve
        // the `gh` working directory, so an event with no `repo` stamped
        // narrates its `ack` only. Every failure inside degrades to `None`.
        if let Event::SweepExited {
            issue,
            duration_sec,
            repo: Some(workspace_root),
            ..
        } = &event
        {
            if let Some(completion) = completion_for_exit(
                &config.persona,
                workspace_root,
                *issue,
                *duration_sec,
                Utc::now(),
                &mut slug_cache,
                &mut completed,
                activity_db.as_ref(),
            )
            .await
            {
                outbox.push(completion);
            }
        }

        if outbox.is_empty() {
            continue;
        }

        // (Re)connect lazily, honoring the backoff window so an absent peer is
        // not hammered once per event.
        if client.is_none() {
            if Instant::now() < next_attempt {
                continue; // in backoff window — drop this narration silently
            }
            match SafehouseClient::connect(&socket, &config.persona, signal_room.clone()).await {
                Ok(connected) => {
                    if warned {
                        log::info!("safehouse: reconnected to {}", socket.display());
                    }
                    client = Some(connected);
                    backoff = min_backoff;
                    warned = false;
                    // Stickiness (#4464): a successful `hello` does not clear a
                    // prior send-rejection — only an accepted `send` does — so
                    // a reconnect after a transport blip preserves the
                    // `send_rejected` diagnosis rather than flashing "connected".
                    set_state(
                        &state,
                        match &send_rejected {
                            Some(reason) => SafehouseState::SendRejected {
                                socket: socket.clone(),
                                reason: reason.clone(),
                            },
                            None => SafehouseState::Connected {
                                socket: socket.clone(),
                                room: signal_room.clone(),
                            },
                        },
                    );
                }
                Err(err) => {
                    if !warned {
                        log::warn!(
                            "safehouse: cannot reach safehoused at {} ({err:#}); \
                             narration paused, sweep unaffected",
                            socket.display()
                        );
                        warned = true;
                    }
                    set_state(
                        &state,
                        SafehouseState::Unreachable {
                            socket: socket.clone(),
                        },
                    );
                    next_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }
        }

        // Send this event's envelopes in order, stopping at the first failure
        // (the connection is gone or every send would be rejected identically).
        // Each envelope's room is resolved **per envelope** by attention class
        // (#4225), which is why one `SweepExited` can put its `ack` in the signal
        // room and (in a future taxonomy) chatter in the repo firehose — severity
        // routes, and each message lands in exactly one room.
        let narrated_repo = event_repo(&event).map(ToOwned::to_owned);
        let mut send_failure: Option<SendError> = None;
        if let Some(connected) = client.as_mut() {
            for envelope in &outbox {
                let room = match router.resolve(&envelope.kind, narrated_repo.as_deref()) {
                    RoomDecision::Send(room) => room,
                    // Lazy creation (#4225): a repo's firehose room is created on
                    // its first narration, not eagerly for every managed repo.
                    RoomDecision::Create {
                        repo,
                        alias,
                        fallback,
                    } => match connected.create_room(&alias).await {
                        Ok(room) => {
                            log::info!(
                                "safehouse: created the {alias} firehose room for {repo} \
                                 narration (room={room})"
                            );
                            router.record_created(&repo, room.clone());
                            Some(room)
                        }
                        // safehoused refused: it will refuse identically until the
                        // operator changes something, so write the room off for
                        // this run, warn **once** per repo (never once per
                        // message), and narrate into the signal room instead.
                        Err(err @ SendError::Rejected { .. }) => {
                            if router.record_degraded(&repo) {
                                log::warn!(
                                    "safehouse: cannot create the {alias} firehose room \
                                     ({err}); narrating {repo} into the signal room instead \
                                     for the rest of this run, sweep unaffected"
                                );
                            }
                            fallback
                        }
                        // A dead connection says nothing about whether the room is
                        // creatable — do NOT write the repo off. The send below
                        // fails too, which drops this narration and reconnects;
                        // the next event retries the creation.
                        Err(SendError::Transport(err)) => {
                            log::debug!(
                                "safehouse: create_room for {alias} failed at the transport \
                                 layer ({err:#}); will retry after reconnect"
                            );
                            fallback
                        }
                    },
                };
                if let Err(err) = connected.send_to(envelope, room.as_deref()).await {
                    send_failure = Some(err);
                    break;
                }
            }
        }
        match send_failure {
            // An accepted send clears any prior sticky rejection (#4464): the
            // config was fixed (e.g. `safehouse.room` set + daemon restarted) —
            // return the status to "connected".
            None => {
                if send_rejected.take().is_some() {
                    log::info!("safehouse: narration accepted again; resuming");
                    set_state(
                        &state,
                        SafehouseState::Connected {
                            socket: socket.clone(),
                            room: signal_room.clone(),
                        },
                    );
                }
            }
            // Protocol rejection (#4464): the connection is healthy, so keep it
            // — retrying would be rejected identically until the operator fixes
            // config. Sticky: report "connected, sends rejected: <reason>" and
            // name the fix when the reason is a missing room. Dropped narration
            // is never retried (module contract), so we simply move on.
            Some(SendError::Rejected { reason }) => {
                if send_rejected.as_deref() != Some(reason.as_str()) {
                    if reason.contains("'room' required") {
                        log::warn!(
                            "safehouse: narration rejected — set safehouse.room — \
                             safehoused rejected send: {reason}; sweep unaffected"
                        );
                    } else {
                        log::warn!(
                            "safehouse: narration rejected (safehoused rejected send: \
                             {reason}); sweep unaffected"
                        );
                    }
                }
                send_rejected = Some(reason.clone());
                set_state(
                    &state,
                    SafehouseState::SendRejected {
                        socket: socket.clone(),
                        reason,
                    },
                );
            }
            // Transport failure: the connection is gone — drop it and reconnect
            // with backoff, exactly as before.
            Some(SendError::Transport(err)) => {
                log::warn!(
                    "safehouse: narration send failed ({err:#}); will reconnect, sweep unaffected"
                );
                client = None;
                set_state(
                    &state,
                    SafehouseState::Unreachable {
                        socket: socket.clone(),
                    },
                );
                next_attempt = Instant::now() + backoff;
                backoff = (backoff * 2).min(max_backoff);
                warned = true;
            }
        }
    }
}

// ============================================================================
// Peer-claim coordination (Issue #4028, Phase 1)
// ============================================================================

/// Bounded outbound claim-ad channel capacity. Ads are tiny and rare (one per
/// dispatch / terminal outcome); the bound only matters during a safehoused
/// outage, where [`SweepRegistry`](crate::sweep_registry::SweepRegistry)'s
/// `try_send` drops on `Full` (fail-open) rather than blocking the dispatch path.
pub const PEER_CLAIM_CHANNEL_CAP: usize = 256;

/// A generic sink for inbound room events. Kept intentionally generic (rather
/// than hard-wiring peer-claims) so the shared inbound read task can later fan an
/// event out to additional consumers — e.g. inbound human steering (the
/// follow-up noted at `.loom/docs/safehouse.md`) — without another connection.
pub trait InboundEventSink: Send + Sync {
    /// Handle one inbound room-event push line (a JSON object carrying an
    /// `event` key). Best-effort: an implementation must never panic or block.
    fn on_event(&self, event: &Value);
}

/// The peer-claim consumer: parses claim ads out of inbound room events and
/// folds them into a shared [`PeerClaimView`] (self-claim recognition + TTL live
/// in the view). A non-claim event (a human chat message, a narration line) is
/// silently ignored.
pub struct PeerClaimSink {
    view: Arc<Mutex<PeerClaimView>>,
}

impl PeerClaimSink {
    #[must_use]
    pub fn new(view: Arc<Mutex<PeerClaimView>>) -> Self {
        Self { view }
    }
}

impl InboundEventSink for PeerClaimSink {
    fn on_event(&self, event: &Value) {
        let Some(body) = event.get("body").and_then(Value::as_str) else {
            return;
        };
        let Some(ad) = ClaimAd::from_body_str(body) else {
            return; // not a claim (human chat, narration, malformed) — ignore
        };
        match self.view.lock() {
            Ok(mut view) => {
                let now = Instant::now();
                view.observe_at(&ad, now);
                // Opportunistically prune so a crashed peer's entries do not
                // accumulate between work-finder queries.
                view.prune_expired(now);
            }
            Err(poisoned) => {
                log::error!("safehouse: peer-claim view mutex poisoned ({poisoned:?})");
            }
        }
    }
}

/// Build the `task`-typed advertisement envelope for a claim ad (Gap 2 of
/// #4028): the envelope `type` enum is closed and owned by the safehouse repo, so
/// a claim rides a `task` envelope with the bare issue number as `task_id` and
/// the structured payload in `body`.
///
/// **Routing exception (#4225).** By the attention-class table this `task`
/// envelope would belong in the per-repo firehose — claim ads *are* per-repo
/// machine chatter. They deliberately stay on the **signal room** anyway; see
/// [`run_coordination`] for the full rationale (in one line: it is the only room
/// every host's bot is guaranteed to be joined to, and cross-host dedup is a
/// correctness property, not a cosmetic one).
#[must_use]
pub fn claim_ad_to_envelope(ad: &ClaimAd) -> Envelope {
    Envelope {
        to: "*".to_owned(),
        kind: "task".to_owned(),
        task_id: Some(ad.issue.to_string()),
        body: ad.to_body_json(),
        meta: None,
    }
}

/// Spawn the peer-claim coordination task: one dedicated safehouse connection
/// that **reads** inbound peer advertisements into `sink` and **writes** this
/// daemon's outbound claim ads drained from `outbound`. Returns `None` — a
/// byte-for-byte no-op (no socket, no task) — when safehouse is disabled or no
/// socket resolves, mirroring [`spawn_sink`]'s contract.
///
/// This is the **dedicated inbound read task** the issue's Gap 1a requires: it
/// drains the socket continuously via `select!`, so an idle daemon that emits no
/// narration still observes peer claims promptly (the narration sink's
/// `read_reply` only drains while it is emitting).
///
/// `state` (#4345) is updated with the resolved config-only state immediately
/// (before any connection attempt) and further updated by [`run_coordination`]
/// as connect/disconnect transitions happen — see [`SafehouseState`]. This
/// task connects **eagerly** (unlike the narration sink's lazy first-event
/// connect), so it is usually the first to observe a fresh daemon's true
/// connection state.
#[must_use]
pub fn spawn_peer_coordination(
    config: SafehouseConfig,
    sink: Arc<dyn InboundEventSink>,
    outbound: tokio::sync::mpsc::Receiver<ClaimAd>,
    runtime: &tokio::runtime::Handle,
    state: SharedSafehouseState,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        set_state(&state, SafehouseState::NotConfigured);
        return None; // disabled ⇒ no socket, no task, no syscalls
    }
    let Some(socket) = resolve_socket(&config) else {
        log::warn!(
            "safehouse: peer coordination enabled but no socket path resolved \
             (set safehouse.socket, $LOOM_SAFEHOUSE_SOCKET, or $SAFEHOUSED_SOCKET) — \
             soft-claim coordination off"
        );
        // See spawn_sink's identical fallback: no socket resolved groups under
        // "not configured", the AC's bucket for "nothing to even try".
        set_state(&state, SafehouseState::NotConfigured);
        return None;
    };
    log::info!(
        "safehouse: peer-claim coordination enabled (persona={}, socket={})",
        config.persona,
        socket.display()
    );
    Some(runtime.spawn(async move {
        run_coordination(
            config,
            socket,
            sink,
            outbound,
            DEFAULT_MIN_BACKOFF,
            DEFAULT_MAX_BACKOFF,
            state,
        )
        .await;
    }))
}

/// The coordination loop. Reconnects lazily with capped backoff; on each live
/// connection it concurrently reads inbound room events (→ `sink`) and drains
/// outbound claim ads (→ socket). Any I/O failure degrades to a reconnect — it
/// never blocks or fails a dispatch. Returns when all outbound senders drop.
///
/// # Room routing: claim ads stay on the signal room (#4225, resolved in-PR)
///
/// Attention-class routing sends `task` envelopes to the per-repo firehose, and a
/// claim ad *is* a per-repo `task` envelope — yet this connection deliberately
/// advertises into (and reads from) the **signal room**, the one deliberate
/// exception to "signal-only". Why:
///
/// 1. **The signal room is the only room with guaranteed common membership.**
///    Rooms are per-repo and created **lazily** by whichever host narrates that
///    repo first (`RoomDecision::Create`), and hosts run **separate per-host bot
///    accounts**. Host A creating `fleet-loom` does not join host B's bot to it,
///    so an ad posted there is invisible to B until an operator invites it —
///    silently disabling cross-host dedup with no error anywhere, exactly the
///    failure class #4464 had to add a status state for. Every host's bot is
///    already in the signal room; that is what makes it usable as a coordination
///    channel at all.
/// 2. **Dedup is correctness, room hygiene is cosmetics.** A missed claim ad
///    costs a duplicate cross-host sweep (wasted tokens, two PRs for one issue).
///    A little machine JSON in the signal room costs the operator some scroll.
///    When those trade off, correctness wins.
/// 3. **The reader must agree with the writer.** This task's inbound handler is
///    unfiltered — it folds *any* inbound line carrying a parseable `loom_claim`
///    body into the view — so it consumes ads from whatever rooms safehoused
///    pushes to it. Keeping the write side on the signal room keeps the pair
///    trivially consistent instead of depending on which rooms this host's bot
///    happens to have joined.
///
/// Ads are low volume (one per dispatch / terminal outcome). A dedicated
/// coordination room (a third tier) is the clean long-term fix and is left as a
/// follow-up — it needs cross-host *provisioning*, not just routing, which is the
/// same reason tier 3 (the Matrix Space) is out of scope here.
async fn run_coordination(
    config: SafehouseConfig,
    socket: PathBuf,
    sink: Arc<dyn InboundEventSink>,
    mut outbound: tokio::sync::mpsc::Receiver<ClaimAd>,
    min_backoff: Duration,
    max_backoff: Duration,
    state: SharedSafehouseState,
) {
    let mut backoff = min_backoff;
    let mut warned = false;
    // The signal room (see the routing rationale above). In single-room mode this
    // *is* `config.room`, so the pre-#4225 behavior is byte-identical.
    let signal_room = config.signal_room().map(ToOwned::to_owned);
    loop {
        set_state(
            &state,
            SafehouseState::Unreachable {
                socket: socket.clone(),
            },
        );
        let client = match SafehouseClient::connect(&socket, &config.persona, signal_room.clone())
            .await
        {
            Ok(client) => {
                if warned {
                    log::info!("safehouse: peer coordination reconnected to {}", socket.display());
                }
                backoff = min_backoff;
                warned = false;
                set_state(
                    &state,
                    SafehouseState::Connected {
                        socket: socket.clone(),
                        room: signal_room.clone(),
                    },
                );
                client
            }
            Err(err) => {
                if !warned {
                    log::warn!(
                        "safehouse: cannot reach safehoused for peer coordination at {} \
                             ({err:#}); coordination paused, dispatch unaffected",
                        socket.display()
                    );
                    warned = true;
                }
                // Drain-and-drop queued ads during the outage so the bounded
                // channel does not wedge; exit if all senders are gone.
                loop {
                    match outbound.try_recv() {
                        Ok(_) => continue,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        let (reader, mut writer, mut next_id, room) = client.into_parts();
        let mut lines = reader.lines();
        // Per-connection dedup for the claim-ad rejection WARN (#4464): one WARN
        // per outage, reset on each fresh connection — mirrors the sink's
        // `send_rejected` discipline.
        let mut ad_rejected = false;
        let reconnect = loop {
            tokio::select! {
                line = lines.next_line() => {
                    match line {
                        Ok(Some(l)) => {
                            let trimmed = l.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                                continue; // malformed line — fail-open, skip
                            };
                            if value.get("event").is_some() {
                                sink.on_event(&value);
                            } else if value.get("id").is_some()
                                && !value.get("ok").and_then(Value::as_bool).unwrap_or(true)
                            {
                                // A rejected reply (has `id`, no `event`,
                                // `ok:false`) to one of our own claim ads
                                // (#4464). On a multi-room host with
                                // `safehouse.room` unset these are rejected
                                // server-side with no other signal, silently
                                // disabling peer-claim dedup (#4028/#4431). WARN
                                // once per outage and name the fix when it is a
                                // missing room; dispatch is unaffected.
                                if !ad_rejected {
                                    let reason = value
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown error");
                                    if reason.contains("'room' required") {
                                        log::warn!(
                                            "safehouse: peer claim-ad rejected — set \
                                             safehouse.room — safehoused rejected send: \
                                             {reason}; peer-claim dedup disabled, dispatch \
                                             unaffected"
                                        );
                                    } else {
                                        log::warn!(
                                            "safehouse: peer claim-ad rejected (safehoused \
                                             rejected send: {reason}); peer-claim dedup \
                                             disabled, dispatch unaffected"
                                        );
                                    }
                                    ad_rejected = true;
                                }
                            }
                            // An accepted reply echo (has `id`, `ok:true`) to
                            // one of our own sends: nothing to do, drop it.
                        }
                        Ok(None) => break true,          // peer closed → reconnect
                        Err(e) => {
                            log::debug!("safehouse: coordination read error ({e}); reconnecting");
                            break true;
                        }
                    }
                }
                ad = outbound.recv() => {
                    match ad {
                        Some(ad) => {
                            let env = claim_ad_to_envelope(&ad);
                            let id = next_id;
                            next_id = next_id.wrapping_add(1);
                            match build_send_request(&env, id, room.as_deref()) {
                                Ok(req) => {
                                    let mut line = match serde_json::to_string(&req) {
                                        Ok(s) => s,
                                        Err(e) => {
                                            log::warn!("safehouse: cannot serialize claim ad ({e})");
                                            continue;
                                        }
                                    };
                                    line.push('\n');
                                    if writer.write_all(line.as_bytes()).await.is_err()
                                        || writer.flush().await.is_err()
                                    {
                                        log::debug!(
                                            "safehouse: coordination write failed; reconnecting"
                                        );
                                        break true;
                                    }
                                }
                                // A claim ad that would be rejected by safehoused
                                // (bad type/task_id) is a bug, not a transport
                                // failure — log and drop, do not reconnect.
                                Err(e) => log::warn!(
                                    "safehouse: refusing to send invalid claim ad ({e})"
                                ),
                            }
                        }
                        None => return, // all senders dropped → task done
                    }
                }
            }
        };
        if reconnect {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::types::{SweepId, SweepKind};
    use serde_json::json;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use tokio::net::UnixListener;

    // ---- connection-state cell + wire rendering (#4345) ----

    #[test]
    fn new_shared_state_defaults_to_not_configured() {
        let state = new_shared_state();
        assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
    }

    #[test]
    fn set_not_configured_overwrites_any_prior_state() {
        let state = new_shared_state();
        set_state(
            &state,
            SafehouseState::Connected {
                socket: PathBuf::from("/tmp/x.sock"),
                room: None,
            },
        );
        set_not_configured(&state);
        assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
    }

    #[test]
    fn state_to_status_maps_all_three_states() {
        let not_configured = SafehouseState::NotConfigured.to_status();
        assert_eq!(not_configured.state, "not_configured");
        assert!(not_configured.socket.is_none());
        assert!(not_configured.room.is_none());

        let unreachable = SafehouseState::Unreachable {
            socket: PathBuf::from("/tmp/x.sock"),
        }
        .to_status();
        assert_eq!(unreachable.state, "unreachable");
        assert_eq!(unreachable.socket, Some(PathBuf::from("/tmp/x.sock")));
        assert!(unreachable.room.is_none());

        let connected = SafehouseState::Connected {
            socket: PathBuf::from("/tmp/x.sock"),
            room: Some("fleet".to_owned()),
        }
        .to_status();
        assert_eq!(connected.state, "connected");
        assert_eq!(connected.socket, Some(PathBuf::from("/tmp/x.sock")));
        assert_eq!(connected.room.as_deref(), Some("fleet"));

        // A connected state with no configured room (safehoused resolved the
        // sole joined room server-side) still reports "connected" — `room`
        // just stays `None` rather than inventing a name.
        let connected_no_room = SafehouseState::Connected {
            socket: PathBuf::from("/tmp/x.sock"),
            room: None,
        }
        .to_status();
        assert_eq!(connected_no_room.state, "connected");
        assert!(connected_no_room.room.is_none());

        // #4464: the send-rejected state renders a socket + reason, no room,
        // and a distinct wire string so `loom-daemon status` can say
        // "connected, sends rejected: <reason>" rather than "unreachable".
        let send_rejected = SafehouseState::SendRejected {
            socket: PathBuf::from("/tmp/x.sock"),
            reason: "'room' required: 3 rooms joined".to_owned(),
        }
        .to_status();
        assert_eq!(send_rejected.state, "send_rejected");
        assert_eq!(send_rejected.socket, Some(PathBuf::from("/tmp/x.sock")));
        assert!(send_rejected.room.is_none());
        assert_eq!(send_rejected.reason.as_deref(), Some("'room' required: 3 rooms joined"));
    }

    // ---- config resolution (config > default, no env) ----

    #[test]
    fn config_absent_block_is_disabled_default() {
        let cfg = config_from_value(None);
        assert_eq!(cfg, SafehouseConfig::default());
        assert!(!cfg.enabled);
        assert_eq!(cfg.persona, "loom_daemon");
        assert!(cfg.socket.is_none());
    }

    #[test]
    fn config_reads_block_over_default() {
        let block = json!({
            "enabled": true,
            "socket": "/tmp/x.sock",
            "room": "fleet",
            "persona": "loom_daemon"
        });
        let cfg = config_from_value(Some(&block));
        assert!(cfg.enabled);
        assert_eq!(cfg.socket, Some(PathBuf::from("/tmp/x.sock")));
        assert_eq!(cfg.room.as_deref(), Some("fleet"));
        assert_eq!(cfg.persona, "loom_daemon");
    }

    #[test]
    fn config_malformed_block_is_disabled_not_panic() {
        // A non-object (e.g. a stray string) must resolve to disabled default.
        let cfg = config_from_value(Some(&json!("not-an-object")));
        assert!(!cfg.enabled);
        assert_eq!(cfg, SafehouseConfig::default());
    }

    #[test]
    fn config_empty_string_fields_fall_back_to_default() {
        let block = json!({"enabled": true, "socket": "", "room": "  ", "persona": ""});
        let cfg = config_from_value(Some(&block));
        assert!(cfg.enabled);
        assert!(cfg.socket.is_none());
        assert!(cfg.room.is_none());
        assert_eq!(cfg.persona, "loom_daemon");
    }

    // ---- config resolution (env > config) ----

    #[test]
    #[serial]
    fn env_overrides_config_for_all_keys() {
        std::env::set_var(ENABLED_ENV, "true");
        std::env::set_var(SOCKET_ENV, "/env/sock");
        std::env::set_var(ROOM_ENV, "env-room");
        std::env::set_var(PERSONA_ENV, "loom_env");

        let base = config_from_value(Some(&json!({
            "enabled": false, "socket": "/cfg/sock", "room": "cfg-room", "persona": "loom_cfg"
        })));
        let cfg = apply_env_overrides(base);

        assert!(cfg.enabled);
        assert_eq!(cfg.socket, Some(PathBuf::from("/env/sock")));
        assert_eq!(cfg.room.as_deref(), Some("env-room"));
        assert_eq!(cfg.persona, "loom_env");

        std::env::remove_var(ENABLED_ENV);
        std::env::remove_var(SOCKET_ENV);
        std::env::remove_var(ROOM_ENV);
        std::env::remove_var(PERSONA_ENV);
    }

    #[test]
    #[serial]
    fn env_enabled_false_overrides_config_true() {
        std::env::set_var(ENABLED_ENV, "0");
        let cfg = apply_env_overrides(config_from_value(Some(&json!({"enabled": true}))));
        assert!(!cfg.enabled);
        std::env::remove_var(ENABLED_ENV);
    }

    #[test]
    #[serial]
    fn socket_falls_back_to_safehoused_socket_env() {
        std::env::remove_var(SOCKET_ENV);
        std::env::set_var(SAFEHOUSED_SOCKET_ENV, "/run/safehoused.sock");
        let cfg = SafehouseConfig {
            enabled: true,
            ..SafehouseConfig::default()
        };
        assert_eq!(resolve_socket(&cfg), Some(PathBuf::from("/run/safehoused.sock")));
        std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
    }

    // ---- attention-class room routing: kind → room table (#4225) ----

    /// The routing `match` is exhaustive over [`EnvelopeKind`] with no wildcard
    /// arm, so a sixth *enum* member fails to compile. This test is the other
    /// half of that guard: it pins the enum to the wire-level [`KNOWN_TYPES`], so
    /// a sixth member added to only one of the two representations fails here
    /// instead of silently escaping the routing table.
    #[test]
    fn known_types_and_envelope_kind_stay_in_lockstep() {
        assert_eq!(
            KNOWN_TYPES.len(),
            EnvelopeKind::ALL.len(),
            "a new envelope type must be added to BOTH KNOWN_TYPES and EnvelopeKind \
             (the latter is what makes the #4225 routing match exhaustive)"
        );
        for (wire, kind) in KNOWN_TYPES.iter().zip(EnvelopeKind::ALL) {
            assert_eq!(*wire, kind.as_str(), "KNOWN_TYPES and EnvelopeKind::ALL must agree");
            assert_eq!(EnvelopeKind::parse(wire), Some(kind));
        }
        assert_eq!(EnvelopeKind::parse("smoke_signal"), None);
    }

    /// The final routing table (#4225): `handoff`/`ack`/`completion` → signal,
    /// `task`/`chat` → the repo firehose. Every known type is covered and each
    /// resolves to exactly one class.
    #[test]
    fn attention_class_routes_every_known_type_to_exactly_one_tier() {
        let expected = [
            ("chat", AttentionClass::Firehose),
            ("task", AttentionClass::Firehose),
            ("handoff", AttentionClass::Signal),
            ("ack", AttentionClass::Signal),
            ("completion", AttentionClass::Signal),
        ];
        assert_eq!(
            expected.len(),
            KNOWN_TYPES.len(),
            "every KNOWN_TYPES member needs a routing expectation here"
        );
        for (kind, class) in expected {
            let parsed = EnvelopeKind::parse(kind).expect("known type must parse");
            assert_eq!(
                parsed.attention_class(),
                class,
                "{kind:?} must route to {class:?} and nowhere else"
            );
        }
    }

    /// `completion` (#4553, the newest `KNOWN_TYPES` member) is a terminal
    /// outcome and belongs in the operator's signal room — called out explicitly
    /// because it is the easiest one to miss.
    #[test]
    fn completion_routes_to_the_signal_room() {
        assert_eq!(EnvelopeKind::Completion.attention_class(), AttentionClass::Signal);

        let cfg = routing_config();
        let router = RoomRouter::new(&cfg);
        assert_eq!(
            router.resolve("completion", Some("/home/x/GitHub/loom")),
            RoomDecision::Send(Some("!signal:example.org".to_owned())),
            "a completion must reach the signal room even when the repo has its own firehose"
        );
    }

    // ---- attention-class room routing: RoomRouter (#4225) ----

    /// A `rooms` map with a signal room and one pre-configured repo firehose.
    fn routing_config() -> SafehouseConfig {
        SafehouseConfig {
            enabled: true,
            room: None,
            rooms: Some(RoomMap {
                signal: Some("!signal:example.org".to_owned()),
                by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                    .into_iter()
                    .collect(),
            }),
            ..SafehouseConfig::default()
        }
    }

    /// **The most important regression guard of #4225**: with no `rooms` map,
    /// every envelope of every kind resolves to the single configured `room`,
    /// exactly as before — including the `None` "let safehoused resolve its sole
    /// room" form, which must still serialize with no `room` key at all.
    #[test]
    fn absent_rooms_map_is_byte_identical_single_room_behavior() {
        for room in [Some("loom-fleet".to_owned()), None] {
            let cfg = SafehouseConfig {
                enabled: true,
                room: room.clone(),
                ..SafehouseConfig::default()
            };
            assert!(!cfg.routes_by_attention());
            let router = RoomRouter::new(&cfg);
            for kind in KNOWN_TYPES {
                for repo in [Some("/home/x/GitHub/vibesql"), None] {
                    assert_eq!(
                        router.resolve(kind, repo),
                        RoomDecision::Send(room.clone()),
                        "with no rooms map, {kind:?} (repo={repo:?}) must go to the single room"
                    );
                }
            }
        }

        // And the wire shape of that `None` case: no `room` key is emitted.
        let env = Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some("loom_4225".to_owned()),
            body: "loom#4225 · dispatch".to_owned(),
            meta: None,
        };
        let req = build_send_request(&env, 1, None).unwrap();
        assert!(req.get("room").is_none(), "single-room mode with room=null sends no room key");
    }

    #[test]
    fn signal_room_falls_back_to_the_legacy_room_key() {
        // Migration shape: an operator adds `rooms.byRepo` but leaves the signal
        // room as the existing scalar `safehouse.room`.
        let cfg = SafehouseConfig {
            enabled: true,
            room: Some("!legacy:example.org".to_owned()),
            rooms: Some(RoomMap {
                signal: None,
                by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                    .into_iter()
                    .collect(),
            }),
            ..SafehouseConfig::default()
        };
        assert_eq!(cfg.signal_room(), Some("!legacy:example.org"));
        let router = RoomRouter::new(&cfg);
        assert_eq!(
            router.resolve("handoff", Some("/home/x/GitHub/loom")),
            RoomDecision::Send(Some("!legacy:example.org".to_owned()))
        );
        // …and `rooms.signal`, when set, wins over the legacy scalar.
        assert_eq!(routing_config().signal_room(), Some("!signal:example.org"));
    }

    #[test]
    fn firehose_kinds_route_to_the_configured_repo_room() {
        let cfg = routing_config();
        let router = RoomRouter::new(&cfg);
        for kind in ["task", "chat"] {
            assert_eq!(
                router.resolve(kind, Some("/home/x/GitHub/loom")),
                RoomDecision::Send(Some("!fleet-loom:example.org".to_owned())),
                "{kind:?} is repo chatter and belongs in that repo's firehose"
            );
        }
        for kind in ["handoff", "ack", "completion"] {
            assert_eq!(
                router.resolve(kind, Some("/home/x/GitHub/loom")),
                RoomDecision::Send(Some("!signal:example.org".to_owned())),
                "{kind:?} is a human-attention outcome and belongs in the signal room"
            );
        }
    }

    #[test]
    fn unconfigured_repo_firehose_is_created_lazily_then_reused() {
        let cfg = routing_config();
        let mut router = RoomRouter::new(&cfg);
        // vibesql has no configured firehose ⇒ create `fleet-vibesql` on first
        // narration (lazily — never eagerly for every managed repo).
        assert_eq!(
            router.resolve("task", Some("/home/x/GitHub/vibesql")),
            RoomDecision::Create {
                repo: "vibesql".to_owned(),
                alias: "fleet-vibesql".to_owned(),
                fallback: Some("!signal:example.org".to_owned()),
            }
        );
        router.record_created("vibesql", "!created:example.org".to_owned());
        // Every later message reuses the recorded id — no second create_room op.
        assert_eq!(
            router.resolve("task", Some("/home/x/GitHub/vibesql")),
            RoomDecision::Send(Some("!created:example.org".to_owned()))
        );
        // Signal-class traffic for the same repo still goes to the signal room.
        assert_eq!(
            router.resolve("handoff", Some("/home/x/GitHub/vibesql")),
            RoomDecision::Send(Some("!signal:example.org".to_owned()))
        );
    }

    #[test]
    fn uncreatable_repo_room_degrades_to_signal_and_warns_once() {
        let cfg = routing_config();
        let mut router = RoomRouter::new(&cfg);
        assert!(matches!(
            router.resolve("task", Some("/home/x/GitHub/anvil")),
            RoomDecision::Create { .. }
        ));
        // First failure ⇒ warn (record_degraded returns true exactly once).
        assert!(router.record_degraded("anvil"), "the first failure must warn");
        assert!(!router.record_degraded("anvil"), "later failures must NOT warn again");
        // …and from then on this repo narrates into the signal room, with no
        // further creation attempts (never a blocked or failed sweep).
        for kind in KNOWN_TYPES {
            assert_eq!(
                router.resolve(kind, Some("/home/x/GitHub/anvil")),
                RoomDecision::Send(Some("!signal:example.org".to_owned())),
                "a degraded repo must keep narrating ({kind:?}), just into the signal room"
            );
        }
    }

    #[test]
    fn firehose_without_a_repo_degrades_to_signal_rather_than_inventing_a_room() {
        let cfg = routing_config();
        let router = RoomRouter::new(&cfg);
        // No repo stamped (a synthetic/test event, or daemon-wide news): there is
        // no per-repo firehose to route to, so it lands in the signal room.
        assert_eq!(
            router.resolve("task", None),
            RoomDecision::Send(Some("!signal:example.org".to_owned()))
        );
        // A routing mode with no signal room configured at all resolves to `None`
        // — the documented "explicit ids required once the map exists" caveat,
        // where safehoused answers `'room' required` and #4464's send-rejected
        // status names the fix. It never panics and never drops the message.
        let cfg = SafehouseConfig {
            enabled: true,
            room: None,
            rooms: Some(RoomMap {
                signal: None,
                by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                    .into_iter()
                    .collect(),
            }),
            ..SafehouseConfig::default()
        };
        assert_eq!(RoomRouter::new(&cfg).resolve("handoff", None), RoomDecision::Send(None));
    }

    #[test]
    fn repo_room_alias_uses_the_narration_basename_convention() {
        assert_eq!(repo_room_alias("vibesql"), "fleet-vibesql");
        // The basename convention (#4201) is what keys the map, so a full
        // workspace path resolves to the same room as its basename.
        let cfg = routing_config();
        let router = RoomRouter::new(&cfg);
        assert_eq!(
            router.resolve("task", Some("/Users/someone/GitHub/loom")),
            router.resolve("task", Some("loom"))
        );
    }

    #[test]
    fn event_repo_reads_the_stamped_workspace_root() {
        assert_eq!(
            event_repo(&Event::SweepPhase {
                issue: 4225,
                phase: "builder".to_owned(),
                pr_number: None,
                repo: Some("/home/x/GitHub/loom".to_owned()),
            }),
            Some("/home/x/GitHub/loom")
        );
        assert_eq!(
            event_repo(&Event::SweepGlobalCompleted {
                sweep_id: "sweep-issue-4225-1".to_owned() as SweepId,
                outcome: crate::types::SweepOutcome::Exited,
            }),
            None
        );
    }

    // ---- attention-class room routing: config + env (#4225) ----

    #[test]
    fn config_parses_the_rooms_map() {
        let block = json!({
            "enabled": true,
            "rooms": {
                "signal": "!signal:example.org",
                "byRepo": {"loom": "!fleet-loom:example.org", "vibesql": "!fleet-vibesql:example.org"}
            }
        });
        let cfg = config_from_value(Some(&block));
        let rooms = cfg.rooms.expect("the rooms map must parse");
        assert_eq!(rooms.signal.as_deref(), Some("!signal:example.org"));
        assert_eq!(rooms.by_repo.get("loom").map(String::as_str), Some("!fleet-loom:example.org"));
        assert_eq!(
            rooms.by_repo.get("vibesql").map(String::as_str),
            Some("!fleet-vibesql:example.org")
        );
    }

    #[test]
    fn config_without_a_rooms_map_stays_in_single_room_mode() {
        // The migration default: absent, malformed, and present-but-empty all
        // resolve to `None` ⇒ unchanged single-room behavior.
        for block in [
            json!({"enabled": true, "room": "loom-fleet"}),
            json!({"enabled": true, "rooms": {}}),
            json!({"enabled": true, "rooms": "loom-fleet"}),
            json!({"enabled": true, "rooms": {"signal": "  ", "byRepo": {}}}),
            json!({"enabled": true, "rooms": {"byRepo": {"loom": ""}}}),
            json!({"enabled": true, "rooms": {"byRepo": ["loom"]}}),
        ] {
            let cfg = config_from_value(Some(&block));
            assert!(
                cfg.rooms.is_none(),
                "{block} must resolve to single-room mode, got {:?}",
                cfg.rooms
            );
            assert!(!cfg.routes_by_attention());
        }
    }

    #[test]
    #[serial]
    fn env_overrides_config_for_the_rooms_map() {
        std::env::set_var(ROOM_SIGNAL_ENV, "!env-signal:example.org");
        std::env::set_var(ROOMS_BY_REPO_ENV, "loom=!env-loom:example.org, anvil=!env-anvil:x");

        let cfg = apply_env_overrides(config_from_value(Some(&json!({
            "enabled": true,
            "rooms": {
                "signal": "!cfg-signal:example.org",
                "byRepo": {"loom": "!cfg-loom:example.org", "vibesql": "!cfg-vibesql:example.org"}
            }
        }))));
        let rooms = cfg.rooms.expect("env must keep the map present");
        assert_eq!(rooms.signal.as_deref(), Some("!env-signal:example.org"));
        assert_eq!(rooms.by_repo.get("loom").map(String::as_str), Some("!env-loom:example.org"));
        assert_eq!(rooms.by_repo.get("anvil").map(String::as_str), Some("!env-anvil:x"));
        assert!(
            !rooms.by_repo.contains_key("vibesql"),
            "the byRepo env override replaces the whole map rather than merging into it"
        );

        std::env::remove_var(ROOM_SIGNAL_ENV);
        std::env::remove_var(ROOMS_BY_REPO_ENV);
    }

    #[test]
    #[serial]
    fn env_alone_can_enable_routing_and_absent_env_changes_nothing() {
        // Env with no config `rooms` block at all ⇒ routing enabled from env.
        std::env::set_var(ROOM_SIGNAL_ENV, "!env-signal:example.org");
        let cfg = apply_env_overrides(config_from_value(Some(&json!({"enabled": true}))));
        assert_eq!(cfg.signal_room(), Some("!env-signal:example.org"));
        std::env::remove_var(ROOM_SIGNAL_ENV);

        // Neither env var set ⇒ the config layer's map is returned untouched, so
        // the absent-map single-room default stays byte-identical. (`room` itself
        // is deliberately not asserted here: `LOOM_SAFEHOUSE_ROOM` may be set in
        // the ambient environment, and its precedence is already covered by
        // `env_overrides_config_for_all_keys`.)
        std::env::remove_var(ROOMS_BY_REPO_ENV);
        let cfg = apply_env_overrides(config_from_value(Some(
            &json!({"enabled": true, "room": "loom-fleet"}),
        )));
        assert!(cfg.rooms.is_none());
        assert_eq!(
            config_from_value(Some(&json!({"enabled": true, "room": "loom-fleet"}))).signal_room(),
            Some("loom-fleet"),
            "with no rooms map the signal room IS the legacy scalar room"
        );

        // A garbage byRepo env value degrades to the pairs it can parse (here:
        // none) instead of panicking.
        assert!(parse_by_repo_env("loom,,=,=x,vibesql=").is_empty());
        assert_eq!(
            parse_by_repo_env("loom=!a:x,,vibesql = !b:x ")
                .get("vibesql")
                .map(String::as_str),
            Some("!b:x")
        );
    }

    // ---- envelope serialization / validation ----

    #[test]
    fn send_request_emits_v1_and_never_from() {
        let env = Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some("4137".to_owned()),
            body: "hi".to_owned(),
            meta: None,
        };
        let req = build_send_request(&env, 7, None).unwrap();
        assert_eq!(req["v"], json!(1));
        assert_eq!(req["id"], json!(7));
        assert_eq!(req["op"], json!("send"));
        assert_eq!(req["to"], json!("*"));
        assert_eq!(req["type"], json!("task"));
        assert_eq!(req["task_id"], json!("4137"));
        assert!(req.get("from").is_none(), "from must never be serialized");
    }

    #[test]
    fn send_request_omits_task_id_when_absent() {
        let env = Envelope {
            to: "*".to_owned(),
            kind: "ack".to_owned(),
            task_id: None,
            body: "done".to_owned(),
            meta: None,
        };
        let req = build_send_request(&env, 1, None).unwrap();
        assert!(req.get("task_id").is_none());
    }

    #[test]
    fn send_request_includes_room_when_present() {
        let env = Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: None,
            body: "x".to_owned(),
            meta: None,
        };
        let req = build_send_request(&env, 1, Some("fleet")).unwrap();
        assert_eq!(req["room"], json!("fleet"));
    }

    #[test]
    fn send_request_rejects_unknown_type() {
        let env = Envelope {
            to: "*".to_owned(),
            kind: "smoke_signal".to_owned(),
            task_id: None,
            body: "x".to_owned(),
            meta: None,
        };
        assert!(build_send_request(&env, 1, None).is_err());
    }

    #[test]
    fn send_request_rejects_hyphenated_task_id() {
        let env = Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some("issue-4137".to_owned()),
            body: "x".to_owned(),
            meta: None,
        };
        assert!(build_send_request(&env, 1, None).is_err());
    }

    #[test]
    fn to_normalization_folds_hyphens_and_rejects_garbage() {
        // Hyphenated persona is normalized to underscore (not sent into the void).
        let env = Envelope {
            to: "loom-builder".to_owned(),
            kind: "task".to_owned(),
            task_id: None,
            body: "x".to_owned(),
            meta: None,
        };
        let req = build_send_request(&env, 1, None).unwrap();
        assert_eq!(req["to"], json!("loom_builder"));

        // "*" and @matrix ids pass through untouched.
        assert_eq!(normalize_to("*").unwrap(), "*");
        assert_eq!(normalize_to("@a:b.c").unwrap(), "@a:b.c");

        // A value that cannot be a persona is rejected, not silently sent.
        assert!(normalize_to("has space").is_err());
    }

    // ---- completion envelopes + completion-v1 meta (#4426) ----

    /// A valid `completion-v1` source, tweaked per-test.
    fn sample_completion_meta() -> CompletionMeta {
        CompletionMeta {
            agent: "loom_daemon".to_owned(),
            repo_slug: "rjwalters/loom".to_owned(),
            pr_url: "https://github.com/rjwalters/loom/pull/4321".to_owned(),
            result: CompletionResult::Success,
            started_at: "2026-07-29T10:00:00Z".to_owned(),
            completed_at: "2026-07-29T10:12:30Z".to_owned(),
            issue: Some(4321),
            tokens: Some(791_000),
            title: Some("Add repo-qualified task_id".to_owned()),
            additions: Some(214),
            deletions: Some(37),
        }
    }

    #[test]
    fn completion_is_a_known_type() {
        assert!(KNOWN_TYPES.contains(&"completion"));
    }

    #[test]
    fn send_request_accepts_completion_with_valid_meta() {
        let meta = sample_completion_meta().to_meta_value().unwrap();
        let env = Envelope {
            to: "*".to_owned(),
            kind: "completion".to_owned(),
            task_id: Some("loom_4321".to_owned()),
            body: "loom#4321 · merged ✓ · PR #4321 · 12m30s".to_owned(),
            meta: Some(meta),
        };
        let req = build_send_request(&env, 3, Some("fleet")).unwrap();

        assert_eq!(req["type"], json!("completion"));
        assert_eq!(req["v"], json!(1));
        assert_eq!(req["room"], json!("fleet"));
        assert!(req.get("from").is_none(), "from must never be serialized");
        // The whole completion-v1 payload rides in `meta`, and `body` stays
        // human prose (a room reader sees a sentence, not JSON).
        assert_eq!(req["meta"]["schema"], json!("completion-v1"));
        assert_eq!(req["meta"]["agent"], json!("loom_daemon"));
        assert_eq!(req["meta"]["repo"], json!("rjwalters/loom"));
        assert_eq!(req["meta"]["ref"], json!("https://github.com/rjwalters/loom/pull/4321"));
        assert_eq!(req["meta"]["result"], json!("success"));
        assert_eq!(req["meta"]["started_at"], json!("2026-07-29T10:00:00Z"));
        assert_eq!(req["meta"]["completed_at"], json!("2026-07-29T10:12:30Z"));
        assert_eq!(req["meta"]["issue"], json!(4321));
        assert_eq!(req["meta"]["tokens"], json!(791_000));
        // Feed display fields (#4497) ride the same `meta`, so the egress
        // publishes them with no schema revision.
        assert_eq!(req["meta"]["title"], json!("Add repo-qualified task_id"));
        assert_eq!(req["meta"]["additions"], json!(214));
        assert_eq!(req["meta"]["deletions"], json!(37));
        assert!(req["body"].as_str().unwrap().contains("merged ✓"));
    }

    #[test]
    fn send_request_refuses_completion_without_meta() {
        // safehoused would degrade this to a `chat` and it would vanish from
        // the public feed with no error — so it must never leave this client.
        let env = Envelope {
            to: "*".to_owned(),
            kind: "completion".to_owned(),
            task_id: None,
            body: "merged".to_owned(),
            meta: None,
        };
        assert!(build_send_request(&env, 1, None).is_err());
    }

    #[test]
    fn send_request_refuses_meta_on_non_completion_types() {
        for kind in ["chat", "task", "handoff", "ack"] {
            let env = Envelope {
                to: "*".to_owned(),
                kind: kind.to_owned(),
                task_id: None,
                body: "x".to_owned(),
                meta: Some(sample_completion_meta().to_meta_value().unwrap()),
            };
            assert!(
                build_send_request(&env, 1, None).is_err(),
                "`meta` must be rejected on a {kind:?} envelope, not silently dropped"
            );
        }
    }

    #[test]
    fn send_request_refuses_every_flavor_of_malformed_completion_meta() {
        let valid = sample_completion_meta().to_meta_value().unwrap();
        let mut cases: Vec<(&str, Value)> = vec![
            ("not an object", json!("completion-v1")),
            ("wrong schema", {
                let mut m = valid.clone();
                m["schema"] = json!("completion-v2");
                m
            }),
            ("empty agent", {
                let mut m = valid.clone();
                m["agent"] = json!("");
                m
            }),
            ("invalid persona charset", {
                let mut m = valid.clone();
                m["agent"] = json!("Loom-Daemon");
                m
            }),
            ("repo is a path basename, not a forge slug", {
                let mut m = valid.clone();
                m["repo"] = json!("loom");
                m
            }),
            ("ref is not an absolute URL", {
                let mut m = valid.clone();
                m["ref"] = json!("rjwalters/loom#4321");
                m
            }),
            ("unknown result", {
                let mut m = valid.clone();
                m["result"] = json!("merged");
                m
            }),
            ("started_at is not RFC3339", {
                let mut m = valid.clone();
                m["started_at"] = json!("29 July 2026");
                m
            }),
            ("completed_at precedes started_at", {
                let mut m = valid.clone();
                m["completed_at"] = json!("2026-07-29T09:00:00Z");
                m
            }),
            ("tokens is a string", {
                let mut m = valid.clone();
                m["tokens"] = json!("791000");
                m
            }),
            // #4497 display fields are validated to the same standard as the
            // pre-existing extensions.
            ("additions is a string", {
                let mut m = valid.clone();
                m["additions"] = json!("214");
                m
            }),
            ("deletions is negative", {
                let mut m = valid.clone();
                m["deletions"] = json!(-1);
                m
            }),
            ("title is blank", {
                let mut m = valid.clone();
                m["title"] = json!("   ");
                m
            }),
            ("title is not a string", {
                let mut m = valid.clone();
                m["title"] = json!(4497);
                m
            }),
        ];
        // Every required key, dropped one at a time.
        for key in COMPLETION_REQUIRED_KEYS {
            let mut m = valid.clone();
            m.as_object_mut().unwrap().remove(key);
            cases.push((key, m));
        }

        for (label, meta) in cases {
            assert!(
                validate_completion_meta(&meta).is_err(),
                "validate_completion_meta must reject: {label}"
            );
            let env = Envelope {
                to: "*".to_owned(),
                kind: "completion".to_owned(),
                task_id: None,
                body: "merged".to_owned(),
                meta: Some(meta),
            };
            assert!(
                build_send_request(&env, 1, None).is_err(),
                "a completion with malformed meta must not be sent: {label}"
            );
        }
    }

    #[test]
    fn completion_meta_omits_absent_optional_fields() {
        let meta = CompletionMeta {
            issue: None,
            tokens: None,
            title: None,
            additions: None,
            deletions: None,
            ..sample_completion_meta()
        }
        .to_meta_value()
        .unwrap();
        assert!(meta.get("issue").is_none(), "absent issue must be omitted, not null/0");
        assert!(meta.get("tokens").is_none(), "absent tokens must be omitted, not null/0");
        assert!(meta.get("title").is_none(), "absent title must be omitted, not null/empty");
        assert!(meta.get("additions").is_none(), "absent additions must be omitted, not 0");
        assert!(meta.get("deletions").is_none(), "absent deletions must be omitted, not 0");
        // With every extension absent, the envelope is exactly the required
        // completion-v1 object — no new keys, so no new failure modes (#4497).
        assert_eq!(
            meta.as_object().unwrap().len(),
            COMPLETION_REQUIRED_KEYS.len(),
            "all-absent extensions ⇒ meta identical to the required-keys-only envelope; got {meta}"
        );
        // A zero token count is indistinguishable from "no accounting data".
        let zeroed = CompletionMeta {
            tokens: Some(0),
            ..sample_completion_meta()
        }
        .to_meta_value()
        .unwrap();
        assert!(zeroed.get("tokens").is_none());
        // A blank/whitespace title would render as an empty feed row label.
        for blank in ["", "   ", "\n\t"] {
            let meta = CompletionMeta {
                title: Some(blank.to_owned()),
                ..sample_completion_meta()
            }
            .to_meta_value()
            .unwrap();
            assert!(meta.get("title").is_none(), "blank title must be omitted: {blank:?}");
        }
    }

    #[test]
    fn completion_meta_publishes_zero_diff_counts_and_trims_the_title() {
        // Unlike `tokens`, `0` additions/deletions is a *fact* about the merge
        // (a pure revert, an empty-diff merge commit), not a "no data" sentinel
        // — so it is published rather than filtered (#4497).
        let meta = CompletionMeta {
            additions: Some(0),
            deletions: Some(0),
            title: Some("  fix: trim me  ".to_owned()),
            ..sample_completion_meta()
        }
        .to_meta_value()
        .unwrap();
        assert_eq!(meta["additions"], json!(0));
        assert_eq!(meta["deletions"], json!(0));
        assert_eq!(meta["title"], json!("fix: trim me"));
    }

    #[test]
    fn completion_meta_carries_the_title_verbatim_for_downstream_redaction() {
        // Deny-pattern redaction is safehoused's egress job (it redacts every
        // string in the published payload), not loom's — so the contract loom
        // owns is that `title` reaches the wire as an ordinary JSON string in
        // `meta`, exactly like `repo`/`ref`, with no escaping or bespoke
        // encoding that would let it bypass that pass (#4497 AC3).
        let secretish = "fix: rotate ghp_EXAMPLETOKEN0123456789 in \"prod\"\\config";
        let env = build_completion_envelope(
            Some("/x/loom"),
            4497,
            4500,
            60,
            &CompletionMeta {
                title: Some(secretish.to_owned()),
                ..sample_completion_meta()
            },
        )
        .unwrap();
        let req = build_send_request(&env, 1, Some("fleet")).unwrap();
        assert_eq!(
            req["meta"]["title"],
            json!(secretish),
            "title must be a plain JSON string in meta, like every other redactable field"
        );
        // And it survives a JSON round-trip through the wire encoding intact —
        // the shape safehoused's redactor walks.
        let line = serde_json::to_string(&req).unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["meta"]["title"], json!(secretish));
        assert!(parsed["meta"]["title"].is_string());
    }

    #[test]
    fn completion_meta_construction_fails_on_bad_fields() {
        // The typed constructor is the only route to a `completion`, so it
        // must refuse the same things validate_completion_meta does.
        assert!(CompletionMeta {
            repo_slug: "not a slug".to_owned(),
            ..sample_completion_meta()
        }
        .to_meta_value()
        .is_err());
        assert!(CompletionMeta {
            started_at: "yesterday".to_owned(),
            ..sample_completion_meta()
        }
        .to_meta_value()
        .is_err());
    }

    #[test]
    fn build_completion_envelope_threads_with_the_issue_and_reads_as_prose() {
        let env = build_completion_envelope(
            Some("/Users/x/GitHub/loom"),
            4321,
            4400,
            750,
            &sample_completion_meta(),
        )
        .unwrap();

        assert_eq!(env.kind, "completion");
        assert_eq!(env.to, "*");
        // Same repo-qualified thread key as this issue's other narration lines
        // (#4201), so the completion lands in the existing Matrix thread.
        assert_eq!(env.task_id.as_deref(), Some("loom_4321"));
        assert_eq!(env.body, "loom#4321 · merged ✓ · PR #4400 · 12m30s");
        assert_eq!(env.meta.as_ref().unwrap()["schema"], json!("completion-v1"));
        // And it survives the pre-send gate.
        assert!(build_send_request(&env, 1, None).is_ok());
    }

    #[test]
    fn valid_repo_slug_accepts_owner_repo_and_rejects_the_rest() {
        assert!(valid_repo_slug("rjwalters/loom"));
        assert!(valid_repo_slug("2AMLogic/marketing"));
        assert!(valid_repo_slug("owner/kicad-tools.git"));
        assert!(!valid_repo_slug("loom"), "a bare basename is not a forge slug");
        assert!(!valid_repo_slug("a/b/c"));
        assert!(!valid_repo_slug("/loom"));
        assert!(!valid_repo_slug("rjwalters/"));
        assert!(!valid_repo_slug("rjwalters/lo om"));
    }

    // ---- repo qualification helpers (issue #4201) ----

    #[test]
    fn repo_basename_extracts_final_path_segment() {
        assert_eq!(repo_basename(Some("/Users/x/GitHub/vibesql")).as_deref(), Some("vibesql"));
        assert_eq!(repo_basename(Some("/repos/kicad-tools")).as_deref(), Some("kicad-tools"));
        assert_eq!(repo_basename(None), None);
        assert_eq!(repo_basename(Some("")), None);
    }

    #[test]
    fn qualify_task_id_sanitizes_and_qualifies() {
        assert_eq!(qualify_task_id(Some("/repos/vibesql"), 6173), "vibesql_6173");
        // Non-alphanumeric basename characters (hyphen) fold to `_` so the
        // result stays in the task_id charset validated by build_send_request.
        assert_eq!(qualify_task_id(Some("/repos/kicad-tools"), 9), "kicad_tools_9");
        // No repo known ⇒ bare issue number (pre-#4201 behavior preserved).
        assert_eq!(qualify_task_id(None, 42), "42");
    }

    #[test]
    fn cross_repo_same_issue_number_gets_distinct_task_ids() {
        // The bug this issue fixes: loom #4201 and vibesql #4201 must not
        // collide into the same Matrix thread.
        let loom_id = qualify_task_id(Some("/Users/x/GitHub/loom"), 4201);
        let vibesql_id = qualify_task_id(Some("/Users/x/GitHub/vibesql"), 4201);
        assert_ne!(loom_id, vibesql_id);
        assert_eq!(loom_id, "loom_4201");
        assert_eq!(vibesql_id, "vibesql_4201");
    }

    #[test]
    fn repo_issue_prefix_falls_back_without_repo() {
        assert_eq!(repo_issue_prefix(Some("/repos/vibesql"), 6173), "vibesql#6173");
        assert_eq!(repo_issue_prefix(None, 42), "#42");
    }

    #[test]
    fn format_narrated_duration_drops_zero_minutes() {
        assert_eq!(format_narrated_duration(415), "6m55s");
        assert_eq!(format_narrated_duration(24), "24s");
        assert_eq!(format_narrated_duration(0), "0s");
        assert_eq!(format_narrated_duration(60), "1m0s");
    }

    #[test]
    fn decode_exit_code_annotates_only_well_known_codes() {
        assert_eq!(decode_exit_code_annotation(78), " (EX_CONFIG: token pool)");
        assert_eq!(decode_exit_code_annotation(1), "");
        assert_eq!(decode_exit_code_annotation(0), "");
    }

    // ---- event → envelope mapping ----

    #[test]
    fn maps_the_narrated_events() {
        let dispatch = Event::SweepGlobalDispatch {
            sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
            kind: SweepKind::Issue(42),
            runtime: None,
            runtime_source: None,
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&dispatch).unwrap();
        assert_eq!(env.kind, "task");
        assert_eq!(env.task_id.as_deref(), Some("vibesql_42"));
        assert_eq!(env.body, "vibesql#42 · dispatch");

        let phase = Event::SweepPhase {
            issue: 42,
            phase: "builder".to_owned(),
            pr_number: Some(99),
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&phase).unwrap();
        assert_eq!(env.kind, "task");
        assert_eq!(env.task_id.as_deref(), Some("vibesql_42"));
        assert!(env.body.starts_with("vibesql#42 · builder"));
        assert!(env.body.contains("PR #99 open"));

        let blocker = Event::SweepBlocker {
            issue: 42,
            reason: "missing dep".to_owned(),
            label_added: "loom:blocked".to_owned(),
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&blocker).unwrap();
        assert_eq!(env.kind, "handoff");
        assert_eq!(env.body, "vibesql#42 · BLOCKED — missing dep");

        let exited = Event::SweepExited {
            issue: 42,
            exit_code: Some(0),
            duration_sec: 12,
            no_progress: false,
            death_class: None,
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&exited).unwrap();
        assert_eq!(env.kind, "ack");
        assert_eq!(env.body, "vibesql#42 · done ✓ · 12s");

        // A non-zero exit decodes its well-known meaning (78 = EX_CONFIG).
        let exited_failed = Event::SweepExited {
            issue: 42,
            exit_code: Some(78),
            duration_sec: 24,
            no_progress: false,
            death_class: None,
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&exited_failed).unwrap();
        assert_eq!(env.body, "vibesql#42 · failed ✗ · exit 78 (EX_CONFIG: token pool) · 24s");

        // #4366: a clean exit 0 classified as no-progress (parked on a
        // monitored background task) narrates distinctly from an ordinary
        // benign self-skip.
        let exited_no_progress = Event::SweepExited {
            issue: 42,
            exit_code: Some(0),
            duration_sec: 90,
            no_progress: true,
            death_class: None,
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&exited_no_progress).unwrap();
        assert_eq!(env.kind, "ack");
        assert_eq!(env.body, "vibesql#42 · no progress ⚠ · exit 0, no checkpoint/PR · 1m30s");

        let crashed = Event::SweepCrashed {
            issue: 42,
            checkpoint_phase: Some("judge".to_owned()),
            classification: None,
            death_class: None,
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&crashed).unwrap();
        assert_eq!(env.kind, "handoff");
        assert_eq!(env.body, "vibesql#42 · crashed ✗ at judge — resumable (checkpoint kept)");

        // Issue #4256: a successful reaper-driven resume narrates as a
        // `handoff` naming the phase + PR, so an operator watching chat sees
        // recovery happen without having to run `/loom:sweep --prs` by hand.
        let resumed = Event::SweepResumeDispatched {
            issue: 42,
            pr: 4300,
            checkpoint_phase: Some("builder-done".to_owned()),
            dispatched: true,
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&resumed).unwrap();
        assert_eq!(env.kind, "handoff");
        assert_eq!(
            env.body,
            "vibesql#42 · reaper resumed crashed sweep at builder-done (open PR #4300) — \
             resuming without operator intervention"
        );

        // A resume ATTEMPT whose own dispatch call failed still narrates —
        // the recovery attempt must never be silent even on failure.
        let resume_failed = Event::SweepResumeDispatched {
            issue: 42,
            pr: 4300,
            checkpoint_phase: Some("builder-done".to_owned()),
            dispatched: false,
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&resume_failed).unwrap();
        assert!(env.body.contains("still stranded"), "got: {}", env.body);

        let idle_exit = Event::DaemonIdleExit {
            trigger: "token_starvation".to_owned(),
            idle_minutes: 60,
            in_flight_sweeps: 0,
            active_role_runs: 1,
            healthy_tokens: 0,
            total_tokens: 8,
            message: "idle for 60m — exiting for host idle-shutdown".to_owned(),
        };
        let env = event_to_envelope(&idle_exit).unwrap();
        assert_eq!(env.kind, "handoff");
        assert_eq!(env.task_id.as_deref(), Some("daemon-idle-exit"));
        assert!(env.body.contains("host idle-shutdown"));
        assert_eq!(env.meta.unwrap()["trigger"], "token_starvation");
    }

    #[test]
    fn maps_events_without_repo_using_bare_fallback() {
        // No `repo` stamped (a pre-#4201 event, or a registry that never
        // wires the bus) still narrates — just without repo qualification,
        // matching the pre-#4201 behavior for task_id and body prefix.
        let dispatch = Event::SweepGlobalDispatch {
            sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
            kind: SweepKind::Issue(42),
            runtime: None,
            runtime_source: None,
            repo: None,
        };
        let env = event_to_envelope(&dispatch).unwrap();
        assert_eq!(env.task_id.as_deref(), Some("42"));
        assert_eq!(env.body, "#42 · dispatch");
    }

    #[test]
    fn does_not_narrate_global_completed_or_generic() {
        let completed = Event::SweepGlobalCompleted {
            sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
            outcome: crate::types::SweepOutcome::Exited,
        };
        assert!(event_to_envelope(&completed).is_none());
        assert!(event_to_envelope(&Event::TopicLag { skipped: 3 }).is_none());
    }

    #[test]
    fn narrates_child_published_phase_and_blocker_after_typed_upgrade() {
        // Issue #4466: a child-published `sweep.issue.{N}.*` event arrives via
        // `PublishEvent`; before the typed-upgrade fix it became `Event::Generic`
        // and `event_to_envelope` returned `None` (silently dropped). Drive the
        // exact `PublishEvent` construction (`Event::from_published`) end to end
        // and assert the documented room lines now appear.
        let phase = Event::from_published(
            "sweep.issue.42.phase".to_owned(),
            json!({"phase": "builder", "pr_number": 99, "repo": "/repos/vibesql"}),
        );
        let env = event_to_envelope(&phase).expect("phase must narrate (was dropped as Generic)");
        assert_eq!(env.kind, "task");
        assert_eq!(env.task_id.as_deref(), Some("vibesql_42"));
        assert!(env.body.starts_with("vibesql#42 · builder"));
        assert!(env.body.contains("PR #99 open"));

        let blocker = Event::from_published(
            "sweep.issue.42.blocker".to_owned(),
            json!({"reason": "missing dep", "label_added": "loom:blocked", "repo": "/repos/vibesql"}),
        );
        let env =
            event_to_envelope(&blocker).expect("blocker must narrate (was dropped as Generic)");
        assert_eq!(env.kind, "handoff");
        assert_eq!(env.body, "vibesql#42 · BLOCKED — missing dep");

        // A malformed child payload still falls through to Generic and stays
        // un-narrated (publish is fire-and-forget advisory — never rejected).
        let malformed =
            Event::from_published("sweep.issue.42.phase".to_owned(), json!({"pr_number": 1}));
        assert!(matches!(malformed, Event::Generic { .. }));
        assert!(event_to_envelope(&malformed).is_none());
    }

    // ---- dispatch-title fetch (issue #4201, sink-side gh lookup) ----

    /// Write an executable fake `gh` script at `dir/fake-gh.sh` that logs its
    /// argv to `dir/gh-invocations.log` (one line per call) and prints `stdout`
    /// on success, or exits 1 when `stdout` is `None` — mirrors the fake-`gh`
    /// convention already used in `sweep_registry.rs`'s tests.
    fn write_fake_gh(dir: &std::path::Path, stdout: Option<&str>) -> (PathBuf, PathBuf) {
        let log = dir.join("gh-invocations.log");
        let script_path = dir.join("fake-gh.sh");
        let body = match stdout {
            Some(text) => format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nprintf '%s\\n' {}\nexit 0\n",
                log.display(),
                shell_quote(text),
            ),
            None => format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 1\n",
                log.display()
            ),
        };
        std::fs::write(&script_path, body).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        (script_path, log)
    }

    /// Minimal single-quote shell escaping sufficient for test title strings.
    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    #[tokio::test]
    #[serial]
    async fn fetch_issue_title_returns_title_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) = write_fake_gh(dir.path(), Some("Fix the frobnicator"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let title = fetch_issue_title(dir.path(), 42).await;

        std::env::remove_var(GH_BIN_ENV);
        assert_eq!(title.as_deref(), Some("Fix the frobnicator"));
    }

    #[tokio::test]
    #[serial]
    async fn fetch_issue_title_degrades_to_none_on_gh_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) = write_fake_gh(dir.path(), None);
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let title = fetch_issue_title(dir.path(), 42).await;

        std::env::remove_var(GH_BIN_ENV);
        assert!(title.is_none(), "a failing gh call must degrade to None, not panic/hang");
    }

    #[tokio::test]
    #[serial]
    async fn fetch_title_cached_reuses_cache_within_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, log) = write_fake_gh(dir.path(), Some("Cached title"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let mut cache: HashMap<(String, u32), (String, Instant)> = HashMap::new();
        let root = dir.path().to_string_lossy().into_owned();
        let first = fetch_title_cached(&mut cache, &root, 7).await;
        let second = fetch_title_cached(&mut cache, &root, 7).await;

        std::env::remove_var(GH_BIN_ENV);
        assert_eq!(first.as_deref(), Some("Cached title"));
        assert_eq!(second.as_deref(), Some("Cached title"));
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            calls.lines().count(),
            1,
            "second lookup within the TTL must reuse the cache, not re-shell to gh; log: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn run_sink_enriches_dispatch_body_with_title() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) = write_fake_gh(dir.path(), Some("Add repo-qualified task_id"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(stub_server(listener, false, 1));

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket,
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            new_shared_state(),
            None,
        ));

        bus.publish(Event::SweepGlobalDispatch {
            sweep_id: "sweep-issue-4201-1".to_owned() as SweepId,
            kind: SweepKind::Issue(4201),
            runtime: None,
            runtime_source: None,
            repo: Some(dir.path().to_string_lossy().into_owned()),
        })
        .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("stub server must receive the enriched dispatch send")
            .unwrap();

        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
        std::env::remove_var(GH_BIN_ENV);

        assert_eq!(received.len(), 1);
        let body = received[0]["body"].as_str().unwrap();
        assert!(
            body.contains("Add repo-qualified task_id"),
            "dispatch body must be enriched with the fetched title; got: {body:?}"
        );
        assert!(body.contains("#4201 · dispatch"));
    }

    // ---- run_sink room routing end-to-end (#4225) ----

    /// End-to-end through the sink and a stub safehoused: a `task` (phase) line
    /// lazily creates and lands in the repo firehose room, while a `handoff`
    /// (blocker) line from the *same* repo lands in the signal room — one room per
    /// message, chosen by severity.
    #[tokio::test]
    async fn run_sink_routes_by_attention_class_and_creates_the_repo_room_lazily() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_string_lossy().into_owned();
        let repo_name = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let alias = format!("fleet-{repo_name}");

        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // 3 requests: create_room, the routed task send, the routed handoff send.
        let server = tokio::spawn(stub_server(listener, false, 3));

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                room: None,
                rooms: Some(RoomMap {
                    signal: Some("!signal:example.org".to_owned()),
                    by_repo: std::collections::BTreeMap::new(),
                }),
                ..SafehouseConfig::default()
            },
            socket,
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            new_shared_state(),
            None,
        ));

        bus.publish(Event::SweepPhase {
            issue: 4225,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: Some(repo.clone()),
        })
        .unwrap();
        bus.publish(Event::SweepBlocker {
            issue: 4225,
            reason: "needs a human".to_owned(),
            label_added: "loom:blocked".to_owned(),
            repo: Some(repo),
        })
        .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("stub server must receive the create_room + both routed sends")
            .unwrap();
        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;

        assert_eq!(received.len(), 3);
        // 1. The firehose room is created lazily, on this repo's first narration.
        assert_eq!(received[0]["op"], json!("create_room"));
        assert_eq!(received[0]["name"], json!(alias));
        // 2. `task` (dispatch/phase chatter) → the repo firehose room.
        assert_eq!(received[1]["op"], json!("send"));
        assert_eq!(received[1]["type"], json!("task"));
        assert_eq!(received[1]["room"], json!(alias));
        // 3. `handoff` (a human must act) → the signal room, same repo.
        assert_eq!(received[2]["type"], json!("handoff"));
        assert_eq!(received[2]["room"], json!("!signal:example.org"));
    }

    /// The migration default: with **no** `rooms` map the wire shape is exactly
    /// the pre-#4225 one — one send per event addressed at the single configured
    /// room, and no `create_room` op ever.
    #[tokio::test]
    async fn run_sink_without_a_rooms_map_keeps_the_single_room_wire_shape() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(stub_server(listener, false, 1));

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let state = new_shared_state();
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                room: Some("loom-fleet".to_owned()),
                rooms: None, // ← the migration default
                ..SafehouseConfig::default()
            },
            socket.clone(),
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            state.clone(),
            None,
        ));

        // A `task`-class event, which routing mode would have sent to a firehose.
        bus.publish(Event::SweepPhase {
            issue: 4225,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: Some(dir.path().to_string_lossy().into_owned()),
        })
        .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("stub server must receive the single-room send")
            .unwrap();
        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;

        assert_eq!(received.len(), 1, "no create_room op, no extra sends");
        assert_eq!(received[0]["op"], json!("send"));
        assert_eq!(received[0]["type"], json!("task"));
        assert_eq!(received[0]["room"], json!("loom-fleet"));
        // …and `status` still reports the same room it always did.
        assert_eq!(
            snapshot_state(&state),
            SafehouseState::Connected {
                socket,
                room: Some("loom-fleet".to_owned()),
            }
        );
    }

    // ---- completion emit point: SweepExited + forge merge check (#4426) ----

    /// Write an executable fake `gh` that answers the two forge lookups the
    /// completion path makes — `gh pr list …` and `gh repo view …` — logging
    /// every invocation. `None` for either makes that subcommand exit 1, which
    /// is how a missing/unauthenticated/offline `gh` presents.
    fn write_fake_forge_gh(
        dir: &std::path::Path,
        pr_list_json: Option<&str>,
        slug: Option<&str>,
    ) -> (PathBuf, PathBuf) {
        let log = dir.join("gh-forge-invocations.log");
        let script_path = dir.join("fake-forge-gh.sh");
        let arm = |stdout: Option<&str>| match stdout {
            Some(text) => format!("printf '%s\\n' {}; exit 0", shell_quote(text)),
            None => "exit 1".to_owned(),
        };
        let body = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\ncase \"$1 $2\" in\n  \
             'pr list') {} ;;\n  'repo view') {} ;;\n  *) exit 1 ;;\nesac\n",
            log.display(),
            arm(pr_list_json),
            arm(slug),
        );
        std::fs::write(&script_path, body).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        (script_path, log)
    }

    /// A `gh pr list` row carrying everything the enriched query asks for: the
    /// load-bearing merge facts plus the #4497 feed display fields.
    const MERGED_PR_JSON: &str = r#"[{"number":4400,"url":"https://github.com/rjwalters/loom/pull/4400","mergedAt":"2026-07-29T10:12:00Z","title":"feat: enrich completion meta","additions":214,"deletions":37}]"#;

    /// The pre-#4497 row shape: merge facts only. Stands in for a forge/`gh`
    /// that answers the query but returns none of the display fields — the
    /// completion must still publish, minus those keys.
    const MERGED_PR_JSON_NO_DISPLAY_FIELDS: &str = r#"[{"number":4400,"url":"https://github.com/rjwalters/loom/pull/4400","mergedAt":"2026-07-29T10:12:00Z"}]"#;

    /// Build an activity DB under `dir` carrying exactly one per-issue token
    /// rollup for `issue`, wired the way [`fetch_issue_tokens`]'s query reads it:
    /// a recorded input, a forge event linking that input to the issue, and a
    /// resource-usage sample linked to the same input.
    fn seed_activity_db_with_issue_tokens(
        dir: &std::path::Path,
        issue: i32,
        tokens_input: i64,
        tokens_output: i64,
    ) -> Arc<Mutex<ActivityDb>> {
        use crate::activity::{
            AgentInput, InputContext, InputType, PromptForgeEvent, PromptForgeEventType,
        };

        let db = ActivityDb::new(dir.join("activity.db")).unwrap();
        let input_id = db
            .record_input(&AgentInput {
                id: None,
                terminal_id: "loom-builder-1".to_owned(),
                timestamp: Utc::now(),
                input_type: InputType::Autonomous,
                content: "/loom:builder".to_owned(),
                agent_role: Some("builder".to_owned()),
                context: InputContext::default(),
            })
            .unwrap();
        db.record_prompt_forge_event(&PromptForgeEvent {
            id: None,
            input_id: Some(input_id),
            issue_number: Some(issue),
            pr_number: None,
            label_before: None,
            label_after: None,
            event_type: PromptForgeEventType::PrCreated,
        })
        .unwrap();
        db.record_resource_usage(&crate::activity::resource_usage::ResourceUsage {
            input_id: Some(input_id),
            model: "claude-opus-5".to_owned(),
            tokens_input,
            tokens_output,
            tokens_cache_read: None,
            tokens_cache_write: None,
            cost_usd: 1.25,
            duration_ms: Some(1_000),
            provider: "anthropic".to_owned(),
            timestamp: Utc::now(),
        })
        .unwrap();
        Arc::new(Mutex::new(db))
    }

    /// Write an executable fake `gh` whose `pr list` arm **rejects** the
    /// enriched `--json` field set exactly the way a `gh` too old to know
    /// `additions` does, and answers the narrower pre-#4497 set. Also logs every
    /// invocation so a test can prove the retry happened.
    fn write_fake_gh_rejecting_display_fields(dir: &std::path::Path) -> (PathBuf, PathBuf) {
        let log = dir.join("gh-forge-invocations.log");
        let script_path = dir.join("fake-old-gh.sh");
        let body = format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\n\
             case \"$1 $2\" in\n  \
             'pr list')\n    \
             if [[ \"$*\" == *additions* ]]; then\n      \
             echo 'unknown JSON field: \"additions\"' >&2\n      exit 1\n    fi\n    \
             printf '%s\\n' {}; exit 0 ;;\n  \
             'repo view') printf '%s\\n' {}; exit 0 ;;\n  \
             *) exit 1 ;;\nesac\n",
            log.display(),
            shell_quote(MERGED_PR_JSON_NO_DISPLAY_FIELDS),
            shell_quote("rjwalters/loom"),
        );
        std::fs::write(&script_path, body).unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();
        (script_path, log)
    }

    #[tokio::test]
    #[serial]
    async fn completion_for_exit_emits_when_the_pr_merged() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) =
            write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let root = dir.path().to_string_lossy().into_owned();
        let exited_at = DateTime::parse_from_rfc3339("2026-07-29T10:12:30Z")
            .unwrap()
            .with_timezone(&Utc);
        let envelope = completion_for_exit(
            "loom_daemon",
            &root,
            4426,
            750,
            exited_at,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            None,
        )
        .await;

        std::env::remove_var(GH_BIN_ENV);
        let envelope = envelope.expect("a merged PR must produce a completion envelope");
        assert_eq!(envelope.kind, "completion");
        let meta = envelope.meta.as_ref().unwrap();
        assert_eq!(meta["schema"], json!("completion-v1"));
        assert_eq!(meta["agent"], json!("loom_daemon"));
        // The forge slug, not the `#4201` path-basename narration convention.
        assert_eq!(meta["repo"], json!("rjwalters/loom"));
        assert_eq!(meta["ref"], json!("https://github.com/rjwalters/loom/pull/4400"));
        assert_eq!(meta["result"], json!("success"));
        assert_eq!(meta["issue"], json!(4426));
        // started_at is derived from the exit clock minus duration_sec.
        assert_eq!(meta["started_at"], json!("2026-07-29T10:00:00Z"));
        assert_eq!(meta["completed_at"], json!("2026-07-29T10:12:30Z"));
        // Feed display fields (#4497), harvested from the same `gh pr list` call
        // that verified the merge — no extra forge round-trip.
        assert_eq!(meta["title"], json!("feat: enrich completion meta"));
        assert_eq!(meta["additions"], json!(214));
        assert_eq!(meta["deletions"], json!(37));
        // No activity-DB handle was threaded in ⇒ `tokens` is omitted, never
        // guessed (the degradation contract).
        assert!(meta.get("tokens").is_none());
        assert!(build_send_request(&envelope, 1, None).is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn completion_for_exit_omits_display_fields_the_forge_did_not_return() {
        // The pre-#4497 row shape: the merge facts are all there, none of the
        // display fields are. The completion must still publish, with an
        // envelope byte-identical to the pre-#4497 one (#4497 AC2).
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) = write_fake_forge_gh(
            dir.path(),
            Some(MERGED_PR_JSON_NO_DISPLAY_FIELDS),
            Some("rjwalters/loom"),
        );
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let envelope = completion_for_exit(
            "loom_daemon",
            &dir.path().to_string_lossy(),
            4426,
            750,
            Utc::now(),
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            None,
        )
        .await;

        std::env::remove_var(GH_BIN_ENV);
        let envelope = envelope.expect("missing display fields must not cost us the completion");
        let meta = envelope.meta.as_ref().unwrap();
        for key in ["title", "additions", "deletions", "tokens"] {
            assert!(meta.get(key).is_none(), "{key} must be omitted when unavailable");
        }
        // Required keys + `issue`, i.e. exactly the pre-#4497 envelope.
        assert_eq!(meta.as_object().unwrap().len(), COMPLETION_REQUIRED_KEYS.len() + 1);
        assert!(build_send_request(&envelope, 1, None).is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn completion_for_exit_retries_the_base_field_set_when_gh_rejects_the_new_ones() {
        // A `gh` that does not know `additions` rejects the *whole* request, so
        // without the narrower retry #4497 would have silently cost every
        // completion on such a host.
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, log) = write_fake_gh_rejecting_display_fields(dir.path());
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let envelope = completion_for_exit(
            "loom_daemon",
            &dir.path().to_string_lossy(),
            4426,
            750,
            Utc::now(),
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            None,
        )
        .await;

        std::env::remove_var(GH_BIN_ENV);
        let envelope = envelope.expect("an old gh must still yield a completion, minus the extras");
        let meta = envelope.meta.as_ref().unwrap();
        assert_eq!(meta["ref"], json!("https://github.com/rjwalters/loom/pull/4400"));
        assert!(meta.get("title").is_none());
        assert!(meta.get("additions").is_none());
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.contains("additions"),
            "the enriched field set must be tried first; log: {calls:?}"
        );
        assert!(
            calls
                .lines()
                .any(|l| l.starts_with("pr list") && !l.contains("additions")),
            "the rejection must be retried with the base field set; log: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn completion_for_exit_carries_tokens_from_the_activity_db_rollup() {
        // The per-issue rollup is the sink's token source (#4497). Attribution is
        // knowingly imperfect (see `fetch_issue_tokens`), but when the DB *has* a
        // rollup for the issue the completion must publish input+output.
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) =
            write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let db = seed_activity_db_with_issue_tokens(dir.path(), 4426, 700_000, 91_000);

        let envelope = completion_for_exit(
            "loom_daemon",
            &dir.path().to_string_lossy(),
            4426,
            750,
            Utc::now(),
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            Some(&db),
        )
        .await;

        std::env::remove_var(GH_BIN_ENV);
        let envelope = envelope.expect("a merged PR must produce a completion envelope");
        let meta = envelope.meta.as_ref().unwrap();
        assert_eq!(meta["tokens"], json!(791_000), "tokens = input + output");
        assert!(build_send_request(&envelope, 1, None).is_ok());
    }

    #[tokio::test]
    #[serial]
    async fn completion_for_exit_omits_tokens_when_the_rollup_is_empty() {
        // "Omit rather than guess": an activity DB with no rollup for this issue
        // must not publish a `0`, which the feed would chart as free work.
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) =
            write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        // Seeded for a *different* issue, so this issue's rollup is empty.
        let db = seed_activity_db_with_issue_tokens(dir.path(), 999, 12_345, 678);

        let envelope = completion_for_exit(
            "loom_daemon",
            &dir.path().to_string_lossy(),
            4426,
            750,
            Utc::now(),
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            Some(&db),
        )
        .await;

        std::env::remove_var(GH_BIN_ENV);
        let meta = envelope
            .as_ref()
            .and_then(|e| e.meta.as_ref())
            .expect("an empty rollup must never cost us the completion");
        assert!(meta.get("tokens").is_none(), "an empty rollup ⇒ omitted, not 0");
        // The forge-sourced display fields are unaffected by the token miss.
        assert_eq!(meta["title"], json!("feat: enrich completion meta"));
    }

    #[tokio::test]
    #[serial]
    async fn completion_for_exit_is_silent_when_the_pr_did_not_merge() {
        // Exit 0 is not a merge: a clean sweep whose PR is still open must not
        // claim `result: "success"` on the public feed.
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) = write_fake_forge_gh(dir.path(), Some("[]"), Some("rjwalters/loom"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let out = completion_for_exit(
            "loom_daemon",
            &dir.path().to_string_lossy(),
            4426,
            750,
            Utc::now(),
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            None,
        )
        .await;

        std::env::remove_var(GH_BIN_ENV);
        assert!(out.is_none(), "no merged PR ⇒ no completion");
    }

    #[tokio::test]
    #[serial]
    async fn completion_for_exit_degrades_to_none_when_gh_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) = write_fake_forge_gh(dir.path(), None, None);
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let out = completion_for_exit(
            "loom_daemon",
            &dir.path().to_string_lossy(),
            4426,
            750,
            Utc::now(),
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            None,
        )
        .await;

        std::env::remove_var(GH_BIN_ENV);
        assert!(out.is_none(), "a failing gh must degrade to None, never panic or block");
    }

    #[tokio::test]
    #[serial]
    async fn completion_for_exit_emits_at_most_once_per_merge() {
        // A resumed sweep produces a second SweepExited for the same issue —
        // the merge is still the same one, so only one completion is emitted.
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, log) =
            write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let root = dir.path().to_string_lossy().into_owned();
        let mut slug_cache = HashMap::new();
        let mut completed = std::collections::HashSet::new();
        let first = completion_for_exit(
            "loom_daemon",
            &root,
            4426,
            750,
            Utc::now(),
            &mut slug_cache,
            &mut completed,
            None,
        )
        .await;
        let second = completion_for_exit(
            "loom_daemon",
            &root,
            4426,
            760,
            Utc::now(),
            &mut slug_cache,
            &mut completed,
            None,
        )
        .await;

        std::env::remove_var(GH_BIN_ENV);
        assert!(first.is_some());
        assert!(second.is_none(), "a second exit for the same issue must not double-post");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            calls.lines().count(),
            2,
            "the dedupe must short-circuit before re-shelling to gh; log: {calls:?}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn run_sink_narrates_exit_ack_then_completion() {
        // The emit-point mapping test: one `SweepExited` whose PR merged
        // produces the human `ack` and exactly one public-feed `completion`.
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) =
            write_fake_forge_gh(dir.path(), Some(MERGED_PR_JSON), Some("rjwalters/loom"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(stub_server(listener, false, 2));

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket,
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            new_shared_state(),
            None,
        ));

        bus.publish(Event::SweepExited {
            issue: 4426,
            exit_code: Some(0),
            duration_sec: 750,
            no_progress: false,
            death_class: None,
            repo: Some(dir.path().to_string_lossy().into_owned()),
        })
        .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("stub server must receive the ack and the completion")
            .unwrap();

        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
        std::env::remove_var(GH_BIN_ENV);

        assert_eq!(received.len(), 2);
        assert_eq!(received[0]["type"], json!("ack"));
        assert!(received[0].get("meta").is_none(), "an ack carries no meta");
        assert_eq!(received[1]["type"], json!("completion"));
        assert_eq!(received[1]["meta"]["schema"], json!("completion-v1"));
        assert_eq!(received[1]["meta"]["repo"], json!("rjwalters/loom"));
        assert_eq!(received[1]["meta"]["result"], json!("success"));
        assert_eq!(received[1]["meta"]["issue"], json!(4426));
        // The #4497 display fields make it all the way onto the wire, where
        // safehoused's egress redacts and publishes them.
        assert_eq!(received[1]["meta"]["title"], json!("feat: enrich completion meta"));
        assert_eq!(received[1]["meta"]["additions"], json!(214));
        assert_eq!(received[1]["meta"]["deletions"], json!(37));
        // Both lines thread together under the repo-qualified task_id.
        assert_eq!(received[0]["task_id"], received[1]["task_id"]);
        assert!(received[1]["body"].as_str().unwrap().contains("merged ✓"));
    }

    #[tokio::test]
    #[serial]
    async fn run_sink_narrates_only_the_ack_when_nothing_merged() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_gh, _log) = write_fake_forge_gh(dir.path(), Some("[]"), Some("rjwalters/loom"));
        std::env::set_var(GH_BIN_ENV, &fake_gh);

        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(stub_server(listener, false, 1));

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket,
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            new_shared_state(),
            None,
        ));

        bus.publish(Event::SweepExited {
            issue: 4426,
            exit_code: Some(1),
            duration_sec: 90,
            no_progress: false,
            death_class: None,
            repo: Some(dir.path().to_string_lossy().into_owned()),
        })
        .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("stub server must receive the failure ack")
            .unwrap();

        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
        std::env::remove_var(GH_BIN_ENV);

        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["type"], json!("ack"));
        assert!(received[0]["body"].as_str().unwrap().contains("failed ✗"));
    }

    // ---- integration: stub AF_UNIX socket ----

    /// Minimal stub safehoused: accept one connection, read the `hello`, reply
    /// `{"ok":true}`, then for each `send` optionally emit an interleaved push
    /// line (no id) before the id-echoed reply. Returns received `send` bodies.
    async fn stub_server(
        listener: UnixListener,
        interleave_push: bool,
        expected_sends: usize,
    ) -> Vec<Value> {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();

        // hello
        let hello: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(hello["op"], json!("hello"), "first request must be hello");
        write_half
            .write_all(b"{\"ok\":true,\"id\":0}\n")
            .await
            .unwrap();

        let mut received = Vec::new();
        for _ in 0..expected_sends {
            let Some(line) = lines.next_line().await.unwrap() else {
                break;
            };
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = req["id"].clone();
            received.push(req.clone());
            if interleave_push {
                // An async inbound room event: has `event`, no `id`. The client
                // must skip this and still match the reply below.
                write_half
                    .write_all(b"{\"event\":\"message\",\"body\":\"hello human\"}\n")
                    .await
                    .unwrap();
            }
            let reply = json!({"ok": true, "event_id": "$evt", "id": id});
            write_half
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        received
    }

    #[tokio::test]
    async fn client_hello_send_and_push_demux() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(stub_server(listener, true, 1));

        let mut client = SafehouseClient::connect(&socket, "loom_daemon", None)
            .await
            .unwrap();
        client
            .send(&Envelope {
                to: "*".to_owned(),
                kind: "task".to_owned(),
                task_id: Some("42".to_owned()),
                body: "issue #42 → builder".to_owned(),
                meta: None,
            })
            .await
            .expect("send must succeed despite the interleaved push line");

        let received = server.await.unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["op"], json!("send"));
        assert_eq!(received[0]["v"], json!(1));
        assert_eq!(received[0]["task_id"], json!("42"));
        assert!(received[0].get("from").is_none());
    }

    /// #4464: a protocol rejection (`ok:false`) is surfaced as the typed
    /// [`SendError::Rejected`] carrying safehoused's raw reason — the sink can
    /// tell it from a transport failure without string-matching an untyped
    /// error chain.
    #[tokio::test]
    async fn send_rejection_is_typed_and_names_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            // hello → ok
            let _ = lines.next_line().await.unwrap().unwrap();
            write_half
                .write_all(b"{\"ok\":true,\"id\":0}\n")
                .await
                .unwrap();
            // send → rejected with the canonical multi-room reason
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let id = req["id"].clone();
            write_half
                .write_all(
                    format!(
                        "{{\"ok\":false,\"error\":\"'room' required: 3 rooms joined\",\"id\":{id}}}\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            // Hold the connection open so the client observes the reply (a
            // rejection, not an EOF), which is the whole point of the split.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut client = SafehouseClient::connect(&socket, "loom_daemon", None)
            .await
            .unwrap();
        let err = client
            .send(&Envelope {
                to: "*".to_owned(),
                kind: "task".to_owned(),
                task_id: Some("42".to_owned()),
                body: "issue #42 → builder".to_owned(),
                meta: None,
            })
            .await
            .expect_err("a rejected send must be an error");
        match err {
            SendError::Rejected { reason } => {
                assert!(
                    reason.contains("'room' required"),
                    "reason must carry safehoused's raw error, got: {reason}"
                );
            }
            SendError::Transport(e) => panic!("expected Rejected, got Transport: {e:#}"),
        }
        server.abort();
    }

    /// #4464: a transport-level failure (peer closes mid-send) is surfaced as
    /// [`SendError::Transport`], NOT `Rejected` — the sink must reconnect
    /// rather than stick on a rejection diagnosis.
    #[tokio::test]
    async fn send_transport_failure_is_typed_transport() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            // hello → ok, then drop the connection: the send's reply read hits EOF.
            let _ = lines.next_line().await.unwrap().unwrap();
            write_half
                .write_all(b"{\"ok\":true,\"id\":0}\n")
                .await
                .unwrap();
            drop(write_half);
            drop(lines);
        });

        let mut client = SafehouseClient::connect(&socket, "loom_daemon", None)
            .await
            .unwrap();
        // The server has (or will imminently) close the connection after the
        // hello; the send's reply read then hits EOF.
        server.await.unwrap();
        let err = client
            .send(&Envelope {
                to: "*".to_owned(),
                kind: "task".to_owned(),
                task_id: Some("42".to_owned()),
                body: "body".to_owned(),
                meta: None,
            })
            .await
            .expect_err("a closed connection must fail the send");
        assert!(
            matches!(err, SendError::Transport(_)),
            "a closed connection is a transport failure, got: {err:?}"
        );
    }

    /// #4464: the sink reports `send_rejected` (with the reason) rather than
    /// `unreachable` when safehoused rejects the send, and clears back to
    /// `connected` once a send is accepted (config fixed + daemon restarted).
    #[tokio::test]
    async fn sink_send_rejection_reports_send_rejected_then_clears_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        // hello → ok; send 1 → rejected; send 2 → accepted.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await.unwrap().unwrap(); // hello
            write_half
                .write_all(b"{\"ok\":true,\"id\":0}\n")
                .await
                .unwrap();
            // send 1 → reject
            let r1: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let id1 = r1["id"].clone();
            write_half
                .write_all(
                    format!(
                        "{{\"ok\":false,\"error\":\"'room' required: 3 rooms joined\",\"id\":{id1}}}\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            // send 2 → accept (the operator set safehouse.room + restarted, in
            // effect — here we just accept to exercise the clear-on-success arm)
            let r2: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let id2 = r2["id"].clone();
            write_half
                .write_all(format!("{{\"ok\":true,\"event_id\":\"$e\",\"id\":{id2}}}\n").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let state = new_shared_state();
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket.clone(),
            subscription,
            Duration::from_millis(50),
            Duration::from_millis(200),
            state.clone(),
            None,
        ));

        // Event 1 → rejected → send_rejected (with reason), NOT unreachable.
        let _ = bus.publish(Event::SweepPhase {
            issue: 1,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: None,
        });
        let s = wait_for_state(&state, |s| matches!(s, SafehouseState::SendRejected { .. })).await;
        match s {
            SafehouseState::SendRejected { socket: sk, reason } => {
                assert_eq!(sk, socket);
                assert!(reason.contains("'room' required"), "reason: {reason}");
            }
            other => panic!("expected SendRejected, got {other:?}"),
        }

        // Event 2 → accepted → clears back to connected.
        let _ = bus.publish(Event::SweepPhase {
            issue: 2,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: None,
        });
        let s = wait_for_state(&state, |s| matches!(s, SafehouseState::Connected { .. })).await;
        assert!(matches!(s, SafehouseState::Connected { .. }), "got {s:?}");

        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
        server.abort();
    }

    /// #4464: the send-rejected diagnosis is sticky across a reconnect — a
    /// fresh `hello` after a transport blip must NOT flash "connected"; only an
    /// accepted send clears it.
    #[tokio::test]
    async fn sink_send_rejected_survives_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            // Connection 1: hello ok, reject send 1, then close (transport drop).
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await.unwrap().unwrap(); // hello
            write_half
                .write_all(b"{\"ok\":true,\"id\":0}\n")
                .await
                .unwrap();
            let r1: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let id1 = r1["id"].clone();
            write_half
                .write_all(
                    format!(
                        "{{\"ok\":false,\"error\":\"'room' required: 3 rooms joined\",\"id\":{id1}}}\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            drop(write_half);
            drop(lines);

            // Connection 2: hello ok, then stall before replying to the send so
            // the sink's state is observable at "just reconnected, no send
            // accepted yet" — it must be SendRejected, not Connected.
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await.unwrap().unwrap(); // hello
            write_half
                .write_all(b"{\"ok\":true,\"id\":0}\n")
                .await
                .unwrap();
            let _ = lines.next_line().await.unwrap().unwrap(); // send (read, do not reply yet)
            tokio::time::sleep(Duration::from_millis(400)).await;
        });

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let state = new_shared_state();
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket.clone(),
            subscription,
            Duration::from_millis(30),
            Duration::from_millis(60),
            state.clone(),
            None,
        ));

        // Event 1 → conn1 rejects → SendRejected.
        let _ = bus.publish(Event::SweepPhase {
            issue: 1,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: None,
        });
        wait_for_state(&state, |s| matches!(s, SafehouseState::SendRejected { .. })).await;

        // Event 2 → send on the now-closed conn1 fails (transport) → Unreachable.
        let _ = bus.publish(Event::SweepPhase {
            issue: 2,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: None,
        });
        wait_for_state(&state, |s| matches!(s, SafehouseState::Unreachable { .. })).await;

        // Event 3 → reconnect (conn2 hello ok); the send stalls server-side so
        // the state settles at the reconnect value. Stickiness ⇒ SendRejected.
        tokio::time::sleep(Duration::from_millis(80)).await; // clear the backoff window
        let _ = bus.publish(Event::SweepPhase {
            issue: 3,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: None,
        });
        let s = wait_for_state(&state, |s| matches!(s, SafehouseState::SendRejected { .. })).await;
        assert!(
            matches!(s, SafehouseState::SendRejected { .. }),
            "a reconnect whose hello succeeds must NOT clear the send-rejected \
             diagnosis, got {s:?}"
        );

        drop(bus);
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
        server.abort();
    }

    /// Poll `state` until `pred` holds (or panic after ~1s) — test helper for
    /// the lazily-connecting sink whose transitions are not synchronous with
    /// `bus.publish`.
    async fn wait_for_state(
        state: &SharedSafehouseState,
        pred: impl Fn(&SafehouseState) -> bool,
    ) -> SafehouseState {
        for _ in 0..100 {
            let s = snapshot_state(state);
            if pred(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let s = snapshot_state(state);
        panic!("state never satisfied predicate; last = {s:?}");
    }

    #[tokio::test]
    async fn disabled_config_does_not_subscribe() {
        let bus = EventBus::new();
        assert_eq!(bus.receiver_count(), 0);
        let state = new_shared_state();
        let handle = spawn_sink(
            SafehouseConfig::default(), // disabled
            &bus,
            &tokio::runtime::Handle::current(),
            state.clone(),
            None,
        );
        assert!(handle.is_none(), "disabled ⇒ no sink task");
        // The load-bearing no-op assertion: no subscription was created.
        assert_eq!(bus.receiver_count(), 0, "disabled ⇒ no bus subscription");
        // #4345: disabled must report as "not configured", never silence.
        assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
    }

    #[tokio::test]
    async fn absent_peer_degrades_without_blocking() {
        // enabled + nonexistent socket: the sink subscribes, but every connect
        // fails and is swallowed — publishing never blocks or errors.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("does-not-exist.sock");
        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());

        let state = new_shared_state();
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket),
                ..SafehouseConfig::default()
            },
            PathBuf::from("/nonexistent/safehoused.sock"),
            subscription,
            Duration::from_millis(50),
            Duration::from_millis(200),
            state.clone(),
            None,
        ));

        // Publish a burst; the sink must consume them all without wedging.
        for issue in 0..5u32 {
            let _ = bus.publish(Event::SweepPhase {
                issue,
                phase: "builder".to_owned(),
                pr_number: None,
                repo: None,
            });
        }
        // Give the sink a moment to drain, then drop the bus to close the sub.
        tokio::time::sleep(Duration::from_millis(100)).await;
        // #4345: an absent peer must report "unreachable" (with the resolved
        // socket path), never silence and never a stale "not configured".
        match snapshot_state(&state) {
            SafehouseState::Unreachable { socket } => {
                assert_eq!(socket, PathBuf::from("/nonexistent/safehoused.sock"));
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
        drop(bus);
        // The sink must exit cleanly once the bus closes (it never blocked).
        tokio::time::timeout(Duration::from_secs(2), sink)
            .await
            .expect("sink must terminate after bus close")
            .unwrap();
    }

    #[tokio::test]
    async fn reconnects_after_mid_run_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");

        // First listener: serve one send, then drop (simulating a restart).
        let listener1 = UnixListener::bind(&socket).unwrap();
        let server1 = tokio::spawn(stub_server(listener1, false, 1));

        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());
        let state = new_shared_state();
        let sink = tokio::spawn(run_sink(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket.clone(),
            subscription,
            Duration::from_millis(20),
            Duration::from_millis(80),
            state.clone(),
            None,
        ));

        // First event: delivered over listener1.
        bus.publish(Event::SweepPhase {
            issue: 1,
            phase: "builder".to_owned(),
            pr_number: None,
            repo: None,
        })
        .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), server1)
            .await
            .expect("first server should receive one send")
            .unwrap();
        assert_eq!(first.len(), 1);

        // listener1 is dropped now; the client's next send will fail. Rebind a
        // fresh listener on the same path (the "restart") — a real daemon
        // restart unlinks the stale socket file first, so mirror that here.
        std::fs::remove_file(&socket).ok();
        let listener2 = UnixListener::bind(&socket).unwrap();
        let server2 = tokio::spawn(stub_server(listener2, false, 1));

        // Publish more events until one lands on listener2 (the first may hit
        // the dead connection and be dropped; the sink reconnects with backoff).
        let sink_done = tokio::spawn(async move {
            for issue in 2..40u32 {
                let _ = bus.publish(Event::SweepPhase {
                    issue,
                    phase: "judge".to_owned(),
                    pr_number: None,
                    repo: None,
                });
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            drop(bus);
        });

        let second = tokio::time::timeout(Duration::from_secs(5), server2)
            .await
            .expect("sink must reconnect and deliver to the second server")
            .unwrap();
        assert!(!second.is_empty(), "a narration must land post-reconnect");
        // #4345: the reconnect must be visible as Connected again, not stuck
        // reporting the mid-outage Unreachable value.
        match snapshot_state(&state) {
            SafehouseState::Connected { socket: s, .. } => assert_eq!(s, socket),
            other => panic!("expected Connected after reconnect, got {other:?}"),
        }

        sink_done.await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), sink).await;
    }

    // ---- peer-claim coordination (#4028) ----

    #[test]
    fn claim_ad_serializes_to_a_valid_task_envelope() {
        // Envelope-validity AC: the advertisement must serialize to a `type`
        // within KNOWN_TYPES and a `task_id` matching [A-Za-z0-9_], asserted
        // against build_send_request so a rejected-by-safehoused envelope fails
        // at test time, not runtime.
        let ad = ClaimAd::advertise(4028, "loom".into(), "maple".into(), 7, "ts".into());
        let env = claim_ad_to_envelope(&ad);
        assert_eq!(env.kind, "task");
        assert!(KNOWN_TYPES.contains(&env.kind.as_str()));
        assert_eq!(env.task_id.as_deref(), Some("4028"));

        let req = build_send_request(&env, 1, Some("fleet")).unwrap();
        assert_eq!(req["type"], json!("task"));
        assert_eq!(req["task_id"], json!("4028"));
        // The body round-trips back to the same claim on the receive side.
        let body = req["body"].as_str().unwrap();
        assert_eq!(ClaimAd::from_body_str(body), Some(ad));
    }

    #[tokio::test]
    async fn disabled_config_spawns_no_coordination_task() {
        // Byte-for-byte no-op AC: safehouse.enabled=false ⇒ no coordination task
        // and no socket.
        let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(1))));
        let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view));
        let (_tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(1);
        let state = new_shared_state();
        let handle = spawn_peer_coordination(
            SafehouseConfig::default(), // disabled
            sink,
            rx,
            &tokio::runtime::Handle::current(),
            state.clone(),
        );
        assert!(handle.is_none(), "disabled ⇒ no coordination task");
        // #4345: disabled must report as "not configured".
        assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
    }

    #[tokio::test]
    async fn idle_daemon_still_reads_inbound_peer_ad() {
        // The core regression Gap 1a exists to fix: a daemon that emits NOTHING
        // must still observe an inbound peer advertisement. A read_reply-piggyback
        // implementation only reads while sending, so it would never see this.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let hello: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(hello["op"], json!("hello"));
            write_half
                .write_all(b"{\"ok\":true,\"id\":0}\n")
                .await
                .unwrap();
            // Unprompted inbound room event carrying a peer claim — the client
            // sends nothing.
            let claim = ClaimAd::advertise(4028, "loom".into(), "peer".into(), 5, "ts".into());
            let push =
                json!({"event": "message", "from": "loom_daemon", "body": claim.to_body_json()});
            write_half
                .write_all(format!("{push}\n").as_bytes())
                .await
                .unwrap();
            // Hold the connection open so the client can read the push.
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(120))));
        let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view.clone()));
        let (_tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(8); // held → task stays alive; never send (idle)

        let state = new_shared_state();
        let task = tokio::spawn(run_coordination(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket.clone(),
            sink,
            rx,
            Duration::from_millis(20),
            Duration::from_millis(80),
            state.clone(),
        ));

        // Poll the view for the claim (bounded condition-poll, not a fixed
        // ordering sleep).
        let mut seen = false;
        for _ in 0..50 {
            if view
                .lock()
                .unwrap()
                .is_claimed_at("loom", 4028, Instant::now())
            {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(seen, "an idle daemon must still observe the inbound peer claim (Gap 1a)");
        // #4345: a live, idle coordination connection must report Connected.
        match snapshot_state(&state) {
            SafehouseState::Connected { socket: s, .. } => assert_eq!(s, socket),
            other => panic!("expected Connected, got {other:?}"),
        }
        server.await.unwrap();
        task.abort();
    }

    /// #4225's resolved open question: claim ads are per-repo `task` chatter, but
    /// they are advertised into the **signal room** as a deliberate exception —
    /// it is the only room every host's bot is guaranteed to be joined to, and
    /// cross-host dedup is a correctness property (see `run_coordination`'s doc
    /// comment). The reader is trivially consistent with that choice because it
    /// consumes any inbound claim regardless of room.
    #[tokio::test]
    async fn coordination_advertises_claim_ads_into_the_signal_room() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("safehoused.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let hello: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(hello["op"], json!("hello"));
            write_half
                .write_all(b"{\"ok\":true,\"id\":0}\n")
                .await
                .unwrap();
            let ad: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let reply = json!({"ok": true, "id": ad["id"].clone()});
            write_half
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            ad
        });

        let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(120))));
        let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view));
        let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(8);

        let task = tokio::spawn(run_coordination(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                // Routing mode with the legacy scalar `room` unset — exactly the
                // configuration in which an ad would be rejected outright
                // (`'room' required`) if it did not resolve the signal room.
                room: None,
                rooms: Some(RoomMap {
                    signal: Some("!signal:example.org".to_owned()),
                    by_repo: [("loom".to_owned(), "!fleet-loom:example.org".to_owned())]
                        .into_iter()
                        .collect(),
                }),
                ..SafehouseConfig::default()
            },
            socket,
            sink,
            rx,
            Duration::from_millis(20),
            Duration::from_millis(80),
            new_shared_state(),
        ));

        tx.send(ClaimAd::advertise(4225, "loom".into(), "maple".into(), 7, "ts".into()))
            .await
            .unwrap();

        let ad = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("the stub must receive the claim ad")
            .unwrap();
        assert_eq!(ad["op"], json!("send"));
        assert_eq!(ad["type"], json!("task"));
        assert_eq!(
            ad["room"],
            json!("!signal:example.org"),
            "claim ads ride the signal room, NOT the repo firehose (documented exception)"
        );
        task.abort();
    }

    #[tokio::test]
    async fn coordination_reconnects_when_socket_absent_and_exits_on_sender_drop() {
        // Fail-open: an absent socket must never wedge — the task loops with
        // backoff and exits cleanly once its senders drop.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("nope.sock"); // never bound
        let view = Arc::new(Mutex::new(PeerClaimView::new("me".into(), Duration::from_secs(1))));
        let sink: Arc<dyn InboundEventSink> = Arc::new(PeerClaimSink::new(view));
        let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(4);
        let state = new_shared_state();

        let task = tokio::spawn(run_coordination(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket.clone(),
            sink,
            rx,
            Duration::from_millis(10),
            Duration::from_millis(30),
            state.clone(),
        ));

        // Drop the only sender: the connect-fail drain loop must observe
        // Disconnected and return, so the task terminates rather than spinning.
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("coordination task must terminate after its senders drop")
            .unwrap();
        // #4345: a socket that never accepts must report Unreachable, never a
        // stale "not configured" (the config here IS enabled).
        match snapshot_state(&state) {
            SafehouseState::Unreachable { socket: s } => assert_eq!(s, socket),
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }
}
