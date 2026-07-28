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

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use crate::event_bus::{EventBus, RecvError};
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
// Event → envelope mapping (existing frozen taxonomy only)
// ============================================================================

/// Map an existing bus [`Event`] to a narration [`Envelope`], or `None` for
/// events phase 1 does not narrate.
///
/// | Event | type | body |
/// |---|---|---|
/// | `SweepGlobalDispatch(Issue n)` | `task` | `sweep dispatched: issue #n` |
/// | `SweepPhase` | `task` | `issue #n → <phase>` (+ PR when present) |
/// | `SweepBlocker` | `handoff` | `issue #n blocked: <reason>` |
/// | `SweepExited` | `ack` | `issue #n complete (exit <code>, <dur>s)` |
/// | `SweepCrashed` | `handoff` | `issue #n crashed at <checkpoint_phase>` |
///
/// `SweepGlobalCompleted` is intentionally **not** narrated: it carries only a
/// `sweep_id` (no issue number), and `SweepExited` already emits the completion
/// `ack` with richer data — narrating both would double-post per completion.
#[must_use]
pub fn event_to_envelope(event: &Event) -> Option<Envelope> {
    match event {
        Event::SweepGlobalDispatch {
            kind: SweepKind::Issue(issue),
            ..
        } => Some(Envelope {
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some(issue.to_string()),
            body: format!("sweep dispatched: issue #{issue}"),
        }),
        Event::SweepPhase {
            issue,
            phase,
            pr_number,
            ..
        } => {
            let mut body = format!("issue #{issue} → {phase}");
            if let Some(pr) = pr_number {
                body.push_str(&format!(" (PR #{pr})"));
            }
            Some(Envelope {
                to: "*".to_owned(),
                kind: "task".to_owned(),
                task_id: Some(issue.to_string()),
                body,
            })
        }
        Event::SweepBlocker { issue, reason, .. } => Some(Envelope {
            to: "*".to_owned(),
            kind: "handoff".to_owned(),
            task_id: Some(issue.to_string()),
            body: format!("issue #{issue} blocked: {reason}"),
        }),
        Event::SweepExited {
            issue,
            exit_code,
            duration_sec,
            ..
        } => {
            let code = exit_code.map_or_else(|| "?".to_owned(), |c| c.to_string());
            Some(Envelope {
                to: "*".to_owned(),
                kind: "ack".to_owned(),
                task_id: Some(issue.to_string()),
                body: format!("issue #{issue} complete (exit {code}, {duration_sec}s)"),
            })
        }
        Event::SweepCrashed {
            issue,
            checkpoint_phase,
            ..
        } => {
            let phase = checkpoint_phase.as_deref().unwrap_or("unknown");
            Some(Envelope {
                to: "*".to_owned(),
                kind: "handoff".to_owned(),
                task_id: Some(issue.to_string()),
                body: format!("issue #{issue} crashed at {phase}"),
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
                // out; phase 1 is emit-only and does not consume inbound.
                continue;
            }
            return Ok(value);
        }
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

        let Some(envelope) = event_to_envelope(&event) else {
            continue;
        };

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
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::types::{SweepId, SweepKind};
    use serde_json::json;
    use serial_test::serial;
    use std::sync::Arc;
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

    // ---- event → envelope mapping ----

    #[test]
    fn maps_the_five_narrated_events() {
        let dispatch = Event::SweepGlobalDispatch {
            sweep_id: "sweep-issue-42-1".to_owned() as SweepId,
            kind: SweepKind::Issue(42),
        };
        let env = event_to_envelope(&dispatch).unwrap();
        assert_eq!(env.kind, "task");
        assert_eq!(env.task_id.as_deref(), Some("42"));
        assert!(env.body.contains("issue #42"));

        let phase = Event::SweepPhase {
            issue: 42,
            phase: "builder".to_owned(),
            pr_number: Some(99),
            repo: None,
        };
        let env = event_to_envelope(&phase).unwrap();
        assert_eq!(env.kind, "task");
        assert!(env.body.contains("builder"));
        assert!(env.body.contains("PR #99"));

        let blocker = Event::SweepBlocker {
            issue: 42,
            reason: "missing dep".to_owned(),
            label_added: "loom:blocked".to_owned(),
            repo: None,
        };
        let env = event_to_envelope(&blocker).unwrap();
        assert_eq!(env.kind, "handoff");
        assert!(env.body.contains("blocked"));

        let exited = Event::SweepExited {
            issue: 42,
            exit_code: Some(0),
            duration_sec: 12,
            repo: None,
        };
        let env = event_to_envelope(&exited).unwrap();
        assert_eq!(env.kind, "ack");
        assert!(env.body.contains("exit 0"));
        assert!(env.body.contains("12s"));

        let crashed = Event::SweepCrashed {
            issue: 42,
            checkpoint_phase: Some("judge".to_owned()),
            repo: None,
        };
        let env = event_to_envelope(&crashed).unwrap();
        assert_eq!(env.kind, "handoff");
        assert!(env.body.contains("judge"));
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
}
