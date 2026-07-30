//! `loom-daemon calibrate` — **measure** the host and the currently-resolved
//! concurrency knobs, and name which admission term binds (issue #4390,
//! reduced to measurement-only in #4512).
//!
//! # What this is, after #4512
//!
//! Originally (#4390) this subcommand also *recommended* — and with `--write`
//! applied — new knob values, deriving a `maxConcurrent` ceiling from the CPU
//! headroom term (`cpu_headroom + 25% slack`). #4512 deleted that term from the
//! admission formula: it priced every sweep as a build and throttled the
//! API-wait-dominated majority (an 8-core worker measured **95% idle** was
//! capped at 2). With the CPU estimate gone there is nothing left to derive a
//! recommendation *from* — `autonomous.workFinder.maxConcurrent` is now the one
//! per-machine knob, **tuned empirically by the operator** from observed
//! behavior.
//!
//! So calibrate is now strictly a **measurement report**: it prints what the
//! host looks like right now, what the knobs currently resolve to, the
//! `min(token axis, disk headroom, maxConcurrent)` breakdown, and which term
//! binds. Reading it is how an operator decides whether to raise the knob:
//!
//! - **binds on `ceiling` while the host sits idle** ⇒ raise `maxConcurrent`.
//! - **binds on `ceiling` while the host is saturated** (low idle fraction, or
//!   the host breaker tripping) ⇒ the knob is already at/above what this machine
//!   sustains; leave it or lower it.
//! - **binds on `token`/`disk`** ⇒ raising `maxConcurrent` changes nothing; add
//!   accounts or free scratch space.
//!
//! It never writes config. `--write` is accepted-but-ignored with a deprecation
//! warning so an existing `loom-daemon calibrate --write` invocation in a
//! script does not start failing.
//!
//! # Two halves: pure rendering, impure measurement
//!
//! [`measure`] is the **only** function here that touches the live host (CPU
//! idle sampling, `df`, the token-pool ranking file) — it reuses exactly the
//! primitives [`crate::work_finder`]'s dynamic cap and `loom-daemon status`
//! already use, so calibrate can never disagree with what dispatch actually
//! does. Everything else ([`binding_term`], [`report_json`], [`report_human`])
//! is pure over [`HostMeasurements`] and unit-tested against injected host
//! classes.

use std::path::Path;

use serde_json::Value;

// ============================================================================
// Measurements — the report's inputs
// ============================================================================

/// The host measurements + currently-resolved config knobs the report renders.
/// Every field is either a live measurement ([`measure`] populates it from the
/// host) or an already-resolved config value (env > config > default, via
/// [`crate::work_finder`]) — nothing here requires further resolution, so the
/// rendering functions are pure and fully exercised by injected values.
#[derive(Debug, Clone, PartialEq)]
pub struct HostMeasurements {
    /// Host logical CPU count ([`crate::cpu_headroom::logical_cpu_count`]).
    pub logical_cpus: usize,
    /// Measured CPU idle fraction (`0.0..=1.0`), or `None` when no sample has
    /// been taken on this platform yet
    /// ([`crate::cpu_headroom::cached_cpu_idle_fraction`]). **Observational**
    /// since #4512 — this is the evidence for tuning `maxConcurrent`, not an
    /// input to the cap.
    pub cpu_idle_fraction: Option<f64>,
    /// Current 1-minute load average, or `None` on a platform with no source
    /// ([`crate::cpu_headroom::read_loadavg_1m`]). The load-**per-core** ratio
    /// derived from it is what the host-distress breaker (#4235) trips on.
    pub loadavg_1m: Option<f64>,
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
    /// The currently resolved `autonomous.workFinder.maxConcurrent` — **the**
    /// per-machine admission knob (env > config > default —
    /// [`crate::work_finder::resolve_max_concurrent_with_config`]).
    pub configured_max_concurrent: usize,
    /// Whether `LOOM_WORK_FINDER_MAX_CONCURRENT` is set in the environment —
    /// when `true`, editing `.loom/config.json` has no effect until the env var
    /// is unset or updated (env always outranks config).
    pub max_concurrent_env_override: bool,
    /// **Which config tier file** actually set
    /// `autonomous.workFinder.maxConcurrent`
    /// ([`crate::config_resolver::source_of`]), or `None` when no tier does (the
    /// value is then the env override or the built-in default).
    ///
    /// Reported because the effective config is a *merge* of up to four files and
    /// the daemon manages several workspaces, so "which `.loom/config.json`
    /// supplied that number?" is a genuine operator question — #4512's live
    /// tuning session lost time to a host where the answer turned out to be a
    /// *different repo's* config than the one being edited. Naming the file makes
    /// the one knob that now carries the whole admission policy unambiguous.
    pub max_concurrent_source: Option<std::path::PathBuf>,
    /// The currently resolved `autonomous.perTokenConcurrency` factor (env >
    /// config > default — [`crate::work_finder::resolve_per_token_concurrency`]).
    pub configured_per_token_concurrency: usize,
    /// Whether `LOOM_PER_TOKEN_CONCURRENCY` is set in the environment (same
    /// env-outranks-config note as [`Self::max_concurrent_env_override`]).
    pub per_token_concurrency_env_override: bool,
    /// How many machine-wide build slots serialize the designated high-CPU
    /// stages ([`crate::build_slot::resolve_slots`], `0` = disabled). Reported
    /// because this — not an admission-time CPU estimate — is what bounds
    /// concurrent heavy builds since #4512.
    pub build_slots: usize,
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

