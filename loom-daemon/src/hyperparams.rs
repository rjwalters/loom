//! Unified operational hyperparameters (Issue #9683).
//!
//! Loom's operational tunables — tick intervals, concurrency ceilings,
//! admission caps, lease TTLs, backoff thresholds — were scattered across
//! per-module built-in constants, one-off `LOOM_*` env vars, and
//! module-specific `autonomous.*` config blocks. That dispersion made two
//! things impossible: recording *which* hyperparameter vector a given run
//! executed under (run provenance), and having an external optimizer (CMA-ES,
//! Bayesian search) sample the vector programmatically.
//!
//! This module is the consolidation seam. It defines one typed, validated
//! vector over a first tranche of those tunables, addresses it at a single
//! config surface — `.loom/config.json → "hyperparameters"` — with single-knob
//! env vars layered on top:
//!
//! ```text
//! single-knob env (LOOM_WORK_FINDER_INTERVAL_SECS, LOOM_LEASE_TTL_MINUTES, …)
//!   > .loom/config.json "hyperparameters" block (the committed, validated home)
//!   > legacy per-module config key (autonomous.workFinder.*, autonomous.idleExit.*)
//!   > built-in default (the constant each knob uses today)
//! ```
//!
//! The legacy tier keeps working unchanged — a committed `autonomous.*` key
//! never breaks across this upgrade — but the `hyperparameters` block wins
//! where both are present, and new tuning should target it.
//!
//! Guarantees, matching the issue's acceptance criteria:
//!
//! 1. **Typed schema + one structured surface** — [`Hyperparameters`] groups
//!    the knobs (`dispatch`, `lifecycle`, `rework`, `champion`) with documented ranges.
//! 2. **Fail-fast startup validation** — [`startup_init`] resolves the layer,
//!    rejects unknown keys, wrong types, out-of-range values and the
//!    contradictory `low >= high` backoff pair *by name*, and aborts daemon
//!    startup; it never half-applies a bad vector. (Legacy-tier values keep
//!    each module's own soft-fallback semantics — only the new surface is
//!    strict.)
//! 3. **Per-field provenance** — [`resolve_effective`] reports which tier
//!    supplied each field ([`Source`]), logged at startup and printed by
//!    `loom-daemon hyperparams`.
//!
//! Tranche 1 fields (each a real consumed tunable — see the field docs):
//! `dispatch.{tickIntervalSecs,maxConcurrent,maxAdmissionsPerTick}`,
//! `lifecycle.{leaseTtlMinutes,idleExitMinutes}`, and
//! `rework.{buildBackoffHigh,buildBackoffLow}`. The `champion` group (#10753)
//! carries Champion's promotion-throughput knobs
//! (`prSlice,promotionSlice,tier2Cap,tier3Cap,tier3BacklogCap`). Later tranches migrate the
//! remaining knobs (host breaker, admission brake, merge-train bounds, role
//! budgets) onto the same surface; the schema, validation and provenance
//! mechanics here are the whole point — adding a field is one struct entry,
//! one range check, and one consumer overlay.

use anyhow::{bail, Result};
use clap::Args;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

use crate::config_resolver;
use crate::work_finder::{
    build_backoff::{BuildBackoffConfig, DEFAULT_HIGH, DEFAULT_LOW},
    DEFAULT_MAX_ADMISSIONS_PER_TICK, DEFAULT_WORK_FINDER_INTERVAL_SECS,
    DEFAULT_WORK_FINDER_MAX_CONCURRENT,
};

/// Retired env var that used to carry a JSON hyperparameter vector (Issue
/// #11107). It is no longer read as a tier; it is only named so a stale
/// export gets one warning instead of silently doing nothing.
pub const HYPERPARAMS_ENV: &str = "LOOM_HYPERPARAMS";

static WARN_RETIRED_ENV: Once = Once::new();

/// Warn (once per process) that a set, non-empty `$LOOM_HYPERPARAMS` is
/// ignored. Never fails: the value has no effect, valid or not.
pub fn warn_if_retired_env_set() {
    if std::env::var(HYPERPARAMS_ENV).is_ok_and(|raw| !raw.trim().is_empty()) {
        WARN_RETIRED_ENV.call_once(|| {
            log::warn!(
                "hyperparams: ${HYPERPARAMS_ENV} is no longer supported and is ignored; \
                 set the `hyperparameters` block in .loom/config.json or a single-knob env var instead"
            );
        });
    }
}

