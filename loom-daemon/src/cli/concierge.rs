//! `loom-daemon concierge …` — the operator-agent persona's entire I/O surface
//! (Issue #7947, Phase 3b of #4196).
//!
//! # Why the persona gets a CLI rather than free rein
//!
//! The concierge is an LLM session. Left to itself it would read the room and
//! write the room with whatever general-purpose tools its host offers, and
//! every safety property of Phase 3b would live in prose inside its prompt.
//! These four subcommands are the alternative: the one sanctioned path in and
//! out, with `loom_daemon::concierge::relay::vet_relay` sitting in the middle of it.
//!
//! | Subcommand | Direction | Gate |
//! |---|---|---|
//! | `listen` | in | drops everything not from an allowlisted sender, caps the batch |
//! | `propose` | — | a deterministic second opinion; issues no command |
//! | `relay` | out | **the boundary**: vets, charges budget, then sends |
//! | `say` | out | prose only; a body the daemon would read as addressed to it is **refused** |
//! | `budget` | — | admits or refuses a turn |
//!
//! # What this is and is not a boundary against
//!
//! It is the boundary at the layer where the persona's *actions* are made, in
//! exactly the sense `defaults/docs/guard-hooks.md` is for a Builder's shell:
//! the sanctioned path is vetted, and the refusals are mechanical rather than
//! advisory. It is **not** a sandbox. An agent with shell access can always
//! open a socket itself, which is why the unconditional backstop stays where
//! Phase 3a put it — the daemon's own sender allowlist, its closed six-verb
//! grammar, and its confirm nonce.
//!
//! # The daemon's allowlist is the outer gate, and today it is shut
//!
//! `safehoused` stamps a local socket client's `from` from its persona, so a
//! relay reaches the daemon as `from = loom_concierge`. 3a's `accept_sender`
//! discards any `safehouse.chatops.allowedSenders` entry not shaped
//! `@localpart:server`, so that name cannot be put on the list: on a stock
//! deployment **every relayed command is refused `sender-not-allowlisted`** and
//! the persona is a read-only narrator (`listen` / `propose` / `say`) whether
//! or not an operator wanted it that way. `check` reports this as
//! `relay authorized: no` rather than leaving it to be discovered as silence;
//! #8745 carries the fix. See `defaults/docs/safehouse.md` § "Can a relay from
//! this persona be authorized at all?".

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;

use loom_daemon::concierge::budget::{today_utc, BudgetLedger};
use loom_daemon::concierge::intent::{propose, Proposal, RoomMessage, Verb};
use loom_daemon::concierge::relay::{vet_relay, vet_say, Authorization, RelayRequest};
use loom_daemon::concierge::{resolve_concierge_config, ConciergeConfig};
use loom_daemon::safehouse::{Envelope, SafehouseClient, SafehouseConfig};

/// Envelope type for everything this surface sends: `chat` is the envelope-v1
/// type for human-facing prose, and a steering line typed into a room is prose
/// as far as the transport is concerned.
const ENVELOPE_KIND: &str = "chat";

/// `loom-daemon concierge <action>`.
///
/// A wrapper struct rather than an inline `#[command(subcommand)]` field on
/// `Commands::Concierge`, because `main.rs` is frozen by the file-size ratchet
/// (`.loom/docs/file-size-policy.md`): the whole subcommand shape lives here and
/// `main.rs` carries a one-line tuple variant. Same treatment, same reason, as
/// `Commands::Telemetry`.
#[derive(Args, Debug)]
pub struct ConciergeArgs {
    #[command(subcommand)]
    pub action: ConciergeAction,
}

/// Sub-actions for `loom-daemon concierge`.
#[derive(Subcommand, Debug)]
pub enum ConciergeAction {
    /// Print the resolved `safehouse.concierge` config, or report that the
    /// persona is off.
    ///
    /// Exit 0 = configured and on; exit 1 = off (absent block, `enabled:
    /// false`, or an empty/unusable allowlist). The role runner gates the
    /// `concierge` role on this same resolution, so this is the command to run
    /// when the role is not ticking.
    Check {
        /// Emit JSON instead of prose.
        #[arg(long)]
        json: bool,
    },

