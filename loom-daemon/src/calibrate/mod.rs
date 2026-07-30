//! `loom-daemon calibrate` — measure the host and recommend (or, with
//! `--write`, apply) the autonomous concurrency knobs (issue #4390).
//!
//! # Motivation
//!
//! [`crate::work_finder::resolve_dynamic_max_concurrent`] bounds the
//! autonomous dispatch cap by `min(healthy_tokens × per_token, disk_headroom,
//! cpu_headroom, configured_max)`. The three live-measured terms
//! (`healthy_tokens`, `disk_headroom`, `cpu_headroom`) already scale with the
//! host; the fourth, `configured_max` (`autonomous.workFinder.maxConcurrent`),
//! does not — it is a static operator ceiling that defaults to
//! [`crate::work_finder::DEFAULT_WORK_FINDER_MAX_CONCURRENT`] (`3`) for every
//! consumer install, regardless of whether the host has 4 cores or 28. On a
//! large host the ceiling becomes the accidental binding term, silently
//! wasting the CPU/disk/token headroom the other three terms would otherwise
//! allow. Tuning this by hand (as commit `3fdfbf81` did for this repo's own
//! host) requires reading `loom-daemon status`'s cap breakdown, understanding
//! the four-term formula, and knowing which config key to edit — this module
//! automates that judgment call.
//!
//! # Two halves: pure policy, impure measurement
//!
//! [`recommend`] is a **pure** function over [`HostMeasurements`] — no I/O, no
//! live host probing — so the recommendation policy is exhaustively unit
//! tested against injected measurements (host classes: a large pilot host, a
//! small laptop, a disk-bound host, an unbootstrapped token pool, …).
//! [`measure`] is the **only** function in this module that touches the live
//! host (CPU idle sampling, `df`, the token-pool ranking file) — it builds a
//! [`HostMeasurements`] from real inputs, reusing the same measurement
//! primitives [`crate::work_finder`]'s dynamic cap and `loom-daemon status`
//! already use, so calibrate can never disagree with what dispatch actually
//! does.
//!
//! # Recommendation policy
//!
//! - **Ceiling** (`autonomous.workFinder.maxConcurrent`): recommend raising it
//!   to `cpu_headroom + slack` (never lowering it — see the fleet-safety note
//!   below) so the ceiling stops being the artificial binding term and the
//!   measured-CPU governor binds instead, mirroring `3fdfbf81`'s manual
//!   derivation. `slack` is [`CEILING_SLACK_FRACTION`] of `cpu_headroom`
//!   (floored at 1), so the new ceiling does not sit razor-thin against the
//!   CPU term (which would flip back to ceiling-bound on the next
//!   slightly-busier tick).
//! - **Per-token concurrency** (`autonomous.perTokenConcurrency`): only raised
//!   when the token axis (`healthy_accounts × per_token_concurrency`) is
//!   itself the smallest of the three *resource* terms (cpu/disk/the new
//!   ceiling) — i.e. only when tokens are the actual bottleneck, not
//!   proactively. Raised just enough to stop binding below that resource
//!   floor, not further.
//! - **Disk** has no config key at all — `LOOM_PER_WORKTREE_GB` is
//!   deliberately env-only (see [`crate::disk_headroom`] module docs, #4032
//!   decision) — so calibrate always names the env-var alternative rather
//!   than a config write.
//!
//! # Fleet-safety note
//!
//! Raising the committed `maxConcurrent` ceiling is safe fleet-wide precisely
//! because the dynamic cap keeps three other, independently-measured terms in
//! the `min(...)`: each host still binds on its **own** measured
//! cpu/tokens/disk regardless of how generous the shared ceiling is. A large
//! ceiling recommended for an 18-core pilot host does not let a 4-core laptop
//! over-dispatch — the laptop's own `cpu_headroom` term still clamps it.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Fraction of `cpu_headroom` added as slack when recommending a new ceiling,
/// so the recommendation does not sit razor-thin against the CPU term (a
/// ceiling equal to `cpu_headroom` would flip back to being the binding term
/// the moment the host gets slightly busier). `0.25` reproduces this repo's
/// own hand-derived `maxConcurrent=8` (commit `3fdfbf81`) from its measured
/// `cpu_headroom≈6` on the 18-core pilot host almost exactly (`6 + ceil(6 ×
/// 0.25) = 8`), which is why this constant was chosen over a flat `+N`.
pub const CEILING_SLACK_FRACTION: f64 = 0.25;

// ============================================================================
// Measurements — the inputs to the recommendation policy
// ============================================================================