/// The resolved vector, captured once at daemon startup.
static RESOLVED: OnceLock<Resolved> = OnceLock::new();
/// The workspace root `startup_init` resolved against — the anchor for
/// hot-applied knob re-reads ([`lease_ttl_minutes_from_layer`]). `None`
/// before `startup_init` runs.
static ROOT: OnceLock<PathBuf> = OnceLock::new();

// ============================================================================
// Typed schema
// ============================================================================

/// Dispatch & scheduling tunables — the work-finder loop's cadence and
/// admission shape.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DispatchParams {
    /// Seconds between work-finder ticks. Source:
    /// `work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS` (60). Legacy config
    /// home: `autonomous.workFinder.intervalSecs`; single-knob env:
    /// `LOOM_WORK_FINDER_INTERVAL_SECS`. Range `[5, 3600]`: below 5s hammers
    /// the forge listing API; above 1h the `loom:issue` backlog stops draining.
    pub tick_interval_secs: u64,
    /// Operator ceiling on concurrent sweeps this host admits. Source:
    /// `work_finder::DEFAULT_WORK_FINDER_MAX_CONCURRENT` (3). Legacy:
    /// `autonomous.workFinder.maxConcurrent`; env:
    /// `LOOM_WORK_FINDER_MAX_CONCURRENT`. The dynamic cap is additionally
    /// bounded by disk/RAM headroom, so this is an upper bound, not a target.
    /// Range `[1, 256]`.
    pub max_concurrent: usize,
    /// How many *new* sweeps one tick may admit (the #4234 ramp cap).
    /// Source: `work_finder::DEFAULT_MAX_ADMISSIONS_PER_TICK` (3). Legacy:
    /// `autonomous.workFinder.maxAdmissionsPerTick`; env:
    /// `LOOM_WORK_FINDER_MAX_ADMISSIONS_PER_TICK`. Range `[1, 64]`.
    pub max_admissions_per_tick: usize,
}

/// Lifecycle timeout tunables — how long Loom waits before treating work or
/// a host as gone.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LifecycleParams {
    /// Minutes a sweep's lease record stays fresh before its claim is treated
    /// as unproven. Source: `claim_reconciliation::DEFAULT_LEASE_TTL_MINUTES`
    /// (15.0 = 3x the ~5-minute lease-renewal interval). Single-knob env:
    /// `LOOM_LEASE_TTL_MINUTES`. Range `(0, 1440]` — one day maximum.
    /// Hot-applied (#9768): re-resolved from the layer on every lease check,
    /// so a committed-block edit lands without a daemon restart.
    pub lease_ttl_minutes: f64,
    /// Minutes of full idleness before the opt-in idle exit powers a remote
    /// host down. Source: `idle_exit::DEFAULT_IDLE_MINUTES` (60). Legacy:
    /// `autonomous.idleExit.idleMinutes`; env:
    /// `LOOM_AUTONOMOUS_IDLE_EXIT_MINUTES`. Range `[1, 10080]` — one week.
    pub idle_exit_minutes: u64,
}

/// Rework & review bound tunables — the build back-off's hysteresis band on
/// PR debt (issue #9410).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ReworkParams {
    /// Engage a repo's back-off when its own PR debt (review + changes +
    /// merge) rises strictly above this (per repo since #10624). Source:
    /// `build_backoff::DEFAULT_HIGH` (40).
    /// Legacy: `autonomous.workFinder.buildBackoff.high`. Range `[1, 100000]`.
    pub build_backoff_high: usize,
    /// Release a repo's back-off when its PR debt falls strictly below this.
    /// Source: `build_backoff::DEFAULT_LOW` (25). Legacy:
    /// `autonomous.workFinder.buildBackoff.low`. Range `[0, 100000)`, and
    /// always `< build_backoff_high` — a crossed pair is a startup error from
    /// this surface (the legacy tier keeps its soft fallback to 40/25).
    pub build_backoff_low: usize,
}

