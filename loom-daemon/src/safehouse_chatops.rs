//! Inbound safehouse **ChatOps steering** for `loom-daemon` (Issue #7893 —
//! Phase 3a of #4196), daemon side only.
//!
//! Phase 1 (#3997) gave the daemon a voice in the safehouse room. This module
//! gives it an ear — a deliberately tiny one.
//!
//! # The boundary (operator ruling, 2026-09-16)
//!
//! Two layers, not one:
//!
//! 1. **The daemon keeps a closed, typed command enum** ([`command::Command`]).
//!    It never interprets natural language, never falls back to a shell, and
//!    refuses anything outside the enum with a reply. This layer stays boring
//!    and auditable.
//! 2. **Natural language lives in a separate operator-agent persona** (Phase
//!    3b), which reads human intent and *uses* this typed surface plus the
//!    forge. Out of scope here.
//!
//! # The three gates an inbound message must pass
//!
//! | Gate | Where | Failure |
//! |---|---|---|
//! | Addressed to the daemon | [`inbound_command`] | ignored (not refused — the room is full of traffic that is not for us) |
//! | Sender on the allowlist | [`ChatOpsRouter::handle_at`] | logged + published refusal, **no room reply** |
//! | Parses to a [`command::Command`] | [`command::Command::parse`] | logged + published refusal, **with** a room reply |
//!
//! A destructive command then needs a fourth: a single-use, TTL-bounded,
//! sender-bound confirm-nonce round-trip ([`nonce`]).
//!
//! # The sender is whatever safehoused stamped, never what the body claims
//!
//! [`inbound_command`] reads the sender **only** from the envelope's `from`
//! field, which safehoused stamps from the socket identity (envelope-v1 §6 —
//! the same reason Phase 1's client never sends a `from`). A body that says
//! `from: @someone:example.org` is just text: the parser has no such token and
//! the allowlist check never looks at it.
//!
//! # Off unless configured
//!
//! [`resolve_chatops_config`] returns `None` — no task, no socket, no
//! subscription — unless a `safehouse.chatops` block (or its env override) is
//! present **and** names at least one allowed sender. An install without
//! safehouse, or with safehouse but without this block, is byte-for-byte
//! unaffected.
//!
//! # Conventions reused (not re-invented)
//!
//! - Phase 1 (#3997): [`crate::safehouse::SafehouseClient`], [`crate::safehouse::Envelope`],
//!   `build_send_request`, the persona/socket/room config resolution, and the
//!   capped-backoff reconnect shape. No second wire-protocol implementation.
//! - Phase 2 (#4197/#4199): the same `safehouse.*` config-block + `LOOM_SAFEHOUSE_*`
//!   env-override convention, precedence **env > config > default**.
//! - The frozen event taxonomy: refusals/acceptances ride
//!   [`crate::types::Event::Generic`] under `safehouse.chatops.*`, adding no
//!   typed variant and no narration (a `Generic` event is never narrated, so
//!   replies cannot feed back into the room).

pub mod command;
pub mod nonce;
pub mod runtime;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::event_bus::EventBus;
use crate::safehouse::SafehouseConfig;

pub use command::{Command, ParseError};
pub use nonce::{ConfirmOutcome, PendingConfirmations, DEFAULT_CONFIRM_TTL};

/// Config-block env overrides (precedence **env > config > default**), matching
/// the `LOOM_SAFEHOUSE_*` naming Phase 1 established.
const CHATOPS_ENABLED_ENV: &str = "LOOM_SAFEHOUSE_CHATOPS_ENABLED";
const CHATOPS_SENDERS_ENV: &str = "LOOM_SAFEHOUSE_CHATOPS_SENDERS";
const CHATOPS_ROOM_ENV: &str = "LOOM_SAFEHOUSE_CHATOPS_ROOM";
const CHATOPS_CONFIRM_TTL_ENV: &str = "LOOM_SAFEHOUSE_CHATOPS_CONFIRM_TTL_SECS";

/// Clamp bounds for `confirmTtlSecs`. A sub-10s window is unusable by a human;
/// an hour-long one is a standing capability, which is the thing the nonce
/// exists to avoid.
const MIN_CONFIRM_TTL: Duration = Duration::from_secs(10);
const MAX_CONFIRM_TTL: Duration = Duration::from_secs(3600);

