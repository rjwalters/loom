//! Unsatisfiable-drain detection for the auto-update roll (Issue #8998).
//!
//! # The livelock this closes
//!
//! #6007 made a refused drain deadline *retain* the roll intent instead of
//! handing the admission window back to the work finder, and bounded that
//! retention with a total paused-dispatch budget
//! ([`crate::ipc::drain_roll::drain_pending_budget`]) so a wedged sweep could
//! not starve the host of work forever. Both halves are right. Neither is
//! enough, because the budget bounds **one drain**, and the thing that
//! livelocked was the *sequence* of drains:
//!
//! 1. `auto_update` arms a roll; dispatch pauses.
//! 2. The deadline expires with sweeps still in flight. The fail-safe correctly
//!    refuses to cancel them and re-arms a widened window.
//! 3. The budget is eventually spent, the roll is abandoned, dispatch resumes.
//! 4. A new release lands (the fleet cuts one every ~30 min) — or #8514
//!    supersedes the pending roll onto it, which *restarts the budget clock* —
//!    and step 1 happens again.
//!
//! Every step is individually correct and the aggregate never terminates. On
//! 2026-09-25 two fleet dispatchers spent **21 hours** in that cycle (72 and 65
//! consecutive `"a drain-and-restart roll is already armed … skipping this
//! tick"` ticks) and never once rolled. The blocker was a genuinely-working
//! 9h32m analog-simulation sweep, so "wait for in-flight to reach zero" was
//! structurally unachievable on that host — and because `draining: true`
//! suppresses **role spawns** as well as sweep dispatch, one host produced zero
//! role ticks for 4h20m.
//!
//! # What this module adds
//!
//! An **episode**: one continuous run of "a roll is armed (or was, and is about
//! to be re-armed) and the in-flight set is not emptying". It counts drain
//! deadline expiries *across* roll lifetimes — the boundary the livelock hid
//! behind, since [`crate::ipc::DrainDescriptor::refusals`] restarts at `0` on
//! every fresh drain — and once `threshold` deadlines have expired without the
//! in-flight count ever improving, declares the wait condition **unsatisfiable**.
//!
//! The declaration is sticky, and it clears on exactly two things.
//!
//! **The fast path is an observation of `in_flight == 0`** — proof that the
//! condition the roll waits for is reachable after all. Anything weaker (a
//! decrease from 7 to 2 while a 9-hour sweep keeps running) does not clear it,
//! because re-arming there buys another budget with the same outcome, which is
//! the cycling being stopped.
//!
//! **That fast path is harder to hit than "self-clearing" suggests**, which is
//! why it is not the only one. It is an auto-update tick *sampling* `in_flight
//! == 0` on the `intervalSecs` cadence
//! ([`super::DEFAULT_AUTO_UPDATE_INTERVAL_SECS`], 900s) **with dispatch
//! running** — strictly harder than the drain's own quiescence watch, which is
//! continuous *and* observes a paused dispatcher where in-flight can only fall.
//! Once dispatch resumes, a cap-12 dispatcher refills the in-flight set as soon
//! as the long sweep ends, so a 900s sample can miss every lull.
//!
//! **So the declaration also expires on time (#9010).** Once it has stood for
//! [`resolve_roll_stall_cooldown`] seconds ([`DEFAULT_ROLL_STALL_COOLDOWN_SECS`],
//! 6h) the whole episode is dropped and the next tick arms a roll normally. If
//! the host still cannot drain, the detector re-declares after `threshold` more
//! deadlines — so the cost is **one** bounded paused-dispatch budget per cooldown
//! period, not a continuous one, and staleness is bounded rather than indefinite.
//! That is the property worth having: #8998 was *unbounded*; a retry every N
//! hours is not. The cooldown alone is deliberately the whole mechanism — no
//! "was in-flight ever seen at zero between ticks" bookkeeping — because a period
//! is the simpler thing to reason about and to state in a WARN line an operator
//! has to act on.
//!
//! # What it deliberately does not change
//!
//! - **The #6007 fail-safe.** No sweep is ever cancelled. Abandoning a roll
//!   resumes dispatch and leaves the pre-update binary running, exactly as the
//!   budget-exhaustion path already did.
//! - **Anyone else's drain.** Only a roll this daemon's auto-updater armed and
//!   labelled with an artifact target advances an episode or is abandoned — the
//!   same conservatism [`super::supersede`] applies. An operator `restart
//!   --drain` and a `fleet drain` teardown (`then_exit`) are untouched — while a
//!   declaration is standing, too: [`RollStallTracker::observe`] applies that
//!   ownership test *ahead* of the sticky flag, and `run_tick` re-tests it before
//!   reaching `DrainState::abort()`. Both halves are needed, because `fleet
//!   drain` detects a remote refusal by observing `drain.draining == false`, so
//!   aborting an operator's teardown would leave the host running and be read as
//!   a refusal nobody is told about.
//! - **Directions 2 and 3 of #8998** (age-excluding a long sweep from the drain
//!   condition; dropping the full-drain requirement for artifact rolls). Both
//!   change safety-relevant semantics and are the work that would let such a
//!   host actually update; this module only converts a silent indefinite
//!   livelock into one loud, actionable state.

