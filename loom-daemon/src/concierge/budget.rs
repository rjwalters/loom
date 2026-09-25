//! The concierge's cost bounds: a per-tick message cap and a per-UTC-day turn
//! cap (Issue #7947).
//!
//! # Why a persisted ledger and not just a prompt number
//!
//! `autonomous.roleRunner.architectMaxProposals` (#5656) is carried into the
//! dispatch *as part of the prompt*, and its own doc explains why: "carrying
//! the cap in the prompt is what makes it an actuator limit rather than a doc
//! note". That works for Architect because the thing being capped — proposals
//! filed in one session — is entirely inside one session's own view.
//!
//! A daily turn budget is not. It spans sessions, so no prompt can hold it: a
//! fresh `claude -p` has no memory of the eleven turns that already ran today.
//! The same is true of the per-tick message cap once a turn can relay more than
//! once. So the number lives in a small JSON file the daemon owns, and the
//! persona **asks** rather than being **told** — `loom-daemon concierge budget
//! --begin-turn` either admits the turn or exits non-zero, and the session's
//! first instruction is to stop when it does.
//!
//! That is the same actuator-limit property `architectMaxProposals` has, moved
//! to the only layer that can hold it.
//!
//! # Shape
//!
//! One file per workspace root, at [`LEDGER_REL`]. Gitignored (it is in
//! `EPHEMERAL_PATTERNS`), machine-local, and disposable: deleting it costs at
//! most one day's spent budget, which is why nothing here fails hard on a
//! corrupt or unreadable file — a ledger that cannot be read is rebuilt, and a
//! ledger that cannot be written is reported to the caller rather than
//! swallowed (an unwritable ledger means the *next* turn would not see this
//! one's spend, which is a refusal-worthy condition, not a warning).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ConciergeConfig;

/// Ledger location, relative to a workspace root.
pub const LEDGER_REL: &str = ".loom/concierge/budget.json";

/// The persisted state. Deliberately tiny and forward-compatible: an unknown
/// field is ignored on read, and a file from a future version that fails to
/// parse is treated as a fresh day rather than as a fatal error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetState {
    /// UTC date (`YYYY-MM-DD`) the counters below belong to. A different date
    /// on read means "new day" and the counters reset.
    pub day: String,
    /// Turns begun today.
    pub turns: u32,
    /// Relays vetted-and-sent in the current turn.
    pub relays_this_turn: u32,
    /// The turn id the `relays_this_turn` counter belongs to, so a stale file
    /// from a crashed turn cannot silently donate its remaining per-tick
    /// allowance to the next one.
    pub turn_id: String,
    /// Daemon-originated room narrations sent today (digests, watch results).
    ///
    /// **Deliberately not a per-turn counter.** A narration is produced by
    /// deterministic code from state the daemon already has — it is not a relay
    /// and it is not an LLM conclusion — so it neither needs a turn to exist
    /// nor may it spend the relay allowance the persona needs for actual
    /// commands. What it shares with the other two counters is the thing that
    /// matters: one ledger, one UTC-day rollover key, one write-then-rename.
    ///
    /// Present by `#[serde(default)]`, so a ledger written before this field
    /// existed reads back as `0` rather than as a corrupt file (which
    /// [`BudgetLedger::read`] would have treated as a fresh day, refilling the
    /// *turn* budget as a side effect).
    pub narrations: u32,
}

/// A read-only view, for `--json` output and for tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BudgetSnapshot {
    pub day: String,
    pub turns_used: u32,
    pub turns_max: u32,
    pub relays_this_turn: u32,
    pub relays_max_per_turn: u32,
    pub turn_id: String,
    pub narrations_today: u32,
    pub narrations_max_per_day: u32,
}

