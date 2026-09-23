//! The **Concierge** — Loom's operator-agent persona (Issue #7947, Phase 3b of
//! #4196) — and the mechanical guardrails around it.
//!
//! # What this module is, and what it deliberately is not
//!
//! Phase 3a (#7893) gave `loom-daemon` an ear that understands exactly six
//! typed verbs and nothing else (`loom-daemon/src/safehouse_chatops/`). This
//! module is the **other** side of the operator boundary ruling of 2026-09-16:
//! the layer that reads free-form human prose out of a safehouse room, decides
//! what the human meant, and then steers the daemon using **only** those same
//! six verbs, exactly as a human typing into the room would.
//!
//! The *deciding* is done by an LLM session — the `concierge` role prompt
//! (`defaults/.claude/commands/loom/concierge.md`), dispatched by the
//! daemon-native role runner like Champion or Curator. Nothing in this file
//! interprets language on the daemon's behalf; there is no classifier here and
//! no "intent model". What this file provides is the **chokepoint the persona's
//! conclusions must pass through before they can become a command**:
//!
//! | Layer | Lives in | Enforces |
//! |---|---|---|
//! | Sender gating | [`ConciergeConfig::allows`] | who may address the persona at all |
//! | Budget | [`budget`] | per-tick message cap + per-day turn cap |
//! | Intent aid | [`intent`] | a conservative, deterministic prose → proposal map |
//! | **Relay vetting** | [`relay`] | **the security boundary**: what may become a command |
//!
//! # The three properties that are structural, not advisory
//!
//! 1. **The persona can never confirm a destructive command.** Its verb
//!    vocabulary is the type [`intent::Verb`], which has **no `Confirm`
//!    variant**. `confirm <nonce>` is therefore not something the persona can
//!    express, mis-map onto, or be talked into — it is unrepresentable. The
//!    nonce the daemon mints is relayed to the human verbatim and answered by
//!    the human.
//! 2. **`cancel` and `dispatch` need a second, distinct human message.**
//!    [`relay::vet_relay`] refuses both unless the caller supplies an
//!    [`relay::Authorization::Human`] whose message is *not* the message that
//!    asked for the action, comes from an allowlisted sender, is not itself
//!    flagged as an injection attempt, contains an affirmation, and echoes the
//!    target (issue number / sweep id). An injected room message therefore
//!    cannot authorize itself, **regardless of whether any heuristic notices it
//!    is an injection**. That is the property the #7947 acceptance criterion
//!    asks for, and it holds without depending on detection.
//! 3. **Nothing free-form becomes a command.** [`relay::vet_relay`] builds a
//!    [`Command`](crate::safehouse_chatops::Command) from a typed verb plus one
//!    charset-validated argument, renders it with 3a's own renderer, and then
//!    **re-parses that rendering with 3a's own parser**, asserting it round-trips
//!    to the same command. Room prose is never interpolated into the wire text.
//!
//! Everything else — the role prompt's guardrails, [`intent::scan_for_injection`]
//! — is defense in depth in the sense `defaults/docs/untrusted-external-content.md`
//! means it: it raises the cost of an injection and makes refusal the documented
//! default. The three properties above are what make a successful injection
//! non-catastrophic.
//!
//! # Off unless configured
//!
//! [`resolve_concierge_config`] returns `None` — no role tick, no budget file,
//! no relay — unless a `safehouse.concierge` block (or its env override) is
//! present **and** names at least one allowed sender. An install without
//! safehouse, or with safehouse but without this block, is byte-for-byte
//! unaffected: `role_runner` gates the `concierge` role on this same function,
//! and the role is additionally excluded from the "unset `roles` ⇒ all
//! defaults" fallback.
//!
//! # Conventions reused (not re-invented)
//!
//! The config shape, the strict `enabled` parse, the fail-closed empty
//! allowlist, and the `LOOM_SAFEHOUSE_*` env-override precedence
//! (**env > config > default**) are all deliberately identical to
//! [`crate::safehouse_chatops`]'s, including the two hardening rules #8021
//! extracted from 3a's Judge review — a non-boolean `enabled` is **off with a
//! warning**, and an empty allowlist resolves to no config at all rather than to
//! an accept-nobody config that still reports as enabled.