/// The host measurements + currently-resolved config knobs the recommendation
/// policy ([`recommend`]) consumes. Every field is either a live measurement
/// ([`measure`] populates it from the host) or an already-resolved config
/// value (env > config > default, via [`crate::work_finder`]) — nothing in
/// this struct requires further resolution, so [`recommend`] can be a pure
/// function with no I/O, fully exercised by injected values in tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostMeasurements {
    /// Host logical CPU count ([`crate::cpu_headroom::logical_cpu_count`]).
    pub logical_cpus: usize,
    /// The CPU headroom term as currently computed
    /// ([`crate::cpu_headroom::cpu_headroom_limit`]) — concurrent-sweep
    /// slots the host's CPU can currently absorb.
    pub cpu_headroom: usize,
    /// The disk headroom term
    /// ([`crate::disk_headroom::disk_headroom_limit`]) — worktree slots the
    /// scratch volume can hold at the resolved per-worktree GB estimate.
    /// `usize::MAX` means the free-space probe was unmeasurable (skip the
    /// clamp — see [`crate::disk_headroom`] docs).
    pub disk_headroom: usize,
    /// Free GB on the scratch volume, when measurable (`None` when
    /// unmeasurable — see [`crate::disk_headroom::worktree_root_free_gb`]).
    pub disk_free_gb: Option<u64>,
    /// The resolved per-worktree GB estimate
    /// ([`crate::disk_headroom::per_worktree_gb`]) — env-only, no config key.
    pub per_worktree_gb: u64,
    /// Count of `available` (healthy) accounts from the ranking snapshot, or
    /// the raw pool size when no ranking data exists — mirrors
    /// [`crate::capacity::token_axis_limit`], the same source
    /// [`crate::work_finder`]'s own dispatch decision reads.
    pub token_axis_limit: usize,
    /// Whether [`Self::token_axis_limit`] came from a parsed ranking snapshot
    /// (`true`) or the unranked raw-pool-size fallback (`false` — no probe
    /// has ever run, or the pool is unbootstrapped).
    pub ranking_present: bool,
    /// Total accounts recorded in the ranking (`0` when
    /// [`Self::ranking_present`] is `false`).
    pub total_accounts: usize,
    /// The currently resolved `autonomous.workFinder.maxConcurrent` ceiling
    /// (env > config > default —
    /// [`crate::work_finder::resolve_max_concurrent_with_config`]).
    pub configured_max_concurrent: usize,
    /// Whether `LOOM_WORK_FINDER_MAX_CONCURRENT` is set in the environment —
    /// when `true`, a `--write` recommendation is masked at runtime until the
    /// env var is unset or updated (env always outranks config).
    pub max_concurrent_env_override: bool,
    /// The currently resolved `autonomous.perTokenConcurrency` factor (env >
    /// config > default —
    /// [`crate::work_finder::resolve_per_token_concurrency`]).
    pub configured_per_token_concurrency: usize,
    /// Whether `LOOM_PER_TOKEN_CONCURRENCY` is set in the environment (same
    /// env-outranks-config masking as [`Self::max_concurrent_env_override`]).
    pub per_token_concurrency_env_override: bool,
}

impl HostMeasurements {
    /// The token axis of the dynamic cap at the *current* configured
    /// per-token factor: `token_axis_limit × configured_per_token_concurrency`
    /// (floored at factor `1`, mirroring
    /// [`crate::work_finder::resolve_dynamic_max_concurrent`]).
    #[must_use]
    pub fn token_axis_effective(&self) -> usize {
        self.token_axis_limit
            .saturating_mul(self.configured_per_token_concurrency.max(1))
    }

    /// Render this struct as the `"measurements"` JSON object for `--json`
    /// output.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "logical_cpus": self.logical_cpus,
            "cpu_headroom": self.cpu_headroom,
            "disk_headroom": disk_headroom_json(self.disk_headroom),
            "disk_free_gb": self.disk_free_gb,
            "per_worktree_gb": self.per_worktree_gb,
            "token_axis_limit": self.token_axis_limit,
            "token_axis_effective": self.token_axis_effective(),
            "ranking_present": self.ranking_present,
            "total_accounts": self.total_accounts,
            "configured_max_concurrent": self.configured_max_concurrent,
            "max_concurrent_env_override": self.max_concurrent_env_override,
            "configured_per_token_concurrency": self.configured_per_token_concurrency,
            "per_token_concurrency_env_override": self.per_token_concurrency_env_override,
        })
    }
}

/// `usize::MAX` (the disk-unmeasurable / unbounded sentinel) does not survive
/// JSON's number type cleanly across every consumer — render it as the string
/// `"unbounded"` instead of a giant integer, matching how an operator should
/// read it.
fn disk_headroom_json(disk_headroom: usize) -> Value {
    if disk_headroom == usize::MAX {
        Value::String("unbounded".to_string())
    } else {
        serde_json::json!(disk_headroom)
    }
}

// ============================================================================
// Binding term — which of the four `min(...)` terms is smallest
// ============================================================================

/// Which of the four dynamic-cap terms is the binding (smallest) constraint.
/// Tie-break order mirrors `loom-daemon status`'s own diagnosis
/// (`resolve_capacity` / `token_bound`): token axis first, then disk, then
/// cpu, then the ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingTerm {
    /// The token axis (`healthy_accounts × per_token_concurrency`) is the
    /// smallest term.
    Token,
    /// Disk headroom is the smallest term.
    Disk,
    /// CPU headroom is the smallest term.
    Cpu,
    /// The operator-configured ceiling is the smallest term.
    Ceiling,
}

impl BindingTerm {
    /// The stable lowercase identifier used in `--json` output and log lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Disk => "disk",
            Self::Cpu => "cpu",
            Self::Ceiling => "ceiling",
        }
    }
}

/// Determine which of the four terms is smallest (binds), applying the same
/// tie-break order [`BindingTerm`] documents.
#[must_use]
fn binding_term(
    token_axis_effective: usize,
    disk_headroom: usize,
    cpu_headroom: usize,
    ceiling: usize,
) -> BindingTerm {
    let min_val = token_axis_effective
        .min(disk_headroom)
        .min(cpu_headroom)
        .min(ceiling);
    if token_axis_effective == min_val {
        BindingTerm::Token
    } else if disk_headroom == min_val {
        BindingTerm::Disk
    } else if cpu_headroom == min_val {
        BindingTerm::Cpu
    } else {
        BindingTerm::Ceiling
    }
}

// ============================================================================
// Recommendation — the pure policy
// ============================================================================

