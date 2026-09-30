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
//!    instead of rediscovering it. This note is **not** a hold and applies no
//!    label; it carries [`PRLESS_ATTEMPT_COMMENT_MARKER`] so nothing has to
//!    infer that from prose (#9239).
//! 4. On the [`PrlessRetryConfig::threshold`]-th consecutive PR-less release the
//!    issue is **held**: `loom:blocked` on, `loom:issue` off, plus a comment
//!    naming the failure and the count. The work finder's own skip-label filter
//!    takes it from there, and the hold is released the way every other
//!    `loom:blocked` park is — by a human or a Doctor who fixed the cause.
//!
//! # The tally is fleet-wide, not per-host (Issue #9292)
//!
//! Steps 2–4 all read **one** number: how many consecutive claims of this issue
//! ended without a pull request. Until #9292 that number lived in a single
//! process's `HashMap`, so each dispatch host counted only the attempts *it*
//! made — the #7477 gap, one mechanism later. A four-host fleet therefore spent
//! up to `4 × threshold` claim/release cycles before any one host's private
//! integer reached the threshold, and posted up to four near-identical attempt
//! notes on the way there. `rjwalters/loom#8812`, 2026-09-24: nine cycles at
//! ~90 s apart, four `Attempt 2 of 3` notes (one per host), one hold.
//!
//! So each recorded release is now broadcast over the peer-claim room's brake
//! lane ([`crate::peer_claims::ClaimKind::PrlessReleaseArmed`]) carrying this
//! host's own running count, and the threshold is compared against the **sum**:
//! local count + every peer's live broadcast count. The attempt note is posted
//! when that sum first reaches 2 — once per streak per *issue*, wherever in the
//! fleet it lands — instead of once per streak per host.
//!
//! Every part of this is fail-open in the same direction as the rest of the
//! module. No peer coordination (`safehouse.enabled` false), a dropped ad, a
//! pre-#9292 peer, or a poisoned view all reduce the peer term to what could be
//! read, leaving this host's own exact tally behind — which is the pre-#9292
//! behaviour.
//!
//! A peer's contribution lapses on [`crate::peer_claims::PEER_PRLESS_STREAK_TTL`]
//! — the fleet-wide form of the local streak-cold rule, so a crashed host stops
//! contributing rather than pinning an issue one release short of a hold
//! forever. That clock is deliberately **not** the broadcast backoff window
//! (which suppresses dispatch and is capped by
//! [`crate::peer_claims::MAX_PEER_PRLESS_RELEASE_TTL`]): the window's whole
//! purpose is to elapse before the next claim, so a tally keyed to it would be
//! empty every time the fleet's next release landed, and the sum could never
//! grow past one. A streak is a longer-lived thing than a backoff and gets the
//! longer clock.
//!
//! The lane is **arm-only**, mirroring #7477's cooldown lane rather than #8001's
//! arm/clear pool lane: there is no "my streak was cleared" ad. A clear happens
//! on positive evidence the loop broke (an open PR, an observed merge, a
//! self-reported no-op), and the first two of those are public forge state every
//! host reads for itself. The residual — a peer's hour-long entry still counting
//! releases that a later open PR retroactively excused — is caught where it
//! would matter, immediately before the label write: `apply_prless_hold_label`
//! spends one closes-graph query and returns
//! [`PrlessHoldOutcome::VetoedOpenPr`], so the worst case is a vetoed hold
//! attempt rather than a park on a false premise. Paying for a clear ad to
//! shorten a window that is already guarded, and self-heals within the hour, is
//! not worth a second wire kind.
//!
//! # The label write is the hold (Issue #9239)
//!
//! Step 4 has one deliverable and it is not the comment. `loom:blocked` is what
//! `PARK_LABELS` consults and therefore what actually removes the issue from
//! dispatch; the notice only explains a park that has already happened. The
//! first implementation wrote the label fire-and-forget — `output_with_timeout`
//! returns `Ok(Some(output))` for a child that *completed*, at any exit status,
//! and the match arm was `Ok(Some(_)) => {}` — and then posted the notice
//! unconditionally. A rejected write therefore left behind a comment asserting
//! a park that had not occurred, on an issue dispatch immediately re-claimed:
//! `2AMLogic/sky130-sar-adc#121` collected **seven** hold notices across four
//! days and two bot accounts with **zero** `loom:blocked` label events.
//!
//! So [`SweepRegistry::write_prless_hold_label`] now checks the exit status,
//! retries the add half alone when the combined flip is rejected, reads the
//! label back before giving up, and returns
//! [`PrlessHoldOutcome::LabelWriteFailed`] when the park cannot be confirmed —
//! in which case no "held" notice is posted, the tally does **not** record the
//! issue as held, and the failure is raised both in the log and (once per
//! streak) on the issue itself.
//!
//! One measurement note, since it is what #9239 was originally filed on: the
//! attempt notes of step 3 and the hold notice of step 4 shared a single
//! comment marker, so an audit grepping `prless-retry` counted both as holds.
//! On `rjwalters/loom#8812` four of five "holds" were `Attempt 2 of 3` notes —
//! one per dispatch host, since the tally is per-host — and the one real hold
//! labelled the issue in the same second it commented. The kind markers exist
//! so that reading is no longer available to anyone.
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
//!   failure; [`SweepRegistry::record_noop_release`] clears this tally, **and**
//!   (Issue #8912) the reaper excludes that dispatch's terminal outcome at both
//!   call sites, so the classification it just cleared is not re-armed a minute
//!   later when the process is reaped,
//! - a superseded claim — a newer sweep owns the issue.
//!
//! Each of those is excluded at the call site (or, for the no-op release, by an
//! explicit clear), never by this module second-guessing the classification.

