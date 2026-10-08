//! The daemon-global drain state machine (Issue #4090, extended by #4521,
//! #8514, #8652, #9588 and #10831).
//!
//! Moved out of `ipc.rs` unchanged apart from #9588's additions (drain origin,
//! the operator timed-out hold, the startup operator-stop hold, and the
//! durable operator-stop record) because that file is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen. Re-exported from
//! `crate::ipc` verbatim, so every existing caller is unchanged.
//!
//! #10831 removed #6007's retained-roll bookkeeping (a refused roll deadline
//! re-armed on a widening window, then abandoned): every automatic roll is now
//! a [`DrainOrigin::PauseRoll`] drain whose supervisor pauses the in-flight
//! agents and restarts, so nothing waits for the in-flight count to reach
//! zero. The pause's own state transitions live in the `pause` child module.

use super::drain_ledger;
use super::drain_status::PauseRollStatus;
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared drain-and-restart coordination state (Issue #4090).
///
/// Owns the daemon-global drain flag OR'd into the producers' halt checks (work
/// finder, epic supervisor, role runner) and the descriptor the `DaemonStatus`
/// snapshot renders. Exactly one drain-supervisor task may be live at a time; a
/// monotonic `generation` lets a running supervisor detect it has been
/// superseded (a fresh drain) or aborted, and stop **without** exiting the
/// process.
#[derive(Debug)]
pub struct DrainState {
    /// The flag consulted by the dispatch producers. `true` ⇒ new dispatch is
    /// paused pending a supervised restart. Cloned out to each producer via
    /// [`Self::flag`].
    flag: Arc<AtomicBool>,
    /// Bumped on every accepted drain start AND on abort/timeout-resume, so a
    /// running drain-supervisor task can tell it is still the current one.
    generation: AtomicU64,
    /// Mutable descriptor of the active/last drain, for status rendering.
    inner: Mutex<DrainDescriptor>,
    /// #8652's paused-time ledger. Observational only: taken (never before
    /// `inner`) at the transitions that open/close a pause, never read by the
    /// #6007 fail-safe policy.
    ledger: Mutex<drain_ledger::PausedLedger>,
    /// The resolved `autonomy-desired` marker path whose operator-stop record
    /// (#9588, [`crate::operator_stop`]) a then-exit drain writes and an abort
    /// clears. `None` for every in-memory (test) state, so a unit test can never
    /// move an operator's real `~/.loom` marker aside.
    stop_marker: Option<PathBuf>,
}

/// Who started a drain (Issue #9588) — it decides how the drain completes.
///
/// - [`Self::Operator`] (every IPC `DrainAndRestartDaemon`: `restart --drain`,
///   `loom-daemon-update.sh --drain`, `fleet drain`): the operator asked for
///   dispatch to stop. The drain completes when in-flight reaches zero; a
///   timeout keeps dispatch **paused** and never resumes it on its own — only
///   `--abort-drain` or a completed drain ends the pause.
/// - [`Self::PauseRoll`] (#10831, every automatic roll: floor, repo-ahead,
///   restart-only config, autoUpdate): the drain completes when every
///   in-flight agent is stopped at a safe point or requeued and the pause
///   manifest is written (`crate::auto_update::pause_roll`, design
///   `docs/design/daemon-roll-pause-resume.md` §7). It never waits for the
///   in-flight count to reach zero, and its deadline is the pause budget.
///
/// One-way: an operator request against a pause roll that has not stopped
/// anything yet promotes it to `Operator`; the reverse never happens.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DrainOrigin {
    #[default]
    Operator,
    PauseRoll,
}

impl DrainOrigin {
    /// Stable machine name (`status --json`, events, notes).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::PauseRoll => "pause-roll",
        }
    }
}