use super::supersede::ArmedRoll;
use super::AutoUpdateConfig;
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Env override for the unsatisfiability threshold (Issue #8998).
pub const AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV: &str = "LOOM_AUTO_UPDATE_ROLL_STALL_DEADLINES";

/// Env override for the suppression cooldown (Issue #9010).
pub const AUTO_UPDATE_ROLL_STALL_COOLDOWN_SECS_ENV: &str =
    "LOOM_AUTO_UPDATE_ROLL_STALL_COOLDOWN_SECS";

/// Default threshold: how many drain deadlines may expire — across roll
/// lifetimes — with the in-flight count never improving before the roll's wait
/// condition is declared unsatisfiable (Issue #8998).
///
/// `3` is chosen against #6007's own geometry rather than picked round, and the
/// property it buys is stronger than "about one roll's worth": at this default
/// the detector **cannot** fire inside a single drain's own fail-safe, so it
/// always requires crossing a roll boundary — exactly the boundary the livelock
/// hid behind.
///
/// Walk [`crate::ipc::drain_roll::drain_refusal_decision`] with the default 1800s
/// drain timeout (`DRAIN_PENDING_BUDGET_MULTIPLIER = 4` → a 7200s budget;
/// windows `base·2^(n+1)`, capped at `MAX_DRAIN_RETRY_WINDOW_SECS` and at the
/// remaining budget; `Abandon` once under `MIN_DRAIN_RETRY_WINDOW_SECS = 60`):
///
/// | refusal | elapsed | remaining | decision |
/// |---|---|---|---|
/// | 1 | 1800 | 5400 | `Defer` 3600 → `refusals = 1` |
/// | 2 | 5400 | 1800 | `Defer` `min(7200, 1800)` = 1800 → `refusals = 2` |
/// | — | 7200 | 0 | `0 < 60` → `Abandon` |
///
/// `DrainDescriptor::refusals` is incremented only on `Defer`, so **the maximum
/// a single drain reaches is 2, not 3**. A lower value would fire inside that
/// still-working fail-safe (pre-empting a wait that is bounded already); a much
/// higher one is what the fleet effectively had, since each new release reset the
/// count to zero.
pub const DEFAULT_ROLL_STALL_DEADLINES: u32 = 3;

/// Resolve the threshold with precedence **env > config > default** (Issue
/// #8998), matching every other `autonomous.autoUpdate.*` knob. A zero or
/// unparseable env value falls through rather than disabling the detector: `0`
/// would declare *every* armed roll unsatisfiable on its first observation.
#[must_use]
pub fn resolve_roll_stall_deadlines(config: &AutoUpdateConfig) -> u32 {
    std::env::var(AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        // Filtered on this side too, not only at `read_auto_update_config` time:
        // a directly-constructed config must not be able to smuggle a `0` past
        // the resolver either.
        .or(config.roll_stall_deadlines.filter(|&n| n > 0))
        .unwrap_or(DEFAULT_ROLL_STALL_DEADLINES)
}

/// Default cooldown: how long a standing unsatisfiability declaration may stand
/// before it expires and one more bounded roll attempt is released (Issue #9010).
///
/// `21600` (6h) is chosen against the incident's own geometry rather than picked
/// round. Two numbers bracket it:
///
/// - **The paused-dispatch cost of one retry.** A released retry re-arms a roll
///   that this host still cannot satisfy, so it spends one #6007 paused-dispatch
///   budget — 2h at the default 1800s drain timeout — plus the further deadlines
///   it takes the detector to re-declare from a clean slate, about 3h in total.
///   A 6h cooldown therefore leaves dispatch paused for roughly 3h of every ~9h
///   cycle: about a third of this host's ticks, against the ~100% the #8998
///   livelock paused. Raise the knob to trade staleness for dispatch throughput;
///   a 1h cooldown would be barely distinguishable from the livelock it replaces.
/// - **The length of the sweep that caused it.** The blocker on 2026-09-25 was a
///   genuinely-working 9h32m analog-simulation sweep. At 6h such a sweep costs
///   *one* retry, which is the worst case worth paying for the chance that the
///   host has gone quiet since.
///
/// There is deliberately no "never retry" value — that is #8998's bug. Set it very
/// large to make the retry effectively unreachable.
pub const DEFAULT_ROLL_STALL_COOLDOWN_SECS: u64 = 21_600;

/// Resolve the suppression cooldown with precedence **env > config > default**
/// (Issue #9010), filtered exactly like [`resolve_roll_stall_deadlines`]: a zero
/// or unparseable value falls through on **both** sides rather than being taken
/// literally, since `0` would clear a declaration on the very tick it was made —
/// i.e. restore the #8998 livelock through the knob.
#[must_use]
pub fn resolve_roll_stall_cooldown(config: &AutoUpdateConfig) -> Duration {
    std::env::var(AUTO_UPDATE_ROLL_STALL_COOLDOWN_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.roll_stall_cooldown_secs.filter(|&s| s > 0))
        .map_or_else(|| Duration::from_secs(DEFAULT_ROLL_STALL_COOLDOWN_SECS), Duration::from_secs)
}

/// The operator-facing finding: this host's roll cannot complete, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollStallReport {
    /// Drain deadlines that have expired during this episode, summed across
    /// every roll lifetime in it.
    pub deadlines: u32,
    /// The in-flight sweep count at the observation that produced this report.
    pub in_flight: usize,
    /// The lowest in-flight count seen anywhere in the episode — the number that
    /// makes "it is not emptying" concrete rather than asserted.
    pub floor: usize,
    /// How long the **episode** has run, in seconds — measured from its first
    /// observation, not from any one roll's arming. An episode deliberately spans
    /// ticks with nothing armed (that gap is where the 21h cycle reset itself),
    /// so this is longer than the live roll has been armed and is named for what
    /// it measures.
    pub episode_secs: u64,
    /// The artifact identity the abandoned roll was targeting, when known.
    pub target: Option<String>,
    /// #9010: how long this declaration may stand before it expires on its own
    /// and one more bounded roll attempt is released, in seconds.
    pub cooldown_secs: u64,
    /// #9010: how many cooldown-released retries this host has already spent
    /// since the last time an auto-update tick saw it idle (0 on a first
    /// declaration). Names the difference between "stalled once" and "stalled all
    /// week", which the deadline count cannot: a retry resets that count.
    pub retries: u32,
}

impl RollStallReport {
    /// The WARN line and `status` note. Loud, and every sentence is an action or
    /// a fact an operator needs to choose between them.
    #[must_use]
    pub fn note(&self) -> String {
        let Self {
            deadlines,
            in_flight,
            floor,
            episode_secs,
            target,
            cooldown_secs,
            retries,
        } = self;
        let target = target
            .as_deref()
            .map_or_else(String::new, |t| format!(" (target {t})"));
        // Named only when it has happened, so a first declaration does not carry a
        // "0 retries spent" clause nobody needs to read.
        let spent = if *retries > 0 {
            format!(
                " This host has already spent {retries} cooldown retry(ies) since an auto-update \
                 tick last saw it idle, so it is persistently — not momentarily — unable to drain."
            )
        } else {
            String::new()
        };
        format!(
            "ABANDONING the drain-and-restart roll{target}: its wait condition is UNSATISFIABLE. \
             {deadlines} drain deadline(s) have expired across {episode_secs}s of stalled episode \
             (since the first refusal) and the \
             in-flight sweep count has never improved on {floor} ({in_flight} in flight now) — \
             re-arming right now would pause dispatch for another budget and reach the same \
             refusal, which is the loop that cost two fleet hosts 21h of paused dispatch (#8998). \
             The roll intent is DISCARDED and NORMAL DISPATCH RESUMES, including role spawns. No \
             sweep was cancelled and the pre-update binary keeps running (the #6007 fail-safe is \
             unchanged).{spent} THIS HOST WILL NOT AUTO-UPDATE until whichever comes first of: an \
             auto-update tick SAMPLES in-flight at zero (the fast path — checked once per interval, \
             with dispatch running, so on a continuously-busy host that sample may not land), or \
             this suppression's {cooldown_secs}s COOLDOWN EXPIRES, at which point the roll re-arms \
             for ONE more bounded attempt and is declared again if the host still cannot drain \
             (#9010). Staleness is therefore BOUNDED, not indefinite — but a host that stays this \
             busy will keep paying about one paused-dispatch budget per cooldown, so ACT rather \
             than wait if you want it updated sooner: find the long-running sweep with \
             `loom-daemon list`, then either let it finish, cancel it with `loom-daemon cancel \
             --sweep <id>`, or force the roll through with `loom-daemon restart --drain \
             --force-after-timeout` (which DOES cancel it)."
        )
    }
}

/// The other stall this loop can declare (Issue #10712): the fleet floor
/// (`loom_min_version`) is above the running version and above every published
/// release, so no roll can satisfy it.
///
/// Unlike a [`RollStallReport`] there is nothing to abandon: no roll was armed
/// for the floor, and none is. The host keeps dispatching on its current
/// version and ordinary autoUpdate rolls still apply. The report exists so the
/// stall is typed, alerted at ERROR, and visible in `status`, instead of a
/// floor that silently does nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorStallReport {
    /// The floor in force, `X.Y.Z`.
    pub floor: String,
    /// The running version, below the floor.
    pub running: String,
    /// The newest published release's version, also below the floor.
    pub newest: String,
}