/// Champion promotion-throughput tunables (issue #10753). Each has a
/// single-knob env var the Champion shell snippets read
/// (`LOOM_CHAMPION_*`, shown per field), resolved at the top tier with
/// `Source::Env` provenance; there is no legacy config tier.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ChampionParams {
    /// PR rows processed before the promotion pass runs. Env:
    /// `LOOM_CHAMPION_PR_SLICE`. Default 10. Range `[1, 1000]`.
    pub pr_slice: usize,
    /// Fresh promotion verdicts per pause while PR rows remain. Env:
    /// `LOOM_CHAMPION_PROMOTION_SLICE`. Default 3. Range `[1, 100]`.
    pub promotion_slice: usize,
    /// Tier 2 promotions per repository per pass. Env:
    /// `LOOM_CHAMPION_TIER2_CAP`. Default 2. Range `[0, 100]` (`0` disables).
    pub tier2_cap: usize,
    /// Tier 3 promotions per repository per pass. Env:
    /// `LOOM_CHAMPION_TIER3_CAP`. Default 1. Range `[0, 100]` (`0` disables).
    pub tier3_cap: usize,
    /// Open unheld `tier:maintenance` `loom:issue`/`loom:building` issues
    /// above which Tier 3 promotion is gated. Env:
    /// `LOOM_CHAMPION_TIER3_BACKLOG_CAP`. Default 5. Range `[0, 1000]`.
    pub tier3_backlog_cap: usize,
}

/// The unified hyperparameter vector: every consolidated operational tunable,
/// grouped by concern. Fields serialize in declaration order (the
/// `hyperparams --json` output).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Hyperparameters {
    pub dispatch: DispatchParams,
    pub lifecycle: LifecycleParams,
    pub rework: ReworkParams,
    pub champion: ChampionParams,
}

impl Default for Hyperparameters {
    fn default() -> Self {
        Self {
            dispatch: DispatchParams {
                tick_interval_secs: DEFAULT_WORK_FINDER_INTERVAL_SECS,
                max_concurrent: DEFAULT_WORK_FINDER_MAX_CONCURRENT,
                max_admissions_per_tick: DEFAULT_MAX_ADMISSIONS_PER_TICK,
            },
            lifecycle: LifecycleParams {
                lease_ttl_minutes: crate::claim_reconciliation::DEFAULT_LEASE_TTL_MINUTES,
                idle_exit_minutes: crate::idle_exit::DEFAULT_IDLE_MINUTES,
            },
            rework: ReworkParams {
                build_backoff_high: DEFAULT_HIGH,
                build_backoff_low: DEFAULT_LOW,
            },
            champion: ChampionParams {
                pr_slice: 10,
                promotion_slice: 3,
                tier2_cap: 2,
                tier3_cap: 1,
                tier3_backlog_cap: 5,
            },
        }
    }
}

// ============================================================================
// The hyperparameters layer: config block
// ============================================================================

/// Normalize an object's dotted keys into nested groups: `{"a.b": 1}` becomes
/// `{"a": {"b": 1}}`, merging (last wins) when a dotted key splits into an
/// existing group. Non-object values pass through untouched.
#[must_use]
pub fn normalize_dotted(value: &Value) -> Value {
    let Some(obj) = value.as_object() else {
        return value.clone();
    };
    let mut out = serde_json::Map::new();
    for (key, val) in obj {
        match key.split_once('.') {
            None => {
                let normalized = normalize_dotted(val);
                // A plain key and a dotted key can address the same group
                // (e.g. `dispatch` and `dispatch.maxConcurrent`); deep-merge
                // so both survive, the later key winning per field.
                out.insert(
                    key.clone(),
                    match out.get(key) {
                        Some(existing) => config_resolver::deep_merge(existing, &normalized),
                        None => normalized,
                    },
                );
            }
            Some((head, tail)) => {
                let nested = normalize_dotted(&Value::Object(
                    [(tail.to_string(), val.clone())].into_iter().collect(),
                ));
                let merged = match out.get(head) {
                    Some(existing) => config_resolver::deep_merge(existing, &nested),
                    None => nested,
                };
                out.insert(head.to_string(), merged);
            }
        }
    }
    Value::Object(out)
}

/// The hyperparameters layer for an already-resolved effective config: the
/// committed `"hyperparameters"` block, with dotted keys normalized.
#[must_use]
pub fn overlay_from_effective(effective: &Value) -> Value {
    warn_if_retired_env_set();
    effective
        .get("hyperparameters")
        .map_or(Value::Null, normalize_dotted)
}

/// Convenience wrapper: resolve the effective config for `root`, then
/// [`overlay_from_effective`].
#[must_use]
pub fn layer(root: &Path) -> Value {
    overlay_from_effective(&config_resolver::resolve_effective_config(root))
}

// ============================================================================
// Validation (strict, on the new surface only)
// ============================================================================

