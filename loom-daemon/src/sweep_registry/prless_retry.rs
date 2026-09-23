//! Bounded retry for PR-less claim/release cycles (Issue #7972).
//!
//! # The gap
//!
//! Three bounded-retry mechanisms already live in this registry, and none of
//! them covers the shape #7972 reports:
//!
//! - [`super::quarantine`] (#3939) counts **insta-crashes** — a checkpoint-less
//!   terminal transition inside a 60s window. A sweep that runs for eight
//!   minutes and then dies is not an insta-crash.
//! - [`super::dispatch`]'s per-issue backoff (#4485) is armed by an insta-crash,
//!   by the #4366 no-progress predicate (a *clean* exit that advanced nothing),
//!   or by the #4123 open-PR guard. A sweep that exits non-zero after writing a
//!   phase checkpoint trips none of those — worse, `reap_once`'s
//!   `checkpoint_progress` arm actively **clears** the backoff, the quarantine
//!   tally, and the resume runway, because a rewritten checkpoint is read as
//!   proof of forward progress.
//! - [`super::noop_cooldown`] (#6670) only fires when a Builder **self-reports**
//!   "no changes needed" by writing `.no-changes-needed` before exiting cleanly.
//!   A Builder that crashes, hits a build error, or cannot find its scope writes
//!   no marker.
//!
//! The intersection of those three carve-outs is a real, observed loop:
//! `loom/#7893` was claimed and released **14 times in just over four hours**
//! (55 `loom:issue`/`loom:building` label events, several claim/release pairs
//! landing inside the same minute) and produced **zero PRs**. Every attempt
//! advanced its checkpoint far enough to look productive to the reaper, so every
//! attempt reset every existing brake, and the work finder re-offered the issue
//! on the very next tick. Nothing counted the attempts, nothing spaced them out,
//! and nothing ever wrote down *why* the previous attempt had failed.
//!
//! # The mechanism
//!
//! One more per-issue tally, deliberately keyed on the only signal that is
//! honest about this shape — **did the dispatch leave a pull request behind?**
//! — rather than on exit code, duration, or checkpoint movement, each of which
//! the observed loop satisfied:
//!
//! 1. Every terminal sweep outcome is classified by
//!    [`SweepRegistry::note_prless_terminal_outcome`]. An observed merge phase
//!    or a verified open linked PR **clears** the tally; a verified "no open
//!    linked PR" **records** a PR-less release; an unverifiable probe does
//!    neither (fail-open, like every other forge-probing predicate here).
//! 2. Each recorded PR-less release arms an exponential window
//!    ([`DEFAULT_PRLESS_RETRY_BACKOFF_SECS`] doubling to
//!    [`DEFAULT_PRLESS_RETRY_MAX_BACKOFF_SECS`]) that the work finder skips
//!    before the capacity gate — so the next re-claim cannot land in the same
//!    minute as the last one.
//! 3. On the **second** consecutive PR-less release the reason is posted to the
//!    issue, so the next claimer (or a human) can see why it keeps failing
//!    instead of rediscovering it.
//! 4. On the [`PrlessRetryConfig::threshold`]-th consecutive PR-less release the
//!    issue is **held**: `loom:blocked` on, `loom:issue` off, plus a comment
//!    naming the failure and the count. The work finder's own skip-label filter
//!    takes it from there, and the hold is released the way every other
//!    `loom:blocked` park is — by a human or a Doctor who fixed the cause.
//!
//! # Non-interference
//!
//! A distinct in-memory map, a distinct config block, and a distinct
//! work-finder skip set — the same separation #6670 and #7528 each established,
//! for the same reason: an issue can be quarantined, dispatch-backed-off,
//! no-op-cooling-down, decline-cooled *and* PR-less-held simultaneously without
//! any of the five perturbing the others. In particular this tally is **not**
//! charged for outcomes another mechanism already owns and which say nothing
//! about the issue itself:
//!
//! - a `no-usable-account` pool death (#7708) — a host-level fault,
//! - a pre-flight-classified death (#4386) — a workspace-level fault,
//! - a hard-exclusion decline (#7528) — no role has standing to act yet,
//! - a self-reported no-op release (#6670) — a deliberate conclusion, not a
//!   failure; [`SweepRegistry::record_noop_release`] clears this tally,
//! - a superseded claim — a newer sweep owns the issue.
//!
//! Each of those is excluded at the call site (or, for the no-op release, by an
//! explicit clear), never by this module second-guessing the classification.

use super::*;

/// Env var toggling the PR-less retry bound (Issue #7972). `0`/`false`/`no`/
/// `off` disables; `1`/`true`/`yes`/`on` forces on. Overrides config. Defaults
/// ON, like every sibling brake — it is a dispatch-efficiency backstop, and the
/// loop it bounds burns a worker spawn, a rotated token, and a pair of forge
/// label writes per cycle.
pub const PRLESS_RETRY_ENABLE_ENV: &str = "LOOM_WORK_FINDER_PRLESS_RETRY";

/// Env var overriding the consecutive-PR-less-release threshold at which the
/// issue is held (Issue #7972). A zero/invalid value falls through to
/// config/default.
pub const PRLESS_RETRY_THRESHOLD_ENV: &str = "LOOM_WORK_FINDER_PRLESS_RETRY_THRESHOLD";

/// Env var overriding the first PR-less-release backoff, in seconds (Issue
/// #7972). Doubled per consecutive release, capped by
/// [`PRLESS_RETRY_MAX_BACKOFF_SECS_ENV`]. A zero/invalid value falls through to
/// config/default.
pub const PRLESS_RETRY_BACKOFF_SECS_ENV: &str = "LOOM_WORK_FINDER_PRLESS_RETRY_BACKOFF_SECS";

/// Env var overriding the PR-less-release backoff ceiling, in seconds (Issue
/// #7972) — also the idle window after which a cold streak restarts from zero,
/// and the TTL of the in-memory half of a hold. A zero/invalid value falls
/// through to config/default.
pub const PRLESS_RETRY_MAX_BACKOFF_SECS_ENV: &str =
    "LOOM_WORK_FINDER_PRLESS_RETRY_MAX_BACKOFF_SECS";

/// Default consecutive-PR-less-release threshold before the issue is held
/// (#7972): 3, matching [`DEFAULT_QUARANTINE_THRESHOLD`]. Two failures can be
/// coincidence (a forge blip, a one-off token death that dodged the #7708
/// carve-out); a third consecutive dispatch that ends with no pull request is a
/// pattern, and the observed incident ran to fourteen.
pub const DEFAULT_PRLESS_RETRY_THRESHOLD: u32 = 3;

/// Default first-release backoff (#7972): 5 minutes.
///
/// Deliberately longer than [`DEFAULT_DISPATCH_BACKOFF_BASE_SECS`] (60s): the
/// sharpest edge in the reported evidence is claim/release pairs landing *in
/// the same minute*, and unlike a fast crash — which the 60s ladder is sized
/// for — a PR-less release usually follows a full agent session, so re-trying
/// it a minute later cannot plausibly reach a different outcome.
pub const DEFAULT_PRLESS_RETRY_BACKOFF_SECS: u64 = 300;

/// Default backoff ceiling (#7972): one hour. Reached after four consecutive
/// releases (300s -> 600 -> 1200 -> 2400 -> 3600), which on the default
/// threshold of 3 is past the point where the issue is held anyway — the
/// ceiling matters mostly as the streak-cold window and the hold's in-memory
/// TTL.
pub const DEFAULT_PRLESS_RETRY_MAX_BACKOFF_SECS: u64 = 3600;

/// Marker prefix on every comment this module posts, so the per-attempt notes
/// and the hold notice are machine-identifiable (and greppable) the way
/// [`QUARANTINE_COMMENT_MARKER`] is for #3939.
pub const PRLESS_RETRY_COMMENT_MARKER: &str = "<!-- loom:prless-retry (#7972) -->";