impl FloorStallReport {
    /// The ERROR line and `status` note.
    #[must_use]
    pub fn note(&self) -> String {
        let Self {
            floor,
            running,
            newest,
        } = self;
        format!(
            "FLEET FLOOR UNSATISFIABLE: loom_min_version {floor} is above every published release \
             (newest {newest}), so this host (running {running}) cannot roll to it. Most likely a \
             typo in the fleet store's loom_min_version. DISPATCH CONTINUES on {running}: the \
             floor never refuses work, and ordinary autoUpdate rolls still apply. Fix the floor, \
             or publish a release at or above {floor}."
        )
    }
}

/// One episode of drain-deadline accounting that survives roll re-arms
/// (Issue #8998). Pure — no I/O, no clock of its own — so the whole
/// widen/re-arm/abandon sequence is a unit test rather than a 21-hour
/// reproduction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RollStallTracker {
    /// How many non-improving deadlines end the episode.
    threshold: u32,
    /// #9010: how long a declaration may stand before it expires on time,
    /// releasing one more bounded roll attempt. Seconds rather than a `Duration`
    /// because every comparison here is against a `chrono` wall-clock delta.
    cooldown_secs: u64,
    /// Deadline expiries inherited from rolls that have already ended
    /// (budget-abandoned, superseded, or replaced by a newer release).
    carried_deadlines: u32,
    /// The live roll's `refusals` as of the previous observation. A *decrease*
    /// is how a new roll replacing an old one is detected between two ticks.
    live_refusals: Option<u32>,
    /// The lowest in-flight count observed in this episode.
    floor: Option<usize>,
    /// The episode's deadline total when `floor` last improved — the rebase
    /// point, so a host that is genuinely draining keeps earning fresh patience.
    deadlines_at_floor: u32,
    /// When the episode's first observation was made.
    since: Option<DateTime<Utc>>,
    /// The most recent roll target seen, for the report.
    target: Option<String>,
    /// Sticky once set: cleared by an `in_flight == 0` observation (the fast
    /// path) or by `cooldown_secs` elapsing since `declared_at` (#9010).
    unsatisfiable: bool,
    /// When the standing declaration was made — the cooldown's origin. `None`
    /// exactly when `unsatisfiable` is `false`.
    declared_at: Option<DateTime<Utc>>,
    /// #9010: cooldown-released retries spent since the last idle observation.
    /// Survives a cooldown clear (that is the count's whole point) and is zeroed
    /// by an idle observation, which is proof the host is no longer stuck.
    retries: u32,
    /// One-shot, drained by [`Self::take_retry_note`]: the WARN line owed to the
    /// operator on the tick a cooldown released a retry. Held here rather than
    /// logged inline so this module stays pure.
    retry_note: Option<String>,
}

impl Default for RollStallTracker {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_ROLL_STALL_DEADLINES,
            cooldown_secs: DEFAULT_ROLL_STALL_COOLDOWN_SECS,
            carried_deadlines: 0,
            live_refusals: None,
            floor: None,
            deadlines_at_floor: 0,
            since: None,
            target: None,
            unsatisfiable: false,
            declared_at: None,
            retries: 0,
            retry_note: None,
        }
    }
}

impl RollStallTracker {
    /// Override the threshold (the resolved `rollStallDeadlines` knob).
    pub(super) fn set_threshold(&mut self, threshold: u32) {
        self.threshold = threshold.max(1);
    }

    /// Override the suppression cooldown (the resolved `rollStallCooldownSecs`
    /// knob, #9010). Floored at one second for the same reason `set_threshold`
    /// floors at one: a zero cooldown would clear a declaration on the tick it was
    /// made, restoring the very livelock this module exists to stop. The resolver
    /// already drops a zero on both tiers; this is the second line of defence for
    /// a directly-constructed value.
    pub(super) fn set_cooldown(&mut self, cooldown: Duration) {
        self.cooldown_secs = cooldown.as_secs().max(1);
    }

    /// Drop the whole episode, keeping only the resolved knobs and the retry
    /// count the caller decides on (`0` from the idle fast path — the host is
    /// demonstrably not stuck; `retries + 1` from a cooldown expiry).
    fn clear_episode(&mut self, retries: u32) {
        *self = Self {
            threshold: self.threshold,
            cooldown_secs: self.cooldown_secs,
            retries,
            ..Self::default()
        };
    }

