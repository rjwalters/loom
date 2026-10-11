//! No-op re-dispatch cooldown (Issue #6670).
//!
//! # The gap
//!
//! The insta-crash quarantine ([`super::quarantine`]) and the per-issue
//! dispatch backoff ([`super::dispatch`]) both bound how often a *failing*
//! issue is re-dispatched. Neither covers a sweep that runs to a clean,
//! **successful** conclusion and finds nothing to do: a phase checkpoint was
//! written, the `loom:building` claim was released cleanly back to
//! `loom:issue`, and zero forge/issue mutation happened — a standing/tracking
//! issue with no pending content change is the canonical shape. That sweep
//! looks identical to a normal, healthy completion from the reaper's point of
//! view, so it trips neither existing brake, and the work finder re-offers
//! the same candidate on the very next tick — burning a dispatch slot and a
//! full agent session purely to re-confirm "still nothing to do" (observed:
//! ~20 claim/release cycles over 4 hours on one issue).
//!
//! # The mechanism
//!
//! This module gives a sweep a way to signal that outcome back to the daemon
//! — "no actionable delta this pass" — and lets the work finder apply a
//! cooldown before re-offering the same candidate, **independent of and
//! non-interfering with** both the insta-crash quarantine and the dispatch
//! backoff (a distinct in-memory map, a distinct config block, a distinct
//! work-finder skip-set).
//!
//! Unlike the quarantine's crash *tally* (N consecutive insta-crashes before
//! quarantine engages) this is a single-shot arm: a caller that itself
//! concluded "no actionable delta" has already assessed the ENTIRE candidate
//! — repeating that assessment inside the daemon would just duplicate the
//! sweep-side re-derivation logic issue #6670 explicitly says is not the
//! daemon's job. The daemon takes the sweep's self-report at face value.
//!
//! There is deliberately no daemon-automatic path that infers this state
//! (e.g. from a checkpoint field): the signal is call-through only, mirroring
//! [`super::dispatch::record_dispatch_failure`]'s IPC-reachable sibling
//! (`RecordDispatchFailure`) rather than the reaper's automatic
//! terminal-outcome classification. A sweep (or the `/loom:sweep` orchestrator
//! driving it) that reaches "no actionable delta, releasing the claim" calls
//! `loom-daemon noop-cooldown record --issue <N>` before exiting, exactly the
//! way `build-gate.sh` calls `loom-daemon dispatch-backoff record` on a step
//! timeout (Issue #6192).
//!
//! # Fleet-wide visibility (Issue #7477)
//!
//! The window armed here (and [`super::dispatch`]'s sibling dispatch-backoff
//! window) used to live ONLY in this host's in-process [`NoopCooldownState`]
//! map — invisible to every other host in the fleet. On a multi-dispatcher
//! fleet that reintroduced the exact symptom this module was built to fix:
//! host A dispatches, bails, and arms its own cooldown, but hosts B/C/D never
//! see that and immediately re-dispatch the same freshly-released candidate,
//! each bailing in turn — an N-host fleet round-robins the bail loop up to N×
//! faster than a single-host cooldown was designed to prevent (confirmed via
//! #7466/#7468's sub-minute `loom:issue`/`loom:building` flapping across four
//! hosts). [`SweepRegistry::record_noop_release`] now also broadcasts the
//! armed window over the peer-claim channel
//! (`SweepRegistry::publish_peer_cooldown_claim`), and
//! [`SweepRegistry::noop_cooldown_issues`] unions the local map with the
//! peer-observed view before the work finder reads it. See
//! `defaults/docs/safehouse.md` → "Fleet-wide no-op cooldown / dispatch
//! backoff" for the full mechanism.
//!
//! Issue #9928 extended that union to the *dispatch-path* guard
//! ([`SweepRegistry::noop_cooldown_dispatch_block`], step 2.75 of
//! `begin_issue_dispatch`). #7477 made the work finder's advisory pre-filter
//! fleet-aware but left the one guard EVERY dispatch route funnels through
//! reading the host-local window only — so a peer host's direct
//! `{"Issue": <N>}` re-dispatch (and a work-finder tick that had already
//! selected the candidate before the peer's ad landed) sailed straight
//! through a window #7477 had broadcast to it.
//!
//! That broadcast is **config-gated**: with no peer-claim publisher attached
//! (`safehouse.enabled` false on this host) it is a byte-for-byte no-op and the
//! window is host-local again. That is *one* of the two ways #8912's multi-host
//! symptom arises — the other being a host whose peer-claim *receive* path is
//! dead (#9294), which this module cannot observe. The arm path now logs the gated case
//! rather than degrading silently; see `defaults/docs/safehouse.md` →
//! "'Fleet-wide' has two preconditions" for the other one and how to check it.
//!
//! # One release, one classification (Issue #8912)
//!
//! [`SweepRegistry::record_noop_release`] runs synchronously when the sweep's
//! `RecordNoopRelease` IPC call lands; the reaper classifies the same sweep's
//! terminal outcome minutes later, and used to have no way to tell that the
//! release it was looking at had already been self-reported as a deliberate
//! no-op. So one release armed a 3600s no-op cooldown AND a 300s PR-less retry
//! window AND an insta-crash strike toward quarantine — the shorter, wrongly
//! classified windows undercutting the correct one. The record now names the
//! dispatch it belongs to, and the three carve-out wrappers below
//! ([`SweepRegistry::charge_insta_crash`],
//! [`SweepRegistry::charge_dispatch_failure`],
//! [`SweepRegistry::clear_noop_cooldown_unless_reported`]) plus
//! `prless_retry`'s own check make that dispatch's terminal outcome exempt
//! from every failure classifier — for that sweep id only.

use super::*;

mod fingerprint;
mod hold;

pub(crate) use fingerprint::ParkKind;
pub(crate) use hold::NoopCooldownTable;
pub use hold::{
    NOOP_HOLD_APPLIED_COMMENT_MARKER, NOOP_HOLD_COMMENT_MARKER, NOOP_HOLD_FAILED_COMMENT_MARKER,
};

/// Env var overriding how many consecutive unchanged no-op releases park an
/// issue (Issue #10156). `0` disables the hold only; the cooldown is unaffected.
pub const NOOP_HOLD_THRESHOLD_ENV: &str = "LOOM_WORK_FINDER_NOOP_HOLD_THRESHOLD";

