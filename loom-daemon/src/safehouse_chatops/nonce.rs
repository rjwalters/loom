//! Single-use, TTL-bounded, sender-bound confirmation nonces for destructive
//! ChatOps commands (Issue #7893, Phase 3a of #4196).
//!
//! # The round-trip
//!
//! 1. An allowlisted sender issues a destructive command (today: `cancel`).
//! 2. The daemon does **not** execute it. It stores the command against a
//!    freshly-minted nonce bound to that sender and replies with the nonce.
//! 3. The same sender replies `confirm <nonce>` within the TTL; the entry is
//!    **removed** and the stored command is returned for execution.
//!
//! Every other path refuses: an unknown or already-redeemed nonce (replay), an
//! expired nonce, or a nonce redeemed by a different allowlisted sender.
//!
//! # Why the daemon stores the command, not the sender
//!
//! The confirm message carries only the nonce — never a repeat of the command.
//! That is what makes the round-trip meaningful: the thing executed is the thing
//! the daemon already echoed back and the operator already read, so a confirm
//! cannot be tricked into executing a *different* command than the one it
//! appears to acknowledge.
//!
//! There was no prior nonce/confirm precedent anywhere in `loom-daemon` when
//! this landed, so this module owns the contract outright.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::command::Command;

/// Default confirm window. Long enough for a human to read a room line and
/// paste a reply, short enough that a forgotten nonce is not a standing
/// capability. Overridable via `safehouse.chatops.confirmTtlSecs`.
pub const DEFAULT_CONFIRM_TTL: Duration = Duration::from_secs(120);

/// Hard cap on simultaneously-outstanding nonces. Only allowlisted senders can
/// mint one, so this is a bookkeeping bound rather than an abuse control — but
/// an unbounded map fed by a chat room is exactly the kind of slow leak that
/// outlives the daemon restart that would have cleared it. At the cap the
/// **oldest** entry is evicted, which is also the one closest to expiry.
pub const MAX_PENDING: usize = 32;

/// Nonce length in hex characters (48 bits of v4-UUID randomness). Short enough
/// to retype by hand from a phone, far beyond guessable within a 120s window by
/// a sender who must already be on the allowlist to try.
const NONCE_LEN: usize = 12;

/// One outstanding confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    /// The **normalized** Matrix ID the nonce is bound to.
    sender: String,
    /// The command to execute on a successful confirm.
    command: Command,
    issued_at: Instant,
}

/// The outcome of redeeming a nonce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// Redeemed: execute this command. The entry is gone — a replay of the same
    /// nonce yields [`ConfirmOutcome::Unknown`].
    Confirmed(Command),
    /// No such outstanding nonce. Covers both "never issued" and "already
    /// redeemed" (replay) — deliberately indistinguishable to the sender, since
    /// telling them apart only helps someone probing the nonce space.
    Unknown,
    /// Issued, but the TTL elapsed. The entry is dropped.
    Expired,
    /// Issued to a *different* sender. The entry is left intact — one
    /// allowlisted operator must not be able to burn another's pending
    /// confirmation by guessing at it.
    WrongSender,
}

/// The ledger of outstanding confirmations.
///
/// Clock-injected throughout (`*_at(.., now: Instant)`) so expiry and replay are
/// unit-testable without sleeping.
#[derive(Debug)]
pub struct PendingConfirmations {
    ttl: Duration,
    max: usize,
    pending: HashMap<String, Pending>,
}

impl PendingConfirmations {
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, MAX_PENDING)
    }

    #[must_use]
    pub fn with_capacity(ttl: Duration, max: usize) -> Self {
        Self {
            ttl,
            max: max.max(1),
            pending: HashMap::new(),
        }
    }

    /// Number of outstanding (not-yet-pruned) nonces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    #[must_use]
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Mint a nonce for `command` bound to `sender` (already normalized by the
    /// router) and return it. The caller replies with the nonce; it is never
    /// logged or published.
    pub fn issue_at(&mut self, sender: &str, command: Command, now: Instant) -> String {
        self.prune_expired_at(now);
        while self.pending.len() >= self.max {
            let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(_, pending)| pending.issued_at)
                .map(|(nonce, _)| nonce.clone())
            else {
                break;
            };
            self.pending.remove(&oldest);
        }
        let mut nonce = new_nonce();
        while self.pending.contains_key(&nonce) {
            nonce = new_nonce();
        }
        self.pending.insert(
            nonce.clone(),
            Pending {
                sender: sender.to_owned(),
                command,
                issued_at: now,
            },
        );
        nonce
    }

    /// Redeem `nonce` on behalf of `sender`. Single-use: a successful redeem
    /// removes the entry before returning it.
    pub fn confirm_at(&mut self, sender: &str, nonce: &str, now: Instant) -> ConfirmOutcome {
        let Some(entry) = self.pending.get(nonce) else {
            return ConfirmOutcome::Unknown;
        };
        if now.saturating_duration_since(entry.issued_at) > self.ttl {
            self.pending.remove(nonce);
            return ConfirmOutcome::Expired;
        }
        if !entry.sender.eq_ignore_ascii_case(sender.trim()) {
            return ConfirmOutcome::WrongSender;
        }
        self.pending
            .remove(nonce)
            .map_or(ConfirmOutcome::Unknown, |entry| ConfirmOutcome::Confirmed(entry.command))
    }

    /// Drop every entry past its TTL. Returns how many were dropped.
    pub fn prune_expired_at(&mut self, now: Instant) -> usize {
        let before = self.pending.len();
        let ttl = self.ttl;
        self.pending
            .retain(|_, pending| now.saturating_duration_since(pending.issued_at) <= ttl);
        before - self.pending.len()
    }
}

/// Mint a fresh nonce: the leading [`NONCE_LEN`] hex characters of a v4 UUID.
///
/// `uuid` is already a direct dependency of this crate and v4 draws from the
/// OS CSPRNG via `getrandom`, so this adds no dependency and no hand-rolled
/// randomness. The truncation is deliberate — the full 32-character form is
/// hostile to retype, and 48 bits is far beyond brute-forcible inside a
/// two-minute window by a sender who is already on the allowlist.
fn new_nonce() -> String {
    uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(NONCE_LEN)
        .collect()
}