/// Event-bus topics. Prefix-matched by [`EventBus::subscribe`], so a subscriber
/// can take the whole surface with `safehouse.chatops`.
pub const TOPIC_ACCEPTED: &str = "safehouse.chatops.accepted";
pub const TOPIC_REFUSED: &str = "safehouse.chatops.refused";
pub const TOPIC_CONFIRM_REQUIRED: &str = "safehouse.chatops.confirm_required";

// ============================================================================
// Config
// ============================================================================

/// Resolved `safehouse.chatops` block. Existing **at all** means inbound
/// steering is on — there is no separate "enabled but unusable" state, because
/// an empty allowlist resolves to `None` rather than to an accept-nobody
/// config that looks enabled in `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatOpsConfig {
    /// Normalized Matrix IDs permitted to steer this daemon. Never empty.
    pub allowed_senders: BTreeSet<String>,
    /// Room to read steering from and reply into. `None` ⇒ the signal room
    /// ([`SafehouseConfig::signal_room`]).
    pub room: Option<String>,
    /// Confirm-nonce window for destructive commands.
    pub confirm_ttl: Duration,
}

impl ChatOpsConfig {
    /// Whether `sender` (raw, as stamped by safehoused) may steer this daemon.
    #[must_use]
    pub fn allows(&self, sender: &str) -> bool {
        self.allowed_senders.contains(&normalize_sender(sender))
    }

    /// The room this daemon reads steering from: the explicit
    /// `safehouse.chatops.room` when set, else the signal room the narration
    /// sink already uses. Deliberately **not** `claims_room` — peer-claim
    /// traffic may be routed to a dedicated machine-chatter room (#4713) that
    /// a human operator is not even joined to.
    #[must_use]
    pub fn room<'a>(&'a self, safehouse: &'a SafehouseConfig) -> Option<&'a str> {
        self.room.as_deref().or_else(|| safehouse.signal_room())
    }
}

/// Resolve the effective `safehouse.chatops` config for `repo_root`.
///
/// `None` ⇒ inbound steering is off: the block is absent, explicitly disabled,
/// or names no usable sender. Never panics; a malformed tree resolves to `None`
/// (fail closed).
#[must_use]
pub fn resolve_chatops_config(repo_root: &Path) -> Option<ChatOpsConfig> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let block = crate::config_resolver::get_path(&effective, "safehouse.chatops");
    apply_env_overrides(config_from_value(block))
}

/// Read the config layer only (no env), so unit tests can assert
/// config-over-default without mutating process env — the same split
/// `safehouse::config_from_value` uses.
#[must_use]
fn config_from_value(block: Option<&Value>) -> Option<ChatOpsConfig> {
    let block = block.and_then(Value::as_object)?;
    // `enabled` is parsed **strictly** (#8021): absent ⇒ on (the block's
    // presence is the opt-in), literal `true` ⇒ on, literal `false` ⇒ off, and
    // anything that is not a JSON boolean ⇒ **off, with a warning**. A
    // hand-edited `"enabled": "false"` — the JSON *string*, not the literal —
    // is a realistic typo, and reading it as "not a bool, so use the default"
    // would silently switch an inbound control channel on. A value this
    // function cannot understand is never taken as consent.
    match block.get("enabled") {
        None | Some(Value::Bool(true)) => {}
        // Present but explicitly disabled — off, and deliberately not a warning:
        // an operator who wrote `"enabled": false` knows.
        Some(Value::Bool(false)) => return None,
        Some(other) => {
            log::warn!(
                "safehouse chatops: `enabled` must be a JSON boolean, got {} — \
                 treating it as false; inbound steering stays OFF",
                value_kind(other)
            );
            return None;
        }
    }
    let allowed_senders = block
        .get("allowedSenders")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .filter_map(accept_sender)
                .collect::<BTreeSet<String>>()
        })
        .unwrap_or_default();
    let room = block
        .get("room")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|room| !room.is_empty())
        .map(ToOwned::to_owned);
    let confirm_ttl = block
        .get("confirmTtlSecs")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_CONFIRM_TTL, clamp_ttl);
    Some(ChatOpsConfig {
        allowed_senders,
        room,
        confirm_ttl,
    })
}