    /// Listen on the safehouse room for a bounded window and print the
    /// messages addressed to the persona by an allowlisted sender.
    ///
    /// Messages sent while nobody was listening are not visible: safehoused's
    /// wire protocol has no history op (`defaults/docs/safehouse.md` §"Wire
    /// protocol"), so a cadence-driven persona sees the window it is awake for.
    /// That is a deliberate Phase 3b limitation, not an oversight — a durable
    /// inbox is Phase 4 territory.
    Listen {
        /// How long to listen, in seconds.
        #[arg(long, value_name = "SECS", default_value_t = 20)]
        secs: u64,
    },

    /// Print the deterministic reading of one room message: which verb (if
    /// any), which target, or which clarification to ask for.
    ///
    /// **Advisory.** It issues no command and its conclusion is not consulted
    /// by `relay`, which re-derives every safety decision itself. Its value is
    /// as a second opinion the persona is required to consult before acting on
    /// its own reading.
    Propose {
        /// Sender, as safehoused stamped it.
        #[arg(long, value_name = "MATRIX_ID")]
        sender: String,
        /// The message text.
        #[arg(long, value_name = "TEXT")]
        body: String,
    },

    /// Vet a typed command and, if it passes every gate, send it to the daemon.
    ///
    /// This is the only path from a concierge conclusion to a daemon command.
    /// `--verb confirm` is refused with an explanation: a confirmation nonce is
    /// answered by the human it was shown to, never by the persona.
    Relay {
        /// The verb: one of `status`, `dispatch`, `cancel`, `unblock`, `watch`.
        #[arg(long, value_name = "VERB")]
        verb: String,
        /// The verb's argument (issue number, or sweep id for `cancel`).
        #[arg(long, value_name = "ARG")]
        arg: Option<String>,
        /// Sender of the message that asked for this action.
        #[arg(long, value_name = "MATRIX_ID")]
        sender: String,
        /// Text of the message that asked for this action.
        #[arg(long, value_name = "TEXT")]
        body: String,
        /// Sender of the **separate** message affirming the action. Required
        /// for `cancel` and `dispatch`.
        #[arg(long, value_name = "MATRIX_ID")]
        affirm_sender: Option<String>,
        /// Text of the affirming message. It must name the target and read as
        /// an explicit go-ahead.
        #[arg(long, value_name = "TEXT")]
        affirm_body: Option<String>,
        /// The turn this relay belongs to (see `budget --begin-turn`).
        #[arg(long, value_name = "ID")]
        turn: String,
        /// Vet and charge nothing; print what would be sent.
        #[arg(long)]
        dry_run: bool,
    },

    /// Send prose into the room as the concierge persona.
    ///
    /// Cannot carry a command, and that is enforced rather than assumed.
    /// Addressing the envelope to `*` (the room) is **not** sufficient on its
    /// own: 3a reads a leading `@persona` / `persona:` mention as addressing
    /// too, regardless of `to`. So the body is checked against 3a's own
    /// `addresses_persona` and refused (`addresses-daemon`) when the daemon
    /// would hear it as a command — which is what keeps `confirm <nonce>`
    /// unrepresentable on this path as well as on `relay`.
    Say {
        /// What to say.
        #[arg(long, value_name = "TEXT")]
        body: String,
    },

    /// Show, or open, the persona's budget.
    Budget {
        /// Admit a new turn, or exit non-zero when the daily budget is spent.
        #[arg(long)]
        begin_turn: bool,
        /// The turn id to record (required with `--begin-turn`).
        #[arg(long, value_name = "ID")]
        turn: Option<String>,
        /// Emit JSON instead of prose.
        #[arg(long)]
        json: bool,
    },
}

impl ConciergeAction {
    /// Run the action.
    ///
    /// # Errors
    ///
    /// Any refusal (persona off, sender not allowed, verb not relayable, budget
    /// spent) or transport failure. Every refusal is terminal: the persona's
    /// prompt tells it to report the refusal into the room and stop, never to
    /// retry with a relaxed request.
    pub async fn run(self) -> Result<()> {
        match self {
            Self::Check { json } => check(json),
            Self::Listen { secs } => listen(secs).await,
            Self::Propose { sender, body } => print_proposal(&sender, &body),
            Self::Relay {
                verb,
                arg,
                sender,
                body,
                affirm_sender,
                affirm_body,
                turn,
                dry_run,
            } => {
                relay(
                    &verb,
                    arg.as_deref(),
                    &sender,
                    &body,
                    affirm_sender.as_deref(),
                    affirm_body.as_deref(),
                    &turn,
                    dry_run,
                )
                .await
            }
            Self::Say { body } => say(&body).await,
            Self::Budget {
                begin_turn,
                turn,
                json,
            } => budget(begin_turn, turn.as_deref(), json),
        }
    }
}

