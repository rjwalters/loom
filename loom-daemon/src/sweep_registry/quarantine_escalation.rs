//! Quarantine escalation: config surface, per-issue state, and the
//! generation-based TTL ladder (vibesql#6639).
//!
//! Split out of `quarantine.rs` when the escalation landed (vibesql#6639):
//! `quarantine.rs` is over the file-size ratchet's frozen threshold
//! (`.loom/docs/file-size-policy.md`), so the new logic lives here instead of
//! growing it — and the config-resolution + wire-type blocks moved with it so
//! that file actually shrank. Everything in this module is about *policy*
//! (how long a quarantine lasts, how a relapse escalates) rather than
//! *mechanics* (tallying deaths, applying labels), which stay in
//! `quarantine.rs`.
//!
//! | piece | role |
//! |-------|------|
//! | [`QuarantineConfig`] | resolved knobs (env > config-file > default) |
//! | [`effective_quarantine_ttl_secs`] | the generation-N TTL ladder |
//! | [`QuarantineState`] | the per-registry mutable maps the reaper updates |
//! | [`QuarantineEntry`] | the read-only wire row behind `quarantine list` |

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ============================================================================
// Insta-crash quarantine constants (Issue #3939)
// ============================================================================

/// Env var toggling insta-crash quarantine (Issue #3939). `0`/`false`/`no`/`off`
/// disables; `1`/`true`/`yes`/`on` forces on. Overrides config. Defaults ON — it
/// is a safety backstop against a broken workspace starving the shared queue.
pub const QUARANTINE_ENABLE_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE";

/// Env var overriding the consecutive-insta-crash threshold at which an issue is
/// quarantined. A zero/invalid value falls through to config/default.
pub const QUARANTINE_THRESHOLD_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE_THRESHOLD";

/// Env var overriding the quarantine TTL, in seconds. A zero/invalid value falls
/// through to config/default.
pub const QUARANTINE_TTL_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE_TTL_SECS";

/// Env var overriding the quarantine TTL ceiling, in seconds — the cap on the
/// per-generation escalation introduced for vibesql#6639. A zero/invalid value
/// falls through to config/default.
pub const QUARANTINE_TTL_MAX_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE_TTL_MAX_SECS";

/// Env var overriding the insta-crash window, in seconds: a checkpoint-less
/// terminal transition within this wall-clock window of dispatch counts as an
/// insta-crash. A zero/invalid value falls through to config/default.
pub const QUARANTINE_INSTA_CRASH_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE_INSTA_CRASH_SECS";

/// Default consecutive-insta-crash threshold before quarantine (#3939).
pub const DEFAULT_QUARANTINE_THRESHOLD: u32 = 3;

/// Default quarantine TTL: a quarantined issue is auto-released after this window
/// so a transient breakage (e.g. a token pool that was re-provisioned) recovers
/// without operator action (#3939). This is the **generation-1** TTL: a
/// quarantine that relapses after release serves an escalated TTL
/// (vibesql#6639) — see [`effective_quarantine_ttl_secs`].
pub const DEFAULT_QUARANTINE_TTL_SECS: u64 = 3600;

/// Default ceiling on the escalated quarantine TTL (vibesql#6639): generation N
/// serves `ttl * 2^(N-1)` capped here, so a persistently-broken issue's
/// crash-pause-repeat flap decays toward at-most-daily retries instead of
/// cycling every TTL forever. 24h: an operator running
/// `loom-daemon quarantine list` on any working day still sees the entry.
pub const DEFAULT_QUARANTINE_TTL_MAX_SECS: u64 = 86400;

/// Default insta-crash window (#3939): a checkpoint-less terminal transition
/// within this many seconds of dispatch counts toward the insta-crash tally. A
/// real build that reaches even the Curator checkpoint, or a slow death past this
/// window, is a *different* failure mode (handled by the mid-build watchdog) and
/// never counts here.
pub const DEFAULT_QUARANTINE_INSTA_CRASH_SECS: i64 = 60;

