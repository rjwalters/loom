//! Sweep-reaper cadence knobs: the reaper polling interval and the per-call
//! `gh` timeout, each resolved with precedence **single-knob env var >
//! hyperparameters layer > built-in default**.
//!
//! Split out of the reaper parent module for the file-size-ratchet reason the
//! budget script enforces: that module sits at the 1000-code-line threshold,
//! so additions must land in a new sibling
//! (`.loom/docs/file-size-policy.md`; same move as
//! [`crate::work_finder::config`]). Everything here is re-exported from the
//! parent, so call sites are unchanged.
//!
//! The layer tier reads `hyperparameters.supervision.sweepReaperIntervalSecs`
//! / `hyperparameters.supervision.reapGhTimeoutSecs` through
//! [`crate::config_resolver::u64_from_layer_global`], which returns `None` before
//! daemon startup captures the root — so outside a running daemon this module
//! behaves exactly as env > default.

use std::time::Duration;

/// Default reaper polling interval in seconds. Matches
/// `defaults/scripts/spawn-loop.sh:110` `POLL_INTERVAL`.
pub const DEFAULT_REAPER_INTERVAL_SECS: u64 = 30;

/// Environment variable for overriding the reaper interval. Naming follows
/// the existing `LOOM_*` conventions in `main.rs` (e.g., `LOOM_CLAIM_TTL_SECS`,
/// `LOOM_WORKSPACE`, `LOOM_SOCKET_PATH`).
pub const REAPER_INTERVAL_ENV: &str = "LOOM_SWEEP_REAPER_INTERVAL_SECS";

/// Per-call ceiling for a best-effort `gh` subprocess invoked from the reaper
/// (Issue #3973).
///
/// The reaper's forge-label reconciliation (`restore_label_to_ready`,
/// `issue_has_blocked_label`, the quarantine label flips) runs on the
/// `ListSweeps` / `GetSweepStatus` **read path** via
/// `SweepRegistry::reap_liveness`.
/// During the 2026-07-26 incident a wedged `gh`/XPC blocked that read under the
/// registry mutex indefinitely, so an operator `list_sweeps` hung ~15 minutes.
/// Every reaper `gh` call is bounded to this window: on timeout the child is
/// killed and the call is treated as the same best-effort failure any other
/// `gh` error already is, so the in-memory liveness transition always completes.
/// Overridable via [`REAP_GH_TIMEOUT_ENV`] for operability.
pub(crate) const REAP_GH_TIMEOUT: Duration = Duration::from_secs(REAP_GH_TIMEOUT_SECS);

/// The same 5s budget, in whole seconds — the tranche-2 hyperparameter
/// default (`hyperparameters.supervision.reapGhTimeoutSecs`) sources this so
/// the two constants cannot drift.
pub(crate) const REAP_GH_TIMEOUT_SECS: u64 = 5;

/// Env var overriding [`REAP_GH_TIMEOUT`] (whole seconds; zero/invalid ignored).
pub const REAP_GH_TIMEOUT_ENV: &str = "LOOM_REAP_GH_TIMEOUT_SECS";

/// Resolve the per-call reaper `gh` timeout (Issue #3973) with precedence
/// **env ([`REAP_GH_TIMEOUT_ENV`], whole seconds, must be > 0) > hyperparameters
/// layer (`hyperparameters.supervision.reapGhTimeoutSecs`, startup-anchored) >
/// default ([`REAP_GH_TIMEOUT`])**. The env var sits above the layer.
pub(crate) fn reap_gh_timeout() -> Duration {
    reap_gh_timeout_with_layer(crate::config_resolver::u64_from_layer_global(
        "supervision",
        "reapGhTimeoutSecs",
    ))
}

/// [`reap_gh_timeout`] with the layer tier injected — the testable form (no
/// process-global read).
pub(crate) fn reap_gh_timeout_with_layer(layer: Option<u64>) -> Duration {
    Duration::from_secs(gh_timeout_secs_from(
        std::env::var(REAP_GH_TIMEOUT_ENV)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok()),
        layer,
    ))
}

