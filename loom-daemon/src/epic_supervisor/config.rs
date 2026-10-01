//! Epic-supervisor cadence knobs: the tick interval and the in-flight
//! singleton TTL, each resolved with precedence **single-knob env var >
//! hyperparameters layer > built-in default**.
//!
//! Split out of `epic_supervisor.rs` for the same file-size-ratchet reason as
//! [`crate::work_finder::config`]: `epic_supervisor.rs` is frozen at its
//! current size (`.loom/docs/file-size-policy.md`). Pure move plus the new
//! hyperparameters layer tier — every symbol that was `pub` before is
//! re-exported from [`super`], so call sites are unchanged.
//!
//! The layer tier reads `hyperparameters.supervision.epicSupervisorIntervalSecs`
//! / `hyperparameters.supervision.epicInflightTtlSecs` through
//! [`crate::config_resolver::u64_from_layer_global`], which returns `None` before
//! daemon startup captures the root — so outside a running daemon this module
//! behaves exactly as env > default.

use std::time::Duration;

/// Default time-to-live for an in-flight singleton transition before the
/// supervisor is willing to re-dispatch it. A role dispatch that lands its
/// mutation advances the derived state (clearing the ledger) well within this
/// window; the TTL only ever fires when a dispatched role *crashed* without
/// landing its mutation, so re-dispatch is the correct recovery.
pub const DEFAULT_INFLIGHT_TTL_SECS: u64 = 900;

/// Environment variable overriding [`DEFAULT_INFLIGHT_TTL_SECS`]. Follows the
/// `LOOM_*` convention used elsewhere in the daemon.
pub const INFLIGHT_TTL_ENV: &str = "LOOM_EPIC_INFLIGHT_TTL_SECS";

/// Environment variable overriding the supervisor tick interval (seconds).
pub const SUPERVISOR_INTERVAL_ENV: &str = "LOOM_EPIC_SUPERVISOR_INTERVAL_SECS";

/// Default supervisor tick interval. Epics advance on the order of minutes
/// (each transition spawns a role process), so a 5-minute cadence is ample and
/// keeps forge query volume low.
pub const DEFAULT_SUPERVISOR_INTERVAL_SECS: u64 = 300;

/// Resolve the in-flight TTL: [`INFLIGHT_TTL_ENV`] override, else the
/// hyperparameters layer (`hyperparameters.supervision.epicInflightTtlSecs`,
/// startup-anchored), else [`DEFAULT_INFLIGHT_TTL_SECS`]. The env var sits
/// above the layer.
pub(super) fn resolve_inflight_ttl() -> Duration {
    resolve_inflight_ttl_with_layer(crate::config_resolver::u64_from_layer_global(
        "supervision",
        "epicInflightTtlSecs",
    ))
}

/// [`resolve_inflight_ttl`] with the layer tier injected — the testable form
/// (no process-global read). A zero/unparseable value at any tier falls
/// through to the next.
pub(super) fn resolve_inflight_ttl_with_layer(layer: Option<u64>) -> Duration {
    Duration::from_secs(inflight_ttl_from(
        std::env::var(INFLIGHT_TTL_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok()),
        layer,
    ))
}

/// Pure **env > layer > default** precedence over already-parsed tiers —
/// split out so tests exercise the precedence without touching process-global
/// env state. A zero/unparseable value at any tier falls through to the next.
fn inflight_ttl_from(env: Option<u64>, layer: Option<u64>) -> u64 {
    env.filter(|&s| s > 0)
        .or(layer)
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_INFLIGHT_TTL_SECS)
}

/// Resolve the supervisor tick interval: [`SUPERVISOR_INTERVAL_ENV`] override,
/// else the hyperparameters layer
/// (`hyperparameters.supervision.epicSupervisorIntervalSecs`,
/// startup-anchored), else [`DEFAULT_SUPERVISOR_INTERVAL_SECS`]. The env var
/// sits above the layer. A zero or unparseable value at any tier falls through
/// to the next (a zero-interval busy loop is never useful).
#[must_use]
pub fn resolve_supervisor_interval() -> Duration {
    resolve_supervisor_interval_with_layer(crate::config_resolver::u64_from_layer_global(
        "supervision",
        "epicSupervisorIntervalSecs",
    ))
}

/// [`resolve_supervisor_interval`] with the layer tier injected — the
/// testable form (no process-global read).
#[must_use]
pub fn resolve_supervisor_interval_with_layer(layer: Option<u64>) -> Duration {
    Duration::from_secs(supervisor_interval_from(
        std::env::var(SUPERVISOR_INTERVAL_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok()),
        layer,
    ))
}

/// Pure **env > layer > default** precedence over already-parsed tiers —
/// split out so tests exercise the precedence without touching process-global
/// env state. A zero/unparseable value at any tier falls through to the next.
fn supervisor_interval_from(env: Option<u64>, layer: Option<u64>) -> u64 {
    env.filter(|&s| s > 0)
        .or(layer)
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_SUPERVISOR_INTERVAL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== resolve tier precedence (env > hyperparameters layer > default) =====
    //
    // Pure-tier tests: the *_from helpers take already-parsed Option tiers, so
    // these never touch process-global env state (no `#[serial]` needed).

    #[test]
    fn interval_tiers_env_beats_layer_beats_default() {
        assert_eq!(supervisor_interval_from(Some(60), Some(120)), 60);
        assert_eq!(supervisor_interval_from(None, Some(120)), 120);
        assert_eq!(supervisor_interval_from(None, None), DEFAULT_SUPERVISOR_INTERVAL_SECS);
    }

    #[test]
    fn interval_tiers_zero_or_invalid_falls_through_to_next_tier() {
        // Env set-but-zero falls through to the layer, then the default.
        assert_eq!(supervisor_interval_from(Some(0), Some(120)), 120);
        assert_eq!(supervisor_interval_from(Some(0), None), DEFAULT_SUPERVISOR_INTERVAL_SECS);
        // A zero layer is dropped the same way.
        assert_eq!(supervisor_interval_from(None, Some(0)), DEFAULT_SUPERVISOR_INTERVAL_SECS);
    }

    #[test]
    fn inflight_ttl_tiers_env_beats_layer_beats_default() {
        assert_eq!(inflight_ttl_from(Some(60), Some(1200)), 60);
        assert_eq!(inflight_ttl_from(None, Some(1200)), 1200);
        assert_eq!(inflight_ttl_from(None, None), DEFAULT_INFLIGHT_TTL_SECS);
    }

    #[test]
    fn inflight_ttl_tiers_zero_or_invalid_falls_through_to_next_tier() {
        assert_eq!(inflight_ttl_from(Some(0), Some(1200)), 1200);
        assert_eq!(inflight_ttl_from(Some(0), None), DEFAULT_INFLIGHT_TTL_SECS);
        assert_eq!(inflight_ttl_from(None, Some(0)), DEFAULT_INFLIGHT_TTL_SECS);
    }
}