    /// The dynamic cap these measurements imply — exactly
    /// [`crate::work_finder::resolve_dynamic_max_concurrent`], so calibrate can
    /// never print a different cap than dispatch would compute.
    #[must_use]
    pub fn dynamic_cap(&self) -> usize {
        crate::work_finder::resolve_dynamic_max_concurrent(
            self.token_axis_limit,
            self.configured_per_token_concurrency,
            self.disk_headroom,
            self.configured_max_concurrent,
        )
    }

    /// Which of the three cap terms binds ([`binding_term`]).
    #[must_use]
    pub fn binding_term(&self) -> BindingTerm {
        binding_term(
            self.token_axis_effective(),
            self.disk_headroom,
            self.configured_max_concurrent,
        )
    }

    /// Render this struct as the `"measurements"` JSON object for `--json`
    /// output.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "logical_cpus": self.logical_cpus,
            "cpu_idle_fraction": self.cpu_idle_fraction,
            "loadavg_1m": self.loadavg_1m,
            "disk_headroom": disk_headroom_json(self.disk_headroom),
            "disk_free_gb": self.disk_free_gb,
            "per_worktree_gb": self.per_worktree_gb,
            "token_axis_limit": self.token_axis_limit,
            "token_axis_effective": self.token_axis_effective(),
            "ranking_present": self.ranking_present,
            "total_accounts": self.total_accounts,
            "configured_max_concurrent": self.configured_max_concurrent,
            "max_concurrent_env_override": self.max_concurrent_env_override,
            // Which tier file set the knob — `null` when the value came from the
            // env override or the built-in default (#4512).
            "max_concurrent_source": self
                .max_concurrent_source
                .as_ref()
                .map(|p| p.display().to_string()),
            "configured_per_token_concurrency": self.configured_per_token_concurrency,
            "per_token_concurrency_env_override": self.per_token_concurrency_env_override,
            "build_slots": self.build_slots,
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
// Binding term — which of the three `min(...)` terms is smallest
// ============================================================================

/// Which of the three dynamic-cap terms is the binding (smallest) constraint.
/// Tie-break order mirrors `loom-daemon status`'s own diagnosis
/// (`resolve_capacity` / `token_bound`): token axis first, then disk, then the
/// ceiling. (A fourth `Cpu` variant existed until #4512 removed the CPU term
/// from admission.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingTerm {
    /// The token axis (`healthy_accounts × per_token_concurrency`) is the
    /// smallest term.
    Token,
    /// Disk headroom is the smallest term.
    Disk,
    /// The operator-configured `maxConcurrent` is the smallest term.
    Ceiling,
}

impl BindingTerm {
    /// The stable lowercase identifier used in `--json` output and log lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::Disk => "disk",
            Self::Ceiling => "ceiling",
        }
    }
}