/// Pure **env > layer > default** precedence over already-parsed tiers —
/// split out so tests exercise the precedence without touching process-global
/// env state. A zero/unparseable value at any tier falls through to the next.
fn gh_timeout_secs_from(env: Option<u64>, layer: Option<u64>) -> u64 {
    env.filter(|&n| n > 0)
        .or(layer)
        .filter(|&n| n > 0)
        .unwrap_or(REAP_GH_TIMEOUT_SECS)
}

/// Resolve the configured reaper interval with precedence **env
/// ([`REAPER_INTERVAL_ENV`]) > hyperparameters layer
/// (`hyperparameters.supervision.sweepReaperIntervalSecs`, startup-anchored) >
/// default ([`DEFAULT_REAPER_INTERVAL_SECS`])**. The env var sits above the
/// layer. A zero or unparseable value at any tier falls through to the next.
#[must_use]
pub fn resolve_reaper_interval() -> Duration {
    resolve_reaper_interval_with_layer(crate::config_resolver::u64_from_layer_global(
        "supervision",
        "sweepReaperIntervalSecs",
    ))
}

/// [`resolve_reaper_interval`] with the layer tier injected — the testable
/// form (no process-global read).
#[must_use]
pub fn resolve_reaper_interval_with_layer(layer: Option<u64>) -> Duration {
    Duration::from_secs(reaper_interval_secs_from(
        std::env::var(REAPER_INTERVAL_ENV)
            .ok()
            .and_then(|s| s.parse::<u64>().ok()),
        layer,
    ))
}

/// Pure **env > layer > default** precedence over already-parsed tiers —
/// split out so tests exercise the precedence without touching process-global
/// env state. A zero/unparseable value at any tier falls through to the next.
fn reaper_interval_secs_from(env: Option<u64>, layer: Option<u64>) -> u64 {
    env.filter(|&s| s > 0)
        .or(layer)
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_REAPER_INTERVAL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== resolve tier precedence (env > hyperparameters layer > default) =====
    //
    // Pure-tier tests: the *_from helpers take already-parsed Option tiers, so
    // these never touch process-global env state (no `#[serial]` needed).

    #[test]
    fn reaper_interval_tiers_env_beats_layer_beats_default() {
        assert_eq!(reaper_interval_secs_from(Some(7), Some(60)), 7);
        assert_eq!(reaper_interval_secs_from(None, Some(60)), 60);
        assert_eq!(reaper_interval_secs_from(None, None), DEFAULT_REAPER_INTERVAL_SECS);
    }

    #[test]
    fn reaper_interval_tiers_zero_or_invalid_falls_through_to_next_tier() {
        // Env set-but-zero falls through to the layer, then the default (a
        // zero-interval busy loop is never useful).
        assert_eq!(reaper_interval_secs_from(Some(0), Some(60)), 60);
        assert_eq!(reaper_interval_secs_from(Some(0), None), DEFAULT_REAPER_INTERVAL_SECS);
        // A zero layer is dropped the same way.
        assert_eq!(reaper_interval_secs_from(None, Some(0)), DEFAULT_REAPER_INTERVAL_SECS);
    }

    #[test]
    fn gh_timeout_tiers_env_beats_layer_beats_default() {
        assert_eq!(gh_timeout_secs_from(Some(3), Some(10)), 3);
        assert_eq!(gh_timeout_secs_from(None, Some(10)), 10);
        assert_eq!(gh_timeout_secs_from(None, None), REAP_GH_TIMEOUT_SECS);
    }

    #[test]
    fn gh_timeout_tiers_zero_or_invalid_falls_through_to_next_tier() {
        assert_eq!(gh_timeout_secs_from(Some(0), Some(10)), 10);
        assert_eq!(gh_timeout_secs_from(Some(0), None), REAP_GH_TIMEOUT_SECS);
        assert_eq!(gh_timeout_secs_from(None, Some(0)), REAP_GH_TIMEOUT_SECS);
    }
}
