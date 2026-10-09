//! Reaping what an agent leaves behind (issue #10802).
//!
//! A process an agent starts can outlive it: `vite` and `workerd` dev servers
//! `setsid`, and survive the agent's process group, its sweep and its
//! worktree. On loom-worker-1 four `loom-agent-*` scopes were kept alive for
//! days that way (about 2.7 GB), and failed scopes piled up (31 on worker-1).
//! [`crate::orphan_process_reaper`] cannot see any of it: it only walks
//! `issue-<N>` worktrees that still exist, are registered and are provably
//! unowned. This module adds three passes, all sharing the
//! `autonomous.processReaper.{enabled,dryRun,minAgeSecs}` switches and the
//! `LOOM_ORPHAN_PROCESS_REAPER*` env (one switch for all residue reaping):
//!
//! 1. **Exit teardown** ([`request_exit_teardown`]): when a sweep or a role
//!    run reaches a terminal state, for any reason, its systemd scope is
//!    stopped and its recorded process tree is torn down; for a sweep,
//!    processes in its worktree that started at or after the agent did **and
//!    carry its `LOOM_SWEEP_ID` in their environment** are reaped too. No
//!    grace period: the agent is definitively gone. The scope and identity
//!    legs are the shared #10831 teardown helper
//!    ([`crate::auto_update::pause_roll::teardown`]); the worktree leg reuses
//!    [`plan_orphan_trees`]'s fail-safes with the start-time bound standing in
//!    for the age gate.
//! 2. **Periodic pass** ([`run_periodic`]): `loom-agent-*` scopes no live run
//!    claims are stopped, and processes living in a deleted worktree, or in a
//!    `.loom-managed` worktree of a workspace that is no longer registered,
//!    are reaped.
//! 3. **Failed scopes**: after the outcome is recorded, `loom-agent-*` units in
//!    the `failed` state are cleared with `systemctl --user reset-failed`.
//!
//! # Safety
//!
//! A process is touched only through (a) its `loom-agent-*` scope, (b) the
//! run's recorded pid and process group, or (c) the worktree attribution plus
//! the age rule (periodic) or the start-time rule *and* the run's own
//! `LOOM_SWEEP_ID` marker (exit: a later start alone is not ownership; an
//! unreadable environment is not reaped), with the daemon's
//! own ancestry and children and any live agent runtime protected. Never by
//! process name, never a unit outside `loom-agent-*` (`loom-agent-probe-*`
//! included). `dryRun` records the plan and signals, stops and resets nothing.
//!
//! # Records
//!
//! Every reap is logged and emits `daemon.agent_residue.reaped` on the event
//! bus ([`install_event_bus`]), and increments a counter in
//! [`crate::types::AgentResidueStatus`] (`status --json` → `agent_residue`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::auto_update::pause_roll::teardown::{
    process_table, run_bounded, Proc, ScopeCtl, TeardownReport, TreeSpec, TERM_GRACE,
};
use crate::orphan_process_reaper::{
    looks_like_agent, ownership_gate, plan_orphan_trees, production_hooks, reap_tree,
    references_worktree, snapshot_processes, OrphanTree, OwnershipVerdict, ProcEntry, ReapHooks,
    DEFAULT_TERM_GRACE,
};
use crate::types::AgentResidueStatus;

/// The topic published for every reap.
pub const EVENT_TOPIC: &str = "daemon.agent_residue.reaped";
/// Every agent scope starts with this.
pub const SCOPE_PREFIX: &str = "loom-agent-";
/// The spawn script's preflight probe scopes, which are never touched.
pub const PROBE_PREFIX: &str = "loom-agent-probe-";
/// How much later than a scope a process may have started and still be the
/// scope's own spawn process (`systemd-run --scope` execs it in place).
const SCOPE_PID_TOLERANCE_SECS: i64 = 30;
/// How much later than the agent's recorded start a process of that agent may
/// have started: the registry stamps `started_at` just after the spawn.
const START_TOLERANCE_SECS: u64 = 5;
/// Bound on each `systemctl` call the scope pass makes.
const CTL_TIMEOUT: Duration = Duration::from_secs(5);
/// The sweep-identity variable every daemon-dispatched sweep child is given
/// (#8835, `sweep_registry::dispatch::child_env_markers::SWEEP_ID_ENV`) and
/// its descendants inherit, `setsid` or not. The exit leg's ownership proof.
pub const RUN_MARKER_ENV: &str = "LOOM_SWEEP_ID";

// ============================================================================
// Records
// ============================================================================

/// Which pass reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResiduePath {
    Exit,
    Periodic,
}

/// What was reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidueKind {
    Scope,
    Tree,
    FailedScope,
}

impl ResiduePath {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exit => "exit",
            Self::Periodic => "periodic",
        }
    }
}

impl ResidueKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::Tree => "tree",
            Self::FailedScope => "failed-scope",
        }
    }
}

/// One reap (or, under `dryRun`, one planned reap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapRecord {
    pub path: ResiduePath,
    pub kind: ResidueKind,
    pub unit: Option<String>,
    pub pids: Vec<u32>,
    pub issue: Option<u32>,
    pub sweep_id: Option<String>,
    pub dry_run: bool,
    /// Why, or the outcome (`Result`/`ExecMainStatus` for a failed scope).
    pub detail: Option<String>,
}