/// The rendered view of the current (or most recent) drain (Issue #4090).
#[derive(Debug, Default, Clone)]
pub struct DrainDescriptor {
    /// Whether a drain is currently in progress.
    pub active: bool,
    /// Deadline after which the drain gives up waiting.
    pub deadline: Option<chrono::DateTime<Utc>>,
    /// Whether the deadline path force-cancels stragglers (vs. refusing).
    pub force_after_timeout: bool,
    /// A short human-readable note about the last transition (timeout refusal,
    /// abort) surfaced in `loom-daemon status`.
    pub note: Option<String>,
    /// `true` when this drain's terminal action is "exit and stay down"
    /// rather than "exit for a supervised relaunch" (Issue #4343 — `fleet
    /// drain`'s teardown use case). See [`Request::DrainAndRestartDaemon`]'s
    /// `then_exit` field.
    pub then_exit: bool,
    /// When this drain started — the anchor for `paused_secs` in status.
    pub started_at: Option<chrono::DateTime<Utc>>,
    /// The artifact identity this roll was triggered for (Issue #8514), set by
    /// the auto-updater immediately after its trigger is accepted (see
    /// [`DrainState::set_roll_target`]). `None` for any drain the auto-updater
    /// did not arm — an operator `restart --drain`, a `fleet drain` teardown,
    /// or a source-path roll with no artifact identity.
    ///
    /// It exists so a later auto-update tick can ask "is the roll that is
    /// already armed the one for *this* artifact?" and supersede a roll whose
    /// target has been overtaken by a newer release, instead of waiting out a
    /// pause for a binary that is already stale.
    pub roll_target: Option<String>,
    /// Who started (or last promoted) this drain (#9588).
    pub origin: DrainOrigin,
    /// `true` once an **operator** drain passed its deadline without
    /// `--force-after-timeout` (#9588): dispatch stays paused, the deadline is
    /// cleared, and the supervisor keeps waiting for in-flight to reach zero.
    pub timed_out: bool,
    /// `true` when dispatch is held because this daemon started while an
    /// operator-stop record existed (#9588) — no drain supervisor is running,
    /// so a later drain request replaces the hold instead of acking it.
    pub startup_hold: bool,
    /// The H4 pause's progress while a [`DrainOrigin::PauseRoll`] drain runs
    /// (#10831); `None` for every other drain. `stopped` is the commit point
    /// the operator-interplay rules key on (see the `pause` child module).
    pub pause: Option<PauseRollStatus>,
    /// `true` when the hold was placed because the fleet store's
    /// `fleet/state.yml` says this host is **`paused`** (#9598) rather than
    /// because of a local operator stop.
    ///
    /// Always accompanied by `startup_hold` (a fleet hold has no supervisor
    /// behind it either, so a real drain request must replace it rather than ack
    /// it). It exists so [`DrainState::release_fleet_hold`] can release a hold
    /// *this* mechanism placed when the store flips back to `running`, while
    /// never touching a #9588 operator-stop hold — a local `restart --drain
    /// --then-exit` is a different operator action and wins locally.
    pub fleet_hold: bool,
    /// `true` while dispatch is held because this daemon started with a live
    /// pause manifest (#10832): H5 is verifying the new binary and resuming the
    /// paused agents. No supervisor is behind it, so a real drain request
    /// replaces it; the `resume` child module places and releases it.
    pub resume_hold: bool,
}

/// Outcome of [`DrainState::begin`].
#[derive(Debug)]
pub enum DrainBegin {
    /// A new drain was started; the caller must spawn the supervisor task with
    /// this generation.
    Started {
        generation: u64,
        deadline: chrono::DateTime<Utc>,
    },
    /// A drain was already in progress; the request is an idempotent ack and no
    /// second supervisor should be spawned (the deadline/generation are
    /// unchanged).
    AlreadyDraining {
        /// The **active** drain's actual terminal action after this request was
        /// applied — `true` ⇒ it will exit and stay down, `false` ⇒ it will exit
        /// for a supervised relaunch. Never a blind echo of the request
        /// (Issue #4521): the caller must render its ack from this, or it will
        /// promise a teardown that never happens.
        active_then_exit: bool,
        /// `true` when this request *escalated* an in-progress relaunch-drain to
        /// stay-down (the one-way `then_exit` transition — see
        /// [`DrainState::begin`]).
        escalated: bool,
        /// `true` when this request escalated the active drain to
        /// `--force-after-timeout` (one-way, #9588 — see [`DrainState::begin`]).
        force_escalated: bool,
        /// `true` when this operator request promoted a pause roll that had not
        /// stopped anything yet to [`DrainOrigin::Operator`] (#9588, #10831).
        origin_promoted: bool,
    },
}

impl Default for DrainState {
    fn default() -> Self {
        Self::new()
    }
}