    /// Whether a standing declaration has stood for its full cooldown. False when
    /// nothing is declared, and false if the wall clock ran backwards between
    /// ticks (`num_seconds()` goes negative) rather than treating that as an
    /// expiry.
    fn cooldown_expired(&self, now: DateTime<Utc>) -> bool {
        self.declared_at.is_some_and(|declared| {
            (now - declared).num_seconds() >= i64::try_from(self.cooldown_secs).unwrap_or(i64::MAX)
        })
    }

    /// Take the WARN line owed for a cooldown-released retry, if the most recent
    /// [`Self::observe`] released one. One-shot.
    pub(super) fn take_retry_note(&mut self) -> Option<String> {
        self.retry_note.take()
    }

    /// Whether an episode is running — i.e. whether a tick with no armed roll
    /// still needs to read the in-flight count. `true` while a declaration is
    /// standing, which is what lets the suppression clear itself: an idle sample
    /// (fast path) or its cooldown expiring (#9010) are both observed on those
    /// otherwise-idle ticks.
    pub(super) fn is_active(&self) -> bool {
        self.since.is_some()
    }

    /// Fold one tick's observation in.
    ///
    /// `armed` is the live drain's identity (`None` when no drain is armed);
    /// `in_flight` is the cross-root non-terminal sweep count. Returns `Some`
    /// once the wait condition is unsatisfiable — on that tick and every
    /// subsequent one until the host is observed idle, so the state is
    /// re-reported rather than logged once and forgotten.
    pub(super) fn observe(
        &mut self,
        now: DateTime<Utc>,
        armed: Option<&ArmedRoll>,
        in_flight: usize,
    ) -> Option<RollStallReport> {
        // The condition the roll waits for is satisfied right now: whatever the
        // episode believed, it is over. The *fast* way a declaration clears (#9010
        // adds the slow one), and proof the host is not stuck, so the retry count
        // goes back to zero with everything else.
        if in_flight == 0 {
            self.clear_episode(0);
            return None;
        }
        // Not ours to reason about: a `fleet drain` teardown, or an untargeted
        // drain (an operator `restart --drain`, or a source-path roll this daemon
        // cannot key an artifact identity on). Freeze the episode rather than
        // counting someone else's deadlines.
        //
        // This guard is deliberately **above** the sticky check below, and that
        // ordering is load-bearing: `run_tick` abandons whatever drain is armed
        // on the tick this returns `Some`, so reporting a standing declaration
        // while a foreign drain is armed cancels *that* drain — the host an
        // operator asked to tear down never stops, and `fleet drain` reads the
        // cleared `draining` flag as a refusal it never reports. Returning `None`
        // here instead leaves the tick to #6007's pre-existing teardown /
        // untargeted skip, which publishes its own note and touches nothing. The
        // declaration itself is retained, not discarded, so the suppression
        // resumes the moment the foreign drain ends. Pinned by
        // `a_latched_declaration_does_not_report_a_teardown_drain_armed_afterwards`.
        if matches!(armed, Some(roll) if roll.then_exit || roll.target.is_none()) {
            return None;
        }
        if self.unsatisfiable {
            // Issue #9010: the declaration expires on TIME as well as on an idle
            // sample. Deliberately *below* the foreign-drain guard above, so a
            // cooldown can never release a retry while an operator's teardown or
            // untargeted drain is armed — the episode stays frozen for the same
            // reason it does not advance there, and the cooldown is re-checked (and
            // by then long expired) on the first tick after that drain ends.
            //
            // Clearing the episode outright, rather than just the flag, is what
            // makes the retry bounded instead of a return to #8998: the next tick
            // arms a roll normally and the detector has to earn `threshold` fresh
            // non-improving deadlines — one #6007 paused-dispatch budget — before it
            // declares again. So the host pays one budget per cooldown period, and
            // the worst case is a duty cycle, not a livelock.
            if self.cooldown_expired(now) {
                let stood_secs = self.report(now, in_flight).episode_secs;
                let retries = self.retries.saturating_add(1);
                let cooldown_secs = self.cooldown_secs;
                let target = self
                    .target
                    .as_deref()
                    .map_or_else(String::new, |t| format!(" (last target {t})"));
                self.clear_episode(retries);
                self.retry_note = Some(format!(
                    "the unsatisfiable-roll suppression{target} has stood for its full \
                     {cooldown_secs}s cooldown ({stood_secs}s of stalled episode, {in_flight} \
                     sweep(s) still in flight) — RELEASING ONE BOUNDED RETRY (#9010): the next \
                     tick arms a drain-and-restart roll again, and if this host still cannot \
                     drain it is declared unsatisfiable once more after \
                     {threshold} further deadline(s) rather than cycling. Retry {retries} since \
                     an auto-update tick last saw this host idle.",
                    threshold = self.threshold
                ));
                return None;
            }
            return Some(self.report(now, in_flight));
        }
        match armed {
            Some(roll) => {
                // `refusals` is monotonic *within* a drain and restarts at 0 on
                // the next one, so a decrease means a roll ended and another
                // took its place: bank what the old one accumulated.
                if let Some(previous) = self.live_refusals {
                    if roll.refusals < previous {
                        self.carried_deadlines = self.carried_deadlines.saturating_add(previous);
                    }
                }
                self.live_refusals = Some(roll.refusals);
                self.target = roll.target.clone();
            }
            // The roll ended between ticks (budget-abandoned, superseded, or
            // aborted) and nothing is armed yet. The episode continues — this
            // gap is precisely where the 21h cycle reset itself.
            None => {
                if let Some(previous) = self.live_refusals.take() {
                    self.carried_deadlines = self.carried_deadlines.saturating_add(previous);
                }
                // Nothing has been armed in this episode yet, so there is no
                // episode: a busy host with no roll armed is not stalled.
                self.since?;
            }
        }
        if self.since.is_none() {
            self.since = Some(now);
        }
        let deadlines = self.deadlines();
        match self.floor {
            // The episode's very first reading establishes the floor without
            // consuming patience: `deadlines_at_floor` stays `0`, so the
            // threshold is measured against the episode's whole deadline count
            // rather than against whatever the count happened to be when this
            // tracker first looked.
            None => self.floor = Some(in_flight),
            Some(floor) if in_flight < floor => {
                self.floor = Some(in_flight);
                self.deadlines_at_floor = deadlines;
            }
            Some(_) => {}
        }
        if deadlines.saturating_sub(self.deadlines_at_floor) >= self.threshold {
            self.unsatisfiable = true;
            // #9010's cooldown runs from the declaration, not from the episode's
            // first observation, so a re-declaration always buys a full cooldown
            // before the next retry.
            self.declared_at = Some(now);
            return Some(self.report(now, in_flight));
        }
        None
    }