/// Resolve the repo root once, the same way every other per-repo CLI surface
/// does.
fn root() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Resolve the config or fail with the one message that explains every way the
/// persona can be off.
fn require_config() -> Result<ConciergeConfig> {
    resolve_concierge_config(&root()).ok_or_else(|| {
        anyhow::anyhow!(
            "the concierge persona is off for this workspace: `safehouse.concierge` is absent, \
             `enabled` is false (or not a JSON boolean), or `allowedSenders` names no usable \
             Matrix ID. An empty allowlist is deny-all by design — see \
             `defaults/docs/safehouse.md` § Concierge"
        )
    })
}

fn check(json: bool) -> Result<()> {
    match resolve_concierge_config(&root()) {
        None => {
            if json {
                println!("{}", json!({ "enabled": false }));
            } else {
                println!("concierge: OFF (no usable safehouse.concierge block)");
            }
            std::process::exit(1);
        }
        Some(config) => {
            let relay_authorized = relay_is_authorized(&config);
            if json {
                println!(
                    "{}",
                    json!({
                        "enabled": true,
                        "persona": config.persona,
                        "allowedSenders": config.allowed_senders,
                        "room": config.room,
                        "maxMessagesPerTick": config.max_messages_per_tick,
                        "maxTurnsPerDay": config.max_turns_per_day,
                        "relayAuthorized": relay_authorized,
                    })
                );
            } else {
                println!("concierge: ON");
                println!("  persona:            {}", config.persona);
                println!("  allowed senders:    {}", config.allowed_senders.len());
                println!(
                    "  room:               {}",
                    config.room.as_deref().unwrap_or("<signal room>")
                );
                println!("  max messages/tick:  {}", config.max_messages_per_tick);
                println!("  max turns/day:      {}", config.max_turns_per_day);
                println!("  relay authorized:   {}", if relay_authorized { "yes" } else { "no" });
                if !relay_authorized {
                    println!(
                        "    `{}` is not on safehouse.chatops.allowedSenders, so every relayed\n    \
                         command is refused by the daemon as `sender-not-allowlisted`. 3a drops\n    \
                         any allowlist entry not shaped `@localpart:server`, so a bare persona\n    \
                         name cannot be added there — see defaults/docs/safehouse.md § \"Can a\n    \
                         relay from this persona be authorized at all?\" and #8745. `listen`,\n    \
                         `propose` and `say` are unaffected: the persona is a read-only narrator.",
                        config.persona
                    );
                }
            }
            Ok(())
        }
    }
}