/// One schema violation, named for the fail-fast startup error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Dotted config path, e.g. `dispatch.maxConcurrent`.
    pub path: String,
    /// What is wrong with the value there.
    pub problem: String,
}

impl Violation {
    fn new(path: impl Into<String>, problem: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            problem: problem.into(),
        }
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.problem)
    }
}

/// Range checks for the integer fields, as `(min, max)` inclusive.
const TICK_INTERVAL_SECS_RANGE: (u64, u64) = (5, 3600);
const MAX_CONCURRENT_RANGE: (u64, u64) = (1, 256);
const MAX_ADMISSIONS_PER_TICK_RANGE: (u64, u64) = (1, 64);
const LEASE_TTL_MINUTES_MAX: f64 = 1440.0;
const IDLE_EXIT_MINUTES_RANGE: (u64, u64) = (1, 10080);
const BUILD_BACKOFF_HIGH_RANGE: (u64, u64) = (1, 100_000);
const BUILD_BACKOFF_LOW_RANGE: (u64, u64) = (0, 100_000);
/// Champion knobs: `(key, single-knob env var, range)`. The three caps admit
/// `0` because Champion's shell `_cap` honours it (`0` disables that tier /
/// gates Tier 3 shut — promotion-throughput.md); the slices do not.
const CHAMPION_KEYS: [(&str, &str, (u64, u64)); 5] = [
    ("prSlice", "LOOM_CHAMPION_PR_SLICE", (1, 1000)),
    ("promotionSlice", "LOOM_CHAMPION_PROMOTION_SLICE", (1, 100)),
    ("tier2Cap", "LOOM_CHAMPION_TIER2_CAP", (0, 100)),
    ("tier3Cap", "LOOM_CHAMPION_TIER3_CAP", (0, 100)),
    ("tier3BacklogCap", "LOOM_CHAMPION_TIER3_BACKLOG_CAP", (0, 1000)),
];

/// Parse a `LOOM_CHAMPION_*` value exactly as Champion's shell `_cap` does:
/// a non-empty all-digit string is the value (`0` included, no range clamp —
/// the shell applies none); anything else is absent (falls through).
fn champion_env_value(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// Validate the hyperparameters layer (config block): every key
/// must be known, correctly typed, and in range, and the backoff pair must
/// satisfy `low < high`. Absent keys are fine — they fall through to the
/// legacy tier / defaults. An explicit `null` layer (or group) is absent.
///
/// This is strict **only on the new surface**: a value inherited from a
/// legacy `autonomous.*` key is never checked here — those keys keep their
/// own documented soft-fallback semantics, so an existing committed config
/// can never start failing this gate.
pub fn validate_layer(layer: &Value) -> Vec<Violation> {
    let mut violations = Vec::new();
    let Some(groups) = layer.as_object() else {
        if !layer.is_null() {
            violations.push(Violation::new(
                "hyperparameters",
                "must be an object with `dispatch` / `lifecycle` / `rework` / `champion` groups",
            ));
        }
        return violations;
    };
    for (group, keys) in groups {
        match group.as_str() {
            "dispatch" => validate_dispatch(keys, &mut violations),
            "lifecycle" => validate_lifecycle(keys, &mut violations),
            "rework" => validate_rework(keys, &mut violations),
            "champion" => validate_champion(keys, &mut violations),
            unknown => violations.push(Violation::new(
                format!("hyperparameters.{unknown}"),
                "unknown group (expected dispatch | lifecycle | rework | champion)",
            )),
        }
    }
    violations
}

/// Check one `u64` field: type first, then inclusive range.
fn check_u64(
    group: &Value,
    group_name: &str,
    key: &str,
    range: (u64, u64),
    violations: &mut Vec<Violation>,
) {
    let path = format!("{group_name}.{key}");
    match group.get(key) {
        None | Some(Value::Null) => {}
        Some(value) => match value.as_u64() {
            None => violations.push(Violation::new(path, "expected a non-negative integer")),
            Some(n) if n < range.0 || n > range.1 => violations.push(Violation::new(
                path,
                format!("expected {}..={} (inclusive), got {n}", range.0, range.1),
            )),
            Some(_) => {}
        },
    }
}

fn validate_dispatch(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("dispatch", "must be an object"));
        return;
    }
    for (key, _) in group.as_object().into_iter().flatten() {
        if !matches!(key.as_str(), "tickIntervalSecs" | "maxConcurrent" | "maxAdmissionsPerTick") {
            violations.push(Violation::new(format!("dispatch.{key}"), "unknown key"));
        }
    }
    check_u64(group, "dispatch", "tickIntervalSecs", TICK_INTERVAL_SECS_RANGE, violations);
    check_u64(group, "dispatch", "maxConcurrent", MAX_CONCURRENT_RANGE, violations);
    check_u64(
        group,
        "dispatch",
        "maxAdmissionsPerTick",
        MAX_ADMISSIONS_PER_TICK_RANGE,
        violations,
    );
}