    /// Deadlines expired in this episode: banked from ended rolls plus the live
    /// roll's own refusals.
    fn deadlines(&self) -> u32 {
        self.carried_deadlines
            .saturating_add(self.live_refusals.unwrap_or(0))
    }

    fn report(&self, now: DateTime<Utc>, in_flight: usize) -> RollStallReport {
        RollStallReport {
            deadlines: self.deadlines(),
            in_flight,
            floor: self.floor.unwrap_or(in_flight),
            episode_secs: self
                .since
                .map_or(0, |since| u64::try_from((now - since).num_seconds()).unwrap_or(0)),
            target: self.target.clone(),
            cooldown_secs: self.cooldown_secs,
            retries: self.retries,
        }
    }
}

// Issue #9010: these two live here, rather than beside
// [`super::AutoUpdateState::with_roll_stall_deadlines`] in `auto_update.rs`, because
// that file is already over `.loom/docs/file-size-policy.md`'s ratchet threshold —
// this module is the sanctioned sibling to grow instead. Both reach the private
// `roll_stall` field, which Rust's privacy rules permit from any descendant of the
// defining module (`auto_update::roll_stall` is one).
impl super::AutoUpdateState {
    /// Set #9010's suppression cooldown (the resolved `rollStallCooldownSecs`
    /// knob) — how long a standing declaration may stand before it expires and
    /// releases one more bounded roll attempt. A builder for the same reason
    /// [`super::AutoUpdateState::with_roll_stall_deadlines`] is.
    #[must_use]
    pub fn with_roll_stall_cooldown(mut self, cooldown: Duration) -> Self {
        self.roll_stall.set_cooldown(cooldown);
        self
    }