/// Apply the env layer. Env can also *create* a config from nothing
/// (`LOOM_SAFEHOUSE_CHATOPS_SENDERS` alone is enough), which keeps the
/// "env > config" precedence honest and makes the live path testable without a
/// config file — but it cannot conjure an allowlist, so the fail-closed
/// empty-allowlist rule below still applies.
#[must_use]
fn apply_env_overrides(config: Option<ChatOpsConfig>) -> Option<ChatOpsConfig> {
    let env_senders = env_nonempty(CHATOPS_SENDERS_ENV);
    let env_enabled = env_enabled_override();
    let mut config = match config {
        Some(config) => config,
        // No config block: only an explicit env opt-in brings it into existence.
        None if env_senders.is_some() || env_enabled == Some(true) => ChatOpsConfig {
            allowed_senders: BTreeSet::new(),
            room: None,
            confirm_ttl: DEFAULT_CONFIRM_TTL,
        },
        None => return None,
    };
    if env_enabled == Some(false) {
        return None;
    }
    if let Some(list) = env_senders {
        config.allowed_senders = list
            .split([',', ' ', '\t'])
            .filter_map(accept_sender)
            .collect();
    }
    if let Some(room) = env_nonempty(CHATOPS_ROOM_ENV) {
        config.room = Some(room);
    }
    if let Some(secs) =
        env_nonempty(CHATOPS_CONFIRM_TTL_ENV).and_then(|raw| raw.parse::<u64>().ok())
    {
        config.confirm_ttl = clamp_ttl(secs);
    }
    // **Load-bearing, not defensive.** An empty allowlist is deny-all, and this
    // early return is the layer that makes it *structurally* so: with no config
    // there is no router, no task, no socket and no subscription. Deleting it
    // would leave only `BTreeSet::contains` (always false on an empty set)
    // standing between an enabled block and an accept-nobody config that still
    // reports as enabled in `status` — i.e. one refactor away from the classic
    // empty-list-means-permissive bug. Covered by
    // `tests::an_empty_allowlist_resolves_to_no_config_at_all` and
    // `tests::an_empty_allowlist_survives_the_whole_resolution_path` (#8021);
    // both fail if this returns `Some`.
    if config.allowed_senders.is_empty() {
        log::warn!(
            "safehouse chatops: configured but no usable entry in allowedSenders \
             (expected Matrix IDs like @you:example.org) — inbound steering stays OFF"
        );
        return None;
    }
    Some(config)
}

/// Validate and normalize one allowlist entry. A Matrix ID is
/// `@localpart:server`; anything else is a typo (or a config-shape mistake like
/// a persona name) and is dropped with a warning rather than silently admitted
/// as an entry that can never match.
fn accept_sender(raw: &str) -> Option<String> {
    let id = normalize_sender(raw);
    if id.is_empty() {
        return None;
    }
    if !id.starts_with('@') || !id.contains(':') {
        log::warn!("safehouse chatops: ignoring malformed allowedSenders entry {id:?}");
        return None;
    }
    Some(id)
}