impl ReapRecord {
    fn new(path: ResiduePath, kind: ResidueKind, dry_run: bool) -> Self {
        Self {
            path,
            kind,
            unit: None,
            pids: Vec::new(),
            issue: None,
            sweep_id: None,
            dry_run,
            detail: None,
        }
    }

    /// The event payload.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "source": "agent-residue-reaper",
            "path": self.path.as_str(),
            "kind": self.kind.as_str(),
            "unit": self.unit,
            "pids": self.pids,
            "issue": self.issue,
            "sweep_id": self.sweep_id,
            "dry_run": self.dry_run,
            "detail": self.detail,
        })
    }
}

/// Where records go.
pub trait Recorder {
    fn record(&self, record: &ReapRecord);
}

impl Recorder for Mutex<Vec<ReapRecord>> {
    fn record(&self, record: &ReapRecord) {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record.clone());
    }
}

static BUS: OnceLock<Arc<crate::event_bus::EventBus>> = OnceLock::new();
static STATUS: Mutex<AgentResidueStatus> = Mutex::new(AgentResidueStatus::new());

/// Give the production recorder the daemon's event bus. Idempotent.
pub fn install_event_bus(bus: Arc<crate::event_bus::EventBus>) {
    let _ = BUS.set(bus);
}

/// The counters `status --json` reports.
#[must_use]
pub fn status_snapshot() -> AgentResidueStatus {
    STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Fold one record into the counters. Planned (dry-run) reaps are counted
/// apart, so the real counters mean "reaped".
pub fn count(status: &mut AgentResidueStatus, record: &ReapRecord, at: DateTime<Utc>) {
    if record.dry_run {
        status.dry_run_planned += 1;
        return;
    }
    match record.kind {
        ResidueKind::Scope => status.scope += 1,
        ResidueKind::Tree => status.tree += 1,
        ResidueKind::FailedScope => status.failed_scope += 1,
    }
    status.last_reap_at = Some(at);
}

/// Logs, counts and publishes.
pub struct ProductionRecorder;

impl Recorder for ProductionRecorder {
    fn record(&self, r: &ReapRecord) {
        log::warn!(
            "agent_residue_reaper: {} {} {}: unit={:?} pids={:?} issue={:?} sweep={:?} \
             detail={:?}{}",
            if r.dry_run { "would reap" } else { "reaped" },
            r.path.as_str(),
            r.kind.as_str(),
            r.unit,
            r.pids,
            r.issue,
            r.sweep_id,
            r.detail,
            if r.dry_run { " (dry-run)" } else { "" }
        );
        count(
            &mut STATUS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            r,
            Utc::now(),
        );
        if let Some(bus) = BUS.get() {
            let _ = bus.publish_generic(EVENT_TOPIC, r.payload());
        }
    }
}

// ============================================================================
// Config
// ============================================================================

/// The resolved switches for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub dry_run: bool,
    pub min_age_secs: u64,
}

/// Resolve the switches across `roots` (the pass is host-wide, so: on when any
/// root has it on, dry-run when any root says so, the longest grace wins).
#[must_use]
pub fn resolve_settings(roots: &[PathBuf]) -> Settings {
    let mut out = Settings {
        enabled: false,
        dry_run: false,
        min_age_secs: 0,
    };
    for root in roots {
        let config = crate::orphan_process_reaper::read_config(root);
        out.enabled |= crate::orphan_process_reaper::resolve_enabled(&config);
        out.dry_run |= crate::orphan_process_reaper::resolve_dry_run(&config);
        out.min_age_secs = out
            .min_age_secs
            .max(crate::orphan_process_reaper::resolve_min_age_secs(&config));
    }
    out
}

// ============================================================================
// systemd
// ============================================================================

/// One `loom-agent-*` scope unit, as systemd reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeInfo {
    pub name: String,
    /// `active`, `failed`, `inactive`, …
    pub active_state: String,
    /// Seconds since the scope became active, when known.
    pub age_secs: Option<u64>,
    /// `Result=` (failed scopes).
    pub result: Option<String>,
    /// `ExecMainStatus=` (failed scopes).
    pub exec_main_status: Option<String>,
}

/// `systemctl --user`, injectable.
pub trait SystemCtl {
    /// Every `loom-agent-*` scope unit.
    fn list_scopes(&self) -> Vec<ScopeInfo>;
    /// Stop a scope, killing every process in it.
    ///
    /// # Errors
    /// Why the scope was not stopped.
    fn stop(&self, unit: &str) -> Result<(), String>;
    /// Clear a failed unit.
    ///
    /// # Errors
    /// Why it was not cleared.
    fn reset_failed(&self, unit: &str) -> Result<(), String>;
}

/// No systemd (macOS, or `systemctl --user` unavailable).
pub struct NoSystemd;

impl SystemCtl for NoSystemd {
    fn list_scopes(&self) -> Vec<ScopeInfo> {
        Vec::new()
    }
    fn stop(&self, unit: &str) -> Result<(), String> {
        Err(format!("no systemd: cannot stop {unit}"))
    }
    fn reset_failed(&self, unit: &str) -> Result<(), String> {
        Err(format!("no systemd: cannot reset {unit}"))
    }
}

