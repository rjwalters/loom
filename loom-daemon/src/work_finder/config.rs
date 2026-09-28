//! `.loom/config.json` → `autonomous.workFinder` parsing: [`WorkFinderConfig`],
//! [`read_work_finder_config`], and the retired-CPU-knob deprecation notice.
//!
//! Split out of `work_finder.rs` (Issue #9034) for the same file-size-ratchet
//! reason as [`super::repo_cap`] / [`super::configured_max`]: `work_finder.rs`
//! is frozen at its current size (`.loom/docs/file-size-policy.md`), and this
//! issue's `hostClass` / `allowHeavyLocal` fields needed room to land. Pure
//! move plus two new fields — no other behavior change.

use std::path::Path;

use super::{host_class, repo_cap};

/// The subset of `.loom/config.json → autonomous.workFinder` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — the precedence is **env > config >
/// default** for every knob.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkFinderConfig {
    /// `autonomous.workFinder.enabled` — whether to run the loop at all.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.intervalSecs` — tick interval in seconds
    /// (a zero/invalid value is dropped to `None`).
    pub interval_secs: Option<u64>,
    /// `autonomous.workFinder.maxConcurrent` — the operator concurrency ceiling
    /// (a zero/invalid value is dropped to `None`).
    pub max_concurrent: Option<usize>,
    /// `autonomous.workFinder.maxAdmissionsPerTick` — the per-tick ramp
    /// admission cap (#4234; a zero/invalid value is dropped to `None`). See
    /// [`super::WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV`] for the full rationale.
    pub max_admissions_per_tick: Option<usize>,
    /// `autonomous.workFinder.maxConcurrentPerRepo` — how many of the shared
    /// `maxConcurrent` budget's slots ONE repo may hold (#9090; a zero/invalid
    /// value is dropped to `None`). Unlike every other knob here, `None` means
    /// **uncapped** rather than "use a built-in default": a non-`None` default
    /// would silently throttle every fleet on upgrade. See [`repo_cap`].
    pub max_concurrent_per_repo: Option<usize>,
    /// `autonomous.workFinder.extraSkipLabels` — additional label names
    /// (Issue #6685) beyond the hardcoded [`super::PARK_LABELS`] this workspace
    /// wants the work-finder to treat as a skip/park signal (e.g. a
    /// repo-local `blocked-upstream` label). `None` when the key is absent —
    /// distinct from `Some(vec![])`, which an explicit `[]` in config would
    /// produce, though both resolve to the same empty-list default behavior.
    pub extra_skip_labels: Option<Vec<String>>,
    /// `autonomous.workFinder.hostClass` — this host's declared class (Issue
    /// #9034): `"local-dev"` | `"remote-worker"`; any other value, or the key
    /// absent, is `None` here (unclassified). See [`host_class`].
    pub host_class: Option<host_class::HostClass>,
    /// `autonomous.workFinder.allowHeavyLocal` — an operator override (Issue
    /// #9034) that suppresses the `host_class`/`loom:heavy` gate for this
    /// workspace's autonomous loop. `None` when the key is absent (falls
    /// through to the env var, then to `false`). See [`host_class`].
    pub allow_heavy_local: Option<bool>,
    /// Names of **retired** config keys found in `autonomous` — currently
    /// `cpuUtilizationTarget` / `estCoresPerSweep` ([`DEPRECATED_CPU_CONFIG_KEYS`]),
    /// whose CPU-headroom admission term #4512 deleted.
    ///
    /// They are **accepted-but-ignored**, never a config error: a fleet's
    /// committed `.loom/config.json` must keep parsing across the upgrade. Their
    /// presence (at any value — no range filtering, since nothing consumes the
    /// value) is recorded here purely so
    /// [`warn_deprecated_cpu_knobs`] can log one deprecation line naming exactly
    /// which keys to delete.
    pub deprecated_cpu_keys: Vec<&'static str>,
}