/// Canonical form for comparing Matrix IDs: trimmed and ASCII-lowercased.
///
/// Matrix requires user IDs to be lowercase, but historical accounts and
/// hand-typed config are not reliably so; comparing case-insensitively avoids an
/// allowlist that silently matches nothing. It is case *folding*, not
/// normalization of any other kind — no unicode casefold, no punycode — because
/// widening the equivalence class is exactly what an allowlist must not do.
#[must_use]
pub fn normalize_sender(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn clamp_ttl(secs: u64) -> Duration {
    Duration::from_secs(secs).clamp(MIN_CONFIRM_TTL, MAX_CONFIRM_TTL)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The `LOOM_SAFEHOUSE_CHATOPS_ENABLED` override, parsed with the same strict
/// rule [`config_from_value`] applies to the config key: `None` ⇒ unset (defer
/// to the config layer), and a value present but not a recognized boolean word
/// ⇒ `Some(false)`, **not** "unset" (#8021). Ignoring a typo'd `off` would leave
/// an operator who tried to switch inbound steering off with it still on.
fn env_enabled_override() -> Option<bool> {
    let raw = env_nonempty(CHATOPS_ENABLED_ENV)?;
    Some(parse_bool_word(&raw).unwrap_or_else(|| {
        log::warn!(
            "safehouse chatops: {CHATOPS_ENABLED_ENV} is not a boolean \
             (expected one of 1/true/yes/on/0/false/no/off) — treating it as false; \
             inbound steering stays OFF"
        );
        false
    }))
}

/// `None` ⇒ not a recognized boolean word. Pure, so the strict/lenient decision
/// lives at the call site rather than inside the parse.
fn parse_bool_word(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The JSON type of `value`, for a warning that must not echo a config blob
/// (or an unbounded string) into the log.
const fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

// ============================================================================
// Addressing
// ============================================================================

/// Extract `(sender, command text)` from one inbound room-event push line, or
/// `None` when the message is not addressed to this daemon.
///
/// Two addressing conventions are accepted, per the issue:
///
/// - the envelope's `to` field equals our persona; or
/// - the body opens with an `@persona` / `persona:` mention.
///
/// Body/`from` are read at `envelope.*` first with a top-level fallback —
/// exactly the shape `safehouse::PeerClaimSink` had to be corrected to in #6249
/// (safehoused's live push nests the envelope; reading only the top level
/// dropped 100% of inbound traffic fleet-wide).
///
/// Messages the daemon itself sent are dropped: replies are addressed to the
/// requesting human, so they cannot re-enter this path, but a `from` matching
/// our own persona is refused anyway so no future addressing convention can
/// create a loop.
#[must_use]
pub fn inbound_command(event: &Value, persona: &str) -> Option<(String, String)> {
    let nested = |key: &str| {
        event
            .pointer(&format!("/envelope/{key}"))
            .and_then(Value::as_str)
            .or_else(|| event.get(key).and_then(Value::as_str))
    };
    let body = nested("body")?.trim();
    if body.is_empty() {
        return None;
    }
    let from = nested("from").unwrap_or_default().trim();
    if from.is_empty() || from.eq_ignore_ascii_case(persona) {
        return None;
    }
    let to = nested("to").unwrap_or_default().trim();
    // Addressed either explicitly (`to`) or by mention. In both cases a leading
    // mention is stripped — redundant when `to` already named us, load-bearing
    // when it did not.
    addresses_persona(to, body, persona)
        .then(|| (from.to_owned(), strip_mention(body, persona).to_owned()))
}

/// Would a message with this `to` and `body` be read as addressed to `persona`?
///
/// This is [`inbound_command`]'s own addressing rule, factored out so an
/// *outbound* path can ask the question before it sends. The concierge's
/// `say` uses it to refuse prose that the daemon would pick up as a command
/// (`loom-daemon concierge say --body "@loom_daemon confirm …"`), which is the
/// only reason it is public: a second, parallel "does this look addressed?"
/// heuristic would be free to drift away from the parser it is supposed to
/// predict, and a drift in that direction is a bypass.
///
/// Deliberately ignores `from`: [`inbound_command`] additionally drops the
/// daemon's own messages, but an outbound caller asking "could this be read as
/// a command?" wants the conservative answer, not the one that depends on who
/// happens to be sending.
#[must_use]
pub fn addresses_persona(to: &str, body: &str, persona: &str) -> bool {
    let body = body.trim();
    if body.is_empty() {
        return false;
    }
    if to.trim().eq_ignore_ascii_case(persona) {
        return true;
    }
    strip_mention(body, persona).len() != body.len()
}

/// Strip a leading `@persona`, `persona:` or `@persona:` mention. Returns the
/// input unchanged when there is no mention, which is how [`inbound_command`]
/// detects the mention convention (by length change).
fn strip_mention<'a>(body: &'a str, persona: &str) -> &'a str {
    let rest = body.strip_prefix('@').unwrap_or(body);
    let Some(rest) = rest.get(..persona.len()).and_then(|head| {
        head.eq_ignore_ascii_case(persona)
            .then(|| &rest[persona.len()..])
    }) else {
        return body;
    };
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    let trimmed = rest.trim_start();
    // A bare mention with no command still counts as addressed (it is refused
    // downstream as an empty command, with the usage reply) — but `@loom_daemonx`
    // must not: require a separator, not just a prefix match.
    if rest.len() == trimmed.len() && !rest.is_empty() {
        return body;
    }
    trimmed
}

// ============================================================================
// Routing
// ============================================================================

/// What the daemon decided to do with one addressed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run it. `command` is never [`Command::Confirm`] — a confirm resolves to
    /// the stored command before it gets here.
    Execute { sender: String, command: Command },
    /// Destructive: a nonce was minted and must be echoed to the sender.
    AwaitConfirmation {
        sender: String,
        command: Command,
        nonce: String,
    },
    /// Refused. [`Decision::reply`] decides whether the room hears about it.
    Refuse { refusal: Refusal },
}