/// The recommended knob values, plus the reasoning an operator needs to trust
/// them: which term binds today, which term would bind after applying the
/// recommendation, and any warnings ([`recommend`] never silently rewrites a
/// knob it isn't confident about — see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct Recommendation {
    /// Recommended `autonomous.workFinder.maxConcurrent`. Never smaller than
    /// [`HostMeasurements::configured_max_concurrent`] — see the module-level
    /// fleet-safety note for why raising it is always safe and lowering it is
    /// never recommended.
    pub recommended_max_concurrent: usize,
    /// Whether [`Self::recommended_max_concurrent`] differs from the current
    /// configured value.
    pub max_concurrent_changed: bool,
    /// Recommended `autonomous.perTokenConcurrency`.
    pub recommended_per_token_concurrency: usize,
    /// Whether [`Self::recommended_per_token_concurrency`] differs from the
    /// current configured value.
    pub per_token_concurrency_changed: bool,
    /// Which term binds today, at the *current* configured knobs.
    pub binding_term_before: BindingTerm,
    /// Which term would bind after applying this recommendation.
    pub binding_term_after: BindingTerm,
    /// Advisory warnings — env overrides that would mask a `--write`, an
    /// unbootstrapped/unhealthy token pool, or a CPU term that looks clearly
    /// miscalibrated for the host class. Never fatal; `calibrate` still
    /// prints a recommendation alongside them.
    pub warnings: Vec<String>,
}

impl Recommendation {
    /// Render this struct as the `"recommendation"` JSON object for `--json`
    /// output.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "recommended_max_concurrent": self.recommended_max_concurrent,
            "max_concurrent_changed": self.max_concurrent_changed,
            "recommended_per_token_concurrency": self.recommended_per_token_concurrency,
            "per_token_concurrency_changed": self.per_token_concurrency_changed,
            "binding_term_before": self.binding_term_before.as_str(),
            "binding_term_after": self.binding_term_after.as_str(),
            "warnings": self.warnings,
        })
    }
}

/// Compute the recommended knob values from `m` — pure, no I/O. See the
/// module docs for the policy rationale.
#[must_use]
pub fn recommend(m: &HostMeasurements) -> Recommendation {
    let mut warnings = Vec::new();

    let token_axis_effective_before = m.token_axis_effective();
    let binding_term_before = binding_term(
        token_axis_effective_before,
        m.disk_headroom,
        m.cpu_headroom,
        m.configured_max_concurrent,
    );

    // --- Ceiling: raise (never lower) so the CPU term binds with slack. ---
    let slack = ((m.cpu_headroom as f64) * CEILING_SLACK_FRACTION)
        .ceil()
        .max(1.0) as usize;
    let cpu_target = m.cpu_headroom.saturating_add(slack);
    let recommended_max_concurrent = m.configured_max_concurrent.max(cpu_target);
    let max_concurrent_changed = recommended_max_concurrent != m.configured_max_concurrent;

    if max_concurrent_changed && m.max_concurrent_env_override {
        warnings.push(format!(
            "LOOM_WORK_FINDER_MAX_CONCURRENT is set in the environment and outranks any config \
             value calibrate writes (env > config > default) — the recommended maxConcurrent={} \
             will NOT take effect until that env var is unset or updated to match.",
            recommended_max_concurrent
        ));
    }

    // --- Per-token concurrency: only raised when tokens are the actual ---
    // --- resource bottleneck (smaller than cpu/disk/the new ceiling),   ---
    // --- and only just enough to stop binding below that floor.        ---
    let resource_floor = recommended_max_concurrent
        .min(m.cpu_headroom)
        .min(m.disk_headroom);
    let recommended_per_token_concurrency = if m.token_axis_limit == 0 {
        // No healthy accounts at all — no factor can help; see the
        // no-healthy-accounts warning below instead.
        m.configured_per_token_concurrency
    } else if token_axis_effective_before >= resource_floor {
        // Tokens are not the bottleneck today; leave the factor alone.
        m.configured_per_token_concurrency
    } else {
        let needed = (resource_floor as f64 / m.token_axis_limit as f64)
            .ceil()
            .max(1.0) as usize;
        needed.max(m.configured_per_token_concurrency)
    };
    let per_token_concurrency_changed =
        recommended_per_token_concurrency != m.configured_per_token_concurrency;

    if per_token_concurrency_changed && m.per_token_concurrency_env_override {
        warnings.push(format!(
            "LOOM_PER_TOKEN_CONCURRENCY is set in the environment and outranks any config value \
             calibrate writes (env > config > default) — the recommended \
             perTokenConcurrency={} will NOT take effect until that env var is unset or updated \
             to match.",
            recommended_per_token_concurrency
        ));
    }

    if m.token_axis_limit == 0 {
        warnings.push(
            "No healthy accounts in the token pool (unbootstrapped, or every account is \
             exhausted/rate-limited/blocked) — dispatch is capped at 0 regardless of \
             maxConcurrent/perTokenConcurrency until the pool has at least one healthy account \
             (`loom-tokens bootstrap` + `loom-tokens check --ranking`)."
                .to_string(),
        );
    }

    // Flag a CPU term that looks clearly miscalibrated for the host class,
    // rather than silently rewriting `cpuUtilizationTarget` /
    // `estCoresPerSweep` (module docs). A CPU term pinned at the hard floor
    // of 1 on a host with meaningfully more than one core is the signature of
    // an `estCoresPerSweep` set far above what this host's cores can supply.
    if m.logical_cpus >= 8 && m.cpu_headroom <= 1 {
        warnings.push(format!(
            "cpu_headroom is pinned at the floor (1) on a {}-core host — this usually means \
             autonomous.estCoresPerSweep (or LOOM_EST_CORES_PER_SWEEP) is set too high, or \
             autonomous.cpuUtilizationTarget too low, for this host class. calibrate does not \
             rewrite either automatically; review them before trusting the ceiling \
             recommendation above.",
            m.logical_cpus
        ));
    }

    let token_axis_effective_after = m
        .token_axis_limit
        .saturating_mul(recommended_per_token_concurrency.max(1));
    let binding_term_after = binding_term(
        token_axis_effective_after,
        m.disk_headroom,
        m.cpu_headroom,
        recommended_max_concurrent,
    );

    Recommendation {
        recommended_max_concurrent,
        max_concurrent_changed,
        recommended_per_token_concurrency,
        per_token_concurrency_changed,
        binding_term_before,
        binding_term_after,
        warnings,
    }
}

// ============================================================================
// measure() — the only impure function in this module
// ============================================================================

