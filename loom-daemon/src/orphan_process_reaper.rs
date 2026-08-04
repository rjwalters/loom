//! Reap orphaned process trees that outlived the agent that started them
//! (issue #5110).
//!
//! # The incident this exists to close
//!
//! `loom-worker-1` sat at **load 65 on 8 cores for 5h52m with no active Loom
//! work**. The cause was a single agent-generated driver script
//! (`sim/.work/cal/run_all.sh`) inside `.loom/worktrees/issue-87`: the agent
//! that launched it had died hours earlier, the tree reparented to
//! `systemd --user`, and the driver kept issuing fresh batches of `ngspice`
//! simulations through its whole matrix. Killing the eight running sims did
//! nothing — the driver launched eight more within 20 seconds. Only killing the
//! **driver** stopped the cycle.
//!
//! ## Why the existing teardown could not reach it
//!
//! [`crate::sweep_registry::reaper`] signals a dead sweep's **process group**
//! (`kill(-pgid, …)`, #4982/#4980). That reaches every descendant that stayed
//! in the leader's group — but GNU `timeout` puts its child in a *new* process
//! group unless `--foreground`, so the observed tree spanned **three** groups
//! and two sessions:
//!
//! | pid | ppid | pgid | sid | comm |
//! |---|---|---|---|---|
//! | 2896990 | 2896986 | **2896986** | 2896986 | ngspice |
//! | 2896986 | 2896924 | **2896986** | 2896986 | timeout |
//! | 2896924 | 2896920 | **2896919** | 2896919 | python3 |
//! | 2896920 | 2896919 | **2896919** | 2896919 | bash (the driver) |
//! | 2896915 | 844 | **2896915** | 2896895 | bash (the agent's Bash call) |
//!
//! A pgid-scoped kill reaps *one* of those groups. And once the sweep leader is
//! gone its descendants have reparented, so there is no parent link left to
//! walk from either. The only durable link between a runaway process and the
//! work it belongs to is the **worktree it is running in** — which is what this
//! module keys on.
//!
//! # What this pass does
//!
//! Every worktree-reaper tick (default 15 min, see [`crate::worktree_reaper`]):
//!
//! 1. Enumerate `issue-<N>` worktrees and keep only those that are
//!    **provably unowned** (see "Fail-safes" below).
//! 2. Snapshot `/proc` once and attribute processes to a worktree by **cwd
//!    inside it** or **argv referencing it** (the `ngspice -b …/issue-87/sim/…`
//!    case, whose cwd had already moved on).
//! 3. Expand each attributed seed to its **whole descendant tree** through the
//!    ppid map — transitively, and across process-group/session boundaries,
//!    because the ppid link survives `setpgid`/`setsid` where the pgid does not.
//! 4. Terminate the tree **freeze-first**: `SIGSTOP` parent-first (so a looping
//!    driver cannot issue another batch while we work), re-snapshot to catch
//!    anything forked between the scan and the freeze, then `SIGTERM` +
//!    `SIGCONT`, and `SIGKILL` only the survivors after a grace period.
//! 5. Log every pid it killed, with its age and argv.
//!
//! # Fail-safes (a live sweep's work must never be touched)
//!
//! A worktree's processes are candidates only when **all** of these hold:
//!
//! - the worktree carries the **`.loom-managed` sentinel**
//!   ([`clean::is_loom_managed`]) — user-provisioned worktrees are never touched,
//!   the same gate [`crate::worktree_reaper`] applies to directory removal;
//! - there is **no `.loom-in-use` marker** in the worktree;
//! - the issue has **no live claim whose own process tree cannot be resolved**.
//!   A live spawn-loop task / claim-lock
//!   ([`crate::worktree_ops::liveness::active_spawn_loop_issues`]) or a
//!   [`crate::live_claim::probe`] hit — a live claim lock, a live machine-level
//!   journal record, or a live `/loom:sweep <N>` process — used to protect the
//!   **whole worktree**. Since issue #5135 it is **pid-scoped**: when the
//!   claim's own root pid is confirmed live
//!   ([`crate::worktree_ops::liveness::active_locked_issue_roots`]) only that
//!   sweep's own process tree is off limits, so an orphan tree that predates
//!   and is unrelated to a concurrently live, *re-dispatched* sweep for the
//!   same issue is still reapable — the exact #5110 shape a blanket
//!   issue-scoped gate could never reach. When the root **cannot** be resolved
//!   (a stale lock, an unparseable owner, a claim tracked only through the
//!   legacy spawn-loop-state union), the whole worktree is protected exactly as
//!   before. The claim is re-checked immediately before each kill, and a claim
//!   that *changed* since the plan was built fails closed;
//! - **no live agent runtime is working inside the worktree**
//!   ([`looks_like_agent`]) — the incident's defining property is that the agent
//!   was *gone*, so a live `claude`/`codex`/`/loom:…` process in the worktree
//!   owns it even when no claim names its issue (PR-set sweeps claim no issue;
//!   a manually driven agent takes no spawn-loop claim). This too is pid-scoped
//!   since #5135: an agent that is part of the *resolved* live sweep's own tree
//!   is already accounted for and no longer vetoes the whole worktree; any
//!   other agent still does;
//! - the process **provably predates the live sweep**, when one owns part of
//!   the worktree — a pid that started at or after the sweep's claim may be a
//!   legitimate descendant that already escaped the ppid walk (`setsid` /
//!   `timeout`), and an unknown start time is never guessed at;
//! - the seed process is **older than the minimum age** (default 30 min), so a
//!   just-launched helper racing its own claim registration is never a target;
//! - the tree contains **neither this daemon, nor any of its ancestors, nor any
//!   of its descendants** — the daemon's own children are the sweep registry's
//!   business, and killing our own ancestry is catastrophic.
//!
//! Every ambiguity resolves to *skip*: an unreadable `/proc` entry, an unknown
//! process age, a `/proc`-less platform. Under-reaping is recoverable on the
//! next tick; a false-positive kill is not.
//!
//! # Default-on
//!
//! Like the worktree reaper it rides along with, this is **default-on**: the
//! failure it prevents is a silent multi-hour loss of a whole host, and the
//! gates above are strictly narrower than the ones already trusted to *delete*
//! worktrees. Opt out with `LOOM_ORPHAN_PROCESS_REAPER=0` or
//! `autonomous.processReaper.enabled=false`; observe without killing with
//! `autonomous.processReaper.dryRun=true`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::worktree_ops::clean;

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on (see module docs): set to
/// `0`/`false`/`no`/`off` to disable, `1`/`true`/`yes`/`on` to force-enable
/// even when config disables it.
pub const ORPHAN_PROCESS_REAPER_ENABLE_ENV: &str = "LOOM_ORPHAN_PROCESS_REAPER";

/// Env override for the minimum seed-process age (seconds).
pub const ORPHAN_PROCESS_REAPER_MIN_AGE_ENV: &str = "LOOM_ORPHAN_PROCESS_REAPER_MIN_AGE_SECS";

/// Env override for dry-run mode (detect + log, never signal).
pub const ORPHAN_PROCESS_REAPER_DRY_RUN_ENV: &str = "LOOM_ORPHAN_PROCESS_REAPER_DRY_RUN";

/// Default minimum age a *seed* process must have before it can be reaped
/// (30 minutes).
///
/// This is the single knob standing between "reap a runaway" and "kill an
/// operator's interactive `cargo test` in a worktree whose sweep already
/// finished". Thirty minutes is well past any plausible short-lived helper and
/// still two orders of magnitude below the 5h52m incident. Descendants of a
/// qualifying seed are *not* age-gated — a driver's freshly-spawned batch is
/// exactly what must die with it.
pub const DEFAULT_MIN_ORPHAN_AGE_SECS: u64 = 1800;

/// Grace between the tree `SIGTERM` and the `SIGKILL` escalation.
pub const DEFAULT_TERM_GRACE: Duration = Duration::from_secs(5);

/// Cap on how many pids one tree may contain before the pass refuses to act.
///
/// A blast radius this large means the attribution is wrong (or a worktree path
/// is absurdly generic), and an unattended killer must fail closed rather than
/// take down a host it misread.
pub const MAX_TREE_PIDS: usize = 512;

// ============================================================================
// Config (.loom/config.json → autonomous.processReaper)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.processReaper` this module
/// consumes. Every field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — precedence **env > config >
/// default**, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanProcessReaperConfig {
    /// `autonomous.processReaper.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `autonomous.processReaper.minAgeSecs` — how old a seed process must be.
    pub min_age_secs: Option<u64>,
    /// `autonomous.processReaper.dryRun` — detect and log, never signal.
    pub dry_run: Option<bool>,
}

/// Read `.loom/config.json → autonomous.processReaper`, soft-failing every
/// field to `None` (env/default resolution) on a missing file, malformed JSON,
/// or a missing `autonomous` / `processReaper` block.
#[must_use]
pub fn read_config(repo_root: &Path) -> OrphanProcessReaperConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.processReaper")
    else {
        return OrphanProcessReaperConfig::default();
    };

    OrphanProcessReaperConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        min_age_secs: block
            .get("minAgeSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        dry_run: block.get("dryRun").and_then(serde_json::Value::as_bool),
    }
}

/// Resolve whether the pass runs — precedence **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &OrphanProcessReaperConfig) -> bool {
    if let Ok(v) = std::env::var(ORPHAN_PROCESS_REAPER_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the minimum seed age — precedence **env > config > default**. A zero
/// or unparseable env value falls through rather than disabling the age gate.
#[must_use]
pub fn resolve_min_age_secs(config: &OrphanProcessReaperConfig) -> u64 {
    std::env::var(ORPHAN_PROCESS_REAPER_MIN_AGE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_age_secs)
        .unwrap_or(DEFAULT_MIN_ORPHAN_AGE_SECS)
}

/// Resolve dry-run mode — precedence **env > config > default(false)**.
#[must_use]
pub fn resolve_dry_run(config: &OrphanProcessReaperConfig) -> bool {
    if let Ok(v) = std::env::var(ORPHAN_PROCESS_REAPER_DRY_RUN_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.dry_run.unwrap_or(false)
}

// ============================================================================
// Process-table snapshot
// ============================================================================

/// One process, as much of it as this pass needs.
///
/// Built from `/proc` on Linux; every field that cannot be read resolves to
/// `None`/empty rather than failing the scan (a process can exit mid-walk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    /// Process id.
    pub pid: u32,
    /// Parent process id — the link that survives `setpgid`/`setsid` and makes
    /// transitive reaping possible.
    pub ppid: u32,
    /// Current working directory, when readable.
    pub cwd: Option<PathBuf>,
    /// argv joined by single spaces (`\0` separators replaced).
    pub cmdline: String,
    /// Seconds since the process started, when derivable.
    pub age_secs: Option<u64>,
}

impl ProcEntry {
    /// A compact `pid=… age=… cmd=…` description for the kill log.
    #[must_use]
    pub fn describe(&self) -> String {
        let age = self
            .age_secs
            .map_or_else(|| "?".to_string(), |a| format!("{a}s"));
        let mut cmd: String = self.cmdline.trim().to_string();
        if cmd.is_empty() {
            cmd = "<unknown>".to_string();
        }
        if cmd.len() > 160 {
            cmd.truncate(157);
            cmd.push_str("...");
        }
        format!("pid={} ppid={} age={age} cmd={cmd}", self.pid, self.ppid)
    }
}

/// Parse the `ppid` and `starttime` (clock ticks since boot) out of a
/// `/proc/<pid>/stat` line.
///
/// `comm` may contain spaces *and* parentheses, so the only safe split point is
/// the **last** `)` — the same trick [`crate::live_claim`]'s zombie probe uses.
/// After it, fields are `state ppid pgrp session …`, so `ppid` is index 1 and
/// `starttime` (overall field 22) is index 19.
#[must_use]
pub fn parse_stat(stat: &str) -> Option<(u32, u64)> {
    let after_comm = stat.rfind(')').map(|i| &stat[i + 1..])?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let ppid = fields.get(1)?.parse::<u32>().ok()?;
    let starttime = fields.get(19)?.parse::<u64>().ok()?;
    Some((ppid, starttime))
}

/// Seconds since boot, from `/proc/uptime`'s first field.
#[cfg(target_os = "linux")]
fn uptime_secs() -> Option<f64> {
    let raw = std::fs::read_to_string("/proc/uptime").ok()?;
    raw.split_whitespace().next()?.parse::<f64>().ok()
}

/// `sysconf(_SC_CLK_TCK)` — the `starttime` unit. Falls back to the near-universal
/// Linux value of 100 if the query fails.
#[cfg(target_os = "linux")]
fn clock_ticks_per_sec() -> f64 {
    // SAFETY: `sysconf` is a read-only query with no memory arguments.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        ticks as f64
    } else {
        100.0
    }
}

