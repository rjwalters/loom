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
//! `min(disk headroom, ram headroom, maxConcurrent)` breakdown, which term
//! binds, and — since #5270 — whether the separate CPU saturation admission
//! brake ([`crate::admission_brake`]) is holding dispatch outright. Reading it
//! is how an operator decides whether to raise the knob:
//!
//! - **binds on `ceiling` while the host sits idle** ⇒ raise `maxConcurrent`.
//! - **binds on `ceiling` while the host is saturated** (low idle fraction, or
//!   the host breaker tripping) ⇒ the knob is already at/above what this machine
//!   sustains; leave it or lower it.
//! - **binds on `disk`** ⇒ raising `maxConcurrent` changes nothing; free
//!   scratch space.
//! - **binds on `ram`** (#5270) ⇒ raising `maxConcurrent` changes nothing;
//!   free memory.
//! - **the CPU admission brake is holding** (#5270) ⇒ dispatch is blocked
//!   THIS tick regardless of the cap above; wait for load to drop, or retune
//!   the brake's threshold if it is firing on routine bursts.
//!
//! The token pool's healthy-account count is still reported (it drives
//! spawn-time *selection*), but since #5270 it is informational only — it no
//! longer bounds the cap (nor does the CPU term, on any auth path — a
//! metered API key or an overage-enabled subscription pool has no
//! exhaustible admission ceiling worth modeling). See
//! [`crate::work_finder::resolve_dynamic_max_concurrent`] for the full
//! rationale.
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
    /// The RAM headroom term (#5270,
    /// [`crate::ram_headroom::ram_headroom_limit`]) — worktree slots the
    /// currently-available memory can hold at the resolved per-worktree RAM
    /// GB estimate. `usize::MAX` means the probe was unmeasurable (skip the
    /// clamp — see [`crate::ram_headroom`] docs).
    pub ram_headroom: usize,
    /// Available RAM (GB), when measurable (`None` when unmeasurable — see
    /// [`crate::ram_headroom::available_ram_gb`]).
    pub ram_free_gb: Option<u64>,
    /// The resolved per-worktree RAM GB estimate
    /// ([`crate::ram_headroom::per_worktree_ram_gb`]) — env-only, no config
    /// key (mirrors [`Self::per_worktree_gb`]).
    pub per_worktree_ram_gb: u64,
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
    /// The currently resolved `autonomous.workFinder.maxConcurrent` — the
    /// per-machine **sweep-dispatch** admission knob (env > config > default —
    /// [`crate::work_finder::resolve_max_concurrent_with_config`]).
    ///
    /// **It bounds sweeps only (#6102).** Role-runner agents are spawned by
    /// [`crate::role_runner`]'s own interval / idle loops and never pass
    /// through work-finder admission, so this number is only half of this
    /// host's agent budget — see [`Self::role_agent_max_concurrent`]. Calibrate
    /// used to call it "**the** per-machine tuning term", which is what led an
    /// operator to lower it from 16 to 8 after a load-induced hard halt and
    /// still measure 1.15 load-per-core with a single sweep in flight.
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
    /// How many machine-wide build slots serialize the designated high-CPU
    /// stages ([`crate::build_slot::resolve_slots`], `0` = disabled). Reported
    /// because this — not an admission-time CPU estimate — is what bounds
    /// concurrent heavy builds since #4512.
    pub build_slots: usize,
    /// Whether the CPU saturation admission brake
    /// ([`crate::admission_brake`], #4903) is enabled for this workspace.
    pub cpu_admission_brake_enabled: bool,
    /// The brake's resolved load-per-core hold threshold (env > config >
    /// default — [`crate::admission_brake::DEFAULT_ADMISSION_BRAKE_LOAD_PER_CORE`],
    /// retuned to the "dumb mode" `0.95` CPU gate by #5270).
    pub cpu_admission_brake_threshold: f64,
    /// Whether the brake would hold new admissions **right now**, computed
    /// fresh from [`Self::loadavg_1m`] / [`Self::logical_cpus`] — a one-shot
    /// `calibrate` invocation is a separate process from the running daemon,
    /// so this recomputes the decision rather than reading the live daemon's
    /// registered brake state (which this process cannot see).
    pub cpu_admission_brake_held: bool,
    /// Whether the role runner is enabled for this workspace
    /// ([`crate::role_runner::resolve_enabled`], #6102). When `true`, this host
    /// carries a second population of agents that
    /// [`Self::configured_max_concurrent`] does not bound, and the advice below
    /// says so instead of attributing all host busy-ness to sweeps.
    pub role_runner_enabled: bool,
    /// The resolved concurrent role-agent ceiling
    /// ([`crate::role_runner::resolve_max_concurrent`], #6102) — the second
    /// half of this host's agent budget, tuned by
    /// `autonomous.roleRunner.maxConcurrent` /
    /// `LOOM_ROLE_RUNNER_MAX_CONCURRENT`.
    ///
    /// Deliberately **not** a term in [`Self::dynamic_cap`]: role agents and
    /// sweeps are admitted by different subsystems against different queues, so
    /// folding them into one `min(...)` would misreport which one is actually
    /// binding. They are reported as two ceilings that sum to a worst-case
    /// agent count, which is the number an operator sizing a host needs.
    pub role_agent_max_concurrent: usize,
}