/// The real `systemctl --user`.
pub struct SystemdUser {
    ctl: ScopeCtl,
}

impl SystemdUser {
    /// `None` off Linux, or when the user manager does not answer.
    #[must_use]
    pub fn system() -> Option<Self> {
        let ctl = ScopeCtl::system()?;
        let me = Self { ctl };
        me.run(&[
            "list-units",
            "--no-pager",
            "--no-legend",
            "--plain",
            "--type=scope",
            "x-probe",
        ])
        .map(|_| me)
    }

    fn run(&self, args: &[&str]) -> Option<(bool, String)> {
        let mut cmd = Command::new(&self.ctl.systemctl);
        cmd.arg("--user").args(args);
        run_bounded(cmd, CTL_TIMEOUT)
            .map(|(ok, out)| (ok, String::from_utf8_lossy(&out).trim().to_string()))
    }
}

/// Parse `list-units --plain --no-legend` into `(name, active_state)` rows for
/// agent scopes. Pure.
#[must_use]
pub fn parse_list_units(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let mut name = it.next()?;
            if name == "●" || name == "*" {
                name = it.next()?;
            }
            let _load = it.next()?;
            let active = it.next()?;
            is_agent_scope(name).then(|| (name.to_string(), active.to_string()))
        })
        .collect()
}

/// `show --property=…` output → `key → value`. Pure.
#[must_use]
pub fn parse_show(out: &str) -> HashMap<String, String> {
    out.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(target_os = "linux")]
fn monotonic_micros() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (rc == 0).then(|| {
        u64::try_from(ts.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000)
            .saturating_add(u64::try_from(ts.tv_nsec).unwrap_or(0) / 1000)
    })
}

#[cfg(not(target_os = "linux"))]
fn monotonic_micros() -> Option<u64> {
    None
}

impl SystemCtl for SystemdUser {
    fn list_scopes(&self) -> Vec<ScopeInfo> {
        let Some((true, out)) = self.run(&[
            "list-units",
            "--all",
            "--no-pager",
            "--no-legend",
            "--plain",
            "--type=scope",
            "loom-agent-*.scope",
        ]) else {
            return Vec::new();
        };
        let now = monotonic_micros();
        parse_list_units(&out)
            .into_iter()
            .map(|(name, active_state)| {
                let props = self
                    .run(&[
                        "show",
                        &name,
                        "--property=ActiveEnterTimestampMonotonic",
                        "--property=Result",
                        "--property=ExecMainStatus",
                    ])
                    .map(|(_, o)| parse_show(&o))
                    .unwrap_or_default();
                let entered = props
                    .get("ActiveEnterTimestampMonotonic")
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|v| *v > 0);
                ScopeInfo {
                    age_secs: entered
                        .zip(now)
                        .map(|(e, n)| n.saturating_sub(e) / 1_000_000),
                    result: props.get("Result").cloned(),
                    exec_main_status: props.get("ExecMainStatus").cloned(),
                    name,
                    active_state,
                }
            })
            .collect()
    }

    fn stop(&self, unit: &str) -> Result<(), String> {
        if !is_agent_scope(unit) {
            return Err(format!("refusing to stop {unit}: not a loom-agent scope"));
        }
        self.ctl.stop(unit)
    }

    fn reset_failed(&self, unit: &str) -> Result<(), String> {
        if !is_agent_scope(unit) {
            return Err(format!("refusing to reset {unit}: not a loom-agent scope"));
        }
        match self.run(&["reset-failed", unit]) {
            Some((true, _)) => Ok(()),
            other => Err(format!("systemctl reset-failed {unit} failed: {other:?}")),
        }
    }
}

/// Whether `name` is a `loom-agent-*` scope this module may act on.
#[must_use]
pub fn is_agent_scope(name: &str) -> bool {
    name.starts_with(SCOPE_PREFIX) && name.ends_with(".scope") && !name.starts_with(PROBE_PREFIX)
}

/// The spawn pid embedded in `loom-agent-<pid>-<rand>.scope`.
#[must_use]
pub fn embedded_pid(name: &str) -> Option<u32> {
    if !is_agent_scope(name) {
        return None;
    }
    let rest = name.strip_prefix(SCOPE_PREFIX)?;
    let (pid, _) = rest.split_once('-')?;
    pid.parse().ok().filter(|p| *p > 1)
}

/// Whether live `pid` is the scope's own spawn process: it started no later
/// than the scope did (plus a tolerance), so it is not a recycled number.
/// An unreadable start time counts as a match: never guess a claim away.
#[must_use]
pub fn pid_matches_scope(
    proc_started: Option<DateTime<Utc>>,
    scope_age_secs: u64,
    now: DateTime<Utc>,
) -> bool {
    let Some(started) = proc_started else {
        return true;
    };
    let scope_start = now - chrono::Duration::seconds(i64::try_from(scope_age_secs).unwrap_or(0));
    started <= scope_start + chrono::Duration::seconds(SCOPE_PID_TOLERANCE_SECS)
}

// ============================================================================
// Periodic scope pass
// ============================================================================

