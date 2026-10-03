//! The **tranche-2 hyperparameters layer** tier of [`crate::restart_verify`]'s
//! numeric knobs: each knob resolves with precedence
//! **single-knob env var > hyperparameters layer > built-in default**, the
//! layer read via `crate::config_resolver::u64_from_layer_global`
//! (startup-anchored — `None` outside a running daemon, so env > default is
//! preserved in CLI-only contexts).
//!
//! This is a separate sibling module purely for the file-size ratchet:
//! [`crate::restart_verify`] sat one batch of code lines under its 1000-line
//! threshold, and the size policy sends new code to a NEW sibling module
//! rather than growing the file. Everything here is re-exported from
//! [`crate::restart_verify`], so every public path is unchanged.

use std::time::Duration;

use super::{
    DEFAULT_POLL_INTERVAL_MS, DEFAULT_POLL_SECS, DEFAULT_RECOVERY_POLL_SECS, POLL_SECS_ENV,
    RECOVERY_POLL_SECS_ENV,
};

/// Resolve the configured post-restart verification poll bound: env override
/// ([`POLL_SECS_ENV`]), else the hyperparameters layer
/// (`hyperparameters.process.restartPollSecs`, startup-anchored — `None`
/// outside a running daemon, so env > default is preserved in CLI-only
/// contexts), else [`DEFAULT_POLL_SECS`] — the exact knob
/// [`verify_and_heal`](crate::restart_verify::verify_and_heal) itself
/// resolves, so every caller describing this bound in a log line (Issue
/// #6969) can never disagree with the verifier that actually polls against
/// it.
#[must_use]
pub fn resolve_configured_poll_secs() -> u64 {
    resolve_configured_poll_secs_with_layer(crate::config_resolver::u64_from_layer_global(
        "process",
        "restartPollSecs",
    ))
}

/// [`resolve_configured_poll_secs`] with the layer tier injected — the
/// testable form (no process-global read). The env tier keeps the shell
/// implementation's verbatim-parse semantics (an unparsable value falls
/// through, a parsed zero is honored); the layer tier treats zero as unset.
#[must_use]
pub fn resolve_configured_poll_secs_with_layer(layer: Option<u64>) -> u64 {
    secs_from(
        std::env::var(POLL_SECS_ENV)
            .ok()
            .as_deref()
            .and_then(|v| v.trim().parse::<u64>().ok()),
        layer,
        DEFAULT_POLL_SECS,
    )
}

/// Resolve the self-heal poll bound: env override
/// ([`RECOVERY_POLL_SECS_ENV`]), else the hyperparameters layer
/// (`hyperparameters.process.restartKickstartPollSecs`), else
/// [`DEFAULT_RECOVERY_POLL_SECS`] — the exact knob
/// [`verify_and_heal`](crate::restart_verify::verify_and_heal)'s
/// systemd/launchd recovery branches poll against.
#[must_use]
pub fn resolve_recovery_poll_secs() -> u64 {
    resolve_recovery_poll_secs_with_layer(crate::config_resolver::u64_from_layer_global(
        "process",
        "restartKickstartPollSecs",
    ))
}

/// [`resolve_recovery_poll_secs`] with the layer tier injected — the testable
/// form (no process-global read). Precedence: env > `layer` > default.
#[must_use]
pub fn resolve_recovery_poll_secs_with_layer(layer: Option<u64>) -> u64 {
    secs_from(
        std::env::var(RECOVERY_POLL_SECS_ENV)
            .ok()
            .as_deref()
            .and_then(|v| v.trim().parse::<u64>().ok()),
        layer,
        DEFAULT_RECOVERY_POLL_SECS,
    )
}

/// [`crate::restart_verify::resolve_interval`] with the layer tier injected —
/// the testable form (no process-global read). The raw tier keeps its exact
/// parse/validation: a fractional string is scaled to whole milliseconds and
/// any non-finite/non-positive value falls through.
#[must_use]
pub fn resolve_interval_with_layer(raw: Option<&str>, layer: Option<u64>) -> Duration {
    Duration::from_millis(interval_ms_from(
        raw.and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| (v * 1000.0) as u64)
            .filter(|ms| *ms > 0),
        layer,
    ))
}

/// Pure **env > layer > default** precedence over already-parsed tiers — the
/// shared core of the poll and self-heal seconds knobs, split out so tests
/// exercise the precedence without touching process-global env state. The
/// env tier keeps the shell implementation's verbatim semantics (a parsed
/// value, including 0, is honored as-is — a zero poll bound still probes
/// once); a zero layer value is treated as unset and falls through to
/// `default`.
fn secs_from(env: Option<u64>, layer: Option<u64>, default: u64) -> u64 {
    env.or(layer.filter(|&s| s > 0)).unwrap_or(default)
}

/// Pure **env > layer > default** precedence over already-parsed millisecond
/// tiers — split out so tests exercise the precedence without touching
/// process-global env state. The env tier's parse already rejects
/// non-positive values, so a `Some` env is always honored; a zero layer
/// value falls through to the default.
fn interval_ms_from(env_ms: Option<u64>, layer_ms: Option<u64>) -> u64 {
    env_ms
        .or(layer_ms)
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_POLL_INTERVAL_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== pure-tier precedence tests (tranche 2) =====
    //
    // `secs_from` / `interval_ms_from` take already-parsed Option tiers, so
    // these never touch process-global env state (no `#[serial]` needed).

    #[test]
    fn restart_poll_secs_tiers_env_beats_layer_beats_default() {
        assert_eq!(secs_from(Some(45), Some(60), DEFAULT_POLL_SECS), 45);
        assert_eq!(secs_from(None, Some(60), DEFAULT_POLL_SECS), 60);
        assert_eq!(secs_from(None, None, DEFAULT_POLL_SECS), DEFAULT_POLL_SECS);
        // The same core serves the self-heal bound with its own default.
        assert_eq!(secs_from(None, Some(60), DEFAULT_RECOVERY_POLL_SECS), 60);
        assert_eq!(secs_from(None, None, DEFAULT_RECOVERY_POLL_SECS), DEFAULT_RECOVERY_POLL_SECS);
        // The env tier is honored verbatim (a parsed 0 still means "probe
        // once and give up", the shell implementation's semantics).
        assert_eq!(secs_from(Some(0), Some(60), DEFAULT_POLL_SECS), 0);
        // A zero layer is treated as unset and falls through to the default.
        assert_eq!(secs_from(None, Some(0), DEFAULT_POLL_SECS), DEFAULT_POLL_SECS);
    }

    #[test]
    fn restart_poll_interval_tiers_env_beats_layer_beats_default() {
        assert_eq!(interval_ms_from(Some(250), Some(2000)), 250);
        assert_eq!(interval_ms_from(None, Some(2000)), 2000);
        assert_eq!(interval_ms_from(None, None), DEFAULT_POLL_INTERVAL_MS);
        // A zero layer falls through, exactly like a non-positive raw value.
        assert_eq!(interval_ms_from(None, Some(0)), DEFAULT_POLL_INTERVAL_MS);
        // The with_layer form rides the same parse/default path as the raw
        // override (fractional seconds in, whole milliseconds out).
        assert_eq!(resolve_interval_with_layer(None, Some(200)), Duration::from_millis(200));
        assert_eq!(
            resolve_interval_with_layer(Some("0.25"), Some(200)),
            Duration::from_millis(250)
        );
        assert_eq!(
            resolve_interval_with_layer(Some("abc"), Some(200)),
            Duration::from_millis(200),
            "an unparsable raw value falls through to the layer, not straight to the default"
        );
    }
}
