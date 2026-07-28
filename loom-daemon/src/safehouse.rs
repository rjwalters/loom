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
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc::error::TryRecvError;

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

/// The closed 4-value `type` enum (`envelope.rs:10`). A `send` outside this set
/// is rejected by safehoused, so we reject it before sending.
pub const KNOWN_TYPES: [&str; 4] = ["chat", "task", "handoff", "ack"];

/// Reconnect backoff floor and ceiling. The floor keeps a burst of events from
/// hammering an absent peer (one `warn`, not a hot loop); the ceiling caps the
/// wait so a peer that comes back is picked up promptly.
const DEFAULT_MIN_BACKOFF: Duration = Duration::from_secs(2);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);

// ============================================================================
// Config
// ============================================================================

/// Resolved `safehouse` config block. `enabled: false` is the default and a
/// byte-for-byte no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafehouseConfig {
    pub enabled: bool,
    /// Socket path; `None` ⇒ resolve `$SAFEHOUSED_SOCKET` at connect time.
    pub socket: Option<PathBuf>,
    /// Room name/id; `None` is valid only when safehoused joined exactly one
    /// room (it then resolves the sole room server-side).
    pub room: Option<String>,
    /// Persona to authenticate as; must be in safehoused's allowlist.
    pub persona: String,
}

impl Default for SafehouseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket: None,
            room: None,
            persona: DEFAULT_PERSONA.to_owned(),
        }
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
    cfg
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
    cfg
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
pub fn build_send_request(env: &Envelope, id: u64, room: Option<&str>) -> Result<Value> {
    if !KNOWN_TYPES.contains(&env.kind.as_str()) {
        bail!("invalid envelope type {:?} (v1 types: {:?})", env.kind, KNOWN_TYPES);
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
    Ok(req)
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
///
/// `SweepGlobalCompleted` is intentionally **not** narrated: it carries only a
/// `sweep_id` (no issue number), and `SweepExited` already emits the completion
/// `ack` with richer data — narrating both would double-post per completion.
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
        }),
        Event::SweepExited {
            issue,
            exit_code,
            duration_sec,
            repo,
        } => {
            let prefix = repo_issue_prefix(repo.as_deref(), *issue);
            let dur = format_narrated_duration(*duration_sec);
            let body = match exit_code {
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
            })
        }
        Event::SweepCrashed {
            issue,
            checkpoint_phase,
            classification: _,
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
            })
        }
        // SweepGlobalCompleted (no issue number — SweepExited covers it),
        // SweepGlobalDispatch(PrSet), EpicAction, CapacityAdvisory, TopicLag,
        // Generic: not narrated in phase 1.
        _ => None,
    }
}