pub mod budget;
pub mod intent;
pub mod relay;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::safehouse::SafehouseConfig;
use crate::safehouse_chatops::normalize_sender;

pub use budget::{BudgetLedger, BudgetRefusal, BudgetSnapshot};
pub use intent::{scan_for_injection, InjectionScan, Proposal, RoomMessage, Verb};
pub use relay::{vet_relay, Authorization, RelayRefusal, RelayRequest};

/// The role name the daemon-native role runner dispatches for this persona,
/// and the stem of its prompt file. Named once so `role_runner`, the config
/// gate, and the tests can never disagree about it.
pub const CONCIERGE_ROLE: &str = "concierge";

/// Config-block env overrides (precedence **env > config > default**), matching
/// the `LOOM_SAFEHOUSE_*` naming Phase 1 established and 3a extended.
const ENABLED_ENV: &str = "LOOM_SAFEHOUSE_CONCIERGE_ENABLED";
const SENDERS_ENV: &str = "LOOM_SAFEHOUSE_CONCIERGE_SENDERS";
const ROOM_ENV: &str = "LOOM_SAFEHOUSE_CONCIERGE_ROOM";
const PERSONA_ENV: &str = "LOOM_SAFEHOUSE_CONCIERGE_PERSONA";
const MAX_MESSAGES_ENV: &str = "LOOM_SAFEHOUSE_CONCIERGE_MAX_MESSAGES";
const MAX_TURNS_ENV: &str = "LOOM_SAFEHOUSE_CONCIERGE_MAX_TURNS";

/// The persona name the concierge speaks as in the room, when
/// `safehouse.concierge.persona` does not name one.
///
/// Deliberately distinct from `safehouse.persona` (`loom_daemon`, the narration
/// + ChatOps identity): the room must be able to tell "the daemon answered"
/// from "the agent answered", and 3a's `inbound_command` drops any message whose
/// `from` matches the daemon's own persona — so a concierge sharing that persona
/// could not steer the daemon at all.
pub const DEFAULT_PERSONA: &str = "loom_concierge";

/// Default per-tick cap on room messages the persona may act on.
///
/// Modeled on `autonomous.roleRunner.architectMaxProposals` (#5656) — the same
/// "actuator saturation limit" shape, for the same reason: a chat room is an
/// unbounded trigger source, and a cadence-driven LLM session in front of one
/// needs a ceiling that does not depend on how chatty the room is.
pub const DEFAULT_MAX_MESSAGES_PER_TICK: u32 = 5;

/// Default cap on concierge **turns** (one role-runner tick = one turn) per UTC
/// day. The cost bound: a turn is a whole `claude -p` session, so this is the
/// knob that decides what the persona can spend in a day.
pub const DEFAULT_MAX_TURNS_PER_DAY: u32 = 24;

/// Clamp bounds. A cap of zero is `enabled: false` spelled confusingly, and an
/// unbounded cap is the thing these knobs exist to prevent — so both tiers drop
/// a `0`/unparseable value to the next tier rather than honoring it, exactly as
/// [`crate::role_runner::resolve_architect_max_proposals`] does.
const MAX_MESSAGES_CEILING: u32 = 50;
const MAX_TURNS_CEILING: u32 = 500;

/// Resolved `safehouse.concierge` block.
///
/// Existing **at all** means the persona is on — there is no separate "enabled
/// but unusable" state, because an empty allowlist resolves to `None` rather
/// than to an accept-nobody config that looks enabled (3a's #8021 rule, applied
/// here from day one rather than retrofitted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConciergeConfig {
    /// Normalized Matrix IDs permitted to address the persona. Never empty.
    ///
    /// **A different trust surface from `safehouse.chatops.allowedSenders`.**
    /// That list gates the *daemon*, which only ever executes one of six typed
    /// verbs. This list gates an agent that exercises *judgement* — so it is a
    /// separate key with its own default-empty, rather than an alias.
    pub allowed_senders: BTreeSet<String>,
    /// The persona the concierge speaks as. See [`DEFAULT_PERSONA`].
    pub persona: String,
    /// Room to read intent from and reply into. `None` ⇒ the signal room.
    pub room: Option<String>,
    /// Per-tick cap on messages acted upon. See [`DEFAULT_MAX_MESSAGES_PER_TICK`].
    pub max_messages_per_tick: u32,
    /// Per-UTC-day cap on turns. See [`DEFAULT_MAX_TURNS_PER_DAY`].
    pub max_turns_per_day: u32,
}