/// Resolved PR-less-retry parameters (Issue #7972), set on the registry at
/// construction so the reaper and work finder can enforce them without a
/// per-tick config read — mirrors [`NoopCooldownConfig`] /
/// [`DispatchBackoffConfig`]'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrlessRetryConfig {
    /// Whether the bound is active. When `false`, recording a PR-less release
    /// is a no-op (no state written, no window armed, no comment, no hold) —
    /// byte-for-byte the pre-#7972 path.
    pub enabled: bool,
    /// Consecutive PR-less releases before the issue is held with
    /// `loom:blocked`.
    pub threshold: u32,
    /// Delay applied after the first PR-less release; doubled per consecutive
    /// release.
    pub backoff: Duration,
    /// Ceiling on the doubling — also the idle window after which an issue's
    /// consecutive tally restarts from zero, and the TTL of a hold's in-memory
    /// half (the durable half is the `loom:blocked` label).
    pub max_backoff: Duration,
}

impl Default for PrlessRetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: DEFAULT_PRLESS_RETRY_THRESHOLD,
            backoff: Duration::from_secs(DEFAULT_PRLESS_RETRY_BACKOFF_SECS),
            max_backoff: Duration::from_secs(DEFAULT_PRLESS_RETRY_MAX_BACKOFF_SECS),
        }
    }
}

/// Per-issue PR-less-retry bookkeeping (Issue #7972).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrlessRetryState {
    /// When the most recent PR-less release was recorded. Also the streak-cold
    /// reference point: a new release more than
    /// [`PrlessRetryConfig::max_backoff`] after this restarts the tally at 1.
    pub(crate) recorded_at: DateTime<Utc>,
    /// The instant at which the next dispatch attempt becomes allowed.
    pub(crate) until: DateTime<Utc>,
    /// Consecutive PR-less releases recorded for this issue without an
    /// intervening dispatch that left a pull request behind.
    pub(crate) consecutive: u32,
    /// Whether the threshold has been reached and the forge hold applied.
    pub(crate) held: bool,
    /// The failure context supplied by the recording caller — logged verbatim
    /// and posted to the issue, so the next claimer sees it.
    pub(crate) reason: String,
}

/// What [`SweepRegistry::apply_prless_hold_label`] actually did (Issue #7972).
/// Only [`Self::VetoedOpenPr`] is positive evidence that the tally itself was
/// wrong; the other two vetoes leave it standing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrlessHoldOutcome {
    /// `loom:blocked` applied, `loom:issue` removed, hold notice posted.
    Applied,
    /// The issue is already closed — nothing to hold, tally kept.
    VetoedClosed,
    /// Re-verification found an open linked PR: this was never a PR-less loop.
    VetoedOpenPr,
    /// Label flips are disabled (hermetic fixture) — no forge state to touch.
    VetoedNoForge,
}