fn validate_lifecycle(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("lifecycle", "must be an object"));
        return;
    }
    for (key, _) in group.as_object().into_iter().flatten() {
        if !matches!(key.as_str(), "leaseTtlMinutes" | "idleExitMinutes") {
            violations.push(Violation::new(format!("lifecycle.{key}"), "unknown key"));
        }
    }
    check_u64(group, "lifecycle", "idleExitMinutes", IDLE_EXIT_MINUTES_RANGE, violations);
    if let Some(value) = group.get("leaseTtlMinutes").filter(|v| !v.is_null()) {
        match value.as_f64() {
            None => violations
                .push(Violation::new("lifecycle.leaseTtlMinutes", "expected a number of minutes")),
            Some(mins) if mins <= 0.0 || mins > LEASE_TTL_MINUTES_MAX => {
                violations.push(Violation::new(
                    "lifecycle.leaseTtlMinutes",
                    format!("expected 0 < minutes <= {LEASE_TTL_MINUTES_MAX}, got {mins}"),
                ))
            }
            Some(_) => {}
        }
    }
}

fn validate_rework(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("rework", "must be an object"));
        return;
    }
    for (key, _) in group.as_object().into_iter().flatten() {
        if !matches!(key.as_str(), "buildBackoffHigh" | "buildBackoffLow") {
            violations.push(Violation::new(format!("rework.{key}"), "unknown key"));
        }
    }
    check_u64(group, "rework", "buildBackoffHigh", BUILD_BACKOFF_HIGH_RANGE, violations);
    check_u64(group, "rework", "buildBackoffLow", BUILD_BACKOFF_LOW_RANGE, violations);
    if let (Some(high), Some(low)) = (
        group.get("buildBackoffHigh").and_then(Value::as_u64),
        group.get("buildBackoffLow").and_then(Value::as_u64),
    ) {
        if low >= high {
            violations.push(Violation::new(
                "rework.buildBackoffLow",
                format!("backoff pair crossed: low ({low}) must be < high ({high})"),
            ));
        }
    }
}

fn validate_champion(group: &Value, violations: &mut Vec<Violation>) {
    if !group.is_null() && !group.is_object() {
        violations.push(Violation::new("champion", "must be an object"));
        return;
    }
    for (key, _) in group.as_object().into_iter().flatten() {
        if !CHAMPION_KEYS.iter().any(|(k, _, _)| k == key) {
            violations.push(Violation::new(format!("champion.{key}"), "unknown key"));
        }
    }
    for (key, _, range) in CHAMPION_KEYS {
        check_u64(group, "champion", key, range, violations);
    }
}

// ============================================================================
// Resolution with provenance
// ============================================================================

/// Which tier supplied a field. Ordered by the precedence documented on the
/// module: a later tier only fills a field the earlier tiers left absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// A single-knob env var (today only the `LOOM_CHAMPION_*` knobs report
    /// this tier — #10753).
    Env,
    /// `.loom/config.json → "hyperparameters"` block.
    Config,
    /// The legacy per-module key (`autonomous.workFinder.*`, …).
    Legacy,
    /// The built-in default constant.
    Default,
}

impl Source {
    /// Label used in the startup log and the `hyperparams` CLI output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Config => "config",
            Self::Legacy => "legacy",
            Self::Default => "default",
        }
    }
}

/// The effective vector plus where each field came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The effective hyperparameter vector.
    pub params: Hyperparameters,
    /// Per-field provenance, keyed by dotted field name.
    pub sources: BTreeMap<String, Source>,
}

/// A layer group lookup in the committed block.
fn tier_value<'a>(block: &'a Value, group: &str, key: &str) -> Option<&'a Value> {
    block.get(group).and_then(|g| g.get(key))
}

