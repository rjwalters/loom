//! Idle-gating for the `#6157` peer-coordination health verdict (Issue #8026).
//!
//! # The false positive this closes
//!
//! [`super::PeerClaimView::evaluate_coordination`] flips `peer_coordination`
//! DEGRADED purely on **receive-quiet time**: once `grace` elapses with no
//! genuine inbound peer ad, the verdict reads *"no peer claim received in Ns
//! despite this host advertising — the receive path looks one-way or dead"*.
//!
//! The second half of that sentence was never checked, and advertising is
//! **entirely dispatch-gated**: the only two `ClaimKind::Advertise` publishers
//! in the daemon are `SweepRegistry::dispatch` (one ad per newly-dispatched
//! issue) and `SweepRegistry::readvertise_peer_claims`, which re-advertises
//! only entries in `SweepState::Running`/`Pending`. A host with zero live
//! sweeps at a given reaper tick sends **zero** ads that tick. There is no
//! periodic liveness heartbeat on this channel.
//!
//! Consequence: during a fleet-wide dispatch lull — nobody anywhere has work
//! in flight, so nobody's reaper has anything to re-advertise — no host
//! transmits, therefore no host receives, therefore **every** host's quiet
//! clock runs out at roughly the same time and the whole fleet reports
//! DEGRADED simultaneously. Nothing is broken; there is simply nothing to
//! hear. The check cannot distinguish that from a genuinely one-way receive
//! path, because both look identical from inside one process: `received`
//! stalls while `quiet_for` grows.
//!
//! # The rule
//!
//! **The quiet clock only runs while this host is itself advertising.** A host
//! that is saying nothing has no standing to conclude that the silence coming
//! back is anyone's fault — its own verdict's stated premise ("despite this
//! host advertising") is false. So:
//!
//! - *Actively advertising* — this host published an `Advertise` within
//!   [`resolve_advertise_activity_window`] (default
//!   [`DEFAULT_ADVERTISE_ACTIVITY_WINDOW`], 6× the 30s reaper cadence, so a few
//!   dropped ticks never read as idle). Evaluation proceeds **exactly as
//!   before**: same anchor, same grace, same recovery threshold.
//! - *Idle* — no ad within that window. The verdict is withheld and the quiet
//!   clock is rebased to `now`, so a host that later resumes dispatching gets a
//!   **full fresh grace window** to hear back from its peers rather than
//!   tripping DEGRADED on its first tick back (the stale-anchor edge case).
//!
//! # Why this is the right trade for a diagnostic-only check
//!
//! Since #6317 this verdict no longer gates stale-claim reclamation (#6286's
//! lease record is the sole gate), so a false DEGRADED costs noisy auto-filed
//! watchdog issues, not a safety gap — and the observed record is all noise:
//! #7850, #7927, #8276, #8303 were every recorded episode since the one true
//! positive (2026-08-13, `received=0` across 2510 ads sustained ~21h), and
//! #8276 already spent the cheap "raise the grace" lever (600s → 1200s)
//! without a clean measurement to justify a number.
//!
//! The cost of the gate is one detection loss: a genuine receive-path break on
//! a host that happens to be idle is not reported **while it is idle**. That is
//! the correct trade — an idle host is coordinating with nobody, so a broken
//! receive path has no live consequence at that moment, and the instant it
//! dispatches again the gate opens and the break is caught within one grace
//! window. The 2026-08-13 signature (a host advertising continuously into
//! silence) is detected byte-for-byte as before.
//!
//! # The residual tail (known, bounded)
//!
//! The window is not zero, so judgment continues for up to one window after the
//! last ad. A lull that begins at the exact moment the quiet clock is already
//! within one window of blowing can therefore still fire once before the gate
//! closes. The window cannot be shrunk to zero to remove it: a daemon managing
//! several repos (#3928) shares one view across registries and runs a
//! `readvertise → evaluate` pair per registry per tick, so an idle repo's
//! registry legitimately evaluates against an ad published a full tick earlier.
//! Six reaper cadences is the tolerance that keeps that ordering (and a few
//! dropped heartbeats) out of the idle bucket; the tail is the price.
//!
//! # What this deliberately does NOT do
//!
//! It does not make the verdict sound for the *converse* case — this host busy
//! while its **peers** are idle, which #8276's data shows was that host's shape
//! (150+ dispatches per data point throughout its own "degraded" window). No
//! local signal can separate "peers are quiet because idle" from "peers are
//! quiet because my receive path is broken"; only a periodic liveness
//! heartbeat from idle hosts can, and that is a wire-protocol change tracked
//! separately in **#8736** rather than smuggled into this fix.