impl HostMeasurements {
    /// The dynamic cap these measurements imply — exactly
    /// [`crate::work_finder::resolve_dynamic_max_concurrent`], so calibrate can
    /// never print a different cap than dispatch would compute.
    #[must_use]
    pub fn dynamic_cap(&self) -> usize {
        crate::work_finder::resolve_dynamic_max_concurrent(
            self.disk_headroom,
            self.ram_headroom,
            self.configured_max_concurrent,
        )
    }

    /// Which of the three cap terms binds ([`binding_term`]). The token axis
    /// is no longer part of the admission formula (#5270) — `token_axis_limit`
    /// remains an informational-only field on this report, reflecting
    /// spawn-time account health, not a concurrency cap.
    #[must_use]
    pub fn binding_term(&self) -> BindingTerm {
        binding_term(self.disk_headroom, self.ram_headroom, self.configured_max_concurrent)
    }

    /// The single "which machine axis is limiting dispatch right now" answer
    /// (#5270 AC4 — max/disk/RAM/CPU observability). The CPU saturation
    /// admission brake is a **point-in-time hold**, not a term in the
    /// `min(...)` [`Self::dynamic_cap`] formula (see
    /// [`crate::work_finder::resolve_dynamic_max_concurrent`]'s own docs for
    /// why), so it takes priority here: a held brake means dispatch is
    /// blocked THIS tick regardless of what the numeric cap says. When the
    /// brake is not holding, this falls back to [`Self::binding_term`].
    #[must_use]
    pub fn admission_axis(&self) -> &'static str {
        if self.cpu_admission_brake_held {
            "cpu"
        } else {
            self.binding_term().as_str()
        }
    }

    /// Render this struct as the `"measurements"` JSON object for `--json`
    /// output.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "logical_cpus": self.logical_cpus,
            "cpu_idle_fraction": self.cpu_idle_fraction,
            "loadavg_1m": self.loadavg_1m,
            "disk_headroom": headroom_json(self.disk_headroom),
            "disk_free_gb": self.disk_free_gb,
            "per_worktree_gb": self.per_worktree_gb,
            "ram_headroom": headroom_json(self.ram_headroom),
            "ram_free_gb": self.ram_free_gb,
            "per_worktree_ram_gb": self.per_worktree_ram_gb,
            "token_axis_limit": self.token_axis_limit,
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
            "build_slots": self.build_slots,
            "cpu_admission_brake": {
                "enabled": self.cpu_admission_brake_enabled,
                "held": self.cpu_admission_brake_held,
                "load_per_core_threshold": self.cpu_admission_brake_threshold,
                // #6102: the brake samples host-wide load (so role-agent load
                // DOES push it over the threshold) but only gates sweep
                // admission — it cannot hold back a role agent, and it cannot
                // shed load already in flight. Stated in the payload so a
                // machine consumer reads the same guarantee the human report
                // prints.
                "holds": "new sweep admissions only",
            },
            // The other half of this host's agent budget (#6102) — a separate
            // ceiling, not a term in `dynamic_cap`.
            "role_runner": {
                "enabled": self.role_runner_enabled,
                "max_concurrent": self.role_agent_max_concurrent,
                "bounded_by_work_finder_max_concurrent": false,
            },
        })
    }
}