/// Read `.loom/config.json → autonomous.workFinder`, soft-failing every field
/// to `None` (env/default resolution) on any of: missing file, malformed JSON,
/// or a missing `autonomous` / `workFinder` block.
///
/// Mirrors the soft-fail contract of
/// [`crate::main_health_gate::read_build_gate_config`] — a repo with no
/// `autonomous` block gets zero behavior change (env-only, exactly like today).
/// A zero or non-integer `intervalSecs` / `maxConcurrent` is treated as absent
/// so it falls through to the built-in default rather than a useless value.
#[must_use]
pub fn read_work_finder_config(repo_root: &Path) -> WorkFinderConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(autonomous) = crate::config_resolver::get_path(&effective, "autonomous") else {
        return WorkFinderConfig::default();
    };

    // `cpuUtilizationTarget` / `estCoresPerSweep` used to live at the
    // `autonomous` level too (#4032), feeding the CPU-headroom admission term
    // #4512 deleted. They are now accepted-but-ignored: note their presence for
    // the one-shot deprecation warning and parse nothing — no range filtering,
    // no type coercion, because no consumer reads the value any more. A
    // consumer's committed config keeps parsing unchanged (never a hard error).
    let deprecated_cpu_keys: Vec<&'static str> = DEPRECATED_CPU_CONFIG_KEYS
        .iter()
        .copied()
        .filter(|key| autonomous.get(*key).is_some_and(|v| !v.is_null()))
        .collect();

    // The `workFinder` sub-block is optional; each field independently falls
    // through to `None` (env/default resolution) when absent.
    let wf = autonomous.get("workFinder");

    WorkFinderConfig {
        enabled: wf
            .and_then(|w| w.get("enabled"))
            .and_then(serde_json::Value::as_bool),
        interval_secs: wf
            .and_then(|w| w.get("intervalSecs"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        max_concurrent: wf
            .and_then(|w| w.get("maxConcurrent"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| usize::try_from(n).ok()),
        max_admissions_per_tick: wf
            .and_then(|w| w.get("maxAdmissionsPerTick"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0)
            .and_then(|n| usize::try_from(n).ok()),
        max_concurrent_per_repo: repo_cap::parse_config(wf),
        extra_skip_labels: wf.and_then(|w| w.get("extraSkipLabels")).and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
        }),
        host_class: host_class::parse_config(wf),
        allow_heavy_local: host_class::parse_config_allow_heavy_local(wf),
        deprecated_cpu_keys,
    }
}

/// Retired `autonomous.*` config keys, accepted-but-ignored since #4512 (they
/// fed the deleted CPU-headroom admission term, #3978/#4031).
pub const DEPRECATED_CPU_CONFIG_KEYS: [&str; 2] = ["cpuUtilizationTarget", "estCoresPerSweep"];

/// Retired env vars, accepted-but-ignored since #4512 — the env half of
/// [`DEPRECATED_CPU_CONFIG_KEYS`].
pub const DEPRECATED_CPU_ENV_VARS: [&str; 2] =
    ["LOOM_CPU_UTILIZATION_TARGET", "LOOM_EST_CORES_PER_SWEEP"];

/// One-shot guard so the deprecation warning is logged **once per process**, not
/// once per config read (the config is re-read on several paths, including every
/// `status` request).
static DEPRECATION_WARNED: std::sync::Once = std::sync::Once::new();

/// Render the deprecation notice for any retired CPU-headroom knob still set in
/// `config` or the environment — `None` when none is set (#4512).
///
/// Split out from [`warn_deprecated_cpu_knobs`] because the two channels an
/// operator actually watches are different processes: the **daemon** has a
/// logger (`~/.loom/daemon.log`) and warns through it, while a **CLI**
/// subcommand (`loom-daemon calibrate`) returns from `main` *before*
/// `setup_logging()` runs, so a `log::warn!` there is a silent no-op. The CLI
/// therefore prints this same string to stderr instead of relying on the log
/// (see `handle_calibrate_command`). One message, two delivery paths — never a
/// warning that exists only in a file nobody is tailing.
#[must_use]
pub fn deprecated_cpu_knob_notice(config: &WorkFinderConfig) -> Option<String> {
    let env_set: Vec<&str> = DEPRECATED_CPU_ENV_VARS
        .iter()
        .copied()
        .filter(|v| std::env::var_os(v).is_some())
        .collect();
    if config.deprecated_cpu_keys.is_empty() && env_set.is_empty() {
        return None;
    }
    let mut sources = Vec::new();
    if !config.deprecated_cpu_keys.is_empty() {
        sources.push(format!("config `autonomous.{{{}}}`", config.deprecated_cpu_keys.join(", ")));
    }
    if !env_set.is_empty() {
        sources.push(format!("env {}", env_set.join(", ")));
    }
    Some(format!(
        "{} set but IGNORED — #4512 removed the CPU-headroom term from the admission formula \
         (now min(token axis, disk headroom, maxConcurrent)). Tune \
         `autonomous.workFinder.maxConcurrent` for this machine instead; heavy build/test stages \
         are serialized by the machine-wide build slot (LOOM_BUILD_SLOTS), and the host-distress \
         breaker remains the load safety net. Delete the setting(s) to silence this warning.",
        sources.join(" and ")
    ))
}

/// Log a single deprecation warning naming any retired CPU-headroom knob still
/// set in config or the environment (#4512).
///
/// Accepted-but-ignored is a deliberate compatibility contract: a fleet upgrades
/// the daemon binary before it edits every repo's committed `.loom/config.json`,
/// so a stale key must **never** be a parse error — it must be a *visible*
/// no-op. Called once at daemon startup, it is internally idempotent via
/// [`std::sync::Once`], so extra call sites are free. CLI subcommands print
/// [`deprecated_cpu_knob_notice`] to stderr instead (no logger is initialized on
/// that path).
pub fn warn_deprecated_cpu_knobs(config: &WorkFinderConfig) {
    let Some(notice) = deprecated_cpu_knob_notice(config) else {
        return;
    };
    DEPRECATION_WARNED.call_once(|| {
        log::warn!("work_finder: {notice}");
    });
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