/// Snapshot every process this host will admit to (Linux: one `/proc` walk).
///
/// Unreadable entries are skipped, never guessed at. On non-Linux hosts this
/// returns an empty snapshot, which makes the whole pass a no-op — the
/// attribution needs `cwd` + `argv` + `ppid` for *every* process, and there is
/// no cheap portable equivalent (the same stance [`crate::live_claim`]'s leg 3
/// takes).
#[cfg(target_os = "linux")]
#[must_use]
pub fn snapshot_processes() -> Vec<ProcEntry> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let uptime = uptime_secs();
    let hz = clock_ticks_per_sec();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
            continue;
        };
        let Some((ppid, starttime)) = parse_stat(&stat) else {
            continue;
        };
        let age_secs = uptime.and_then(|up| {
            let started = starttime as f64 / hz;
            let age = up - started;
            (age >= 0.0).then_some(age as u64)
        });
        let cwd = std::fs::read_link(dir.join("cwd")).ok();
        let cmdline = std::fs::read(dir.join("cmdline"))
            .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
            .unwrap_or_default();
        out.push(ProcEntry {
            pid,
            ppid,
            cwd,
            cmdline,
            age_secs,
        });
    }
    out
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn snapshot_processes() -> Vec<ProcEntry> {
    Vec::new()
}

// ============================================================================
// Attribution: which worktree is a process working in?
// ============================================================================

/// Whether `cmdline` references `path` as a path (not as a prefix of a longer
/// name).
///
/// The boundary check is load-bearing: worktree paths end in `issue-<N>`, so a
/// naive `contains` would attribute `…/issue-870`'s processes to issue 87. A
/// match must be followed by end-of-string, `/`, or a character that cannot
/// continue a path segment.
#[must_use]
pub fn cmdline_references_path(cmdline: &str, path: &Path) -> bool {
    let needle = path.to_string_lossy();
    if needle.is_empty() || needle == "/" {
        return false;
    }
    let mut from = 0usize;
    while let Some(idx) = cmdline[from..].find(needle.as_ref()) {
        let end = from + idx + needle.len();
        let next = cmdline[end..].chars().next();
        let is_boundary = match next {
            None => true,
            Some(c) => !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.'),
        };
        if is_boundary {
            return true;
        }
        from = from + idx + 1;
    }
    false
}

/// Whether `entry` is working inside `worktree` — cwd inside it, or argv
/// referencing it.
///
/// Both legs are needed. The incident's `ngspice` processes had a cwd outside
/// the worktree and named it only in argv (`-b …/issue-87/sim/records/…`),
/// while the driver script had the cwd but a bare relative argv.
#[must_use]
pub fn references_worktree(entry: &ProcEntry, worktree: &Path) -> bool {
    if let Some(cwd) = &entry.cwd {
        if cwd.starts_with(worktree) {
            return true;
        }
    }
    cmdline_references_path(&entry.cmdline, worktree)
}

/// Whether a command line belongs to an **agent runtime** rather than to the
/// work an agent left behind.
///
/// The defining property of the #5110 incident is that *the agent was gone* —
/// only its detached work kept running. So the presence of a live agent inside
/// a worktree is itself proof that something still owns it, whatever the forge
/// bookkeeping says. This catches the cases the claim probes structurally
/// cannot:
///
/// - **PR-set mode** (`/loom:sweep --prs 456`) claims no issue at all, so no
///   claim lock or journal entry names the worktree's issue;
/// - a **manually driven** agent (`/loom:builder` in a MOM terminal) that never
///   took a spawn-loop claim;
/// - a sweep for a *different* issue that is legitimately working in this
///   worktree.
///
/// Deliberately narrow: an `argv[0]` whose basename is a known runtime
/// (`claude`, `codex`), or any argv naming a `/loom:` slash command. It must
/// not match the incident's own tree, whose driver was a bare `bash` running a
/// scratch script (its argv mentions `~/.claude/shell-snapshots/…`, which is
/// why the runtime check is basename-anchored rather than a substring search).
#[must_use]
pub fn looks_like_agent(cmdline: &str) -> bool {
    let mut tokens = cmdline.split_whitespace();
    if let Some(argv0) = tokens.next() {
        let base = Path::new(argv0)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if matches!(base.as_str(), "claude" | "codex") {
            return true;
        }
    }
    cmdline.contains("/loom:")
}

// ============================================================================
// Process-tree algebra
// ============================================================================

/// `ppid -> [child pid]`, from a snapshot.
#[must_use]
pub fn children_map(procs: &[ProcEntry]) -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        map.entry(p.ppid).or_default().push(p.pid);
    }
    for kids in map.values_mut() {
        kids.sort_unstable();
    }
    map
}

/// `pid -> ppid`, from a snapshot.
#[must_use]
pub fn parent_map(procs: &[ProcEntry]) -> HashMap<u32, u32> {
    procs.iter().map(|p| (p.pid, p.ppid)).collect()
}

/// Every transitive descendant of `seeds` (the seeds themselves excluded).
///
/// Breadth-first with a visited set, so a `/proc` snapshot that is internally
/// inconsistent (a pid recycled mid-walk producing a ppid cycle) terminates
/// instead of looping forever.
#[must_use]
pub fn descendants_of(seeds: &[u32], children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut seen: HashSet<u32> = seeds.iter().copied().collect();
    let mut queue: Vec<u32> = seeds.to_vec();
    let mut out = Vec::new();
    while let Some(pid) = queue.pop() {
        let Some(kids) = children.get(&pid) else {
            continue;
        };
        for &kid in kids {
            if kid == pid || !seen.insert(kid) {
                continue;
            }
            out.push(kid);
            queue.push(kid);
        }
    }
    out.sort_unstable();
    out
}

/// Every ancestor of `pid` (the pid itself excluded), walking `parents`.
///
/// Bounded and cycle-safe for the same reason [`descendants_of`] is.
#[must_use]
pub fn ancestors_of(pid: u32, parents: &HashMap<u32, u32>) -> HashSet<u32> {
    let mut out = HashSet::new();
    let mut cursor = pid;
    while let Some(&parent) = parents.get(&cursor) {
        if parent == 0 || !out.insert(parent) {
            break;
        }
        cursor = parent;
    }
    out
}

/// Order `pids` parent-first: a process is signalled before its children, so a
/// looping driver is frozen before the batch it would otherwise re-spawn.
///
/// Depth is measured *within the set* (how many of a pid's ancestors are also
/// being reaped), with pid as a stable tiebreak.
#[must_use]
pub fn order_parent_first(pids: &HashSet<u32>, parents: &HashMap<u32, u32>) -> Vec<u32> {
    let mut with_depth: Vec<(usize, u32)> = pids
        .iter()
        .map(|&pid| {
            let mut depth = 0usize;
            let mut cursor = pid;
            let mut seen = HashSet::new();
            while let Some(&parent) = parents.get(&cursor) {
                if parent == 0 || !seen.insert(parent) {
                    break;
                }
                if pids.contains(&parent) {
                    depth += 1;
                }
                cursor = parent;
            }
            (depth, pid)
        })
        .collect();
    with_depth.sort_unstable();
    with_depth.into_iter().map(|(_, pid)| pid).collect()
}

// ============================================================================
// Planning
// ============================================================================

/// One orphaned process tree, ready to be reaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanTree {
    /// The issue whose worktree the tree is working in.
    pub issue: u32,
    /// That worktree's path.
    pub worktree: PathBuf,
    /// Processes attributed to the worktree directly (cwd/argv).
    pub seeds: Vec<u32>,
    /// Seeds **plus** every transitive descendant, parent-first.
    pub pids: Vec<u32>,
    /// `pid → description` for the kill log, in `pids` order.
    pub details: Vec<String>,
    /// Pids attributed to the same worktree that were deliberately left alone
    /// because they belong to a live sweep's own process tree, or could not be
    /// proven older than it (#5135). Empty when no live sweep claims the issue.
    pub protected_pids: Vec<u32>,
}

/// A live sweep's own process-tree root, expressed in the same units
/// [`ProcEntry::age_secs`] uses so [`plan_orphan_trees`] stays clock-free
/// (issue #5135).
///
/// Built by [`reap_repo_processes`] from
/// [`crate::worktree_ops::liveness::LiveSweepRoot`] (the claim lock's
/// `owner_pid` + `acquired_at`, confirmed live) against the pass's own wall
/// clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveSweepTreeRoot {
    /// The live sweep's own root pid.
    pub pid: u32,
    /// How long ago that sweep's claim was acquired, in seconds — the proxy
    /// for "when did this sweep's own process tree begin". A candidate pid is
    /// only ever a reap candidate when its own age is **strictly greater**
    /// (i.e. it provably predates the live sweep).
    pub age_secs: u64,
}

/// What one planning pass found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanOutcome {
    /// Trees eligible for reaping.
    pub trees: Vec<OrphanTree>,
    /// Worktrees whose processes were deliberately left alone, with the reason.
    pub skipped: Vec<(u32, String)>,
}