/// Pick one `u64` field down the tier chain, recording its source.
fn pick_u64(
    block: &Value,
    group: &str,
    key: &str,
    legacy: Option<u64>,
    default: u64,
    sources: &mut BTreeMap<String, Source>,
) -> u64 {
    let from_layer = tier_value(block, group, key)
        .filter(|v| !v.is_null())
        .and_then(Value::as_u64);
    let (value, source) = if let Some(n) = from_layer {
        (n, Source::Config)
    } else if let Some(n) = legacy {
        (n, Source::Legacy)
    } else {
        (default, Source::Default)
    };
    sources.insert(format!("{group}.{key}"), source);
    value
}

/// Resolve the effective vector for `root` down the full precedence chain,
/// recording per-field provenance. Pure read — no globals, no validation
/// (call [`validate_layer`] for the strict gate, [`startup_init`] for both).
///
/// Legacy-tier values reuse each module's own parser, so their documented
/// soft-fallback semantics (e.g. build-backoff's crossed-pair rejection)
/// carry over unchanged.
#[must_use]
pub fn resolve_effective(root: &Path) -> Resolved {
    let effective = config_resolver::resolve_effective_config(root);
    warn_if_retired_env_set();
    let block = overlay_from_effective(&effective);

    // Legacy tier, read through each module's own parser (no hyperparams
    // overlay — that is exactly the distinction this provenance reports).
    let wf_legacy = crate::work_finder::config::parse_effective(&effective);
    // The build-backoff pair only counts as legacy-supplied when its legacy
    // block exists at all — `parse` fills a missing block with defaults, and
    // a defaulted pair must report `Source::Default`, not `Legacy`.
    let bb_legacy = effective
        .get("autonomous")
        .and_then(|a| a.get("workFinder"))
        .and_then(|w| w.get("buildBackoff"))
        .filter(|block| !block.is_null())
        .map(|block| BuildBackoffConfig::parse(block).config);
    let (bb_high, bb_low) = match bb_legacy {
        Some(config) => (Some(config.high as u64), Some(config.low as u64)),
        None => (None, None),
    };
    let idle_legacy = crate::idle_exit::parse_effective(&effective);

    let mut sources = BTreeMap::new();
    let params = Hyperparameters {
        dispatch: DispatchParams {
            tick_interval_secs: pick_u64(
                &block,
                "dispatch",
                "tickIntervalSecs",
                wf_legacy.interval_secs,
                DEFAULT_WORK_FINDER_INTERVAL_SECS,
                &mut sources,
            ),
            max_concurrent: pick_u64(
                &block,
                "dispatch",
                "maxConcurrent",
                wf_legacy.max_concurrent.map(|n| n as u64),
                DEFAULT_WORK_FINDER_MAX_CONCURRENT as u64,
                &mut sources,
            ) as usize,
            max_admissions_per_tick: pick_u64(
                &block,
                "dispatch",
                "maxAdmissionsPerTick",
                wf_legacy.max_admissions_per_tick.map(|n| n as u64),
                DEFAULT_MAX_ADMISSIONS_PER_TICK as u64,
                &mut sources,
            ) as usize,
        },
        lifecycle: LifecycleParams {
            lease_ttl_minutes: {
                // No legacy config tier for the lease TTL (env-only until
                // this module) — the chain is block > default.
                let value = tier_value(&block, "lifecycle", "leaseTtlMinutes")
                    .filter(|v| !v.is_null())
                    .and_then(Value::as_f64)
                    .filter(|mins| *mins > 0.0);
                let source = if value.is_some() {
                    Source::Config
                } else {
                    Source::Default
                };
                sources.insert("lifecycle.leaseTtlMinutes".to_string(), source);
                value.unwrap_or(crate::claim_reconciliation::DEFAULT_LEASE_TTL_MINUTES)
            },
            idle_exit_minutes: pick_u64(
                &block,
                "lifecycle",
                "idleExitMinutes",
                idle_legacy.idle_minutes,
                crate::idle_exit::DEFAULT_IDLE_MINUTES,
                &mut sources,
            ),
        },
        rework: ReworkParams {
            build_backoff_high: pick_u64(
                &block,
                "rework",
                "buildBackoffHigh",
                bb_high,
                DEFAULT_HIGH as u64,
                &mut sources,
            ) as usize,
            build_backoff_low: pick_u64(
                &block,
                "rework",
                "buildBackoffLow",
                bb_low,
                DEFAULT_LOW as u64,
                &mut sources,
            ) as usize,
        },
        champion: {
            // The `LOOM_CHAMPION_*` single-knob env tier is what Champion's
            // shell actually consumes, so it outranks the block here
            // and reports `Source::Env`, naming the values the role really
            // ran under (#10753).
            let d = Hyperparameters::default().champion;
            let mut pick = |key: &str, default: usize| {
                let env_var = CHAMPION_KEYS
                    .iter()
                    .find(|(k, _, _)| *k == key)
                    .map(|e| e.1);
                if let Some(n) = env_var
                    .and_then(|var| std::env::var(var).ok())
                    .and_then(|raw| champion_env_value(&raw))
                {
                    sources.insert(format!("champion.{key}"), Source::Env);
                    return n as usize;
                }
                pick_u64(&block, "champion", key, None, default as u64, &mut sources) as usize
            };
            ChampionParams {
                pr_slice: pick("prSlice", d.pr_slice),
                promotion_slice: pick("promotionSlice", d.promotion_slice),
                tier2_cap: pick("tier2Cap", d.tier2_cap),
                tier3_cap: pick("tier3Cap", d.tier3_cap),
                tier3_backlog_cap: pick("tier3BacklogCap", d.tier3_backlog_cap),
            }
        },
    };
    Resolved { params, sources }
}