// ============================================================================
// Client
// ============================================================================

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

    /// Serialize and send one narration envelope, then read + `id`-match the
    /// reply (skipping any interleaved push line).
    pub async fn send(&mut self, env: &Envelope) -> Result<()> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let req = build_send_request(env, id, self.room.as_deref())?;
        self.write_line(&req).await?;
        let reply = self.read_reply().await?;
        if !reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let err = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("safehoused rejected send: {err}");
        }
        // Replies echo the request id (handle_conn stamps it). A mismatch means
        // the stream desynced — treat it as a hard error so the sink reconnects.
        if let Some(reply_id) = reply.get("id").and_then(Value::as_u64) {
            if reply_id != id {
                bail!("safehoused reply id {reply_id} != request id {id} (stream desync)");
            }
        }
        Ok(())
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
/// disabled or when no socket path can be resolved.
#[must_use]
pub fn spawn_sink(
    config: SafehouseConfig,
    bus: &EventBus,
    runtime: &tokio::runtime::Handle,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        // Disabled ⇒ do not even subscribe. No syscalls, no behavior change.
        return None;
    }
    let Some(socket) = resolve_socket(&config) else {
        log::warn!(
            "safehouse: enabled but no socket path resolved \
             (set safehouse.socket, $LOOM_SAFEHOUSE_SOCKET, or $SAFEHOUSED_SOCKET) — narration off"
        );
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
        run_sink(config, socket, subscription, DEFAULT_MIN_BACKOFF, DEFAULT_MAX_BACKOFF).await;
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
async fn run_sink(
    config: SafehouseConfig,
    socket: PathBuf,
    mut subscription: crate::event_bus::Subscription,
    min_backoff: Duration,
    max_backoff: Duration,
) {
    let mut client: Option<SafehouseClient> = None;
    // Next instant a reconnect may be attempted, and the current backoff.
    let mut next_attempt = Instant::now();
    let mut backoff = min_backoff;
    // Suppress duplicate outage warnings — one warn per outage, not per event.
    let mut warned = false;
    // Short-TTL cache for the dispatch-line title lookup (issue #4201).
    let mut title_cache: HashMap<(String, u32), (String, Instant)> = HashMap::new();

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

        let Some(mut envelope) = event_to_envelope(&event) else {
            continue;
        };

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
            if let Some(title) = fetch_title_cached(&mut title_cache, workspace_root, *issue).await
            {
                envelope.body.push_str(&format!(" — \"{title}\""));
            }
        }

        // (Re)connect lazily, honoring the backoff window so an absent peer is
        // not hammered once per event.
        if client.is_none() {
            if Instant::now() < next_attempt {
                continue; // in backoff window — drop this narration silently
            }
            match SafehouseClient::connect(&socket, &config.persona, config.room.clone()).await {
                Ok(connected) => {
                    if warned {
                        log::info!("safehouse: reconnected to {}", socket.display());
                    }
                    client = Some(connected);
                    backoff = min_backoff;
                    warned = false;
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
                    next_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }
        }

        if let Some(connected) = client.as_mut() {
            if let Err(err) = connected.send(&envelope).await {
                log::warn!(
                    "safehouse: narration send failed ({err:#}); will reconnect, sweep unaffected"
                );
                client = None;
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
#[must_use]
pub fn claim_ad_to_envelope(ad: &ClaimAd) -> Envelope {
    Envelope {
        to: "*".to_owned(),
        kind: "task".to_owned(),
        task_id: Some(ad.issue.to_string()),
        body: ad.to_body_json(),
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
#[must_use]
pub fn spawn_peer_coordination(
    config: SafehouseConfig,
    sink: Arc<dyn InboundEventSink>,
    outbound: tokio::sync::mpsc::Receiver<ClaimAd>,
    runtime: &tokio::runtime::Handle,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        return None; // disabled ⇒ no socket, no task, no syscalls
    }
    let Some(socket) = resolve_socket(&config) else {
        log::warn!(
            "safehouse: peer coordination enabled but no socket path resolved \
             (set safehouse.socket, $LOOM_SAFEHOUSE_SOCKET, or $SAFEHOUSED_SOCKET) — \
             soft-claim coordination off"
        );
        return None;
    };
    log::info!(
        "safehouse: peer-claim coordination enabled (persona={}, socket={})",
        config.persona,
        socket.display()
    );
    Some(runtime.spawn(async move {
        run_coordination(config, socket, sink, outbound, DEFAULT_MIN_BACKOFF, DEFAULT_MAX_BACKOFF)
            .await;
    }))
}

/// The coordination loop. Reconnects lazily with capped backoff; on each live
/// connection it concurrently reads inbound room events (→ `sink`) and drains
/// outbound claim ads (→ socket). Any I/O failure degrades to a reconnect — it
/// never blocks or fails a dispatch. Returns when all outbound senders drop.
async fn run_coordination(
    config: SafehouseConfig,
    socket: PathBuf,
    sink: Arc<dyn InboundEventSink>,
    mut outbound: tokio::sync::mpsc::Receiver<ClaimAd>,
    min_backoff: Duration,
    max_backoff: Duration,
) {
    let mut backoff = min_backoff;
    let mut warned = false;
    loop {
        let client = match SafehouseClient::connect(&socket, &config.persona, config.room.clone())
            .await
        {
            Ok(client) => {
                if warned {
                    log::info!("safehouse: peer coordination reconnected to {}", socket.display());
                }
                backoff = min_backoff;
                warned = false;
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
                            }
                            // A reply echo (has `id`, no `event`) to one of our
                            // own sends: nothing to do, drop it.
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

    // ---- envelope serialization / validation ----

    #[test]
    fn send_request_emits_v1_and_never_from() {
        let env = Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some("4137".to_owned()),
            body: "hi".to_owned(),
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
        };
        let req = build_send_request(&env, 1, None).unwrap();
        assert_eq!(req["to"], json!("loom_builder"));

        // "*" and @matrix ids pass through untouched.
        assert_eq!(normalize_to("*").unwrap(), "*");
        assert_eq!(normalize_to("@a:b.c").unwrap(), "@a:b.c");

        // A value that cannot be a persona is rejected, not silently sent.
        assert!(normalize_to("has space").is_err());
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
    fn maps_the_five_narrated_events() {
        let dispatch = Event::SweepGlobalDispatch {
            sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
            kind: SweepKind::Issue(42),
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
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&exited_failed).unwrap();
        assert_eq!(env.body, "vibesql#42 · failed ✗ · exit 78 (EX_CONFIG: token pool) · 24s");

        let crashed = Event::SweepCrashed {
            issue: 42,
            checkpoint_phase: Some("judge".to_owned()),
            classification: None,
            repo: Some("/repos/vibesql".to_owned()),
        };
        let env = event_to_envelope(&crashed).unwrap();
        assert_eq!(env.kind, "handoff");
        assert_eq!(env.body, "vibesql#42 · crashed ✗ at judge — resumable (checkpoint kept)");
    }

    #[test]
    fn maps_events_without_repo_using_bare_fallback() {
        // No `repo` stamped (a pre-#4201 event, or a registry that never
        // wires the bus) still narrates — just without repo qualification,
        // matching the pre-#4201 behavior for task_id and body prefix.
        let dispatch = Event::SweepGlobalDispatch {
            sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
            kind: SweepKind::Issue(42),
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
        ));

        bus.publish(Event::SweepGlobalDispatch {
            sweep_id: "sweep-issue-4201-1".to_owned() as SweepId,
            kind: SweepKind::Issue(4201),
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

    #[tokio::test]
    async fn disabled_config_does_not_subscribe() {
        let bus = EventBus::new();
        assert_eq!(bus.receiver_count(), 0);
        let handle = spawn_sink(
            SafehouseConfig::default(), // disabled
            &bus,
            &tokio::runtime::Handle::current(),
        );
        assert!(handle.is_none(), "disabled ⇒ no sink task");
        // The load-bearing no-op assertion: no subscription was created.
        assert_eq!(bus.receiver_count(), 0, "disabled ⇒ no bus subscription");
    }

    #[tokio::test]
    async fn absent_peer_degrades_without_blocking() {
        // enabled + nonexistent socket: the sink subscribes, but every connect
        // fails and is swallowed — publishing never blocks or errors.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("does-not-exist.sock");
        let bus = Arc::new(EventBus::new());
        let subscription = bus.subscribe(Vec::<String>::new());

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
        let handle = spawn_peer_coordination(
            SafehouseConfig::default(), // disabled
            sink,
            rx,
            &tokio::runtime::Handle::current(),
        );
        assert!(handle.is_none(), "disabled ⇒ no coordination task");
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
        server.await.unwrap();
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

        let task = tokio::spawn(run_coordination(
            SafehouseConfig {
                enabled: true,
                socket: Some(socket.clone()),
                ..SafehouseConfig::default()
            },
            socket,
            sink,
            rx,
            Duration::from_millis(10),
            Duration::from_millis(30),
        ));

        // Drop the only sender: the connect-fail drain loop must observe
        // Disconnected and return, so the task terminates rather than spinning.
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("coordination task must terminate after its senders drop")
            .unwrap();
    }
}