/// What the periodic scope pass does to one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeAction {
    /// Stop an unclaimed scope.
    Stop { unit: String },
    /// Record a failed scope's outcome, then clear it.
    ResetFailed {
        unit: String,
        result: Option<String>,
        exec_main_status: Option<String>,
    },
}

/// Decide the scope actions. `claimed_units` are scope names recorded by live
/// runs; `pid_claimed(pid, scope_age)` says whether the embedded pid is alive
/// and is that scope's own process. Pure.
#[must_use]
pub fn plan_scopes(
    scopes: &[ScopeInfo],
    claimed_units: &HashSet<String>,
    pid_claimed: &dyn Fn(u32, u64) -> bool,
    min_age_secs: u64,
) -> Vec<ScopeAction> {
    let mut out = Vec::new();
    for scope in scopes.iter().filter(|s| is_agent_scope(&s.name)) {
        match scope.active_state.as_str() {
            "failed" => out.push(ScopeAction::ResetFailed {
                unit: scope.name.clone(),
                result: scope.result.clone(),
                exec_main_status: scope.exec_main_status.clone(),
            }),
            "active" => {
                // An unknown age is never guessed at.
                let Some(age) = scope.age_secs.filter(|a| *a >= min_age_secs) else {
                    continue;
                };
                if claimed_units.contains(&scope.name) {
                    continue;
                }
                if embedded_pid(&scope.name).is_some_and(|pid| pid_claimed(pid, age)) {
                    continue;
                }
                out.push(ScopeAction::Stop {
                    unit: scope.name.clone(),
                });
            }
            _ => {}
        }
    }
    out
}

/// Execute the scope actions, recording each.
pub fn run_scope_actions(
    actions: &[ScopeAction],
    ctl: &dyn SystemCtl,
    recorder: &dyn Recorder,
    dry_run: bool,
) {
    for action in actions {
        match action {
            ScopeAction::Stop { unit } => {
                let mut rec = ReapRecord::new(ResiduePath::Periodic, ResidueKind::Scope, dry_run);
                rec.unit = Some(unit.clone());
                rec.detail = Some("no live run claims this scope".to_string());
                if dry_run {
                    recorder.record(&rec);
                } else {
                    match ctl.stop(unit) {
                        Ok(()) => recorder.record(&rec),
                        Err(e) => log::warn!("agent_residue_reaper: stop {unit}: {e}"),
                    }
                }
            }
            ScopeAction::ResetFailed {
                unit,
                result,
                exec_main_status,
            } => {
                let mut rec =
                    ReapRecord::new(ResiduePath::Periodic, ResidueKind::FailedScope, dry_run);
                rec.unit = Some(unit.clone());
                rec.detail = Some(format!(
                    "Result={} ExecMainStatus={}",
                    result.as_deref().unwrap_or("?"),
                    exec_main_status.as_deref().unwrap_or("?")
                ));
                // The outcome is recorded before the unit (and with it the
                // outcome) is cleared.
                recorder.record(&rec);
                if !dry_run {
                    if let Err(e) = ctl.reset_failed(unit) {
                        log::warn!("agent_residue_reaper: {e}");
                    }
                }
            }
        }
    }
}

// ============================================================================
// Periodic worktree pass
// ============================================================================

/// Probes for the worktree pass.
pub struct WorktreeProbes<'a> {
    pub exists: &'a dyn Fn(&Path) -> bool,
    pub is_managed: &'a dyn Fn(&Path) -> bool,
    /// A live claim on `(repo_root, issue)`, with its evidence.
    pub live_claim: &'a dyn Fn(&Path, u32) -> Option<String>,
}

/// A path's last component: records carry no absolute host paths.
fn dir_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Strip the kernel's ` (deleted)` marker off a cwd, reporting whether it was
/// there.
fn strip_deleted(cwd: &Path) -> (PathBuf, bool) {
    let s = cwd.to_string_lossy();
    s.strip_suffix(" (deleted)")
        .map_or_else(|| (cwd.to_path_buf(), false), |p| (PathBuf::from(p), true))
}

/// `(<repo>/.loom/worktrees/issue-<N>, N, <repo>)` for a path inside one.
#[must_use]
pub fn issue_worktree_of(path: &Path) -> Option<(PathBuf, u32, PathBuf)> {
    let comps: Vec<Component<'_>> = path.components().collect();
    for i in 2..comps.len().saturating_sub(1) {
        if comps[i].as_os_str() == "worktrees" && comps[i - 1].as_os_str() == ".loom" {
            let issue = crate::worktree_ops::naming::issue_from_worktree(
                &comps[i + 1].as_os_str().to_string_lossy(),
            )?;
            let worktree: PathBuf = comps[..=i + 1].iter().collect();
            let repo: PathBuf = comps[..i - 1].iter().collect();
            return Some((worktree, issue, repo));
        }
    }
    None
}