/// `usize::MAX` (the disk/RAM-unmeasurable / unbounded sentinel) does not
/// survive JSON's number type cleanly across every consumer — render it as
/// the string `"unbounded"` instead of a giant integer, matching how an
/// operator should read it.
fn headroom_json(headroom: usize) -> Value {
    if headroom == usize::MAX {
        Value::String("unbounded".to_string())
    } else {
        serde_json::json!(headroom)
    }
}

// ============================================================================
// Binding term — which of the three `min(...)` terms is smallest
// ============================================================================

/// Which of the three dynamic-cap terms is the binding (smallest) constraint.
/// Tie-break order mirrors `loom-daemon status`'s own diagnosis: disk first,
/// then RAM, then the ceiling. (A `Token` variant existed until #5270 removed
/// the token axis from admission — see
/// [`crate::work_finder::resolve_dynamic_max_concurrent`] — and a `Cpu`
/// variant existed until #4512 removed the CPU term from this same formula;
/// CPU saturation is reported separately via
/// [`crate::admission_brake`]/`loom-daemon status`'s `admission_brake` block,
/// since it holds admission as a point-in-time gate rather than shrinking
/// this `min(...)`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingTerm {
    /// Disk headroom is the smallest term.
    Disk,
    /// RAM headroom is the smallest term (#5270).
    Ram,
    /// The operator-configured `maxConcurrent` is the smallest term.
    Ceiling,
}

impl BindingTerm {
    /// The stable lowercase identifier used in `--json` output and log lines.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Ram => "ram",
            Self::Ceiling => "ceiling",
        }
    }
}

