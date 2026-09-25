//! The one vetted way any body reaches the safehouse room as the concierge
//! persona (Issue #8762, Phase 4 of #4196).
//!
//! # Why this is one function and not three
//!
//! Phase 3b had exactly one prose out-path — `loom-daemon concierge say` — and
//! the check that keeps it from becoming a command channel lived inline in the
//! CLI handler: [`vet_say`], then connect, then send. That was fine while
//! "prose the persona wrote" was the only kind of prose there was.
//!
//! Phase 4 adds two more producers ([`super::digest`] and
//! [`super::watch_narration`]), and *three* copies of a security check is how a
//! security check rots: the next producer copies whichever copy it found, and
//! the copy it found is the one somebody simplified. So the check moved here,
//! `say` was rewritten to call it, and the two new producers physically cannot
//! reach a socket by another route — [`emit`] is the only function in the crate
//! that opens one as the concierge persona.
//!
//! The ordering below is load-bearing and matches
//! [`super::relay`]'s, deliberately:
//!
//! 1. **Vet before any I/O.** A body the daemon would hear as a command is
//!    refused before a socket is opened, so a refusal cannot be a partial send.
//! 2. **Charge before the send, not after.** A send that succeeds and then
//!    fails to be counted is an uncounted action — the direction in which a
//!    wedged ledger becomes an unbounded room firehose.
//! 3. **Send last**, and let a transport failure be loud. There is no retry
//!    here and no fallback channel; the next tick reconnects.
//!
//! # What this is not
//!
//! It is not a *content* filter. [`vet_say`] asks one question — "would 3a's
//! [`inbound_command`](crate::safehouse_chatops::inbound_command) read this as
//! addressed to the daemon?" — using 3a's own
//! [`addresses_persona`](crate::safehouse_chatops::addresses_persona), so the
//! answer cannot drift away from the parser it predicts. Everything else about
//! the body is the producer's business.

use std::fmt;
use std::path::Path;

use super::budget::{today_utc, BudgetLedger, BudgetRefusal};
use super::relay::{vet_say, SayRefusal};
use super::ConciergeConfig;
use crate::safehouse::{Envelope, SafehouseClient};

/// Envelope `to` for everything this path sends: the room, not a peer.
///
/// **Not sufficient on its own to make a body inert** — 3a reads a leading
/// `@persona` / `persona:` mention as addressing regardless of `to`, which is
/// exactly why [`emit`] runs [`vet_say`] rather than trusting this constant.
pub const ROOM_TO: &str = "*";

/// Envelope type: `chat` is envelope-v1's type for human-facing prose.
const ENVELOPE_KIND: &str = "chat";

/// Which budget counter an [`emit`] spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charge {
    /// Nothing. For `loom-daemon concierge say`, whose cost is already bounded
    /// by the turn that produced it (`budget --begin-turn`): a second charge
    /// would double-count the same session.
    Never,
    /// One daemon-originated narration against
    /// `safehouse.concierge.maxNarrationsPerDay`. For output the daemon
    /// produces on its own initiative, which no turn bounds.
    DailyNarration,
}

/// Why an [`emit`] did not reach the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitRefusal {
    /// The body would be read by the daemon as an addressed command.
    Say(SayRefusal),
    /// The narration budget refused, or the ledger could not be written.
    Budget(BudgetRefusal),
    /// safehouse is off, no socket resolved, or safehoused rejected the send.
    Transport(String),
}

impl EmitRefusal {
    /// Stable machine-readable code, same convention as
    /// [`super::relay::RelayRefusal::code`].
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Say(refusal) => refusal.code(),
            Self::Budget(refusal) => refusal.code(),
            Self::Transport(_) => "transport-unavailable",
        }
    }
}