/// The absolute paths an argv names (bare, or after `=`, `:`, `,` or a
/// quote), for candidate discovery where no cwd is readable (the `ps`
/// snapshot). Pure.
#[must_use]
pub fn argv_paths(cmdline: &str) -> Vec<PathBuf> {
    cmdline
        .split(|c: char| c.is_whitespace() || matches!(c, '=' | ':' | ',' | '\'' | '"'))
        .filter(|s| s.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

/// What the worktree pass found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreePlan {
    pub trees: Vec<OrphanTree>,
    pub skipped: Vec<(u32, String)>,
}

/// Plan the reap of processes living in a deleted worktree, or in a
/// `.loom-managed` worktree of a workspace that is no longer registered.
///
/// Candidate worktrees are the `.loom/worktrees/issue-<N>` directories a
/// process's cwd is in or its argv names (the `ps` snapshot off Linux has no
/// cwd, so argv is all it has), so an operator process elsewhere in the same
/// checkout is left alone. The per-process fail-safes (age gate, daemon
/// ancestry and children, live agent runtime, tree cap) are
/// [`plan_orphan_trees`]'s own.
#[must_use]
pub fn plan_worktree_residue(
    procs: &[ProcEntry],
    registered: &[PathBuf],
    me: u32,
    min_age_secs: u64,
    probes: &WorktreeProbes<'_>,
) -> WorktreePlan {
    let mut plan = WorktreePlan::default();
    let mut normalized: Vec<ProcEntry> = Vec::with_capacity(procs.len());
    let mut candidates: BTreeMap<PathBuf, u32> = BTreeMap::new();
    let mut repos: HashMap<PathBuf, PathBuf> = HashMap::new();
    for p in procs {
        let mut p = p.clone();
        p.cwd = p.cwd.as_deref().map(|cwd| strip_deleted(cwd).0);
        for path in p.cwd.iter().cloned().chain(argv_paths(&p.cmdline)) {
            if let Some((worktree, issue, repo)) = issue_worktree_of(&path) {
                candidates.insert(worktree.clone(), issue);
                repos.insert(worktree, repo);
            }
        }
        normalized.push(p);
    }
    let mut selected: Vec<(u32, PathBuf)> = Vec::new();
    for (worktree, issue) in candidates {
        if !(probes.exists)(&worktree) {
            selected.push((issue, worktree));
            continue;
        }
        let repo = repos.get(&worktree).cloned().unwrap_or_default();
        if registered.iter().any(|r| r == &repo) {
            // A registered workspace's live worktree is the orphan reaper's.
            continue;
        }
        let claim = |n: u32| (probes.live_claim)(&repo, n);
        match ownership_gate(&worktree, issue, probes.is_managed, &HashSet::new(), &claim, &|_| {
            None
        }) {
            OwnershipVerdict::Unowned => selected.push((issue, worktree)),
            OwnershipVerdict::Skip(why) => plan.skipped.push((issue, why)),
            OwnershipVerdict::LiveSweep(_) => {}
        }
    }
    let outcome = plan_orphan_trees(&normalized, &selected, me, min_age_secs, &HashMap::new());
    plan.trees = outcome.trees;
    plan.skipped.extend(outcome.skipped);
    plan
}

/// Reap the planned trees, recording each.
pub fn run_worktree_plan(
    plan: &WorktreePlan,
    hooks: &ReapHooks<'_>,
    recorder: &dyn Recorder,
    dry_run: bool,
) {
    for (issue, why) in &plan.skipped {
        log::debug!("agent_residue_reaper: issue-{issue} left alone: {why}");
    }
    for tree in &plan.trees {
        let mut rec = ReapRecord::new(ResiduePath::Periodic, ResidueKind::Tree, dry_run);
        rec.issue = Some(tree.issue);
        rec.pids.clone_from(&tree.pids);
        rec.detail = Some(format!("worktree {}", dir_name(&tree.worktree)));
        if !dry_run {
            let _ = reap_tree(tree, DEFAULT_TERM_GRACE, hooks);
        }
        recorder.record(&rec);
    }
}

// ============================================================================
// Exit teardown
// ============================================================================

/// What the exit teardown needs to know about the finished run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitRequest {
    /// The sweep id, or a role run's item id (`role-<role>-…`).
    pub sweep_id: String,
    pub issue: Option<u32>,
    pub pid: u32,
    pub pgid: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub workspace_root: PathBuf,
    /// `scope_unit` from the claim lock's `owner.json`, when it names one.
    pub scope_unit: Option<String>,
}

#[derive(serde::Deserialize)]
struct OwnerView {
    #[serde(default)]
    scope_unit: Option<String>,
    #[serde(default)]
    sweep_id: Option<String>,
    #[serde(default)]
    owner_pid: u32,
}