impl Decision {
    /// The room reply this decision owes the sender, if any.
    ///
    /// [`Decision::Execute`] has none — its reply is the executor's rendered
    /// result, produced after the command runs.
    #[must_use]
    pub fn reply(&self) -> Option<String> {
        match self {
            Self::Execute { .. } => None,
            Self::AwaitConfirmation { command, nonce, .. } => Some(format!(
                "`{}` is destructive and needs confirmation. Reply `confirm {nonce}` to run it. \
                 The nonce is single-use, expires shortly, and only works from you.",
                command.summary()
            )),
            Self::Refuse { refusal } => refusal.reply(),
        }
    }
}

/// Why a message was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The envelope carried no usable `from`. Cannot be attributed, so cannot
    /// be trusted, and gets no reply (there is nobody to reply to).
    NoSender,
    /// The stamped sender is not on the allowlist. **Logged and published, but
    /// never replied to** — the acceptance criterion says "a logged refusal and
    /// no action", and replying would turn the daemon into an echo an
    /// unauthorized party can drive.
    NotAllowlisted { sender: String },
    /// Allowlisted, but the text is not a command.
    Unparsed { sender: String, error: ParseError },
    /// `confirm` with a nonce that is not outstanding (never issued, or already
    /// redeemed — i.e. a replay).
    ConfirmUnknown { sender: String },
    /// `confirm` with a nonce whose TTL elapsed.
    ConfirmExpired { sender: String },
    /// `confirm` with another allowlisted sender's nonce.
    ConfirmWrongSender { sender: String },
}

impl Refusal {
    /// Stable machine-readable reason for the event payload.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::NoSender => "no-sender".to_owned(),
            Self::NotAllowlisted { .. } => "sender-not-allowlisted".to_owned(),
            Self::Unparsed { error, .. } => format!("unparsed:{}", error.code()),
            Self::ConfirmUnknown { .. } => "confirm-unknown-nonce".to_owned(),
            Self::ConfirmExpired { .. } => "confirm-expired".to_owned(),
            Self::ConfirmWrongSender { .. } => "confirm-wrong-sender".to_owned(),
        }
    }

    /// The stamped sender, when there was one.
    #[must_use]
    pub fn sender(&self) -> Option<&str> {
        match self {
            Self::NoSender => None,
            Self::NotAllowlisted { sender }
            | Self::Unparsed { sender, .. }
            | Self::ConfirmUnknown { sender }
            | Self::ConfirmExpired { sender }
            | Self::ConfirmWrongSender { sender } => Some(sender),
        }
    }

    /// The room reply, or `None` when this refusal is deliberately silent.
    #[must_use]
    pub fn reply(&self) -> Option<String> {
        match self {
            // Silent by design: an unauthenticated or unauthorized sender gets
            // a log line and a bus event, not a room line they can drive.
            Self::NoSender | Self::NotAllowlisted { .. } => None,
            Self::Unparsed { error, .. } => Some(format!("Refused: {error}. {}", command::USAGE)),
            Self::ConfirmUnknown { .. } => Some(
                "Refused: that confirmation is not outstanding (never issued, or already used). \
                 Re-issue the command to get a fresh nonce."
                    .to_owned(),
            ),
            Self::ConfirmExpired { .. } => Some(
                "Refused: that confirmation expired. Re-issue the command to get a fresh nonce."
                    .to_owned(),
            ),
            Self::ConfirmWrongSender { .. } => {
                Some("Refused: that confirmation was issued to someone else.".to_owned())
            }
        }
    }
}

/// The gate itself: allowlist → parse → nonce, with logging and event-bus
/// publication on every outcome.
///
/// Holds no I/O. The live task ([`runtime::run`]) reads the room, calls
/// [`ChatOpsRouter::handle`], and does what the [`Decision`] says — which keeps
/// every security-relevant branch a pure, clock-injected unit test.
pub struct ChatOpsRouter {
    config: ChatOpsConfig,
    /// Our own persona, for addressing and for the log prefix.
    persona: String,
    pending: Mutex<PendingConfirmations>,
    events: Option<Arc<EventBus>>,
}