    /// Take the WARN line owed when #9010's cooldown just released a bounded
    /// retry on the most recent `observe_roll_stall`. One-shot.
    pub(super) fn take_roll_stall_retry_note(&mut self) -> Option<String> {
        self.roll_stall.take_retry_note()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn roll(refusals: u32) -> ArmedRoll {
        ArmedRoll {
            target: Some("v0.19.390@aaaa".to_string()),
            pending: refusals > 0,
            then_exit: false,
            refusals,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    // ---- the knob -----------------------------------------------------------

    #[test]
    fn the_threshold_falls_back_from_config_to_the_default() {
        let empty = AutoUpdateConfig::default();
        assert_eq!(resolve_roll_stall_deadlines(&empty), DEFAULT_ROLL_STALL_DEADLINES);
        let configured = AutoUpdateConfig {
            roll_stall_deadlines: Some(7),
            ..AutoUpdateConfig::default()
        };
        assert_eq!(resolve_roll_stall_deadlines(&configured), 7);
    }

    #[test]
    fn a_zero_threshold_never_disables_the_detector() {
        // `0` would declare every armed roll unsatisfiable on sight, so it is
        // dropped exactly like a zero `settleSecs` / `deferDeadlineSecs`.
        let configured = AutoUpdateConfig {
            roll_stall_deadlines: Some(0),
            ..AutoUpdateConfig::default()
        };
        assert_eq!(
            resolve_roll_stall_deadlines(&configured),
            DEFAULT_ROLL_STALL_DEADLINES,
            "a 0 is filtered at read time AND in the resolver"
        );
        let mut tracker = RollStallTracker::default();
        tracker.set_threshold(0);
        assert!(
            tracker.observe(at(0), Some(&roll(0)), 2).is_none(),
            "a 0 threshold is floored at 1, so the first observation cannot fire"
        );
    }

    #[test]
    fn the_cooldown_falls_back_from_config_to_the_default() {
        let empty = AutoUpdateConfig::default();
        assert_eq!(
            resolve_roll_stall_cooldown(&empty),
            Duration::from_secs(DEFAULT_ROLL_STALL_COOLDOWN_SECS)
        );
        let configured = AutoUpdateConfig {
            roll_stall_cooldown_secs: Some(3600),
            ..AutoUpdateConfig::default()
        };
        assert_eq!(resolve_roll_stall_cooldown(&configured), Duration::from_secs(3600));
    }

    #[test]
    fn a_zero_cooldown_never_collapses_the_suppression_to_nothing() {
        // `0` would clear a declaration on the tick it was made — #8998's livelock
        // re-entered through the knob — so it is dropped on both tiers, exactly
        // like a zero `rollStallDeadlines`.
        let configured = AutoUpdateConfig {
            roll_stall_cooldown_secs: Some(0),
            ..AutoUpdateConfig::default()
        };
        assert_eq!(
            resolve_roll_stall_cooldown(&configured),
            Duration::from_secs(DEFAULT_ROLL_STALL_COOLDOWN_SECS),
            "a 0 is filtered at read time AND in the resolver"
        );
        let mut tracker = RollStallTracker::default();
        tracker.set_cooldown(Duration::from_secs(0));
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        assert!(
            tracker.observe(at(2700), None, 2).is_some(),
            "a 0 cooldown is floored at 1s, so the declaration cannot expire on the \
             very tick that made it"
        );
        assert!(tracker.take_retry_note().is_none());
    }

    // ---- the happy path: a busy host that is actually draining ---------------

    #[test]
    fn an_idle_observation_clears_the_episode_immediately() {
        let mut tracker = RollStallTracker::default();
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        assert!(tracker.observe(at(3600), Some(&roll(4)), 2).is_some(), "declared by now");
        // Zero in flight: the wait condition is reachable after all.
        assert!(tracker.observe(at(4500), Some(&roll(4)), 0).is_none());
        assert!(!tracker.is_active(), "the episode is gone, not merely quiet");
        assert!(
            tracker.observe(at(5400), Some(&roll(1)), 2).is_none(),
            "a fresh roll starts from a clean slate"
        );
    }

    #[test]
    fn a_first_attempt_roll_inside_its_own_deadline_is_never_declared() {
        let mut tracker = RollStallTracker::default();
        // `refusals == 0` for as many ticks as the drain's own deadline spans.
        for tick in 0..8 {
            assert_eq!(tracker.observe(at(tick * 300), Some(&roll(0)), 3), None);
        }
    }

    #[test]
    fn a_decreasing_in_flight_count_rebases_the_patience() {
        let mut tracker = RollStallTracker::default();
        // Three deadlines expire, but the host is genuinely draining 4 → 3 → 2 →
        // 1, so each improvement buys another `threshold` deadlines.
        for (deadline, in_flight) in [(0, 4), (1, 3), (2, 2), (3, 1)] {
            assert_eq!(
                tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), in_flight),
                None,
                "deadline {deadline} with {in_flight} in flight must not declare"
            );
        }
        // It then stops draining: three more deadlines at the same floor.
        assert_eq!(tracker.observe(at(3600), Some(&roll(4)), 1), None);
        assert_eq!(tracker.observe(at(4500), Some(&roll(5)), 1), None);
        let report = tracker.observe(at(5400), Some(&roll(6)), 1).unwrap();
        assert_eq!(report.floor, 1);
        assert_eq!(report.deadlines, 6);
    }

    // ---- the defect: deadlines counted ACROSS roll lifetimes -----------------

    #[test]
    fn deadlines_accumulate_across_a_roll_that_is_abandoned_and_re_armed() {
        let mut tracker = RollStallTracker::default();
        // Roll A reaches two refusals, then spends its budget and is abandoned.
        assert_eq!(tracker.observe(at(0), Some(&roll(1)), 2), None);
        assert_eq!(tracker.observe(at(900), Some(&roll(2)), 2), None);
        // Nothing armed: dispatch resumed, the pre-#8998 reset point.
        assert_eq!(tracker.observe(at(1800), None, 2), None);
        // Roll B is armed for the next release and refuses once. Pre-#8998 that
        // was "retry 1" all over again; now it is deadline three.
        let report = tracker.observe(at(2700), Some(&roll(1)), 2).unwrap();
        assert_eq!(report.deadlines, 3, "2 banked from roll A + 1 from roll B");
        assert_eq!(report.in_flight, 2);
        assert_eq!(report.episode_secs, 2700, "the EPISODE's span, including the unarmed tick");
        assert_eq!(report.target.as_deref(), Some("v0.19.390@aaaa"));
    }

    #[test]
    fn deadlines_accumulate_across_a_supersede_observed_without_an_idle_gap() {
        let mut tracker = RollStallTracker::default();
        assert_eq!(tracker.observe(at(0), Some(&roll(1)), 7), None);
        assert_eq!(tracker.observe(at(900), Some(&roll(2)), 7), None);
        // #8514 superseded the pending roll onto a newer release between ticks:
        // a brand-new drain, `refusals` back to 0 — and, before #8998, a
        // brand-new paused-dispatch budget too.
        let mut newer = roll(0);
        newer.target = Some("v0.19.391@bbbb".to_string());
        assert_eq!(tracker.observe(at(1800), Some(&newer), 7), None, "0 new deadlines yet");
        newer.refusals = 1;
        let report = tracker.observe(at(2700), Some(&newer), 7).unwrap();
        assert_eq!(report.deadlines, 3);
        assert_eq!(
            report.target.as_deref(),
            Some("v0.19.391@bbbb"),
            "the report names the roll that was actually armed"
        );
    }

    #[test]
    fn the_declaration_is_re_reported_every_tick_until_the_host_goes_idle() {
        let mut tracker = RollStallTracker::default();
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        assert!(tracker.is_active());
        // No roll armed any more (it was abandoned), host still busy.
        let again = tracker.observe(at(4500), None, 2).unwrap();
        assert_eq!(again.in_flight, 2);
        let and_again = tracker.observe(at(5400), None, 5).unwrap();
        assert_eq!(and_again.in_flight, 5);
        assert_eq!(and_again.floor, 2, "the floor is the episode's best, not this tick's");
    }

    // ---- the bounded cooldown retry (#9010) ---------------------------------

    /// The headline of #9010: the declaration expires on TIME, so a host whose
    /// `in_flight == 0` sample never lands is stale for a bounded period rather
    /// than indefinitely.
    #[test]
    fn a_standing_declaration_expires_on_its_cooldown_with_no_idle_sample_ever() {
        let mut tracker = RollStallTracker::default();
        tracker.set_cooldown(Duration::from_secs(7200));
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        let declared = tracker.observe(at(3600), Some(&roll(3)), 2).unwrap();
        assert_eq!(declared.cooldown_secs, 7200, "the note must name the bound it promises");
        assert_eq!(declared.retries, 0, "no retry has been spent yet");

        // Still standing one second short of the cooldown — in-flight never once
        // sampled at zero along the way.
        assert!(
            tracker.observe(at(2700 + 7199), None, 2).is_some(),
            "the suppression holds for the whole cooldown"
        );
        assert!(tracker.take_retry_note().is_none(), "and nothing is claimed yet");

        // …and expires on the tick that reaches it.
        assert_eq!(
            tracker.observe(at(2700 + 7200), None, 2),
            None,
            "the cooldown releases the suppression without any idle observation"
        );
        assert!(!tracker.is_active(), "the episode is gone, so the next tick arms a roll");
        let retry = tracker.take_retry_note().unwrap();
        assert!(retry.contains("RELEASING ONE BOUNDED RETRY"), "{retry}");
        assert!(retry.contains("7200s cooldown"), "{retry}");
        assert!(retry.contains("Retry 1"), "{retry}");
        assert!(tracker.take_retry_note().is_none(), "the note is one-shot");
    }

    /// The retry is a *bounded* attempt, not a return to #8998: a host that still
    /// cannot drain must earn `threshold` fresh deadlines and be declared again —
    /// and the second declaration must say it is the second.
    #[test]
    fn a_host_that_still_cannot_drain_re_declares_after_a_full_threshold_again() {
        let mut tracker = RollStallTracker::default();
        tracker.set_cooldown(Duration::from_secs(7200));
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        assert!(tracker.observe(at(3600), None, 2).is_some(), "declared");
        assert_eq!(tracker.observe(at(2700 + 7200), None, 2), None, "cooldown expired");
        tracker.take_retry_note();

        // The retry arms a roll that refuses its first two deadlines: NOT enough,
        // because the slate was wiped — the cost is one bounded budget, not a
        // declaration on sight.
        let base = 2700 + 7200;
        assert_eq!(tracker.observe(at(base + 900), Some(&roll(1)), 2), None);
        assert_eq!(tracker.observe(at(base + 1800), Some(&roll(2)), 2), None);
        let again = tracker.observe(at(base + 2700), Some(&roll(3)), 2).unwrap();
        assert_eq!(again.deadlines, 3, "a full fresh threshold, not a carried-over one");
        assert_eq!(again.retries, 1, "and the note says one retry has already been spent");
        assert!(again.note().contains("already spent 1 cooldown retry(ies)"), "{}", again.note());
        // The new declaration re-arms the cooldown from scratch rather than
        // inheriting the first one's (already-expired) clock.
        assert!(
            tracker.observe(at(base + 2700 + 7199), None, 2).is_some(),
            "a re-declaration buys a full cooldown of its own"
        );
    }

    /// The acceptance property, stated as a budget count. Run a host that never
    /// drains and never samples idle across several cooldown periods and count
    /// what it spends: #8998's livelock paused dispatch essentially continuously,
    /// and the pre-#9010 suppression spent one budget and then never updated
    /// again. This must spend **one budget per cooldown period** — a bounded duty
    /// cycle, in both directions.
    #[test]
    fn several_cooldown_periods_spend_one_paused_dispatch_budget_each() {
        // Production geometry at the defaults: a 900s tick, a drain that refuses
        // at 1800s and 5400s and is budget-abandoned at 7200s (#6007's
        // `4 × --timeout`), and a host stuck at 3 in flight forever.
        const TICK: i64 = 900;
        const BUDGET: i64 = 7200;
        const COOLDOWN: i64 = DEFAULT_ROLL_STALL_COOLDOWN_SECS as i64;
        const HORIZON: i64 = 3 * 86_400; // three days

        let mut tracker = RollStallTracker::default();
        let mut armed_since: Option<i64> = None;
        let mut arms = 0_u32;
        let mut declarations = 0_u32;
        let mut paused_ticks = 0_u32;
        let mut total_ticks = 0_u32;
        let mut retries_at_last_declaration = 0_u32;
        // What `run_tick` does with the tracker's verdict: a `Some` returns early,
        // so no roll is armed on the ticks a declaration is standing.
        let mut suppressed = false;

        let mut t = 0;
        while t < HORIZON {
            // An auto-update tick with nothing armed and no suppression standing
            // arms a roll for the (always newer) release — the step that made
            // #8998 a loop. Dispatch pauses for as long as it stays armed.
            if armed_since.is_none() && !suppressed {
                armed_since = Some(t);
                arms += 1;
            }
            let live = armed_since.map(|since| {
                let elapsed = t - since;
                roll(match elapsed {
                    e if e >= 5400 => 2,
                    e if e >= 1800 => 1,
                    _ => 0,
                })
            });
            if live.is_some() {
                paused_ticks += 1;
            }
            total_ticks += 1;

            let was_suppressed = suppressed;
            let report = tracker.observe(at(t), live.as_ref(), 3);
            suppressed = report.is_some();
            if let Some(report) = report {
                // A standing declaration is re-reported every tick, so only the
                // transition into one is a fresh give-up.
                if !was_suppressed {
                    declarations += 1;
                    retries_at_last_declaration = report.retries;
                }
                armed_since = None; // `run_tick` abandons it; dispatch resumes.
            } else if armed_since.is_some_and(|since| t - since >= BUDGET) {
                armed_since = None; // #6007 spent the budget on its own.
            }
            t += TICK;
        }

        // Three days at a 6h cooldown: one declaration per ~9h cycle (one 2h
        // budget + the deadlines to re-declare + the cooldown), so single digits —
        // NOT one per roll, and emphatically not "1, then never again".
        let cycles = u32::try_from(HORIZON / (COOLDOWN + BUDGET + 2 * 1800)).unwrap();
        assert_eq!(declarations, 8, "one declaration per cooldown period, no more");
        assert!(
            (cycles..=cycles + 1).contains(&declarations),
            "{declarations} declarations is not ~{cycles} cooldown periods"
        );
        assert_eq!(
            arms,
            declarations * 2,
            "each period arms exactly two rolls: one that spends #6007's budget, one that \
             reaches the deadline that re-declares"
        );
        // The property the issue names: a bounded duty cycle. #8998's host was
        // paused essentially always; this one pays one budget's worth per
        // cooldown — 12 of every 36 ticks, i.e. ~3h of every ~9h.
        assert_eq!(paused_ticks, 96, "12 paused ticks per cooldown period, eight times");
        assert!(
            paused_ticks * 100 / total_ticks <= 34,
            "dispatch paused for {paused_ticks}/{total_ticks} ticks — not a bounded duty cycle"
        );
        // And each period is a *retry*, not a reset: the count keeps rising, so the
        // WARN line always says how long this host has really been stuck rather
        // than reporting eight indistinguishable "first" declarations.
        assert_eq!(
            retries_at_last_declaration,
            declarations - 1,
            "every declaration after the first must name the retries already spent"
        );
    }

    /// An idle sample is proof the host is no longer stuck, so it clears the retry
    /// count along with everything else — a host that stalls again next month
    /// starts from "retry 1", not "retry 9".
    #[test]
    fn an_idle_observation_zeroes_the_retry_count() {
        let mut tracker = RollStallTracker::default();
        tracker.set_cooldown(Duration::from_secs(7200));
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        assert_eq!(tracker.observe(at(2700 + 7200), None, 2), None, "cooldown expired");
        tracker.take_retry_note();
        assert_eq!(tracker.observe(at(20_000), None, 0), None, "the host goes quiet");
        let mut report = None;
        for deadline in 1..=3 {
            report =
                tracker.observe(at(30_000 + i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        let report = report.unwrap();
        assert_eq!(report.retries, 0, "the earlier retry belongs to a finished stall");
        assert!(!report.note().contains("cooldown retry(ies)"), "{}", report.note());
    }

    /// The cooldown must not release a retry while someone else's drain is armed:
    /// the episode is frozen there for the same reason it does not advance, and
    /// re-arming a roll of ours mid-teardown is exactly the destructive behaviour
    /// the ownership test exists to prevent.
    #[test]
    fn a_cooldown_cannot_release_a_retry_while_a_foreign_drain_is_armed() {
        let mut tracker = RollStallTracker::default();
        tracker.set_cooldown(Duration::from_secs(7200));
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 4);
        }
        assert!(tracker.observe(at(3600), None, 4).is_some(), "declared");

        let mut teardown = roll(0);
        teardown.then_exit = true;
        for tick in 10..30 {
            assert_eq!(
                tracker.observe(at(tick * 900), Some(&teardown), 4),
                None,
                "tick {tick}: a teardown is never reported…"
            );
            assert!(
                tracker.take_retry_note().is_none(),
                "tick {tick}: …and never releases a retry either"
            );
            assert!(tracker.is_active(), "tick {tick}: the declaration is retained");
        }
        // The moment the operator's drain is gone the (long-expired) cooldown is
        // honoured on the very next tick.
        assert_eq!(tracker.observe(at(27_000), None, 4), None);
        assert!(tracker.take_retry_note().is_some(), "the retry is released once it can be");
    }

    // ---- what it must never touch -------------------------------------------

    #[test]
    fn a_teardown_drain_never_advances_an_episode() {
        let mut tracker = RollStallTracker::default();
        let mut teardown = roll(9);
        teardown.then_exit = true;
        for tick in 0..10 {
            assert_eq!(tracker.observe(at(tick * 900), Some(&teardown), 4), None);
        }
        assert!(!tracker.is_active(), "`fleet drain`'s teardown is not an auto-update roll");
    }

    #[test]
    fn an_untargeted_operator_drain_never_advances_an_episode() {
        let mut tracker = RollStallTracker::default();
        let mut operator = roll(9);
        operator.target = None;
        for tick in 0..10 {
            assert_eq!(tracker.observe(at(tick * 900), Some(&operator), 4), None);
        }
        assert!(!tracker.is_active());
    }

    /// The case the two tests above cannot reach, because they arm the foreign
    /// drain from the first observation and so never start an episode: a
    /// declaration already **latched**, and a teardown armed afterwards. The
    /// sticky flag must not turn a foreign drain into a reportable episode —
    /// `run_tick` abandons whatever `observe` reports on, so a `Some` here is a
    /// cancelled operator teardown (PR #9004's first pass shipped exactly that).
    #[test]
    fn a_latched_declaration_does_not_report_a_teardown_drain_armed_afterwards() {
        let mut tracker = RollStallTracker::default();
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 4);
        }
        assert!(
            tracker.observe(at(3600), Some(&roll(4)), 4).is_some(),
            "our own roll is declared unsatisfiable"
        );
        // The host is still busy (the 9h sweep runs on) and an operator tears it
        // down. `fleet drain` reads a cleared `draining` flag as a *refusal*, so
        // an abandonment here is silently destructive.
        let mut teardown = roll(0);
        teardown.then_exit = true;
        for tick in 5..10 {
            assert_eq!(
                tracker.observe(at(tick * 900), Some(&teardown), 4),
                None,
                "tick {tick}: a teardown must not be reported even while latched"
            );
        }
        // And the suppression itself survives: it is the roll that is given up
        // on, not the detection.
        assert!(tracker.is_active(), "the episode is frozen, not discarded");
        assert!(
            tracker.observe(at(9000), None, 4).is_some(),
            "once the foreign drain is gone the declaration is re-reported"
        );
    }