/// The subset of `.loom/config.json → autonomous.workFinder.prlessRetry` this
/// module consumes (Issue #7972). Mirrors [`NoopCooldownFileConfig`]'s shape:
/// every field is `Option` so an absent key falls through to the env-var /
/// built-in-default resolution — precedence **env > config > default**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrlessRetryFileConfig {
    /// `autonomous.workFinder.prlessRetry.enabled`.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.prlessRetry.threshold` (zero/invalid dropped).
    pub threshold: Option<u32>,
    /// `autonomous.workFinder.prlessRetry.backoffSecs` (zero/invalid dropped).
    pub backoff_secs: Option<u64>,
    /// `autonomous.workFinder.prlessRetry.maxBackoffSecs` (zero/invalid
    /// dropped).
    pub max_backoff_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.workFinder.prlessRetry` (Issue #7972),
/// soft-failing every field to `None` on a missing file, malformed JSON, or an
/// absent block — mirrors [`read_noop_cooldown_file_config`].
#[must_use]
pub fn read_prless_retry_file_config(repo_root: &Path) -> PrlessRetryFileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(c) = crate::config_resolver::get_path(&effective, "autonomous.workFinder.prlessRetry")
    else {
        return PrlessRetryFileConfig::default();
    };
    PrlessRetryFileConfig {
        enabled: c.get("enabled").and_then(serde_json::Value::as_bool),
        threshold: c
            .get("threshold")
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| u32::try_from(n).ok()),
        backoff_secs: c
            .get("backoffSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        max_backoff_secs: c
            .get("maxBackoffSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve the full [`PrlessRetryConfig`] for `repo_root` with precedence
/// **env > config > default** for every knob (Issue #7972), mirroring
/// [`resolve_noop_cooldown_config`].
#[must_use]
pub fn resolve_prless_retry_config(repo_root: &Path) -> PrlessRetryConfig {
    let file = read_prless_retry_file_config(repo_root);

    let enabled = if let Ok(v) = std::env::var(PRLESS_RETRY_ENABLE_ENV) {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    } else {
        file.enabled.unwrap_or(true)
    };

    let threshold = std::env::var(PRLESS_RETRY_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .or(file.threshold)
        .unwrap_or(DEFAULT_PRLESS_RETRY_THRESHOLD);

    let backoff_secs = std::env::var(PRLESS_RETRY_BACKOFF_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.backoff_secs)
        .unwrap_or(DEFAULT_PRLESS_RETRY_BACKOFF_SECS);

    let max_backoff_secs = std::env::var(PRLESS_RETRY_MAX_BACKOFF_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.max_backoff_secs)
        .unwrap_or(DEFAULT_PRLESS_RETRY_MAX_BACKOFF_SECS)
        // A ceiling below the base would make `backoff_delay` return the
        // ceiling immediately; clamp rather than silently inverting the ladder.
        .max(backoff_secs);

    PrlessRetryConfig {
        enabled,
        threshold,
        backoff: Duration::from_secs(backoff_secs),
        max_backoff: Duration::from_secs(max_backoff_secs),
    }
}

/// Resolve, apply, and announce `registry`'s PR-less-retry parameters for
/// `repo_root` (Issue #7972) — the provision-time one-liner
/// `daemon_service.rs` calls for the default workspace.
///
/// The resolve/set/log trio lives here rather than inline at the call site
/// because `daemon_service.rs` is over the file-size ratchet's threshold and
/// frozen at its current size (`.loom/docs/file-size-policy.md`); keeping the
/// announcement next to the config it describes is the better home anyway.
pub fn configure_prless_retry(registry: &mut SweepRegistry, repo_root: &Path) {
    let config = resolve_prless_retry_config(repo_root);
    registry.set_prless_retry_config(config);
    log::info!(
        "sweep_registry: PR-less retry bound {} (threshold={}, backoff={}s, max={}s) (#7972)",
        if config.enabled {
            "enabled"
        } else {
            "disabled"
        },
        config.threshold,
        config.backoff.as_secs(),
        config.max_backoff.as_secs()
    );
}

impl SweepRegistry {
    /// Set the PR-less-retry parameters (Issue #7972). `daemon_service.rs` and
    /// the workspace pool call this once at provision time with the resolved
    /// env > config > default value, mirroring [`Self::set_noop_cooldown_config`].
    pub fn set_prless_retry_config(&mut self, config: PrlessRetryConfig) {
        self.prless_retry_config = config;
    }

    /// Read-only accessor for the PR-less-retry parameters (Issue #7972).
    #[must_use]
    pub fn prless_retry_config(&self) -> PrlessRetryConfig {
        self.prless_retry_config
    }

    /// Classify one terminal sweep outcome for `issue` against the only
    /// question this bound cares about — **did it leave a pull request
    /// behind?** (Issue #7972).
    ///
    /// `open_pr` is the caller's already-computed open-PR verdict when it has
    /// one (the checkpoint-less clean-exit branch of `reap_once` probes anyway,
    /// for #4366/#6350). `None` means "I did not probe" — and this
    /// deliberately does **not** probe on the caller's behalf. Two reasons:
    ///
    /// 1. `reap_once`'s crashed branch gates its closes-graph query on a
    ///    Builder-or-later checkpoint phase, and issuing one unconditionally
    ///    here would break that (pinned by
    ///    `reaper_does_not_resume_pre_builder_checkpoint`) — a pre-Builder
    ///    crash must stay a pure, forge-free crash handling path.
    /// 2. A per-reap extra round trip is precisely the forge spend this bound
    ///    exists to save.
    ///
    /// So the `None` case falls back to state the daemon already holds in
    /// memory: the PR number this sweep sampled from its own checkpoint
    /// (#4704 `sampled_pr_number`, written from `builder-done` onward), then
    /// the verified open-PR memo (#6788). The residual risk — an issue with an
    /// open PR whose memo has expired, reached via a sweep that sampled no PR
    /// number — is caught at the one place it matters, immediately before the
    /// hold is applied (see [`Self::apply_prless_hold_label`]).
    ///
    /// Three outcomes, and only one of them is a failure:
    ///
    /// - **Productive** — the sweep was observed reaching the merge phase, or
    ///   the forge verifiably has an open linked PR. Clears the tally. (The
    ///   #4123 open-PR self-skip lands here too, and correctly so: an issue with
    ///   an open PR is not stuck in a PR-less loop, it is waiting on Judge.)
    /// - **Unverifiable** — the probe failed (`gh` missing, timed out,
    ///   rate-limited). Neither arms nor clears: a forge outage must never be
    ///   able to manufacture a hold, and must never silently erase a real
    ///   streak either. Same fail-open discipline as
    ///   [`Self::is_no_progress`](Self::is_no_progress)'s two arms.
    /// - **PR-less** — the forge verifiably has no open linked PR. Records the
    ///   release (see [`Self::record_prless_release`]).
    ///
    /// Carve-outs that must NOT reach here at all (pool death #7708, pre-flight
    /// death #4386, hard-exclusion decline #7528, superseded claim) are excluded
    /// by the caller — see the module doc.
    pub(crate) fn note_prless_terminal_outcome(
        &mut self,
        issue: u32,
        sweep_id: &str,
        open_pr: Option<OpenPrProbe>,
        reason: &str,
    ) {
        if !self.prless_retry_config.enabled {
            return;
        }
        // Strongest productive signal first, and free: an observed merge phase
        // means the work landed, whatever the forge says about open PRs now (a
        // merged PR is not an open one).
        if self.sampled_reached_merge(sweep_id) {
            if self.clear_prless_retry(issue) {
                log::info!(
                    "sweep_registry: issue #{issue} reached the merge phase — clearing its \
                     PR-less retry tally (#7972)"
                );
            }
            return;
        }
        let verdict = match open_pr {
            Some(v) => v,
            // Forge-free fallback — see this method's doc comment.
            None => match self.sampled_pr_number(sweep_id) {
                Some(pr) => OpenPrProbe::Open(pr),
                None => self
                    .fresh_open_pr_memo(issue, Utc::now())
                    .map_or(OpenPrProbe::NoneOpen, |memo| OpenPrProbe::Open(memo.pr)),
            },
        };
        match verdict {
            OpenPrProbe::Open(pr) => {
                if self.clear_prless_retry(issue) {
                    log::info!(
                        "sweep_registry: issue #{issue} has an open linked PR #{pr} — clearing \
                         its PR-less retry tally (#7972)"
                    );
                }
            }
            OpenPrProbe::ProbeFailed => {
                log::debug!(
                    "sweep_registry: open-PR probe for issue #{issue} was unverifiable — leaving \
                     its PR-less retry tally untouched (fail-open, #7972)"
                );
            }
            OpenPrProbe::NoneOpen => self.record_prless_release(issue, reason),
        }
    }

    /// `reap_once`'s **crashed / checkpointed** call site (Issue #7972): a
    /// sweep that left a phase checkpoint behind and then died.
    ///
    /// This is the branch the observed #7893 loop lived in — and precisely the
    /// one no existing brake could see, because `reap_once` reads a checkpoint
    /// rewritten by the run as proof of forward progress and therefore
    /// *clears* the dispatch backoff, the quarantine tally and the resume
    /// runway on it. Fourteen consecutive dispatches each reached the builder
    /// phase, each produced nothing, and each reset every brake.
    ///
    /// Lives here rather than inline at the call site so `reaper.rs` carries
    /// only the guard and the call: the reason strings are this mechanism's
    /// vocabulary, not the reaper's.
    pub(crate) fn note_prless_crash_outcome(
        &mut self,
        issue: u32,
        sweep_id: &str,
        exit_code: Option<i32>,
        duration_sec: i64,
        phase: Option<&str>,
    ) {
        let died = match exit_code {
            Some(code) => format!("crashed after {duration_sec}s (exit {code})"),
            None => format!("died after {duration_sec}s (no exit status observed)"),
        };
        let at_phase = phase
            .map(|p| format!(" at phase `{p}`"))
            .unwrap_or_default();
        let reason = format!("sweep {died}{at_phase} without opening a pull request");
        // `None` open-PR verdict: this branch never probed, so let the bound
        // run (and memo-serve) its own.
        self.note_prless_terminal_outcome(issue, sweep_id, None, &reason);
    }

    /// `reap_once`'s **checkpoint-less exit** call site (Issue #7972) — the
    /// sibling of [`Self::note_prless_crash_outcome`] for a sweep that left no
    /// checkpoint at all.
    ///
    /// `open_pr` carries that branch's already-computed verdict when it has one
    /// (the #4366/#6350 arms probe on a clean exit, and #8355 seeds the #6788
    /// memo from the sweep's own sampled PR number), so the common path costs
    /// no extra forge round trip.
    pub(crate) fn note_prless_exit_outcome(
        &mut self,
        issue: u32,
        sweep_id: &str,
        open_pr: Option<OpenPrProbe>,
        exit_code: Option<i32>,
        duration_sec: i64,
    ) {
        let ended = match exit_code {
            Some(code) => format!("exited {code} after {duration_sec}s"),
            None => format!("ended after {duration_sec}s (no exit status observed)"),
        };
        let reason = format!(
            "sweep {ended} without opening a pull request, without a phase checkpoint, and \
             without a `.no-changes-needed` marker"
        );
        self.note_prless_terminal_outcome(issue, sweep_id, open_pr, &reason);
    }

    /// Record one **PR-less release** for `issue` (Issue #7972): a dispatch
    /// claimed the issue, released it, and left no pull request behind.
    ///
    /// Arms (or re-arms) an exponential window — `backoff` doubling per
    /// consecutive release, capped at `max_backoff` — so the next re-claim
    /// cannot land in the same minute as this one. On the second consecutive
    /// release the reason is posted to the issue; on the
    /// [`PrlessRetryConfig::threshold`]-th the issue is held (`loom:blocked` on,
    /// `loom:issue` off) with a comment naming the failure and the count.
    ///
    /// A streak older than `max_backoff` is treated as cold and restarts at 1,
    /// mirroring [`Self::record_dispatch_failure`]'s own staleness rule: a
    /// failure a day ago and a failure now are not a pattern.
    ///
    /// A no-op when the mechanism is disabled, mirroring
    /// [`Self::record_noop_release`]'s disabled-path contract.
    pub(crate) fn record_prless_release(&mut self, issue: u32, reason: &str) {
        if !self.prless_retry_config.enabled {
            return;
        }
        let cfg = self.prless_retry_config;
        let now = Utc::now();
        let cold_secs = i64::try_from(cfg.max_backoff.as_secs()).unwrap_or(i64::MAX);
        let consecutive = match self.prless_retry.get(&issue) {
            Some(prev) if (now - prev.recorded_at).num_seconds() <= cold_secs => {
                prev.consecutive.saturating_add(1)
            }
            _ => 1,
        };
        let held = consecutive >= cfg.threshold;
        // A hold's in-memory half rides the ceiling rather than the ladder:
        // past the threshold the ladder has nothing left to say, and the
        // durable half of the hold is the `loom:blocked` label the work
        // finder's skip-label filter already honors.
        let delay = if held {
            cfg.max_backoff
        } else {
            backoff_delay(consecutive, cfg.backoff, cfg.max_backoff)
        };
        let until =
            now + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero());
        self.prless_retry.insert(
            issue,
            PrlessRetryState {
                recorded_at: now,
                until,
                consecutive,
                held,
                reason: reason.to_string(),
            },
        );
        if held {
            log::warn!(
                "sweep_registry: issue #{issue} has now been claimed and released {consecutive} \
                 times in a row without producing a pull request — holding it with `loom:blocked` \
                 instead of re-claiming it again (#7972). Last failure: {reason}"
            );
            // The re-verification inside can veto the hold. An OPEN PR is
            // positive evidence this was never a PR-less loop at all, so drop
            // the whole tally rather than leaving a `held` record that would
            // keep the issue out of dispatch on a false premise. The other two
            // vetoes (already-closed, hermetic fixture) leave the record
            // standing — neither contradicts the tally.
            if self.apply_prless_hold_label(issue, consecutive, reason)
                == PrlessHoldOutcome::VetoedOpenPr
            {
                self.clear_prless_retry(issue);
            }
        } else {
            log::warn!(
                "sweep_registry: issue #{issue} released without a pull request ({consecutive}/{} \
                 consecutive) — next dispatch allowed in {}s (#7972). Failure: {reason}",
                cfg.threshold,
                delay.as_secs()
            );
            // The first PR-less release is plausibly a one-off; a REPEAT is the
            // thing the next claimer needs to know about, and commenting from
            // the second onward keeps the comment count bounded by the
            // threshold (2 per streak on default config) rather than by the
            // length of the loop.
            if consecutive >= 2 {
                self.post_prless_attempt_comment(issue, consecutive, cfg.threshold, reason);
            }
        }
    }

    /// Clear `issue`'s PR-less-retry record (Issue #7972). Returns `true` when a
    /// record existed.
    ///
    /// Called on positive evidence the loop is broken: a dispatch that left an
    /// open PR or reached the merge phase (see
    /// [`Self::note_prless_terminal_outcome`]), a run that advanced its
    /// checkpoint *and* produced a PR, or a self-reported no-op release
    /// ([`Self::record_noop_release`] — a deliberate "nothing to do this pass"
    /// conclusion is not a failed attempt, and #6670's own cooldown is the
    /// right brake for it).
    pub(crate) fn clear_prless_retry(&mut self, issue: u32) -> bool {
        self.prless_retry.remove(&issue).is_some()
    }

    /// Remaining PR-less-retry window for `issue` at `now` (Issue #7972), or
    /// `None` when it may be dispatched immediately. `Some(Duration::ZERO)` is
    /// never returned — an elapsed window reads as `None`. Mirrors
    /// [`Self::noop_cooldown_remaining`].
    #[must_use]
    pub fn prless_retry_remaining(&self, issue: u32, now: DateTime<Utc>) -> Option<Duration> {
        if !self.prless_retry_config.enabled {
            return None;
        }
        let state = self.prless_retry.get(&issue)?;
        let remaining = state.until - now;
        if remaining <= chrono::Duration::zero() {
            return None;
        }
        remaining.to_std().ok().filter(|d| !d.is_zero())
    }

    /// Consecutive PR-less releases recorded for `issue` (Issue #7972). `0` when
    /// none is on record. Test/inspection helper, mirroring
    /// [`Self::noop_release_count`].
    #[must_use]
    pub fn prless_release_count(&self, issue: u32) -> u32 {
        self.prless_retry.get(&issue).map_or(0, |s| s.consecutive)
    }

    /// Whether `issue` reached the threshold and was held (Issue #7972).
    #[must_use]
    pub fn prless_retry_held(&self, issue: u32) -> bool {
        self.prless_retry.get(&issue).is_some_and(|s| s.held)
    }

    /// The failure reason recorded with `issue`'s most recent PR-less release
    /// (Issue #7972), or `None` when none is on record.
    #[must_use]
    pub fn prless_retry_reason(&self, issue: u32) -> Option<String> {
        self.prless_retry.get(&issue).map(|s| s.reason.clone())
    }

    /// Every issue inside a live PR-less-retry window at `now` (Issue #7972) —
    /// the set the work finder skips *before* the capacity gate, mirroring
    /// [`Self::noop_cooldown_issues`] / [`Self::decline_cooldown_issues`].
    ///
    /// Not unioned with a peer-observed view (unlike `noop_cooldown_issues`,
    /// #7477): once an issue is held, `loom:blocked` is public forge state every
    /// host reads for itself in its own candidate filter, and below the
    /// threshold each host's own tally is the honest record of what *it*
    /// dispatched. Fleet-wide broadcast of the sub-threshold window is left to a
    /// follow-up rather than guessed at here.
    #[must_use]
    pub fn prless_retry_issues(&self, now: DateTime<Utc>) -> HashSet<u32> {
        if !self.prless_retry_config.enabled {
            return HashSet::new();
        }
        self.prless_retry
            .iter()
            .filter(|(_, s)| s.until > now)
            .map(|(issue, _)| *issue)
            .collect()
    }

    /// Best-effort comment naming the failure behind a sub-threshold PR-less
    /// release (Issue #7972 AC3) — so the next claimer, human or agent, starts
    /// from "the last attempt died like *this*" instead of from nothing.
    ///
    /// Skipped entirely when label flips are disabled (test fixtures /
    /// `skip_label_flip`). Best-effort: a `gh` failure is logged at debug and
    /// never affects the load-bearing in-memory tally.
    fn post_prless_attempt_comment(
        &self,
        issue: u32,
        consecutive: u32,
        threshold: u32,
        reason: &str,
    ) {
        let body = format!(
            "{PRLESS_RETRY_COMMENT_MARKER}\n\
             **Attempt {consecutive} of {threshold} ended without a pull request.** This issue's \
             `loom:building` claim has now been taken and released {consecutive} times in a row \
             with no PR to show for it.\n\n\
             Last failure: {reason}\n\n\
             Re-dispatch is spaced out while this streak continues; after {threshold} consecutive \
             PR-less releases the issue is held with `loom:blocked` rather than re-claimed again \
             (Issue #7972). If you are the next claimer, read the failure above before repeating \
             it — if the issue's scope no longer exists on `main`, say so and rescope or close it \
             rather than retrying."
        );
        self.post_prless_comment(issue, &body, "attempt note");
    }

    /// Best-effort forge mutation on a PR-less hold (Issue #7972): add
    /// `loom:blocked`, remove `loom:issue`, and post a comment naming the
    /// failure and the count — so the pause is visible to a human on the forge,
    /// not just in the daemon log. Modeled directly on
    /// [`Self::apply_quarantine_label`].
    ///
    /// `loom:blocked` (not `loom:operator`) on purpose: `loom:blocked` is the
    /// established automated-hold state that the work finder's skip-label filter
    /// and `quarantine_reconciliation` already understand, and #7972's own
    /// acceptance criterion names it first. Skipped entirely when label flips
    /// are disabled; every step is best-effort.
    fn apply_prless_hold_label(
        &self,
        issue: u32,
        consecutive: u32,
        reason: &str,
    ) -> PrlessHoldOutcome {
        if self.config.skip_label_flip {
            return PrlessHoldOutcome::VetoedNoForge;
        }
        // A closed issue (or a PR number that slipped through) needs no hold:
        // it is already out of the candidate pool, and parking it would add a
        // `loom:blocked` label and a comment to settled work. Fail-open — an
        // unverifiable read (`None`) proceeds with the hold, since a stranded
        // re-claim loop is the failure this exists to stop.
        if self.issue_is_closed_or_pr(issue) == Some(true) {
            log::info!(
                "sweep_registry: issue #{issue} reached {consecutive} consecutive PR-less \
                 releases but is already closed — recording the tally without applying a \
                 `loom:blocked` hold (#7972)"
            );
            return PrlessHoldOutcome::VetoedClosed;
        }
        // Last-chance re-verification, and the ONLY forge probe this mechanism
        // adds anywhere. `note_prless_terminal_outcome`'s classification is
        // deliberately forge-free (see its doc comment), which leaves one
        // residual false-positive shape: an issue whose open PR was never
        // sampled by any of these sweeps and whose #6788 memo had expired. A
        // hold is the one irreversible-ish step here (a `loom:blocked` label a
        // human has to clear), so it is worth exactly one closes-graph query
        // to rule that out. `Open` means the issue is waiting on Judge, not
        // stuck in a PR-less loop; `ProbeFailed`/`NoneOpen` proceed, because a
        // forge outage must not be able to strand the loop either.
        if let OpenPrProbe::Open(pr) = self.probe_open_linked_pr(issue) {
            log::info!(
                "sweep_registry: issue #{issue} reached {consecutive} consecutive PR-less \
                 releases, but re-verification found open linked PR #{pr} — NOT holding; the \
                 issue is waiting on review, not looping (#7972)"
            );
            return PrlessHoldOutcome::VetoedOpenPr;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut edit = Command::new(&gh);
        edit.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--add-label")
            .arg("loom:blocked")
            .arg("--remove-label")
            .arg("loom:issue");
        edit.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut edit,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            edit.arg("--repo").arg(repo);
        }
        // Bounded (Issue #3973): this runs from `reap_once`, which is on the
        // `ListSweeps` / `GetSweepStatus` read path.
        let timeout = reap_gh_timeout();
        match output_with_timeout(edit, timeout) {
            Ok(Some(_)) => {}
            Ok(None) => log::warn!(
                "sweep_registry: PR-less hold label edit for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => {
                log::warn!("sweep_registry: PR-less hold label edit for #{issue} failed: {e}");
            }
        }

        let body = format!(
            "{PRLESS_RETRY_COMMENT_MARKER}\n\
             **Held after {consecutive} consecutive claims that produced no pull request.** This \
             issue's `loom:building` claim was taken and released {consecutive} times in a row \
             without a PR and without a `.no-changes-needed` marker — the dispatch loop is \
             retrying something that keeps failing the same way, so it is now held with \
             `loom:blocked` instead of being re-claimed a {next} time (Issue #7972).\n\n\
             Last failure: {reason}\n\n\
             **What to do**: read the failure above and fix its cause — the common ones are a \
             scope that no longer exists on `main` (rescope or close the issue), a build/test \
             failure the Builder cannot get past, and a missing dependency or credential. Then \
             flip `loom:blocked` back to `loom:issue` to return the issue to the queue; the tally \
             resets, so it gets a full runway of {consecutive} fresh attempts. Nothing here says \
             the issue is invalid — only that repeating the same dispatch unchanged will not \
             produce a different result.",
            next = consecutive + 1,
        );
        self.post_prless_comment(issue, &body, "hold notice");
        PrlessHoldOutcome::Applied
    }

    /// Shared best-effort `gh issue comment` behind
    /// [`Self::post_prless_attempt_comment`] and
    /// [`Self::apply_prless_hold_label`] (Issue #7972) — identical transport,
    /// timeout, and credential handling; only the body and the log label differ.
    fn post_prless_comment(&self, issue: u32, body: &str, what: &str) {
        if self.config.skip_label_flip {
            return;
        }
        let gh = self
            .config
            .gh_bin
            .clone()
            .unwrap_or_else(|| PathBuf::from("gh"));
        let mut comment = Command::new(&gh);
        comment
            .arg("issue")
            .arg("comment")
            .arg(issue.to_string())
            .arg("--body")
            .arg(body);
        comment.current_dir(&self.config.workspace_root);
        // #5401: cross-owner managed repo -> its own owner's installation-token
        // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
        crate::credential_preflight::apply_gh_config_for_root(
            &mut comment,
            &self.config.workspace_root,
        );
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            comment.arg("--repo").arg(repo);
        }
        let timeout = reap_gh_timeout();
        match output_with_timeout(comment, timeout) {
            Ok(Some(_)) => {}
            Ok(None) => log::debug!(
                "sweep_registry: PR-less retry {what} for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => {
                log::debug!("sweep_registry: PR-less retry {what} for #{issue} failed: {e}");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::{
        fake_gh_graphql_arm, fake_gh_timeline_rest_arm, state_probe_json,
    };
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// A registry with forge writes disabled — the tests below that use it
    /// drive the in-memory tally directly, which is the load-bearing half.
    /// The forge-visible half (the label flip and the comments) is covered
    /// separately, against a fake `gh`, by [`forge_registry`] (#8728).
    fn test_registry() -> SweepRegistry {
        let dir = tempdir().unwrap();
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.skip_label_flip = true;
        SweepRegistry::new(config)
    }

    /// A registry whose forge writes are **enabled** (`skip_label_flip =
    /// false`), driven by a fake `gh` that appends every invocation's argv to
    /// the returned log path (Issue #8728).
    ///
    /// Three answering arms. Their order in the script is **bash arm-matching
    /// precedence, not call order**: the #5911 REST timeline (`timeline_pr`,
    /// empty for "no open PR") is spliced first because its endpoint also
    /// matches the state probe's `repos/*` glob — the ordering note
    /// [`fake_gh_timeline_rest_arm`] carries — then the GraphQL closes-graph
    /// (`graphql_prs`, whitespace-separated open PR numbers), then the #4504
    /// issue-state probe (`issue_state`, `"open"` / `"closed"`). The *call*
    /// order is the reverse: the hold consults the state probe first and only
    /// then `probe_open_linked_pr`, which tries GraphQL and falls back to the
    /// REST timeline. `repo view` resolves the owner/repo the two `gh api`
    /// probes cannot infer from the working directory. Everything else — in
    /// particular the `issue edit` and `issue comment` mutations these tests
    /// exist to observe — is logged and exits 0.
    fn forge_registry(
        ws: &Path,
        issue_state: &str,
        graphql_prs: &str,
        timeline_pr: &str,
    ) -> (SweepRegistry, PathBuf) {
        let gh_log = ws.join("gh-invocations.log");
        let fake_gh = ws.join("fake-gh-prless.sh");
        let script = format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             {timeline}\
             {gql}\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
            log = gh_log.display(),
            timeline = fake_gh_timeline_rest_arm(timeline_pr, 0),
            gql = fake_gh_graphql_arm(graphql_prs, 0),
            state = state_probe_json(issue_state, false),
        );
        std::fs::write(&fake_gh, &script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_gh) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(ws.to_path_buf());
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        config.journal_path = Some(ws.join("test-sweeps-journal.json"));
        (SweepRegistry::new(config), gh_log)
    }

    /// Set `threshold` and drive `issue` straight to its hold with that many
    /// consecutive PR-less releases (#8728).
    fn hold_at_threshold(reg: &mut SweepRegistry, issue: u32, threshold: u32, reason: &str) {
        reg.set_prless_retry_config(PrlessRetryConfig {
            threshold,
            ..PrlessRetryConfig::default()
        });
        for _ in 0..threshold {
            reg.record_prless_release(issue, reason);
        }
    }

    /// Every logged `gh` invocation whose argv begins with `prefix` — the
    /// fake `gh` logs `"$*"`, so a multi-line `--body` spans several lines and
    /// only the first one carries the subcommand (#8728).
    fn gh_calls_starting_with(gh_log: &Path, prefix: &str) -> Vec<String> {
        std::fs::read_to_string(gh_log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with(prefix))
            .map(std::string::ToString::to_string)
            .collect()
    }

    #[test]
    fn a_single_prless_release_arms_a_window_and_does_not_hold() {
        let mut reg = test_registry();
        assert_eq!(reg.prless_release_count(7893), 0);
        assert!(reg.prless_retry_remaining(7893, Utc::now()).is_none());

        reg.record_prless_release(7893, "builder exited 78 without opening a PR");

        assert_eq!(reg.prless_release_count(7893), 1);
        assert!(!reg.prless_retry_held(7893));
        let remaining = reg.prless_retry_remaining(7893, Utc::now()).unwrap();
        assert_eq!(
            remaining.as_secs(),
            DEFAULT_PRLESS_RETRY_BACKOFF_SECS - 1,
            "the first window is the configured base backoff (minus the sub-second elapsed)"
        );
        assert!(reg.prless_retry_issues(Utc::now()).contains(&7893));
    }

    /// #7972 AC2: "consecutive re-claims of the same issue back off rather than
    /// retrying within the same minute" — the sharpest edge in the reported
    /// evidence was claim/release pairs landing in the SAME MINUTE.
    #[test]
    fn consecutive_releases_back_off_well_past_a_minute_and_grow() {
        let mut reg = test_registry();
        reg.record_prless_release(7893, "first");
        let first = reg.prless_retry_remaining(7893, Utc::now()).unwrap();
        reg.record_prless_release(7893, "second");
        let second = reg.prless_retry_remaining(7893, Utc::now()).unwrap();

        assert!(
            first.as_secs() > 60,
            "the very first re-claim must already be spaced past a minute, got {first:?}"
        );
        assert!(
            second > first,
            "consecutive releases must back off further, got {second:?} after {first:?}"
        );
    }

    /// #7972 AC1 + AC4, the headline test: a stubbed always-failing worker —
    /// every dispatch terminates with a verified "no open linked PR" — must be
    /// BOUNDED. After `threshold` consecutive PR-less releases the issue is
    /// held rather than offered for another claim.
    #[test]
    fn an_always_failing_worker_is_bounded_at_the_threshold() {
        let mut reg = test_registry();
        let threshold = reg.prless_retry_config().threshold;
        assert!(threshold >= 2, "a threshold of 1 would make this vacuous");

        // The stub: N dispatches, each ending exactly the way the #7893 loop
        // did — a terminal outcome with no PR, no merge phase observed, and no
        // `.no-changes-needed` marker (so no noop cooldown is armed either).
        for attempt in 1..=threshold {
            assert!(
                !reg.prless_retry_held(7893),
                "must not hold before the threshold (attempt {attempt})"
            );
            reg.note_prless_terminal_outcome(
                7893,
                "sweep-issue-7893-stub",
                Some(OpenPrProbe::NoneOpen),
                "builder failed: scope `safehouse_chatops/` does not exist on main",
            );
            assert_eq!(reg.prless_release_count(7893), attempt);
        }

        assert!(
            reg.prless_retry_held(7893),
            "the {threshold}th consecutive PR-less release must hold the issue"
        );
        assert!(
            reg.prless_retry_issues(Utc::now()).contains(&7893),
            "a held issue must stay out of the work finder's candidate set"
        );
        // #7972 AC3: the reason from the previous attempt is retained and
        // surfaced, not discarded.
        assert_eq!(
            reg.prless_retry_reason(7893).as_deref(),
            Some("builder failed: scope `safehouse_chatops/` does not exist on main")
        );
    }

    /// The loop is bounded in *count*, so a fourteenth claim like the one in
    /// the incident cannot happen: by then the issue is held and its window is
    /// the ceiling, not the base.
    #[test]
    fn the_observed_fourteen_claim_loop_cannot_recur() {
        let mut reg = test_registry();
        for _ in 0..14 {
            reg.note_prless_terminal_outcome(
                7893,
                "sweep-stub",
                Some(OpenPrProbe::NoneOpen),
                "no PR",
            );
        }
        assert!(reg.prless_retry_held(7893));
        let remaining = reg.prless_retry_remaining(7893, Utc::now()).unwrap();
        assert_eq!(
            remaining.as_secs(),
            reg.prless_retry_config().max_backoff.as_secs() - 1,
            "a held issue rides the ceiling, not the ladder"
        );
    }

    /// The crashed-branch entry point must classify WITHOUT a forge probe (it
    /// is called for pre-Builder checkpoint phases too, where `reap_once`
    /// deliberately issues no closes-graph query — see
    /// `reaper_does_not_resume_pre_builder_checkpoint`). With no sampled PR
    /// number and no fresh memo, the honest verdict is "no PR", and the reason
    /// it records names the failure the way the next claimer needs to read it.
    #[test]
    fn a_crash_with_no_sampled_pr_records_a_release_without_probing() {
        let mut reg = test_registry();

        reg.note_prless_crash_outcome(7893, "sweep-stub", Some(1), 2510, Some("builder"));

        assert_eq!(reg.prless_release_count(7893), 1);
        let reason = reg.prless_retry_reason(7893).unwrap();
        assert!(reason.contains("at phase `builder`"), "names the phase: {reason}");
        assert!(reason.contains("2510s"), "names the duration: {reason}");
        assert!(reason.contains("exit 1"), "names the exit status: {reason}");
        assert!(reason.contains("without opening a pull request"), "names the shape: {reason}");
    }

    /// The checkpoint-less entry point threads the caller's already-computed
    /// verdict straight through, so a clean exit that the #8355 memo seed
    /// proved productive never charges the tally.
    #[test]
    fn the_exit_entry_point_threads_the_callers_verdict_through() {
        let mut reg = test_registry();

        reg.note_prless_exit_outcome(
            7893,
            "sweep-stub",
            Some(OpenPrProbe::Open(8123)),
            Some(0),
            90,
        );
        assert_eq!(reg.prless_release_count(7893), 0);

        reg.note_prless_exit_outcome(7893, "sweep-stub", Some(OpenPrProbe::NoneOpen), Some(78), 41);
        let reason = reg.prless_retry_reason(7893).unwrap();
        assert!(reason.contains("exited 78"), "names the exit status: {reason}");
        assert!(
            reason.contains("`.no-changes-needed`"),
            "distinguishes this from a self-reported no-op: {reason}"
        );
    }

    /// A verified open linked PR is the productive outcome this bound exists to
    /// distinguish from the loop — it must CLEAR the tally, not add to it. The
    /// #4123 open-PR self-skip lands here, and correctly so.
    #[test]
    fn an_open_linked_pr_clears_the_tally() {
        let mut reg = test_registry();
        reg.record_prless_release(7893, "first");
        reg.record_prless_release(7893, "second");
        assert_eq!(reg.prless_release_count(7893), 2);

        reg.note_prless_terminal_outcome(7893, "sweep-stub", Some(OpenPrProbe::Open(8123)), "n/a");

        assert_eq!(reg.prless_release_count(7893), 0);
        assert!(reg.prless_retry_remaining(7893, Utc::now()).is_none());
    }

    /// Fail-open: an unverifiable probe must neither arm a window (a forge
    /// outage cannot manufacture a hold) nor clear an existing streak (a
    /// dispatch that proved nothing must not erase what earlier ones proved).
    #[test]
    fn an_unverifiable_probe_neither_arms_nor_clears() {
        let mut reg = test_registry();

        reg.note_prless_terminal_outcome(1, "sweep-stub", Some(OpenPrProbe::ProbeFailed), "x");
        assert_eq!(reg.prless_release_count(1), 0, "outage must not arm");

        reg.record_prless_release(1, "real failure");
        reg.note_prless_terminal_outcome(1, "sweep-stub", Some(OpenPrProbe::ProbeFailed), "x");
        assert_eq!(
            reg.prless_release_count(1),
            1,
            "outage must not clear an existing streak either"
        );
    }

    /// A streak older than the ceiling is cold: a failure an hour ago and a
    /// failure now are not a pattern, so the tally restarts at 1 rather than
    /// creeping toward a hold over days.
    #[test]
    fn a_cold_streak_restarts_at_one() {
        let mut reg = test_registry();
        reg.record_prless_release(42, "long ago");
        assert_eq!(reg.prless_release_count(42), 1);

        // Backdate the record past the ceiling.
        let ceiling = reg.prless_retry_config().max_backoff.as_secs();
        if let Some(state) = reg.prless_retry.get_mut(&42) {
            state.recorded_at = Utc::now() - chrono::Duration::seconds(ceiling as i64 + 1);
        }
        reg.record_prless_release(42, "much later");
        assert_eq!(reg.prless_release_count(42), 1, "the cold streak restarted");
    }

    #[test]
    fn the_window_expires_on_schedule() {
        let mut reg = test_registry();
        reg.record_prless_release(7, "boom");
        let past =
            Utc::now() + chrono::Duration::seconds(DEFAULT_PRLESS_RETRY_BACKOFF_SECS as i64 + 1);
        assert!(reg.prless_retry_remaining(7, past).is_none());
        assert!(!reg.prless_retry_issues(past).contains(&7));
    }

    #[test]
    fn clear_removes_the_record() {
        let mut reg = test_registry();
        reg.record_prless_release(55, "boom");
        assert!(reg.prless_retry_remaining(55, Utc::now()).is_some());
        assert!(reg.clear_prless_retry(55));
        assert!(reg.prless_retry_remaining(55, Utc::now()).is_none());
        assert!(!reg.clear_prless_retry(55), "second clear is a no-op");
    }

    #[test]
    fn disabled_mechanism_records_nothing() {
        let mut reg = test_registry();
        reg.set_prless_retry_config(PrlessRetryConfig {
            enabled: false,
            ..PrlessRetryConfig::default()
        });
        reg.record_prless_release(1, "boom");
        reg.note_prless_terminal_outcome(1, "s", Some(OpenPrProbe::NoneOpen), "boom");
        assert_eq!(reg.prless_release_count(1), 0);
        assert!(reg.prless_retry_remaining(1, Utc::now()).is_none());
        assert!(reg.prless_retry_issues(Utc::now()).is_empty());
        assert!(!reg.prless_retry_held(1));
    }

    /// #6670 regression: a Builder that self-reports "no changes needed" is
    /// making a deliberate conclusion, not failing. Its cooldown is the right
    /// brake; this tally must be cleared, so a standing/tracking issue with
    /// nothing to do never accretes toward a `loom:blocked` hold.
    #[test]
    fn a_self_reported_noop_release_clears_the_prless_tally() {
        let mut reg = test_registry();
        reg.record_prless_release(6670, "crashed");
        reg.record_prless_release(6670, "crashed again");
        assert_eq!(reg.prless_release_count(6670), 2);

        reg.record_noop_release(6670, Some("no actionable delta".into()));

        assert_eq!(
            reg.prless_release_count(6670),
            0,
            "a self-reported no-op is not a PR-less failure"
        );
        assert!(reg.noop_cooldown_remaining(6670, Utc::now()).is_some());
    }

    /// #7972 regression requirement: this is a THIRD, distinct bound. An issue
    /// can be quarantined, dispatch-backed-off, no-op-cooling-down and
    /// PR-less-held at once, and each is tracked and cleared independently.
    #[test]
    fn independent_of_quarantine_backoff_and_noop_cooldown() {
        let mut reg = test_registry();
        reg.seed_quarantine_for_test(321);
        reg.record_dispatch_failure(321);
        reg.record_prless_release(321, "boom");

        assert!(reg.is_quarantined(321));
        assert!(reg.dispatch_backoff_remaining(321, Utc::now()).is_some());
        assert!(reg.prless_retry_remaining(321, Utc::now()).is_some());

        // Clearing the PR-less record alone leaves the other two untouched.
        assert!(reg.clear_prless_retry(321));
        assert!(reg.is_quarantined(321));
        assert!(reg.dispatch_backoff_remaining(321, Utc::now()).is_some());
        assert!(reg.prless_retry_remaining(321, Utc::now()).is_none());
    }

    // --- The hold's forge path, against a fake `gh` (Issue #8728) ----------
    //
    // Every test above builds its registry with `skip_label_flip = true`, under
    // which `apply_prless_hold_label` returns `VetoedNoForge` on its first line
    // and never runs — so they pin only the in-memory `held` flag. These five
    // drive the same mechanism with flips ENABLED and a logging fake `gh`, so
    // the half a human actually sees on the forge (the label flip, the two
    // comments) and the two vetoes that suppress it are pinned too.
    //
    // All five are `#[serial]` and clear `LOOM_REPO`, matching
    // `dispatch_refuses_closed_issue_without_flipping_labels` — the argv these
    // tests assert on is process-global-env-dependent (`resolve_owner_repo`
    // reads `LOOM_REPO`, and a set value appends `--repo <slug>` to the hold's
    // edit), so pinning exact argv requires pinning that env.

    /// #8728 AC1: at the threshold the hold flips the forge labels — exactly
    /// one `gh issue edit`, naming the RIGHT issue, adding `loom:blocked` and
    /// removing `loom:issue`. The work finder's skip-label filter reads that
    /// label, not this process's memory, so the flip is the durable half of
    /// the hold.
    #[test]
    #[serial]
    fn the_hold_flips_loom_blocked_on_and_loom_issue_off_for_the_right_issue() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        // Open issue, no open linked PR on either transport: neither veto fires.
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "", "");

        hold_at_threshold(&mut reg, 7893, 2, "builder crashed without opening a PR");

        assert!(reg.prless_retry_held(7893), "the threshold must hold the issue");
        let edits = gh_calls_starting_with(&gh_log, "issue edit ");
        assert_eq!(edits.len(), 1, "exactly one label flip at the threshold, got: {edits:?}");
        assert_eq!(
            edits[0], "issue edit 7893 --add-label loom:blocked --remove-label loom:issue",
            "the hold's argv must name the held issue and flip both labels"
        );
    }

    /// #8728 AC2: the hold posts its notice to the issue, marked with
    /// [`PRLESS_RETRY_COMMENT_MARKER`] so it is machine-identifiable, and the
    /// body names both the count and the failure — the whole point of the
    /// comment is that the next reader does not have to rediscover why.
    #[test]
    #[serial]
    fn the_hold_notice_comment_is_posted_and_carries_the_marker() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "", "");

        hold_at_threshold(&mut reg, 7893, 2, "scope `safehouse_chatops/` does not exist on main");

        let comments = gh_calls_starting_with(&gh_log, "issue comment ");
        assert_eq!(comments.len(), 1, "the hold posts exactly one notice, got: {comments:?}");
        assert!(
            comments[0]
                .starts_with(&format!("issue comment 7893 --body {PRLESS_RETRY_COMMENT_MARKER}")),
            "the notice must go to the held issue and lead with the marker: {}",
            comments[0]
        );
        let body = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            body.contains("**Held after 2 consecutive claims that produced no pull request.**"),
            "the notice names the count: {body}"
        );
        assert!(
            body.contains("scope `safehouse_chatops/` does not exist on main"),
            "the notice names the failure: {body}"
        );
    }

    /// #8728 AC3: the last-chance re-verification is the one forge probe this
    /// mechanism adds, and an OPEN linked PR is positive evidence the tally was
    /// wrong — the issue is waiting on Judge, not looping. No label flip, no
    /// comment, and (via `record_prless_release`'s `VetoedOpenPr` arm) the
    /// whole tally is dropped rather than left standing on a false premise.
    #[test]
    #[serial]
    fn re_verification_finding_an_open_linked_pr_vetoes_the_hold_and_clears_the_tally() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        // The closes-graph answers with an open PR #8123.
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "8123", "");

        hold_at_threshold(&mut reg, 7893, 2, "looked PR-less from in-memory state");

        assert_eq!(
            reg.prless_release_count(7893),
            0,
            "an open linked PR is positive evidence the tally was wrong — drop it"
        );
        assert!(!reg.prless_retry_held(7893));
        assert!(
            reg.prless_retry_remaining(7893, Utc::now()).is_none(),
            "a cleared tally must not keep the issue out of dispatch"
        );
        assert!(
            gh_calls_starting_with(&gh_log, "issue edit ").is_empty(),
            "a vetoed hold must not flip any label"
        );
        assert!(
            gh_calls_starting_with(&gh_log, "issue comment ").is_empty(),
            "a vetoed hold must not post a hold notice either"
        );
    }

    /// #8728 AC4: a closed issue is already out of the candidate pool, so
    /// holding it would only add a `loom:blocked` label and a comment to
    /// settled work. The veto fires BEFORE the closes-graph re-verification —
    /// no point spending that query — and, unlike the open-PR veto, leaves the
    /// tally standing: being closed does not contradict the count.
    #[test]
    #[serial]
    fn the_closed_issue_veto_does_not_flip_labels() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry(dir.path(), "closed", "", "");

        hold_at_threshold(&mut reg, 7893, 2, "crashed without opening a PR");

        assert!(
            gh_calls_starting_with(&gh_log, "issue edit ").is_empty(),
            "a closed issue must not be labeled `loom:blocked`"
        );
        assert!(
            gh_calls_starting_with(&gh_log, "issue comment ").is_empty(),
            "a closed issue must not get a hold notice"
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls
                .lines()
                .any(|l| l.starts_with("api repos/") && l.contains("/issues/7893 --jq")),
            "the closed-issue state probe is what vetoed the hold: {calls}"
        );
        assert!(
            !calls.contains("api graphql"),
            "the closed veto short-circuits before the closes-graph re-verification: {calls}"
        );
        assert!(
            reg.prless_retry_held(7893),
            "a closed issue does not contradict the tally — the record stands"
        );
        assert_eq!(reg.prless_release_count(7893), 2);
    }

    /// #7972 AC3 on the forge side (#8728): the second consecutive PR-less
    /// release — one short of the default threshold — posts the attempt note,
    /// marked like the hold notice, and flips nothing. Commenting from the
    /// second onward is what bounds the comment count by the threshold rather
    /// than by the length of the loop.
    #[test]
    #[serial]
    fn the_second_consecutive_release_posts_a_marked_attempt_note_without_flipping_labels() {
        let dir = tempdir().unwrap();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = forge_registry(dir.path(), "open", "", "");
        assert_eq!(reg.prless_retry_config().threshold, DEFAULT_PRLESS_RETRY_THRESHOLD);

        reg.record_prless_release(7893, "first failure");
        assert!(
            gh_calls_starting_with(&gh_log, "issue comment ").is_empty(),
            "a single PR-less release is plausibly a one-off — no comment yet"
        );

        reg.record_prless_release(7893, "second failure: build error the Builder cannot pass");

        assert!(!reg.prless_retry_held(7893), "still one short of the threshold");
        let comments = gh_calls_starting_with(&gh_log, "issue comment ");
        assert_eq!(comments.len(), 1, "one attempt note, got: {comments:?}");
        assert!(
            comments[0]
                .starts_with(&format!("issue comment 7893 --body {PRLESS_RETRY_COMMENT_MARKER}")),
            "the attempt note must go to the right issue and carry the marker: {}",
            comments[0]
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap();
        assert!(
            calls.contains(&format!(
                "**Attempt 2 of {DEFAULT_PRLESS_RETRY_THRESHOLD} ended without a pull request.**"
            )),
            "the note names where in the runway this attempt sits: {calls}"
        );
        assert!(
            gh_calls_starting_with(&gh_log, "issue edit ").is_empty(),
            "below the threshold nothing is held, so no label may be flipped"
        );
    }

    // --- Config resolution precedence (env > config > default) -------------

    #[test]
    #[serial]
    fn resolve_uses_shipped_defaults_with_no_env_or_file() {
        let dir = tempdir().unwrap();
        for var in [
            PRLESS_RETRY_ENABLE_ENV,
            PRLESS_RETRY_THRESHOLD_ENV,
            PRLESS_RETRY_BACKOFF_SECS_ENV,
            PRLESS_RETRY_MAX_BACKOFF_SECS_ENV,
        ] {
            std::env::remove_var(var);
        }
        let cfg = resolve_prless_retry_config(dir.path());
        assert!(cfg.enabled);
        assert_eq!(cfg.threshold, DEFAULT_PRLESS_RETRY_THRESHOLD);
        assert_eq!(cfg.backoff.as_secs(), DEFAULT_PRLESS_RETRY_BACKOFF_SECS);
        assert_eq!(cfg.max_backoff.as_secs(), DEFAULT_PRLESS_RETRY_MAX_BACKOFF_SECS);
    }

    #[test]
    #[serial]
    fn resolve_file_config_overrides_default() {
        let dir = tempdir().unwrap();
        for var in [
            PRLESS_RETRY_ENABLE_ENV,
            PRLESS_RETRY_THRESHOLD_ENV,
            PRLESS_RETRY_BACKOFF_SECS_ENV,
            PRLESS_RETRY_MAX_BACKOFF_SECS_ENV,
        ] {
            std::env::remove_var(var);
        }
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"prlessRetry":{"enabled":false,"threshold":5,"backoffSecs":120,"maxBackoffSecs":480}}}}"#,
        )
        .unwrap();
        let cfg = resolve_prless_retry_config(dir.path());
        assert!(!cfg.enabled);
        assert_eq!(cfg.threshold, 5);
        assert_eq!(cfg.backoff.as_secs(), 120);
        assert_eq!(cfg.max_backoff.as_secs(), 480);
    }

    #[test]
    #[serial]
    fn resolve_env_overrides_file() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"prlessRetry":{"enabled":false,"threshold":5,"backoffSecs":120}}}}"#,
        )
        .unwrap();
        std::env::set_var(PRLESS_RETRY_ENABLE_ENV, "1");
        std::env::set_var(PRLESS_RETRY_THRESHOLD_ENV, "2");
        std::env::set_var(PRLESS_RETRY_BACKOFF_SECS_ENV, "42");
        let cfg = resolve_prless_retry_config(dir.path());
        for var in [
            PRLESS_RETRY_ENABLE_ENV,
            PRLESS_RETRY_THRESHOLD_ENV,
            PRLESS_RETRY_BACKOFF_SECS_ENV,
        ] {
            std::env::remove_var(var);
        }
        assert!(cfg.enabled);
        assert_eq!(cfg.threshold, 2);
        assert_eq!(cfg.backoff.as_secs(), 42);
    }

    #[test]
    #[serial]
    fn zero_or_invalid_env_values_fall_through() {
        let dir = tempdir().unwrap();
        std::env::set_var(PRLESS_RETRY_THRESHOLD_ENV, "0");
        std::env::set_var(PRLESS_RETRY_BACKOFF_SECS_ENV, "not-a-number");
        let cfg = resolve_prless_retry_config(dir.path());
        std::env::remove_var(PRLESS_RETRY_THRESHOLD_ENV);
        std::env::remove_var(PRLESS_RETRY_BACKOFF_SECS_ENV);
        assert_eq!(cfg.threshold, DEFAULT_PRLESS_RETRY_THRESHOLD);
        assert_eq!(cfg.backoff.as_secs(), DEFAULT_PRLESS_RETRY_BACKOFF_SECS);
    }

    /// A ceiling configured below the base would make the ladder start at the
    /// ceiling; clamp instead of silently inverting it.
    #[test]
    #[serial]
    fn a_ceiling_below_the_base_is_clamped_up() {
        let dir = tempdir().unwrap();
        std::env::remove_var(PRLESS_RETRY_ENABLE_ENV);
        std::env::set_var(PRLESS_RETRY_BACKOFF_SECS_ENV, "600");
        std::env::set_var(PRLESS_RETRY_MAX_BACKOFF_SECS_ENV, "60");
        let cfg = resolve_prless_retry_config(dir.path());
        std::env::remove_var(PRLESS_RETRY_BACKOFF_SECS_ENV);
        std::env::remove_var(PRLESS_RETRY_MAX_BACKOFF_SECS_ENV);
        assert_eq!(cfg.backoff.as_secs(), 600);
        assert_eq!(cfg.max_backoff.as_secs(), 600);
    }
}