fn read_owner(root: &Path, issue: u32) -> Option<OwnerView> {
    let path = crate::worktree_ops::liveness::locks_dir(root)
        .join(format!("issue-{issue}"))
        .join("owner.json");
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

impl ExitRequest {
    /// Capture a terminal run's identity. Call before its claim lock is
    /// released: the recorded scope unit lives in the lock.
    #[must_use]
    pub fn capture(
        workspace_root: &Path,
        sweep_id: &str,
        issue: Option<u32>,
        pid: u32,
        pgid: Option<u32>,
        started_at: DateTime<Utc>,
    ) -> Self {
        let scope_unit = issue
            .and_then(|n| read_owner(workspace_root, n))
            .filter(|o| o.sweep_id.as_deref().is_none_or(|s| s == sweep_id))
            .and_then(|o| o.scope_unit);
        Self {
            sweep_id: sweep_id.to_string(),
            issue,
            pid,
            pgid,
            started_at,
            workspace_root: workspace_root.to_path_buf(),
            scope_unit,
        }
    }

    /// A finished role run (Champion, Judge, …): its synthetic item id stands
    /// in for the sweep id, it leads its own process group, and its scope is
    /// the one recorded at launch. No issue, so no worktree leg.
    #[must_use]
    pub fn for_role(
        workspace_root: &Path,
        item_id: &str,
        pid: u32,
        started_at: DateTime<Utc>,
        scope_unit: Option<String>,
    ) -> Self {
        Self {
            sweep_id: item_id.to_string(),
            issue: None,
            pid,
            pgid: Some(pid),
            started_at,
            workspace_root: workspace_root.to_path_buf(),
            scope_unit,
        }
    }

    fn worktree(&self) -> Option<PathBuf> {
        self.issue.map(|n| {
            crate::worktree_root::worktree_root(&self.workspace_root).join(format!("issue-{n}"))
        })
    }
}

/// The scope an exited run owned: its recorded unit when systemd knows it,
/// else the one whose embedded pid is the run's pid and which began within the
/// run's lifetime (an older scope wearing the same pid is a previous run's).
/// Pure.
#[must_use]
pub fn resolve_exit_scope(
    recorded: Option<&str>,
    pid: u32,
    run_age_secs: u64,
    scopes: &[ScopeInfo],
) -> Option<String> {
    let listed = |n: &str| {
        scopes
            .iter()
            .any(|s| s.name == n && s.active_state == "active")
    };
    if let Some(unit) = recorded.filter(|u| is_agent_scope(u) && listed(u)) {
        return Some(unit.to_string());
    }
    scopes
        .iter()
        .filter(|s| s.active_state == "active" && embedded_pid(&s.name) == Some(pid))
        .find(|s| {
            s.age_secs
                .is_some_and(|a| a <= run_age_secs + START_TOLERANCE_SECS)
        })
        .map(|s| s.name.clone())
}

/// Processes of the finished run: those attributable to its worktree that
/// started at or after the run did **and** that `owned` (the run's
/// [`RUN_MARKER_ENV`] in the process environment) proves are the run's. A
/// later start alone is not ownership: an operator's shell or editor opened
/// in the worktree mid-run starts later too. An attributed process without
/// the marker (or whose environment is unreadable) is dropped from the table,
/// so it never seeds, and neither do its children through it; an attributed
/// agent runtime is kept, so a newer dispatch is still a hard stop in
/// [`plan_orphan_trees`]. The age gate is zero. Pure.
#[must_use]
pub fn plan_exit_tree(
    procs: &[ProcEntry],
    issue: u32,
    worktree: &Path,
    run_age_secs: u64,
    me: u32,
    owned: &dyn Fn(u32) -> bool,
) -> Option<OrphanTree> {
    let limit = run_age_secs + START_TOLERANCE_SECS;
    let table: Vec<ProcEntry> = procs
        .iter()
        .filter(|p| p.age_secs.is_some_and(|a| a <= limit))
        .filter(|p| {
            !references_worktree(p, worktree) || looks_like_agent(&p.cmdline) || owned(p.pid)
        })
        .cloned()
        .collect();
    plan_orphan_trees(&table, &[(issue, worktree.to_path_buf())], me, 0, &HashMap::new())
        .trees
        .into_iter()
        .next()
}

/// Injected effects of the exit teardown.
pub struct ExitPorts<'a> {
    pub ctl: &'a dyn SystemCtl,
    pub procs: &'a dyn Fn() -> Vec<ProcEntry>,
    pub hooks: &'a ReapHooks<'a>,
    /// The scope-less recorded-identity leg (pid, process group).
    pub identity_leg: &'a dyn Fn(&TreeSpec) -> TeardownReport,
    pub recorder: &'a dyn Recorder,
    /// Whether a different run holds the issue's claim now.
    pub other_claim: &'a dyn Fn() -> bool,
    /// Whether `pid`'s environment carries `LOOM_SWEEP_ID=<sweep_id>`.
    pub carries_marker: &'a dyn Fn(u32, &str) -> bool,
    pub now: DateTime<Utc>,
    pub me: u32,
    pub dry_run: bool,
}