/// The vector captured at daemon startup, when [`startup_init`] has run.
/// CLI subcommands and tests that never start a daemon read `None`.
#[must_use]
pub fn resolved_global() -> Option<&'static Resolved> {
    RESOLVED.get()
}

/// The lease-freshness TTL from the hyperparameters layer, when the layer —
/// not the default — supplied it. Consumed by
/// `claim_reconciliation::resolve_lease_ttl_minutes` between its env tier and
/// its default. Startup-captured: reads the process global so a per-call
/// config re-read is unnecessary (documented restart-to-apply).
#[must_use]
pub fn lease_ttl_minutes_from_layer() -> Option<f64> {
    // Hot-applied (#9768): re-resolve the layer against the startup root on
    // every call, so a committed-block edit lands without a daemon
    // restart. Returns the layer value only when the layer itself supplies
    // one — a legacy `autonomous.*` value keeps this `None` (the lease TTL
    // has no legacy config tier), falling through to the caller's default.
    // `None` before `startup_init` has run (no root known).
    let root = ROOT.get()?;
    let layer = layer(root);
    layer
        .get("lifecycle")
        .and_then(|l| l.get("leaseTtlMinutes"))
        .filter(|v| !v.is_null())
        .and_then(Value::as_f64)
        .filter(|mins| *mins > 0.0)
}