impl fmt::Display for EmitRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Say(refusal) => refusal.fmt(f),
            Self::Budget(refusal) => refusal.fmt(f),
            Self::Transport(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for EmitRefusal {}

/// Vet `body`, charge `charge`, then send it into the concierge's room.
///
/// `repo_root` resolves both the safehouse transport (socket, room, daemon
/// persona) and the budget ledger, so a caller cannot accidentally vet against
/// one workspace's persona and send to another's socket.
///
/// # Errors
///
/// [`EmitRefusal`]. Every variant is terminal for the attempt: callers report
/// and stop, exactly as they do for a [`super::relay::RelayRefusal`]. In
/// particular a refused body must never be retried through another path —
/// there is no other path.
pub async fn emit(
    repo_root: &Path,
    config: &ConciergeConfig,
    body: &str,
    charge: Charge,
) -> Result<(), EmitRefusal> {
    let safehouse = crate::safehouse::resolve_config(repo_root);
    // Gate 1 — the body must not be something the daemon would hear as a
    // command addressed to it. Checked against the *resolved* daemon persona
    // (a fleet that renamed it is still protected), and before any I/O.
    vet_say(ROOM_TO, body, &safehouse.persona).map_err(EmitRefusal::Say)?;
    if !safehouse.enabled {
        return Err(EmitRefusal::Transport(
            "safehouse is disabled for this workspace; nothing to send to".to_owned(),
        ));
    }
    let Some(socket) = crate::safehouse::resolve_socket(&safehouse) else {
        return Err(EmitRefusal::Transport(
            "no safehouse socket path resolved (safehouse.socket / LOOM_SAFEHOUSE_SOCKET)"
                .to_owned(),
        ));
    };
    // Gate 2 — budget, before the send.
    if charge == Charge::DailyNarration {
        BudgetLedger::for_root(repo_root)
            .charge_narration(config, &today_utc())
            .map_err(EmitRefusal::Budget)?;
    }
    let room = config.room(&safehouse).map(ToOwned::to_owned);
    // Deliberately not a long-lived connection (Phase 3b's choice, unchanged):
    // room output is a rare, human-paced event, and a short-lived client cannot
    // hold a socket open across a daemon restart or accumulate a push backlog.
    let mut client = SafehouseClient::connect(&socket, &config.persona, room.clone())
        .await
        .map_err(|e| {
            EmitRefusal::Transport(format!(
                "connecting to safehoused at {}: {e:#}",
                socket.display()
            ))
        })?;
    let envelope = Envelope {
        to: ROOM_TO.to_owned(),
        kind: ENVELOPE_KIND.to_owned(),
        task_id: None,
        body: body.to_owned(),
        meta: None,
    };
    client
        .send_to(&envelope, room.as_deref())
        .await
        .map_err(|e| EmitRefusal::Transport(format!("safehoused rejected the send: {e}")))
}

/// Collapse a body to one room line.
///
/// Called by both Phase 4 producers on every span of text that came from
/// somewhere else — a `WatchSpec::note` an operator typed, a workspace path, a
/// forge slug. Two reasons, in order of how much they matter:
///
/// 1. **A narration is one line.** 3a's addressing rule only looks at the
///    *leading* mention of the whole body, so embedded text cannot make a body
///    addressed — but it can make it *look* like two messages in a room
///    client, one of which appears to open with `@loom_daemon`. A human reading
///    that is being lied to about who said what, which is worth preventing even
///    though the parser is not fooled.
/// 2. **A line is bounded.** Control characters and stray `\r` do not reach the
///    room to be interpreted by whatever renders it.
///
/// This is hygiene layered on top of [`vet_say`], never a substitute for it:
/// the refusal is what makes the property structural, and this function is
/// deliberately incapable of turning an addressed body into an unaddressed one
/// (it does not touch a leading mention).
#[must_use]
pub fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space_pending = false;
    for ch in text.chars() {
        if ch.is_whitespace() || ch.is_control() {
            space_pending = !out.is_empty();
            continue;
        }
        if space_pending {
            out.push(' ');
            space_pending = false;
        }
        out.push(ch);
    }
    out
}
