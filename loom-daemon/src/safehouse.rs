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
use std::sync::{Arc, Mutex, OnceLock};
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
use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::types::{Event, SweepKind};

/// Runtime attribution on a `completion-v1` payload (Issue #8507): the
/// `runtime`/`provider`/`profile` label emit + validation, and the
/// runtime-dispatched per-model token lookup. A sibling module because this
/// file is over `.loom/docs/file-size-policy.md`'s threshold and frozen.
mod completion_runtime;

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

/// The dedicated peer-claim coordination room (#4713): when set, claim
/// advertise/retract traffic rides this room instead of the signal room, so a
/// human watching the signal room no longer sees the 30-second heartbeat
/// cadence `sweep_registry::readvertise_peer_claims` re-publishes. Absent ⇒
/// falls back to [`SafehouseConfig::signal_room`] — see
/// [`SafehouseConfig::claims_room`].
const ROOM_CLAIMS_ENV: &str = "LOOM_SAFEHOUSE_ROOM_CLAIMS";

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
///
/// `digest` (#4217) is the newest member: one wave-dispatch digest root
/// (`run_sink`'s dispatch-digest window) in place of N near-identical `task`
/// roots, routed to the signal room like `handoff`/`ack`/`completion`.
pub const KNOWN_TYPES: [&str; 6] = ["chat", "task", "handoff", "ack", "completion", "digest"];

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
///
/// A **claims-only** map (`claims` set, no `signal`, no `byRepo`) is kept — so
/// [`SafehouseConfig::claims_room`] resolves — but it does **not** activate
/// attention-class narration routing: [`routes_narration`](Self::routes_narration)
/// gates narration on a narration target, keeping `claims` an independent knob
/// (#4713's "opt-in, default-unchanged" contract).
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
    /// The dedicated peer-claim coordination room (#4713), opt-in. `None` ⇒
    /// fall back to [`signal`](Self::signal) (see
    /// [`SafehouseConfig::claims_room`]) — the same "absent means
    /// unchanged-default" contract `signal` itself has against the legacy
    /// [`SafehouseConfig::room`] scalar. Setting this splits the machine-cadence
    /// claim advertise/retract heartbeat out of the human-facing signal room
    /// into its own coordination room; an operator who does this **must**
    /// ensure every host's safehoused bot is joined to it (the same
    /// cross-host-provisioning requirement documented for `signal`).
    pub claims: Option<String>,
}

impl RoomMap {
    /// Whether this map carries no usable routing target at all, in which case
    /// callers normalize it to `None` (single-room mode).
    #[must_use]
    fn is_empty(&self) -> bool {
        self.signal.is_none() && self.by_repo.is_empty() && self.claims.is_none()
    }

