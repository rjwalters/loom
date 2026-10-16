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
//! So calibrate is strictly a **measurement report**: it prints what the host
//! looks like right now and what the knobs currently resolve to. Since #12406
//! the cap is `maxConcurrent` and nothing else, so reading the report is how
//! an operator decides whether to move it:
//!
//! - **the host sits idle at the cap** ⇒ raise `maxConcurrent`.
//! - **the host is saturated** (low idle fraction, high load per core, little
//!   free disk or RAM) ⇒ the knob is already at/above what this machine
//!   sustains; lower it.
//!
//! The token pool's healthy-account count is still reported (it drives
//! spawn-time *selection*), but it does not bound the cap.
//!
//! It never writes config. `--write` is accepted-but-ignored with a deprecation
//! warning so an existing `loom-daemon calibrate --write` invocation in a
//! script does not start failing.
//!
//! # Two halves: pure rendering, impure measurement
//!
//! [`measure`] is the **only** function here that touches the live host (CPU
//! idle sampling, `df`, the token-pool ranking file) — it reuses exactly the
//! primitives `loom-daemon status` already uses. Everything else
//! ([`report_json`], [`report_human`])
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
    /// derived from it is what an operator reads for CPU pressure.
    pub loadavg_1m: Option<f64>,
    /// Free GB on the scratch volume, when measurable (`None` when
    /// unmeasurable — see [`crate::disk_headroom::worktree_root_free_gb`]).
    pub disk_free_gb: Option<u64>,
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
    /// per-machine **sweep-dispatch** cap, and nothing else limits it (#12406) (env > config > default —
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
    /// Role agents and sweeps are admitted by different subsystems against
    /// different queues, so they are reported as two ceilings that sum to a
    /// worst-case agent count, which is the number an operator sizing a host
    /// needs.
    pub role_agent_max_concurrent: usize,
}

impl HostMeasurements {
    /// Render this struct as the `"measurements"` JSON object for `--json`
    /// output.
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "logical_cpus": self.logical_cpus,
            "cpu_idle_fraction": self.cpu_idle_fraction,
            "loadavg_1m": self.loadavg_1m,
            "disk_free_gb": self.disk_free_gb,
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
            // The other half of this host's agent budget (#6102) — a separate
            // ceiling.
            "role_runner": {
                "enabled": self.role_runner_enabled,
                "max_concurrent": self.role_agent_max_concurrent,
                "bounded_by_work_finder_max_concurrent": false,
            },
        })
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

    // Role-runner agent budget (#6102) — read from the same root's config as
    // every other knob above.
    let role_runner_config = crate::role_runner::read_role_runner_config(workspace_root);

    HostMeasurements {
        logical_cpus,
        cpu_idle_fraction,
        loadavg_1m,
        disk_free_gb,
        token_axis_limit,
        ranking_present,
        total_accounts,
        configured_max_concurrent,
        max_concurrent_env_override,
        max_concurrent_source,
        build_slots: crate::build_slot::resolve_slots(),
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
        // The cap is `maxConcurrent` alone (#12406).
        "cap": m.configured_max_concurrent,
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
    match m.cpu_idle_fraction {
        Some(idle) if idle >= IDLE_HEADROOM_FRACTION => format!(
            "maxConcurrent={} is the cap and the host is {:.0}% idle — this machine looks \
             under-subscribed; raise autonomous.workFinder.maxConcurrent (a step at a time) and \
             re-measure.{}",
            m.configured_max_concurrent,
            idle * 100.0,
            role_agent_caveat(m)
        ),
        Some(idle) => format!(
            "maxConcurrent={} is the cap and the host is only {:.0}% idle — it is already close \
             to what this machine sustains; do not raise it further without freeing CPU.{}",
            m.configured_max_concurrent,
            idle * 100.0,
            role_agent_caveat(m)
        ),
        None => format!(
            "maxConcurrent={} is the cap, but no CPU idle sample is available on this host — \
             run this again (or read `loom-daemon status`) once the daemon has sampled, then \
             tune.{}",
            m.configured_max_concurrent,
            role_agent_caveat(m)
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
    match m.disk_free_gb {
        Some(free) => {
            out.push_str(&format!("  disk free:      {free} GB on the worktree volume\n"))
        }
        None => out.push_str("  disk free:      unmeasurable\n"),
    }
    out.push_str(
        "                  (CPU, load and disk are not cap terms since #12406 — watch them and \
         move maxConcurrent)\n",
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

    out.push_str(&format!(
        "\nConcurrency cap: {} (maxConcurrent, nothing else limits it — #12406)\n",
        m.configured_max_concurrent
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

    out.push_str(&format!("\nReading: {}\n", advice(m)));
    out.push_str(
        "\nThis report is read-only — calibrate no longer recommends or writes knob values \
         (#4512): maxConcurrent is a per-machine knob you tune from observations like the ones \
         above. The daemon re-reads it every tick, so an edit applies without a restart \
         (unless LOOM_WORK_FINDER_MAX_CONCURRENT is set).\n",
    );

    out
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
            disk_free_gb: Some(72),
            token_axis_limit: 6,
            ranking_present: true,
            total_accounts: 8,
            configured_max_concurrent: 3,
            max_concurrent_env_override: false,
            max_concurrent_source: Some(std::path::PathBuf::from("/repo/.loom/config.json")),
            build_slots: 1,
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
    fn advice_handles_a_host_with_no_idle_sample() {
        let mut m = idle_worker_host();
        m.cpu_idle_fraction = None;
        assert!(advice(&m).contains("no CPU idle sample"));
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
        for m in [idle_worker_host(), saturated_host()] {
            let text = advice(&m);
            assert!(!text.contains("#6102"), "role runner disabled ⇒ no role-agent caveat: {text}");
        }
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
            "the cap block must scope itself: {text}"
        );
        assert!(
            text.contains("worst-case agents on this host: 3 sweep(s) + 7 role agent(s) = 10"),
            "{text}"
        );
    }

    // ========================================================================
    // JSON round-trip
    // ========================================================================

    #[test]
    fn json_round_trips_measurements_and_the_cap() {
        let m = idle_worker_host();
        let report = report_json(&m);

        assert_eq!(report["measurements"]["logical_cpus"], 8);
        assert_eq!(report["measurements"]["cpu_idle_fraction"], 0.95);
        assert_eq!(report["measurements"]["configured_max_concurrent"], 3);
        assert_eq!(report["measurements"]["build_slots"], 1);
        assert_eq!(report["cap"], 3);
        assert!(report.get("binding_term").is_none(), "no binding term since #12406");
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

    // ========================================================================
    // Human report
    // ========================================================================

    #[test]
    fn human_report_contains_key_lines() {
        let text = report_human(&idle_worker_host());
        assert!(text.contains("loom-daemon calibrate"));
        assert!(text.contains("Concurrency cap: 3 (maxConcurrent"));
        assert!(text.contains("build slots:    1"));
        assert!(text.contains("disk free:      72 GB"));
        assert!(!text.contains("LOOM_PER_WORKTREE_GB"), "no disk term since #12406");
        assert!(!text.contains("min(disk"), "no disk term since #12406");
        assert!(text.contains("read-only"));
    }

    #[test]
    fn human_report_flags_a_disabled_build_slot() {
        let mut m = idle_worker_host();
        m.build_slots = 0;
        assert!(report_human(&m).contains("0 — DISABLED"));
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
}