use std::time::{Duration, Instant};

/// Env var overriding how recently this host must have published an
/// `Advertise` for its receive-quiet verdict to be considered meaningful, in
/// whole seconds (Issue #8026). A zero/unparseable value falls through to
/// [`DEFAULT_ADVERTISE_ACTIVITY_WINDOW`].
///
/// Raise this on a host running a deliberately slow reaper
/// (`LOOM_REAPER_INTERVAL_SECS`), whose re-advertisement heartbeat would
/// otherwise read as idleness between ticks.
pub const ADVERTISE_ACTIVITY_WINDOW_ENV: &str =
    "LOOM_PEER_COORDINATION_ADVERTISE_ACTIVITY_WINDOW_SECS";

/// Default advertise-activity window: 3 minutes — 6× the default reaper
/// re-advertisement cadence
/// ([`crate::sweep_registry::reaper::DEFAULT_REAPER_INTERVAL_SECS`], 30s), so a
/// handful of consecutive dropped heartbeats never mislabels a busy host as
/// idle, while still being far (6.7×) below the 1200s degrade grace — the gate
/// must resolve well inside the window it guards, or it would only ever delay
/// the verdict rather than withhold it.
pub const DEFAULT_ADVERTISE_ACTIVITY_WINDOW: Duration = Duration::from_secs(180);

/// Resolve the advertise-activity window: env override, else
/// [`DEFAULT_ADVERTISE_ACTIVITY_WINDOW`]. Mirrors
/// [`super::resolve_coordination_degrade_grace`]'s precedence exactly
/// (**env > default**; there is no config key for this — it is a diagnostic
/// windowing knob, not an operational one).
#[must_use]
pub fn resolve_advertise_activity_window() -> Duration {
    std::env::var(ADVERTISE_ACTIVITY_WINDOW_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_ADVERTISE_ACTIVITY_WINDOW)
}

/// Whether this host counts as **actively advertising** at local time `now` —
/// i.e. whether its receive-quiet clock has any standing to run.
///
/// `last_advertised_at` is `None` on a host that has never advertised, which is
/// already handled upstream by `evaluate_coordination`'s "nothing to judge yet"
/// early return; treating it as idle here keeps the helper total rather than
/// relying on that ordering.
#[must_use]
pub(crate) fn is_advertising_actively(
    last_advertised_at: Option<Instant>,
    now: Instant,
    window: Duration,
) -> bool {
    last_advertised_at.is_some_and(|at| now.saturating_duration_since(at) < window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_that_never_advertised_is_not_actively_advertising() {
        assert!(!is_advertising_actively(None, Instant::now(), Duration::from_secs(180)));
    }

    #[test]
    fn an_ad_inside_the_window_counts_as_active_and_one_outside_does_not() {
        let base = Instant::now();
        let window = Duration::from_secs(180);
        // Just inside.
        assert!(is_advertising_actively(Some(base), base + Duration::from_secs(179), window));
        // Exactly at the boundary is already idle (matches the `>= grace`
        // convention `evaluate_coordination` uses for its own boundary).
        assert!(!is_advertising_actively(Some(base), base + Duration::from_secs(180), window));
        // Well outside.
        assert!(!is_advertising_actively(Some(base), base + Duration::from_secs(3600), window));
    }

    #[test]
    fn the_default_window_resolves_well_inside_the_default_degrade_grace() {
        // The gate must resolve inside the window it guards — otherwise it
        // would delay the verdict rather than withhold it.
        assert!(
            DEFAULT_ADVERTISE_ACTIVITY_WINDOW < super::super::DEFAULT_COORDINATION_DEGRADE_GRACE
        );
    }
}