/// Measure the live host + resolve the currently-configured knobs for
/// `workspace_root`, building the [`HostMeasurements`] [`recommend`]
/// consumes. Reuses exactly the primitives
/// [`crate::work_finder::spawn_multi_work_finder_task`] and `loom-daemon
/// status` already use, so calibrate can never disagree with what dispatch
/// actually does.
///
/// **May block** (~1s on macOS, via [`crate::cpu_headroom::cpu_headroom_limit`]'s
/// `iostat` sample). Fine for a one-shot CLI invocation; callers on an async
/// runtime should run this through `spawn_blocking`.
#[must_use]
pub fn measure(workspace_root: &Path) -> HostMeasurements {
    let config = crate::work_finder::read_work_finder_config(workspace_root);

    let utilization_target = crate::work_finder::resolve_cpu_utilization_target(&config);
    let est_cores_per_sweep = crate::work_finder::resolve_cpu_est_cores_per_sweep(&config);
    let logical_cpus = crate::cpu_headroom::logical_cpu_count();
    let cpu_headroom =
        crate::cpu_headroom::cpu_headroom_limit(utilization_target, est_cores_per_sweep);

    let disk_free_gb = crate::disk_headroom::worktree_root_free_gb(workspace_root);
    let per_worktree_gb = crate::disk_headroom::per_worktree_gb();
    let disk_headroom = crate::disk_headroom::disk_headroom_limit(workspace_root);

    let pool_size = crate::tokens::token_pool_size(workspace_root);
    let ranking = crate::capacity::read_ranking(workspace_root);
    let (token_axis_limit, ranking_present, total_accounts) = match &ranking {
        Some(snap) => (snap.available, true, snap.total),
        None => (pool_size, false, 0),
    };

    let configured_max_concurrent = crate::work_finder::resolve_max_concurrent_with_config(&config);
    let max_concurrent_env_override =
        std::env::var(crate::work_finder::WORK_FINDER_MAX_CONCURRENT_ENV).is_ok();
    let configured_per_token_concurrency =
        crate::work_finder::resolve_per_token_concurrency(&config);
    let per_token_concurrency_env_override =
        std::env::var(crate::work_finder::PER_TOKEN_CONCURRENCY_ENV).is_ok();

    HostMeasurements {
        logical_cpus,
        cpu_headroom,
        disk_headroom,
        disk_free_gb,
        per_worktree_gb,
        token_axis_limit,
        ranking_present,
        total_accounts,
        configured_max_concurrent,
        max_concurrent_env_override,
        configured_per_token_concurrency,
        per_token_concurrency_env_override,
    }
}

// ============================================================================
// --write — merge the recommendation into .loom/config.json
// ============================================================================

/// Deep-merge `max_concurrent` / `per_token_concurrency` into `existing`
/// (a full parsed `.loom/config.json`), touching **only**
/// `autonomous.workFinder.maxConcurrent` and `autonomous.perTokenConcurrency`
/// — every other key at any depth (including sibling `autonomous.workFinder`
/// keys like `intervalSecs`/`enabled`, and sibling `autonomous` keys like
/// `cpuUtilizationTarget`/`roleRunner`) is preserved byte-for-byte. Pure
/// function (no I/O) so the merge is unit-tested directly, including
/// idempotency (merging the same values twice yields the same
/// [`Value`]) without touching a filesystem.
///
/// Reuses [`crate::init::deep_merge_existing_wins`] with the roles inverted
/// from `init`'s own usage: there, the existing **consumer** config wins over
/// the shipped template; here, the **new recommended values** (the overlay)
/// win over whatever `existing` already has at those two leaf paths — the
/// whole point of `--write` is to update them. The underlying merge semantics
/// (overlay wins on conflict, base's untouched keys survive, recurses into
/// objects) are identical; only which side is `base` and which is `overlay`
/// differs.
#[must_use]
pub fn merge_workfinder_values(
    existing: &Value,
    max_concurrent: usize,
    per_token_concurrency: usize,
) -> Value {
    let mut merged = if existing.is_object() {
        existing.clone()
    } else {
        serde_json::json!({})
    };
    let overlay = serde_json::json!({
        "autonomous": {
            "perTokenConcurrency": per_token_concurrency,
            "workFinder": {
                "maxConcurrent": max_concurrent,
            },
        },
    });
    crate::init::deep_merge_existing_wins(&mut merged, &overlay);
    merged
}

/// Read `<workspace_root>/.loom/config.json` (treating a missing file as
/// `{}`), merge in the recommended `maxConcurrent` / `perTokenConcurrency`
/// via [`merge_workfinder_values`], and write the result back with the same
/// canonical pretty-printed + trailing-newline formatting
/// [`crate::init::initialize_workspace`]'s own config merge uses — so a
/// repeat `--write` with the same recommendation is byte-identical (AC).
///
/// Refuses (returns `Err`) rather than clobbering when the existing file is
/// present but not a valid JSON object — `--write` must never silently
/// discard a consumer's config.
pub fn write_workfinder_config(
    workspace_root: &Path,
    max_concurrent: usize,
    per_token_concurrency: usize,
) -> Result<PathBuf, String> {
    let loom_dir = workspace_root.join(".loom");
    std::fs::create_dir_all(&loom_dir)
        .map_err(|e| format!("Failed to create {}: {e}", loom_dir.display()))?;
    let config_path = loom_dir.join("config.json");

    let existing: Value = if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read {}: {e}", config_path.display()))?;
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}) — refusing to write; fix or remove it first",
                config_path.display()
            )
        })?;
        if !parsed.is_object() {
            return Err(format!(
                "{} does not contain a JSON object at the top level — refusing to write",
                config_path.display()
            ));
        }
        parsed
    } else {
        serde_json::json!({})
    };

    let merged = merge_workfinder_values(&existing, max_concurrent, per_token_concurrency);
    let mut serialized = serde_json::to_string_pretty(&merged)
        .map_err(|e| format!("Failed to serialize merged config: {e}"))?;
    serialized.push('\n');
    std::fs::write(&config_path, serialized)
        .map_err(|e| format!("Failed to write {}: {e}", config_path.display()))?;
    Ok(config_path)
}