/// Why the budget refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetRefusal {
    /// Today's turns are spent.
    DailyTurnsExhausted { used: u32, max: u32 },
    /// This turn has relayed as many messages as it may.
    TickMessagesExhausted { used: u32, max: u32 },
    /// Today's daemon-originated narrations (digest / watch result) are spent.
    DailyNarrationsExhausted { used: u32, max: u32 },
    /// A relay was attempted without a turn (no `--begin-turn` ran, or the
    /// ledger was cleared underneath it).
    NoTurnInProgress,
    /// The ledger could not be persisted. Refusing is deliberate: an
    /// un-persisted turn is an uncounted turn.
    NotPersisted { detail: String },
}

impl BudgetRefusal {
    /// Stable machine-readable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::DailyTurnsExhausted { .. } => "daily-turns-exhausted",
            Self::TickMessagesExhausted { .. } => "tick-messages-exhausted",
            Self::DailyNarrationsExhausted { .. } => "daily-narrations-exhausted",
            Self::NoTurnInProgress => "no-turn-in-progress",
            Self::NotPersisted { .. } => "not-persisted",
        }
    }
}

impl fmt::Display for BudgetRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DailyTurnsExhausted { used, max } => write!(
                f,
                "concierge daily turn budget spent ({used}/{max} today) — stop here; the next \
                 turn is admitted after 00:00 UTC (raise safehouse.concierge.maxTurnsPerDay to \
                 change that)"
            ),
            Self::TickMessagesExhausted { used, max } => write!(
                f,
                "this turn has already relayed {used}/{max} message(s) \
                 (safehouse.concierge.maxMessagesPerTick) — say so in the room and stop"
            ),
            Self::DailyNarrationsExhausted { used, max } => write!(
                f,
                "concierge daily room-narration budget spent ({used}/{max} today) — the digest \
                 and watch-result narrations stay quiet until 00:00 UTC (raise \
                 safehouse.concierge.maxNarrationsPerDay to change that)"
            ),
            Self::NoTurnInProgress => write!(
                f,
                "no concierge turn in progress — run `loom-daemon concierge budget \
                 --begin-turn` first"
            ),
            Self::NotPersisted { detail } => {
                write!(f, "concierge budget ledger could not be written: {detail}")
            }
        }
    }
}

impl std::error::Error for BudgetRefusal {}

/// The ledger for one workspace root.
#[derive(Debug, Clone)]
pub struct BudgetLedger {
    path: PathBuf,
}

impl BudgetLedger {
    /// The ledger for `repo_root`.
    #[must_use]
    pub fn for_root(repo_root: &Path) -> Self {
        Self {
            path: repo_root.join(LEDGER_REL),
        }
    }

    /// The ledger at an explicit path (tests).
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Where this ledger lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the state, rolled over to `today` if it belongs to another day.
    ///
    /// Never fails: a missing, unreadable or malformed file is a fresh day. The
    /// cost of being wrong in that direction is one extra day's allowance after
    /// an operator deletes the file; the cost of the other direction is an
    /// operator who cannot use the persona because a stray byte wedged it.
    #[must_use]
    pub fn read(&self, today: &str) -> BudgetState {
        let state: BudgetState = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        if state.day == today {
            state
        } else {
            BudgetState {
                day: today.to_owned(),
                ..BudgetState::default()
            }
        }
    }

    /// Admit a new turn, or refuse.
    ///
    /// `turn_id` is the caller's identifier for this session (the role runner's
    /// tick, in practice); it is recorded so per-tick relay counting cannot
    /// leak across turns.
    ///
    /// # Errors
    ///
    /// [`BudgetRefusal::DailyTurnsExhausted`] when today's turns are spent, or
    /// [`BudgetRefusal::NotPersisted`] when the ledger cannot be written.
    pub fn begin_turn(
        &self,
        config: &ConciergeConfig,
        today: &str,
        turn_id: &str,
    ) -> Result<BudgetSnapshot, BudgetRefusal> {
        let mut state = self.read(today);
        if state.turns >= config.max_turns_per_day {
            return Err(BudgetRefusal::DailyTurnsExhausted {
                used: state.turns,
                max: config.max_turns_per_day,
            });
        }
        state.turns += 1;
        state.relays_this_turn = 0;
        state.turn_id = turn_id.to_owned();
        self.write(&state)?;
        Ok(snapshot(&state, config))
    }