/// Default consecutive no-op count at which the hold parks an issue (#10156).
pub const DEFAULT_NOOP_HOLD_THRESHOLD: u32 = 3;

/// Env var toggling the no-op re-dispatch cooldown (Issue #6670). `0`/`false`/
/// `no`/`off` disables; `1`/`true`/`yes`/`on` forces on. Overrides config.
/// Defaults ON — like quarantine/dispatch-backoff it is a dispatch-efficiency
/// backstop, and (like dispatch backoff, unlike quarantine) it never blocks an
/// issue for longer than [`NoopCooldownConfig::cooldown`].
pub const NOOP_COOLDOWN_ENABLE_ENV: &str = "LOOM_WORK_FINDER_NOOP_COOLDOWN";

/// Env var overriding the no-op cooldown duration, in seconds (Issue #6670). A
/// zero/invalid value falls through to config/default.
pub const NOOP_COOLDOWN_SECS_ENV: &str = "LOOM_WORK_FINDER_NOOP_COOLDOWN_SECS";

/// Default no-op cooldown duration (#6670): one hour. Matches
/// [`super::quarantine::DEFAULT_QUARANTINE_TTL_SECS`] deliberately — the
/// observed incident's own re-verification cadence (an external ref that had
/// not moved in an hour) makes an hour a reasonable default "come back later"
/// window without a repo-specific tuning pass.
pub const DEFAULT_NOOP_COOLDOWN_SECS: u64 = 3600;

/// Resolved no-op-cooldown parameters (Issue #6670), set on the registry at
/// construction so the work finder can enforce them without a per-tick config
/// read — mirrors [`QuarantineConfig`] / [`DispatchBackoffConfig`]'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoopCooldownConfig {
    /// Whether the no-op cooldown is active. When `false`, recording a no-op
    /// release is a no-op (no state written, no cooldown armed) — byte-for-byte
    /// the pre-#6670 path.
    pub enabled: bool,
    /// How long a recorded no-op release holds the issue out of dispatch.
    pub cooldown: Duration,
    /// Consecutive no-ops with an unchanged forge fingerprint after which the
    /// issue is durably parked (Issue #10156); `0` disables the hold.
    pub hold_threshold: u32,
}

impl Default for NoopCooldownConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown: Duration::from_secs(DEFAULT_NOOP_COOLDOWN_SECS),
            hold_threshold: DEFAULT_NOOP_HOLD_THRESHOLD,
        }
    }
}

/// Per-issue no-op-cooldown bookkeeping (Issue #6670).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoopCooldownState {
    /// When this cooldown window was (most recently) armed.
    pub(crate) recorded_at: DateTime<Utc>,
    /// The instant at which the next dispatch attempt becomes allowed.
    pub(crate) until: DateTime<Utc>,
    /// Consecutive no-op releases recorded for this issue without an
    /// intervening real dispatch (cleared by [`SweepRegistry::clear_noop_cooldown`]
    /// — including the automatic clear on a successful non-no-op dispatch, see
    /// [`SweepRegistry::dispatch`]). Observability only; never affects the
    /// (flat, non-exponential) cooldown duration.
    pub(crate) consecutive: u32,
    /// Free-form context supplied by the recording caller, logged verbatim.
    pub(crate) reason: Option<String>,
    /// The dispatch this self-report belongs to (Issue #8912): the id of the
    /// live sweep this host had running for the issue when the report arrived.
    /// `None` when none could be attributed — an out-of-band `loom-daemon
    /// noop-cooldown record`, or a registry that never saw the dispatch
    /// (another host's sweep) — in which case there is no reap of this
    /// daemon's to carve out either. Read by
    /// [`SweepRegistry::noop_release_covers_dispatch`].
    pub(crate) released_by_sweep: Option<String>,
}

/// The subset of `.loom/config.json → autonomous.workFinder.noopCooldown`
/// this module consumes (Issue #6670). Mirrors [`DispatchBackoffFileConfig`]'s
/// shape: every field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — precedence **env > config >
/// default**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoopCooldownFileConfig {
    /// `autonomous.workFinder.noopCooldown.enabled`.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.noopCooldown.cooldownSecs` (zero/invalid dropped).
    pub cooldown_secs: Option<u64>,
    /// `autonomous.workFinder.noopCooldown.holdThreshold` (`0` disables).
    pub hold_threshold: Option<u32>,
}

/// Read `.loom/config.json → autonomous.workFinder.noopCooldown` (Issue
/// #6670), soft-failing every field to `None` on a missing file, malformed
/// JSON, or an absent block — mirrors [`read_dispatch_backoff_file_config`].
#[must_use]
pub fn read_noop_cooldown_file_config(repo_root: &Path) -> NoopCooldownFileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(c) =
        crate::config_resolver::get_path(&effective, "autonomous.workFinder.noopCooldown")
    else {
        return NoopCooldownFileConfig::default();
    };
    NoopCooldownFileConfig {
        enabled: c.get("enabled").and_then(serde_json::Value::as_bool),
        cooldown_secs: c
            .get("cooldownSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        hold_threshold: c
            .get("holdThreshold")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
    }
}