/// Build the reap plan for `worktrees` (already vetted by the caller's
/// [`ownership_gate`]) against a `/proc` snapshot.
///
/// `live_roots` maps an issue to the process-tree root of a **live sweep that
/// legitimately owns part of that worktree** (#5135). An issue absent from the
/// map has no live sweep at all, and every attributed process is a candidate —
/// the pre-#5135 behaviour, unchanged. An issue *present* in the map is one
/// where a live claim exists **and** its own root pid was confirmed live: the
/// sweep's own tree is protected pid-by-pid instead of the whole worktree being
/// skipped, so an unrelated orphan sharing that worktree (the #5110 incident
/// shape, re-dispatched onto the same issue) is still reapable.
///
/// Pure: no signals, no filesystem, no clock. This is where every
/// process-level fail-safe lives, so it is exhaustively unit-testable.
#[must_use]
pub fn plan_orphan_trees(
    procs: &[ProcEntry],
    worktrees: &[(u32, PathBuf)],
    self_pid: u32,
    min_age_secs: u64,
    live_roots: &HashMap<u32, LiveSweepTreeRoot>,
) -> PlanOutcome {
    let mut outcome = PlanOutcome::default();
    if procs.is_empty() || worktrees.is_empty() {
        return outcome;
    }

    let children = children_map(procs);
    let parents = parent_map(procs);
    let by_pid: HashMap<u32, &ProcEntry> = procs.iter().map(|p| (p.pid, p)).collect();

    // Never signal ourselves, our ancestry, or our own children: the daemon's
    // children belong to the sweep registry, and our ancestry includes the
    // service manager that started us.
    let mut protected: HashSet<u32> = HashSet::from([0, 1, self_pid]);
    protected.extend(ancestors_of(self_pid, &parents));
    protected.extend(descendants_of(&[self_pid], &children));

    for (issue, worktree) in worktrees {
        let attributed: Vec<&ProcEntry> = procs
            .iter()
            .filter(|p| !protected.contains(&p.pid) && references_worktree(p, worktree))
            .collect();

        // The live sweep's OWN process tree, when one was resolved for this
        // issue (#5135): its root pid plus every transitive descendant. Empty
        // when no live sweep claims the issue, which makes every check below
        // collapse to its pre-#5135 form.
        let live_root = live_roots.get(issue).copied();
        let live_tree: HashSet<u32> = live_root.map_or_else(HashSet::new, |root| {
            let mut tree: HashSet<u32> =
                descendants_of(&[root.pid], &children).into_iter().collect();
            tree.insert(root.pid);
            tree
        });

        // A live agent runtime inside the worktree owns it, whatever the forge
        // bookkeeping says (PR-set sweeps claim no issue; a manually driven
        // agent takes no spawn-loop claim). The incident's defining property is
        // that the agent was GONE — so its presence is a hard stop.
        //
        // Since #5135 this is pid-scoped in exactly the way the claim gate is:
        // an agent that is *part of the resolved live sweep's own tree* is
        // already accounted for (it IS that sweep, and `live_tree` protects it
        // individually below), so it no longer blanket-protects every unrelated
        // process sharing the worktree. An agent runtime OUTSIDE that tree — or
        // any agent at all when no live root was resolved — is still a hard
        // stop for the whole worktree.
        if let Some(agent) = attributed
            .iter()
            .find(|p| looks_like_agent(&p.cmdline) && !live_tree.contains(&p.pid))
        {
            outcome.skipped.push((
                *issue,
                format!("an agent runtime is working in this worktree ({})", agent.describe()),
            ));
            continue;
        }

        let mut seeds: Vec<u32> = Vec::new();
        let mut protected_pids: Vec<u32> = Vec::new();
        for p in &attributed {
            if let Some(root) = live_root {
                // The live sweep's own root, or one of its descendants.
                if live_tree.contains(&p.pid) {
                    protected_pids.push(p.pid);
                    continue;
                }
                // Only a process PROVABLY older than the live sweep itself can
                // be a leftover from a previous, unrelated agent. A pid that
                // started at or after the sweep did may be a legitimate
                // descendant that already escaped the ppid walk (the very
                // `setsid`/`timeout` daemonization trick this module exists to
                // catch), and an unknown age is never guessed at — ambiguous or
                // concurrent timing is never license to kill.
                if !p.age_secs.is_some_and(|age| age > root.age_secs) {
                    protected_pids.push(p.pid);
                    continue;
                }
            }
            // An unknown age is treated as "too young" — never guessed at.
            if p.age_secs.is_some_and(|age| age >= min_age_secs) {
                seeds.push(p.pid);
            }
        }
        if seeds.is_empty() {
            if !protected_pids.is_empty() {
                protected_pids.sort_unstable();
                outcome.skipped.push((
                    *issue,
                    format!(
                        "every process here belongs to (or cannot be proven older than) the live \
                         sweep's own tree: pids={protected_pids:?}"
                    ),
                ));
            }
            continue;
        }
        seeds.sort_unstable();
        protected_pids.sort_unstable();

        let mut pids: HashSet<u32> = seeds.iter().copied().collect();
        pids.extend(descendants_of(&seeds, &children));

        // The orphan tree must not overlap the live sweep's own tree at all. An
        // overlap means the attribution is wrong (an "orphan" that is really an
        // ancestor of the live sweep), so refuse the whole tree rather than
        // partially reaping around it — the same fail-closed stance the daemon
        // self-protection check below takes.
        if let Some(hit) = pids.iter().find(|p| live_tree.contains(p)) {
            outcome.skipped.push((
                *issue,
                format!(
                    "refusing: the attributed tree contains pid {hit}, which belongs to the live \
                     sweep's own process tree"
                ),
            ));
            continue;
        }

        // A tree that has swallowed the daemon (or its ancestry) means the
        // attribution is wrong — refuse the whole tree rather than part of it.
        if let Some(hit) = pids.iter().find(|p| protected.contains(p)) {
            outcome.skipped.push((
                *issue,
                format!(
                    "refusing: the attributed tree contains protected pid {hit} \
                     (this daemon, its ancestry, or its own children)"
                ),
            ));
            continue;
        }
        if pids.len() > MAX_TREE_PIDS {
            outcome.skipped.push((
                *issue,
                format!(
                    "refusing: attributed tree of {} processes exceeds the {MAX_TREE_PIDS}-pid \
                     safety cap",
                    pids.len()
                ),
            ));
            continue;
        }

        // A pid can be both "not provably older than the live sweep" (so it
        // never seeds on its own) and a ppid-descendant of a seed that IS
        // provably older. It belongs to the orphan tree — its parent chain
        // proves it is not the live sweep's — so it is reaped with the tree and
        // must not also be reported as protected. Overlap with the live sweep's
        // own tree cannot reach here: that refuses the whole tree above.
        protected_pids.retain(|p| !pids.contains(p));

        let ordered = order_parent_first(&pids, &parents);
        let details = ordered
            .iter()
            .map(|pid| {
                by_pid
                    .get(pid)
                    .map_or_else(|| format!("pid={pid} (gone)"), |p| p.describe())
            })
            .collect();

        outcome.trees.push(OrphanTree {
            issue: *issue,
            worktree: worktree.clone(),
            seeds,
            pids: ordered,
            details,
            protected_pids,
        });
    }

    outcome
}

// ============================================================================
// Execution
// ============================================================================

/// Injected side effects, so the whole kill sequence is testable with recorded
/// signals and a scripted process table.
pub struct ReapHooks<'a> {
    /// Send `sig` to `pid`; `true` when the signal was delivered.
    pub signal: &'a dyn Fn(u32, i32) -> bool,
    /// A fresh process-table snapshot (used once, after the freeze).
    pub snapshot: &'a dyn Fn() -> Vec<ProcEntry>,
    /// Whether a pid is still alive.
    pub is_alive: &'a dyn Fn(u32) -> bool,
    /// Sleep (the `SIGTERM` → `SIGKILL` grace).
    pub sleep: &'a dyn Fn(Duration),
}

/// What reaping one tree actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeReapOutcome {
    /// The issue whose worktree the tree belonged to.
    pub issue: u32,
    /// Pids frozen with `SIGSTOP` (including late arrivals found after the
    /// freeze).
    pub frozen: Vec<u32>,
    /// Pids that forked *after* the initial scan and were caught by the
    /// post-freeze re-snapshot.
    pub late_arrivals: Vec<u32>,
    /// Pids that accepted `SIGTERM`.
    pub terminated: Vec<u32>,
    /// Pids that needed `SIGKILL`.
    pub killed: Vec<u32>,
    /// Pids still alive after the escalation (a signal we are not permitted to
    /// send, or an uninterruptible state).
    pub survivors: Vec<u32>,
}

/// Terminate one orphaned tree, freeze-first.
///
/// The ordering is the whole point (issue #5110: "killing leaves is useless
/// when a driver is looping"):
///
/// 1. **`SIGSTOP`, parent-first** — the driver stops before its children, so it
///    cannot issue another batch while we are working. `SIGSTOP` cannot be
///    caught or ignored.
/// 2. **Re-snapshot** and freeze anything that forked between the scan and the
///    freeze, so a batch launched in that window does not escape.
/// 3. **`SIGTERM` then `SIGCONT`** — a stopped process only *receives* the
///    pending `SIGTERM` once continued, so the graceful signal is genuinely
///    graceful rather than a no-op.
/// 4. **`SIGKILL` the survivors** after `grace`.
pub fn reap_tree(tree: &OrphanTree, grace: Duration, hooks: &ReapHooks<'_>) -> TreeReapOutcome {
    let mut outcome = TreeReapOutcome {
        issue: tree.issue,
        ..TreeReapOutcome::default()
    };

    // 1. Freeze, parent-first.
    for &pid in &tree.pids {
        if (hooks.signal)(pid, libc::SIGSTOP) {
            outcome.frozen.push(pid);
        }
    }

    // 2. Catch anything forked between the scan and the freeze.
    let fresh = (hooks.snapshot)();
    if !fresh.is_empty() {
        let children = children_map(&fresh);
        let known: HashSet<u32> = tree.pids.iter().copied().collect();
        for pid in descendants_of(&tree.pids, &children) {
            if known.contains(&pid) {
                continue;
            }
            outcome.late_arrivals.push(pid);
            if (hooks.signal)(pid, libc::SIGSTOP) {
                outcome.frozen.push(pid);
            }
        }
    }

    let mut all: Vec<u32> = tree.pids.clone();
    all.extend(outcome.late_arrivals.iter().copied());

    // 3. Graceful stop: SIGTERM (queued while stopped), then SIGCONT so it is
    //    actually delivered.
    for &pid in &all {
        if (hooks.signal)(pid, libc::SIGTERM) {
            outcome.terminated.push(pid);
        }
    }
    for &pid in &all {
        let _ = (hooks.signal)(pid, libc::SIGCONT);
    }

    (hooks.sleep)(grace);

    // 4. Escalate for anything still standing.
    for &pid in &all {
        if (hooks.is_alive)(pid) {
            (hooks.signal)(pid, libc::SIGKILL);
            outcome.killed.push(pid);
        }
    }
    if !outcome.killed.is_empty() {
        (hooks.sleep)(Duration::from_millis(200));
        outcome.survivors = outcome
            .killed
            .iter()
            .copied()
            .filter(|&pid| (hooks.is_alive)(pid))
            .collect();
    }

    outcome
}

/// The production [`ReapHooks`]: real signals, a real `/proc` snapshot, a real
/// sleep.
#[must_use]
pub fn production_hooks() -> ReapHooks<'static> {
    ReapHooks {
        signal: &send_signal,
        snapshot: &snapshot_processes,
        is_alive: &crate::sweep_registry::is_pid_alive,
        sleep: &std::thread::sleep,
    }
}

/// Send `sig` to `pid`, refusing pid 0 (POSIX broadcast-to-own-group) and pid 1.
fn send_signal(pid: u32, sig: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    crate::sweep_registry::send_signal(pid, sig)
}

// ============================================================================
// One repo pass
// ============================================================================