/// Resolved insta-crash-quarantine parameters (Issue #3939), set on the registry
/// at construction so `SweepRegistry::reap_once` can enforce them without a
/// per-tick config read. Defaults mirror the shipped constants (enabled — it is
/// a safety backstop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuarantineConfig {
    /// Whether insta-crash quarantine is active. When `false` the reaper neither
    /// counts insta-crashes nor quarantines (byte-for-byte the pre-#3939 path).
    pub enabled: bool,
    /// Consecutive insta-crashes before an issue is quarantined.
    pub threshold: u32,
    /// How long a quarantine entry persists before auto-release.
    pub ttl: Duration,
    /// Ceiling on the escalated (per-generation) TTL (vibesql#6639).
    pub ttl_max: Duration,
    /// The insta-crash wall-clock window: a checkpoint-less terminal transition
    /// within this many seconds of dispatch counts as an insta-crash.
    pub insta_crash_secs: i64,
}

impl Default for QuarantineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: DEFAULT_QUARANTINE_THRESHOLD,
            ttl: Duration::from_secs(DEFAULT_QUARANTINE_TTL_SECS),
            ttl_max: Duration::from_secs(DEFAULT_QUARANTINE_TTL_MAX_SECS),
            insta_crash_secs: DEFAULT_QUARANTINE_INSTA_CRASH_SECS,
        }
    }
}

/// The escalated TTL, in seconds, a **generation-N** quarantine serves
/// (vibesql#6639): `min(ttl * 2^(N-1), ttl_max)` with saturating arithmetic.
/// Generation 1 is the plain configured TTL; every relapse without an
/// intervening healthy outcome or operator clear doubles the pause.
#[must_use]
pub fn effective_quarantine_ttl_secs(config: &QuarantineConfig, generation: u32) -> u64 {
    let shift = generation.saturating_sub(1).min(31);
    config
        .ttl
        .as_secs()
        .saturating_mul(1u64 << shift)
        .min(config.ttl_max.as_secs())
        .max(config.ttl.as_secs())
}