// ============================================================================
// Output rendering
// ============================================================================

/// Render the full `measurements` + `recommendation` (+ optional write
/// outcome) as the top-level `--json` payload.
#[must_use]
pub fn report_json(m: &HostMeasurements, r: &Recommendation, written: Option<&Path>) -> Value {
    serde_json::json!({
        "measurements": m.to_json(),
        "recommendation": r.to_json(),
        "write": {
            "applied": written.is_some(),
            "config_path": written.map(|p| p.display().to_string()),
        },
        "restart_required": true,
    })
}

/// Render the human-readable report — per-term measurements, the `min(...)`
/// breakdown before and after, and the recommendation, in the same format
/// `loom-daemon status` uses for its own cap breakdown
/// (`print_status_human`).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn report_human(m: &HostMeasurements, r: &Recommendation, written: Option<&Path>) -> String {
    let mut out = String::new();
    let factor = m.configured_per_token_concurrency.max(1);
    let token_axis_before = m.token_axis_effective();

    out.push_str("\n=== loom-daemon calibrate ===\n\n");

    out.push_str("Host measurements:\n");
    out.push_str(&format!("  logical cpus:   {}\n", m.logical_cpus));
    out.push_str(&format!("  cpu headroom:   {} concurrent-sweep slot(s)\n", m.cpu_headroom));
    match (m.disk_free_gb, m.disk_headroom) {
        (Some(free), headroom) if headroom != usize::MAX => {
            out.push_str(&format!(
                "  disk headroom:  {headroom} worktree slot(s) ({free} GB free / {} GB per-worktree estimate)\n",
                m.per_worktree_gb
            ));
        }
        _ => {
            out.push_str(
                "  disk headroom:  unmeasurable (skipping the disk clamp — treated as unbounded)\n",
            );
        }
    }
    out.push_str(
        "                  (LOOM_PER_WORKTREE_GB is env-only — there is no config key for the \
         disk estimate; override with `export LOOM_PER_WORKTREE_GB=<gb>`)\n",
    );
    if m.ranking_present {
        out.push_str(&format!(
            "  token pool:     {}/{} accounts healthy (from .loom/tokens/.ranking)\n",
            m.token_axis_limit, m.total_accounts
        ));
    } else {
        out.push_str(&format!(
            "  token pool:     no ranking — using raw pool size {} (run `loom-tokens check \
             --ranking` for a healthy-account count)\n",
            m.token_axis_limit
        ));
    }

    out.push_str("\nCurrently configured:\n");
    out.push_str(&format!(
        "  autonomous.workFinder.maxConcurrent = {}{}\n",
        m.configured_max_concurrent,
        if m.max_concurrent_env_override {
            " (from LOOM_WORK_FINDER_MAX_CONCURRENT)"
        } else {
            ""
        }
    ));
    out.push_str(&format!(
        "  autonomous.perTokenConcurrency      = {}{}\n",
        m.configured_per_token_concurrency,
        if m.per_token_concurrency_env_override {
            " (from LOOM_PER_TOKEN_CONCURRENCY)"
        } else {
            ""
        }
    ));
    out.push_str(&format!(
        "  = min(healthy {} × per-token {} = {}, disk {}, cpu {}, configured max {})\n",
        m.token_axis_limit,
        factor,
        token_axis_before,
        display_headroom(m.disk_headroom),
        m.cpu_headroom,
        m.configured_max_concurrent,
    ));
    out.push_str(&format!("  binds on: {}\n", r.binding_term_before.as_str()));

    out.push_str("\nRecommendation:\n");
    out.push_str(&format!(
        "  autonomous.workFinder.maxConcurrent -> {}{}\n",
        r.recommended_max_concurrent,
        if r.max_concurrent_changed {
            format!(" (was {})", m.configured_max_concurrent)
        } else {
            " (no change)".to_string()
        }
    ));
    out.push_str(&format!(
        "  autonomous.perTokenConcurrency      -> {}{}\n",
        r.recommended_per_token_concurrency,
        if r.per_token_concurrency_changed {
            format!(" (was {})", m.configured_per_token_concurrency)
        } else {
            " (no change)".to_string()
        }
    ));
    let token_axis_after = m
        .token_axis_limit
        .saturating_mul(r.recommended_per_token_concurrency.max(1));
    out.push_str(&format!(
        "  = min(healthy {} × per-token {} = {}, disk {}, cpu {}, configured max {})\n",
        m.token_axis_limit,
        r.recommended_per_token_concurrency.max(1),
        token_axis_after,
        display_headroom(m.disk_headroom),
        m.cpu_headroom,
        r.recommended_max_concurrent,
    ));
    out.push_str(&format!("  would bind on: {}\n", r.binding_term_after.as_str()));

    if !r.warnings.is_empty() {
        out.push('\n');
        for w in &r.warnings {
            out.push_str(&format!("WARNING: {w}\n"));
        }
    }

    out.push('\n');
    if let Some(path) = written {
        out.push_str(&format!("Wrote the recommendation above to {}.\n", path.display()));
    } else {
        out.push_str("Not writing changes (pass --write to apply this recommendation).\n");
    }
    out.push_str(
        "These knobs are read once at daemon startup, not re-read per tick — restart the \
         daemon for a change to take effect:\n  ./.loom/scripts/cli/loom-daemon-stop.sh && \
         ./.loom/scripts/cli/loom-daemon-start.sh\n",
    );

    out
}