/// Determine which of the three terms is smallest (binds), applying the same
/// tie-break order [`BindingTerm`] documents.
#[must_use]
pub fn binding_term(disk_headroom: usize, ram_headroom: usize, ceiling: usize) -> BindingTerm {
    let min_val = disk_headroom.min(ram_headroom).min(ceiling);
    if disk_headroom == min_val {
        BindingTerm::Disk
    } else if ram_headroom == min_val {
        BindingTerm::Ram
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

    let ram_free_gb = crate::ram_headroom::available_ram_gb();
    let per_worktree_ram_gb = crate::ram_headroom::per_worktree_ram_gb();
    let ram_headroom = crate::ram_headroom::ram_headroom_limit();

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

    // CPU saturation admission brake (#4903, retuned by #5270 into the "dumb
    // mode" CPU gate): recomputed fresh rather than read from the live
    // daemon's registered global, since a one-shot `calibrate` invocation runs
    // in a separate process that never sees that state.
    let brake_config = crate::admission_brake::resolve_config_for(workspace_root);
    let brake_decision = crate::admission_brake::decide(&brake_config, loadavg_1m, logical_cpus);

    // Role-runner agent budget (#6102) — read from the same root's config as
    // every other knob above.
    let role_runner_config = crate::role_runner::read_role_runner_config(workspace_root);

    HostMeasurements {
        logical_cpus,
        cpu_idle_fraction,
        loadavg_1m,
        disk_headroom,
        disk_free_gb,
        per_worktree_gb,
        ram_headroom,
        ram_free_gb,
        per_worktree_ram_gb,
        token_axis_limit,
        ranking_present,
        total_accounts,
        configured_max_concurrent,
        max_concurrent_env_override,
        max_concurrent_source,
        build_slots: crate::build_slot::resolve_slots(),
        cpu_admission_brake_enabled: brake_config.enabled,
        cpu_admission_brake_threshold: brake_config.load_per_core_threshold,
        cpu_admission_brake_held: brake_decision.held,
        // Role-runner agent budget (#6102): resolved the same way the daemon's
        // own role loops resolve it, so calibrate can never print a ceiling
        // different from the one that will actually refuse a tick. NOT the live
        // in-flight count — `calibrate` is a separate one-shot process with no
        // view of the running daemon's guard (`loom-daemon status` is where the
        // live count is reported, exactly as it is for sweeps).
        role_runner_enabled: crate::role_runner::resolve_enabled(&role_runner_config),
        role_agent_max_concurrent: crate::role_runner::resolve_max_concurrent(&role_runner_config),
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
        // #5270 AC4: the CPU brake is a point-in-time hold outside the
        // `min(...)` cap formula, so it can override `binding_term` as the
        // real answer to "what's stopping dispatch right now" — see
        // `HostMeasurements::admission_axis`.
        "admission_axis": m.admission_axis(),
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
    if m.cpu_admission_brake_held {
        return format!(
            "the CPU saturation admission brake is HOLDING new SWEEP admissions right now \
             (load-per-core ≥ {:.2}) — this is the actual limiter this instant, ahead of the \
             disk/RAM/maxConcurrent cap below; in-flight sweeps are untouched and it releases the \
             moment load drops back under the threshold (#5270, #4903).{}",
            m.cpu_admission_brake_threshold,
            role_agent_caveat(m)
        );
    }
    match m.binding_term() {
        BindingTerm::Ceiling => match m.cpu_idle_fraction {
            Some(idle) if idle >= IDLE_HEADROOM_FRACTION => format!(
                "maxConcurrent={} binds sweep dispatch while the host is {:.0}% idle — this \
                 machine looks under-subscribed; raise autonomous.workFinder.maxConcurrent (a \
                 step at a time) and re-measure. The host-distress breaker is the safety net if \
                 you overshoot.{}",
                m.configured_max_concurrent,
                idle * 100.0,
                role_agent_caveat(m)
            ),
            Some(idle) => format!(
                "maxConcurrent={} binds sweep dispatch and the host is only {:.0}% idle — it is \
                 already close to what this machine sustains; do not raise it further without \
                 freeing CPU.{}",
                m.configured_max_concurrent,
                idle * 100.0,
                role_agent_caveat(m)
            ),
            None => format!(
                "maxConcurrent={} binds sweep dispatch, but no CPU idle sample is available on \
                 this host — run this again (or read `loom-daemon status`) once the daemon has \
                 sampled, then tune.{}",
                m.configured_max_concurrent,
                role_agent_caveat(m)
            ),
        },
        BindingTerm::Disk => format!(
            "disk headroom binds ({} worktree slot(s) at {} GB each) — raising maxConcurrent \
             changes nothing; free scratch space or lower LOOM_PER_WORKTREE_GB.",
            display_headroom(m.disk_headroom),
            m.per_worktree_gb
        ),
        BindingTerm::Ram => format!(
            "RAM headroom binds ({} worktree slot(s) at {} GB each) — raising maxConcurrent \
             changes nothing; free memory or lower LOOM_PER_WORKTREE_RAM_GB (#5270).",
            display_headroom(m.ram_headroom),
            m.per_worktree_ram_gb
        ),
    }
}

/// The sentence appended to every `maxConcurrent`-centric reading when the role
/// runner is enabled (#6102): a warning that the knob being discussed does not
/// bound the other population of agents on this host.
///
/// Empty when the role runner is disabled — there is no second population to
/// warn about, and the pre-#6102 advice text is then exactly right.
///
/// This exists because the advice is what an operator acts on after a
/// load-induced crash. Saying "maxConcurrent=16 binds and the host is only 41%
/// idle — do not raise it further" while silently omitting that up to N more
/// role agents are admitted outside that number is not a wording problem: it
/// sends the operator to tune a knob that cannot deliver the relief implied.
fn role_agent_caveat(m: &HostMeasurements) -> String {
    if !m.role_runner_enabled {
        return String::new();
    }
    format!(
        " NOTE (#6102): this knob bounds SWEEP dispatch only. The role runner is enabled here \
         and admits up to {} more concurrent agent(s) outside it \
         (autonomous.roleRunner.maxConcurrent / LOOM_ROLE_RUNNER_MAX_CONCURRENT), so this host's \
         worst-case agent count is {} + {} = {}. If load is high with few sweeps in flight, \
         lower the role-agent ceiling — lowering maxConcurrent will not reach that load.",
        m.role_agent_max_concurrent,
        m.configured_max_concurrent,
        m.role_agent_max_concurrent,
        m.configured_max_concurrent
            .saturating_add(m.role_agent_max_concurrent)
    )
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
    out.push_str(&format!(
        "                  (not a `min(...)` cap term since #4512 — but the CPU admission \
         brake {} watches it: holds new admissions at load-per-core ≥ {:.2}, currently {})\n",
        if m.cpu_admission_brake_enabled {
            "IS enabled"
        } else {
            "is disabled"
        },
        m.cpu_admission_brake_threshold,
        if m.cpu_admission_brake_held {
            "HOLDING"
        } else {
            "not holding"
        }
    ));
    // #6102 AC4 — state the brake's guarantee accurately at the point it is
    // reported. It samples host-wide load (role-agent load DOES push it over
    // the threshold), but the only thing it can withhold is a NEW sweep
    // admission: it cannot refuse a role agent, and it cannot shed load already
    // in flight. Both limits caused real confusion on the incident host.
    out.push_str(
        "                  (what the brake can hold back: NEW sweep admissions only. It cannot \
         refuse a role-runner agent, and it cannot shed load already in flight — see the \
         role-agent ceiling below, #6102)\n",
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
    match (m.ram_free_gb, m.ram_headroom) {
        (Some(free), headroom) if headroom != usize::MAX => {
            out.push_str(&format!(
                "  ram headroom:   {headroom} worktree slot(s) ({free} GB available / {} GB per-worktree estimate)\n",
                m.per_worktree_ram_gb
            ));
        }
        _ => {
            out.push_str(
                "  ram headroom:   unmeasurable (skipping the RAM clamp — treated as unbounded)\n",
            );
        }
    }
    out.push_str(
        "                  (LOOM_PER_WORKTREE_RAM_GB is env-only — there is no config key for \
         the RAM estimate; override with `export LOOM_PER_WORKTREE_RAM_GB=<gb>`, #5270)\n",
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
    // #6102 AC1: report the OTHER agent ceiling right next to `maxConcurrent`,
    // so an operator reading "Currently configured" sees this host's whole
    // agent budget rather than the sweep half of it.
    out.push_str(&format!(
        "  autonomous.roleRunner.maxConcurrent = {} (role runner is {})\n",
        m.role_agent_max_concurrent,
        if m.role_runner_enabled {
            "ENABLED"
        } else {
            "disabled — no role agents run"
        }
    ));
    out.push_str(
        "                  (SEPARATE from maxConcurrent above: role agents are spawned by the \
         role runner's own interval/idle loops and never pass through work-finder admission, so \
         maxConcurrent has never bounded them — #6102)\n",
    );

    out.push_str(&format!("\nDynamic concurrency cap: {}\n", m.dynamic_cap()));
    out.push_str(&format!(
        "  = min(disk {}, ram {}, maxConcurrent {})\n",
        display_headroom(m.disk_headroom),
        display_headroom(m.ram_headroom),
        m.configured_max_concurrent,
    ));
    out.push_str("  bounds: SWEEP dispatch only (#6102)\n");
    if m.role_runner_enabled {
        out.push_str(&format!(
            "  worst-case agents on this host: {} sweep(s) + {} role agent(s) = {}\n",
            m.configured_max_concurrent,
            m.role_agent_max_concurrent,
            m.configured_max_concurrent
                .saturating_add(m.role_agent_max_concurrent)
        ));
    }
    out.push_str(&format!("  cap binds on: {}\n", m.binding_term().as_str()));
    out.push_str(&format!(
        "  admission blocked by (max/disk/ram/cpu, #5270 AC4): {}\n",
        m.admission_axis()
    ));

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
            ram_headroom: 40,
            ram_free_gb: Some(80),
            per_worktree_ram_gb: 2,
            token_axis_limit: 6,
            ranking_present: true,
            total_accounts: 8,
            configured_max_concurrent: 3,
            max_concurrent_env_override: false,
            max_concurrent_source: Some(std::path::PathBuf::from("/repo/.loom/config.json")),
            build_slots: 1,
            cpu_admission_brake_enabled: true,
            cpu_admission_brake_threshold: 0.95,
            cpu_admission_brake_held: false,
            // #6102: the base fixture keeps the role runner OFF so every
            // pre-#6102 advice assertion below reads the unchanged text (the
            // caveat sentence is appended only when there is a second agent
            // population to warn about). `role_runner_enabled_host()` is the
            // fixture that turns it on.
            role_runner_enabled: false,
            role_agent_max_concurrent: 7,
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

    /// Plenty of CPU/RAM/tokens, nearly-full scratch volume.
    fn disk_bound_host() -> HostMeasurements {
        HostMeasurements {
            disk_headroom: 2,
            disk_free_gb: Some(5),
            configured_max_concurrent: 10,
            ..idle_worker_host()
        }
    }

    /// Plenty of CPU/tokens/disk, critically low available RAM (#5270).
    fn ram_bound_host() -> HostMeasurements {
        HostMeasurements {
            ram_headroom: 1,
            ram_free_gb: Some(2),
            configured_max_concurrent: 10,
            ..idle_worker_host()
        }
    }

    /// A near-exhausted token pool: no longer binds the cap since #5270 (only
    /// disk/ram/maxConcurrent do), but the field remains for the informational
    /// token-pool line in the report.
    fn token_starved_host() -> HostMeasurements {
        HostMeasurements {
            token_axis_limit: 1,
            total_accounts: 8,
            configured_max_concurrent: 10,
            ..idle_worker_host()
        }
    }

    /// A host where the CPU saturation admission brake is currently holding
    /// new admissions — the "dumb mode" case #5270 introduces.
    fn cpu_brake_held_host() -> HostMeasurements {
        HostMeasurements {
            cpu_admission_brake_held: true,
            loadavg_1m: Some(7.8),
            ..idle_worker_host()
        }
    }

    /// The #6102 incident shape: a busy host whose `maxConcurrent` binds while
    /// the role runner is ALSO enabled, so a second population of agents is
    /// admitted entirely outside that knob.
    fn role_runner_enabled_host() -> HostMeasurements {
        HostMeasurements {
            role_runner_enabled: true,
            role_agent_max_concurrent: 7,
            cpu_idle_fraction: Some(0.05),
            loadavg_1m: Some(32.4),
            ..idle_worker_host()
        }
    }

    // ========================================================================
    // The cap is exactly `resolve_dynamic_max_concurrent` — no CPU or token
    // axis term (#5270)
    // ========================================================================

    #[test]
    fn dynamic_cap_matches_the_three_term_admission_formula() {
        let m = idle_worker_host();
        // min(disk 36, ram 40, maxConcurrent 3) = 3 — the token axis (6 × 2 =
        // 12) plays no role even though it would have been smaller under the
        // old formula.
        assert_eq!(m.dynamic_cap(), 3);
        assert_eq!(
            m.dynamic_cap(),
            crate::work_finder::resolve_dynamic_max_concurrent(36, 40, 3),
            "calibrate must never compute a different cap than dispatch"
        );
    }

    #[test]
    fn a_token_starved_host_is_not_capped_by_its_token_pool() {
        // #5270: even with only 1 healthy account, the cap is min(disk, ram,
        // maxConcurrent) — the token axis never binds any more.
        let m = token_starved_host();
        assert_eq!(m.dynamic_cap(), 10, "min(disk 36, ram 40, maxConcurrent 10)");
        assert_eq!(m.binding_term(), BindingTerm::Ceiling);
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

    #[test]
    fn a_ram_starved_host_is_capped_by_ram_headroom() {
        // #5270 AC3: critically-low RAM caps the dynamic concurrency the same
        // way disk headroom already did.
        let m = ram_bound_host();
        assert_eq!(m.dynamic_cap(), 1, "min(disk 36, ram 1, maxConcurrent 10)");
        assert_eq!(m.binding_term(), BindingTerm::Ram);
    }

    // ========================================================================
    // binding_term
    // ========================================================================

    #[test]
    fn binding_term_prefers_disk_then_ram_then_ceiling() {
        assert_eq!(binding_term(2, 9, 9), BindingTerm::Disk);
        assert_eq!(binding_term(9, 2, 9), BindingTerm::Ram);
        assert_eq!(binding_term(9, 9, 2), BindingTerm::Ceiling);
        // A tie between disk and ram resolves to disk, matching `status`'s
        // tie-break order.
        assert_eq!(binding_term(3, 3, 9), BindingTerm::Disk);
        assert_eq!(binding_term(9, 3, 3), BindingTerm::Ram);
    }

    #[test]
    fn host_classes_bind_where_expected() {
        assert_eq!(idle_worker_host().binding_term(), BindingTerm::Ceiling);
        assert_eq!(disk_bound_host().binding_term(), BindingTerm::Disk);
        assert_eq!(ram_bound_host().binding_term(), BindingTerm::Ram);
        // #5270: even a near-exhausted token pool never binds the cap any more.
        assert_eq!(token_starved_host().binding_term(), BindingTerm::Ceiling);
    }

    // ========================================================================
    // admission_axis — the CPU brake overrides the numeric binding_term
    // ========================================================================

    #[test]
    fn admission_axis_falls_back_to_binding_term_when_the_brake_is_not_holding() {
        assert_eq!(idle_worker_host().admission_axis(), "ceiling");
        assert_eq!(disk_bound_host().admission_axis(), "disk");
        assert_eq!(ram_bound_host().admission_axis(), "ram");
    }

    #[test]
    fn admission_axis_reports_cpu_when_the_brake_is_holding() {
        // Even a host whose numeric cap would say "ceiling" reports "cpu" here
        // — the brake blocks dispatch THIS tick regardless of the cap.
        assert_eq!(cpu_brake_held_host().admission_axis(), "cpu");
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
    fn advice_points_at_disk_when_it_binds() {
        assert!(advice(&disk_bound_host()).contains("LOOM_PER_WORKTREE_GB"));
        // Must not tell the operator to raise maxConcurrent — it would do
        // nothing while disk binds.
        assert!(!advice(&disk_bound_host()).contains("raise autonomous"));
    }

    #[test]
    fn advice_handles_a_host_with_no_idle_sample() {
        let mut m = idle_worker_host();
        m.cpu_idle_fraction = None;
        assert!(advice(&m).contains("no CPU idle sample"));
    }

    #[test]
    fn advice_points_at_ram_when_it_binds() {
        assert!(advice(&ram_bound_host()).contains("LOOM_PER_WORKTREE_RAM_GB"));
        // Must not tell the operator to raise maxConcurrent — it would do
        // nothing while RAM binds.
        assert!(!advice(&ram_bound_host()).contains("raise autonomous"));
    }

    #[test]
    fn advice_names_the_cpu_brake_when_it_is_holding_ahead_of_the_numeric_cap() {
        // The brake held-state must win even though this fixture's numeric cap
        // (min(disk, ram, maxConcurrent)) would otherwise say "ceiling" — the
        // brake is the real limiter this instant.
        let text = advice(&cpu_brake_held_host());
        assert!(text.contains("HOLDING"), "{text}");
        assert!(text.contains("0.95"), "{text}");
        assert!(!text.contains("raise autonomous"), "{text}");
    }

    // ========================================================================
    // Role-agent budget (#6102) — maxConcurrent bounds sweeps only
    // ========================================================================

    /// AC1: `calibrate`'s reading is what an operator acts on after a
    /// load-induced crash. When the role runner is enabled it must say that
    /// `maxConcurrent` bounds sweep dispatch only, and name the ceiling that
    /// does bound the other agents — otherwise it sends the operator to tune a
    /// knob that cannot deliver the relief implied (exactly what happened when
    /// this host's `maxConcurrent` went 16 → 8 and load stayed at 1.15/core).
    #[test]
    fn advice_warns_that_max_concurrent_does_not_bound_role_agents() {
        let text = advice(&role_runner_enabled_host());
        assert!(
            text.contains("bounds SWEEP dispatch only"),
            "the reading must scope the knob it is discussing: {text}"
        );
        assert!(
            text.contains("autonomous.roleRunner.maxConcurrent"),
            "the reading must name the knob that DOES bound role agents: {text}"
        );
        // 3 (fixture maxConcurrent) + 7 (role ceiling) = 10 worst-case agents.
        assert!(
            text.contains("3 + 7 = 10"),
            "the reading must state this host's worst-case agent count: {text}"
        );
    }

    /// The caveat is appended only when there IS a second agent population —
    /// with the role runner off, the pre-#6102 advice is already accurate and
    /// must not grow a spurious warning.
    #[test]
    fn advice_omits_the_role_agent_caveat_when_the_role_runner_is_disabled() {
        for m in [
            idle_worker_host(),
            saturated_host(),
            disk_bound_host(),
            cpu_brake_held_host(),
        ] {
            let text = advice(&m);
            assert!(!text.contains("#6102"), "role runner disabled ⇒ no role-agent caveat: {text}");
        }
    }

    /// The caveat rides along even when the CPU brake is the reported limiter
    /// — that branch returns early, and it is precisely the branch an operator
    /// reads on a saturated host, so omitting it there would miss the case
    /// that matters most.
    #[test]
    fn advice_carries_the_caveat_on_the_brake_held_branch_too() {
        let text = advice(&HostMeasurements {
            cpu_admission_brake_held: true,
            ..role_runner_enabled_host()
        });
        assert!(text.contains("HOLDING"), "{text}");
        assert!(text.contains("#6102"), "{text}");
    }

    #[test]
    fn human_report_lists_the_role_agent_ceiling_next_to_max_concurrent() {
        let text = report_human(&role_runner_enabled_host());
        assert!(
            text.contains("autonomous.roleRunner.maxConcurrent = 7"),
            "the configured section must show both ceilings: {text}"
        );
        assert!(
            text.contains("bounds: SWEEP dispatch only"),
            "the dynamic-cap block must scope itself: {text}"
        );
        assert!(
            text.contains("worst-case agents on this host: 3 sweep(s) + 7 role agent(s) = 10"),
            "{text}"
        );
    }

    /// AC4: the brake's stated guarantee must be accurate wherever it is
    /// reported — it samples host-wide load, but can only withhold a NEW sweep
    /// admission, and can shed nothing already running.
    #[test]
    fn human_report_states_what_the_brake_can_and_cannot_hold_back() {
        let text = report_human(&idle_worker_host());
        assert!(text.contains("NEW sweep admissions only"), "{text}");
        assert!(text.contains("cannot refuse a role-runner agent"), "{text}");
        assert!(text.contains("cannot shed load already in flight"), "{text}");
    }

    #[test]
    fn json_reports_the_role_runner_block_and_the_brake_scope() {
        let json = role_runner_enabled_host().to_json();
        assert_eq!(json["role_runner"]["enabled"], true);
        assert_eq!(json["role_runner"]["max_concurrent"], 7);
        assert_eq!(
            json["role_runner"]["bounded_by_work_finder_max_concurrent"], false,
            "machine consumers must get the #6102 fact too, not just the prose"
        );
        assert_eq!(json["cpu_admission_brake"]["holds"], "new sweep admissions only");
        // Role agents are deliberately NOT a term in the `min(...)` cap — the
        // two populations are admitted by different subsystems, so folding
        // them into one number would misreport which is binding.
        assert_eq!(
            report_json(&role_runner_enabled_host())["dynamic_cap"],
            role_runner_enabled_host().dynamic_cap()
        );
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

    #[test]
    fn ram_headroom_unbounded_renders_as_string_not_a_giant_int() {
        let mut m = idle_worker_host();
        m.ram_headroom = usize::MAX;
        m.ram_free_gb = None;
        assert_eq!(m.to_json()["ram_headroom"], "unbounded");
    }

    // ========================================================================
    // Human report
    // ========================================================================

    #[test]
    fn human_report_contains_key_lines() {
        let text = report_human(&idle_worker_host());
        assert!(text.contains("loom-daemon calibrate"));
        assert!(text.contains("Dynamic concurrency cap: 3"));
        assert!(text.contains("= min(disk 36, ram 40, maxConcurrent 3)"));
        assert!(text.contains("cap binds on: ceiling"));
        assert!(text.contains("admission blocked by"));
        assert!(text.contains("build slots:    1"));
        assert!(text.contains("LOOM_PER_WORKTREE_GB"));
        assert!(text.contains("LOOM_PER_WORKTREE_RAM_GB"));
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
        let text = report_human(&m);
        assert!(text.contains("from LOOM_WORK_FINDER_MAX_CONCURRENT"));
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