/// Determine which of the three terms is smallest (binds), applying the same
/// tie-break order [`BindingTerm`] documents.
#[must_use]
pub fn binding_term(
    token_axis_effective: usize,
    disk_headroom: usize,
    ceiling: usize,
) -> BindingTerm {
    let min_val = token_axis_effective.min(disk_headroom).min(ceiling);
    if token_axis_effective == min_val {
        BindingTerm::Token
    } else if disk_headroom == min_val {
        BindingTerm::Disk
    } else {
        BindingTerm::Ceiling
    }
}

// ============================================================================
// measure() — the only impure function in this module
// ============================================================================

/// Measure the live host + resolve the currently-configured knobs for
/// `workspace_root`. Reuses exactly the primitives
/// [`crate::work_finder::spawn_multi_work_finder_task`] and `loom-daemon
/// status` already use, so calibrate can never disagree with what dispatch
/// actually does.
///
/// Also emits the retired-knob deprecation warning
/// ([`crate::work_finder::warn_deprecated_cpu_knobs`]) — an operator running
/// `calibrate` to size a host is exactly who needs to hear that a stale
/// `estCoresPerSweep` is doing nothing. That call covers an *in-daemon* caller
/// (which has a logger); the CLI path has none, so `handle_calibrate_command`
/// additionally prints [`crate::work_finder::deprecated_cpu_knob_notice`] to
/// stderr. Both are idempotent, so the duplication is harmless.
///
/// **May block** (~1s on macOS: refreshing the memoized `iostat` idle sample).
/// Fine for a one-shot CLI invocation; callers on an async runtime should run
/// this through `spawn_blocking`.
#[must_use]
pub fn measure(workspace_root: &Path) -> HostMeasurements {
    let config = crate::work_finder::read_work_finder_config(workspace_root);
    crate::work_finder::warn_deprecated_cpu_knobs(&config);

    let logical_cpus = crate::cpu_headroom::logical_cpu_count();
    // Refresh so a one-shot CLI run reports a *fresh* observation rather than
    // whatever a previous process left memoized (on Linux the first refresh only
    // seeds the cross-tick delta, so `cpu_idle_fraction` can legitimately be
    // `None` on the very first call — the renderer says so explicitly).
    crate::cpu_headroom::refresh_cpu_util_cache();
    let cpu_idle_fraction = crate::cpu_headroom::cached_cpu_idle_fraction();
    let loadavg_1m = crate::cpu_headroom::read_loadavg_1m();

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
    let max_concurrent_source =
        crate::config_resolver::source_of(workspace_root, "autonomous.workFinder.maxConcurrent");
    let configured_per_token_concurrency =
        crate::work_finder::resolve_per_token_concurrency(&config);
    let per_token_concurrency_env_override =
        std::env::var(crate::work_finder::PER_TOKEN_CONCURRENCY_ENV).is_ok();

    HostMeasurements {
        logical_cpus,
        cpu_idle_fraction,
        loadavg_1m,
        disk_headroom,
        disk_free_gb,
        per_worktree_gb,
        token_axis_limit,
        ranking_present,
        total_accounts,
        configured_max_concurrent,
        max_concurrent_env_override,
        max_concurrent_source,
        configured_per_token_concurrency,
        per_token_concurrency_env_override,
        build_slots: crate::build_slot::resolve_slots(),
    }
}

// ============================================================================
// Output rendering
// ============================================================================

/// Render the full measurement report as the top-level `--json` payload.
#[must_use]
pub fn report_json(m: &HostMeasurements) -> Value {
    serde_json::json!({
        "measurements": m.to_json(),
        "dynamic_cap": m.dynamic_cap(),
        "binding_term": m.binding_term().as_str(),
        "advice": advice(m),
        // Retained for machine consumers that keyed off it before #4512: this
        // subcommand no longer writes anything, ever.
        "write": { "applied": false, "config_path": Value::Null },
        "restart_required": true,
    })
}