// ============================================================================
// Insta-crash quarantine config resolution (Issue #3939)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.workFinder.quarantine` this
/// module consumes (Issue #3939). Each field is `Option` so an absent key falls
/// through to the env-var / built-in-default resolution — precedence
/// **env > config > default** for every knob, matching the rest of the module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuarantineFileConfig {
    /// `autonomous.workFinder.quarantine.enabled` — whether quarantine runs.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.quarantine.threshold` — consecutive insta-crashes
    /// before quarantine (zero/invalid dropped to `None`).
    pub threshold: Option<u32>,
    /// `autonomous.workFinder.quarantine.ttlSecs` — quarantine TTL, in seconds
    /// (zero/invalid dropped to `None`).
    pub ttl_secs: Option<u64>,
    /// `autonomous.workFinder.quarantine.ttlMaxSecs` — ceiling on the
    /// escalated per-generation TTL (vibesql#6639; zero/invalid dropped to
    /// `None`).
    pub ttl_max_secs: Option<u64>,
    /// `autonomous.workFinder.quarantine.instaCrashSecs` — insta-crash window, in
    /// seconds (zero/invalid dropped to `None`).
    pub insta_crash_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.workFinder.quarantine` (Issue #3939),
/// soft-failing every field to `None` on a missing file, malformed JSON, or an
/// absent `autonomous` / `workFinder` / `quarantine` block. Mirrors
/// [`read_startup_race_config`].
///
/// [`read_startup_race_config`]: crate::config_resolver
#[must_use]
pub fn read_quarantine_file_config(repo_root: &Path) -> QuarantineFileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(q) = crate::config_resolver::get_path(&effective, "autonomous.workFinder.quarantine")
    else {
        return QuarantineFileConfig::default();
    };
    QuarantineFileConfig {
        enabled: q.get("enabled").and_then(serde_json::Value::as_bool),
        threshold: q
            .get("threshold")
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| u32::try_from(n).ok()),
        ttl_secs: q
            .get("ttlSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        ttl_max_secs: q
            .get("ttlMaxSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        insta_crash_secs: q
            .get("instaCrashSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve the full [`QuarantineConfig`] for `repo_root` with precedence
/// **env > config > default** for every knob (Issue #3939). Reads the file
/// config internally, then layers env overrides on top, then the shipped
/// defaults. Enabled defaults **on** — it is a safety backstop.
#[must_use]
pub fn resolve_quarantine_config(repo_root: &Path) -> QuarantineConfig {
    let file = read_quarantine_file_config(repo_root);

    let enabled = if let Ok(v) = std::env::var(QUARANTINE_ENABLE_ENV) {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    } else {
        file.enabled.unwrap_or(true)
    };

    let threshold = std::env::var(QUARANTINE_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        .or(file.threshold)
        .unwrap_or(DEFAULT_QUARANTINE_THRESHOLD);

    let ttl_secs = std::env::var(QUARANTINE_TTL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.ttl_secs)
        .unwrap_or(DEFAULT_QUARANTINE_TTL_SECS);

    let ttl_max_secs = std::env::var(QUARANTINE_TTL_MAX_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.ttl_max_secs)
        .unwrap_or(DEFAULT_QUARANTINE_TTL_MAX_SECS)
        // The ceiling only ever *extends* the pause; a ceiling below the base
        // TTL would silently shorten generation 1 — clamp instead.
        .max(ttl_secs);

    let insta_crash_secs = std::env::var(QUARANTINE_INSTA_CRASH_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.insta_crash_secs)
        .and_then(|s| i64::try_from(s).ok())
        .unwrap_or(DEFAULT_QUARANTINE_INSTA_CRASH_SECS);

    QuarantineConfig {
        enabled,
        threshold,
        ttl: Duration::from_secs(ttl_secs),
        ttl_max: Duration::from_secs(ttl_max_secs),
        insta_crash_secs,
    }
}

// ============================================================================
// Per-registry quarantine state (vibesql#6639 consolidation)
// ============================================================================

/// The four mutable maps the insta-crash quarantine tracks per registry
/// (Issue #3939, consolidated vibesql#6639). Previously four loose fields on
/// `SweepRegistry` — grouped here when the escalation added a fifth
/// (`generations`) so `sweep_registry/mod.rs` (over the file-size ratchet's
/// frozen threshold) would hold one field instead of five. All access goes
/// through `SweepRegistry`'s methods; the fields are `pub(crate)` only because
/// sibling modules' tests seed them directly.
#[derive(Debug, Default)]
pub struct QuarantineState {
    /// Consecutive insta-crash tally per issue. Any non-insta-crash terminal
    /// outcome resets the entry.
    pub(crate) insta_crash_counts: HashMap<u32, u32>,
    /// Currently-quarantined issues → the instant they were quarantined (Issue
    /// #3939). The work finder skips these until the entry ages past the
    /// issue's **effective** TTL (generation-escalated, vibesql#6639).
    pub(crate) quarantined: HashMap<u32, DateTime<Utc>>,
    /// Per-issue quarantine **generation** (vibesql#6639): how many times this
    /// issue has been quarantined this daemon lifetime without an intervening
    /// healthy outcome (progress/clean exit) or operator clear. An entry's
    /// presence is the probation condition — after a TTL release, the FIRST
    /// further insta-crash re-quarantines immediately at an escalated TTL
    /// (`ttl * 2^(gen-1)`, capped at `ttl_max`) instead of re-running the full
    /// 3-crash runway, so a persistent breakage's crash-pause-repeat flap decays
    /// instead of cycling every TTL forever. Not persisted across daemon
    /// restarts, same as `quarantined` itself.
    pub(crate) generations: HashMap<u32, u32>,
    /// Issues whose `loom:blocked` -> `loom:issue` label restore failed at
    /// least once (Issue #4110): the release flip is a best-effort `gh` call,
    /// and a transient failure must not silently strand the issue at
    /// `loom:blocked` forever — the reaper retries every entry here on each
    /// tick until the flip succeeds.
    pub(crate) pending_release: HashSet<u32>,
}

// ============================================================================
// Wire surface (Issue #4215; generation fields vibesql#6639)
// ============================================================================

/// One active insta-crash quarantine (Issue #4215), as surfaced by
/// `loom-daemon quarantine list` / `crate::types::Request::ListQuarantines`.
/// Joins the quarantine state the registry tracks in-memory into one read-only
/// row. Re-exported from `crate::types` for path stability with the rest of
/// the status-wire surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuarantineEntry {
    /// The quarantined issue number.
    pub issue: u32,
    /// The workspace whose registry this quarantine lives in — meaningful once
    /// `ListQuarantines` enumerates across every registered workspace.
    pub workspace_root: PathBuf,
    /// When the quarantine was applied.
    pub quarantined_at: DateTime<Utc>,
    /// The consecutive-insta-crash tally that triggered (or is at) quarantine.
    pub insta_crash_count: u32,
    /// The consecutive-insta-crash threshold configured for this issue's
    /// workspace, so the CLI can render "tally / threshold" instead of a bare
    /// count. Read from the same per-workspace [`QuarantineConfig`] the reaper
    /// enforces against — different managed workspaces may configure different
    /// thresholds.
    pub insta_crash_threshold: u32,
    /// The quarantine **generation** this entry is serving (vibesql#6639): 1
    /// for a first quarantine, N for the (N-1)th relapse without an
    /// intervening healthy outcome or operator clear. Drives the escalated
    /// TTL (`ttl * 2^(generation-1)`, capped at `ttl_max`) the reaper
    /// enforces and `ttl_remaining_secs` reflects.
    pub generation: u32,
    /// Seconds remaining before TTL auto-release, clamped to `0` — the TTL is
    /// enforced only by the reaper's expiry sweep, so an entry can be
    /// momentarily past-TTL between ticks; a negative remainder would be a
    /// confusing thing to render.
    pub ttl_remaining_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// vibesql#6639: the escalation math itself. Generation N serves
    /// `ttl * 2^(N-1)` capped at `ttl_max`; a ceiling below the base TTL clamps
    /// UP to the base (the cap may only extend the pause, never shorten gen 1).
    #[test]
    fn quarantine_ttl_escalates_by_generation_and_caps() {
        let config = QuarantineConfig {
            ttl: Duration::from_secs(3600),
            ttl_max: Duration::from_secs(86400),
            ..QuarantineConfig::default()
        };
        assert_eq!(effective_quarantine_ttl_secs(&config, 1), 3600);
        assert_eq!(effective_quarantine_ttl_secs(&config, 2), 7200);
        assert_eq!(effective_quarantine_ttl_secs(&config, 3), 14400);
        assert_eq!(effective_quarantine_ttl_secs(&config, 5), 57600);
        // 3600 << 5 = 115200 > 86400: capped.
        assert_eq!(effective_quarantine_ttl_secs(&config, 6), 86400);
        assert_eq!(effective_quarantine_ttl_secs(&config, 40), 86400, "saturates, never overflows");
        // A misconfigured ceiling below the base TTL never shortens generation 1.
        let clamped = QuarantineConfig {
            ttl: Duration::from_secs(3600),
            ttl_max: Duration::from_secs(60),
            ..QuarantineConfig::default()
        };
        assert_eq!(effective_quarantine_ttl_secs(&clamped, 1), 3600);
        assert_eq!(effective_quarantine_ttl_secs(&clamped, 4), 3600);
    }

    /// vibesql#6639: the default 24h ceiling is above the default 1h base TTL,
    /// so a stock deployment's generation ladder is strictly increasing until
    /// the cap.
    #[test]
    fn default_ceiling_sits_above_default_base_ttl() {
        let config = QuarantineConfig::default();
        assert!(config.ttl_max > config.ttl, "ttl_max must exceed ttl by default");
        assert_eq!(effective_quarantine_ttl_secs(&config, 1), DEFAULT_QUARANTINE_TTL_SECS);
    }
}