    /// The same blind spot for an operator `restart --drain`, which carries no
    /// artifact target.
    #[test]
    fn a_latched_declaration_does_not_report_an_untargeted_drain_armed_afterwards() {
        let mut tracker = RollStallTracker::default();
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 4);
        }
        assert!(tracker.observe(at(3600), Some(&roll(4)), 4).is_some());
        let mut operator = roll(0);
        operator.target = None;
        for tick in 5..10 {
            assert_eq!(
                tracker.observe(at(tick * 900), Some(&operator), 4),
                None,
                "tick {tick}: an operator drain must not be reported even while latched"
            );
        }
        assert!(tracker.is_active());
    }

    #[test]
    fn a_busy_host_with_no_roll_armed_is_not_an_episode() {
        let mut tracker = RollStallTracker::default();
        for tick in 0..10 {
            assert_eq!(tracker.observe(at(tick * 900), None, 12), None);
        }
        assert!(!tracker.is_active());
    }

    // ---- the note -----------------------------------------------------------

    #[test]
    fn the_note_names_the_numbers_and_all_three_operator_actions() {
        let note = RollStallReport {
            deadlines: 4,
            in_flight: 7,
            floor: 2,
            episode_secs: 75_600,
            target: Some("v0.19.390@aaaa".to_string()),
            cooldown_secs: 21_600,
            retries: 0,
        }
        .note();
        assert!(
            !note.contains("cooldown retry(ies)"),
            "a first declaration must not claim spent retries: {note}"
        );
        for needle in [
            "UNSATISFIABLE",
            "4 drain deadline(s)",
            "75600s of stalled episode",
            "7 in flight now",
            // Both end conditions, stated precisely rather than as
            // "self-clearing": the idle fast path is a SAMPLE that may not land on
            // a busy host, and the cooldown is what makes the staleness bounded
            // anyway (#9010).
            "SAMPLES in-flight at zero",
            "21600s COOLDOWN EXPIRES",
            "ONE more bounded attempt",
            "BOUNDED, not indefinite",
            "ACT rather than wait",
            "never improved on 2",
            "v0.19.390@aaaa",
            "NORMAL DISPATCH RESUMES",
            "No sweep was cancelled",
            "loom-daemon list",
            "loom-daemon cancel --sweep",
            "--force-after-timeout",
        ] {
            assert!(note.contains(needle), "missing {needle:?} in: {note}");
        }
    }

    #[test]
    fn an_untargeted_report_does_not_render_an_empty_target_clause() {
        let note = RollStallReport {
            deadlines: 3,
            in_flight: 1,
            floor: 1,
            episode_secs: 60,
            target: None,
            cooldown_secs: 21_600,
            retries: 0,
        }
        .note();
        assert!(!note.contains("(target "), "{note}");
    }
}