impl ConciergeConfig {
    /// Whether `sender` (raw, as stamped by safehoused) may address the
    /// persona.
    #[must_use]
    pub fn allows(&self, sender: &str) -> bool {
        self.allowed_senders.contains(&normalize_sender(sender))
    }

    /// The room the persona works in: explicit `safehouse.concierge.room` when
    /// set, else the signal room the narration sink already uses.
    ///
    /// Same choice, for the same reason, as
    /// [`crate::safehouse_chatops::ChatOpsConfig::room`]: deliberately **not**
    /// `claims_room`, which an operator may have routed to a machine-chatter
    /// room (#4713) no human is joined to.
    #[must_use]
    pub fn room<'a>(&'a self, safehouse: &'a SafehouseConfig) -> Option<&'a str> {
        self.room.as_deref().or_else(|| safehouse.signal_room())
    }
}

/// Resolve the effective `safehouse.concierge` config for `repo_root`.
///
/// `None` ⇒ the persona is off: the block is absent, explicitly disabled, or
/// names no usable sender. Never panics; a malformed tree resolves to `None`
/// (fail closed).
#[must_use]
pub fn resolve_concierge_config(repo_root: &Path) -> Option<ConciergeConfig> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let block = crate::config_resolver::get_path(&effective, "safehouse.concierge");
    apply_env_overrides(config_from_value(block))
}

/// Read the config layer only (no env), so unit tests can assert
/// config-over-default without mutating process env — the same split
/// `safehouse_chatops::config_from_value` uses.
#[must_use]
fn config_from_value(block: Option<&Value>) -> Option<ConciergeConfig> {
    let block = block.and_then(Value::as_object)?;
    // Strict `enabled` parse (3a's #8021 finding 3, applied here from day one):
    // absent ⇒ on (the block's presence is the opt-in), literal `true` ⇒ on,
    // literal `false` ⇒ off, and anything that is not a JSON boolean ⇒ **off,
    // with a warning**. A hand-edited `"enabled": "false"` — the JSON *string*
    // — must never be read as "not a bool, so use the default" and silently
    // switch a judgement-exercising agent on.
    match block.get("enabled") {
        None | Some(Value::Bool(true)) => {}
        Some(Value::Bool(false)) => return None,
        Some(other) => {
            log::warn!(
                "safehouse concierge: `enabled` must be a JSON boolean, got {} — \
                 treating it as false; the operator-agent persona stays OFF",
                value_kind(other)
            );
            return None;
        }
    }
    Some(ConciergeConfig {
        allowed_senders: block
            .get("allowedSenders")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(accept_sender)
                    .collect::<BTreeSet<String>>()
            })
            .unwrap_or_default(),
        persona: block
            .get("persona")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or(DEFAULT_PERSONA)
            .to_owned(),
        room: block
            .get("room")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|room| !room.is_empty())
            .map(ToOwned::to_owned),
        max_messages_per_tick: clamp_cap(
            block.get("maxMessagesPerTick").and_then(Value::as_u64),
            DEFAULT_MAX_MESSAGES_PER_TICK,
            MAX_MESSAGES_CEILING,
        ),
        max_turns_per_day: clamp_cap(
            block.get("maxTurnsPerDay").and_then(Value::as_u64),
            DEFAULT_MAX_TURNS_PER_DAY,
            MAX_TURNS_CEILING,
        ),
    })
}