fn print_proposal(sender: &str, body: &str) -> Result<()> {
    let config = require_config()?;
    let message = RoomMessage::new(sender, body);
    if !config.allows(&message.sender) {
        println!(
            "{}",
            json!({
                "id": message.id,
                "proposal": "ignore",
                "reason": "sender is not on safehouse.concierge.allowedSenders",
            })
        );
        return Ok(());
    }
    let rendered = match propose(&message) {
        Proposal::Ignore => json!({ "proposal": "ignore" }),
        Proposal::Clarify(reason) => json!({
            "proposal": "clarify",
            "ask": reason.to_string(),
        }),
        Proposal::Relay { verb, arg } => json!({
            "proposal": "relay",
            "verb": verb.as_str(),
            "arg": arg,
        }),
        Proposal::Confirmable { verb, arg } => json!({
            "proposal": "confirmable",
            "verb": verb.as_str(),
            "arg": arg,
            "note": "echo this back into the room and wait for an explicit human go-ahead in a \
                     separate message before relaying",
        }),
    };
    let mut out = rendered;
    out["id"] = json!(message.id);
    println!("{out}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn relay(
    verb: &str,
    arg: Option<&str>,
    sender: &str,
    body: &str,
    affirm_sender: Option<&str>,
    affirm_body: Option<&str>,
    turn: &str,
    dry_run: bool,
) -> Result<()> {
    let config = require_config()?;
    // `confirm` dies here, with its own explanation rather than a generic
    // "unknown verb" — see `concierge::intent::Verb`.
    let verb = Verb::parse(verb).map_err(|e| anyhow::anyhow!("{e}"))?;
    let authorization = match (affirm_sender, affirm_body) {
        (Some(s), Some(b)) => Authorization::Human(RoomMessage::new(s, b)),
        (None, None) => Authorization::None,
        _ => bail!("--affirm-sender and --affirm-body must be given together"),
    };
    let request = RelayRequest {
        origin: RoomMessage::new(sender, body),
        verb,
        arg: arg.map(ToOwned::to_owned),
        authorization,
    };
    let command = vet_relay(&config, &request)
        .map_err(|refusal| anyhow::anyhow!("refused ({}): {refusal}", refusal.code()))?;
    if dry_run {
        println!("would send: {}", command.summary());
        return Ok(());
    }
    // Budget is charged **before** the send, not after: a send that succeeds
    // and then fails to be counted is an uncounted action, which is the
    // direction that lets a wedged ledger become unlimited spend.
    let ledger = BudgetLedger::for_root(&root());
    ledger
        .charge_relay(&config, &today_utc(), turn)
        .map_err(|refusal| anyhow::anyhow!("refused ({}): {refusal}", refusal.code()))?;
    send(&config, &daemon_persona(), &command.summary()).await?;
    println!("sent: {}", command.summary());
    Ok(())
}

/// The room-facing prose path. Vetted before the socket is even opened.
const SAY_TO: &str = "*";

async fn say(body: &str) -> Result<()> {
    let config = require_config()?;
    // Refuse before connecting: a body the daemon would read as addressed to
    // it is not prose, and `relay` is the only door a command goes through.
    vet_say(SAY_TO, body, &daemon_persona())
        .map_err(|refusal| anyhow::anyhow!("refused ({}): {refusal}", refusal.code()))?;
    send(&config, SAY_TO, body).await
}

/// One connect-send-disconnect round trip as the concierge persona.
///
/// Deliberately not a long-lived connection: a relay is a rare, human-paced
/// event, and a short-lived client cannot hold a socket open across a daemon
/// restart or accumulate an unread push backlog.
async fn send(config: &ConciergeConfig, to: &str, body: &str) -> Result<()> {
    let safehouse = loom_daemon::safehouse::resolve_config(&root());
    if !safehouse.enabled {
        bail!("safehouse is disabled for this workspace; nothing to send to");
    }
    let socket = loom_daemon::safehouse::resolve_socket(&safehouse)
        .context("no safehouse socket path resolved (safehouse.socket / LOOM_SAFEHOUSE_SOCKET)")?;
    let room = config.room(&safehouse).map(ToOwned::to_owned);
    let mut client = SafehouseClient::connect(&socket, &config.persona, room.clone())
        .await
        .with_context(|| format!("connecting to safehoused at {}", socket.display()))?;
    let envelope = Envelope {
        to: to.to_owned(),
        kind: ENVELOPE_KIND.to_owned(),
        task_id: None,
        body: body.to_owned(),
        meta: None,
    };
    client
        .send_to(&envelope, room.as_deref())
        .await
        .map_err(|e| anyhow::anyhow!("safehoused rejected the send: {e}"))
}

/// Would the daemon accept a relay from this persona at all?
///
/// `safehoused` stamps a local socket client's `from` from its **persona**, so
/// a relay arrives at the daemon as `from = <concierge persona>` — a bare name,
/// not a Matrix ID. 3a's `accept_sender` drops any `chatops.allowedSenders`
/// entry not shaped `@localpart:server`, so on a stock deployment that name
/// cannot be allowlisted and every relay is refused `sender-not-allowlisted`.
/// That is a real limitation, and `check` states it rather than letting an
/// operator discover it as silence in the room (#8745 carries the fix).
///
/// Computed, not assumed: an operator whose `safehoused` stamps a Matrix-ID
/// persona (or a future 3a that can allowlist a local persona) gets `yes` here
/// with no code change — and that is also exactly the configuration in which
/// `say`'s refusal above stops being belt-and-braces and starts being the thing
/// keeping `confirm` out of the room.
fn relay_is_authorized(config: &ConciergeConfig) -> bool {
    loom_daemon::safehouse_chatops::resolve_chatops_config(&root())
        .is_some_and(|chatops| chatops.allows(&config.persona))
}

/// The daemon's own persona — the addressee every relayed command carries.
///
/// Read from the resolved safehouse config rather than hardcoded, so a fleet
/// that renamed its daemon persona does not silently start talking to nobody.
fn daemon_persona() -> String {
    loom_daemon::safehouse::resolve_config(&root()).persona
}

async fn listen(secs: u64) -> Result<()> {
    let config = require_config()?;
    let safehouse: SafehouseConfig = loom_daemon::safehouse::resolve_config(&root());
    if !safehouse.enabled {
        bail!("safehouse is disabled for this workspace; nothing to listen to");
    }
    let socket = loom_daemon::safehouse::resolve_socket(&safehouse)
        .context("no safehouse socket path resolved (safehouse.socket / LOOM_SAFEHOUSE_SOCKET)")?;
    let room = config.room(&safehouse).map(ToOwned::to_owned);
    let client = SafehouseClient::connect(&socket, &config.persona, room)
        .await
        .with_context(|| format!("connecting to safehoused at {}", socket.display()))?;
    let (reader, _writer, _id, _room) = client.into_parts();
    let mut lines = {
        use tokio::io::AsyncBufReadExt;
        reader.lines()
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut collected: Vec<serde_json::Value> = Vec::new();
    while collected.len() < config.max_messages_per_tick as usize {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(next) = tokio::time::timeout(remaining, lines.next_line()).await else {
            break; // window elapsed
        };
        let line = match next {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue; // unparseable lines are dropped, never interpreted
        };
        if value.get("event").is_none() {
            continue; // a reply echo to one of our own sends
        }
        // Addressing is 3a's own function, so "addressed to me" means exactly
        // the same thing for both personas and cannot drift.
        let Some((sender, text)) =
            loom_daemon::safehouse_chatops::inbound_command(&value, &config.persona)
        else {
            continue;
        };
        // Sender gating happens HERE, before the text is printed into an LLM's
        // context — not after. An unallowlisted sender's prose never becomes
        // something the persona has read and has to resist.
        let message = RoomMessage::new(&sender, &text);
        if !config.allows(&message.sender) {
            log::debug!("concierge: dropping a message from an unallowlisted sender");
            continue;
        }
        collected.push(json!({
            "id": message.id,
            "sender": message.sender,
            "body": message.body,
        }));
    }
    println!(
        "{}",
        json!({
            "messages": collected,
            "cap": config.max_messages_per_tick,
        })
    );
    Ok(())
}

fn budget(begin_turn: bool, turn: Option<&str>, json_out: bool) -> Result<()> {
    let config = require_config()?;
    let ledger = BudgetLedger::for_root(&root());
    let today = today_utc();
    let snapshot = if begin_turn {
        let turn = turn.context("--begin-turn needs --turn <id>")?;
        ledger
            .begin_turn(&config, &today, turn)
            .map_err(|refusal| anyhow::anyhow!("refused ({}): {refusal}", refusal.code()))?
    } else {
        ledger.snapshot(&config, &today)
    };
    if json_out {
        println!("{}", serde_json::to_string(&snapshot)?);
    } else {
        println!(
            "concierge budget {}: turns {}/{}, relays this turn {}/{}",
            snapshot.day,
            snapshot.turns_used,
            snapshot.turns_max,
            snapshot.relays_this_turn,
            snapshot.relays_max_per_turn
        );
    }
    Ok(())
}

/// Compile-time proof that the CLI cannot name a verb the persona lacks.
///
/// Not a behavioral test — a type-level one: if a `Confirm` variant is ever
/// added to [`Verb`], this match stops being exhaustive and the build fails
/// here, at the surface an operator types into, rather than silently gaining a
/// sixth relayable verb.
#[allow(dead_code)]
fn exhaustive_verb_check(verb: Verb) -> &'static str {
    match verb {
        Verb::Status => "status",
        Verb::Dispatch => "dispatch",
        Verb::Cancel => "cancel",
        Verb::Unblock => "unblock",
        Verb::Watch => "watch",
    }
}