/// Resolve the full [`NoopCooldownConfig`] for `repo_root` with precedence
/// **env > config > default** for every knob (Issue #6670), mirroring
/// [`resolve_dispatch_backoff_config`].
#[must_use]
pub fn resolve_noop_cooldown_config(repo_root: &Path) -> NoopCooldownConfig {
    let file = read_noop_cooldown_file_config(repo_root);

    let enabled = if let Ok(v) = std::env::var(NOOP_COOLDOWN_ENABLE_ENV) {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    } else {
        file.enabled.unwrap_or(true)
    };

    let cooldown_secs = std::env::var(NOOP_COOLDOWN_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.cooldown_secs)
        .unwrap_or(DEFAULT_NOOP_COOLDOWN_SECS);

    let hold_threshold = std::env::var(NOOP_HOLD_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .or(file.hold_threshold)
        .unwrap_or(DEFAULT_NOOP_HOLD_THRESHOLD);

    NoopCooldownConfig {
        enabled,
        cooldown: Duration::from_secs(cooldown_secs),
        hold_threshold,
    }
}

impl SweepRegistry {
    /// Set the no-op-cooldown parameters (Issue #6670). `main.rs` and the
    /// workspace pool call this once at provision time with the resolved
    /// env > config > default value, mirroring
    /// [`Self::set_dispatch_backoff_config`].
    pub fn set_noop_cooldown_config(&mut self, config: NoopCooldownConfig) {
        self.noop_cooldown_config = config;
    }

    /// Read-only accessor for the no-op-cooldown parameters (Issue #6670).
    #[must_use]
    pub fn noop_cooldown_config(&self) -> NoopCooldownConfig {
        self.noop_cooldown_config
    }

    /// Record a **no-op release** for `issue` (Issue #6670): a sweep concluded
    /// "no actionable delta this pass" — its own checkpoint was written, the
    /// `loom:building` claim was released cleanly back to `loom:issue`, and it
    /// made zero forge/issue mutation — and reports that back so the work
    /// finder does not immediately re-offer the same candidate. Arms (or
    /// refreshes) a flat cooldown window; unlike
    /// [`Self::record_dispatch_failure`] this never grows exponentially — a
    /// caller that determines "still nothing to do" on every pass is expected
    /// to keep reporting it, and each report just re-arms the same window.
    ///
    /// A no-op when the mechanism is disabled
    /// (`autonomous.workFinder.noopCooldown.enabled: false` /
    /// [`NOOP_COOLDOWN_ENABLE_ENV`]) — no state is written, mirroring
    /// [`Self::record_dispatch_failure`]'s disabled-path contract.
    pub(crate) fn record_noop_release(&mut self, issue: u32, reason: Option<String>) {
        if !self.noop_cooldown_config.enabled {
            return;
        }
        let now = Utc::now();
        let consecutive = self
            .noop_cooldown
            .get(&issue)
            .map_or(1, |prev| prev.consecutive.saturating_add(1));
        let until = now
            + chrono::Duration::from_std(self.noop_cooldown_config.cooldown)
                .unwrap_or_else(|_| chrono::Duration::zero());
        log::info!(
            "sweep_registry: issue #{issue} no-op-release cooldown armed — {consecutive} \
             consecutive no-op release(s), next dispatch allowed in {}s (#6670){}",
            self.noop_cooldown_config.cooldown.as_secs(),
            reason
                .as_deref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        );
        // Issue #8912: remember WHICH dispatch self-reported the no-op, so the
        // reaper can recognise its own terminal outcome as the deliberate
        // conclusion it is rather than classifying it a second time as a
        // PR-less failure — see `noop_release_covers_dispatch`.
        let released_by_sweep = self.live_issue_sweep_id(issue);
        let reason_for_hold = reason.clone();
        self.noop_cooldown.insert(
            issue,
            NoopCooldownState {
                recorded_at: now,
                until,
                consecutive,
                reason,
                released_by_sweep: released_by_sweep.clone(),
            },
        );
        // Issue #10156: feed the durable hold's streak (see `hold`). After the
        // window is armed, so a slow forge read cannot delay the cooldown.
        self.note_noop_for_hold(issue, released_by_sweep.as_deref(), reason_for_hold.as_deref());
        // Issue #7972: a self-reported no-op is a *deliberate conclusion*, not
        // a failed attempt — and this cooldown is the correct brake for it. A
        // standing/tracking issue that keeps legitimately concluding "still
        // nothing to do" must therefore never accrete toward the PR-less retry
        // bound's `loom:blocked` hold, so clear that tally here. See
        // `super::prless_retry`'s module doc (the non-interference section).
        self.clear_prless_retry(issue);
        // Issue #7477: broadcast the armed window fleet-wide so a peer host
        // does not immediately re-offer the same candidate this host just
        // self-reported "no actionable delta" on — see
        // `SweepRegistry::publish_peer_cooldown_claim`'s doc comment for why
        // this is one-shot rather than re-advertised.
        self.publish_peer_cooldown_claim(
            crate::peer_claims::ClaimKind::NoopCooldownArmed,
            issue,
            self.noop_cooldown_config.cooldown,
        );
        // Issue #8912: without a peer-claim publisher attached
        // (`safehouse.enabled` false / no coordination on this host) the
        // broadcast above is a byte-for-byte no-op and this window is
        // HOST-LOCAL — a peer host will re-offer the same candidate inside it,
        // which is exactly the multi-host symptom #8912 reports. Say so at the
        // moment it matters instead of leaving the degradation invisible: the
        // fleet-wide half of #7477 is config-gated, and nothing else in the log
        // distinguishes "armed fleet-wide" from "armed on this host only".
        if self.peer_claim_publisher.is_none() {
            log::info!(
                "sweep_registry: issue #{issue}'s no-op cooldown is HOST-LOCAL only — no \
                 peer-claim publisher is attached on this host (safehouse peer coordination \
                 disabled), so a peer host may re-dispatch inside the window (#7477/#8912)"
            );
        }
    }

    /// The id of the live (`Running`/`Pending`) sweep this host has running for
    /// `issue`, if any (Issue #8912) — the most recently started one when more
    /// than one entry matches (a superseded claim leaves the older entry behind
    /// until its own reap).
    fn live_issue_sweep_id(&self, issue: u32) -> Option<String> {
        self.entries
            .values()
            .filter(|info| {
                matches!(info.kind, SweepKind::Issue(n) if n == issue)
                    && matches!(info.state, SweepState::Running | SweepState::Pending)
            })
            .max_by_key(|info| info.started_at)
            .map(|info| info.sweep_id.clone())
    }

    /// Whether the dispatch `sweep_id` self-reported a no-op release for
    /// `issue` (Issue #8912) — the discriminator the reaper's terminal-outcome
    /// classifiers consult before charging this dispatch as a failure.
    ///
    /// # Why this is needed
    ///
    /// [`Self::record_noop_release`] runs **synchronously**, at the moment the
    /// sweep's `RecordNoopRelease` IPC call lands — it arms the cooldown and
    /// clears the PR-less retry tally there and then. The reaper's
    /// terminal-outcome classification runs **later**, when the process is
    /// actually reaped, and used to have no way of knowing a no-op had just
    /// been self-reported for this very dispatch: it re-classified the same
    /// release as a PR-less failure, an insta-crash and a failed dispatch, and
    /// so re-armed — with windows an order of magnitude shorter — exactly what
    /// `record_noop_release` had just cleared. The observed shape, one release
    /// on `2AMLogic/llm-cim#1`: a 3600s no-op cooldown, a 300s PR-less retry
    /// window and an insta-crash strike toward quarantine, all within 60
    /// seconds of each other.
    ///
    /// # Why it is scoped to a sweep id rather than to the issue
    ///
    /// A pure read with no "consume" step, because the sweep id already scopes
    /// it to exactly one dispatch: the record names the sweep that reported it
    /// (`released_by_sweep`), a given sweep id is reaped at most once, and a
    /// *later* dispatch of the same issue carries a different id and so is
    /// classified normally. A genuinely failing re-attempt still accrues every
    /// brake it should — the suppression covers one terminal outcome, never the
    /// issue.
    ///
    /// `false` whenever the release could not be attributed to a live sweep at
    /// record time (`released_by_sweep: None` — an out-of-band `loom-daemon
    /// noop-cooldown record` with no dispatch of this daemon's to attach to):
    /// there is no reap of ours to suppress in that case either.
    #[must_use]
    pub(crate) fn noop_release_covers_dispatch(&self, issue: u32, sweep_id: &str) -> bool {
        self.noop_cooldown
            .get(&issue)
            .and_then(|s| s.released_by_sweep.as_deref())
            .is_some_and(|id| id == sweep_id)
    }

    /// [`Self::record_insta_crash_outcome`] with the Issue #8912 carve-out
    /// applied: a dispatch that self-reported a no-op release reached a
    /// deliberate conclusion, so it is neither an insta-crash to tally nor a
    /// terminal outcome to reset the tally on. Every other outcome is passed
    /// straight through unchanged.
    ///
    /// These three wrappers live here, beside the discriminator, rather than
    /// as extra conditions at `reap_once`'s call sites: the carve-out is this
    /// mechanism's rule, both reaper branches want it applied identically, and
    /// keeping it here means the reaper carries one call, not one call plus a
    /// predicate it would have to keep in sync twice.
    pub(crate) fn charge_insta_crash(&mut self, sweep_id: &SweepId, issue: u32, counted: bool) {
        if self.log_noop_carve_out(issue, sweep_id, "the quarantine tally") {
            return;
        }
        self.record_insta_crash_outcome(sweep_id, issue, counted);
    }

    /// [`Self::record_dispatch_failure`] with the same Issue #8912 carve-out:
    /// a self-reported no-op release is a deliberate conclusion, not a failed
    /// dispatch, and its own (far longer) cooldown is already armed — so the
    /// #4485 retry ladder must neither escalate on it nor be cleared by it.
    pub(crate) fn charge_dispatch_failure(&mut self, sweep_id: &str, issue: u32) {
        if self.log_noop_carve_out(issue, sweep_id, "the dispatch-backoff ladder") {
            return;
        }
        self.record_dispatch_failure(issue);
    }

    /// [`Self::clear_noop_cooldown`] unless `sweep_id` is the very dispatch
    /// that armed the window (Issue #8912) — the inverse half of the same
    /// carve-out. `reap_once`'s crashed branch clears the cooldown on
    /// checkpoint progress (#6670: real progress proves the candidate is live
    /// again), which is right for every dispatch except the one whose own
    /// self-report armed it seconds earlier.
    pub(crate) fn clear_noop_cooldown_unless_reported(&mut self, sweep_id: &str, issue: u32) {
        if self.noop_release_covers_dispatch(issue, sweep_id) {
            return;
        }
        self.clear_noop_cooldown(issue);
    }

    /// Shared predicate + log line behind the two `charge_*` wrappers above
    /// (Issue #8912). Returns `true` when the classifier must be skipped.
    fn log_noop_carve_out(&self, issue: u32, sweep_id: &str, mechanism: &str) -> bool {
        if !self.noop_release_covers_dispatch(issue, sweep_id) {
            return false;
        }
        log::info!(
            "sweep_registry: issue #{issue} self-reported a no-op release for sweep {sweep_id} \
             — not charging that outcome to {mechanism}; the no-op cooldown is this outcome's \
             brake (#8912)"
        );
        true
    }

    /// Clear `issue`'s no-op-cooldown record (Issue #6670) — called on any
    /// dispatch that actually starts a sweep, so a genuine content change
    /// before the cooldown elapses is never suppressed (a fresh dispatch is
    /// itself proof the candidate is live again). Returns `true` when a record
    /// existed. Mirrors [`Self::clear_dispatch_backoff`].
    pub(crate) fn clear_noop_cooldown(&mut self, issue: u32) -> bool {
        self.noop_cooldown.remove(&issue).is_some()
    }

    /// Remaining no-op cooldown for `issue` at `now` (Issue #6670), or `None`
    /// when it may be dispatched immediately. `Some(Duration::ZERO)` is never
    /// returned — an elapsed window reads as `None`. Mirrors
    /// [`Self::dispatch_backoff_remaining`].
    #[must_use]
    pub fn noop_cooldown_remaining(&self, issue: u32, now: DateTime<Utc>) -> Option<Duration> {
        if !self.noop_cooldown_config.enabled {
            return None;
        }
        let state = self.noop_cooldown.get(&issue)?;
        let remaining = state.until - now;
        if remaining <= chrono::Duration::zero() {
            return None;
        }
        remaining.to_std().ok().filter(|d| !d.is_zero())
    }

    /// Remaining no-op cooldown that must **block a dispatch** of `issue` at
    /// `now` (Issue #9928): the longer of this host's own window
    /// ([`Self::noop_cooldown_remaining`]) and any live window a **peer** host
    /// broadcast (#7477), or `None` when neither is live.
    ///
    /// # Why the dispatch path needs its own accessor
    ///
    /// The two readers of this state had different scopes, and the narrower
    /// one is the one that actually enforces:
    ///
    /// - The work finder's **advisory pre-filter** reads the fleet-unioned
    ///   [`Self::noop_cooldown_issues`] (local ∪ peer-armed, since #7477). It
    ///   runs a tick ahead of the dispatch it gates, so a peer ad arriving
    ///   between selection and dispatch is already too late for it.
    /// - The **step 2.75 dispatch guard** (#6917) is the seam every dispatch
    ///   route funnels through — the work finder itself, the IPC/CLI
    ///   `{"Issue": <N>}` RPC behind `loom-daemon dispatch <N>` /
    ///   `mcp__loom__dispatch_sweep`, the epic supervisor, all three
    ///   watchdogs, the reaper's runtime-handoff re-dispatch — and it read the
    ///   HOST-LOCAL [`Self::noop_cooldown_remaining`].
    ///
    /// So a window armed by host A bound A's own dispatches and every peer's
    /// *pre-filter*, but nothing that reached `begin_issue_dispatch` on a peer
    /// by another route or a tick too early: hosts B/C/D re-claimed the same
    /// issue inside the very cooldown A's sweep had just armed, which is the
    /// fleet round-robin #7477 set out to stop (observed on
    /// `2AMLogic/gf180-pll#127`: four hosts re-dispatching one tracker issue
    /// inside ~10 minutes against byte-identical `origin/main`, two of them
    /// *after* a self-reported no-op release). Reading the union here makes
    /// the enforcing seam at least as fleet-aware as the advisory one, for
    /// every route at once.
    ///
    /// Takes the **longer** of the two rather than preferring either: both are
    /// statements that nothing has changed since a real check, and the later
    /// expiry is the one that still holds. Empty peer view
    /// (`safehouse.enabled` false, no coordination attached) degrades exactly
    /// to [`Self::noop_cooldown_remaining`] — the pre-#9928 behaviour.
    #[must_use]
    pub fn noop_cooldown_dispatch_block(&self, issue: u32, now: DateTime<Utc>) -> Option<Duration> {
        if !self.noop_cooldown_config.enabled {
            return None;
        }
        let local = self.noop_cooldown_remaining(issue, now);
        let fleet = self.fleet_noop_cooldown_remaining(issue);
        match (local, fleet) {
            (Some(l), Some(f)) => Some(l.max(f)),
            (l, f) => l.or(f),
        }
    }

    /// Remaining time on a live fleet-wide no-op cooldown a **peer** host
    /// armed for `issue` (Issue #9928) — the per-issue read behind
    /// [`Self::noop_cooldown_dispatch_block`], mirroring
    /// [`Self::fleet_noop_cooldown_issues`]'s disabled-state contract (`None`
    /// when no peer-claim view is attached).
    #[must_use]
    fn fleet_noop_cooldown_remaining(&self, issue: u32) -> Option<Duration> {
        let view = self.peer_claims.as_ref()?;
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        match view.lock() {
            Ok(v) => v.noop_cooldown_remaining_at(&repo, issue, Instant::now()),
            Err(poisoned) => {
                log::error!("sweep_registry: peer-claim view mutex poisoned ({poisoned:?})");
                None
            }
        }
    }

    /// Absolute no-op-cooldown expiry for `issue` at `now` (Issue #9311), or
    /// `None` when it may be dispatched immediately. Mirrors
    /// [`Self::noop_cooldown_remaining`] but returns the instant itself rather
    /// than the duration until it — this host's own local window only, not
    /// the fleet-unioned [`Self::noop_cooldown_issues`]: a peer-armed window's
    /// expiry is not available here.
    #[must_use]
    pub fn noop_cooldown_until(&self, issue: u32, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if !self.noop_cooldown_config.enabled {
            return None;
        }
        let state = self.noop_cooldown.get(&issue)?;
        (state.until > now).then_some(state.until)
    }

    /// Consecutive no-op releases recorded for `issue` (Issue #6670). `0` when
    /// no release is on record. Test/inspection helper, mirroring
    /// [`Self::dispatch_failure_count`].
    #[must_use]
    pub fn noop_release_count(&self, issue: u32) -> u32 {
        self.noop_cooldown.get(&issue).map_or(0, |s| s.consecutive)
    }

    /// Every issue whose no-op cooldown is still in effect at `now` (Issue
    /// #6670) — the set the work finder skips *before* the capacity gate,
    /// mirroring [`Self::quarantined_issues`] / [`Self::dispatch_backoff_issues`].
    ///
    /// Fleet-wide as of Issue #7477: unions this host's own local cooldown
    /// state with any live cooldown window a **peer** host has broadcast (see
    /// [`Self::fleet_noop_cooldown_issues`]) — this module's own doc comment
    /// (top of file) describes the fleet-scope gap this closes: a plain
    /// per-process `HashMap` let an N-host fleet round-robin a claim/release
    /// bail loop up to N times faster than the single-host cooldown was
    /// designed to prevent.
    #[must_use]
    pub fn noop_cooldown_issues(&self, now: DateTime<Utc>) -> HashSet<u32> {
        if !self.noop_cooldown_config.enabled {
            return HashSet::new();
        }
        let mut set: HashSet<u32> = self
            .noop_cooldown
            .iter()
            .filter(|(_, s)| s.until > now)
            .map(|(issue, _)| *issue)
            .collect();
        set.extend(self.fleet_noop_cooldown_issues());
        set
    }

    /// Issues with a live fleet-wide no-op cooldown armed by a **peer** host
    /// (Issue #7477) — empty when no peer-claim view is attached
    /// (`safehouse.enabled` false), mirroring
    /// [`Self::peer_claimed_issues`]'s disabled-state contract.
    #[must_use]
    fn fleet_noop_cooldown_issues(&self) -> HashSet<u32> {
        let Some(view) = &self.peer_claims else {
            return HashSet::new();
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        match view.lock() {
            Ok(v) => v.noop_cooldown_issues_at(&repo, Instant::now()),
            Err(poisoned) => {
                log::error!("sweep_registry: peer-claim view mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::{
        insert_clean_exit_running, no_progress_test_registry,
    };
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use serial_test::serial;
    use tempfile::tempdir;

    fn test_registry() -> SweepRegistry {
        let dir = tempdir().unwrap();
        SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
    }

    #[test]
    fn record_arms_cooldown_and_is_reflected_in_skip_set() {
        let mut reg = test_registry();
        assert_eq!(reg.noop_release_count(4242), 0);
        assert!(reg.noop_cooldown_remaining(4242, Utc::now()).is_none());

        reg.record_noop_release(4242, Some("no delta since last survey".into()));

        assert_eq!(reg.noop_release_count(4242), 1);
        let remaining = reg.noop_cooldown_remaining(4242, Utc::now());
        assert!(remaining.is_some_and(|d| d.as_secs() > 0));
        assert!(reg.noop_cooldown_issues(Utc::now()).contains(&4242));
    }

    #[test]
    fn repeated_records_accumulate_consecutive_but_stay_flat() {
        let mut reg = test_registry();
        reg.record_noop_release(99, None);
        reg.record_noop_release(99, None);
        reg.record_noop_release(99, None);
        assert_eq!(reg.noop_release_count(99), 3);
        // Flat, non-exponential: the window is always exactly the configured
        // cooldown from the most recent record, never longer.
        let remaining = reg.noop_cooldown_remaining(99, Utc::now()).unwrap();
        assert!(remaining.as_secs() <= reg.noop_cooldown_config().cooldown.as_secs());
    }

    #[test]
    fn cooldown_expires_after_ttl() {
        let mut reg = test_registry();
        reg.record_noop_release(7, None);
        let past_ttl = Utc::now() + chrono::Duration::hours(2);
        assert!(reg.noop_cooldown_remaining(7, past_ttl).is_none());
        assert!(!reg.noop_cooldown_issues(past_ttl).contains(&7));
    }

    #[test]
    fn clear_removes_the_record() {
        let mut reg = test_registry();
        reg.record_noop_release(55, None);
        assert!(reg.noop_cooldown_remaining(55, Utc::now()).is_some());
        assert!(reg.clear_noop_cooldown(55));
        assert!(reg.noop_cooldown_remaining(55, Utc::now()).is_none());
        assert!(!reg.clear_noop_cooldown(55), "second clear is a no-op");
    }

    #[test]
    fn disabled_mechanism_records_nothing() {
        let mut reg = test_registry();
        reg.set_noop_cooldown_config(NoopCooldownConfig {
            enabled: false,
            cooldown: Duration::from_secs(60),
            ..NoopCooldownConfig::default()
        });
        reg.record_noop_release(1, None);
        assert_eq!(reg.noop_release_count(1), 0);
        assert!(reg.noop_cooldown_remaining(1, Utc::now()).is_none());
        assert!(reg.noop_cooldown_issues(Utc::now()).is_empty());
    }

    #[test]
    fn independent_of_quarantine_and_dispatch_backoff() {
        // #6670 AC: the new cooldown must not interfere with either existing
        // mechanism — an issue can be in all three states simultaneously and
        // each is tracked (and clearable) independently.
        let mut reg = test_registry();
        reg.seed_quarantine_for_test(321);
        reg.record_dispatch_failure(321);
        reg.record_noop_release(321, None);

        assert!(reg.is_quarantined(321));
        assert!(reg.dispatch_backoff_remaining(321, Utc::now()).is_some());
        assert!(reg.noop_cooldown_remaining(321, Utc::now()).is_some());

        // Clearing the no-op cooldown alone leaves the other two untouched.
        assert!(reg.clear_noop_cooldown(321));
        assert!(reg.is_quarantined(321));
        assert!(reg.dispatch_backoff_remaining(321, Utc::now()).is_some());
        assert!(reg.noop_cooldown_remaining(321, Utc::now()).is_none());
    }

    // --- One release, one classification (Issue #8912) ----------------------

    /// The discriminator itself: a release is attributed to the sweep that
    /// was live when it was reported, and to no other.
    #[test]
    fn a_release_covers_only_the_dispatch_that_reported_it() {
        let dir = tempdir().unwrap();
        let mut reg = no_progress_test_registry(dir.path(), "OPEN", "", true);
        let sweep_id = insert_clean_exit_running(&mut reg, 89_120, 0);

        reg.record_noop_release(89_120, Some("epic tracking container".into()));

        assert!(reg.noop_release_covers_dispatch(89_120, &sweep_id));
        assert!(
            !reg.noop_release_covers_dispatch(89_120, "sweep-issue-89120-some-other-dispatch"),
            "a different dispatch of the same issue must be classified normally"
        );
        assert!(
            !reg.noop_release_covers_dispatch(99_999, &sweep_id),
            "and it must not leak to another issue"
        );
    }

    /// A release reported with no live sweep to attribute it to (an
    /// out-of-band `loom-daemon noop-cooldown record`) covers no reap — there
    /// is no dispatch of this daemon's to carve out.
    #[test]
    fn an_unattributable_release_covers_no_dispatch() {
        let mut reg = test_registry();
        reg.record_noop_release(89_120, None);
        assert!(reg.noop_cooldown_remaining(89_120, Utc::now()).is_some());
        assert!(!reg.noop_release_covers_dispatch(89_120, "sweep-issue-89120-x"));
    }

    /// THE regression, end to end through `reap_once`: the exact sequence
    /// observed on `2AMLogic/llm-cim#1` (2026-09-25) — the sweep self-reports
    /// a no-op release over IPC, then exits cleanly with no checkpoint, no PR
    /// and the issue still open. Before #8912 that single release armed a
    /// 3600s no-op cooldown AND a 300s PR-less retry window AND an
    /// insta-crash strike, the reaper re-classifying, a minute later, the very
    /// outcome `record_noop_release` had just cleared.
    #[test]
    fn a_self_reported_noop_release_is_not_also_a_prless_failure() {
        let dir = tempdir().unwrap();
        // Issue OPEN, no open linked PR, real forge probes enabled — the
        // fixture that DOES arm every brake when nothing self-reports a no-op
        // (asserted by the sibling test below).
        let mut reg = no_progress_test_registry(dir.path(), "OPEN", "", false);

        insert_clean_exit_running(&mut reg, 89_121, 0);
        // The sweep's `RecordNoopRelease` IPC call, while it is still Running.
        reg.record_noop_release(
            89_121,
            Some("epic tracking container, no direct buildable work".into()),
        );
        reg.reap_once();

        assert!(
            reg.noop_cooldown_remaining(89_121, Utc::now()).is_some(),
            "the self-reported cooldown must survive the reap — it is this outcome's brake"
        );
        assert_eq!(
            reg.prless_release_count(89_121),
            0,
            "a self-reported no-op must not also be counted as a PR-less release (#8912)"
        );
        assert!(
            reg.prless_retry_remaining(89_121, Utc::now()).is_none(),
            "no 300s PR-less window may undercut the 3600s no-op cooldown"
        );
        assert_eq!(
            reg.insta_crash_count(89_121),
            0,
            "a deliberate no-op conclusion must not accrue toward quarantine"
        );
        assert_eq!(
            reg.dispatch_failure_count(89_121),
            0,
            "a deliberate no-op conclusion is not a failed dispatch"
        );
    }

    /// The guard on the fix above: the SAME fixture without a no-op
    /// self-report must still arm every brake. The carve-out is scoped to a
    /// dispatch that actually reported one — it must never become a blanket
    /// exemption for clean, PR-less exits.
    #[test]
    fn a_prless_exit_without_a_noop_release_still_counts() {
        let dir = tempdir().unwrap();
        let mut reg = no_progress_test_registry(dir.path(), "OPEN", "", false);

        insert_clean_exit_running(&mut reg, 89_122, 0);
        reg.reap_once();

        assert_eq!(
            reg.prless_release_count(89_122),
            1,
            "an ordinary PR-less clean exit must still charge the #7972 tally"
        );
        assert_eq!(
            reg.insta_crash_count(89_122),
            1,
            "an ordinary no-progress clean exit must still accrue toward quarantine"
        );
        assert_eq!(
            reg.dispatch_failure_count(89_122),
            1,
            "an ordinary no-progress clean exit must still arm the #4485 backoff"
        );
    }

    /// The carve-out covers ONE terminal outcome, not the issue: the next
    /// dispatch — a genuine PR-less failure — counts normally, so the #7972
    /// bound still converges on a genuinely failing issue.
    #[test]
    fn the_carve_out_does_not_carry_into_the_next_dispatch() {
        let dir = tempdir().unwrap();
        let mut reg = no_progress_test_registry(dir.path(), "OPEN", "", false);

        insert_clean_exit_running(&mut reg, 89_123, 0);
        reg.record_noop_release(89_123, Some("nothing to do this pass".into()));
        reg.reap_once();
        assert_eq!(reg.prless_release_count(89_123), 0);

        // A second dispatch that self-reports nothing.
        insert_clean_exit_running(&mut reg, 89_123, 1);
        reg.reap_once();

        assert_eq!(
            reg.prless_release_count(89_123),
            1,
            "the suppression is scoped to one release, not to the issue (#8912)"
        );
    }

    // --- Config resolution precedence (env > config > default) -------------

    #[test]
    #[serial]
    fn resolve_uses_shipped_defaults_with_no_env_or_file() {
        let dir = tempdir().unwrap();
        // Ensure a clean env for this process-serial test.
        std::env::remove_var(NOOP_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(NOOP_COOLDOWN_SECS_ENV);
        let cfg = resolve_noop_cooldown_config(dir.path());
        assert!(cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), DEFAULT_NOOP_COOLDOWN_SECS);
    }

    #[test]
    #[serial]
    fn resolve_file_config_overrides_default() {
        let dir = tempdir().unwrap();
        std::env::remove_var(NOOP_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(NOOP_COOLDOWN_SECS_ENV);
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"noopCooldown":{"enabled":false,"cooldownSecs":120}}}}"#,
        )
        .unwrap();
        let cfg = resolve_noop_cooldown_config(dir.path());
        assert!(!cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), 120);
    }

    #[test]
    #[serial]
    fn resolve_env_overrides_file() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"noopCooldown":{"enabled":false,"cooldownSecs":120}}}}"#,
        )
        .unwrap();
        std::env::set_var(NOOP_COOLDOWN_ENABLE_ENV, "1");
        std::env::set_var(NOOP_COOLDOWN_SECS_ENV, "42");
        let cfg = resolve_noop_cooldown_config(dir.path());
        std::env::remove_var(NOOP_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(NOOP_COOLDOWN_SECS_ENV);
        assert!(cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), 42);
    }

    #[test]
    #[serial]
    fn zero_or_invalid_env_values_fall_through() {
        let dir = tempdir().unwrap();
        std::env::set_var(NOOP_COOLDOWN_SECS_ENV, "0");
        let cfg = resolve_noop_cooldown_config(dir.path());
        std::env::remove_var(NOOP_COOLDOWN_SECS_ENV);
        assert_eq!(cfg.cooldown.as_secs(), DEFAULT_NOOP_COOLDOWN_SECS);
    }

    // --- Fleet-wide visibility (Issue #7477) --------------------------------

    /// Arming a LOCAL no-op cooldown must also broadcast it fleet-wide over
    /// the same peer-claim channel dispatch claims use, so a peer host does
    /// not immediately re-offer the same candidate this host just
    /// self-reported "no actionable delta" on. This is the exact fleet-scope
    /// gap #7477 reports: pre-fix, this state was a plain per-process
    /// `HashMap`, invisible to any other host.
    #[test]
    fn record_noop_release_broadcasts_fleet_wide() {
        let mut reg = test_registry();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        reg.set_peer_claim_publisher(tx);

        reg.record_noop_release(7466, Some("still nothing to do".into()));

        let ad = rx.try_recv().expect("a cooldown ad must be published");
        assert_eq!(ad.kind, crate::peer_claims::ClaimKind::NoopCooldownArmed);
        assert_eq!(ad.issue, 7466);
        assert_eq!(ad.remaining_secs, Some(reg.noop_cooldown_config().cooldown.as_secs()));
    }

    /// A disabled mechanism must not broadcast either — mirrors
    /// `disabled_mechanism_records_nothing`'s local-state contract.
    #[test]
    fn disabled_mechanism_does_not_broadcast() {
        let mut reg = test_registry();
        reg.set_noop_cooldown_config(NoopCooldownConfig {
            enabled: false,
            cooldown: Duration::from_secs(60),
            ..NoopCooldownConfig::default()
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        reg.set_peer_claim_publisher(tx);

        reg.record_noop_release(1, None);

        assert!(rx.try_recv().is_err(), "a disabled mechanism must publish nothing");
    }

    /// `noop_cooldown_issues` must union this host's own local cooldown
    /// state with a live window a PEER host has broadcast — the fleet-scope
    /// fix. A registry with NO local record for an issue still must not
    /// offer it while a peer's broadcast window is live.
    #[test]
    fn noop_cooldown_issues_reflects_a_peer_armed_window() {
        let mut reg = test_registry();
        assert!(!reg.noop_cooldown_issues(Utc::now()).contains(&7466));

        let repo = peer_claims::repo_slug(&reg.config().workspace_root);
        let view =
            Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
        {
            let mut v = view.lock().unwrap();
            v.observe_noop_cooldown_at(
                &ClaimAd::noop_cooldown_armed(7466, repo, "peer".into(), 1, "ts".into(), 3600),
                Instant::now(),
            );
        }
        reg.set_peer_claims(view);
        assert!(
            reg.noop_cooldown_issues(Utc::now()).contains(&7466),
            "a peer-armed no-op cooldown must suppress this host's dispatch too"
        );
    }

    // --- Fleet-aware dispatch-path read (Issue #9928) -----------------------

    /// Helper: attach a peer-claim view to `reg` carrying a live
    /// `remaining_secs`-long no-op cooldown a PEER host armed for `issue`.
    fn attach_peer_armed_cooldown(reg: &mut SweepRegistry, issue: u32, remaining_secs: u64) {
        let repo = peer_claims::repo_slug(&reg.config().workspace_root);
        let view =
            Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
        {
            let mut v = view.lock().unwrap();
            v.observe_noop_cooldown_at(
                &ClaimAd::noop_cooldown_armed(
                    issue,
                    repo,
                    "peer".into(),
                    1,
                    "ts".into(),
                    remaining_secs,
                ),
                Instant::now(),
            );
        }
        reg.set_peer_claims(view);
    }

    /// THE #9928 gap at the registry level: with NO local record but a live
    /// peer-armed window, the host-local `noop_cooldown_remaining` reads
    /// `None` (so the dispatch guard let a direct `--claim-owned N`
    /// re-dispatch straight through) while the fleet-unioned
    /// `noop_cooldown_issues` the work finder reads already said "skip".
    /// `noop_cooldown_dispatch_block` is the dispatch path's fleet-aware read.
    #[test]
    fn dispatch_block_sees_a_peer_armed_window_that_the_local_read_misses() {
        let mut reg = test_registry();
        attach_peer_armed_cooldown(&mut reg, 9928, 3600);

        assert!(
            reg.noop_cooldown_remaining(9928, Utc::now()).is_none(),
            "the host-local read sees nothing — this is the gap, not the fix"
        );
        assert!(
            reg.noop_cooldown_issues(Utc::now()).contains(&9928),
            "the work-finder pre-filter has been fleet-aware since #7477"
        );

        let remaining = reg
            .noop_cooldown_dispatch_block(9928, Utc::now())
            .expect("the dispatch path must see a peer-armed window too (#9928)");
        assert!(remaining.as_secs() > 0);
    }

    /// With both windows live, the longer one holds — neither reader is
    /// preferred, and a short local window can never shorten a peer's.
    #[test]
    fn dispatch_block_takes_the_longer_of_local_and_peer_windows() {
        let mut reg = test_registry();
        reg.set_noop_cooldown_config(NoopCooldownConfig {
            enabled: true,
            cooldown: Duration::from_secs(60),
            ..NoopCooldownConfig::default()
        });
        reg.record_noop_release(9929, None);
        attach_peer_armed_cooldown(&mut reg, 9929, 3600);

        let remaining = reg.noop_cooldown_dispatch_block(9929, Utc::now()).unwrap();
        assert!(
            remaining.as_secs() > 60,
            "the peer's longer window must win; got {}s",
            remaining.as_secs()
        );
    }

    /// No peer view attached (`safehouse.enabled` false) degrades exactly to
    /// the host-local read — the pre-#9928 behaviour, unchanged.
    #[test]
    fn dispatch_block_degrades_to_the_local_window_with_no_peer_view() {
        let mut reg = test_registry();
        assert!(reg.noop_cooldown_dispatch_block(42, Utc::now()).is_none());
        reg.record_noop_release(42, None);
        // One fixed `now` for both reads — two `Utc::now()` calls differ by
        // the microseconds between them.
        let now = Utc::now();
        assert_eq!(reg.noop_cooldown_dispatch_block(42, now), reg.noop_cooldown_remaining(42, now),);
    }

    /// An expired peer window never blocks — the `expiry > now` discipline
    /// every other read in this lane uses.
    #[test]
    fn dispatch_block_ignores_an_expired_peer_window() {
        let mut reg = test_registry();
        // `remaining_secs: 0` is how a malformed/already-lapsed ad degrades.
        attach_peer_armed_cooldown(&mut reg, 9930, 0);
        assert!(reg.noop_cooldown_dispatch_block(9930, Utc::now()).is_none());
    }

    /// A disabled mechanism must ignore a peer-armed window on the dispatch
    /// path too — mirrors `disabled_mechanism_ignores_peer_armed_window`.
    #[test]
    fn disabled_mechanism_dispatch_block_ignores_peer_armed_window() {
        let mut reg = test_registry();
        reg.set_noop_cooldown_config(NoopCooldownConfig {
            enabled: false,
            cooldown: Duration::from_secs(60),
            ..NoopCooldownConfig::default()
        });
        attach_peer_armed_cooldown(&mut reg, 9928, 3600);
        assert!(reg.noop_cooldown_dispatch_block(9928, Utc::now()).is_none());
    }

    /// A disabled mechanism must ignore even a live peer-armed window —
    /// mirrors the local-state early return.
    #[test]
    fn disabled_mechanism_ignores_peer_armed_window() {
        let mut reg = test_registry();
        reg.set_noop_cooldown_config(NoopCooldownConfig {
            enabled: false,
            cooldown: Duration::from_secs(60),
            ..NoopCooldownConfig::default()
        });
        let repo = peer_claims::repo_slug(&reg.config().workspace_root);
        let view =
            Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
        {
            let mut v = view.lock().unwrap();
            v.observe_noop_cooldown_at(
                &ClaimAd::noop_cooldown_armed(7466, repo, "peer".into(), 1, "ts".into(), 3600),
                Instant::now(),
            );
        }
        reg.set_peer_claims(view);
        assert!(!reg.noop_cooldown_issues(Utc::now()).contains(&7466));
    }
}