/// The one-line, actionable reading of the measurements — the whole point of
/// the report. Pure over [`HostMeasurements`], so the tuning policy is
/// unit-tested against injected host classes.
#[must_use]
pub fn advice(m: &HostMeasurements) -> String {
    match m.binding_term() {
        BindingTerm::Ceiling => match m.cpu_idle_fraction {
            Some(idle) if idle >= IDLE_HEADROOM_FRACTION => format!(
                "maxConcurrent={} binds while the host is {:.0}% idle — this machine looks \
                 under-subscribed; raise autonomous.workFinder.maxConcurrent (a step at a time) \
                 and re-measure. The host-distress breaker is the safety net if you overshoot.",
                m.configured_max_concurrent,
                idle * 100.0
            ),
            Some(idle) => format!(
                "maxConcurrent={} binds and the host is only {:.0}% idle — it is already close to \
                 what this machine sustains; do not raise it further without freeing CPU.",
                m.configured_max_concurrent,
                idle * 100.0
            ),
            None => format!(
                "maxConcurrent={} binds, but no CPU idle sample is available on this host — run \
                 this again (or read `loom-daemon status`) once the daemon has sampled, then tune.",
                m.configured_max_concurrent
            ),
        },
        BindingTerm::Token => format!(
            "the token axis binds ({} healthy account(s) × per-token {} = {}) — raising \
             maxConcurrent changes nothing; add accounts (`loom-daemon tokens bootstrap`) or refresh \
             the ranking (`loom-daemon tokens check --ranking`).",
            m.token_axis_limit,
            m.configured_per_token_concurrency.max(1),
            m.token_axis_effective()
        ),
        BindingTerm::Disk => format!(
            "disk headroom binds ({} worktree slot(s) at {} GB each) — raising maxConcurrent \
             changes nothing; free scratch space or lower LOOM_PER_WORKTREE_GB.",
            display_headroom(m.disk_headroom),
            m.per_worktree_gb
        ),
    }
}

/// Idle fraction at or above which a host is treated as clearly
/// under-subscribed for the purpose of the "raise the knob" advice. `0.50` is
/// deliberately generous: #4512's motivating host measured **95%** idle at cap
/// 2, and the point of the advice is to catch that class of gross
/// under-subscription, not to fine-tune the last slot.
pub const IDLE_HEADROOM_FRACTION: f64 = 0.50;