    /// Charge one relay against the current turn, or refuse.
    ///
    /// # Errors
    ///
    /// [`BudgetRefusal::NoTurnInProgress`] when `turn_id` does not match the
    /// recorded turn, [`BudgetRefusal::TickMessagesExhausted`] at the cap, or
    /// [`BudgetRefusal::NotPersisted`].
    pub fn charge_relay(
        &self,
        config: &ConciergeConfig,
        today: &str,
        turn_id: &str,
    ) -> Result<BudgetSnapshot, BudgetRefusal> {
        let mut state = self.read(today);
        if state.turn_id.is_empty() || state.turn_id != turn_id {
            return Err(BudgetRefusal::NoTurnInProgress);
        }
        if state.relays_this_turn >= config.max_messages_per_tick {
            return Err(BudgetRefusal::TickMessagesExhausted {
                used: state.relays_this_turn,
                max: config.max_messages_per_tick,
            });
        }
        state.relays_this_turn += 1;
        self.write(&state)?;
        Ok(snapshot(&state, config))
    }

    /// Charge one daemon-originated room narration against today's cap, or
    /// refuse.
    ///
    /// **No turn required, and none consumed.** See [`BudgetState::narrations`]
    /// for why: a digest or a watch-result line is rendered by deterministic
    /// code from state the daemon already holds, so tying it to an LLM turn
    /// would make a mechanical narration depend on a session, and charging it
    /// to [`BudgetState::relays_this_turn`] would let narration starve the
    /// persona's actual commands.
    ///
    /// Charged **before** the send, for the same reason
    /// [`Self::charge_relay`] is: a send that succeeds and then fails to be
    /// counted is an uncounted action, and that is the direction in which a
    /// wedged ledger becomes an unbounded room firehose.
    ///
    /// # Errors
    ///
    /// [`BudgetRefusal::DailyNarrationsExhausted`] at the cap, or
    /// [`BudgetRefusal::NotPersisted`].
    pub fn charge_narration(
        &self,
        config: &ConciergeConfig,
        today: &str,
    ) -> Result<BudgetSnapshot, BudgetRefusal> {
        let mut state = self.read(today);
        if state.narrations >= config.max_narrations_per_day {
            return Err(BudgetRefusal::DailyNarrationsExhausted {
                used: state.narrations,
                max: config.max_narrations_per_day,
            });
        }
        state.narrations += 1;
        self.write(&state)?;
        Ok(snapshot(&state, config))
    }

    /// The current state as a snapshot, charging nothing.
    #[must_use]
    pub fn snapshot(&self, config: &ConciergeConfig, today: &str) -> BudgetSnapshot {
        snapshot(&self.read(today), config)
    }

    fn write(&self, state: &BudgetState) -> Result<(), BudgetRefusal> {
        let fail = |detail: String| BudgetRefusal::NotPersisted { detail };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| fail(e.to_string()))?;
        }
        let body = serde_json::to_string_pretty(state).map_err(|e| fail(e.to_string()))?;
        // Write-then-rename, so a crash mid-write cannot leave a truncated
        // ledger that `read` would silently treat as a fresh day (i.e. as a
        // free refill of the budget).
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| fail(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| fail(e.to_string()))
    }
}

fn snapshot(state: &BudgetState, config: &ConciergeConfig) -> BudgetSnapshot {
    BudgetSnapshot {
        day: state.day.clone(),
        turns_used: state.turns,
        turns_max: config.max_turns_per_day,
        relays_this_turn: state.relays_this_turn,
        relays_max_per_turn: config.max_messages_per_tick,
        turn_id: state.turn_id.clone(),
        narrations_today: state.narrations,
        narrations_max_per_day: config.max_narrations_per_day,
    }
}

/// Today's UTC date as `YYYY-MM-DD` — the ledger's rollover key.
#[must_use]
pub fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}