/// Daemon-startup gate (Issue #9683): resolve the hyperparameters layer,
/// **fail fast** on any violation (naming every offending path), otherwise
/// capture the resolved vector into the process global the per-knob
/// resolvers read. Run once, early, in `daemon_service::run_daemon`.
///
/// # Errors
/// Aborts startup when the layer has any schema violation. A set
/// `$LOOM_HYPERPARAMS` only logs a warning.
pub fn startup_init(root: &Path) -> Result<()> {
    warn_if_retired_env_set();
    let layer = layer(root);
    let violations = validate_layer(&layer);
    if !violations.is_empty() {
        let listed: String = violations.iter().map(|v| format!("\n  - {v}")).collect();
        bail!(
            "hyperparams: {} invalid hyperparameter value(s) in `{}`:{listed}\n\
             Fix the named path(s) and restart the daemon.",
            violations.len(),
            root.join(".loom/config.json").display(),
        );
    }
    let resolved = resolve_effective(root);
    let fields: String = resolved
        .sources
        .iter()
        .map(|(path, source)| format!("{path}={}", source.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = RESOLVED.set(resolved);
    let _ = ROOT.set(root.to_path_buf());
    log::info!("hyperparams: ({fields})");
    Ok(())
}

// ============================================================================
// CLI surface: `loom-daemon hyperparams`
// ============================================================================

/// Print the resolved hyperparameter vector and its provenance —
/// the inspection surface optimizer loops (CMA-ES) use to confirm an
/// config actually took effect. With `--validate`, run the same
/// strict gate daemon startup runs (`startup_init`: unknown keys, types,
/// ranges, crossed pair) **without starting a daemon** — a config lint for a
/// proposed `.loom/config.json` edit (#9768).
#[derive(Debug, Args)]
pub struct HyperparamsArgs {
    /// Emit machine-readable JSON (`params`, `sources`) instead of
    /// a human-readable table. With `--validate`, emit the violations as a
    /// JSON array instead of prose lines.
    #[arg(long)]
    pub json: bool,

    /// Validate the hyperparameters layer the way daemon startup would, and
    /// exit non-zero naming every violation, without printing the vector.
    /// Combinable with `--json` for machine-readable violations.
    #[arg(long)]
    pub validate: bool,

    /// Workspace root whose config tiers to resolve. Defaults to the current
    /// directory.
    #[arg(value_name = "PATH", default_value = ".")]
    pub workspace: PathBuf,
}

impl HyperparamsArgs {
    /// The `--validate` gate: the same checks `startup_init` enforces at
    /// daemon startup, runnable against a workspace without booting one.
    fn run_validate(&self) -> Result<()> {
        warn_if_retired_env_set();
        let layer = layer(&self.workspace);
        let violations = validate_layer(&layer);
        if violations.is_empty() {
            if self.json {
                println!("{{\"ok\": true, \"violations\": []}}");
            } else {
                println!("hyperparams: OK — layer valid (workspace {})", self.workspace.display());
            }
            return Ok(());
        }
        let listed: String = violations.iter().map(|v| format!("\n  - {v}")).collect();
        if self.json {
            let rows: Vec<String> = violations
                .iter()
                .map(|v| serde_json::json!({"path": v.path, "problem": v.problem}).to_string())
                .collect();
            println!("[{}]", rows.join(","));
        }
        bail!(
            "hyperparams: {} invalid hyperparameter value(s) in `{}`:{listed}",
            violations.len(),
            self.workspace.join(".loom/config.json").display(),
        )
    }

    /// Run the `hyperparams` subcommand.
    ///
    /// # Errors
    /// Propagates config-resolution I/O failures; with `--validate`, returns
    /// an error naming every violation when the layer is invalid.
    pub fn run(&self) -> Result<()> {
        if self.validate {
            return self.run_validate();
        }
        let resolved = resolve_effective(&self.workspace);
        if self.json {
            let out = serde_json::json!({
                "params": resolved.params,
                "sources": resolved
                    .sources
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str()))
                    .collect::<BTreeMap<_, _>>(),
            });
            println!("{out}");
            return Ok(());
        }
        println!(
            "dispatch.tickIntervalSecs      = {:>6}  [{}]",
            resolved.params.dispatch.tick_interval_secs,
            resolved.sources["dispatch.tickIntervalSecs"].as_str()
        );
        println!(
            "dispatch.maxConcurrent         = {:>6}  [{}]",
            resolved.params.dispatch.max_concurrent,
            resolved.sources["dispatch.maxConcurrent"].as_str()
        );
        println!(
            "dispatch.maxAdmissionsPerTick  = {:>6}  [{}]",
            resolved.params.dispatch.max_admissions_per_tick,
            resolved.sources["dispatch.maxAdmissionsPerTick"].as_str()
        );
        println!(
            "lifecycle.leaseTtlMinutes      = {:>6}  [{}]",
            resolved.params.lifecycle.lease_ttl_minutes,
            resolved.sources["lifecycle.leaseTtlMinutes"].as_str()
        );
        println!(
            "lifecycle.idleExitMinutes      = {:>6}  [{}]",
            resolved.params.lifecycle.idle_exit_minutes,
            resolved.sources["lifecycle.idleExitMinutes"].as_str()
        );
        println!(
            "rework.buildBackoffHigh        = {:>6}  [{}]",
            resolved.params.rework.build_backoff_high,
            resolved.sources["rework.buildBackoffHigh"].as_str()
        );
        println!(
            "rework.buildBackoffLow         = {:>6}  [{}]",
            resolved.params.rework.build_backoff_low,
            resolved.sources["rework.buildBackoffLow"].as_str()
        );
        let c = &resolved.params.champion;
        for (key, value) in [
            ("prSlice", c.pr_slice),
            ("promotionSlice", c.promotion_slice),
            ("tier2Cap", c.tier2_cap),
            ("tier3Cap", c.tier3_cap),
            ("tier3BacklogCap", c.tier3_backlog_cap),
        ] {
            println!(
                "champion.{key:<23}= {value:>6}  [{}]",
                resolved.sources[&format!("champion.{key}")].as_str()
            );
        }
        Ok(())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[path = "hyperparams_tests.rs"]
mod tests;