/// What one process-reap pass over one repo did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessReapReport {
    /// `issue-<N>` worktree directories examined.
    pub worktrees_scanned: usize,
    /// Worktrees that survived every ownership gate and were searched for
    /// processes.
    pub unowned_worktrees: usize,
    /// Processes in the `/proc` snapshot.
    pub processes_scanned: usize,
    /// Trees found (whether or not they were signalled — see `dry_run`).
    pub trees: Vec<OrphanTree>,
    /// Per-tree kill outcomes (empty in dry-run mode).
    pub outcomes: Vec<TreeReapOutcome>,
    /// Worktrees a safety gate preserved, with the gate's own wording.
    pub skipped: Vec<(u32, String)>,
    /// Whether this pass was observation-only.
    pub dry_run: bool,
}

impl ProcessReapReport {
    /// Total pids across every tree found.
    #[must_use]
    pub fn pids_found(&self) -> usize {
        self.trees.iter().map(|t| t.pids.len()).sum()
    }

    /// A compact one-line summary for the daemon log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "worktrees={} unowned={} procs={} trees={} pids={} skipped={}{}",
            self.worktrees_scanned,
            self.unowned_worktrees,
            self.processes_scanned,
            self.trees.len(),
            self.pids_found(),
            self.skipped.len(),
            if self.dry_run { " (dry-run)" } else { "" }
        )
    }
}

/// What one worktree's ownership check concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipVerdict {
    /// Leave every process in this worktree alone, for the given reason.
    Skip(String),
    /// Provably unowned — every attributed process is an orphan candidate.
    Unowned,
    /// A live sweep owns *part* of this worktree (#5135): its own process tree
    /// is off limits, but a process that provably predates it is still a
    /// candidate. Carries the resolved root so the planner can discriminate.
    LiveSweep(crate::worktree_ops::liveness::LiveSweepRoot),
}

/// Why a worktree's processes are off limits, or how much of it a live sweep
/// legitimately owns.
///
/// Before #5135 a live claim for the issue blanket-protected **every** process
/// inside the worktree, so an orphan tree that predates and is unrelated to a
/// concurrently live, re-dispatched sweep for the same issue could never be
/// reaped — the exact #5110 incident shape. The claim gate is now the point
/// where "who owns this worktree" becomes pid-scoped: when the claim's own root
/// pid can be confirmed live, the verdict is [`OwnershipVerdict::LiveSweep`] and
/// the discrimination happens per-pid in [`plan_orphan_trees`]. When it cannot
/// (a stale lock, an unparseable owner, or a claim tracked only through the
/// legacy spawn-loop-state union with no lock dir at all), there is nothing to
/// compare a candidate against, so the verdict falls back to a whole-worktree
/// [`OwnershipVerdict::Skip`] — exactly the pre-#5135 behaviour.
///
/// The probes are injected so the gate chain is testable without a forge, a
/// process table, or a live sweep.
#[must_use]
pub fn ownership_gate(
    worktree: &Path,
    issue: u32,
    is_managed: &dyn Fn(&Path) -> bool,
    active_issues: &HashSet<u32>,
    live_claim: &dyn Fn(u32) -> Option<String>,
    live_sweep_root: &dyn Fn(u32) -> Option<crate::worktree_ops::liveness::LiveSweepRoot>,
) -> OwnershipVerdict {
    if !is_managed(worktree) {
        return OwnershipVerdict::Skip("no .loom-managed sentinel (user-provisioned)".to_string());
    }
    if worktree.join(".loom-in-use").exists() {
        return OwnershipVerdict::Skip(".loom-in-use marker present".to_string());
    }
    // `active_issues` is the cheap (filesystem-only) probe, so it is consulted
    // before `live_claim`, which may reach the journal and the process table.
    let claim_evidence = if active_issues.contains(&issue) {
        Some(format!("issue #{issue} has a live spawn-loop task or claim-lock"))
    } else {
        live_claim(issue).map(|evidence| format!("live sweep claim: {evidence}"))
    };
    let Some(evidence) = claim_evidence else {
        return OwnershipVerdict::Unowned;
    };
    match live_sweep_root(issue) {
        Some(root) => OwnershipVerdict::LiveSweep(root),
        // Nothing to compare a candidate pid against — protect everything.
        None => OwnershipVerdict::Skip(format!(
            "{evidence} (no live root pid resolvable — protecting the whole worktree)"
        )),
    }
}

/// Run one production process-reap pass over `repo_root`.
pub fn reap_repo_processes(
    repo_root: &Path,
    config: &OrphanProcessReaperConfig,
) -> ProcessReapReport {
    let min_age = resolve_min_age_secs(config);
    let dry_run = resolve_dry_run(config);
    let mut report = ProcessReapReport {
        dry_run,
        ..ProcessReapReport::default()
    };

    let worktrees_dir = crate::worktree_root::worktree_root(repo_root);
    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        return report;
    };
    let mut dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect();
    dirs.sort_by_key(std::fs::DirEntry::path);

    let active_issues = crate::worktree_ops::liveness::active_spawn_loop_issues(repo_root);
    let live_claim =
        |issue: u32| crate::live_claim::probe(repo_root, None, issue).map(|e| e.to_string());
    // Resolved once per pass, not once per worktree (#5135) — the same "one
    // filesystem scan up front" shape `active_issues` above already uses. Only
    // claims whose recorded owner pid is a confirmed-live, non-zombie process
    // appear here; everything else falls back to whole-worktree protection.
    let locked_roots = crate::worktree_ops::liveness::active_locked_issue_roots(repo_root);
    let live_sweep_root = |issue: u32| locked_roots.get(&issue).copied();
    let now = chrono::Utc::now();

    let mut unowned: Vec<(u32, PathBuf)> = Vec::new();
    // The live sweep root each surviving worktree was partitioned against, kept
    // so the pre-kill recheck can tell "the same sweep we already discriminated
    // against" from "a different sweep claimed this issue mid-pass".
    let mut planned_roots: HashMap<u32, crate::worktree_ops::liveness::LiveSweepRoot> =
        HashMap::new();
    for entry in dirs {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(issue) = crate::worktree_ops::naming::issue_from_worktree(&name) else {
            continue;
        };
        report.worktrees_scanned += 1;
        let path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());
        match ownership_gate(
            &path,
            issue,
            &clean::is_loom_managed,
            &active_issues,
            &live_claim,
            &live_sweep_root,
        ) {
            OwnershipVerdict::Skip(reason) => {
                report.skipped.push((issue, reason));
                continue;
            }
            OwnershipVerdict::LiveSweep(root) => {
                planned_roots.insert(issue, root);
            }
            OwnershipVerdict::Unowned => {}
        }
        unowned.push((issue, path));
    }
    report.unowned_worktrees = unowned.len();
    if unowned.is_empty() {
        return report;
    }

    // Translate each live sweep root into the planner's clock-free units. A
    // claim whose `acquired_at` is in the future (clock skew) yields age 0, so
    // no candidate can ever be "provably older" than it — fail-safe.
    let live_roots: HashMap<u32, LiveSweepTreeRoot> = planned_roots
        .iter()
        .map(|(issue, root)| {
            let age = (now - root.started_at).num_seconds();
            (
                *issue,
                LiveSweepTreeRoot {
                    pid: root.pid,
                    age_secs: u64::try_from(age).unwrap_or(0),
                },
            )
        })
        .collect();

    let procs = snapshot_processes();
    report.processes_scanned = procs.len();
    let plan = plan_orphan_trees(&procs, &unowned, std::process::id(), min_age, &live_roots);
    report.skipped.extend(plan.skipped);
    report.trees = plan.trees;

    if report.trees.is_empty() {
        return report;
    }

    let hooks = production_hooks();
    for tree in &report.trees {
        // Re-verify liveness immediately before signalling: a sweep may have
        // claimed the issue while this pass was scanning /proc. The whole safety
        // argument rests on never racing a live claim — but since #5135 a live
        // claim is no longer disqualifying by itself, only a claim we did NOT
        // already discriminate this tree against. Re-resolving the root and
        // comparing it to the one used at plan time distinguishes the two: an
        // identical root means the partition still holds; anything else (a new
        // sweep, a re-acquired lock, or a claim whose root is unresolvable) is
        // an unmodelled race, and we fail closed.
        if let Some(evidence) = live_claim(tree.issue) {
            let planned = planned_roots.get(&tree.issue).copied();
            // Deliberately a FRESH lock-directory read, not the pass-start
            // `live_sweep_root` snapshot — the whole point is to observe a
            // claim that changed while we were scanning.
            let fresh = crate::worktree_ops::liveness::active_locked_issue_roots(repo_root)
                .get(&tree.issue)
                .copied();
            if planned.is_none() || fresh != planned {
                report
                    .skipped
                    .push((tree.issue, format!("live sweep claim appeared mid-pass: {evidence}")));
                continue;
            }
        }
        log::warn!(
            "orphan_process_reaper: {} issue-{}: {} orphaned process(es) in {} not owned by any \
             live sweep (protected live-sweep pids={:?}){}: [{}]",
            repo_root.display(),
            tree.issue,
            tree.pids.len(),
            tree.worktree.display(),
            tree.protected_pids,
            if dry_run {
                " (dry-run, not signalled)"
            } else {
                ""
            },
            tree.details.join("; ")
        );
        if dry_run {
            continue;
        }
        let outcome = reap_tree(tree, DEFAULT_TERM_GRACE, &hooks);
        log::warn!(
            "orphan_process_reaper: {} issue-{}: reaped frozen={} late_arrivals={:?} \
             terminated={} killed={:?} survivors={:?}",
            repo_root.display(),
            tree.issue,
            outcome.frozen.len(),
            outcome.late_arrivals,
            outcome.terminated.len(),
            outcome.killed,
            outcome.survivors
        );
        report.outcomes.push(outcome);
    }

    report
}

