//! Do not start a gate the host is too loaded to finish (Issue #10955).
//!
//! At a load of 150 to 240 the workspace nextest run alone needed more than the
//! whole `timeoutSeconds` budget, so every run timed out and each timeout added
//! load. Budgets are not changed here (the operator tunes them later). Instead,
//! above the threshold the orchestrator-side gate already uses
//! (`buildGate.loadThreshold`, default 0.9 load per CPU, #4259), pre-flight
//! returns [`super::Verdict::Deferred`] without running: it is neither a
//! failure nor a timeout, and the claim is kept.
//!
//! The deferral is bounded the same way #4259's is: once deferrals in one
//! episode have spanned `buildGate.maxDeferSeconds` (default 1800 s), the gate
//! runs regardless of load. A reading that cannot be taken never defers.

use std::path::Path;

/// One host-load reading: the 1-minute load average and the CPU count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct HostLoad {
    pub loadavg_1m: Option<f64>,
    pub cpus: usize,
}

impl HostLoad {
    /// The live reading.
    pub(super) fn sample() -> Self {
        Self {
            loadavg_1m: crate::cpu_headroom::read_loadavg_1m(),
            cpus: crate::cpu_headroom::logical_cpu_count(),
        }
    }

    /// No reading: never defers.
    pub(super) const UNKNOWN: Self = Self {
        loadavg_1m: None,
        cpus: 0,
    };
}

/// What the load check decided for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Decision {
    /// Run the gate. `forced` when the host is still loaded but the max-defer
    /// window has run out.
    Run { forced: bool },
    /// Do not run yet.
    Defer {
        /// Load per CPU and the threshold, formatted for the message.
        load_per_cpu: String,
        threshold: String,
        /// Seconds since this episode's first deferral, and the bound.
        waited_secs: u64,
        max_secs: u64,
    },
}

/// Decide whether to run, given `load` at `now` (epoch seconds) and the time
/// this episode first deferred (`deferred_since`, updated in place).
pub(super) fn decide(
    worktree: &Path,
    load: HostLoad,
    now: u64,
    deferred_since: &mut Option<u64>,
) -> Decision {
    let threshold = crate::main_health_gate::resolve_gate_load_threshold(worktree);
    let saturated = crate::cpu_headroom::is_host_saturated(load.loadavg_1m, load.cpus, threshold);
    if saturated != Some(true) {
        *deferred_since = None;
        return Decision::Run { forced: false };
    }
    let max_secs = crate::main_health_gate::resolve_gate_max_defer(worktree).as_secs();
    let since = *deferred_since.get_or_insert(now);
    let waited_secs = now.saturating_sub(since);
    if waited_secs >= max_secs {
        *deferred_since = None;
        return Decision::Run { forced: true };
    }
    let per_cpu = crate::cpu_headroom::load_per_core_from(load.loadavg_1m, load.cpus);
    Decision::Defer {
        load_per_cpu: format!("{:.2}", per_cpu.unwrap_or_default()),
        threshold: format!("{threshold:.2}"),
        waited_secs,
        max_secs,
    }
}

/// Now, in epoch seconds.
pub(super) fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
