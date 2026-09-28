//! The red-main-fix lane (#9244 §6).
//!
//! An issue whose body carries `<!-- loom:main-red-fix -->` claims to fix a
//! red `main`. Two things follow, and **only while that repo's `main` is
//! verified red**:
//!
//! - it sorts ahead of every unstarred candidate (key 3 of
//!   [`super::ordering::candidate_cmp`]);
//! - a repo the main-health gate has halted still admits it — and nothing
//!   else. Every other ready issue in that repo, starred or not, keeps the
//!   `WorkspaceHalted` disposition.
//!
//! "Verified red" is [`WorkspaceHealthStates::is_halted`], never the
//! work finder's per-root `halted` hold, which is also set while a gate run is
//! merely in flight (`suppress_dispatch_during_gate`), during a drain, under
//! the host breaker and by the pre-flight / pool holds. None of those is a red
//! `main`, so none of them opens the lane. A repo whose gate is disabled has
//! no verified-red signal at all, so the latest default-branch CI conclusion
//! stands in for it — one cached forge read per repo per tick, made only when
//! the repo has a marker-bearing candidate.
//!
//! A marker on a green repo is inert: no boost, no halt bypass.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::WorkItem;
use crate::main_health_gate::{MainHealthState, WorkspaceHealthStates};

/// The body marker declaring an issue a fix for a red `main` (#9244).
pub const MAIN_RED_FIX_MARKER: &str = "<!-- loom:main-red-fix -->";

/// How long one repo's CI-fallback verdict is reused: about one tick, so the
/// fallback costs at most one forge read per repo per tick.
pub const CI_FALLBACK_TTL: Duration = Duration::from_secs(60);

impl WorkItem {
    /// True when [`Self::body`] carries [`MAIN_RED_FIX_MARKER`] at the start
    /// of a line (the same line-anchored style as the complexity and
    /// recheck-interval markers, so prose quoting the marker mid-sentence does
    /// not fire).
    #[must_use]
    pub fn is_main_red_fix(&self) -> bool {
        self.body.as_deref().is_some_and(|b| {
            b.lines()
                .any(|l| l.trim_start().starts_with(MAIN_RED_FIX_MARKER))
        })
    }
}

/// One repo's inputs to the red-main-fix lane, resolved once per tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RedMainLane {
    /// The main-health gate has verified this repo's `main` red
    /// ([`WorkspaceHealthStates::is_halted`]).
    pub verified_red: bool,
    /// This repo has no enabled `buildGate`, so the CI fallback decides red.
    pub gate_disabled: bool,
    /// Any hold on this repo other than the verified-red halt: a gate run in
    /// flight, a drain, the host breaker, a pre-flight or token-pool hold.
    pub other_hold: bool,
}

impl RedMainLane {
    /// Whether a halted repo still admits its red-main fixes this tick: only
    /// when the halt is the verified-red one and nothing else holds the repo.
    #[must_use]
    pub fn admits_fixes_while_halted(&self) -> bool {
        self.verified_red && !self.other_hold
    }

    /// Whether this repo's `main` counts as red for key 3. `ci_red` is called
    /// only when the gate is disabled and `ready` has a marker-bearing issue.
    pub fn is_red(&self, ready: &[WorkItem], ci_red: impl FnOnce() -> bool) -> bool {
        self.verified_red
            || (self.gate_disabled && ready.iter().any(WorkItem::is_main_red_fix) && ci_red())
    }
}

/// The lane for one repo, from its own health state.
#[must_use]
pub fn lane_for(
    health: &MainHealthState,
    root: &Path,
    suppress_dispatch_during_gate: bool,
    other_hold: bool,
) -> RedMainLane {
    RedMainLane {
        verified_red: health.is_halted(),
        gate_disabled: crate::main_health_gate::read_build_gate_config(root).is_none(),
        other_hold: other_hold || (suppress_dispatch_during_gate && health.is_gate_in_flight()),
    }
}