/// Apply the env layer. Env can also *create* a config from nothing
/// ([`SENDERS_ENV`] alone is enough), which keeps the "env > config" precedence
/// honest and makes the live path testable without a config file — but it
/// cannot conjure an allowlist, so the fail-closed empty-allowlist rule below
/// still applies.
#[must_use]
fn apply_env_overrides(config: Option<ConciergeConfig>) -> Option<ConciergeConfig> {
    let env_senders = env_nonempty(SENDERS_ENV);
    let env_enabled = env_enabled_override();
    let mut config = match config {
        Some(config) => config,
        None if env_senders.is_some() || env_enabled == Some(true) => ConciergeConfig {
            allowed_senders: BTreeSet::new(),
            persona: DEFAULT_PERSONA.to_owned(),
            room: None,
            max_messages_per_tick: DEFAULT_MAX_MESSAGES_PER_TICK,
            max_turns_per_day: DEFAULT_MAX_TURNS_PER_DAY,
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
    if let Some(room) = env_nonempty(ROOM_ENV) {
        config.room = Some(room);
    }
    if let Some(persona) = env_nonempty(PERSONA_ENV) {
        config.persona = persona;
    }
    if let Some(n) = env_cap(MAX_MESSAGES_ENV, MAX_MESSAGES_CEILING) {
        config.max_messages_per_tick = n;
    }
    if let Some(n) = env_cap(MAX_TURNS_ENV, MAX_TURNS_CEILING) {
        config.max_turns_per_day = n;
    }
    // **Load-bearing, not defensive** — the same early return 3a's #8021
    // hardening made load-bearing there, written in from the start here. With
    // no config there is no role tick, no budget file and no relay path.
    // Deleting it would leave only `BTreeSet::contains` (always false on an
    // empty set) standing between an enabled block and an accept-nobody config
    // that still reports as enabled — one refactor away from the classic
    // empty-list-means-permissive bug. Covered by
    // `tests::an_empty_allowlist_resolves_to_no_config_at_all`.
    if config.allowed_senders.is_empty() {
        log::warn!(
            "safehouse concierge: configured but no usable entry in allowedSenders \
             (expected Matrix IDs like @you:example.org) — the operator-agent persona stays OFF"
        );
        return None;
    }
    Some(config)
}

/// Validate and normalize one allowlist entry: a Matrix ID is
/// `@localpart:server`; anything else is a typo (or a config-shape mistake like
/// a persona name) and is dropped with a warning rather than silently admitted
/// as an entry that can never match.
///
/// Deliberately a small local copy of `safehouse_chatops`'s private helper
/// rather than a `pub(crate)` widening of that reviewed module's surface: the
/// two lists gate different trust surfaces, and the warning text names which
/// one the operator got wrong.
fn accept_sender(raw: &str) -> Option<String> {
    let id = normalize_sender(raw);
    if id.is_empty() {
        return None;
    }
    if !id.starts_with('@') || !id.contains(':') {
        log::warn!("safehouse concierge: ignoring malformed allowedSenders entry {id:?}");
        return None;
    }
    Some(id)
}

/// Clamp one cap: `None`/`0`/over-ceiling all fall back to `default`.
///
/// `0` is dropped rather than honored for the same reason
/// `resolve_architect_max_proposals` drops it: a cap of zero spends a whole
/// session forbidden from producing anything, which is `enabled: false` written
/// in a way no operator means.
fn clamp_cap(raw: Option<u64>, default: u32, ceiling: u32) -> u32 {
    match raw {
        Some(n) if n > 0 && n <= u64::from(ceiling) => u32::try_from(n).unwrap_or(default),
        Some(n) if n > u64::from(ceiling) => {
            log::warn!(
                "safehouse concierge: cap {n} exceeds the ceiling of {ceiling} — using {ceiling}"
            );
            ceiling
        }
        _ => default,
    }
}

fn env_cap(key: &str, ceiling: u32) -> Option<u32> {
    let raw = env_nonempty(key)?.parse::<u64>().ok()?;
    (raw > 0).then(|| u32::try_from(raw.min(u64::from(ceiling))).unwrap_or(ceiling))
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The [`ENABLED_ENV`] override, parsed with the same strict rule
/// [`config_from_value`] applies to the config key: `None` ⇒ unset (defer to
/// the config layer), and a value present but not a recognized boolean word ⇒
/// `Some(false)`, **not** "unset" (3a's #8021 finding 3). Ignoring a typo'd
/// `off` would leave an operator who tried to switch the persona off with it
/// still on.
fn env_enabled_override() -> Option<bool> {
    let raw = env_nonempty(ENABLED_ENV)?;
    Some(parse_bool_word(&raw).unwrap_or_else(|| {
        log::warn!(
            "safehouse concierge: {ENABLED_ENV} is not a boolean \
             (expected one of 1/true/yes/on/0/false/no/off) — treating it as false; \
             the operator-agent persona stays OFF"
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

/// The JSON type of `value`, for a warning that must not echo a config blob (or
/// an unbounded string) into the log.
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