/// Stop everything a finished run left behind.
pub fn run_exit_teardown(req: &ExitRequest, ports: &ExitPorts<'_>) {
    let run_age = u64::try_from((ports.now - req.started_at).num_seconds()).unwrap_or(0);
    let base = |kind| {
        let mut rec = ReapRecord::new(ResiduePath::Exit, kind, ports.dry_run);
        rec.issue = req.issue;
        rec.sweep_id = Some(req.sweep_id.clone());
        rec
    };

    // 1. The scope: the whole cgroup, `setsid` descendants included.
    let scopes = ports.ctl.list_scopes();
    if let Some(unit) = resolve_exit_scope(req.scope_unit.as_deref(), req.pid, run_age, &scopes) {
        let mut rec = base(ResidueKind::Scope);
        rec.unit = Some(unit.clone());
        if ports.dry_run {
            ports.recorder.record(&rec);
        } else {
            match ports.ctl.stop(&unit) {
                Ok(()) => ports.recorder.record(&rec),
                Err(e) => log::warn!("agent_residue_reaper: exit stop {unit}: {e}"),
            }
        }
    }

    // 2. The recorded identity: pid and process group, with the start-time and
    //    protected-pid guards of the shared teardown.
    if !ports.dry_run {
        let spec = TreeSpec {
            pid: Some(req.pid),
            pid_started_at: None,
            recorded_started_at: Some(req.started_at),
            pgid: req.pgid,
            scope_unit: None,
        };
        let report = (ports.identity_leg)(&spec);
        if !report.pids.is_empty() {
            let mut rec = base(ResidueKind::Tree);
            rec.pids = report.pids;
            rec.detail = Some("recorded pid/process group".to_string());
            ports.recorder.record(&rec);
        }
    }

    // 3. Whatever else the run's worktree attributes to it, unless a newer run
    //    now owns the worktree.
    let (Some(issue), Some(worktree)) = (req.issue, req.worktree()) else {
        return;
    };
    if (ports.other_claim)() {
        return;
    }
    let owned = |pid| (ports.carries_marker)(pid, &req.sweep_id);
    let procs = (ports.procs)();
    let Some(tree) = plan_exit_tree(&procs, issue, &worktree, run_age, ports.me, &owned) else {
        return;
    };
    let mut rec = base(ResidueKind::Tree);
    rec.pids.clone_from(&tree.pids);
    rec.detail = Some(format!("worktree {}", dir_name(&worktree)));
    if !ports.dry_run {
        let _ = reap_tree(&tree, TERM_GRACE, ports.hooks);
    }
    ports.recorder.record(&rec);
}

// ============================================================================
// Production wiring
// ============================================================================

/// The process table with a cwd where the platform has one: `/proc` on Linux,
/// else a `ps` read (no cwd, so attribution is by argv only).
#[must_use]
pub fn production_entries() -> Vec<ProcEntry> {
    let entries = snapshot_processes();
    if !entries.is_empty() {
        return entries;
    }
    process_table().map_or_else(Vec::new, |table| entries_from_table(&table, Utc::now()))
}

/// `ps` rows as process entries: no cwd, an age from the start time. Pure.
#[must_use]
pub fn entries_from_table(table: &[Proc], now: DateTime<Utc>) -> Vec<ProcEntry> {
    table
        .iter()
        .map(|p| ProcEntry {
            pid: p.pid,
            ppid: p.ppid,
            cwd: None,
            cmdline: p.cmdline.clone(),
            age_secs: p
                .started_at
                .and_then(|s| u64::try_from((now - s).num_seconds()).ok()),
        })
        .collect()
}

/// Whether one of `fields` is exactly `LOOM_SWEEP_ID=<sweep_id>`. Pure.
#[must_use]
pub fn marker_in<'a>(mut fields: impl Iterator<Item = &'a str>, sweep_id: &str) -> bool {
    let needle = format!("{RUN_MARKER_ENV}={sweep_id}");
    !sweep_id.is_empty() && fields.any(|f| f == needle)
}

/// Whether live `pid`'s environment carries the run's marker: `/proc` on
/// Linux, `ps -E` on macOS (fields split on whitespace). Anything unreadable
/// is "no", so the process is left alone.
#[cfg_attr(test, allow(dead_code))]
fn production_carries_marker(pid: u32, sweep_id: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read(format!("/proc/{pid}/environ"))
            .is_ok_and(|raw| marker_in(String::from_utf8_lossy(&raw).split('\0'), sweep_id))
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("ps");
        cmd.args(["-E", "-ww", "-o", "command=", "-p", &pid.to_string()]);
        run_bounded(cmd, CTL_TIMEOUT).is_some_and(|(ok, out)| {
            ok && marker_in(String::from_utf8_lossy(&out).split_whitespace(), sweep_id)
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, sweep_id);
        false
    }
}

fn production_ctl() -> Box<dyn SystemCtl> {
    SystemdUser::system().map_or_else(
        || Box::new(NoSystemd) as Box<dyn SystemCtl>,
        |s| Box::new(s) as Box<dyn SystemCtl>,
    )
}

/// Scope units recorded by live runs on `roots`, and by live role runs.
fn live_claimed_units(roots: &[PathBuf]) -> HashSet<String> {
    let mut units: HashSet<String> = crate::roll_pause::live_runs::snapshot()
        .into_iter()
        .filter_map(|r| r.scope_unit)
        .collect();
    for root in roots {
        let dir = crate::worktree_ops::liveness::locks_dir(root);
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(raw) = std::fs::read_to_string(entry.path().join("owner.json")) else {
                continue;
            };
            let Ok(owner) = serde_json::from_str::<OwnerView>(&raw) else {
                continue;
            };
            if crate::live_claim::pid_is_live_process(owner.owner_pid) {
                units.extend(owner.scope_unit);
            }
        }
    }
    units
}

/// The scope pid-claim probe: the embedded pid is alive and is that scope's
/// own process.
fn production_pid_claimed(pid: u32, scope_age: u64) -> bool {
    crate::live_claim::pid_is_live_process(pid)
        && pid_matches_scope(
            crate::sweep_registry::reaper::pid_identity::pid_start_wallclock(pid),
            scope_age,
            Utc::now(),
        )
}