impl ChatOpsRouter {
    #[must_use]
    pub fn new(config: ChatOpsConfig, persona: String, events: Option<Arc<EventBus>>) -> Self {
        let pending = Mutex::new(PendingConfirmations::new(config.confirm_ttl));
        Self {
            config,
            persona,
            pending,
            events,
        }
    }

    #[must_use]
    pub const fn config(&self) -> &ChatOpsConfig {
        &self.config
    }

    #[must_use]
    pub fn persona(&self) -> &str {
        &self.persona
    }

    /// Outstanding-nonce count, for tests and status rendering.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.lock_pending().len()
    }

    /// Route one addressed message using the wall clock.
    pub fn handle(&self, sender: &str, text: &str) -> Decision {
        self.handle_at(sender, text, Instant::now())
    }

    /// Route one addressed message at an injected `now`.
    ///
    /// `sender` must be the value safehoused stamped on the envelope. Nothing
    /// in `text` can influence sender identity — that is the whole point of
    /// taking them as separate arguments.
    pub fn handle_at(&self, sender: &str, text: &str, now: Instant) -> Decision {
        let sender = normalize_sender(sender);
        if sender.is_empty() {
            return self.refuse(Refusal::NoSender);
        }
        if !self.config.allowed_senders.contains(&sender) {
            return self.refuse(Refusal::NotAllowlisted { sender });
        }
        let command = match Command::parse(text) {
            Ok(command) => command,
            Err(error) => return self.refuse(Refusal::Unparsed { sender, error }),
        };
        if let Command::Confirm { nonce } = &command {
            let outcome = self.lock_pending().confirm_at(&sender, nonce, now);
            return match outcome {
                ConfirmOutcome::Confirmed(command) => self.accept(sender, command, true),
                ConfirmOutcome::Unknown => self.refuse(Refusal::ConfirmUnknown { sender }),
                ConfirmOutcome::Expired => self.refuse(Refusal::ConfirmExpired { sender }),
                ConfirmOutcome::WrongSender => self.refuse(Refusal::ConfirmWrongSender { sender }),
            };
        }
        if command.requires_confirmation() {
            let nonce = self.lock_pending().issue_at(&sender, command.clone(), now);
            log::info!(
                "safehouse chatops: {sender} requested `{}` — confirmation required",
                command.summary()
            );
            self.publish(
                TOPIC_CONFIRM_REQUIRED,
                json!({
                    "sender": sender,
                    "command": command.summary(),
                    "verb": command.verb(),
                    "outcome": "confirm_required",
                    "ttl_secs": self.config.confirm_ttl.as_secs(),
                }),
            );
            return Decision::AwaitConfirmation {
                sender,
                command,
                nonce,
            };
        }
        self.accept(sender, command, false)
    }

    /// Record an accepted command and hand it to the caller to execute.
    fn accept(&self, sender: String, command: Command, confirmed: bool) -> Decision {
        log::info!(
            "safehouse chatops: accepted `{}` from {sender}{}",
            command.summary(),
            if confirmed { " (confirmed)" } else { "" }
        );
        self.publish(
            TOPIC_ACCEPTED,
            json!({
                "sender": sender,
                "command": command.summary(),
                "verb": command.verb(),
                "outcome": "accepted",
                "confirmed": confirmed,
            }),
        );
        Decision::Execute { sender, command }
    }

    /// Record a refusal. Every refused command is logged **and** published with
    /// the stamped sender identity, whether or not the room hears about it.
    fn refuse(&self, refusal: Refusal) -> Decision {
        let sender = refusal.sender().unwrap_or("<unattributed>");
        log::warn!("safehouse chatops: refused a message from {sender} ({})", refusal.reason());
        self.publish(
            TOPIC_REFUSED,
            json!({
                "sender": refusal.sender(),
                "outcome": "refused",
                "reason": refusal.reason(),
            }),
        );
        Decision::Refuse { refusal }
    }

    /// Publish onto the shared bus as an [`crate::types::Event::Generic`].
    ///
    /// Generic events are never narrated back into the room
    /// (`safehouse::event_to_envelope` returns `None` for them), so this
    /// audit trail cannot become a feedback loop. A bus with no subscribers is
    /// not an error — publication is advisory, exactly like every other
    /// `publish_generic` caller.
    fn publish(&self, topic: &str, payload: Value) {
        if let Some(events) = &self.events {
            let _ = events.publish_generic(topic, payload);
        }
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, PendingConfirmations> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