use super::*;

/// The forge-visible half of the hold — the `loom:blocked` write and the
/// comments (Issues #7972, #9239). Split out as a child module both because
/// the two halves answer different questions and because #9239 showed what
/// happens when they drift: see [`hold`]'s own header.
mod hold;
#[allow(unused_imports)]
pub use hold::*;

/// The multi-host half of this module's coverage (Issue #9292) — see its own
/// header. A sibling file because the fleet harness is most of its bulk.
#[cfg(test)]
mod fleet_tests;

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
    /// The **fleet-wide** total at the moment this release was recorded
    /// (Issue #9292): [`Self::consecutive`] plus every peer host's live
    /// broadcast tally for the same issue. This — not `consecutive` — is what
    /// the threshold, the backoff ladder step, and the attempt note are
    /// computed from, so four dispatch hosts spend `threshold` claims between
    /// them rather than `threshold` claims each.
    ///
    /// Equal to `consecutive` on a fleet of one, and whenever peer-claim
    /// coordination is not wired up (`safehouse.enabled` false) — which is
    /// what makes the single-host path byte-for-byte pre-#9292.
    pub(crate) fleet_consecutive: u32,
    /// The failure context supplied by the recording caller — logged verbatim
    /// and posted to the issue, so the next claimer sees it.
    pub(crate) reason: String,
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
        // Issue #8912: a dispatch that self-reported a no-op release
        // (`RecordNoopRelease`) had this tally CLEARED by
        // `record_noop_release` at IPC time, on purpose — a deliberate
        // "nothing to do this pass" conclusion is not a failed attempt, and
        // #6670's cooldown is its brake. The reaper then classifies the same
        // dispatch's terminal outcome minutes later, and without this check it
        // re-armed here exactly what was just cleared, with a window an order
        // of magnitude shorter (observed on `2AMLogic/llm-cim#1`: a 3600s
        // no-op cooldown and a 300s PR-less window armed 59s apart for one
        // release). Scoped to the reporting sweep id, so a later, genuinely
        // PR-less dispatch of the same issue still counts.
        if self.noop_release_covers_dispatch(issue, sweep_id) {
            log::info!(
                "sweep_registry: issue #{issue} self-reported a no-op release for sweep \
                 {sweep_id} — not counting that outcome as a PR-less release (#8912)"
            );
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
        // #8912: this used to claim the sweep exited "without a
        // `.no-changes-needed` marker" — a hardcoded literal, not the result of
        // any filesystem probe, and nothing in this path ever looked for that
        // file. It read as evidence while asserting an unperformed check, and
        // it fired even on releases the orchestrator HAD self-reported as
        // no-ops. The marker's daemon-visible consequence is the
        // `RecordNoopRelease` call `record-noop-release.sh` makes from it, and
        // *that* is now genuinely checked: `note_prless_terminal_outcome`
        // below returns without recording anything when the dispatch
        // self-reported a no-op release (`noop_release_covers_dispatch`). So
        // the clause below is true whenever this reason string is ever read —
        // by construction, not by assertion.
        let reason = format!(
            "sweep {ended} without opening a pull request, without a phase checkpoint, and \
             without a self-reported no-op release (#6670)"
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
    /// # Fleet-wide as of Issue #9292
    ///
    /// The threshold, the ladder step, and the attempt note are computed from
    /// the **fleet** total — this host's own consecutive count plus every peer
    /// host's live broadcast tally for the same issue — and the release is
    /// re-broadcast so peers can do the same. Before #9292 each host counted
    /// only its own releases, so a four-host fleet spent up to
    /// `4 × threshold` claim/release cycles, and posted up to four
    /// near-identical attempt notes, before any one host reached the
    /// threshold. On a single host, or with peer coordination disabled, the
    /// fleet total is exactly the local count and this path is unchanged.
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
        // #9292: the peer term. Each peer reports only its OWN running total
        // and this host's own ad is never folded back into its view, so the
        // sum is disjoint from `consecutive` above. Zero without peer
        // coordination, which is what keeps the single-host path identical.
        let peers = self.peer_prless_release_count(issue);
        let fleet_consecutive = consecutive.saturating_add(peers);
        let held = fleet_consecutive >= cfg.threshold;
        // A hold's in-memory half rides the ceiling rather than the ladder:
        // past the threshold the ladder has nothing left to say, and the
        // durable half of the hold is the `loom:blocked` label the work
        // finder's skip-label filter already honors.
        let delay = if held {
            cfg.max_backoff
        } else {
            backoff_delay(fleet_consecutive, cfg.backoff, cfg.max_backoff)
        };
        let until =
            now + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero());
        self.prless_retry.insert(
            issue,
            PrlessRetryState {
                recorded_at: now,
                until,
                consecutive,
                fleet_consecutive,
                held,
                reason: reason.to_string(),
            },
        );
        // Broadcast BEFORE the hold/comment branch below, so a peer that is
        // classifying the same issue concurrently sees this release even if
        // the forge work that follows is slow or wedged. Fail-open: a dropped
        // ad costs at most one extra spaced-out dispatch somewhere in the
        // fleet, never correctness — the local tally above is already exact.
        self.publish_peer_prless_release_claim(issue, consecutive, delay);
        self.warn_if_prless_tally_is_host_local(issue, consecutive);
        if held {
            log::warn!(
                "sweep_registry: issue #{issue} has now been claimed and released \
                 {fleet_consecutive} times in a row fleet-wide ({consecutive} on this host, \
                 {peers} broadcast by peers) without producing a pull request — holding it with \
                 `loom:blocked` instead of re-claiming it again (#7972, #9292). Last failure: \
                 {reason}"
            );
            // The re-verification inside can veto the hold. An OPEN PR is
            // positive evidence this was never a PR-less loop at all, so drop
            // the whole tally rather than leaving a `held` record that would
            // keep the issue out of dispatch on a false premise. The other two
            // vetoes (already-closed, hermetic fixture) leave the record
            // standing — neither contradicts the tally.
            match self.apply_prless_hold_label(issue, fleet_consecutive, reason) {
                PrlessHoldOutcome::VetoedOpenPr => {
                    self.clear_prless_retry(issue);
                }
                // The park did not happen (Issue #9239). `held` is the claim
                // "this issue is out of the pool", and the only thing that
                // makes it true is `loom:blocked` on the forge — so record the
                // truth instead, and let the next PR-less release re-attempt
                // the hold. The backoff window stays armed either way, so this
                // costs at most one further spaced-out dispatch rather than
                // the unbounded re-claim loop a false `held` produced.
                PrlessHoldOutcome::LabelWriteFailed => {
                    if let Some(state) = self.prless_retry.get_mut(&issue) {
                        state.held = false;
                    }
                }
                PrlessHoldOutcome::Applied
                | PrlessHoldOutcome::VetoedClosed
                | PrlessHoldOutcome::VetoedNoForge => {}
            }
        } else {
            log::warn!(
                "sweep_registry: issue #{issue} released without a pull request \
                 ({fleet_consecutive}/{} consecutive fleet-wide; {consecutive} on this host) — \
                 next dispatch allowed in {}s (#7972, #9292). Failure: {reason}",
                cfg.threshold,
                delay.as_secs()
            );
            // The first PR-less release is plausibly a one-off; a REPEAT is the
            // thing the next claimer needs to know about.
            //
            // #9292: exactly `== 2`, and on the FLEET total rather than this
            // host's own. The old `consecutive >= 2` posted once per
            // sub-threshold release per host — four hosts × a threshold of 3
            // meant four near-identical "Attempt 2 of 3" notes on a single
            // issue (`rjwalters/loom#8812`), and a threshold of 5 would have
            // meant three notes per host. Because the fleet total is strictly
            // increasing across the streak, `== 2` is reached exactly once,
            // wherever in the fleet the second release happens to land: one
            // note per streak per ISSUE. (Two hosts recording simultaneously,
            // neither having yet seen the other's ad, can still both compute
            // 2 — the residual is bounded at one duplicate per race, against
            // the four-per-streak floor this replaces.)
            if fleet_consecutive == 2 {
                self.post_prless_attempt_comment(issue, fleet_consecutive, cfg.threshold, reason);
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

    /// The **fleet-wide** consecutive PR-less release total recorded for
    /// `issue` at its most recent release (Issue #9292) — this host's own
    /// count plus the peer tallies live at that moment. `0` when none is on
    /// record, and always equal to [`Self::prless_release_count`] on a single
    /// host or with peer coordination disabled.
    ///
    /// This, not [`Self::prless_release_count`], is the number compared
    /// against [`PrlessRetryConfig::threshold`].
    #[must_use]
    pub fn prless_fleet_release_count(&self, issue: u32) -> u32 {
        self.prless_retry
            .get(&issue)
            .map_or(0, |s| s.fleet_consecutive)
    }

    /// The sum of every **peer** host's live broadcast PR-less-release tally
    /// for `issue` (Issue #9292). `0` whenever peer-claim coordination is not
    /// wired up (`safehouse.enabled` false), which is what degrades this host
    /// byte-for-byte to the pre-#9292 per-host tally — mirroring
    /// [`Self::fleet_noop_cooldown_issues`]'s disabled-state contract.
    #[must_use]
    fn peer_prless_release_count(&self, issue: u32) -> u32 {
        let Some(view) = &self.peer_claims else {
            return 0;
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        let now = Instant::now();
        match view.lock() {
            Ok(v) => v.prless_peer_release_count_at(&repo, issue, now),
            Err(poisoned) => {
                log::error!("sweep_registry: peer-claim view mutex poisoned ({poisoned:?})");
                // Fail-open to the local tally: a poisoned view must not be
                // able to manufacture a hold OUT of a count nobody can read,
                // and must not stall the streak either.
                poisoned
                    .into_inner()
                    .prless_peer_release_count_at(&repo, issue, now)
            }
        }
    }

    /// Issues with a live peer-broadcast PR-less-release window (Issue #9292)
    /// — the fleet-wide half of the sub-threshold backoff
    /// [`Self::prless_retry_issues`]'s doc comment used to defer. Empty
    /// without a peer-claim view, mirroring
    /// [`Self::fleet_noop_cooldown_issues`].
    #[must_use]
    fn fleet_prless_retry_issues(&self) -> HashSet<u32> {
        let Some(view) = &self.peer_claims else {
            return HashSet::new();
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        match view.lock() {
            Ok(v) => v.prless_release_issues_at(&repo, Instant::now()),
            Err(poisoned) => {
                log::error!("sweep_registry: peer-claim view mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Broadcast one PR-less release fleet-wide (Issue #9292) — the
    /// [`Self::publish_peer_cooldown_claim`] sibling for
    /// [`peer_claims::ClaimKind::PrlessReleaseArmed`], which needs a parameter
    /// that method's signature has no room for: `consecutive`, **this host's
    /// own** running tally.
    ///
    /// Same outbound channel and fail-open contract as every other lane: a
    /// no-op without a publisher (`safehouse.enabled` false), and a full or
    /// closed channel drops the ad without blocking the reaper. One-shot per
    /// recorded release rather than re-advertised — each further release
    /// naturally re-broadcasts a higher count and refreshes every peer's local
    /// expiry, exactly mirroring the local re-arm.
    pub(crate) fn publish_peer_prless_release_claim(
        &self,
        issue: u32,
        consecutive: u32,
        remaining: Duration,
    ) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        let ad = ClaimAd::prless_release_armed(
            issue,
            peer_claims::repo_slug(&self.config.workspace_root),
            host_identity(),
            std::process::id(),
            Utc::now().to_rfc3339(),
            consecutive,
            remaining.as_secs(),
        );
        if let Err(e) = tx.try_send(ad) {
            log::debug!(
                "sweep_registry: PR-less release advertisement for issue #{issue} dropped ({e}); \
                 this host's own tally and window unaffected (#9292)"
            );
        }
    }

    /// Say out loud, at the moment it matters, that this host's PR-less tally
    /// is **not** fleet-wide (Issue #9292) — the `record_noop_release`
    /// HOST-LOCAL line #8912 added, for the same reason and in the same words.
    ///
    /// Without a peer-claim publisher (`safehouse.enabled` false on this host)
    /// the broadcast above is a byte-for-byte no-op, the peer term is always
    /// zero, and the fleet is back to spending `hosts × threshold` claims. The
    /// degradation is otherwise invisible: nothing else in the log
    /// distinguishes "third claim fleet-wide" from "third claim on this host".
    fn warn_if_prless_tally_is_host_local(&self, issue: u32, consecutive: u32) {
        if self.peer_claim_publisher.is_some() {
            return;
        }
        log::info!(
            "sweep_registry: issue #{issue}'s PR-less retry tally is HOST-LOCAL only \
             ({consecutive} on this host) — no peer-claim publisher is attached (safehouse peer \
             coordination disabled), so each dispatch host counts its own claims and the fleet \
             spends up to hosts × threshold of them before any hold (#9292)"
        );
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
    /// Fleet-wide as of Issue #9292: unions this host's own local windows with
    /// any live window a **peer** host has broadcast, exactly as
    /// [`Self::noop_cooldown_issues`] has since #7477. This is the follow-up
    /// the pre-#9292 version of this comment deferred — "once an issue is
    /// held, `loom:blocked` is public forge state every host reads for itself
    /// … fleet-wide broadcast of the sub-threshold window is left to a
    /// follow-up rather than guessed at here". The measured cost of leaving it
    /// deferred was that each host's private sub-threshold window spaced out
    /// only its own re-claims, so the fleet as a whole kept re-claiming a
    /// failing issue at roughly one host per window.
    #[must_use]
    pub fn prless_retry_issues(&self, now: DateTime<Utc>) -> HashSet<u32> {
        if !self.prless_retry_config.enabled {
            return HashSet::new();
        }
        let mut set: HashSet<u32> = self
            .prless_retry
            .iter()
            .filter(|(_, s)| s.until > now)
            .map(|(issue, _)| *issue)
            .collect();
        set.extend(self.fleet_prless_retry_issues());
        set
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use serial_test::serial;
    use tempfile::tempdir;

    /// A registry with forge writes disabled — the tests here drive the
    /// in-memory tally directly, which is the load-bearing half. The
    /// forge-visible half (the label flip and the comments) is covered
    /// separately, against a fake `gh`, in [`super::hold`]'s own test module
    /// (#8728, #9239).
    fn test_registry() -> SweepRegistry {
        let dir = tempdir().unwrap();
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.skip_label_flip = true;
        SweepRegistry::new(config)
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
        // #8912: the reason must describe a check that was ACTUALLY performed.
        // The old wording asserted a `.no-changes-needed` filesystem probe this
        // path never ran; the self-reported no-op release it stands in for is
        // now genuinely checked by the caller.
        assert!(
            reason.contains("without a self-reported no-op release"),
            "distinguishes this from a self-reported no-op: {reason}"
        );
        assert!(
            !reason.contains(".no-changes-needed"),
            "must not assert a marker check this path never performs: {reason}"
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

    /// #9292: every recorded release is broadcast, carrying THIS host's own
    /// running count (never a fleet total — see [`ClaimAd::consecutive`]) and
    /// the window it just armed, on the brake lane the socket layer already
    /// routes. The multi-host consequences are in [`super::fleet_tests`]; this
    /// pins the wire shape one host puts on the room.
    #[test]
    fn a_recorded_release_broadcasts_this_hosts_own_count_and_window() {
        let mut reg = test_registry();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        reg.set_peer_claim_publisher(tx);

        reg.record_prless_release(8812, "first");
        let ad = rx.try_recv().expect("the first release must be advertised");
        assert_eq!(ad.kind, crate::peer_claims::ClaimKind::PrlessReleaseArmed);
        assert_eq!(ad.issue, 8812);
        assert_eq!(ad.consecutive, Some(1));
        assert_eq!(ad.remaining_secs, Some(DEFAULT_PRLESS_RETRY_BACKOFF_SECS));
        assert!(ad.kind.is_cooldown_lane(), "routed by the existing brake-lane predicate");

        reg.record_prless_release(8812, "second");
        let ad = rx
            .try_recv()
            .expect("each further release re-advertises a higher count");
        assert_eq!(ad.consecutive, Some(2));
        assert_eq!(ad.remaining_secs, Some(DEFAULT_PRLESS_RETRY_BACKOFF_SECS * 2));
    }

    /// A disabled mechanism publishes nothing — mirrors
    /// `noop_cooldown`'s `disabled_mechanism_does_not_broadcast`. The #9292
    /// broadcast must not become a way for a repo that opted out of the bound
    /// to still push tallies at peers that did not.
    #[test]
    fn a_disabled_mechanism_does_not_broadcast() {
        let mut reg = test_registry();
        reg.set_prless_retry_config(PrlessRetryConfig {
            enabled: false,
            ..PrlessRetryConfig::default()
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        reg.set_peer_claim_publisher(tx);

        reg.record_prless_release(8812, "boom");

        assert!(rx.try_recv().is_err(), "a disabled mechanism must publish nothing");
    }

    /// With no peer view attached (`safehouse.enabled` false) the fleet count
    /// IS the local count, so every single-host assertion above holds
    /// unchanged — the #9292 acceptance criterion that the single-host case
    /// must not regress, stated as an invariant rather than left implicit.
    #[test]
    fn without_a_peer_view_the_fleet_count_equals_the_local_count() {
        let mut reg = test_registry();
        for expected in 1..=3u32 {
            reg.record_prless_release(7893, "no PR");
            assert_eq!(reg.prless_release_count(7893), expected);
            assert_eq!(reg.prless_fleet_release_count(7893), expected);
        }
        assert!(reg.prless_retry_held(7893));
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