/// One periodic pass over every registered workspace, blocking.
pub fn run_periodic(roots: &[PathBuf]) {
    let settings = resolve_settings(roots);
    if !settings.enabled {
        log::debug!("agent_residue_reaper: disabled (autonomous.processReaper.enabled) — skipping");
        return;
    }
    let ctl = production_ctl();
    let claimed = live_claimed_units(roots);
    let actions =
        plan_scopes(&ctl.list_scopes(), &claimed, &production_pid_claimed, settings.min_age_secs);
    run_scope_actions(&actions, ctl.as_ref(), &ProductionRecorder, settings.dry_run);

    let registered: Vec<PathBuf> = roots
        .iter()
        .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone()))
        .collect();
    let probes = WorktreeProbes {
        exists: &|p| p.exists(),
        is_managed: &crate::worktree_ops::clean::is_loom_managed,
        live_claim: &|repo, issue| {
            crate::live_claim::probe(repo, None, issue).map(|e| e.to_string())
        },
    };
    let plan = plan_worktree_residue(
        &production_entries(),
        &registered,
        std::process::id(),
        settings.min_age_secs,
        &probes,
    );
    run_worktree_plan(&plan, &production_hooks(), &ProductionRecorder, settings.dry_run);
}

/// The periodic tick, run on the blocking pool.
pub async fn tick(roots: Vec<PathBuf>) {
    if let Err(e) = tokio::task::spawn_blocking(move || run_periodic(&roots)).await {
        log::error!("agent_residue_reaper: periodic pass panicked ({e})");
    }
}

/// One [`crate::orphan_process_reaper`] pass for `root` on the blocking pool,
/// gated by that repo's own `autonomous.processReaper.enabled`. It shares the
/// worktree reaper's tick, so the two never race each other's `/proc` reads.
pub async fn reap_orphan_processes_for(root: &Path) {
    let config = crate::orphan_process_reaper::read_config(root);
    if !crate::orphan_process_reaper::resolve_enabled(&config) {
        log::debug!("orphan_process_reaper: {} disabled — skipping", root.display());
        return;
    }
    let root_for_task = root.to_path_buf();
    let joined = tokio::task::spawn_blocking(move || {
        crate::orphan_process_reaper::reap_repo_processes(&root_for_task, &config)
    })
    .await;
    match joined {
        Ok(report) => crate::orphan_process_reaper::log_report(root, &report),
        Err(e) => log::error!(
            "orphan_process_reaper: pass for {} panicked ({e}); continuing to the next repo",
            root.display()
        ),
    }
}

/// Tear down a terminal run's leftovers on a detached thread (the registry
/// mutex is held by the caller; a scope stop can take seconds). Under
/// `cfg(test)` the request is only recorded ([`take_requested`]), so registry
/// and role-runner tests never reach systemd.
pub fn request_exit_teardown(req: ExitRequest) {
    #[cfg(not(test))]
    {
        let spawned = std::thread::Builder::new()
            .name("agent-residue-exit".to_string())
            .spawn(move || run_exit_teardown_production(&req));
        if let Err(e) = spawned {
            log::warn!("agent_residue_reaper: could not spawn the exit teardown thread: {e}");
        }
    }
    #[cfg(test)]
    REQUESTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(req);
}

/// Requests its run's exit teardown when dropped. The role launcher holds one
/// from its spawn on, so every terminal outcome (success, failure, signal,
/// timeout, a lost wait) hands the run over, like a sweep's terminal state.
pub struct ExitTeardownOnDrop(pub Option<ExitRequest>);

impl Drop for ExitTeardownOnDrop {
    fn drop(&mut self) {
        if let Some(req) = self.0.take() {
            request_exit_teardown(req);
        }
    }
}

#[cfg(test)]
static REQUESTED: Mutex<Vec<ExitRequest>> = Mutex::new(Vec::new());

/// Drain the exit teardowns requested for `root` (tests run in parallel, so
/// each filters by its own temp root).
#[cfg(test)]
pub(crate) fn take_requested(root: &Path) -> Vec<ExitRequest> {
    let mut all = REQUESTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mine, rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|r| r.workspace_root == root);
    *all = rest;
    mine
}

#[cfg(not(test))]
fn run_exit_teardown_production(req: &ExitRequest) {
    let settings = resolve_settings(std::slice::from_ref(&req.workspace_root));
    if !settings.enabled {
        return;
    }
    let ctl = production_ctl();
    let hooks = production_hooks();
    let other_claim = || {
        req.issue.is_some_and(|n| {
            read_owner(&req.workspace_root, n).is_some_and(|o| {
                o.sweep_id.as_deref().is_some_and(|s| s != req.sweep_id)
                    && crate::live_claim::pid_is_live_process(o.owner_pid)
            })
        })
    };
    run_exit_teardown(
        req,
        &ExitPorts {
            ctl: ctl.as_ref(),
            procs: &production_entries,
            hooks: &hooks,
            identity_leg: &|spec| {
                crate::auto_update::pause_roll::teardown::teardown_tree(spec, TERM_GRACE)
            },
            recorder: &ProductionRecorder,
            other_claim: &other_claim,
            carries_marker: &production_carries_marker,
            now: Utc::now(),
            me: std::process::id(),
            dry_run: settings.dry_run,
        },
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "agent_residue_reaper_tests.rs"]
mod tests;