/// The lanes for every root, parallel to `roots`. `preflight_held` is the
/// per-root pre-flight / pool hold slice; `global_hold` is the daemon-wide
/// drain / host-breaker hold.
#[must_use]
pub fn lanes_per_root(
    health_states: &WorkspaceHealthStates,
    roots: &[PathBuf],
    suppress_dispatch_during_gate: bool,
    preflight_held: &[bool],
    global_hold: bool,
) -> Vec<RedMainLane> {
    roots
        .iter()
        .enumerate()
        .map(|(i, root)| {
            let held = global_hold || preflight_held.get(i).copied().unwrap_or(false);
            lane_for(&health_states.get_or_create(root), root, suppress_dispatch_during_gate, held)
        })
        .collect()
}

fn ci_cache() -> &'static Mutex<HashMap<PathBuf, (Instant, bool)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Instant, bool)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `root`'s latest default-branch CI conclusion is a failure — the fallback
/// for a repo with no enabled gate. Cached for [`CI_FALLBACK_TTL`]. Any
/// failure to answer (breaker suppressing, `gh` missing, no runs) is "not
/// red": the fallback can only ever grant the boost on positive evidence.
#[must_use]
pub fn ci_main_red(root: &Path) -> bool {
    let now = Instant::now();
    if let Some((at, red)) = ci_cache().lock().ok().and_then(|c| c.get(root).copied()) {
        if now.saturating_duration_since(at) < CI_FALLBACK_TTL {
            return red;
        }
    }
    let red = !crate::rate_limit_breaker::global_is_suppressed() && probe_ci_main_red(root);
    if let Ok(mut cache) = ci_cache().lock() {
        cache.insert(root.to_path_buf(), (now, red));
    }
    red
}

/// The branch the CI fallback reads: the repo's default branch from
/// `origin/HEAD`, else `main` when that symbolic ref is unset.
pub(super) fn default_branch_for(root: &Path) -> String {
    crate::worktree_ops::clean::default_branch(root).unwrap_or_else(|| "main".to_string())
}

fn probe_ci_main_red(root: &Path) -> bool {
    let branch = default_branch_for(root);
    let mut cmd = Command::new("gh");
    cmd.args(["run", "list", "--branch", &branch, "--limit", "30"])
        .args(["--json", "headSha,status,conclusion,workflowName"])
        .current_dir(root)
        .stdin(Stdio::null());
    crate::credential_preflight::apply_gh_config_for_cwd(&mut cmd, Some(root));
    let unavailable = |why: &str| {
        log::debug!("work_finder: red-main CI fallback unavailable for {} ({why})", root.display());
        false
    };
    match crate::sweep_registry::reaper::output_with_timeout(cmd, Duration::from_secs(30)) {
        Ok(Some(out)) if out.status.success() => {
            latest_run_is_failure(&String::from_utf8_lossy(&out.stdout))
        }
        Ok(Some(out)) => {
            // Feed the rate-limit breaker like every other forge read, so an
            // exhausted pool trips it instead of being retried every minute.
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, "work_finder_main_red_ci");
            unavailable(&stderr)
        }
        Ok(None) => unavailable("timed out"),
        Err(e) => unavailable(&e.to_string()),
    }
}

/// Whether the newest commit in a `gh run list --json headSha,…` payload
/// (newest first) has a failed CI run: any run for that SHA concluded
/// `failure`, `timed_out` or `startup_failure` — the same conclusions the
/// main-health gate's forge-CI reducer treats as a failure verdict.
#[must_use]
pub fn latest_run_is_failure(stdout: &str) -> bool {
    let Ok(runs) = serde_json::from_str::<Vec<serde_json::Value>>(stdout) else {
        return false;
    };
    let field = |r: &serde_json::Value, k: &str| {
        r.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let Some(sha) = runs.first().and_then(|r| field(r, "headSha")) else {
        return false;
    };
    runs.iter()
        .filter(|r| field(r, "headSha").as_deref() == Some(sha.as_str()))
        .any(|r| {
            matches!(
                field(r, "conclusion").as_deref(),
                Some("failure" | "timed_out" | "startup_failure")
            )
        })
}