/// Render the human-readable report — per-term measurements, the `min(...)`
/// breakdown, which term binds, and the one-line advice, in the same format
/// `loom-daemon status` uses for its own cap breakdown (`print_status_human`).
#[must_use]
pub fn report_human(m: &HostMeasurements) -> String {
    let mut out = String::new();
    let factor = m.configured_per_token_concurrency.max(1);

    out.push_str("\n=== loom-daemon calibrate (measurements) ===\n\n");

    out.push_str("Host measurements:\n");
    out.push_str(&format!("  logical cpus:   {}\n", m.logical_cpus));
    match (m.cpu_idle_fraction, m.loadavg_1m) {
        (Some(idle), load) => {
            let consumed = m.logical_cpus as f64 * (1.0 - idle.clamp(0.0, 1.0));
            out.push_str(&format!(
                "  cpu observed:   {:.0}% idle (≈{consumed:.1} of {} cores consumed){}\n",
                idle * 100.0,
                m.logical_cpus,
                load.map_or_else(String::new, |l| format!(", 1m loadavg {l:.2}"))
            ));
        }
        (None, Some(load)) => {
            out.push_str(&format!("  cpu observed:   no idle sample yet, 1m loadavg {load:.2}\n"));
        }
        (None, None) => {
            out.push_str("  cpu observed:   no CPU signal available on this platform\n");
        }
    }
    out.push_str(
        "                  (observational only since #4512 — CPU is no longer an admission term)\n",
    );
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
            "  token pool:     no ranking — using raw pool size {} (run `loom-daemon tokens \
             check --ranking` for a healthy-account count)\n",
            m.token_axis_limit
        ));
    }
    out.push_str(&format!(
        "  build slots:    {} (machine-wide; serializes cargo build/test + the build gate — \
         LOOM_BUILD_SLOTS)\n",
        if m.build_slots == 0 {
            "0 — DISABLED".to_string()
        } else {
            m.build_slots.to_string()
        }
    ));

    out.push_str("\nCurrently configured:\n");
    out.push_str(&format!(
        "  autonomous.workFinder.maxConcurrent = {}{}\n",
        m.configured_max_concurrent,
        if m.max_concurrent_env_override {
            " (from LOOM_WORK_FINDER_MAX_CONCURRENT — env outranks config)"
        } else {
            ""
        }
    ));
    // Name the file, not just the number (#4512): on a multi-workspace host the
    // effective config is a merge of up to four tiers, so "edit the config" is
    // ambiguous until the operator knows WHICH config is in play.
    out.push_str(&match (&m.max_concurrent_source, m.max_concurrent_env_override) {
        (Some(path), false) => format!("                  set by: {}\n", path.display()),
        (Some(path), true) => {
            format!(
                "                  (config tier {} is shadowed by the env var)\n",
                path.display()
            )
        }
        (None, false) => format!(
            "                  set by: no config tier — built-in default ({})\n",
            crate::work_finder::DEFAULT_WORK_FINDER_MAX_CONCURRENT
        ),
        (None, true) => String::new(),
    });
    out.push_str(&format!(
        "  autonomous.perTokenConcurrency      = {}{}\n",
        m.configured_per_token_concurrency,
        if m.per_token_concurrency_env_override {
            " (from LOOM_PER_TOKEN_CONCURRENCY — env outranks config)"
        } else {
            ""
        }
    ));

    out.push_str(&format!("\nDynamic concurrency cap: {}\n", m.dynamic_cap()));
    out.push_str(&format!(
        "  = min(healthy {} × per-token {} = {}, disk {}, maxConcurrent {})\n",
        m.token_axis_limit,
        factor,
        m.token_axis_effective(),
        display_headroom(m.disk_headroom),
        m.configured_max_concurrent,
    ));
    out.push_str(&format!("  binds on: {}\n", m.binding_term().as_str()));

    out.push_str(&format!("\nReading: {}\n", advice(m)));
    out.push_str(
        "\nThis report is read-only — calibrate no longer recommends or writes knob values \
         (#4512): maxConcurrent is a per-machine knob you tune from observations like the ones \
         above. It is read once at daemon startup, so restart after editing:\n  \
         ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh\n",
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
    // Test fixtures — host classes named in the #4390 test plan, re-baselined
    // for the #4512 model (no CPU term)
    // ========================================================================

    /// The #4512 motivating host: an 8-core EC2 worker sitting 95% idle whose
    /// throughput was pinned at 2 by the old CPU term. Under the new model the
    /// ceiling binds and the advice is "raise it".
    fn idle_worker_host() -> HostMeasurements {
        HostMeasurements {
            logical_cpus: 8,
            cpu_idle_fraction: Some(0.95),
            loadavg_1m: Some(0.4),
            disk_headroom: 36,
            disk_free_gb: Some(72),
            per_worktree_gb: 2,
            token_axis_limit: 6,
            ranking_present: true,
            total_accounts: 8,
            configured_max_concurrent: 3,
            max_concurrent_env_override: false,
            max_concurrent_source: Some(std::path::PathBuf::from("/repo/.loom/config.json")),
            configured_per_token_concurrency: 2,
            per_token_concurrency_env_override: false,
            build_slots: 1,
        }
    }

    /// A host whose ceiling binds while the CPU is genuinely busy — the case
    /// where the advice must NOT say "raise it".
    fn saturated_host() -> HostMeasurements {
        HostMeasurements {
            cpu_idle_fraction: Some(0.05),
            loadavg_1m: Some(24.0),
            ..idle_worker_host()
        }
    }

    /// Plenty of CPU and tokens, nearly-full scratch volume.
    fn disk_bound_host() -> HostMeasurements {
        HostMeasurements {
            disk_headroom: 2,
            disk_free_gb: Some(5),
            configured_max_concurrent: 10,
            ..idle_worker_host()
        }
    }

    /// One healthy account: the token axis binds.
    fn token_bound_host() -> HostMeasurements {
        HostMeasurements {
            token_axis_limit: 1,
            total_accounts: 8,
            configured_max_concurrent: 10,
            ..idle_worker_host()
        }
    }

    // ========================================================================
    // The cap is exactly `resolve_dynamic_max_concurrent` — no CPU term
    // ========================================================================

    #[test]
    fn dynamic_cap_matches_the_three_term_admission_formula() {
        let m = idle_worker_host();
        // min(6 × 2 = 12, disk 36, maxConcurrent 3) = 3.
        assert_eq!(m.dynamic_cap(), 3);
        assert_eq!(
            m.dynamic_cap(),
            crate::work_finder::resolve_dynamic_max_concurrent(6, 2, 36, 3),
            "calibrate must never compute a different cap than dispatch"
        );
    }

    #[test]
    fn a_95_percent_idle_host_is_no_longer_capped_by_its_cpu() {
        // The #4512 evidence case: with maxConcurrent raised to 10, the cap
        // follows — under the old formula the cpu term pinned it at 2.
        let mut m = idle_worker_host();
        m.configured_max_concurrent = 10;
        assert_eq!(m.dynamic_cap(), 10);
        assert_eq!(m.binding_term(), BindingTerm::Ceiling);
    }

    // ========================================================================
    // binding_term
    // ========================================================================

    #[test]
    fn binding_term_prefers_token_then_disk_then_ceiling() {
        assert_eq!(binding_term(2, 5, 9), BindingTerm::Token);
        assert_eq!(binding_term(9, 2, 5), BindingTerm::Disk);
        assert_eq!(binding_term(9, 5, 2), BindingTerm::Ceiling);
        // Ties resolve to the earliest term, matching `status`'s diagnosis.
        assert_eq!(binding_term(3, 3, 3), BindingTerm::Token);
        assert_eq!(binding_term(9, 3, 3), BindingTerm::Disk);
    }

    #[test]
    fn host_classes_bind_where_expected() {
        assert_eq!(idle_worker_host().binding_term(), BindingTerm::Ceiling);
        assert_eq!(disk_bound_host().binding_term(), BindingTerm::Disk);
        assert_eq!(token_bound_host().binding_term(), BindingTerm::Token);
    }

    // ========================================================================
    // advice — the tuning read-out
    // ========================================================================

    #[test]
    fn advice_says_raise_the_knob_on_an_idle_ceiling_bound_host() {
        let text = advice(&idle_worker_host());
        assert!(text.contains("raise autonomous.workFinder.maxConcurrent"), "{text}");
        assert!(text.contains("95% idle"), "{text}");
    }

    #[test]
    fn advice_does_not_say_raise_when_the_host_is_saturated() {
        let text = advice(&saturated_host());
        assert!(!text.contains("raise autonomous"), "{text}");
        assert!(text.contains("do not raise it further"), "{text}");
    }

    #[test]
    fn advice_points_at_tokens_or_disk_when_those_bind() {
        assert!(advice(&token_bound_host()).contains("loom-daemon tokens bootstrap"));
        assert!(advice(&disk_bound_host()).contains("LOOM_PER_WORKTREE_GB"));
        // Neither should tell the operator to raise maxConcurrent — it would do
        // nothing.
        assert!(!advice(&token_bound_host()).contains("raise autonomous"));
        assert!(!advice(&disk_bound_host()).contains("raise autonomous"));
    }

    #[test]
    fn advice_handles_a_host_with_no_idle_sample() {
        let mut m = idle_worker_host();
        m.cpu_idle_fraction = None;
        assert!(advice(&m).contains("no CPU idle sample"));
    }

    // ========================================================================
    // JSON round-trip
    // ========================================================================

    #[test]
    fn json_round_trips_measurements_and_the_binding_term() {
        let m = idle_worker_host();
        let report = report_json(&m);

        assert_eq!(report["measurements"]["logical_cpus"], 8);
        assert_eq!(report["measurements"]["cpu_idle_fraction"], 0.95);
        assert_eq!(report["measurements"]["configured_max_concurrent"], 3);
        assert_eq!(report["measurements"]["build_slots"], 1);
        assert_eq!(report["dynamic_cap"], 3);
        assert_eq!(report["binding_term"], "ceiling");
        // #4512: calibrate never writes, so this is unconditionally false.
        assert_eq!(report["write"]["applied"], false);
        assert_eq!(report["restart_required"], true);
        // The retired recommendation block must be gone, not silently zeroed.
        assert!(report.get("recommendation").is_none());

        let text = serde_json::to_string(&report).unwrap();
        let reparsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reparsed, report);
    }

    #[test]
    fn json_reports_no_cpu_headroom_term() {
        let report = report_json(&idle_worker_host());
        assert!(
            report["measurements"].get("cpu_headroom").is_none(),
            "the retired CPU headroom term must not reappear in the payload"
        );
    }

    // ------------------------------------------------------------------
    // Knob provenance (#4512): the report must name WHICH file set the one
    // knob that now carries the whole admission policy.
    // ------------------------------------------------------------------

    #[test]
    fn json_names_the_config_tier_that_set_max_concurrent() {
        let report = report_json(&idle_worker_host());
        assert_eq!(report["measurements"]["max_concurrent_source"], "/repo/.loom/config.json");
    }

    #[test]
    fn json_reports_a_null_source_when_no_tier_sets_the_knob() {
        let m = HostMeasurements {
            max_concurrent_source: None,
            ..idle_worker_host()
        };
        assert!(m.to_json()["max_concurrent_source"].is_null());
    }

    #[test]
    fn human_report_names_the_config_file_that_set_the_knob() {
        let text = report_human(&idle_worker_host());
        assert!(
            text.contains("set by: /repo/.loom/config.json"),
            "the operator must be told which file to edit:\n{text}"
        );
    }

    #[test]
    fn human_report_says_built_in_default_when_no_tier_sets_the_knob() {
        let text = report_human(&HostMeasurements {
            max_concurrent_source: None,
            ..idle_worker_host()
        });
        assert!(text.contains("built-in default"), "{text}");
    }

    #[test]
    fn human_report_flags_a_config_tier_shadowed_by_the_env_var() {
        // Editing the named file would change nothing while the env var is set —
        // the exact trap #4512's operator note called out.
        let text = report_human(&HostMeasurements {
            max_concurrent_env_override: true,
            ..idle_worker_host()
        });
        assert!(text.contains("shadowed by the env var"), "{text}");
        assert!(text.contains("/repo/.loom/config.json"), "{text}");
    }

    #[test]
    fn disk_headroom_unbounded_renders_as_string_not_a_giant_int() {
        let mut m = idle_worker_host();
        m.disk_headroom = usize::MAX;
        m.disk_free_gb = None;
        assert_eq!(m.to_json()["disk_headroom"], "unbounded");
    }

    // ========================================================================
    // Human report
    // ========================================================================

    #[test]
    fn human_report_contains_key_lines() {
        let text = report_human(&idle_worker_host());
        assert!(text.contains("loom-daemon calibrate"));
        assert!(text.contains("Dynamic concurrency cap: 3"));
        assert!(text.contains("= min(healthy 6 × per-token 2 = 12, disk 36, maxConcurrent 3)"));
        assert!(text.contains("binds on: ceiling"));
        assert!(text.contains("build slots:    1"));
        assert!(text.contains("LOOM_PER_WORKTREE_GB"));
        assert!(text.contains("read-only"));
        assert!(text.contains("loom-daemon-start.sh"));
    }

    #[test]
    fn human_report_flags_a_disabled_build_slot() {
        let mut m = idle_worker_host();
        m.build_slots = 0;
        assert!(report_human(&m).contains("0 — DISABLED"));
    }

    #[test]
    fn human_report_handles_unmeasurable_disk_and_missing_cpu_signal() {
        let mut m = idle_worker_host();
        m.disk_headroom = usize::MAX;
        m.disk_free_gb = None;
        m.cpu_idle_fraction = None;
        m.loadavg_1m = None;
        let text = report_human(&m);
        assert!(text.contains("unmeasurable"));
        assert!(text.contains("no CPU signal available"));
    }

    #[test]
    fn human_report_names_env_overrides() {
        let mut m = idle_worker_host();
        m.max_concurrent_env_override = true;
        m.per_token_concurrency_env_override = true;
        let text = report_human(&m);
        assert!(text.contains("from LOOM_WORK_FINDER_MAX_CONCURRENT"));
        assert!(text.contains("from LOOM_PER_TOKEN_CONCURRENCY"));
    }

    // ========================================================================
    // measure() smoke test — the only impure function; guards against a panic
    // against a real (empty) temp workspace. No live-probe assertions (host
    // dependent).
    // ========================================================================

    #[test]
    fn measure_does_not_panic_on_an_empty_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let m = measure(dir.path());
        assert!(m.logical_cpus >= 1);
        // Whatever the host reports, the derived numbers must be coherent.
        assert_eq!(m.dynamic_cap(), m.dynamic_cap());
        if let Some(idle) = m.cpu_idle_fraction {
            assert!((0.0..=1.0).contains(&idle));
        }
    }
}