fn display_headroom(headroom: usize) -> String {
    if headroom == usize::MAX {
        "unbounded".to_string()
    } else {
        headroom.to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ========================================================================
    // Test fixtures — host classes named in the #4390 test plan
    // ========================================================================

    /// 18-core / 8-token pilot host (this repo's own primary build host, the
    /// motivating case for commit `3fdfbf81`'s manual `maxConcurrent=8`).
    /// `cpu_headroom≈6` is the real measured figure for that host at the
    /// default `utilizationTarget=0.75`, `estCoresPerSweep=2.0` (module docs
    /// on [`crate::cpu_headroom::DEFAULT_EST_CORES_PER_SWEEP`]).
    fn pilot_host() -> HostMeasurements {
        HostMeasurements {
            logical_cpus: 18,
            cpu_headroom: 6,
            disk_headroom: 700,
            disk_free_gb: Some(1400),
            per_worktree_gb: 2,
            token_axis_limit: 8,
            ranking_present: true,
            total_accounts: 8,
            configured_max_concurrent: 3, // consumer default (#4390 premise correction 1)
            max_concurrent_env_override: false,
            configured_per_token_concurrency: 2, // DEFAULT_PER_TOKEN_CONCURRENCY
            per_token_concurrency_env_override: false,
        }
    }

    /// 4-core / 1-token laptop host — a small dev machine with only a single
    /// bootstrapped account.
    fn laptop_host() -> HostMeasurements {
        HostMeasurements {
            logical_cpus: 4,
            cpu_headroom: 1, // floored — 4 * 0.75 / 2.0 = 1.5 -> floor 1
            disk_headroom: 200,
            disk_free_gb: Some(400),
            per_worktree_gb: 2,
            token_axis_limit: 1,
            ranking_present: true,
            total_accounts: 1,
            configured_max_concurrent: 3,
            max_concurrent_env_override: false,
            configured_per_token_concurrency: 2,
            per_token_concurrency_env_override: false,
        }
    }

    /// A host with plenty of CPU and tokens but a nearly-full scratch volume —
    /// disk is the real binding constraint regardless of the other knobs.
    fn disk_bound_host() -> HostMeasurements {
        HostMeasurements {
            logical_cpus: 16,
            cpu_headroom: 10,
            disk_headroom: 3,
            disk_free_gb: Some(6),
            per_worktree_gb: 2,
            token_axis_limit: 8,
            ranking_present: true,
            total_accounts: 8,
            configured_max_concurrent: 3,
            max_concurrent_env_override: false,
            configured_per_token_concurrency: 2,
            per_token_concurrency_env_override: false,
        }
    }

    // ========================================================================
    // Ceiling recommendation — makes the cpu term bind, with slack
    // ========================================================================

    #[test]
    fn test_pilot_host_reproduces_hand_derived_ceiling_of_8() {
        let r = recommend(&pilot_host());
        assert_eq!(r.recommended_max_concurrent, 8, "6 + ceil(6*0.25)=2 -> 8, matching 3fdfbf81");
        assert!(r.max_concurrent_changed);
    }

    #[test]
    fn test_pilot_host_ceiling_binds_before_cpu_binds_after() {
        let r = recommend(&pilot_host());
        assert_eq!(r.binding_term_before, BindingTerm::Ceiling, "3 is the min(16, 700, 6, 3)");
        assert_eq!(r.binding_term_after, BindingTerm::Cpu, "6 is the min(16, 700, 6, 8)");
    }

    #[test]
    fn test_pilot_host_per_token_concurrency_unchanged() {
        // Token axis (16) already comfortably exceeds the cpu/disk/ceiling
        // floor (6) at the default factor of 2 — no reason to raise it.
        let r = recommend(&pilot_host());
        assert_eq!(r.recommended_per_token_concurrency, 2);
        assert!(!r.per_token_concurrency_changed);
    }

    #[test]
    fn test_laptop_host_already_correctly_sized_says_no_change() {
        // cpu_target = 1 + ceil(1*0.25) = 2; current configured_max (3, the
        // shipped default) is already >= that -> no change recommended.
        let laptop = laptop_host();
        let r = recommend(&laptop);
        assert_eq!(r.recommended_max_concurrent, laptop.configured_max_concurrent);
        assert!(
            !r.max_concurrent_changed,
            "an already-correctly-sized ceiling must say no change"
        );
    }

    #[test]
    fn test_already_correctly_sized_ceiling_no_change_pilot_class() {
        // Same pilot host, but the ceiling has ALREADY been hand-tuned to the
        // recommended value (8) -> must not recommend a further change.
        let mut m = pilot_host();
        m.configured_max_concurrent = 8;
        let r = recommend(&m);
        assert_eq!(r.recommended_max_concurrent, 8);
        assert!(!r.max_concurrent_changed);
    }

    #[test]
    fn test_recommendation_never_lowers_the_ceiling() {
        // An operator-set ceiling far above any measured headroom must never
        // be recommended DOWNWARD (module docs: raising is always safe,
        // lowering is never recommended — the other terms already clamp).
        let mut m = pilot_host();
        m.configured_max_concurrent = 100;
        let r = recommend(&m);
        assert_eq!(r.recommended_max_concurrent, 100);
        assert!(!r.max_concurrent_changed);
    }

    // ========================================================================
    // Disk-bound host
    // ========================================================================

    #[test]
    fn test_disk_bound_host_binds_on_disk_regardless_of_ceiling_recommendation() {
        let r = recommend(&disk_bound_host());
        // Raising the ceiling never fixes a disk-bound host: disk (3) is
        // still the smallest term after the recommendation (min(16, 3, 10,
        // 13)) — no config key exists that could change this.
        assert_eq!(r.binding_term_after, BindingTerm::Disk);
    }

    #[test]
    fn test_disk_bound_host_has_no_config_key_for_disk() {
        // There is no assertion target on Recommendation for this (disk has
        // no config key at all — module docs) but the measurement round-trips
        // for the render layer to surface the env-var alternative.
        let m = disk_bound_host();
        assert_eq!(m.per_worktree_gb, 2);
    }

    // ========================================================================
    // Unbootstrapped / unhealthy token pool
    // ========================================================================

    #[test]
    fn test_unbootstrapped_token_pool_falls_back_gracefully() {
        let mut m = pilot_host();
        m.token_axis_limit = 0;
        m.ranking_present = false;
        m.total_accounts = 0;
        let r = recommend(&m);
        // No factor helps a 0-healthy pool; must not panic or divide by zero.
        assert_eq!(r.recommended_per_token_concurrency, m.configured_per_token_concurrency);
        assert!(!r.per_token_concurrency_changed);
        assert!(r.warnings.iter().any(|w| w.contains("No healthy accounts")));
    }

    #[test]
    fn test_missing_ranking_uses_raw_pool_size_without_warning() {
        // ranking_present=false but token_axis_limit > 0 (the raw-pool-size
        // fallback, e.g. no `loom-tokens check --ranking` has ever run) is a
        // normal, non-alarming state — no "no healthy accounts" warning.
        let mut m = pilot_host();
        m.ranking_present = false;
        m.total_accounts = 0;
        // token_axis_limit stays 8 (the raw pool size fallback).
        let r = recommend(&m);
        assert!(!r.warnings.iter().any(|w| w.contains("No healthy accounts")));
    }

    // ========================================================================
    // Per-token concurrency raised when tokens ARE the bottleneck
    // ========================================================================

    #[test]
    fn test_per_token_concurrency_raised_when_tokens_are_the_bottleneck() {
        // 2 healthy accounts, plenty of cpu/disk headroom and a raised
        // ceiling target of 8 -> token axis at factor 2 (4) is below the
        // resource floor (6, the cpu term) -> raise the factor to close the
        // gap: ceil(6/2) = 3.
        let mut m = pilot_host();
        m.token_axis_limit = 2;
        m.total_accounts = 2;
        let r = recommend(&m);
        assert_eq!(r.recommended_per_token_concurrency, 3);
        assert!(r.per_token_concurrency_changed);
    }

    // ========================================================================
    // Env overrides — warn that they mask a --write recommendation
    // ========================================================================

    #[test]
    fn test_env_max_concurrent_override_warns_when_recommendation_changes() {
        let mut m = pilot_host();
        m.max_concurrent_env_override = true;
        let r = recommend(&m);
        assert!(r.max_concurrent_changed);
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("LOOM_WORK_FINDER_MAX_CONCURRENT")));
    }

    #[test]
    fn test_env_max_concurrent_override_no_warning_when_already_correct() {
        // The env override is present, but the recommendation matches the
        // current value anyway -> nothing would actually be masked.
        let mut m = pilot_host();
        m.configured_max_concurrent = 8;
        m.max_concurrent_env_override = true;
        let r = recommend(&m);
        assert!(!r.max_concurrent_changed);
        assert!(!r
            .warnings
            .iter()
            .any(|w| w.contains("LOOM_WORK_FINDER_MAX_CONCURRENT")));
    }

    // ========================================================================
    // cpuUtilizationTarget / estCoresPerSweep miscalibration hint
    // ========================================================================

    #[test]
    fn test_flags_cpu_pinned_at_floor_on_large_host() {
        let mut m = pilot_host();
        m.logical_cpus = 16;
        m.cpu_headroom = 1;
        let r = recommend(&m);
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("estCoresPerSweep") || w.contains("cpuUtilizationTarget")));
    }

    #[test]
    fn test_does_not_flag_cpu_floor_on_a_genuinely_small_host() {
        // The laptop's cpu_headroom=1 is expected/normal for 4 cores — must
        // not warn.
        let r = recommend(&laptop_host());
        assert!(!r.warnings.iter().any(|w| w.contains("estCoresPerSweep")));
    }

    // ========================================================================
    // JSON round-trip
    // ========================================================================

    #[test]
    fn test_json_round_trips_measurements_and_recommendation() {
        let m = pilot_host();
        let r = recommend(&m);
        let report = report_json(&m, &r, None);

        assert_eq!(report["measurements"]["logical_cpus"], 18);
        assert_eq!(report["measurements"]["cpu_headroom"], 6);
        assert_eq!(report["measurements"]["configured_max_concurrent"], 3);
        assert_eq!(report["recommendation"]["recommended_max_concurrent"], 8);
        assert_eq!(report["recommendation"]["binding_term_before"], "ceiling");
        assert_eq!(report["recommendation"]["binding_term_after"], "cpu");
        assert_eq!(report["write"]["applied"], false);
        assert_eq!(report["restart_required"], true);

        // Round-trips through a full serialize/deserialize cycle without loss.
        let text = serde_json::to_string(&report).unwrap();
        let reparsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed, report);
    }

    #[test]
    fn test_json_reports_write_applied_when_given_a_path() {
        let m = pilot_host();
        let r = recommend(&m);
        let path = PathBuf::from("/tmp/example/.loom/config.json");
        let report = report_json(&m, &r, Some(&path));
        assert_eq!(report["write"]["applied"], true);
        assert_eq!(report["write"]["config_path"], "/tmp/example/.loom/config.json");
    }

    #[test]
    fn test_disk_headroom_unbounded_renders_as_string_not_a_giant_int() {
        let mut m = pilot_host();
        m.disk_headroom = usize::MAX;
        m.disk_free_gb = None;
        let json = m.to_json();
        assert_eq!(json["disk_headroom"], "unbounded");
    }

    // ========================================================================
    // Human report — smoke test (exact wording covered by other tests; this
    // just guards against a panic and checks a few must-have substrings).
    // ========================================================================

    #[test]
    fn test_human_report_contains_key_lines() {
        let m = pilot_host();
        let r = recommend(&m);
        let text = report_human(&m, &r, None);
        assert!(text.contains("loom-daemon calibrate"));
        assert!(text.contains("maxConcurrent -> 8"));
        assert!(text.contains("LOOM_PER_WORKTREE_GB"));
        assert!(text.contains("Not writing changes"));
        assert!(text.contains("loom-daemon-start.sh"));
    }

    #[test]
    fn test_human_report_names_the_written_path() {
        let m = pilot_host();
        let r = recommend(&m);
        let path = PathBuf::from("/tmp/example/.loom/config.json");
        let text = report_human(&m, &r, Some(&path));
        assert!(text.contains("Wrote the recommendation above to /tmp/example/.loom/config.json"));
    }

    // ========================================================================
    // --write config merge — scoping + idempotency
    // ========================================================================

    #[test]
    fn test_merge_touches_only_workfinder_keys() {
        let existing = serde_json::json!({
            "autonomous": {
                "cpuUtilizationTarget": 0.6,
                "estCoresPerSweep": 3.5,
                "perTokenConcurrency": 2,
                "workFinder": {
                    "enabled": true,
                    "intervalSecs": 90,
                    "maxConcurrent": 3,
                    "maxAdmissionsPerTick": 4,
                },
                "roleRunner": { "enabled": true },
            },
            "unrelatedTopLevel": "preserved",
        });

        let merged = merge_workfinder_values(&existing, 8, 4);

        // The two touched keys changed.
        assert_eq!(merged["autonomous"]["workFinder"]["maxConcurrent"], 8);
        assert_eq!(merged["autonomous"]["perTokenConcurrency"], 4);

        // Every other key at every depth is untouched.
        assert_eq!(merged["autonomous"]["cpuUtilizationTarget"], 0.6);
        assert_eq!(merged["autonomous"]["estCoresPerSweep"], 3.5);
        assert_eq!(merged["autonomous"]["workFinder"]["enabled"], true);
        assert_eq!(merged["autonomous"]["workFinder"]["intervalSecs"], 90);
        assert_eq!(merged["autonomous"]["workFinder"]["maxAdmissionsPerTick"], 4);
        assert_eq!(merged["autonomous"]["roleRunner"]["enabled"], true);
        assert_eq!(merged["unrelatedTopLevel"], "preserved");
    }

    #[test]
    fn test_merge_is_idempotent_on_repeat_application() {
        let existing = serde_json::json!({
            "autonomous": { "workFinder": { "maxConcurrent": 3 } },
        });
        let once = merge_workfinder_values(&existing, 8, 4);
        let twice = merge_workfinder_values(&once, 8, 4);
        assert_eq!(once, twice, "applying the same recommendation twice must be a no-op diff");
    }

    #[test]
    fn test_merge_from_empty_config_creates_the_nested_path() {
        let merged = merge_workfinder_values(&serde_json::json!({}), 8, 4);
        assert_eq!(merged["autonomous"]["workFinder"]["maxConcurrent"], 8);
        assert_eq!(merged["autonomous"]["perTokenConcurrency"], 4);
    }

    #[test]
    fn test_merge_from_non_object_falls_back_to_empty_base() {
        // A malformed "existing" value (e.g. a JSON array) must not panic or
        // propagate garbage — treated as an empty base.
        let merged = merge_workfinder_values(&serde_json::json!([1, 2, 3]), 8, 4);
        assert_eq!(merged["autonomous"]["workFinder"]["maxConcurrent"], 8);
    }

    // ========================================================================
    // write_workfinder_config — filesystem round-trip + idempotency (AC)
    // ========================================================================

    #[test]
    fn test_write_workfinder_config_creates_file_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_workfinder_config(dir.path(), 8, 4).unwrap();
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(parsed["autonomous"]["workFinder"]["maxConcurrent"], 8);
        assert_eq!(parsed["autonomous"]["perTokenConcurrency"], 4);
    }

    #[test]
    fn test_write_workfinder_config_is_byte_idempotent_on_second_run() {
        let dir = tempfile::tempdir().unwrap();
        let path1 = write_workfinder_config(dir.path(), 8, 4).unwrap();
        let bytes1 = std::fs::read(&path1).unwrap();
        let path2 = write_workfinder_config(dir.path(), 8, 4).unwrap();
        let bytes2 = std::fs::read(&path2).unwrap();
        assert_eq!(
            bytes1, bytes2,
            "a repeat --write with the same recommendation must be byte-identical"
        );
    }

    #[test]
    fn test_write_workfinder_config_preserves_unrelated_consumer_keys() {
        let dir = tempfile::tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        std::fs::create_dir_all(&loom_dir).unwrap();
        std::fs::write(
            loom_dir.join("config.json"),
            r#"{"worktree": {"root": "/custom/scratch"}, "autonomous": {"workFinder": {"maxConcurrent": 3}}}"#,
        )
        .unwrap();

        let path = write_workfinder_config(dir.path(), 8, 4).unwrap();
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["worktree"]["root"], "/custom/scratch");
        assert_eq!(parsed["autonomous"]["workFinder"]["maxConcurrent"], 8);
    }

    #[test]
    fn test_write_workfinder_config_refuses_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let loom_dir = dir.path().join(".loom");
        std::fs::create_dir_all(&loom_dir).unwrap();
        std::fs::write(loom_dir.join("config.json"), "not valid json").unwrap();

        let result = write_workfinder_config(dir.path(), 8, 4);
        assert!(result.is_err());
        // The original invalid content must survive untouched.
        let contents = std::fs::read_to_string(loom_dir.join("config.json")).unwrap();
        assert_eq!(contents, "not valid json");
    }

    // ========================================================================
    // measure() smoke test — the only impure function; just guards against a
    // panic against a real (empty) temp workspace. No live-probe assertions
    // (host-dependent), per the #4390 test plan's "no live host probing" for
    // the policy tests above.
    // ========================================================================

    #[test]
    fn test_measure_does_not_panic_on_an_empty_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let m = measure(dir.path());
        assert!(m.logical_cpus >= 1);
        assert!(m.cpu_headroom >= 1);
    }
}