/// Log a pass's outcome at the right volume: quiet when there is nothing to do,
/// loud when processes were killed.
pub fn log_report(repo_root: &Path, report: &ProcessReapReport) {
    if report.trees.is_empty() {
        log::debug!(
            "orphan_process_reaper: {} no orphaned process trees ({})",
            repo_root.display(),
            report.summary()
        );
    } else {
        log::info!("orphan_process_reaper: {} {}", repo_root.display(), report.summary());
    }
    for (issue, reason) in &report.skipped {
        log::debug!(
            "orphan_process_reaper: {} preserving issue-{issue} processes: {reason}",
            repo_root.display()
        );
    }
    for outcome in &report.outcomes {
        if !outcome.survivors.is_empty() {
            log::warn!(
                "orphan_process_reaper: {} issue-{}: {:?} survived SIGKILL — they may be in \
                 uninterruptible I/O or owned by another user",
                repo_root.display(),
                outcome.issue,
                outcome.survivors
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn proc(pid: u32, ppid: u32, cwd: Option<&str>, cmdline: &str, age: Option<u64>) -> ProcEntry {
        ProcEntry {
            pid,
            ppid,
            cwd: cwd.map(PathBuf::from),
            cmdline: cmdline.to_string(),
            age_secs: age,
        }
    }

    // ===================================================================
    // /proc parsing
    // ===================================================================

    #[test]
    fn test_parse_stat_handles_comm_with_spaces_and_parens() {
        // Field 22 (starttime) is the 20th field after `comm`.
        let after = (4..=22)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let stat = format!("1234 (weird )( name) S 99 {after}");
        // fields after comm: state=S ppid=99 then 4..=22 → starttime is index 19
        // → the 18th element of the 4..=22 run → value 21.
        let (ppid, starttime) = parse_stat(&stat).unwrap();
        assert_eq!(ppid, 99);
        assert_eq!(starttime, 21);
    }

    #[test]
    fn test_parse_stat_rejects_garbage() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("no parens here").is_none());
        assert!(parse_stat("1 (sh) S 0").is_none());
    }

    // ===================================================================
    // Attribution
    // ===================================================================

    #[test]
    fn test_cmdline_reference_requires_a_path_boundary() {
        let wt = Path::new("/repo/.loom/worktrees/issue-87");
        // The incident's ngspice argv: worktree named only in argv.
        assert!(cmdline_references_path(
            "ngspice -b /repo/.loom/worktrees/issue-87/sim/records/raw/tt.spice",
            wt
        ));
        assert!(cmdline_references_path("bash /repo/.loom/worktrees/issue-87", wt));
        assert!(cmdline_references_path("bash \"/repo/.loom/worktrees/issue-87\"", wt));
        // A different issue whose number merely starts with 87 must not match.
        assert!(!cmdline_references_path(
            "ngspice -b /repo/.loom/worktrees/issue-870/sim/x.spice",
            wt
        ));
        assert!(!cmdline_references_path("ngspice -b /repo/.loom/worktrees/issue-8", wt));
        assert!(!cmdline_references_path("", wt));
    }

    #[test]
    fn test_references_worktree_matches_cwd_or_argv() {
        let wt = Path::new("/repo/.loom/worktrees/issue-87");
        assert!(references_worktree(
            &proc(2, 1, Some("/repo/.loom/worktrees/issue-87/sim"), "bash x.sh", None),
            wt
        ));
        assert!(references_worktree(
            &proc(3, 1, Some("/tmp"), "ngspice -b /repo/.loom/worktrees/issue-87/a.spice", None),
            wt
        ));
        assert!(!references_worktree(&proc(4, 1, Some("/tmp"), "sleep 1", None), wt));
        // A sibling worktree is never attributed here.
        assert!(!references_worktree(
            &proc(5, 1, Some("/repo/.loom/worktrees/issue-870"), "bash x.sh", None),
            wt
        ));
    }

    // ===================================================================
    // Tree algebra
    // ===================================================================

    #[test]
    fn test_descendants_are_transitive_and_cycle_safe() {
        let procs = vec![
            proc(10, 1, None, "driver", None),
            proc(11, 10, None, "python", None),
            proc(12, 11, None, "timeout", None),
            proc(13, 12, None, "ngspice", None),
            proc(20, 1, None, "unrelated", None),
        ];
        let children = children_map(&procs);
        assert_eq!(descendants_of(&[10], &children), vec![11, 12, 13]);
        assert!(descendants_of(&[20], &children).is_empty());

        // A snapshot with a ppid cycle must terminate.
        let cyclic = vec![proc(30, 31, None, "a", None), proc(31, 30, None, "b", None)];
        let children = children_map(&cyclic);
        assert_eq!(descendants_of(&[30], &children), vec![31]);
    }

    #[test]
    fn test_parent_first_ordering() {
        let procs = vec![
            proc(10, 1, None, "driver", None),
            proc(11, 10, None, "python", None),
            proc(12, 11, None, "timeout", None),
            proc(13, 12, None, "ngspice", None),
        ];
        let parents = parent_map(&procs);
        let set: HashSet<u32> = [13, 11, 12, 10].into_iter().collect();
        assert_eq!(order_parent_first(&set, &parents), vec![10, 11, 12, 13]);
    }

    #[test]
    fn test_ancestors_walk_is_bounded() {
        let procs = vec![
            proc(10, 1, None, "a", None),
            proc(11, 10, None, "b", None),
            proc(12, 11, None, "c", None),
        ];
        let parents = parent_map(&procs);
        assert_eq!(ancestors_of(12, &parents), HashSet::from([11, 10, 1]));
        let cyclic = vec![proc(30, 31, None, "a", None), proc(31, 30, None, "b", None)];
        assert_eq!(ancestors_of(30, &parent_map(&cyclic)), HashSet::from([31, 30]));
    }

    // ===================================================================
    // Planning — the incident's shape
    // ===================================================================

    /// The exact process tree from the incident: three process groups, a
    /// driver whose parent is `systemd --user`, and sims that name the
    /// worktree only in argv.
    fn incident_procs(worktree: &str) -> Vec<ProcEntry> {
        vec![
            proc(844, 1, Some("/"), "systemd --user", Some(999_999)),
            proc(2_896_920, 844, Some(worktree), "bash ./sim/.work/cal/run_all.sh", Some(21_120)),
            proc(
                2_896_924,
                2_896_920,
                Some(worktree),
                "python3 sim/run_corners.py array-liveness --corners tt",
                Some(600),
            ),
            proc(
                2_896_986,
                2_896_924,
                Some("/"),
                &format!("timeout --kill-after=30s 21600s ngspice -b {worktree}/sim/a.spice"),
                Some(120),
            ),
            proc(
                2_896_990,
                2_896_986,
                Some("/"),
                &format!("ngspice -b {worktree}/sim/a.spice"),
                Some(110),
            ),
            proc(999, 1, Some("/home/u"), "sshd", Some(999_999)),
        ]
    }

    #[test]
    fn test_incident_tree_is_planned_whole_across_process_groups() {
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = incident_procs(wt);
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], 4242, 1800, &HashMap::new());
        assert_eq!(plan.trees.len(), 1, "{plan:?}");
        let tree = &plan.trees[0];
        assert_eq!(tree.issue, 87);
        // Only the driver is old enough to seed…
        assert_eq!(tree.seeds, vec![2_896_920]);
        // …but the whole descendant chain comes with it, including the pids
        // that live in other process groups/sessions behind `timeout`.
        assert_eq!(
            tree.pids,
            vec![2_896_920, 2_896_924, 2_896_986, 2_896_990],
            "the tree must be transitive and parent-first"
        );
        assert!(!tree.pids.contains(&844), "systemd must never be in the tree");
        assert!(!tree.pids.contains(&999));
    }

    #[test]
    fn test_looks_like_agent_is_narrow() {
        assert!(looks_like_agent("claude -p /loom:builder"));
        assert!(looks_like_agent("/home/u/.local/bin/claude --resume"));
        assert!(looks_like_agent("codex exec"));
        assert!(looks_like_agent(
            "bash -c .loom/scripts/spawn-worker.sh -p \"/loom:sweep --prs 456\""
        ));
        // The incident's own tree must NOT look like an agent: its driver is a
        // bare shell whose argv merely mentions `~/.claude/shell-snapshots/…`.
        assert!(!looks_like_agent(
            "bash -c source /home/u/.claude/shell-snapshots/snapshot-bash-1.sh && ./run_all.sh"
        ));
        assert!(!looks_like_agent("bash ./sim/.work/cal/run_all.sh"));
        assert!(!looks_like_agent("ngspice -b /repo/sim/a.spice"));
        assert!(!looks_like_agent(""));
    }

    #[test]
    fn test_a_live_agent_in_the_worktree_stops_the_whole_reap() {
        // PR-set mode / a manually driven agent claims no issue, so only this
        // gate can see that the worktree is owned.
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = vec![
            proc(100, 1, Some(wt), "claude -p /loom:sweep --prs 456", Some(99_999)),
            proc(101, 100, Some(wt), "bash ./build.sh", Some(99_999)),
        ];
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], 4242, 1800, &HashMap::new());
        assert!(plan.trees.is_empty(), "{plan:?}");
        assert!(plan.skipped[0].1.contains("agent runtime"), "{plan:?}");
    }

    #[test]
    fn test_young_processes_are_never_seeds() {
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = vec![proc(500, 1, Some(wt), "bash build.sh", Some(60))];
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], 4242, 1800, &HashMap::new());
        assert!(plan.trees.is_empty());
    }

    #[test]
    fn test_unknown_age_is_never_a_seed() {
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = vec![proc(500, 1, Some(wt), "bash build.sh", None)];
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], 4242, 1800, &HashMap::new());
        assert!(plan.trees.is_empty(), "an unknown age must never be assumed old");
    }

    #[test]
    fn test_daemon_self_ancestors_and_children_are_protected() {
        let wt = "/repo/.loom/worktrees/issue-87";
        let self_pid = 700;
        let procs = vec![
            // Our own ancestry, all "working in" the worktree.
            proc(600, 1, Some(wt), "systemd", Some(99_999)),
            proc(self_pid, 600, Some(wt), "loom-daemon", Some(99_999)),
            // Our own child (a live sweep the registry owns).
            proc(800, self_pid, Some(wt), "claude -p /loom:sweep 87", Some(99_999)),
        ];
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], self_pid, 1800, &HashMap::new());
        assert!(
            plan.trees.is_empty(),
            "the daemon, its ancestry, and its children must never be reaped: {plan:?}"
        );
    }

    #[test]
    fn test_an_orphan_whose_child_chain_reaches_the_daemon_is_never_reaped() {
        // The daemon's ancestry is protected, so an "orphan" that is really
        // our own parent yields no seeds at all.
        let wt = "/repo/.loom/worktrees/issue-87";
        let self_pid = 900;
        let procs = vec![
            proc(100, 1, Some(wt), "bash driver.sh", Some(99_999)),
            proc(self_pid, 100, Some("/"), "loom-daemon", Some(99_999)),
        ];
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], self_pid, 1800, &HashMap::new());
        assert!(plan.trees.is_empty(), "{plan:?}");
    }

    #[test]
    fn test_tree_containing_a_protected_pid_is_refused_whole() {
        // Defense in depth for an *inconsistent* snapshot: `/proc` is walked
        // pid by pid while processes come and go, so a recycled pid can appear
        // with two different parents in one snapshot — here 950 looks like a
        // child of both the orphaned driver and this daemon. When the two
        // trees overlap at all, the whole tree is refused rather than
        // partially reaped.
        let wt = "/repo/.loom/worktrees/issue-87";
        let self_pid = 900;
        let procs = vec![
            proc(100, 1, Some(wt), "bash driver.sh", Some(99_999)),
            proc(950, 100, None, "child (stale parent)", Some(99_999)),
            proc(self_pid, 1, Some("/"), "loom-daemon", Some(99_999)),
            proc(950, self_pid, None, "child (fresh parent)", Some(1)),
        ];
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], self_pid, 1800, &HashMap::new());
        assert!(plan.trees.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].1.contains("protected pid"), "{plan:?}");
    }

    #[test]
    fn test_oversized_tree_is_refused() {
        let wt = "/repo/.loom/worktrees/issue-87";
        let mut procs = vec![proc(1000, 1, Some(wt), "bash driver.sh", Some(99_999))];
        for pid in 1001..(1001 + MAX_TREE_PIDS as u32) {
            procs.push(proc(pid, 1000, None, "child", Some(10)));
        }
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], 4242, 1800, &HashMap::new());
        assert!(plan.trees.is_empty());
        assert!(plan.skipped[0].1.contains("safety cap"), "{plan:?}");
    }

    #[test]
    fn test_sibling_worktrees_do_not_bleed_into_each_other() {
        let a = "/repo/.loom/worktrees/issue-87";
        let b = "/repo/.loom/worktrees/issue-870";
        let procs = vec![
            proc(100, 1, Some(a), "bash a.sh", Some(99_999)),
            proc(200, 1, Some(b), "bash b.sh", Some(99_999)),
        ];
        let plan = plan_orphan_trees(
            &procs,
            &[(87, PathBuf::from(a)), (870, PathBuf::from(b))],
            4242,
            1800,
            &HashMap::new(),
        );
        assert_eq!(plan.trees.len(), 2);
        assert_eq!(plan.trees[0].pids, vec![100]);
        assert_eq!(plan.trees[1].pids, vec![200]);
    }

    // ===================================================================
    // Live-sweep discrimination (#5135) — pid-scoped, not issue-scoped
    // ===================================================================

    /// The #5135 fixture: issue 87's worktree holds BOTH a live sweep (root
    /// pid 5000, claimed 3600s ago, with a `claude` child) and an orphan tree
    /// that predates it by hours and belongs to no live process.
    fn live_sweep_and_orphan(worktree: &str) -> Vec<ProcEntry> {
        vec![
            // The live sweep's own tree.
            proc(5000, 1, Some(worktree), "bash spawn-worker.sh /loom:sweep 87", Some(3000)),
            proc(5001, 5000, Some(worktree), "claude -p /loom:sweep 87", Some(2900)),
            proc(5002, 5001, Some(worktree), "cargo test --lib", Some(2800)),
            // The unrelated orphan, started long before the live sweep claimed
            // the issue and reparented to init when its own agent died.
            proc(100, 1, Some(worktree), "bash ./sim/.work/cal/run_all.sh", Some(21_120)),
            proc(101, 100, Some(worktree), "ngspice -b a.spice", Some(600)),
        ]
    }

    /// A live sweep rooted at `pid` whose claim was acquired `age_secs` ago.
    fn tree_root(pid: u32, age_secs: u64) -> HashMap<u32, LiveSweepTreeRoot> {
        HashMap::from([(87, LiveSweepTreeRoot { pid, age_secs })])
    }

    #[test]
    fn test_orphan_is_reaped_while_a_live_sweep_for_the_same_issue_is_untouched() {
        // The whole point of #5135: a live claim for issue 87 no longer
        // blanket-protects every process in issue 87's worktree.
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = live_sweep_and_orphan(wt);
        let plan = plan_orphan_trees(
            &procs,
            &[(87, PathBuf::from(wt))],
            4242,
            1800,
            &tree_root(5000, 3000),
        );
        assert_eq!(plan.trees.len(), 1, "{plan:?}");
        let tree = &plan.trees[0];
        assert_eq!(tree.seeds, vec![100], "only the orphan driver may seed");
        assert_eq!(tree.pids, vec![100, 101], "the orphan's whole tree comes with it");
        for pid in [5000, 5001, 5002] {
            assert!(!tree.pids.contains(&pid), "the live sweep's own tree must survive: {plan:?}");
        }
        assert_eq!(tree.protected_pids, vec![5000, 5001, 5002]);
    }

    #[test]
    fn test_a_worktree_that_is_only_the_live_sweeps_own_tree_yields_nothing() {
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs: Vec<ProcEntry> = live_sweep_and_orphan(wt)
            .into_iter()
            .filter(|p| p.pid >= 5000)
            .collect();
        let plan = plan_orphan_trees(
            &procs,
            &[(87, PathBuf::from(wt))],
            4242,
            1800,
            &tree_root(5000, 3000),
        );
        assert!(plan.trees.is_empty(), "{plan:?}");
        assert_eq!(plan.skipped.len(), 1);
        assert!(plan.skipped[0].1.contains("live sweep's own tree"), "{plan:?}");
    }

    #[test]
    fn test_a_candidate_postdating_the_live_sweep_is_protected() {
        // Not a descendant by ppid (it double-forked through `setsid`), but it
        // started AFTER the live sweep claimed the issue — so it may well be
        // that sweep's own escaped child. Ambiguity never authorises a kill.
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = vec![
            proc(5000, 1, Some(wt), "bash spawn-worker.sh /loom:sweep 87", Some(3000)),
            proc(600, 1, Some(wt), "bash ./long-build.sh", Some(2000)),
        ];
        let plan = plan_orphan_trees(
            &procs,
            &[(87, PathBuf::from(wt))],
            4242,
            1800,
            &tree_root(5000, 3000),
        );
        assert!(plan.trees.is_empty(), "{plan:?}");
        assert!(
            plan.skipped[0].1.contains("2000") || plan.skipped[0].1.contains("600"),
            "{plan:?}"
        );
    }

    #[test]
    fn test_a_candidate_with_an_unknown_age_is_protected_under_a_live_sweep() {
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = vec![
            proc(5000, 1, Some(wt), "bash spawn-worker.sh /loom:sweep 87", Some(3000)),
            proc(600, 1, Some(wt), "bash ./mystery.sh", None),
        ];
        let plan = plan_orphan_trees(
            &procs,
            &[(87, PathBuf::from(wt))],
            4242,
            1800,
            &tree_root(5000, 3000),
        );
        assert!(plan.trees.is_empty(), "an unknown age is never proven to predate: {plan:?}");
    }

    #[test]
    fn test_an_agent_outside_the_live_sweep_tree_still_stops_the_whole_reap() {
        // A second, unclaimed agent (Manual Orchestration Mode) sharing the
        // worktree is NOT the resolved live sweep, so it keeps its pre-#5135
        // whole-worktree veto even though an older orphan is present.
        let wt = "/repo/.loom/worktrees/issue-87";
        let mut procs = live_sweep_and_orphan(wt);
        procs.push(proc(700, 1, Some(wt), "claude -p /loom:builder", Some(9000)));
        let plan = plan_orphan_trees(
            &procs,
            &[(87, PathBuf::from(wt))],
            4242,
            1800,
            &tree_root(5000, 3000),
        );
        assert!(plan.trees.is_empty(), "{plan:?}");
        assert!(plan.skipped[0].1.contains("agent runtime"), "{plan:?}");
    }

    #[test]
    fn test_an_orphan_tree_overlapping_the_live_sweep_tree_is_refused_whole() {
        // The "orphan" is really an ancestor of the live sweep — the
        // attribution is wrong, so refuse the tree rather than reap around it.
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = vec![
            proc(100, 1, Some(wt), "bash ./driver.sh", Some(21_120)),
            proc(5000, 100, Some(wt), "bash spawn-worker.sh /loom:sweep 87", Some(3000)),
        ];
        let plan = plan_orphan_trees(
            &procs,
            &[(87, PathBuf::from(wt))],
            4242,
            1800,
            &tree_root(5000, 3000),
        );
        assert!(plan.trees.is_empty(), "{plan:?}");
        assert!(plan.skipped[0].1.contains("live sweep's own process tree"), "{plan:?}");
    }

    #[test]
    fn test_no_live_root_leaves_every_pre_5135_gate_exactly_as_it_was() {
        // An issue absent from `live_roots` is the pre-#5135 path: the caller's
        // ownership gate already skipped the whole worktree if a claim existed.
        let wt = "/repo/.loom/worktrees/issue-87";
        let procs = live_sweep_and_orphan(wt);
        let plan =
            plan_orphan_trees(&procs, &[(87, PathBuf::from(wt))], 4242, 1800, &HashMap::new());
        // The live sweep's `claude` is an unaccounted-for agent runtime here.
        assert!(plan.trees.is_empty(), "{plan:?}");
        assert!(plan.skipped[0].1.contains("agent runtime"), "{plan:?}");
    }

    // ===================================================================
    // Ownership gate — the fail-safes
    // ===================================================================

    fn managed_worktree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".loom-managed"), "").unwrap();
        tmp
    }

    /// The `Skip` reason, or a panic naming the verdict that was not a skip.
    fn skip_reason(verdict: &OwnershipVerdict) -> String {
        match verdict {
            OwnershipVerdict::Skip(reason) => reason.clone(),
            other => panic!("expected a Skip verdict, got {other:?}"),
        }
    }

    /// A `LiveSweepRoot` at `pid`, claimed at a fixed instant.
    fn live_root(pid: u32) -> crate::worktree_ops::liveness::LiveSweepRoot {
        crate::worktree_ops::liveness::LiveSweepRoot {
            pid,
            started_at: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }
    }

    #[test]
    fn test_ownership_gate_passes_for_an_unowned_managed_worktree() {
        let wt = managed_worktree();
        let gate = ownership_gate(
            wt.path(),
            87,
            &clean::is_loom_managed,
            &HashSet::new(),
            &|_| None,
            &|_| None,
        );
        assert_eq!(gate, OwnershipVerdict::Unowned);
    }

    #[test]
    fn test_ownership_gate_requires_the_managed_sentinel() {
        let wt = tempfile::tempdir().unwrap();
        let gate = ownership_gate(
            wt.path(),
            87,
            &clean::is_loom_managed,
            &HashSet::new(),
            &|_| None,
            &|_| None,
        );
        assert!(skip_reason(&gate).contains(".loom-managed"));
    }

    #[test]
    fn test_ownership_gate_respects_the_in_use_marker() {
        let wt = managed_worktree();
        fs::write(wt.path().join(".loom-in-use"), "{}").unwrap();
        let gate = ownership_gate(
            wt.path(),
            87,
            &clean::is_loom_managed,
            &HashSet::new(),
            &|_| None,
            &|_| None,
        );
        assert!(skip_reason(&gate).contains(".loom-in-use"));
    }

    #[test]
    fn test_ownership_gate_respects_a_claim_lock_with_no_resolvable_root() {
        // Pre-#5135 behaviour is preserved exactly when the claim's own root
        // pid cannot be resolved: protect the whole worktree.
        let wt = managed_worktree();
        let gate = ownership_gate(
            wt.path(),
            87,
            &clean::is_loom_managed,
            &HashSet::from([87]),
            &|_| None,
            &|_| None,
        );
        let reason = skip_reason(&gate);
        assert!(reason.contains("claim-lock"), "{reason}");
        assert!(reason.contains("no live root pid resolvable"), "{reason}");
    }

    #[test]
    fn test_ownership_gate_respects_a_live_sweep_claim_with_no_resolvable_root() {
        let wt = managed_worktree();
        let gate = ownership_gate(
            wt.path(),
            87,
            &clean::is_loom_managed,
            &HashSet::new(),
            &|_| Some("a live `/loom:sweep` process (pid 5)".to_string()),
            &|_| None,
        );
        assert!(skip_reason(&gate).contains("live sweep claim"));
    }

    #[test]
    fn test_ownership_gate_reports_a_live_sweep_root_instead_of_skipping() {
        // #5135: a claim whose own root pid IS resolvable no longer
        // blanket-protects the worktree — the discrimination moves to the
        // planner, per pid.
        let wt = managed_worktree();
        for active in [HashSet::from([87]), HashSet::new()] {
            let gate = ownership_gate(
                wt.path(),
                87,
                &clean::is_loom_managed,
                &active,
                &|_| Some("a live `/loom:sweep` process (pid 5)".to_string()),
                &|_| Some(live_root(4242)),
            );
            assert_eq!(gate, OwnershipVerdict::LiveSweep(live_root(4242)), "{active:?}");
        }
    }

    // ===================================================================
    // Kill sequence
    // ===================================================================

    #[derive(Default)]
    struct SignalLog {
        sent: std::sync::Mutex<Vec<(u32, i32)>>,
    }

    fn tree(pids: &[u32]) -> OrphanTree {
        OrphanTree {
            issue: 87,
            worktree: PathBuf::from("/repo/.loom/worktrees/issue-87"),
            seeds: vec![pids[0]],
            pids: pids.to_vec(),
            details: pids.iter().map(|p| format!("pid={p}")).collect(),
            protected_pids: Vec::new(),
        }
    }

    #[test]
    fn test_kill_sequence_freezes_parent_first_then_terms_then_kills() {
        let log = SignalLog::default();
        let dead_after_term = std::sync::Mutex::new(false);
        let signal = |pid: u32, sig: i32| {
            log.sent.lock().unwrap().push((pid, sig));
            if sig == libc::SIGTERM {
                *dead_after_term.lock().unwrap() = true;
            }
            true
        };
        let snapshot = Vec::new;
        let is_alive = |_: u32| !*dead_after_term.lock().unwrap();
        let sleep = |_: Duration| {};
        let hooks = ReapHooks {
            signal: &signal,
            snapshot: &snapshot,
            is_alive: &is_alive,
            sleep: &sleep,
        };

        let outcome = reap_tree(&tree(&[10, 11, 12]), Duration::ZERO, &hooks);
        let sent = log.sent.lock().unwrap().clone();
        // Every pid is STOPped before any pid is TERMed.
        let first_term = sent.iter().position(|(_, s)| *s == libc::SIGTERM).unwrap();
        let stops: Vec<u32> = sent[..first_term]
            .iter()
            .filter(|(_, s)| *s == libc::SIGSTOP)
            .map(|(p, _)| *p)
            .collect();
        assert_eq!(stops, vec![10, 11, 12], "freeze must be parent-first and complete");
        // SIGCONT must follow SIGTERM so the queued signal is delivered.
        let first_cont = sent.iter().position(|(_, s)| *s == libc::SIGCONT).unwrap();
        assert!(first_cont > first_term);
        assert_eq!(outcome.terminated, vec![10, 11, 12]);
        assert!(outcome.killed.is_empty(), "no SIGKILL when SIGTERM sufficed");
    }

    #[test]
    fn test_kill_sequence_escalates_to_sigkill_for_survivors() {
        let log = SignalLog::default();
        let signal = |pid: u32, sig: i32| {
            log.sent.lock().unwrap().push((pid, sig));
            true
        };
        let snapshot = Vec::new;
        // Nothing ever dies until SIGKILL is recorded.
        let is_alive = |pid: u32| {
            !log.sent
                .lock()
                .unwrap()
                .iter()
                .any(|(p, s)| *p == pid && *s == libc::SIGKILL)
        };
        let sleep = |_: Duration| {};
        let hooks = ReapHooks {
            signal: &signal,
            snapshot: &snapshot,
            is_alive: &is_alive,
            sleep: &sleep,
        };
        let outcome = reap_tree(&tree(&[10, 11]), Duration::ZERO, &hooks);
        assert_eq!(outcome.killed, vec![10, 11]);
        assert!(outcome.survivors.is_empty());
    }

    #[test]
    fn test_kill_sequence_catches_children_forked_after_the_scan() {
        // The defining behavior: a driver that spawned a new batch between the
        // /proc scan and the freeze must not leave that batch running.
        let log = SignalLog::default();
        let signal = |pid: u32, sig: i32| {
            log.sent.lock().unwrap().push((pid, sig));
            true
        };
        let snapshot = || {
            vec![
                proc(10, 1, None, "driver", Some(9999)),
                proc(11, 10, None, "old child", Some(9999)),
                // Forked after the plan was built:
                proc(99, 10, None, "fresh batch", Some(1)),
                proc(100, 99, None, "fresh grandchild", Some(1)),
            ]
        };
        let is_alive = |_: u32| false;
        let sleep = |_: Duration| {};
        let hooks = ReapHooks {
            signal: &signal,
            snapshot: &snapshot,
            is_alive: &is_alive,
            sleep: &sleep,
        };
        let outcome = reap_tree(&tree(&[10, 11]), Duration::ZERO, &hooks);
        assert_eq!(outcome.late_arrivals, vec![99, 100]);
        assert!(outcome.terminated.contains(&99));
        assert!(outcome.terminated.contains(&100));
    }

    #[test]
    fn test_send_signal_refuses_pid_0_and_1() {
        assert!(!send_signal(0, libc::SIGTERM));
        assert!(!send_signal(1, libc::SIGTERM));
    }

    // ===================================================================
    // Repo pass wiring
    // ===================================================================

    #[test]
    fn test_missing_worktree_root_is_a_clean_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let report = reap_repo_processes(tmp.path(), &OrphanProcessReaperConfig::default());
        assert_eq!(report.worktrees_scanned, 0);
        assert!(report.trees.is_empty());
    }

    #[test]
    fn test_unmanaged_worktree_is_skipped_before_any_process_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join(".loom/worktrees/issue-4242");
        fs::create_dir_all(&wt).unwrap();
        let report = reap_repo_processes(tmp.path(), &OrphanProcessReaperConfig::default());
        assert_eq!(report.worktrees_scanned, 1);
        assert_eq!(report.unowned_worktrees, 0);
        assert_eq!(report.processes_scanned, 0, "no /proc walk when nothing is unowned");
        assert!(report.skipped[0].1.contains(".loom-managed"));
    }

    #[test]
    fn test_report_summary_is_compact() {
        let report = ProcessReapReport {
            worktrees_scanned: 3,
            unowned_worktrees: 1,
            processes_scanned: 400,
            trees: vec![tree(&[1, 2, 3])],
            outcomes: Vec::new(),
            skipped: vec![(9, "live sweep claim".to_string())],
            dry_run: true,
        };
        assert_eq!(
            report.summary(),
            "worktrees=3 unowned=1 procs=400 trees=1 pids=3 skipped=1 (dry-run)"
        );
    }

    #[test]
    fn test_describe_truncates_and_labels_unknowns() {
        let long = "x".repeat(400);
        let d = proc(5, 4, None, &long, None).describe();
        assert!(d.contains("age=?"));
        assert!(d.len() < 220, "{d}");
        assert!(proc(5, 4, None, "", Some(3))
            .describe()
            .contains("<unknown>"));
    }

    // ===================================================================
    // Config surface — autonomous.processReaper
    // ===================================================================

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_block_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        let cfg = read_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, OrphanProcessReaperConfig::default());
    }

    #[test]
    fn test_config_reads_every_knob() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"processReaper": {"enabled": false, "minAgeSecs": 60,
               "dryRun": true}}}"#,
        );
        assert_eq!(
            read_config(tmp.path()),
            OrphanProcessReaperConfig {
                enabled: Some(false),
                min_age_secs: Some(60),
                dry_run: Some(true),
            }
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_precedence() {
        std::env::remove_var(ORPHAN_PROCESS_REAPER_ENABLE_ENV);
        assert!(resolve_enabled(&OrphanProcessReaperConfig::default()));
        assert!(!resolve_enabled(&OrphanProcessReaperConfig {
            enabled: Some(false),
            ..OrphanProcessReaperConfig::default()
        }));
        std::env::set_var(ORPHAN_PROCESS_REAPER_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&OrphanProcessReaperConfig {
            enabled: Some(true),
            ..OrphanProcessReaperConfig::default()
        }));
        std::env::set_var(ORPHAN_PROCESS_REAPER_ENABLE_ENV, "1");
        assert!(resolve_enabled(&OrphanProcessReaperConfig {
            enabled: Some(false),
            ..OrphanProcessReaperConfig::default()
        }));
        std::env::remove_var(ORPHAN_PROCESS_REAPER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_min_age_precedence() {
        std::env::remove_var(ORPHAN_PROCESS_REAPER_MIN_AGE_ENV);
        assert_eq!(
            resolve_min_age_secs(&OrphanProcessReaperConfig::default()),
            DEFAULT_MIN_ORPHAN_AGE_SECS
        );
        let cfg = OrphanProcessReaperConfig {
            min_age_secs: Some(120),
            ..OrphanProcessReaperConfig::default()
        };
        assert_eq!(resolve_min_age_secs(&cfg), 120);
        std::env::set_var(ORPHAN_PROCESS_REAPER_MIN_AGE_ENV, "45");
        assert_eq!(resolve_min_age_secs(&cfg), 45);
        // Zero/garbage never disables the age gate.
        std::env::set_var(ORPHAN_PROCESS_REAPER_MIN_AGE_ENV, "0");
        assert_eq!(resolve_min_age_secs(&cfg), 120);
        std::env::set_var(ORPHAN_PROCESS_REAPER_MIN_AGE_ENV, "junk");
        assert_eq!(resolve_min_age_secs(&cfg), 120);
        std::env::remove_var(ORPHAN_PROCESS_REAPER_MIN_AGE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_dry_run_precedence() {
        std::env::remove_var(ORPHAN_PROCESS_REAPER_DRY_RUN_ENV);
        assert!(!resolve_dry_run(&OrphanProcessReaperConfig::default()));
        assert!(resolve_dry_run(&OrphanProcessReaperConfig {
            dry_run: Some(true),
            ..OrphanProcessReaperConfig::default()
        }));
        std::env::set_var(ORPHAN_PROCESS_REAPER_DRY_RUN_ENV, "1");
        assert!(resolve_dry_run(&OrphanProcessReaperConfig::default()));
        std::env::remove_var(ORPHAN_PROCESS_REAPER_DRY_RUN_ENV);
    }

    // ===================================================================
    // Live-fire: a real process tree that escaped via `timeout`
    // (new pgid + new sid), reaped across process-group boundaries.
    // ===================================================================

    #[cfg(target_os = "linux")]
    mod live {
        use super::*;
        use std::process::{Command, Stdio};

        /// A real, **orphaned**, process-group-escaping work tree inside
        /// `worktree`, cleaned up on drop no matter how the test ends.
        ///
        /// Shape (the incident's, reproduced):
        ///
        /// - a driver shell that loops forever launching `timeout`-wrapped
        ///   `sleep`s — GNU `timeout` puts its child in a **new process group**
        ///   and the `setsid` in front puts each batch in a **new session**, so
        ///   the tree spans exactly the pgid/sid boundaries a `kill(-pgid, …)`
        ///   teardown cannot cross (#5110's evidence table);
        /// - double-forked (the intermediate `sh` exits immediately) so the
        ///   driver reparents to `init`/`systemd`, reproducing the "the agent
        ///   is GONE" shape rather than being this test process's own child —
        ///   which also means the reaper's own "never touch your descendants"
        ///   fail-safe is not what makes these tests pass.
        ///
        /// The `Drop` impl is load-bearing: a fixture that outlives a *failing*
        /// test would itself become the runaway this module exists to kill.
        struct Fixture {
            worktree: PathBuf,
            driver_pid: u32,
        }

        impl Fixture {
            fn spawn(worktree: &Path) -> Self {
                let script = worktree.join("run_all.sh");
                fs::write(
                    &script,
                    "#!/bin/sh\n\
                     echo $$ > \"$(dirname \"$0\")/driver.pid\"\n\
                     while true; do\n\
                     \x20 setsid timeout 60 sleep 60 &\n\
                     \x20 sleep 1\n\
                     done\n",
                )
                .unwrap();
                let status = Command::new("sh")
                    .arg("-c")
                    .arg(format!("setsid sh {} </dev/null >/dev/null 2>&1 &", script.display()))
                    .current_dir(worktree)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap();
                assert!(status.success());

                let pid_file = worktree.join("driver.pid");
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while std::time::Instant::now() < deadline {
                    if let Ok(pid) = fs::read_to_string(&pid_file)
                        .map(|raw| raw.trim().to_string())
                        .and_then(|raw| {
                            raw.parse::<u32>()
                                .map_err(|e| std::io::Error::other(e.to_string()))
                        })
                    {
                        return Self {
                            worktree: worktree.to_path_buf(),
                            driver_pid: pid,
                        };
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                panic!("driver never reported its pid");
            }

            /// Block until the driver has escaped into at least `n` other
            /// processes (a `timeout` and its `sleep`).
            fn wait_for_escape(&self, n: usize) {
                assert!(
                    wait_for(|| live_descendants(self.driver_pid).len() >= n, 20),
                    "driver never spawned a timeout-wrapped child"
                );
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                // Kill the driver first so it cannot issue another batch, then
                // sweep everything still working in the worktree (bounded).
                crate::sweep_registry::send_signal(self.driver_pid, libc::SIGKILL);
                for _ in 0..30 {
                    let mut pending = live_in_worktree(&self.worktree);
                    for pid in pending.clone() {
                        pending.extend(live_descendants(pid));
                    }
                    if pending.is_empty() {
                        return;
                    }
                    for pid in pending {
                        crate::sweep_registry::send_signal(pid, libc::SIGKILL);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        fn wait_for<F: Fn() -> bool>(f: F, secs: u64) -> bool {
            let deadline = std::time::Instant::now() + Duration::from_secs(secs);
            while std::time::Instant::now() < deadline {
                if f() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            f()
        }

        fn live_descendants(root: u32) -> Vec<u32> {
            let procs = snapshot_processes();
            descendants_of(&[root], &children_map(&procs))
        }

        /// Every live process (other than this test) still working inside
        /// `worktree` — the "did anything survive or respawn?" oracle, which
        /// unlike a descendant walk survives the driver's own death (its
        /// children reparent to init the moment it dies).
        fn live_in_worktree(worktree: &Path) -> Vec<u32> {
            let self_pid = std::process::id();
            snapshot_processes()
                .into_iter()
                .filter(|p| p.pid != self_pid && references_worktree(p, worktree))
                .map(|p| p.pid)
                .collect()
        }

        /// Wait until `pid`'s age — as the reaper itself computes it, from
        /// `/proc/<pid>/stat`'s `starttime` against `/proc/uptime` — reaches
        /// `secs`, so a test *exercises* the age gate instead of racing it.
        fn wait_until_age(pid: u32, secs: u64) {
            assert!(
                wait_for(
                    || snapshot_processes()
                        .iter()
                        .any(|p| p.pid == pid && p.age_secs.is_some_and(|a| a >= secs)),
                    30
                ),
                "pid {pid} never reached age {secs}s"
            );
        }

        fn alive(pid: u32) -> bool {
            crate::sweep_registry::is_pid_alive(pid)
        }

        /// A managed `issue-<N>` worktree under a throwaway repo root.
        fn managed_repo(issue: u32) -> (tempfile::TempDir, PathBuf) {
            let repo = tempfile::tempdir().unwrap();
            let wt = repo.path().join(format!(".loom/worktrees/issue-{issue}"));
            fs::create_dir_all(&wt).unwrap();
            let wt = wt.canonicalize().unwrap();
            fs::write(wt.join(".loom-managed"), "").unwrap();
            (repo, wt)
        }

        fn test_config(dry_run: bool) -> OrphanProcessReaperConfig {
            OrphanProcessReaperConfig {
                enabled: Some(true),
                min_age_secs: Some(1),
                dry_run: Some(dry_run),
            }
        }

        // The env-var precedence tests above mutate `LOOM_ORPHAN_PROCESS_REAPER_*`
        // process-wide, which would otherwise race these passes' own knob
        // resolution — hence `#[serial]` on every live test.

        #[test]
        #[serial]
        fn test_escaped_tree_is_reaped_across_process_group_boundaries() {
            let tmp = tempfile::tempdir().unwrap();
            let wt = tmp.path().canonicalize().unwrap();
            fs::write(wt.join(".loom-managed"), "").unwrap();
            let fixture = Fixture::spawn(&wt);
            let driver_pid = fixture.driver_pid;
            fixture.wait_for_escape(2);

            let escaped: Vec<u32> = live_descendants(driver_pid);
            let procs = snapshot_processes();
            let by_pid: HashMap<u32, &ProcEntry> = procs.iter().map(|p| (p.pid, p)).collect();

            // The driver really is orphaned (not our child), so the reaper's
            // "never touch your own descendants" fail-safe is not what makes
            // this test pass.
            assert_ne!(
                by_pid.get(&driver_pid).map(|p| p.ppid),
                Some(std::process::id()),
                "fixture was not orphaned"
            );

            // The fixture escapes a pgid-scoped teardown: at least one
            // descendant is in a different process group than the driver.
            let driver_pgid = crate::sweep_registry::process_group_of(driver_pid);
            let escaped_group = escaped.iter().any(|pid| {
                crate::sweep_registry::process_group_of(*pid)
                    .is_some_and(|g| Some(g) != driver_pgid)
            });
            assert!(
                escaped_group,
                "fixture did not escape its process group: {:?}",
                escaped
                    .iter()
                    .filter_map(|p| by_pid.get(p).map(|e| e.describe()))
                    .collect::<Vec<_>>()
            );

            // Plan with a zero age floor (the fixture is seconds old) and reap.
            let plan = plan_orphan_trees(
                &procs,
                &[(87, wt.clone())],
                std::process::id(),
                0,
                &HashMap::new(),
            );
            assert_eq!(plan.trees.len(), 1, "{plan:?}");
            assert!(plan.trees[0].pids.contains(&driver_pid));

            let outcome = reap_tree(&plan.trees[0], Duration::from_secs(2), &production_hooks());
            assert!(!outcome.frozen.is_empty());

            assert!(wait_for(|| !alive(driver_pid), 10), "the driver survived the reap");
            // Transitive: the descendants that escaped into other process
            // groups/sessions are gone too.
            for pid in &escaped {
                assert!(
                    wait_for(|| !alive(*pid), 10),
                    "escaped descendant {pid} (pgid {:?}) survived the reap",
                    crate::sweep_registry::process_group_of(*pid)
                );
            }

            // …and the driver did not get to issue another batch (its interval
            // is 1s, so this window covers more than one iteration).
            std::thread::sleep(Duration::from_millis(2500));
            let survivors = live_in_worktree(&wt);
            assert!(
                survivors.is_empty(),
                "work in the worktree survived or respawned after the reap: {survivors:?}"
            );
        }

        #[test]
        #[serial]
        fn test_production_pass_reaps_a_genuinely_orphaned_tree() {
            // The whole pass, end to end: managed worktree, no claim of any
            // kind, an orphaned driver older than the (test-lowered) age floor
            // ⇒ the tree is reaped and reported.
            let (repo, wt) = managed_repo(4245);
            let fixture = Fixture::spawn(&wt);
            fixture.wait_for_escape(2);
            wait_until_age(fixture.driver_pid, 2);

            let report = reap_repo_processes(repo.path(), &test_config(false));
            assert_eq!(report.trees.len(), 1, "{report:?}");
            assert_eq!(report.outcomes.len(), 1, "{report:?}");
            assert!(
                wait_for(|| !alive(fixture.driver_pid), 10),
                "the orphaned driver survived the production pass"
            );
            std::thread::sleep(Duration::from_millis(2500));
            let survivors = live_in_worktree(&wt);
            assert!(survivors.is_empty(), "leftovers after the pass: {survivors:?}");
        }

        #[test]
        #[serial]
        fn test_a_live_claim_makes_the_pass_leave_the_tree_alone() {
            // The fail-safe, end to end: the same escaping fixture, but the
            // worktree's issue has a live claim, so the production pass must
            // find nothing reapable and signal nothing.
            let (repo, wt) = managed_repo(4242);
            let fixture = Fixture::spawn(&wt);
            fixture.wait_for_escape(1);
            wait_until_age(fixture.driver_pid, 2);

            let lock = repo.path().join(".loom/locks/issue-4242");
            fs::create_dir_all(&lock).unwrap();
            fs::write(
                lock.join("owner.json"),
                format!(r#"{{"owner_pid": {}, "sweep_id": "s1"}}"#, std::process::id()),
            )
            .unwrap();

            let report = reap_repo_processes(repo.path(), &test_config(false));
            assert_eq!(report.worktrees_scanned, 1, "{report:?}");
            assert_eq!(report.unowned_worktrees, 0, "{report:?}");
            assert!(report.trees.is_empty(), "{report:?}");
            assert!(
                report.skipped.iter().any(|(i, r)| *i == 4242
                    && (r.contains("claim-lock") || r.contains("live sweep claim"))),
                "{report:?}"
            );
            assert!(
                alive(fixture.driver_pid),
                "a live-claimed worktree's driver must survive the pass"
            );
            assert!(
                !live_descendants(fixture.driver_pid).is_empty(),
                "a live-claimed worktree's descendants must survive the pass"
            );
        }

        #[test]
        #[serial]
        fn test_unmanaged_worktree_processes_are_never_reaped() {
            // No `.loom-managed` sentinel ⇒ user-provisioned ⇒ untouchable,
            // even with no claim of any kind on the issue.
            let repo = tempfile::tempdir().unwrap();
            let wt = repo.path().join(".loom/worktrees/issue-4243");
            fs::create_dir_all(&wt).unwrap();
            let wt = wt.canonicalize().unwrap();
            let fixture = Fixture::spawn(&wt);
            fixture.wait_for_escape(1);
            wait_until_age(fixture.driver_pid, 2);

            let report = reap_repo_processes(repo.path(), &test_config(false));
            assert_eq!(report.unowned_worktrees, 0, "{report:?}");
            assert!(
                report
                    .skipped
                    .iter()
                    .any(|(i, r)| *i == 4243 && r.contains(".loom-managed")),
                "{report:?}"
            );
            assert!(alive(fixture.driver_pid), "an unmanaged worktree's driver must survive");
        }

        #[test]
        #[serial]
        fn test_dry_run_detects_and_logs_without_signalling() {
            let (repo, wt) = managed_repo(4244);
            let fixture = Fixture::spawn(&wt);
            fixture.wait_for_escape(1);
            wait_until_age(fixture.driver_pid, 2);

            let report = reap_repo_processes(repo.path(), &test_config(true));
            assert_eq!(report.unowned_worktrees, 1, "{report:?}");
            assert_eq!(report.trees.len(), 1, "{report:?}");
            assert!(report.trees[0].pids.contains(&fixture.driver_pid));
            assert!(report.outcomes.is_empty(), "dry run must not signal");
            assert!(alive(fixture.driver_pid), "dry run must leave the tree running");
            drop(wt);
        }
    }
}