// Allow expect_used: a poisoned drain mutex means another thread panicked while
// holding it — unrecoverable, same crash-on-poison policy as the rest of ipc.rs.
#[allow(clippy::expect_used)]
impl DrainState {
    #[must_use]
    pub fn new() -> Self {
        Self::with_ledger(drain_ledger::PausedLedger::in_memory())
    }

    /// [`Self::new`] with an explicit (typically persisted) #8652 ledger.
    #[must_use]
    pub fn with_ledger(ledger: drain_ledger::PausedLedger) -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            generation: AtomicU64::new(0),
            inner: Mutex::new(DrainDescriptor::default()),
            ledger: Mutex::new(ledger),
            stop_marker: None,
        }
    }

    /// Attach the `autonomy-desired` marker path whose operator-stop record
    /// this state writes and clears (#9588), and — when a record already
    /// exists — start **held**: dispatch paused, no supervisor, a note saying
    /// why. That is the "a supervised relaunch must not resurrect an operator
    /// stop" guarantee (launchd `RunAtLoad`, a reboot of an enabled systemd
    /// unit, a manual `kickstart`). `--abort-drain` releases the hold; an
    /// explicit start removes the record before launching, so it never holds.
    #[must_use]
    pub fn with_stop_marker(mut self, marker: PathBuf) -> Self {
        if crate::operator_stop::is_recorded(&marker) {
            let at = crate::operator_stop::field(&marker, "at").unwrap_or_default();
            let reason = crate::operator_stop::field(&marker, "reason").unwrap_or_default();
            let note = format!(
                "dispatch HELD at startup: an operator stop is on record ({} at {at}: {reason}), \
                 so this relaunch must not resume work. Release it with `loom-daemon restart \
                 --abort-drain` (restores the autonomy-desired marker), or stop the daemon.",
                crate::operator_stop::record_path(&marker).display()
            );
            log::warn!("{note}");
            self.set_hold(note, false);
        }
        self.stop_marker = Some(marker);
        self
    }

    /// Start **held** because the fleet store says this host is `paused`
    /// (#9598). `None` — every host whose store says `running`, and every host
    /// with no `fleet.repo` at all — is a no-op, so boot is unchanged.
    ///
    /// Deliberately *after* [`Self::with_stop_marker`] in
    /// [`Self::with_default_ledger`]'s chain and a no-op when a hold is already
    /// in place: a local operator stop is the more specific intent and keeps its
    /// own note (and its own `--abort-drain` release path).
    #[must_use]
    pub fn with_fleet_hold(mut self, note: Option<String>) -> Self {
        if let Some(note) = note {
            if !self.inner.get_mut().expect("Drain mutex poisoned").active {
                log::warn!("{note}");
                self.set_hold(note, true);
            }
        }
        self
    }

    /// Hold dispatch at runtime because the fleet store now says `paused`
    /// (#9598) — the timer-pass counterpart of [`Self::with_fleet_hold`].
    ///
    /// Returns `true` only when this call newly placed the hold. A drain of any
    /// other kind already in progress is left strictly alone: it already pauses
    /// dispatch (which is all `paused` asks for) and its terminal action is the
    /// operator's, not the store's.
    pub fn hold_for_fleet_state(&self, note: String) -> bool {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        // #10832: an H5 resume hold ends by itself, and the store's `paused`
        // must outlive it. The fleet hold takes the pause over in place, so
        // dispatch is never open between H5 finishing and the next sync pass.
        if inner.active && inner.resume_hold {
            inner.resume_hold = false;
            inner.startup_hold = true;
            inner.fleet_hold = true;
            inner.origin = DrainOrigin::Operator;
            inner.note = Some(note);
            return true;
        }
        if inner.active {
            return false;
        }
        let at = Utc::now();
        inner.active = true;
        inner.startup_hold = true;
        inner.fleet_hold = true;
        inner.origin = DrainOrigin::Operator;
        inner.started_at = Some(at);
        inner.note = Some(note);
        self.flag.store(true, Ordering::Relaxed);
        self.ledger_after(inner, at, true);
        true
    }

    /// Release a hold [`Self::hold_for_fleet_state`] / [`Self::with_fleet_hold`]
    /// placed, because the store now says `running` (#9598). Returns `true` when
    /// a fleet hold was actually released.
    ///
    /// Scoped to `fleet_hold`: a #9588 operator-stop hold, a supervised operator
    /// drain and an auto-update roll are all left in place — the store may say
    /// "this host should be dispatching" without that overriding a local
    /// operator's `restart --drain` or the updater's in-flight roll.
    pub fn release_fleet_hold(&self) -> bool {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if !inner.fleet_hold {
            return false;
        }
        self.flag.store(false, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
        inner.active = false;
        inner.deadline = None;
        inner.startup_hold = false;
        inner.fleet_hold = false;
        inner.timed_out = false;
        inner.note = Some(
            "fleet-state hold released — the fleet store's fleet/state.yml says this host is \
             running again, so dispatch resumed"
                .to_string(),
        );
        self.ledger_after(inner, Utc::now(), false);
        true
    }

    /// Whether dispatch is currently held because the fleet store says `paused`
    /// (#9598) — what the sync timer compares the store's latest answer against.
    #[must_use]
    pub fn is_fleet_held(&self) -> bool {
        self.inner.lock().expect("Drain mutex poisoned").fleet_hold
    }

    /// The supervisor-less startup hold shared by #9588's operator-stop record
    /// and #9598's fleet `paused` state. Takes `&mut self` because both callers
    /// are constructors, so no lock contention is possible and the #8652 ledger
    /// is deliberately not opened: a hold placed before the process has an
    /// event loop has no interval to close if the boot then fails.
    fn set_hold(&mut self, note: String, fleet: bool) {
        let inner = self.inner.get_mut().expect("Drain mutex poisoned");
        inner.active = true;
        inner.startup_hold = true;
        inner.fleet_hold = fleet;
        inner.origin = DrainOrigin::Operator;
        inner.started_at = Some(Utc::now());
        inner.note = Some(note);
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Write the operator-stop record for a then-exit drain (#9588). Never
    /// fatal — a failed write is logged and the drain carries on.
    fn record_operator_stop(&self, reason: &str) {
        if let Some(marker) = &self.stop_marker {
            match crate::operator_stop::record(marker, reason) {
                Ok(()) => log::warn!(
                    "operator stop recorded at {} (#9588): the watchdog and startup healing will \
                     not revive this daemon; `restart --abort-drain` or an explicit start clears it",
                    crate::operator_stop::record_path(marker).display()
                ),
                Err(e) => log::warn!("could not record the operator stop (#9588): {e}"),
            }
        }
    }

    /// Clear the operator-stop record on an operator abort (#9588), restoring
    /// the moved-aside marker.
    fn clear_operator_stop(&self) {
        if let Some(marker) = &self.stop_marker {
            match crate::operator_stop::clear(marker) {
                Ok(crate::operator_stop::Cleared::NotRecorded) => {}
                Ok(c) => log::warn!("operator-stop record cleared by --abort-drain (#9588): {c:?}"),
                Err(e) => log::warn!("could not clear the operator-stop record (#9588): {e}"),
            }
        }
    }

    /// [`Self::new`] loaded from the default on-disk #8652 ledger path
    /// (`~/.loom/drain-paused-ledger.json`, or `$LOOM_AUTO_UPDATE_STATE_DIR`),
    /// reconciling any pause a killed predecessor left open. What the real
    /// daemon process constructs; tests use [`Self::new`] (in-memory, no I/O).
    #[must_use]
    pub fn with_default_ledger() -> Self {
        let state = Self::with_ledger(drain_ledger::PausedLedger::load(
            drain_ledger::default_ledger_path(),
            Utc::now(),
        ));
        // #9588: the same marker path the watchdog and startup healing resolve.
        match crate::autonomy_marker::resolve_loom_dir() {
            Some(dir) => state.with_stop_marker(crate::autonomy_marker::resolve_marker_path(&dir)),
            None => state,
        }
    }

    /// #8652: per-UTC-day paused seconds, including the live pause's elapsed
    /// portion. Status-only — the fail-safe never reads this.
    #[must_use]
    pub fn paused_by_day(&self, now: chrono::DateTime<Utc>) -> BTreeMap<chrono::NaiveDate, u64> {
        self.lock_ledger().totals(now)
    }

    /// #8652: close the ledger's open interval. The supervisor calls this
    /// immediately before exiting for a roll — the exit is what would otherwise
    /// lose the interval.
    pub fn close_paused_interval(&self, now: chrono::DateTime<Utc>) {
        self.lock_ledger().close(now);
    }

    /// Poison-tolerant: a ledger panic must never become a drain outage.
    fn lock_ledger(&self) -> std::sync::MutexGuard<'_, drain_ledger::PausedLedger> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hand off from the descriptor lock to the ledger lock (#8652): the ledger
    /// is taken while `inner` is still held, so ledger ops land in transition
    /// order, then `inner` is released before the (file-writing) ledger op.
    fn ledger_after(
        &self,
        inner: std::sync::MutexGuard<'_, DrainDescriptor>,
        at: chrono::DateTime<Utc>,
        open: bool,
    ) {
        let mut ledger = self.lock_ledger();
        drop(inner);
        if open {
            ledger.open(at);
        } else {
            ledger.close(at);
        }
    }

    /// A clone of the drain flag to hand to a dispatch producer.
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }

    /// Whether new dispatch is currently paused for a drain.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// The current generation — the token a supervisor compares against to
    /// detect it has been superseded/aborted.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// A snapshot of the descriptor for status rendering.
    #[must_use]
    pub fn snapshot(&self) -> DrainDescriptor {
        self.inner.lock().expect("Drain mutex poisoned").clone()
    }

    /// Start a drain, or ack an already-running one (idempotent — a second drain
    /// request while DRAINING neither stacks a supervisor nor moves the
    /// deadline). Sets the drain flag on a fresh start.
    ///
    /// **`then_exit` on the already-draining path (Issue #4521 — design
    /// decision).** `timeout`/`force_after_timeout` stay pinned to the active
    /// drain (a later idempotent ack must not move a deadline someone is already
    /// waiting on), but `then_exit` is **escalated one-way**:
    /// `relaunch → stay-down`, never the reverse.
    ///
    /// Rationale: the two options were (a) refuse the escalation and tell the
    /// operator to `--abort-drain` and re-issue, or (b) escalate in place. (a)
    /// is racy in exactly the case that matters — an operator tearing a host
    /// down while an auto-update roll-drain (`then_exit=false`,
    /// `auto_update.rs`) is in flight would have to abort and re-issue, and the
    /// roll can complete *between* those two commands, relaunching the daemon on
    /// a host that is about to be powered off. (b) is monotonic and safe: exiting
    /// and staying down is strictly the more conservative terminal action, and
    /// the operator's teardown intent is honored on the first command. The
    /// reverse direction is deliberately **not** applied — a roll trigger
    /// arriving during an operator teardown drain must never silently downgrade
    /// the teardown into a relaunch.
    ///
    /// The escalation is observed by the already-running supervisor because it
    /// re-reads `then_exit` from this descriptor at its terminal tick rather
    /// than using a value captured at spawn (see [`run_drain_supervisor`]).
    ///
    /// **`force_after_timeout` on the already-draining path (Issue #6007,
    /// widened by #9588).** A later `--force-after-timeout` request escalates
    /// any active drain one-way (`refuse → force`). Before #9588 it only did so
    /// for a pending roll, so an operator trying to force an in-progress drain
    /// got a silent "existing deadline is unchanged" no-op. The deadline only
    /// ever moves **earlier**: a drain already past its deadline (a pending roll,
    /// or an operator drain held after a timeout) forces on the next tick, and a
    /// first-attempt drain forces at `min(existing deadline, now + timeout)`.
    ///
    /// **Origin (#9588, #10831).** An [`DrainOrigin::Operator`] request
    /// promotes an in-progress [`DrainOrigin::PauseRoll`] drain to `Operator`
    /// **only while the pause has stopped nothing** (rule 1 of #10831's
    /// operator interplay): the target label is cleared, the operator's
    /// timeout and force flag become the drain's, and the pause supervisor
    /// stands down at its next step boundary (withdraws its pause requests,
    /// deletes the manifest, stops nothing) and supervises the operator drain
    /// instead. Once the pause has stopped an agent (rule 2) a relaunch request
    /// is acked as `AlreadyDraining` with no promotion and the roll completes;
    /// a then-exit request still escalates the terminal action (rule 3), so the
    /// daemon stops without relaunch once the manifest says `phase = paused`.
    /// The reverse (operator → pause roll) never happens.
    ///
    /// Equivalent to [`Self::begin_as`] with [`DrainOrigin::Operator`].
    pub fn begin(
        &self,
        timeout: Duration,
        force_after_timeout: bool,
        then_exit: bool,
    ) -> DrainBegin {
        self.begin_as(timeout, force_after_timeout, then_exit, DrainOrigin::Operator)
    }

    /// [`Self::begin`] with an explicit [`DrainOrigin`] (#9588).
    pub fn begin_as(
        &self,
        timeout: Duration,
        force_after_timeout: bool,
        then_exit: bool,
        origin: DrainOrigin,
    ) -> DrainBegin {
        let requested =
            chrono::Duration::from_std(timeout).unwrap_or_else(|_| chrono::Duration::seconds(0));
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        // A startup hold (#9588) has no supervisor behind it, so a drain request
        // replaces it with a real, supervised drain rather than acking it.
        if inner.active && !inner.startup_hold && !inner.resume_hold {
            let escalated = then_exit && !inner.then_exit;
            let force_escalated = force_after_timeout && !inner.force_after_timeout;
            let origin_promoted = origin == DrainOrigin::Operator
                && inner.origin == DrainOrigin::PauseRoll
                && !inner.pause.as_ref().is_some_and(|p| p.stopped);
            if escalated {
                inner.then_exit = true;
            }
            if origin_promoted {
                inner.origin = DrainOrigin::Operator;
                // No longer a supersedable roll (#8514), and no longer a pause:
                // the pause supervisor stands down at its next step boundary.
                inner.roll_target = None;
                inner.pause = None;
                // The pause roll's deadline was its pause budget; an operator
                // drain runs on the operator's own timeout from here.
                inner.deadline = Some(Utc::now() + requested);
            }
            if force_escalated {
                inner.force_after_timeout = true;
                let now = Utc::now();
                inner.deadline = Some(match inner.deadline {
                    Some(d) if !inner.timed_out => d.min(now + requested),
                    // Already past its deadline (timed-out hold): act on the
                    // next supervisor tick.
                    _ => now,
                });
            }
            let mut parts: Vec<&str> = Vec::new();
            if escalated {
                parts.push(
                    "escalated to then-exit — will stop and stay down (was: exit for a supervised \
                     relaunch)",
                );
            }
            if force_escalated {
                parts.push(
                    "escalated to --force-after-timeout — the remaining in-flight sweep(s) will be \
                     cancelled at the (possibly earlier) deadline",
                );
            }
            if origin_promoted {
                parts.push(
                    "promoted from a pause roll (nothing stopped yet) to an operator drain — it \
                     now waits for in-flight work to finish, and a timeout keeps dispatch paused",
                );
            }
            if !parts.is_empty() {
                inner.note = Some(format!("in-progress drain {}", parts.join("; ")));
            }
            let active_then_exit = inner.then_exit;
            drop(inner);
            if escalated {
                self.record_operator_stop("in-progress drain escalated to --then-exit");
            }
            return DrainBegin::AlreadyDraining {
                active_then_exit,
                escalated,
                force_escalated,
                origin_promoted,
            };
        }
        let was_held = inner.startup_hold;
        let deadline = Utc::now() + requested;
        inner.active = true;
        inner.deadline = Some(deadline);
        inner.force_after_timeout = force_after_timeout;
        inner.then_exit = then_exit;
        inner.note = None;
        let started_at = Utc::now();
        inner.started_at = Some(started_at);
        // #8514: a fresh drain has no target until whoever armed it records one.
        inner.roll_target = None;
        // #10831: a pause roll's progress is installed by `begin_pause_roll`.
        inner.pause = None;
        // #9588: origin + the timed-out / startup holds.
        inner.origin = if was_held {
            DrainOrigin::Operator
        } else {
            origin
        };
        inner.timed_out = false;
        inner.startup_hold = false;
        inner.resume_hold = false; // #10832: H5 sees the hold gone and stops relaunching
                                   // #9598: a real, supervised drain replaces a fleet-state hold — the
                                   // operator's terminal action (relaunch / stay down) now owns the pause,
                                   // so the store must not be able to release it out from under them.
        inner.fleet_hold = false;
        // Set the flag while holding the descriptor lock so status can never
        // observe `flag=true` with `active=false`.
        self.flag.store(true, Ordering::Relaxed);
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.ledger_after(inner, started_at, true);
        if then_exit {
            self.record_operator_stop("restart --drain --then-exit");
        }
        DrainBegin::Started {
            generation,
            deadline,
        }
    }

    /// Abort an in-progress drain: clear the flag, bump the generation (so the
    /// running supervisor stops without exiting), and record a note. Returns
    /// `true` when a drain was actually aborted — see [`Self::abort_checked`]
    /// for the outcome that tells "nothing to abort" from "refused".
    ///
    /// #9588: the operator abort also clears the operator-stop record (restoring
    /// the moved-aside `autonomy-desired` marker) — even when no drain is active,
    /// so a stale record can always be cleared from a running daemon.
    pub fn abort(&self) -> bool {
        matches!(self.abort_checked(), AbortOutcome::Aborted)
    }

    /// [`Self::abort`] with its outcome (#10831): an operator `--abort-drain` is
    /// **refused** for a pause roll that has already stopped an agent's process
    /// tree (design §7 failure edges: honoured before H4 step 4, or in step 5
    /// while nothing is stopped yet). Aborting then would leave stopped work
    /// behind with no restart to pick it up, so the roll completes instead.
    pub fn abort_checked(&self) -> AbortOutcome {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if !inner.active {
            drop(inner);
            self.clear_operator_stop();
            return AbortOutcome::NotActive;
        }
        if inner.resume_hold {
            return AbortOutcome::Refused(resume::ABORT_REFUSED.to_string());
        }
        if inner.origin == DrainOrigin::PauseRoll {
            if let Some(p) = inner.pause.as_ref().filter(|p| p.stopped) {
                return AbortOutcome::Refused(format!(
                    "refusing --abort-drain: the pause-and-roll pause is at H4 step {} and has \
                     already stopped agent process trees, so aborting would strand stopped work \
                     with no restart to pick it up (#10831, design §7). The roll completes: the \
                     daemon writes the pause manifest and restarts onto the new binary, which \
                     resumes or requeues every recorded agent.",
                    p.step
                ));
            }
        }
        self.flag.store(false, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
        inner.active = false;
        inner.deadline = None;
        inner.note = Some(if inner.pause.is_some() {
            "drain aborted by operator — the pause roll was cancelled before it stopped any \
             agent (its pause requests are withdrawn) and dispatch resumed; this host stays on \
             its current binary until a new roll is triggered"
                .to_string()
        } else {
            "drain aborted by operator — dispatch resumed".to_string()
        });
        inner.roll_target = None;
        inner.pause = None;
        let was_held = inner.startup_hold;
        let was_fleet = inner.fleet_hold;
        inner.timed_out = false;
        inner.startup_hold = false;
        inner.fleet_hold = false;
        if was_held {
            inner.note = Some(if was_fleet {
                // #9598: naming the store matters — an `--abort-drain` here only
                // resumes dispatch until the next sync pass re-reads `paused`.
                "fleet-state hold released by operator — dispatch resumed, but the fleet store \
                 still says this host is paused, so the next sync pass will hold it again. Propose \
                 `running` in the store (`loom-daemon fleet-config propose state running`) to make \
                 it stick."
                    .to_string()
            } else {
                "startup hold released by operator — the operator-stop record was cleared and \
                 dispatch resumed"
                    .to_string()
            });
        }
        self.ledger_after(inner, Utc::now(), false);
        // #9588: an operator abort undoes the stop intent as well.
        self.clear_operator_stop();
        AbortOutcome::Aborted
    }

    /// The auto-updater's own way out of a roll it armed (#8514 supersede):
    /// [`Self::abort`], but **only** for a [`DrainOrigin::PauseRoll`] drain that
    /// is not a then-exit and has not stopped anything yet (#10831: supersede
    /// works until the pause commits). An operator drain — including a pause
    /// roll an operator request promoted — is never ended by the auto-updater,
    /// and the operator-stop record is never touched here. Returns `true` when
    /// a drain was aborted.
    pub fn abort_pause_roll(&self) -> bool {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        // (#10832: an H5 resume hold is not a roll the updater armed.)
        if !inner.active
            || inner.resume_hold
            || inner.origin != DrainOrigin::PauseRoll
            || inner.then_exit
            || inner.pause.as_ref().is_some_and(|p| p.stopped)
        {
            return false;
        }
        self.flag.store(false, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
        inner.active = false;
        inner.deadline = None;
        inner.roll_target = None;
        inner.pause = None;
        inner.note = Some("pause roll ended by the auto-updater — dispatch resumed".to_string());
        self.ledger_after(inner, Utc::now(), false);
        true
    }

    /// Record which artifact the **active** drain is rolling to (Issue #8514).
    ///
    /// Called by the auto-updater right after its trigger is accepted, so a
    /// later tick can compare the roll that is already armed against a freshly
    /// resolved release and supersede it when it has been overtaken. A no-op
    /// when no drain is active — there is nothing to label — and deliberately
    /// **not** applied to a teardown (`then_exit`) drain: `fleet drain`'s
    /// teardown is never superseded by a newer binary.
    pub fn set_roll_target(&self, target: Option<String>) {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if inner.active && !inner.then_exit && inner.origin == DrainOrigin::PauseRoll {
            inner.roll_target = target;
        }
    }

    /// End the drain without exiting: clear the flag, bump the generation, and
    /// record `note` so status explains why the daemon stayed up. Since #10831
    /// this is a pause roll's H7 `pause-failed` path (the manifest could not be
    /// written, so nothing was signalled and dispatch resumes).
    pub(crate) fn resolve_timeout(&self, note: String) {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        self.flag.store(false, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
        inner.active = false;
        inner.deadline = None;
        inner.roll_target = None;
        inner.pause = None;
        inner.note = Some(note);
        self.ledger_after(inner, Utc::now(), false);
    }

    /// The **operator** drain's deadline path without force (#9588): keep
    /// dispatch **paused** and the drain active, clear the deadline (there is
    /// nothing left to time out — only in-flight reaching zero, an abort, or a
    /// force escalation moves it on), and record `note`. The generation is
    /// **not** bumped, so the same supervisor keeps polling and still fires the
    /// terminal action (stay down, or relaunch) the moment in-flight hits zero.
    pub fn hold_after_timeout(&self, note: String) {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        inner.timed_out = true;
        inner.deadline = None;
        inner.note = Some(note);
    }

    /// Record a note on the active/last drain without touching any other state
    /// (Issue #6007).
    pub fn set_note(&self, note: String) {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        inner.note = Some(note);
    }
}

/// What [`DrainState::abort_checked`] did (#10831).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortOutcome {
    /// The drain was aborted and dispatch resumed.
    Aborted,
    /// No drain was active (a stale operator-stop record was still cleared).
    NotActive,
    /// Refused, with the operator-facing reason.
    Refused(String),
}

/// The three terminal/continue decisions a drain-supervisor poll can reach
/// (Issue #4090). Extracted as a pure function so the "2 → 1 → 0" and
/// timeout-vs-force logic is unit-testable without spawning a task or calling
/// `std::process::exit`.
#[derive(Debug, PartialEq, Eq)]
pub enum DrainTick {
    /// Sweeps still in flight and the deadline has not passed — keep waiting.
    Continue,
    /// Zero in-flight — restart now (exit `EXIT_RESTART`).
    Complete,
    /// Deadline passed with sweeps still in flight and no force — refuse the
    /// restart and stay up. Only an operator drain reaches this (a pause roll
    /// is not supervised by this poll), and it holds dispatch paused
    /// ([`DrainState::hold_after_timeout`], #9588).
    TimedOutRefuse,
    /// Deadline passed with sweeps still in flight and `--force-after-timeout` —
    /// cancel the stragglers, then restart.
    TimedOutForce,
}

/// Decide a single drain-supervisor poll (Issue #4090). Zero in-flight always
/// wins (even at/after the deadline: everything drained, so restart), otherwise
/// a passed deadline is refused (fail-safe) or forced.
#[must_use]
pub fn evaluate_drain_tick(in_flight: usize, past_deadline: bool, force: bool) -> DrainTick {
    if in_flight == 0 {
        DrainTick::Complete
    } else if past_deadline {
        if force {
            DrainTick::TimedOutForce
        } else {
            DrainTick::TimedOutRefuse
        }
    } else {
        DrainTick::Continue
    }
}

/// #10831: the pause roll's step/commit transitions.
#[path = "drain_pause.rs"]
mod pause;
#[path = "drain_resume.rs"]
mod resume;
pub use pause::PauseOwnership;
pub use resume::ResumeHold;

#[cfg(test)]
#[path = "drain_state_tests.rs"]
mod tests;