    /// Whether this map carries a **narration** routing target (`signal` and/or
    /// a `byRepo` entry). A claims-only map does **not**: `claims` redirects the
    /// peer-claim coordination connection only (#4713) and must never flip the
    /// narration sink out of its byte-identical single-room mode as a side
    /// effect. Both [`SafehouseConfig::routes_by_attention`] and
    /// [`RoomRouter::resolve`] gate on this, keeping the two knobs independent.
    #[must_use]
    fn routes_narration(&self) -> bool {
        self.signal.is_some() || !self.by_repo.is_empty()
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
    /// Also the **fallback** for the peer-claim coordination connection: since
    /// #4713 [`run_coordination`] resolves [`claims_room`](Self::claims_room),
    /// which lands here only while `rooms.claims` is unset — see
    /// [`run_coordination`] for why claim ads default to the signal room.
    #[must_use]
    pub fn signal_room(&self) -> Option<&str> {
        self.rooms
            .as_ref()
            .and_then(|rooms| rooms.signal.as_deref())
            .or(self.room.as_deref())
    }

    /// The room peer-claim advertise/retract traffic goes to (#4713):
    /// `rooms.claims` when the map configures one, else falls back to
    /// [`signal_room`](Self::signal_room) — which itself falls back to the
    /// legacy scalar [`room`](Self::room). This chain is what keeps existing
    /// deployments (no `rooms.claims` configured) byte-identical to pre-#4713
    /// behavior: claim ads keep riding the signal room exactly as
    /// [`run_coordination`]'s doc comment describes.
    #[must_use]
    pub fn claims_room(&self) -> Option<&str> {
        self.rooms
            .as_ref()
            .and_then(|rooms| rooms.claims.as_deref())
            .or_else(|| self.signal_room())
    }

    /// Whether attention-class **narration** routing is active: the `rooms` map
    /// configures a narration target (`signal` and/or `byRepo`). A claims-only
    /// map deliberately does **not** activate it — `rooms.claims` is an
    /// independent knob that only redirects the peer-claim coordination
    /// connection ([`claims_room`](Self::claims_room)); narration stays in
    /// single-room mode, byte-identical (#4713).
    #[must_use]
    pub fn routes_by_attention(&self) -> bool {
        self.rooms.as_ref().is_some_and(RoomMap::routes_narration)
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
    if let Some(claims) = rooms.get("claims").and_then(Value::as_str) {
        if !claims.trim().is_empty() {
            map.claims = Some(claims.trim().to_owned());
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
/// - `LOOM_SAFEHOUSE_ROOM_CLAIMS` (#4713) overrides `rooms.claims` alone.
/// - `LOOM_SAFEHOUSE_ROOMS_BY_REPO` (`repo=room,repo=room…`) replaces the
///   **whole** `byRepo` map rather than merging into it, so an operator can
///   override a stale committed map from the environment without editing config
///   (the same wholesale-replacement semantics `LOOM_SAFEHOUSE_WORKER_PERSONAS`
///   uses for its list).
/// - Any one of these env vars alone is enough to *enable* routing on a config
///   that has no `rooms` block at all.
/// - None set ⇒ the config-layer map is returned untouched (so the absent-map
///   single-room default stays byte-identical).
#[must_use]
fn apply_room_env_overrides(rooms: Option<RoomMap>) -> Option<RoomMap> {
    let signal = env_nonempty(ROOM_SIGNAL_ENV);
    let claims = env_nonempty(ROOM_CLAIMS_ENV);
    let by_repo = env_nonempty(ROOMS_BY_REPO_ENV).map(|raw| parse_by_repo_env(&raw));
    if signal.is_none() && claims.is_none() && by_repo.is_none() {
        return rooms;
    }
    let mut map = rooms.unwrap_or_default();
    if let Some(signal) = signal {
        map.signal = Some(signal);
    }
    if let Some(claims) = claims {
        map.claims = Some(claims);
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

/// Resolve the socket path at connect time. **Precedence: env > config >
/// None** — `$LOOM_SAFEHOUSE_SOCKET` (already folded into `cfg.socket` by
/// [`apply_env_overrides`]) or the unprefixed `$SAFEHOUSED_SOCKET` convention
/// safehoused clients read both win over `cfg.socket`, which by this point
/// only reflects the committed/local-override config value.
///
/// Checking env directly here (not just relying on the already-merged
/// `cfg.socket`) is load-bearing: before this fix `cfg.socket` was checked
/// *first*, so a committed `safehouse.socket` path (opt-in, per-host, and
/// often copied verbatim into shared `.loom/config.json`) permanently shadowed
/// any `$SAFEHOUSED_SOCKET` override — the same defect class #5354/#5336
/// fixed for `observability.ingestKeyFile`, now mirrored here for
/// `safehouse.socket` (#5457). No built-in `$HOME`-relative default: unlike
/// `ingestKeyFile`, safehouse is opt-in per-host, so an unconfigured socket
/// degrades to `None` (warn + skip) rather than guessing a path.
///
/// `pub` since #7893: the inbound-steering task in
/// [`crate::safehouse_chatops::runtime`] resolves the same socket through this
/// one function rather than re-deriving the env/config precedence, so the two
/// safehouse consumers can never disagree about which socket they dialed.
#[must_use]
pub fn resolve_socket(cfg: &SafehouseConfig) -> Option<PathBuf> {
    env_nonempty(SOCKET_ENV)
        .or_else(|| env_nonempty(SAFEHOUSED_SOCKET_ENV))
        .map(PathBuf::from)
        .or_else(|| cfg.socket.clone())
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
    /// Per-`(model, speed, service_tier)` token totals (#5740) — the raw
    /// counts a consumer needs to actually price a sweep, since `tokens`
    /// alone merges five quantities that price between 0.1x-2x of each other
    /// across models that are themselves 3-5x apart. Additive alongside
    /// `tokens`, which keeps its existing flat-sum meaning; omitted (never
    /// `[]`) when nothing attributable was found, same "unknown != zero"
    /// contract as `tokens`. See [`fetch_transcript_tokens`] for the source.
    pub tokens_by_model: Option<Vec<ModelUsageTotals>>,
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
    /// Repo visibility, `"public"` or `"private"` (#6596) — the field the
    /// public-feed egress gates on, from the same `gh repo view` call that
    /// resolves `repo`. Omitted when the lookup could not determine it (an
    /// older `gh`); an egress consumer must treat anything other than an
    /// explicit `"public"` as not publishable, so absence fails **closed**.
    /// An enum rather than a string for the same reason [`CompletionResult`]
    /// is one: no caller can invent a third visibility the feed would have to
    /// guess about.
    pub visibility: Option<RepoVisibility>,
    /// Runtime adapter the work actually ran on (Issue #8507) — `"opencode"`,
    /// `"pi"`, … — read off the sweep's own `# LOOM_LAUNCH` record
    /// ([`crate::launch_record::sweep_runtime_attribution`]), never re-derived
    /// from dispatch-time config.
    ///
    /// Independent of [`Self::tokens_by_model`] on purpose: it is the field
    /// that lets the feed label a non-Claude completion **even when no usage
    /// numbers were found at all**, which was the whole failure #8507
    /// reported. Omitted — never a fabricated `"claude"` default — when no
    /// launch record was found, which is exactly the Claude/legacy case, so
    /// every pre-#8507 payload stays byte-identical.
    pub runtime: Option<String>,
    /// The runtime's resolved provider namespace (`"friendli"`,
    /// `"zai-coding-plan"`, …), when the launch resolved one. Same source and
    /// same omission contract as [`Self::runtime`]. **Never** a credential,
    /// endpoint or key — the launch record carries none.
    pub provider: Option<String>,
    /// The resolved model-profile name, when the launch selected one. Same
    /// source and same omission contract as [`Self::runtime`].
    pub profile: Option<String>,
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
        // Per-model breakdown (#5740) — additive alongside `tokens`, same
        // omit-rather-than-guess contract: an empty vec is treated as absent.
        if let Some(rows) = self.tokens_by_model.as_ref().filter(|v| !v.is_empty()) {
            let arr: Vec<Value> = rows
                .iter()
                .map(|r| {
                    json!({
                        "model": r.model,
                        "speed": r.speed,
                        "service_tier": r.service_tier,
                        "input": r.input,
                        "cache_read": r.cache_read,
                        "cache_write_5m": r.cache_write_5m,
                        "cache_write_1h": r.cache_write_1h,
                        "output": r.output,
                    })
                })
                .collect();
            obj.insert("tokens_by_model".into(), Value::Array(arr));
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
        // Repo visibility (#6596): the egress gate's input. Omitted when
        // unknown — a consumer that publishes only on an explicit `"public"`
        // then fails closed, which is the intended degradation.
        if let Some(visibility) = self.visibility {
            obj.insert("visibility".into(), json!(visibility.as_str()));
        }
        // Runtime attribution (#8507) — see `completion_runtime`.
        completion_runtime::insert_runtime_labels(obj, self);
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
    // `tokens_by_model` (#5740) is an optional array of per-model rows; when
    // present it must be non-empty (an empty vec is an omission, not a
    // publishable value — `to_meta_value` never emits `[]`) and every row must
    // carry non-empty `model`/`speed`/`service_tier` strings plus five
    // non-negative integer counters.
    if let Some(v) = obj.get("tokens_by_model") {
        let Some(arr) = v.as_array() else {
            bail!("completion `meta.tokens_by_model`, when present, must be an array, got {v}");
        };
        if arr.is_empty() {
            bail!("completion `meta.tokens_by_model`, when present, must be non-empty (omit instead of `[]`)");
        }
        for (i, row) in arr.iter().enumerate() {
            let Some(row_obj) = row.as_object() else {
                bail!("completion `meta.tokens_by_model[{i}]` must be an object, got {row}");
            };
            for key in ["model", "speed", "service_tier"] {
                match row_obj.get(key).and_then(Value::as_str) {
                    Some(s) if !s.trim().is_empty() => {}
                    _ => bail!(
                        "completion `meta.tokens_by_model[{i}].{key}` must be a non-empty string, got {:?}",
                        row_obj.get(key)
                    ),
                }
            }
            for key in [
                "input",
                "cache_read",
                "cache_write_5m",
                "cache_write_1h",
                "output",
            ] {
                match row_obj.get(key) {
                    Some(n) if n.as_u64().is_some() => {}
                    _ => bail!(
                        "completion `meta.tokens_by_model[{i}].{key}` must be a non-negative integer, got {:?}",
                        row_obj.get(key)
                    ),
                }
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
    // `runtime`/`provider`/`profile` (#8507) — see `completion_runtime`.
    completion_runtime::validate_runtime_labels(obj)?;
    // `visibility` (#6596) is an optional closed enum. A consumer gates public
    // egress on it, so a third value (or a non-string) must never reach the
    // wire, where it would have to be guessed about: refuse the envelope
    // instead. Absence stays legal — that is the "unknown ⇒ fail closed" case.
    if let Some(v) = obj.get("visibility") {
        let ok = v.as_str().is_some_and(|s| {
            s == RepoVisibility::Public.as_str() || s == RepoVisibility::Private.as_str()
        });
        if !ok {
            bail!(
                "completion `meta.visibility`, when present, must be {:?} or {:?}, got {v}",
                RepoVisibility::Public.as_str(),
                RepoVisibility::Private.as_str()
            );
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
    Digest,
}

impl EnvelopeKind {
    /// Every member, in [`KNOWN_TYPES`] order.
    pub const ALL: [Self; 6] = [
        Self::Chat,
        Self::Task,
        Self::Handoff,
        Self::Ack,
        Self::Completion,
        Self::Digest,
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
            Self::Digest => "digest",
        }
    }

    /// Parse a wire `type` string; `None` for anything outside [`KNOWN_TYPES`]
    /// (which [`build_send_request`] refuses to send anyway).
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == kind)
    }

    /// **The routing table** (#4225). Exhaustive by construction — no wildcard
    /// arm — so a future member fails to compile here rather than defaulting
    /// into the wrong room. `Digest` (#4217, wave-dispatch digest roots) is the
    /// most recent addition, slotted in as another [`AttentionClass::Signal`]
    /// arm exactly as this comment originally anticipated.
    #[must_use]
    pub const fn attention_class(self) -> AttentionClass {
        match self {
            // Terminal / human-attention outcomes → the signal room.
            Self::Handoff | Self::Ack | Self::Completion | Self::Digest => AttentionClass::Signal,
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
        // single most important invariant of this change. A map with no
        // narration target (claims-only, #4713) is deliberately treated the
        // same: `rooms.claims` redirects the peer-claim coordination connection
        // only, and must not switch narration into attention-class routing —
        // per-repo lazy firehose creation included — as a side effect.
        let Some(map) = self.map.as_ref().filter(|map| map.routes_narration()) else {
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

/// Build the single-issue `task`-kind dispatch envelope (the pre-#4217 shape,
/// unchanged): `<repo>#n · dispatch`, repo-qualified `task_id`. Shared by
/// [`event_to_envelope`] (the pure per-event mapping, still exercised directly
/// by callers/tests that bypass the sink's digest-window batching) and
/// [`run_sink`]'s digest-window flush, which calls this when a window ends
/// with exactly one buffered dispatch — issue #4217's "single dispatches keep
/// current behavior" contract.
fn dispatch_envelope(repo: Option<&str>, issue: u32) -> Envelope {
    Envelope {
        to: "*".to_owned(),
        kind: "task".to_owned(),
        task_id: Some(qualify_task_id(repo, issue)),
        body: format!("{} · dispatch", repo_issue_prefix(repo, issue)),
        meta: None,
    }
}

// ============================================================================
// Dispatch-digest batching (issue #4217)
// ============================================================================

/// Test/operator override for the dispatch-digest coalescing window, mirroring
/// the existing `RECONCILE_INTERVAL_ENV` seam: milliseconds, so a test can
/// exercise a full window without a real wall-clock wait. Unset/unparsable
/// falls back to [`DEFAULT_DISPATCH_DIGEST_WINDOW`].
const DISPATCH_DIGEST_WINDOW_ENV: &str = "LOOM_SAFEHOUSE_DISPATCH_DIGEST_WINDOW_MS";

/// Production default: long enough to coalesce a work-finder tick's admitted
/// dispatches (observed: 7 within seconds, #4217's motivating case — each
/// admission does its own `gh` label-flip round trip, so a wave can spread
/// over several seconds, not just milliseconds) into one digest, short enough
/// that a genuinely isolated dispatch is still narrated within half a minute.
const DEFAULT_DISPATCH_DIGEST_WINDOW: Duration = Duration::from_secs(30);

fn dispatch_digest_window() -> Duration {
    env_nonempty(DISPATCH_DIGEST_WINDOW_ENV)
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DISPATCH_DIGEST_WINDOW)
}

/// One admitted issue-dispatch buffered inside the digest window, waiting to
/// find out whether it is alone (flushed as today's single-dispatch `task`
/// envelope) or part of a burst (folded into one `digest` envelope) — #4217.
#[derive(Debug, Clone)]
struct PendingDispatch {
    repo: Option<String>,
    issue: u32,
}

/// Build the multi-dispatch digest envelope for a burst of buffered
/// dispatches (issue #4217): **one** `digest`-kind root instead of N
/// near-identical `task`-kind roots, grouped per repo and counted, e.g.
/// `dispatched 7: loom×6 (#4028 #4106 #4144 #4157 #4162 #4164), vibesql×1
/// (#6173)`. Groups sort by descending count (ties broken alphabetically) so
/// the busiest repo leads; issue numbers within a group sort ascending.
/// `seq` gives each digest a distinct `task_id` so consecutive digests are
/// separate thread roots rather than one perpetual accumulating thread — this
/// module's contract is "one root per burst", not "one root ever" ([`Envelope::task_id`]
/// must be `[A-Za-z0-9_]`, which a plain counter satisfies trivially).
fn build_dispatch_digest_envelope(batch: &[PendingDispatch], seq: u64) -> Envelope {
    let mut groups: std::collections::BTreeMap<String, Vec<u32>> =
        std::collections::BTreeMap::new();
    for dispatch in batch {
        let name = repo_basename(dispatch.repo.as_deref()).unwrap_or_else(|| "unscoped".to_owned());
        groups.entry(name).or_default().push(dispatch.issue);
    }
    for issues in groups.values_mut() {
        issues.sort_unstable();
    }
    let mut ordered: Vec<(String, Vec<u32>)> = groups.into_iter().collect();
    ordered.sort_by(|(a_repo, a_issues), (b_repo, b_issues)| {
        b_issues
            .len()
            .cmp(&a_issues.len())
            .then_with(|| a_repo.cmp(b_repo))
    });
    let parts: Vec<String> = ordered
        .iter()
        .map(|(repo, issues)| {
            let nums: Vec<String> = issues.iter().map(|n| format!("#{n}")).collect();
            format!("{repo}×{} ({})", issues.len(), nums.join(" "))
        })
        .collect();
    Envelope {
        to: "*".to_owned(),
        kind: "digest".to_owned(),
        task_id: Some(format!("dispatch_digest_{seq}")),
        body: format!("dispatched {}: {}", batch.len(), parts.join(", ")),
        meta: None,
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
        } => Some(dispatch_envelope(repo.as_deref(), *issue)),
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
        // #6596: the credential-gap symptom (`Could not resolve to a
        // Repository`) surfaces exactly here, and used to be swallowed whole.
        log_gh_failure_once("pr list", workspace_root, &stderr_head(&output.stderr));
        return None;
    }
    log_gh_recovery_once("pr list", workspace_root);
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
    let mut cmd = tokio::process::Command::new(gh_bin);
    cmd.arg("pr")
        .arg("list")
        .arg("--head")
        .arg(branch)
        .arg("--state")
        .arg("merged")
        .arg("--json")
        .arg(fields)
        .arg("--limit")
        .arg("1")
        .current_dir(workspace_root);
    apply_owner_gh_config(&mut cmd, workspace_root);
    let run = cmd.output();
    match tokio::time::timeout(MERGE_CHECK_TIMEOUT, run).await {
        Ok(Ok(output)) => Some(output),
        // Neither of these carries a `stderr` to quote: the process either never
        // started or never finished. Say which, once per workspace (#6596).
        Ok(Err(err)) => {
            log_gh_failure_once("pr list", workspace_root, &format!("could not run gh: {err}"));
            None
        }
        Err(_) => {
            log_gh_failure_once(
                "pr list",
                workspace_root,
                &format!("timed out after {}s", MERGE_CHECK_TIMEOUT.as_secs()),
            );
            None
        }
    }
}

/// Point a sink-side `gh` child at the credential scoped to `workspace_root`'s
/// owner (#6596). The daemon process itself runs under the **primary**
/// installation's `GH_CONFIG_DIR` (#4458), which cannot see a *private* repo
/// owned by another org — so before this, every lookup in this module
/// (merge verification, slug/visibility resolution, the reconciliation pass,
/// the dispatch-line title) failed with `Could not resolve to a Repository` on
/// exactly those workspaces and, because every failure here is silent by
/// contract, their completions were permanently and invisibly absent from the
/// feed. A total no-op for single-owner fleets and the root owner's own repos,
/// same as the three dispatch paths this mirrors (#5401/#5508/#5522/#6529).
fn apply_owner_gh_config(cmd: &mut tokio::process::Command, workspace_root: &Path) {
    crate::credential_preflight::apply_gh_config_for_root_async(cmd, workspace_root);
}

/// One-per-`(call, workspace)` record of an already-warned forge failure.
/// Every `gh` failure in this module degrades silently by design, which made
/// the #6596 credential gap a half-day diagnosis: nothing anywhere said the
/// lookups were failing. This keeps the silence at the *behavior* level while
/// still leaving one breadcrumb per workspace in the log.
fn warned_gh_failures() -> &'static Mutex<std::collections::HashSet<(String, String)>> {
    static WARNED: OnceLock<Mutex<std::collections::HashSet<(String, String)>>> = OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Whether this is the first observed failure of `call` in `workspace_root`
/// (and record it). A poisoned mutex degrades to "not the first", so a lock
/// failure can only ever cost a log line — never a panic in the sink.
fn first_gh_failure_for(call: &str, workspace_root: &Path) -> bool {
    let key = (call.to_owned(), workspace_root.display().to_string());
    warned_gh_failures()
        .lock()
        .map(|mut seen| seen.insert(key))
        .unwrap_or(false)
}

/// Clear a previously-recorded failure for `(call, workspace_root)` — the
/// mirror of [`first_gh_failure_for`], called on a **success** rather than a
/// failure (#6619). Returns whether an entry was actually removed, so a
/// caller can tell "this workspace just recovered" from "this workspace was
/// already healthy" without re-deriving state itself — that distinction is
/// what lets [`log_gh_recovery_once`] log at most once per recovery instead
/// of once per successful call. A poisoned mutex degrades to "nothing
/// removed" — the same fail-safe posture [`first_gh_failure_for`] takes on
/// the failure side, never a panic in the sink.
fn clear_gh_failure_for(call: &str, workspace_root: &Path) -> bool {
    let key = (call.to_owned(), workspace_root.display().to_string());
    warned_gh_failures()
        .lock()
        .map(|mut seen| seen.remove(&key))
        .unwrap_or(false)
}

/// A short, single-line, length-capped excerpt of a `gh` stderr, safe to put in
/// a log line (`gh` errors are one line in practice, but a paginated/verbose
/// failure must not dump a screenful into the daemon log).
fn stderr_head(stderr: &[u8]) -> String {
    const MAX: usize = 200;
    let text = String::from_utf8_lossy(stderr);
    let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return "no stderr".to_owned();
    };
    if line.chars().count() <= MAX {
        return line.to_owned();
    }
    let head: String = line.chars().take(MAX).collect();
    format!("{head}…")
}

/// Log a forge-lookup failure **once per `(call, workspace)`** at `warn`, and
/// at `debug` on every repeat (#6596). The reconciliation pass revisits every
/// workspace on a fixed cadence, so an unconditional `warn` on a persistently
/// unauthorized workspace would be a log flood; a one-shot warn plus
/// debug-level repeats is diagnosable without being noisy.
fn log_gh_failure_once(call: &str, workspace_root: &Path, detail: &str) {
    let root = workspace_root.display();
    if first_gh_failure_for(call, workspace_root) {
        log::warn!(
            "safehouse: `gh {call}` failed in {root} ({detail}); \
             narration for this workspace degrades silently — if this is a \
             private repo owned by another org, check its per-owner \
             GH_CONFIG_DIR (further failures here log at debug)"
        );
    } else {
        log::debug!("safehouse: `gh {call}` failed again in {root} ({detail})");
    }
}

/// Log a forge-lookup **recovery** once a `(call, workspace)` pair that
/// previously warned via [`log_gh_failure_once`] succeeds again (#6619) —
/// mirrors the existing `"safehouse: narration accepted again; resuming"`
/// pattern the narration-send path already uses for the same "it was broken,
/// now it isn't" transition. A no-op, silent call on every ordinary success:
/// only a workspace that actually had a recorded failure to clear logs
/// anything here, so a healthy workspace never gains a log line per
/// completion.
fn log_gh_recovery_once(call: &str, workspace_root: &Path) {
    if clear_gh_failure_for(call, workspace_root) {
        log::info!(
            "safehouse: `gh {call}` succeeded again in {}; narration for this workspace resumes",
            workspace_root.display()
        );
    }
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

/// The forge identity of a managed workspace, as one `gh repo view` answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoIdentity {
    /// Forge `owner/repo` slug (`completion-v1` `repo`).
    slug: String,
    /// Repo visibility (#6596), from the same call's `isPrivate`. `None` when
    /// a `gh` too old to know the field forced the fallback query — see
    /// [`RepoVisibility`] for what a consumer must do with an absent value.
    visibility: Option<RepoVisibility>,
}

/// Whether a repo is world-readable, as published in `completion-v1`
/// `meta.visibility` (#6596).
///
/// The completion egress mirrors well-formed `completion` envelopes to a
/// **public** sink, and had no repo-visibility gate at all: the only thing
/// keeping a private repo's PR titles off a public page was the credential gap
/// this same issue fixes. Tagging each envelope is the producer half of that
/// gate; the consumer half (safehoused's egress) must treat anything that is
/// not an explicit `"public"` — including an absent key — as **not
/// egressable**, so an old `gh`, a failed lookup, or an older producer fails
/// closed rather than leaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoVisibility {
    Public,
    Private,
}

impl RepoVisibility {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }
}

/// `gh repo view --json` fields the completion path requests: the forge slug
/// (#4426) plus the visibility flag the egress gate keys on (#6596).
const REPO_VIEW_FIELDS: &str = "nameWithOwner,isPrivate";

/// The pre-#6596 field set, retried only when a `gh` too old to know
/// `isPrivate` rejects [`REPO_VIEW_FIELDS`] outright — the same
/// degrade-a-field-not-the-completion contract [`MERGED_PR_FIELDS_BASE`] has.
const REPO_VIEW_FIELDS_BASE: &str = "nameWithOwner";

/// Best-effort `gh repo view --json nameWithOwner,isPrivate` lookup for the
/// forge `owner/repo` slug (#4426) and the repo's visibility (#6596). The
/// `completion-v1` `repo` field is the forge identity the feed links and
/// displays — deliberately **not** the path-basename narration convention
/// (#4201) used for `task_id`/body prefixes, which is a local directory name
/// with no forge meaning.
async fn fetch_repo_identity(workspace_root: &Path) -> Option<RepoIdentity> {
    let gh_bin = env_nonempty(GH_BIN_ENV).unwrap_or_else(|| "gh".to_owned());
    let mut output = run_repo_view_query(&gh_bin, REPO_VIEW_FIELDS, workspace_root).await?;
    if !output.status.success() && rejects_unknown_json_field(&output.stderr) {
        // A `gh` too old to know `isPrivate` rejects the *whole* request, which
        // would cost the slug and with it every completion for this workspace.
        // Retry the pre-#6596 field set: the completion still publishes, just
        // without a visibility tag (which a correct egress fails closed on).
        log::debug!(
            "safehouse: gh rejected the repo-visibility field; \
             retrying with the base field set (completion will omit visibility)"
        );
        output = run_repo_view_query(&gh_bin, REPO_VIEW_FIELDS_BASE, workspace_root).await?;
    }
    if !output.status.success() {
        log_gh_failure_once("repo view", workspace_root, &stderr_head(&output.stderr));
        return None;
    }
    log_gh_recovery_once("repo view", workspace_root);
    let parsed: Value = serde_json::from_slice(&output.stdout).ok()?;
    let slug = parsed.get("nameWithOwner")?.as_str()?.trim().to_owned();
    if !valid_repo_slug(&slug) {
        return None;
    }
    // Absent/wrongly-typed ⇒ unknown, never "assume public": the whole point of
    // the tag is that a consumer can refuse to publish what it cannot confirm.
    let visibility = parsed
        .get("isPrivate")
        .and_then(Value::as_bool)
        .map(|private| {
            if private {
                RepoVisibility::Private
            } else {
                RepoVisibility::Public
            }
        });
    Some(RepoIdentity { slug, visibility })
}

/// One `gh repo view --json <fields>` invocation, bounded by
/// [`MERGE_CHECK_TIMEOUT`] and carrying the workspace owner's credential
/// (#6596). `None` means the process could not be run or did not finish in
/// time; a nonzero exit is returned so the caller can inspect `stderr`.
async fn run_repo_view_query(
    gh_bin: &str,
    fields: &str,
    workspace_root: &Path,
) -> Option<std::process::Output> {
    let mut cmd = tokio::process::Command::new(gh_bin);
    cmd.arg("repo")
        .arg("view")
        .arg("--json")
        .arg(fields)
        .current_dir(workspace_root);
    apply_owner_gh_config(&mut cmd, workspace_root);
    let run = cmd.output();
    match tokio::time::timeout(MERGE_CHECK_TIMEOUT, run).await {
        Ok(Ok(output)) => Some(output),
        Ok(Err(err)) => {
            log_gh_failure_once("repo view", workspace_root, &format!("could not run gh: {err}"));
            None
        }
        Err(_) => {
            log_gh_failure_once(
                "repo view",
                workspace_root,
                &format!("timed out after {}s", MERGE_CHECK_TIMEOUT.as_secs()),
            );
            None
        }
    }
}

/// [`fetch_repo_identity`] with a process-lifetime cache keyed by workspace
/// root: one `gh` call per managed workspace rather than one per sweep
/// completion (a repo's slug does not change while the daemon runs).
///
/// **Caveat, deliberately accepted**: the cached `visibility` is equally
/// process-lifetime, so flipping a repo public → private mid-run keeps the
/// stale `public` tag until the daemon restarts. That window is bounded by the
/// daemon's own lifetime and is the same staleness the slug has always had;
/// tightening it (a TTL, a re-probe per completion) trades a forge round-trip
/// per completion for it and belongs to whoever needs that guarantee.
async fn fetch_repo_identity_cached(
    cache: &mut HashMap<String, RepoIdentity>,
    workspace_root: &str,
) -> Option<RepoIdentity> {
    if let Some(identity) = cache.get(workspace_root) {
        return Some(identity.clone());
    }
    let identity = fetch_repo_identity(Path::new(workspace_root)).await?;
    cache.insert(workspace_root.to_owned(), identity.clone());
    Some(identity)
}

/// Best-effort per-issue token total from the activity DB's per-issue cost
/// rollup ([`ActivityDb::get_cost_by_issue`]): `sum(input + output)`. The DB is
/// already in-process, so this needs no forge call and no new accounting.
///
/// **Attribution is imperfect, by explicit operator decision** — for a cost
/// *trend*, imperfect-but-consistent beats absent. Three known limits,
/// documented here rather than papered over:
///
/// 1. **Not repo-qualified.** The activity DB's forge-correlation table keys on
///    a bare issue number with no repo column, so a daemon managing several
///    repos conflates identical issue numbers across them.
/// 2. **Only as good as the prompt↔usage linkage.** The rollup joins recorded
///    token samples to an issue through `agent_inputs`; samples recorded without
///    that link contribute nothing, so the figure is a **floor**, not a full
///    accounting.
/// 3. **Unreachable from the dispatch path (#4699).** The `resource_usage` and
///    `prompt_github` rows this rollup joins are written from exactly one place,
///    the IPC `GetTerminalOutput` handler scraping a *managed terminal's*
///    scrollback. `dispatch_sweep` spawns a detached `claude -p` and reaps the
///    OS process — no `SendInput`/`GetTerminalOutput` round trips — so on a
///    dispatch-driven host both tables stay empty forever and this returns
///    `None` unconditionally. That is why [`fetch_issue_tokens`] falls back to
///    the on-disk transcripts; this remains first only because it is the cheaper
///    lookup on hosts that *do* drive managed terminals.
///
/// Every failure — no DB handle, a poisoned mutex, a query error, a timeout, an
/// empty rollup — degrades to `None`. A zero total is likewise indistinguishable
/// from "accounting had nothing" and is filtered out downstream by
/// [`CompletionMeta::to_meta_value`], so no bogus `0` reaches the feed.
async fn fetch_activity_db_tokens(
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
        .filter(|total| *total > 0)
}

/// Whether the on-disk transcript fallback is enabled. Opt **out** with
/// `LOOM_SAFEHOUSE_TRANSCRIPT_TOKENS=0` (also `false`/`off`/`no`) on a host
/// where scanning the Claude Code project directory is unwanted; any other
/// value, or the variable being unset, leaves it on.
fn transcript_tokens_enabled() -> bool {
    match std::env::var("LOOM_SAFEHOUSE_TRANSCRIPT_TOKENS") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

/// Per-issue token total from the sweep's own Claude Code transcripts — the
/// source that the daemon **dispatch** path actually populates (#4699).
///
/// Delegates to [`crate::transcript_tokens`] (see that module for how a sweep's
/// session is located and why the total includes cache tokens). Runs on the
/// blocking pool under the same [`TOKEN_LOOKUP_TIMEOUT`] as the DB lookup, so a
/// slow or pathological project directory can never delay a completion by more
/// than the lookup budget; a timeout degrades to `None` like every other
/// failure.
async fn fetch_transcript_tokens(
    workspace_root: &str,
    issue: u32,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Option<u64> {
    if !transcript_tokens_enabled() {
        return None;
    }
    let projects = crate::transcript_tokens::claude_projects_dir()?;
    let root = PathBuf::from(workspace_root);
    let scan = tokio::task::spawn_blocking(move || {
        crate::transcript_tokens::sum_sweep_tokens(&projects, &root, issue, Some(window))
    });
    tokio::time::timeout(TOKEN_LOOKUP_TIMEOUT, scan)
        .await
        .ok()?
        .ok()?
}

/// Best-effort per-issue token total for a `completion` envelope (#4497,
/// #4699): the activity DB rollup when the host has one, else the sweep's
/// on-disk transcripts.
///
/// The two sources answer with different fidelity — the DB reports
/// `input + output`, the transcripts report all four usage counters (cache reads
/// included, which dominate a sweep both by volume and by cost). The DB is tried
/// first purely because it is cheaper; on a dispatch-driven host it never
/// answers at all (see [`fetch_activity_db_tokens`] limit 3), so in practice the
/// transcript figure is what the fleet feed publishes. Both degrade to `None`
/// independently, and a `None` omits the key rather than publishing a
/// misleading `0`.
async fn fetch_issue_tokens(
    activity_db: Option<&Arc<Mutex<ActivityDb>>>,
    issue: u32,
    workspace_root: &str,
    window: (DateTime<Utc>, DateTime<Utc>),
) -> Option<u64> {
    if let Some(total) = fetch_activity_db_tokens(activity_db, issue).await {
        return Some(total);
    }
    fetch_transcript_tokens(workspace_root, issue, window).await
}

/// Bundles what [`build_and_narrate_completion`] needs to consult and update
/// the **fleet-wide** completion dedup (Issue #6352), on top of the
/// per-host-only `already_narrated`/persisted-file dedup that already existed
/// (#4426/#4583): this host's own identity for the outbound `Completed` ad,
/// the shared [`PeerClaimView`] to check/observe against, and the same
/// outbound channel [`run_coordination`] already drains for
/// `Advertise`/`Retract` ads (Option 1 from the issue's curation — reusing
/// the existing cross-host soft-claim primitive rather than a new socket or
/// protocol amendment).
///
/// Constructed once per [`run_sink`] invocation from the same
/// `Arc<Mutex<PeerClaimView>>` + `mpsc::Sender<ClaimAd>` pair
/// [`crate::workspace_pool::WorkspacePool::start_peer_coordination`] already
/// hands to [`crate::sweep_registry::SweepRegistry`] for dispatch-side
/// claims — see [`crate::workspace_pool::WorkspacePool::start_safehouse_narration`].
/// `None` end-to-end (no coordination established, e.g. `safehouse.enabled`
/// false) degrades every check below to "no peer info available", which is
/// exactly the pre-#6352 per-host-only behavior — byte-for-byte unchanged
/// when peer coordination is not running.
#[derive(Clone)]
pub struct PeerCompletionHandle {
    host: String,
    pid: u32,
    publisher: tokio::sync::mpsc::Sender<ClaimAd>,
    view: Arc<Mutex<PeerClaimView>>,
}

impl PeerCompletionHandle {
    #[must_use]
    pub fn new(
        publisher: tokio::sync::mpsc::Sender<ClaimAd>,
        view: Arc<Mutex<PeerClaimView>>,
    ) -> Self {
        Self {
            host: crate::sweep_registry::host_identity(),
            pid: std::process::id(),
            publisher,
            view,
        }
    }

    /// Whether a peer has already narrated `(repo, issue, pr)`'s completion,
    /// per the shared view, right now. A poisoned mutex degrades to "no peer
    /// info" (`false`) rather than propagating a panic into the narration
    /// sink.
    ///
    /// Keyed on the **merged PR number**, not just the issue (Issue #6062):
    /// an issue can legitimately merge more than one PR over its lifetime
    /// (partial increments, `Part of #N`), and each one is a distinct
    /// completion worth narrating once. Keying on issue alone (the pre-#6062
    /// shape) would permanently suppress every merge after the first for a
    /// still-open issue, fleet-wide, for the rest of the completion TTL.
    fn already_narrated_by_peer(&self, repo: &str, issue: u32, pr: u32) -> bool {
        match self.view.lock() {
            Ok(view) => view.is_narrated_at(repo, issue, pr, Instant::now()),
            Err(poisoned) => poisoned
                .into_inner()
                .is_narrated_at(repo, issue, pr, Instant::now()),
        }
    }

    /// Publish this host's own `Completed` ad for `(repo, issue, pr)`.
    /// Fire-and-forget / fail-open, mirroring
    /// [`crate::sweep_registry::SweepRegistry::publish_peer_claim`]'s
    /// contract exactly: a dropped ad (channel `Full`/`Closed`) never blocks
    /// or unwinds the narration that already succeeded locally — the local
    /// `completion` envelope has already been built and sent by the time
    /// this is called.
    fn publish_completed(&self, repo: &str, issue: u32, pr: u32) {
        let ad = ClaimAd::completed(
            issue,
            repo.to_owned(),
            self.host.clone(),
            self.pid,
            Utc::now().to_rfc3339(),
            pr,
        );
        if let Err(e) = self.publisher.try_send(ad) {
            log::debug!(
                "safehouse: peer-completion advertisement for issue #{issue} (PR #{pr}) dropped \
                 ({e}); narration unaffected (#6352)"
            );
        }
    }
}

/// The Option-B emit point (#4426): on a `SweepExited`, verify against forge
/// truth that the sweep's PR actually merged and, if so, build the
/// public-feed `completion` envelope.
///
/// Runs for **every** exit code, not just `0`: a sweep can land its PR and
/// still exit nonzero on post-merge cleanup, and the merge — not the exit
/// status — is what the feed reports. `already_narrated` keeps that to one
/// completion per `(workspace, issue, merged-PR-number)` for the life of the
/// daemon, so a resumed sweep's second `SweepExited` does not double-post
/// (downstream ingest is additionally idempotent on `event_id`, which covers
/// daemon restarts).
///
/// The dedup key is the **merged PR number**, not the issue alone (Issue
/// #6062): unlike a resumed sweep re-observing the same merge, an issue can
/// legitimately merge more than one PR over its lifetime (partial
/// increments, `Part of #N`), and the second merge is a distinct completion
/// that must still be narrated, not silently swallowed by an issue-only key.
/// Because the PR number is only known once [`fetch_merged_pr`] answers, this
/// function always re-runs that (cheap, bounded) lookup rather than
/// short-circuiting on `issue` alone before it — see
/// [`build_and_narrate_completion`] for where the authoritative
/// PR-number-keyed check happens.
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
    slug_cache: &mut HashMap<String, RepoIdentity>,
    already_narrated: &mut std::collections::HashSet<(String, u32, u32)>,
    activity_db: Option<&Arc<Mutex<ActivityDb>>>,
    peer_completions: Option<&PeerCompletionHandle>,
) -> Option<Envelope> {
    let merged = fetch_merged_pr(Path::new(workspace_root), issue).await?;
    build_and_narrate_completion(
        persona,
        workspace_root,
        issue,
        merged,
        duration_sec,
        exited_at,
        slug_cache,
        already_narrated,
        activity_db,
        peer_completions,
    )
    .await
}

/// Shared envelope-build/dedup-insert core behind **both** completion trigger
/// paths (issue #4583): the `SweepExited` emit point above
/// ([`completion_for_exit`]) and the periodic merge-reconciliation pass below
/// ([`reconcile_recent_merges`]), which exists precisely because a
/// champion-tick merge — landing well after the sweep process (and its one
/// `SweepExited`) already exited — is never observed by the first path alone.
///
/// Every caller must have already confirmed the merge (a [`MergedPr`] in
/// hand); this only re-checks and updates `already_narrated`, so the exactly-
/// once invariant holds identically regardless of which path found the merge
/// first. `duration_sec`/`exited_at` are caller-supplied rather than derived
/// here because the two callers have different clocks available: a live
/// sweep's reaper clock for [`completion_for_exit`], the forge's
/// `createdAt`/`mergedAt` pair (no sweep clock exists) for
/// [`reconcile_recent_merges`].
///
/// The dedup key is `(workspace, issue, merged.number)` (Issue #6062), not
/// `(workspace, issue)` alone — see [`completion_for_exit`]'s doc comment for
/// why: an issue can merge more than one PR over its lifetime, and each
/// merged PR is its own completion.
#[allow(clippy::too_many_arguments)]
async fn build_and_narrate_completion(
    persona: &str,
    workspace_root: &str,
    issue: u32,
    merged: MergedPr,
    duration_sec: i64,
    exited_at: DateTime<Utc>,
    slug_cache: &mut HashMap<String, RepoIdentity>,
    already_narrated: &mut std::collections::HashSet<(String, u32, u32)>,
    activity_db: Option<&Arc<Mutex<ActivityDb>>>,
    peer_completions: Option<&PeerCompletionHandle>,
) -> Option<Envelope> {
    let key = (workspace_root.to_owned(), issue, merged.number);
    if already_narrated.contains(&key) {
        return None;
    }
    // Issue #6352 (keying tightened by #6062): consult the fleet-wide
    // completion dedup before doing any forge/token work. A peer host that
    // already narrated this `(repo, issue, pr)` completion means THIS host
    // must not re-narrate it — adopt the peer's outcome as local dedup state
    // (so this host's own future `SweepExited`/reconciliation passes also
    // short-circuit here) instead of posting a second envelope for the same
    // merge. `None` here (no peer coordination established) degrades
    // byte-for-byte to the pre-#6352 per-host-only behavior.
    if let Some(handle) = peer_completions {
        let repo_key = crate::peer_claims::repo_slug(Path::new(workspace_root));
        if handle.already_narrated_by_peer(&repo_key, issue, merged.number) {
            already_narrated.insert(key);
            return None;
        }
    }
    let identity = fetch_repo_identity_cached(slug_cache, workspace_root).await?;

    // Timing comes from the one clock the caller had available, so the pair is
    // always self-consistent — mixing clocks across callers could render a
    // `completed_at` before `started_at`.
    let started_at = exited_at - chrono::Duration::seconds(duration_sec.max(0));
    // Only reached once the merge is confirmed, so a completion is never delayed
    // by a token lookup it would not have published. The window narrows the
    // transcript fallback's candidate set to sessions that overlap this
    // completion, so it must be computed before the lookup.
    let tokens =
        fetch_issue_tokens(activity_db, issue, workspace_root, (started_at, exited_at)).await;
    // Issue #8507: what this sweep actually launched on, read off its own
    // `# LOOM_LAUNCH` record. Resolved BEFORE the per-model lookup because it
    // also selects that lookup's source — and published in its own right, so a
    // non-Claude completion is labelled even when no usage numbers exist.
    // `None` for a Claude/legacy spawn (writes no record).
    let runtime_attribution =
        crate::launch_record::sweep_runtime_attribution(Path::new(workspace_root), issue);
    // Per-model breakdown (#5740) — a second, independent lookup: it shares
    // the completion's window but not `tokens`' activity-DB fast path, since
    // that rollup has no per-model granularity to offer.
    let tokens_by_model = completion_runtime::fetch_tokens_by_model(
        runtime_attribution.as_ref().map(|r| r.runtime.as_str()),
        workspace_root,
        issue,
        (started_at, exited_at),
    )
    .await;
    let meta = CompletionMeta {
        agent: persona.to_owned(),
        repo_slug: identity.slug,
        pr_url: merged.url,
        result: CompletionResult::Success,
        started_at: started_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        completed_at: exited_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        issue: Some(issue),
        // Best-effort, knowingly imperfect attribution (#4497) — see
        // `fetch_issue_tokens`. Absent/zero ⇒ the key is omitted, never guessed.
        tokens,
        tokens_by_model,
        title: merged.title,
        additions: merged.additions,
        deletions: merged.deletions,
        // #6596: the public-feed egress gate's input. Unknown ⇒ omitted, and a
        // correct consumer then declines to publish rather than guessing.
        visibility: identity.visibility,
        // #8507: the runtime/provider/profile badge inputs, straight from the
        // launch record. All three absent ⇒ byte-identical to a pre-#8507
        // payload, which is the Claude case.
        runtime: runtime_attribution.as_ref().map(|r| r.runtime.clone()),
        provider: runtime_attribution
            .as_ref()
            .and_then(|r| r.provider.clone()),
        profile: runtime_attribution.and_then(|r| r.profile),
    };
    match build_completion_envelope(Some(workspace_root), issue, merged.number, duration_sec, &meta)
    {
        Ok(envelope) => {
            let pr = key.2;
            already_narrated.insert(key);
            // Issue #6352: this host is the one narrating — tell peers so
            // they don't also narrate it. Fire-and-forget; a dropped ad
            // never unwinds a narration that already succeeded locally.
            if let Some(handle) = peer_completions {
                let repo_key = crate::peer_claims::repo_slug(Path::new(workspace_root));
                handle.publish_completed(&repo_key, issue, pr);
            }
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
// Periodic merge reconciliation (#4583) — catches champion-tick merges
// ============================================================================

/// One row from the bulk reconciliation query: everything
/// [`build_and_narrate_completion`] needs, plus the PR's own creation time used
/// as a `started_at` proxy (there is by definition no live sweep clock at
/// reconciliation time).
struct ReconciledMergedPr {
    issue: u32,
    merged: MergedPr,
    created_at: DateTime<Utc>,
    merged_at: DateTime<Utc>,
}

/// `gh pr list --json` fields the reconciliation pass requests. Distinct from
/// [`MERGED_PR_FIELDS`] (the per-issue, branch-filtered lookup): this is a
/// **bulk**, unfiltered-by-branch query, so it additionally needs `headRefName`
/// (to recover the issue number via
/// [`crate::worktree_ops::naming::issue_from_branch`]) and `createdAt` (the
/// `started_at` proxy described above).
const RECONCILE_PR_FIELDS: &str =
    "number,headRefName,url,mergedAt,createdAt,title,additions,deletions";

/// How many of the most-recently-merged PRs one reconciliation pass inspects
/// per workspace. A repo merging more than this between two reconciliation
/// ticks would need a smaller [`reconcile_interval`], not a larger limit.
const RECONCILE_PR_LIMIT: u32 = 30;

/// Test/operator override for the reconciliation lookback window, in seconds.
const RECONCILE_MAX_AGE_ENV: &str = "LOOM_SAFEHOUSE_RECONCILE_MAX_AGE_SECS";

/// How far back a reconciliation pass will narrate a merge it has never seen
/// (issue #4583). [`RECONCILE_PR_LIMIT`] alone bounds a *burst* but not its
/// *staleness*: the very first pass on a host that has never persisted a dedup
/// set — a fresh install, an upgrade to this version, or a lost/corrupt
/// completions file — would otherwise backfill the public feed with the last 30
/// merges regardless of age, which in a low-traffic workspace means narrating
/// months-old PRs as if they just landed. Seven days is well beyond any
/// plausible daemon outage (the case AC3 asks us to recover), so a merge that
/// happens while the daemon is down is still picked up on restart, while
/// genuinely ancient history stays out of the feed.
const DEFAULT_RECONCILE_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;

fn reconcile_max_age() -> chrono::Duration {
    let secs = env_nonempty(RECONCILE_MAX_AGE_ENV)
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|s| *s >= 0)
        .unwrap_or(DEFAULT_RECONCILE_MAX_AGE_SECS);
    chrono::Duration::seconds(secs)
}

/// Best-effort bulk `gh pr list --state merged` lookup across **all** recently
/// merged PRs in `workspace_root` (issue #4583) — unlike [`fetch_merged_pr`]
/// this is not filtered to one issue's branch, which is exactly what lets it
/// discover a merge the branch-scoped, `SweepExited`-triggered lookup never
/// ran for (no sweep, no trigger). Rows merged longer than
/// [`reconcile_max_age`] ago are dropped here, so a first-ever pass cannot
/// backfill stale history onto the feed. Every failure — missing `gh`, no
/// network, unauthenticated, timeout, malformed JSON, an unparsable row —
/// degrades that row (or the whole call) to being silently skipped; a
/// best-effort reconciliation pass must never panic or block the sink.
async fn fetch_recent_merged_prs(workspace_root: &Path) -> Vec<ReconciledMergedPr> {
    let gh_bin = env_nonempty(GH_BIN_ENV).unwrap_or_else(|| "gh".to_owned());
    let mut cmd = tokio::process::Command::new(&gh_bin);
    cmd.arg("pr")
        .arg("list")
        .arg("--state")
        .arg("merged")
        .arg("--json")
        .arg(RECONCILE_PR_FIELDS)
        .arg("--limit")
        .arg(RECONCILE_PR_LIMIT.to_string())
        .current_dir(workspace_root);
    apply_owner_gh_config(&mut cmd, workspace_root);
    let run = cmd.output();
    let output = match tokio::time::timeout(MERGE_CHECK_TIMEOUT, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            log_gh_failure_once(
                "pr list (reconcile)",
                workspace_root,
                &format!("could not run gh: {err}"),
            );
            return Vec::new();
        }
        Err(_) => {
            log_gh_failure_once(
                "pr list (reconcile)",
                workspace_root,
                &format!("timed out after {}s", MERGE_CHECK_TIMEOUT.as_secs()),
            );
            return Vec::new();
        }
    };
    if !output.status.success() {
        // The #6596 symptom for the reconciliation pass — a whole workspace's
        // merges silently absent from the feed, with nothing in the log.
        log_gh_failure_once("pr list (reconcile)", workspace_root, &stderr_head(&output.stderr));
        return Vec::new();
    }
    log_gh_recovery_once("pr list (reconcile)", workspace_root);
    let Ok(rows) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };
    let Some(array) = rows.as_array() else {
        return Vec::new();
    };
    let cutoff = Utc::now() - reconcile_max_age();
    array
        .iter()
        .filter_map(|row| {
            let head_ref = row.get("headRefName")?.as_str()?;
            let issue = crate::worktree_ops::naming::issue_from_branch(head_ref)?;
            let merged_at_str = row
                .get("mergedAt")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())?;
            let merged_at = DateTime::parse_from_rfc3339(merged_at_str)
                .ok()?
                .with_timezone(&Utc);
            // Outside the lookback window: never narrated, and now too old to
            // start (see `DEFAULT_RECONCILE_MAX_AGE_SECS`).
            if merged_at < cutoff {
                return None;
            }
            let created_at = row
                .get("createdAt")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                // A missing/unparsable `createdAt` degrades to a zero-duration
                // proxy rather than losing the whole row — the feed cares far
                // more about the completion existing than about its duration
                // figure being exact for a merge with no sweep clock anyway.
                .unwrap_or(merged_at);
            let number = u32::try_from(row.get("number")?.as_u64()?).ok()?;
            let url = row.get("url")?.as_str()?.to_owned();
            if url.is_empty() {
                return None;
            }
            let title = row
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(ToOwned::to_owned);
            let additions = row.get("additions").and_then(Value::as_u64);
            let deletions = row.get("deletions").and_then(Value::as_u64);
            Some(ReconciledMergedPr {
                issue,
                merged: MergedPr {
                    number,
                    url,
                    title,
                    additions,
                    deletions,
                },
                created_at,
                merged_at,
            })
        })
        .collect()
}

/// The periodic reconciliation pass itself (issue #4583): bulk-lists recently
/// merged PRs for `workspace_root` and narrates a completion for any
/// `(workspace, issue, merged-PR-number)` not already in `already_narrated` —
/// sharing that exact dedup set with [`completion_for_exit`]'s `SweepExited`
/// path is what keeps the two trigger paths from ever double-posting the same
/// merge, in either direction (whichever path observes the merge first wins;
/// the other becomes a no-op `contains` check). Keying on the PR number
/// (Issue #6062) rather than the issue alone also means a *different* merged
/// PR against the same still-open issue (a partial increment) is narrated
/// independently instead of being silently swallowed by the first merge's key.
///
/// This is the option the issue's curation explicitly favors over a new event
/// topic/IPC verb: it needs no change to the frozen v0.10.0 event taxonomy,
/// and — because it queries forge truth directly rather than depending on any
/// daemon-emitted event — it also naturally covers a merge that happened while
/// the daemon itself was down, **provided** the dedup set survives the
/// restart; see [`load_persisted_completed`] / [`persist_completed_best_effort`]
/// for that half of the story (the in-memory set alone does not survive one).
async fn reconcile_recent_merges(
    persona: &str,
    workspace_root: &str,
    slug_cache: &mut HashMap<String, RepoIdentity>,
    already_narrated: &mut std::collections::HashSet<(String, u32, u32)>,
    activity_db: Option<&Arc<Mutex<ActivityDb>>>,
    peer_completions: Option<&PeerCompletionHandle>,
) -> Vec<Envelope> {
    let rows = fetch_recent_merged_prs(Path::new(workspace_root)).await;
    // #6619: answers "did reconciliation run for repo X, and did it see
    // anything" from the log alone — independent of whether any row was new
    // enough to actually narrate below (that's a separate, per-row dedup
    // decision `build_and_narrate_completion` makes).
    log::debug!(
        "safehouse: reconcile visited {workspace_root}, {} rows within lookback window",
        rows.len()
    );
    let mut out = Vec::new();
    for row in rows {
        let duration_sec = (row.merged_at - row.created_at).num_seconds().max(0);
        if let Some(envelope) = build_and_narrate_completion(
            persona,
            workspace_root,
            row.issue,
            row.merged,
            duration_sec,
            row.merged_at,
            slug_cache,
            already_narrated,
            activity_db,
            peer_completions,
        )
        .await
        {
            out.push(envelope);
        }
    }
    out
}

/// Reconciliation-pass cadence (issue #4583). Test/operator override mirrors
/// the existing `GH_BIN_ENV` seam: milliseconds, so a test can drive a whole
/// reconciliation cycle without a real wall-clock wait. Unset/unparsable falls
/// back to [`DEFAULT_RECONCILE_INTERVAL`].
const RECONCILE_INTERVAL_ENV: &str = "LOOM_SAFEHOUSE_RECONCILE_INTERVAL_MS";

/// Production default cadence: frequent enough that a champion-tick merge
/// (production evidence: the fleet feed went dark for 2+ hours) is caught
/// within a few minutes, infrequent enough that a multi-workspace host issues
/// at most one bulk `gh pr list` per workspace every few minutes.
const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(300);

fn reconcile_interval() -> Duration {
    env_nonempty(RECONCILE_INTERVAL_ENV)
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_RECONCILE_INTERVAL)
}

/// Test/operator override for the persisted dedup file's path (issue #4583),
/// mirroring [`crate::workspace_registry::REGISTRY_PATH_ENV`]'s pattern.
const COMPLETIONS_PATH_ENV: &str = "LOOM_SAFEHOUSE_COMPLETIONS_PATH";

/// Resolve the on-disk path for the persisted `(workspace, issue)` completion
/// dedup set. `None` only when no home directory can be resolved at all (an
/// exotic environment) — reconciliation still runs, it just cannot survive a
/// daemon restart without double-checking already-narrated merges.
fn default_completions_path() -> Option<PathBuf> {
    if let Some(path) = env_nonempty(COMPLETIONS_PATH_ENV) {
        return Some(PathBuf::from(path));
    }
    Some(
        dirs::home_dir()?
            .join(".loom")
            .join("safehouse-completed.json"),
    )
}

/// Load the persisted dedup set (issue #4583 AC3: a merge occurring while the
/// daemon was down must be picked up after restart, which requires that
/// *already*-narrated merges are **not** re-narrated just because the
/// in-memory set reset). A missing/corrupt/unreadable file degrades to an
/// empty set — the same "narrate it (again), never lose it" tradeoff every
/// other best-effort lookup in this module makes, and no worse than the
/// pre-#4583 behavior (which never persisted at all).
///
/// Entries are `(workspace, issue, merged-PR-number)` triples as of Issue
/// #6062 (previously `(workspace, issue)` pairs). A file written by a
/// pre-#6062 binary fails to parse against the new shape and degrades to
/// "empty, treated as fresh" exactly like any other corrupt file — see
/// [`persisted_dedup_state_is_fresh`]'s doc comment for why that one-time
/// reset is safe (the seed-only first reconciliation pass never bursts a
/// backlog onto the feed).
fn load_persisted_completed(path: Option<&Path>) -> std::collections::HashSet<(String, u32, u32)> {
    let Some(path) = path else {
        return std::collections::HashSet::new();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return std::collections::HashSet::new();
    };
    serde_json::from_str::<Vec<(String, u32, u32)>>(&contents)
        .map(|triples| triples.into_iter().collect())
        .unwrap_or_default()
}

/// Whether the persisted dedup file represents "no reliable prior state" at
/// process startup (issue #4649): every case [`load_persisted_completed`]
/// already degrades to an empty set for — a missing file (fresh install, a
/// deleted/lost file), an unreadable file (permissions), or one that fails to
/// parse (corrupt) — all indistinguishable from "we have never reconciled
/// this host before". Left unaddressed, the very first reconciliation tick
/// for each workspace would then narrate every in-window merge at once (up
/// to [`RECONCILE_PR_LIMIT`]) as if they had all just happened.
///
/// A file that exists and parses successfully — even to a legitimately empty
/// set, e.g. a host with zero merges since the last restart — is **not**
/// fresh: reconciliation has already run at least once on this host, so no
/// seeding is needed and narration should proceed normally from tick one.
fn persisted_dedup_state_is_fresh(path: Option<&Path>) -> bool {
    let Some(path) = path else {
        return true;
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return true;
    };
    serde_json::from_str::<Vec<(String, u32, u32)>>(&contents).is_err()
}

/// Persist the dedup set atomically (temp file + rename, mirroring
/// [`crate::workspace_registry::WorkspaceRegistry::save`]). Best-effort: a
/// write failure (read-only home, disk full, permissions) is logged once at
/// `debug` and otherwise swallowed — the in-memory set stays authoritative for
/// the rest of this process's life either way, this only affects
/// restart-survival.
fn persist_completed_best_effort(
    path: Option<&Path>,
    completed: &std::collections::HashSet<(String, u32, u32)>,
) {
    let Some(path) = path else {
        return;
    };
    let triples: Vec<&(String, u32, u32)> = completed.iter().collect();
    let Ok(json) = serde_json::to_string(&triples) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            log::debug!(
                "safehouse: could not create completions dir {} ({err:#}); \
                 restart-recovery for merge reconciliation is degraded",
                parent.display()
            );
            return;
        }
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    if let Err(err) = std::fs::write(&tmp, json) {
        log::debug!(
            "safehouse: could not write completions file {} ({err:#}); \
             restart-recovery for merge reconciliation is degraded",
            tmp.display()
        );
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        log::debug!(
            "safehouse: could not install completions file {} ({err:#}); \
             restart-recovery for merge reconciliation is degraded",
            path.display()
        );
    }
}

/// Union of every workspace the reconciliation pass should scan (issue #4583):
/// every repo the sink has *observed* a stamped-`repo` event for
/// (`known_workspaces`, built up live as events flow through [`run_sink`]) plus
/// every repo in the machine-level [`crate::workspace_registry::WorkspaceRegistry`]
/// (which survives a daemon restart on disk, unlike `known_workspaces`) — the
/// union is what lets a champion-driven merge in a registered-but-otherwise-
/// quiet workspace still get reconciled promptly after a restart, before any
/// new sweep event re-populates `known_workspaces`.
///
/// A `BTreeSet` gives deterministic round-robin ordering (both for tests and
/// so a fixed number of workspaces cycle through predictably rather than by
/// hash-order).
fn reconciliation_targets(known_workspaces: &std::collections::HashSet<String>) -> Vec<String> {
    let mut targets: std::collections::BTreeSet<String> =
        known_workspaces.iter().cloned().collect();
    if let Ok(registry) = crate::workspace_registry::WorkspaceRegistry::load_default() {
        for root in registry.roots() {
            targets.insert(root.to_string_lossy().into_owned());
        }
    }
    targets.into_iter().collect()
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
///
/// `peer_completions` (Issue #6352) is the fleet-wide completion-dedup handle
/// — see [`PeerCompletionHandle`] — built by
/// [`crate::workspace_pool::WorkspacePool::start_safehouse_narration`] from
/// the same peer-claim coordination context dispatch already uses. `None`
/// (no peer coordination established) degrades byte-for-byte to the
/// pre-#6352 per-host-only dedup behavior.
#[must_use]
pub fn spawn_sink(
    config: SafehouseConfig,
    bus: &EventBus,
    runtime: &tokio::runtime::Handle,
    state: SharedSafehouseState,
    activity_db: Option<Arc<Mutex<ActivityDb>>>,
    peer_completions: Option<PeerCompletionHandle>,
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
            peer_completions,
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
    let mut cmd = tokio::process::Command::new(&gh_bin);
    cmd.arg("issue")
        .arg("view")
        .arg(issue.to_string())
        .arg("--json")
        .arg("title")
        .arg("--jq")
        .arg(".title")
        .current_dir(workspace_root);
    apply_owner_gh_config(&mut cmd, workspace_root);
    let run = cmd.output();
    let output = match tokio::time::timeout(TITLE_FETCH_TIMEOUT, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            log_gh_failure_once("issue view", workspace_root, &format!("could not run gh: {err}"));
            return None;
        }
        Err(_) => {
            log_gh_failure_once(
                "issue view",
                workspace_root,
                &format!("timed out after {}s", TITLE_FETCH_TIMEOUT.as_secs()),
            );
            return None;
        }
    };
    if !output.status.success() {
        log_gh_failure_once("issue view", workspace_root, &stderr_head(&output.stderr));
        return None;
    }
    log_gh_recovery_once("issue view", workspace_root);
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
    peer_completions: Option<PeerCompletionHandle>,
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
    // Forge `owner/repo` slugs, and the (workspace, issue, merged-PR-number)
    // triples already narrated as completions — both process-lifetime (#4426).
    let mut slug_cache: HashMap<String, RepoIdentity> = HashMap::new();
    // Persisted dedup (#4583 AC3): loaded once at startup so a merge already
    // narrated before a daemon restart is not re-posted just because the
    // process-lifetime set below reset to empty.
    let completions_path = default_completions_path();
    // Seed-only first pass (#4649): captured *before* the load below degrades
    // a missing/corrupt/unreadable file to an empty set indistinguishably
    // from "genuinely reconciled to zero" — this is the only place that
    // distinction is still observable.
    let dedup_state_was_fresh = persisted_dedup_state_is_fresh(completions_path.as_deref());
    let mut completed: std::collections::HashSet<(String, u32, u32)> =
        load_persisted_completed(completions_path.as_deref());
    // Every workspace root observed on a stamped-`repo` event, live (#4583).
    // Reconciliation also folds in the on-disk workspace registry (see
    // `reconciliation_targets`) so a registered-but-quiet workspace is still
    // scanned right after a restart, before any new event repopulates this set.
    let mut known_workspaces: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Workspaces that have already had their one-time seed-only reconciliation
    // pass (#4649, only consulted when `dedup_state_was_fresh`) — each
    // workspace's *first* reconciliation tick on a fresh host seeds
    // `completed` without narrating, so a backlog of in-window merges never
    // bursts onto the feed at once; every later tick for that workspace (or
    // any tick at all when the dedup file was not fresh) narrates normally.
    let mut seeded_fresh_workspaces: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Round-robins one workspace per reconciliation tick (see
    // `reconciliation_targets`) rather than sweeping every known workspace in
    // one tick, so one slow/offline `gh` cannot delay every other workspace's
    // narration behind it.
    let mut reconcile_cursor: usize = 0;
    let mut reconcile_timer = tokio::time::interval(reconcile_interval());
    reconcile_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick fires immediately; skip it here so a freshly-started sink
    // does not race its own first live events with an instant reconciliation
    // pass over an empty `known_workspaces`/registry.
    reconcile_timer.tick().await;
    // Dispatch-digest batching (issue #4217): admitted-dispatch events buffer
    // here for up to `dispatch_digest_window()` from the *first* buffered
    // dispatch, then flush as either today's single-dispatch `task` envelope
    // (exactly one buffered) or one grouped `digest` envelope (a burst) — see
    // `build_dispatch_digest_envelope`/`dispatch_envelope`. The far-future
    // initial deadline never fires on its own; the `if !pending_dispatches...`
    // guard on the flush branch below is what actually gates it, and the
    // guard starts `false` (buffer starts empty).
    let mut pending_dispatches: Vec<PendingDispatch> = Vec::new();
    let mut digest_seq: u64 = 0;
    let digest_sleep = tokio::time::sleep_until(
        tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 3600),
    );
    tokio::pin!(digest_sleep);

    loop {
        // One iteration narrates the envelope(s) produced by exactly one bus
        // event **or** one reconciliation tick — never both at once, so the
        // `completed` dedup set is only ever touched from one place at a time
        // (no cross-task race between the two trigger paths).
        let mut outbox: Vec<Envelope> = Vec::new();
        let mut narrated_repo: Option<String> = None;

        tokio::select! {
            biased;

            recv_result = subscription.recv() => {
                let event = match recv_result {
                    Ok(event) => event,
                    Err(RecvError::Closed) => {
                        log::debug!("safehouse: event bus closed; narration sink stopping");
                        break;
                    }
                    // Empty/Lagged are surfaced as events by `recv`; only Closed ends it.
                    Err(_) => continue,
                };

                if let Some(root) = event_repo(&event) {
                    known_workspaces.insert(root.to_owned());
                }

                // Dispatch-digest batching (issue #4217): an admitted-issue
                // dispatch does not narrate immediately — it joins the pending
                // batch, and the digest window's own flush (below) decides
                // whether it was a lone dispatch (unchanged single-`task`
                // behavior) or part of a burst (one `digest` root). Every
                // other narrated event is unaffected and still goes through
                // `event_to_envelope` exactly as before.
                if let Event::SweepGlobalDispatch {
                    kind: SweepKind::Issue(issue),
                    repo,
                    ..
                } = &event
                {
                    pending_dispatches.push(PendingDispatch { repo: repo.clone(), issue: *issue });
                    if pending_dispatches.len() == 1 {
                        digest_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + dispatch_digest_window());
                    }
                } else if let Some(envelope) = event_to_envelope(&event) {
                    // One bus event can narrate more than one envelope: a
                    // `SweepExited` whose PR merged emits its `ack` **and** the
                    // public-feed `completion` (#4426), in that order.
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
                        peer_completions.as_ref(),
                    )
                    .await
                    {
                        outbox.push(completion);
                        persist_completed_best_effort(completions_path.as_deref(), &completed);
                    }
                }

                narrated_repo = event_repo(&event).map(ToOwned::to_owned);
            }

            _ = reconcile_timer.tick() => {
                // Merge-reconciliation pass (#4583): covers a champion-tick
                // merge, which has no live sweep — and so no `SweepExited` —
                // to trigger the path above. One workspace per tick (see
                // `reconcile_cursor`'s doc comment).
                let targets = reconciliation_targets(&known_workspaces);
                if let Some(workspace_root) = (!targets.is_empty())
                    .then(|| targets[reconcile_cursor % targets.len()].clone())
                {
                    reconcile_cursor = reconcile_cursor.wrapping_add(1);
                    // Seed-only first pass (#4649): this workspace's very
                    // first reconciliation tick on a host with no reliable
                    // persisted dedup state seeds `completed` (and persists
                    // it) but drops the resulting envelopes instead of
                    // narrating them — otherwise every in-window merge (up
                    // to `RECONCILE_PR_LIMIT`) would burst onto the feed at
                    // once. `insert` returns `false` on a repeat visit, so
                    // this only ever suppresses one tick per workspace.
                    let seed_only = dedup_state_was_fresh
                        && seeded_fresh_workspaces.insert(workspace_root.clone());
                    let new_completions = reconcile_recent_merges(
                        &config.persona,
                        &workspace_root,
                        &mut slug_cache,
                        &mut completed,
                        activity_db.as_ref(),
                        peer_completions.as_ref(),
                    )
                    .await;
                    if !new_completions.is_empty() {
                        persist_completed_best_effort(completions_path.as_deref(), &completed);
                    }
                    if seed_only {
                        if !new_completions.is_empty() {
                            log::info!(
                                "safehouse: seeded {} completion(s) for {workspace_root} from a \
                                 fresh dedup file without narrating them (issue #4649)",
                                new_completions.len()
                            );
                        }
                    } else {
                        outbox.extend(new_completions);
                        narrated_repo = Some(workspace_root);
                    }
                }
            }

            () = &mut digest_sleep, if !pending_dispatches.is_empty() => {
                // The window that opened on the first buffered dispatch has
                // elapsed — flush now (#4217). Re-arming happens naturally:
                // the next dispatch to arrive (if any) sees an empty buffer
                // and starts a fresh window; until then this guard is `false`
                // and the (already-elapsed) sleep is never polled again.
                let batch = std::mem::take(&mut pending_dispatches);
                if let [PendingDispatch { repo, issue }] = batch.as_slice() {
                    let (repo, issue) = (repo.clone(), *issue);
                    let mut envelope = dispatch_envelope(repo.as_deref(), issue);
                    // Best-effort dispatch-title enrichment (issue #4201, AC3),
                    // preserved for the single-dispatch (no-burst) case only —
                    // a digest never fetches per-issue titles (would be N `gh`
                    // calls for one narration line).
                    if let Some(workspace_root) = repo.as_deref() {
                        if let Some(title) =
                            fetch_title_cached(&mut title_cache, workspace_root, issue).await
                        {
                            envelope.body.push_str(&format!(" — \"{title}\""));
                        }
                    }
                    narrated_repo = repo;
                    outbox.push(envelope);
                } else {
                    digest_seq = digest_seq.wrapping_add(1);
                    outbox.push(build_dispatch_digest_envelope(&batch, digest_seq));
                    // A digest spans (potentially) several repos and always
                    // routes via `AttentionClass::Signal`, which ignores the
                    // `repo` argument entirely (see `RoomRouter::resolve`) —
                    // `narrated_repo` stays `None`.
                }
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
        // routes, and each message lands in exactly one room. `narrated_repo` was
        // captured above, inside whichever `select!` branch produced this
        // iteration's `outbox` (a live event or a reconciliation tick).
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
        // #6249: safehoused's live push (`main.rs` `on_message`) carries the
        // message text at `envelope.body` — there is no top-level `body`.
        // Reading only the top level dropped 100% of inbound claims fleet-wide
        // (`received=0` while peers visibly advertised). Read the writer's real
        // shape first, keeping the top-level `body` as a fallback for any
        // legacy emitter of the flat shape.
        let Some(body) = event
            .pointer("/envelope/body")
            .and_then(Value::as_str)
            .or_else(|| event.get("body").and_then(Value::as_str))
        else {
            return;
        };
        let Some(ad) = ClaimAd::from_body_str(body) else {
            return; // not a claim (human chat, narration, malformed) — ignore
        };
        match self.view.lock() {
            Ok(mut view) => {
                let now = Instant::now();
                // Issue #6352: a `Completed` ad routes to the dedicated
                // completion-dedup map (its own TTL, no #6157
                // coordination-health side effects) rather than
                // `observe_at`'s dispatch-claims map — see
                // `PeerClaimView::observe_completion_at`'s doc comment.
                if ad.kind.is_filing_lock_lane() {
                    // Issue #6714: the issue-filing lane. Fold into the view's
                    // own single-purpose bookkeeping AND mirror to the
                    // machine-wide on-disk store, which is the only thing the
                    // shell filer (`create-issue.sh` -> `lib/filing-lock.sh`)
                    // can see — the daemon is the bridge between the
                    // cross-host transport and the cross-process lock.
                    if view.observe_filing_lock_at(&ad, now) {
                        mirror_filing_lock_to_disk(&ad);
                    }
                    for expired in view.prune_expired_filing_locks(now) {
                        clear_filing_lock_mirror(&expired);
                    }
                } else if ad.kind.is_cooldown_lane() {
                    // Issue #7477: fleet-wide no-op-cooldown / dispatch-backoff
                    // visibility. Each kind folds into its own single-purpose
                    // map (mirroring the filing-lock lane above) rather than
                    // `observe_at`'s dispatch-claims map — a cooldown/backoff
                    // window answers a different question ("should a peer
                    // re-dispatch this issue right now") than "is a sweep in
                    // flight".
                    match ad.kind {
                        crate::peer_claims::ClaimKind::NoopCooldownArmed => {
                            view.observe_noop_cooldown_at(&ad, now);
                            view.prune_expired_noop_cooldowns(now);
                        }
                        crate::peer_claims::ClaimKind::DispatchBackoffArmed => {
                            view.observe_dispatch_backoff_at(&ad, now);
                            view.prune_expired_dispatch_backoffs(now);
                        }
                        _ => {}
                    }
                } else if ad.kind.is_pool_hold_lane() {
                    // Issue #8001: fleet-wide token-pool exhaustion holds.
                    // Its own single-purpose map again — a pool hold answers
                    // "may ANY sweep spawn from this pool right now", which
                    // is neither "is issue #N in flight" nor a per-issue
                    // brake, and it is keyed by the pool's account
                    // fingerprint rather than by `(repo, issue)`.
                    view.observe_pool_hold_at(&ad, now);
                    view.prune_expired_pool_holds(now);
                } else if ad.kind == crate::peer_claims::ClaimKind::Completed {
                    view.observe_completion_at(&ad, now);
                    view.prune_expired_completions(now);
                } else {
                    view.observe_at(&ad, now);
                    // Opportunistically prune so a crashed peer's entries do
                    // not accumulate between work-finder queries.
                    view.prune_expired(now);
                }
            }
            Err(poisoned) => {
                log::error!("safehouse: peer-claim view mutex poisoned ({poisoned:?})");
            }
        }
    }
}

/// Mirror an observed peer [`crate::peer_claims::ClaimKind::FilingLock`] /
/// `FilingUnlock` into the machine-wide filing-lock store (Issue #6714).
///
/// The daemon is the bridge: only it is connected to the safehouse room, and
/// only the on-disk store is visible to the shell filers
/// (`create-issue.sh` → `lib/filing-lock.sh`) that actually run `gh issue
/// create`. Best-effort — an unresolvable store degrades the fleet tier to
/// host-only serialization, never to a failed filing.
fn mirror_filing_lock_to_disk(ad: &ClaimAd) {
    let Some(store) = crate::filing_lock::store_dir() else {
        return;
    };
    match ad.kind {
        crate::peer_claims::ClaimKind::FilingLock => {
            crate::filing_lock::record_peer_hold(&store, &ad.host);
        }
        crate::peer_claims::ClaimKind::FilingUnlock => {
            crate::filing_lock::clear_peer_hold(&store, &ad.host);
        }
        _ => {}
    }
}

/// Clear a TTL-expired peer's filing-hold mirror (Issue #6714) — the
/// crash-release path: a peer that dies mid-burst never sends `FilingUnlock`,
/// so its marker must be removed when the view expires it.
fn clear_filing_lock_mirror(host: &str) {
    if let Some(store) = crate::filing_lock::store_dir() {
        crate::filing_lock::clear_peer_hold(&store, host);
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
/// # Room routing: claim ads default to the signal room, opt-in dedicated room (#4225, #4713)
///
/// Attention-class routing sends `task` envelopes to the per-repo firehose, and a
/// claim ad *is* a per-repo `task` envelope — yet this connection deliberately
/// advertises into (and reads from) [`SafehouseConfig::claims_room`], **not**
/// the per-repo firehose. By default (`rooms.claims` unset) `claims_room()`
/// resolves to the **signal room**, the one deliberate exception to
/// "signal-only" #4225 established. Why the signal room is the *default*:
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
///    When those trade off, correctness wins — which is exactly why the
///    dedicated-room escape hatch below is opt-in, not the new default.
/// 3. **The reader must agree with the writer.** This task's inbound handler is
///    unfiltered — it folds *any* inbound line carrying a parseable `loom_claim`
///    body into the view — so it consumes ads from whatever rooms safehoused
///    pushes to it. Keeping the write side and the read side on the same
///    resolved room keeps the pair trivially consistent instead of depending on
///    which rooms this host's bot happens to have joined.
///
/// Ads are low volume per dispatch / terminal outcome, but
/// `sweep_registry::readvertise_peer_claims` re-publishes every live sweep's
/// claim each 30s reaper tick (deliberately under the peer-claim TTL — the
/// liveness contract requires the repetition), so at fleet scale the cadence is
/// enough to flood the human-visible signal room (#4713). An operator who finds
/// that disruptive may set `rooms.claims` (or `LOOM_SAFEHOUSE_ROOM_CLAIMS`) to
/// route claim ads into a dedicated coordination room instead, keeping the
/// signal room for sweep-lifecycle narration. **This is provisioning, not just
/// routing**: every host's safehoused bot must already be joined to that room
/// before it is configured, or that host silently stops seeing peers' claims —
/// the exact cross-host dedup failure class reason 1 above exists to avoid.
/// `rooms.claims` absent (the default) is byte-identical to pre-#4713 behavior.
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
    // The claims room (see the routing rationale above): `rooms.claims` when
    // configured (#4713), else the signal room. In single-room mode (no `rooms`
    // block at all) this *is* `config.room`, so the pre-#4225/#4713 behavior is
    // byte-identical.
    let claims_room = config.claims_room().map(ToOwned::to_owned);
    loop {
        set_state(
            &state,
            SafehouseState::Unreachable {
                socket: socket.clone(),
            },
        );
        let client = match SafehouseClient::connect(&socket, &config.persona, claims_room.clone())
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
                        room: claims_room.clone(),
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
mod tests;
